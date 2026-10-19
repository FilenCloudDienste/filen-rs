//! Camera RAW containers, via the preview image they carry.
//!
//! A RAW file is a sensor mosaic, not a picture: demosaicing one is a decode
//! this crate has no business attempting, and the `tiff` crate's attempt is
//! worse than nothing — on a NEF it decodes IFD0, which is a 160x120 postage
//! stamp beside a 12 MP photograph, and hands it back for a 512 px request.
//! Every camera writes a real JPEG of the shot alongside the mosaic, though,
//! so the whole job here is *finding* that JPEG. Once found it is handed to
//! the ordinary JPEG decoder over a [`SubSource`] window, which means the
//! IDCT-scaled decode, the peak estimate and the budget check are the same
//! ones every other format gets — nothing about RAW is special downstream.
//!
//! Where the preview hides, verified against 80 pinned samples:
//!
//! | Container | Where |
//! |---|---|
//! | CR2 | IFD0's single strip (a full-size JPEG) |
//! | NEF, ARW, DNG, SRW | a SubIFD's `JPEGInterchangeFormat`, or its strip |
//! | PEF | a chained IFD's `JPEGInterchangeFormat` |
//! | RW2 | Panasonic's private tag `0x002E`, a bare JPEG blob |
//! | ORF | inside the Olympus maker note (see [`walk_olympus`]) |
//!
//! Some Nikon and DNG files carry no JPEG at all, only a small uncompressed
//! RGB strip; that is picked up too, since a 320x213 preview still beats no
//! thumbnail.
//!
//! Everything here reads attacker-controlled offsets. Directory recursion is
//! depth- and count-capped, every offset and length is checked against the
//! source length before a read, and a candidate is only believed once its
//! bytes have been looked at — a header claiming "4000x3000 JPEG" that does
//! not start with an SOI is discarded, not trusted.

use crate::exif::{Endian, u16_at, u32_at};
use crate::{
	ByteSource, FormatDecoder, LocatedPreview, MIN_PREVIEW_LONG_SIDE, PixelSink, PreparedDecode,
	SmallImage, SubSource, ThumbError, ThumbSpec,
};

/// Directories visited per file, across the IFD chain, SubIFDs and maker
/// notes. Real files use a handful; this is the forged-graph backstop.
const MAX_IFDS: usize = 48;
/// Entries one directory may declare. The largest seen in the pinned set is
/// 142 (an Olympus sub-directory).
const MAX_ENTRIES: u16 = 512;
/// How deep SubIFD / maker-note nesting may go.
const MAX_DEPTH: u8 = 4;
/// Preview candidates collected before the walk stops looking.
const MAX_CANDIDATES: usize = 24;
/// Bytes read at a candidate's offset to decide whether it really is a JPEG
/// and how big. Most previews put their SOF within a few hundred bytes, so
/// that is what is read first.
const SNIFF_BYTES: u64 = 4 * 1024;
/// A candidate that begins with an SOI but has not reached its SOF yet earns
/// bigger reads, growing by this factor, rather than being given up on:
/// Panasonic prefixes its preview with a 31 KB EXIF block and Fujifilm's X-S1
/// with 78 KB of them, and there is no header size a format guarantees.
const SNIFF_GROWTH: u64 = 8;
/// Where growing stops. Reached only by a JPEG whose metadata dwarfs any real
/// one; four reads get there.
const MAX_SNIFF_BYTES: u64 = 1024 * 1024;
/// Anything smaller cannot be a JPEG worth decoding, and rejecting it early
/// keeps degenerate 4-byte "previews" out of the ranking.
const MIN_PREVIEW_BYTES: u64 = 128;
/// Ceiling on an UNCOMPRESSED preview, which is materialised whole rather
/// than scaled during decode. The largest in the pinned set is 320x213; this
/// is ~15x that, and the 4 MB of RGBA it implies still fits the default
/// budget with room to spare.
const MAX_UNCOMPRESSED_PREVIEW_PIXELS: u64 = 1 << 20;

/// TIFF-layout RAW containers whose magic is NOT plain TIFF's `42`.
///
/// Olympus writes `IIRO` / `IIRS` (and `MMOR` / `MMSR` big-endian) and
/// Panasonic writes `IIU\0`; the byte order mark and the directory layout are
/// TIFF's in every case, only the version word differs.
pub(super) fn detect_vendor_tiff(prefix: &[u8]) -> bool {
	matches!(
		prefix.get(0..4),
		Some(b"IIRO" | b"IIRS" | b"MMOR" | b"MMSR" | b"II\x55\x00")
	)
}

/// What the container walk found.
pub(super) struct Index {
	/// EXIF orientation from the first directory that declares one.
	pub orientation: u8,
	/// The largest believable preview, or `None` when the file hides nothing
	/// usable. Either kind: for a thumbnail an uncompressed strip beats a
	/// smaller JPEG stamp.
	pub preview: Option<Preview>,
	/// The largest believable JPEG alone — what a viewer can be handed. Kept
	/// apart because the two kinds must not race for one slot: a bigger RGB
	/// strip would otherwise hide a perfectly good JPEG from the preview
	/// path.
	pub jpeg: Option<Preview>,
}

#[derive(Clone, Copy)]
pub(super) enum Preview {
	/// A JPEG byte range, described by its own leading segments.
	Jpeg { off: u64, len: u64, head: JpegHead },
	/// A single uncompressed 8-bit RGB strip.
	Rgb { off: u64, w: u32, h: u32 },
}

/// What a JPEG's leading segments say about it, from one bounded read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct JpegHead {
	pub w: u32,
	pub h: u32,
	/// Sample precision from the SOF. 8 for anything a viewer decodes.
	pub precision: u8,
	/// Component count from the SOF: 1 grey, 3 YCbCr, 4 CMYK/YCCK.
	pub components: u8,
	/// Where an EXIF APP1 may be spliced into the stream — right after the
	/// SOI, or behind a leading APP0/JFIF, which is the order every camera
	/// JPEG ships. `None` when the stream already carries an EXIF APP1 ahead
	/// of its SOF (a second one is the case no decoder's behaviour is settled
	/// for). An XMP APP1 does not take the slot: EXIF before XMP is the order
	/// XMP itself prescribes. (A JFIF APP0 at its 64 KB maximum pushes the
	/// point past `u16`, and that sliver answers `None` too.)
	pub exif_insert_at: Option<u16>,
	/// The orientation the stream's own EXIF APP1 declares — `Some(1)` when
	/// it carries one that says upright, `None` when it carries none. What a
	/// viewer handed these bytes untouched will apply.
	pub stream_orientation: Option<u8>,
}

/// Most bytes a JPEG preview may declare per pixel of its SOF. An 8-bit
/// baseline or progressive JPEG at the highest quality any camera writes
/// stays under 2; 4 leaves a wide margin, and the slack below covers the EXIF
/// block Fuji and Panasonic put ahead of the SOF. A container claiming more is
/// describing something other than this JPEG — the sensor data behind it —
/// and believing it would stream that out as the preview.
const MAX_PREVIEW_BYTES_PER_PIXEL: u64 = 4;
const PREVIEW_LEN_SLACK: u64 = 1024 * 1024;

impl Preview {
	pub(super) fn area(&self) -> u64 {
		let (w, h) = self.dims();
		u64::from(w) * u64::from(h)
	}

	pub(super) fn dims(&self) -> (u32, u32) {
		match *self {
			Preview::Jpeg { head, .. } => (head.w, head.h),
			Preview::Rgb { w, h, .. } => (w, h),
		}
	}
}

/// The preview a caller can hand to a viewer untouched, or `None`.
///
/// Only a JPEG qualifies — an uncompressed strip is pixels with no file format
/// around them — and only one a viewer will decode: 8-bit, grey or YCbCr
/// (CMYK renders inverted or fails outright on every target), at least
/// [`MIN_PREVIEW_LONG_SIDE`] on its long side.
pub(super) fn locate(index: Index) -> Option<LocatedPreview> {
	match index.jpeg? {
		Preview::Jpeg { off, len, head }
			if head.precision == 8
				&& matches!(head.components, 1 | 3)
				&& head.w.max(head.h) >= MIN_PREVIEW_LONG_SIDE
				&& len
					<= u64::from(head.w) * u64::from(head.h) * MAX_PREVIEW_BYTES_PER_PIXEL
						+ PREVIEW_LEN_SLACK =>
		{
			Some(LocatedPreview {
				offset: off,
				len,
				width: head.w,
				height: head.h,
				orientation: index.orientation,
				exif_insert_at: head.exif_insert_at,
				stream_orientation: head.stream_orientation,
			})
		}
		_ => None,
	}
}

/// Reads exactly `len` bytes, or `None` — short reads, overflowing ranges and
/// anything past the end of the source are all the same answer here.
pub(super) fn read_exact_at(src: &mut dyn ByteSource, offset: u64, len: u64) -> Option<Vec<u8>> {
	let end = offset.checked_add(len)?;
	if len == 0 || end > src.len() {
		return None;
	}
	let mut buf = vec![0u8; usize::try_from(len).ok()?];
	let mut done = 0usize;
	while done < buf.len() {
		match src.read_at(offset + done as u64, &mut buf[done..]) {
			Ok(0) | Err(_) => return None,
			Ok(n) => done += n,
		}
	}
	Some(buf)
}

/// One directory entry, with its value either inline (the common case for the
/// scalars walked here) or at a resolved absolute offset.
struct Entry {
	tag: u16,
	typ: u16,
	count: u32,
	/// The 4 value bytes as stored, for inline values.
	bytes: [u8; 4],
	/// Absolute offset of the value when it does not fit inline.
	value_at: u64,
}

impl Entry {
	fn type_size(&self) -> Option<u64> {
		Some(match self.typ {
			1 | 2 | 6 | 7 => 1,
			3 | 8 => 2,
			4 | 9 | 11 | 13 => 4,
			5 | 10 | 12 => 8,
			_ => return None,
		})
	}

	fn total(&self) -> Option<u64> {
		self.type_size()?.checked_mul(u64::from(self.count))
	}

	/// A single inline unsigned value. Deliberately narrow: every tag this
	/// walker cares about is a count-1 SHORT or LONG, so nothing here has to
	/// chase a value offset, and a tag that does not fit that shape is simply
	/// not read.
	fn scalar(&self, endian: Endian) -> Option<u64> {
		if self.count != 1 || self.total()? > 4 {
			return None;
		}
		match self.typ {
			3 | 8 => u16_at(&self.bytes, 0, endian).map(u64::from),
			4 | 9 | 13 => u32_at(&self.bytes, 0, endian).map(u64::from),
			1 | 6 | 7 => Some(u64::from(self.bytes[0])),
			_ => None,
		}
	}
}

struct Directory {
	entries: Vec<Entry>,
	next: u64,
}

impl Directory {
	fn get(&self, tag: u16) -> Option<&Entry> {
		self.entries.iter().find(|e| e.tag == tag)
	}

	fn scalar(&self, tag: u16, endian: Endian) -> Option<u64> {
		self.get(tag)?.scalar(endian)
	}
}

/// Reads one directory table. `base` is what value offsets inside it are
/// relative to — 0 for the file's own IFDs, the maker-note start for a maker
/// note that rebases its offsets.
fn read_directory(
	src: &mut dyn ByteSource,
	endian: Endian,
	base: u64,
	at: u64,
) -> Option<Directory> {
	let head = read_exact_at(src, at, 2)?;
	let count = u16_at(&head, 0, endian)?;
	if count == 0 || count > MAX_ENTRIES {
		return None;
	}
	let table_len = u64::from(count) * 12;
	// The trailing next-IFD pointer is missing on the sub-directories some
	// maker notes inline, so its absence is not a reason to drop the table.
	let (table, next) = match read_exact_at(src, at + 2, table_len + 4) {
		Some(table) => {
			let next = u32_at(&table, usize::from(count) * 12, endian).unwrap_or(0);
			(table, u64::from(next))
		}
		None => (read_exact_at(src, at + 2, table_len)?, 0),
	};

	let mut entries = Vec::with_capacity(count as usize);
	for i in 0..count as usize {
		let at_entry = i * 12;
		let (Some(tag), Some(typ), Some(n)) = (
			u16_at(&table, at_entry, endian),
			u16_at(&table, at_entry + 2, endian),
			u32_at(&table, at_entry + 4, endian),
		) else {
			continue;
		};
		let bytes: [u8; 4] = match table.get(at_entry + 8..at_entry + 12) {
			Some(slice) => slice.try_into().ok()?,
			None => continue,
		};
		let mut entry = Entry {
			tag,
			typ,
			count: n,
			bytes,
			value_at: at + 2 + at_entry as u64 + 8,
		};
		if entry.total().is_some_and(|total| total > 4)
			&& let Some(raw) = u32_at(&bytes, 0, endian)
		{
			entry.value_at = base.saturating_add(u64::from(raw));
		}
		entries.push(entry);
	}
	Some(Directory { entries, next })
}

/// A candidate before its bytes have been looked at.
struct Candidate {
	off: u64,
	len: u64,
	/// Dimensions the directory declares, used only for the uncompressed
	/// case — a JPEG's real size comes from its own SOF, because the RAW
	/// containers that matter most (NEF, ARW) declare nothing at all for
	/// their big preview, and the ones that do sometimes declare the sensor's
	/// size rather than the preview's.
	w: u32,
	h: u32,
	/// The directory's own tags say this is a small reduced-resolution
	/// uncompressed RGB image whose byte count matches its geometry exactly.
	uncompressed: bool,
}

struct Walk<'a> {
	src: &'a mut dyn ByteSource,
	visited: Vec<u64>,
	candidates: Vec<Candidate>,
	/// The first directory's word, upright included: a deeper directory
	/// (a SubIFD, the Exif IFD) must not overrule an IFD0 that said 1.
	orientation: Option<u8>,
	/// Maker note payload (offset, length), parsed after the main walk so its
	/// own base is known.
	maker_note: Option<(u64, u64)>,
}

/// Which tag vocabulary a directory speaks. Maker notes reuse low tag numbers
/// for entirely different things — `0x0100` is `ImageWidth` in a TIFF IFD and
/// a JPEG thumbnail blob in an Olympus maker note — so the two must never be
/// read with the same rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Vocab {
	Tiff,
	Maker,
}

impl Walk<'_> {
	fn walk(&mut self, endian: Endian, base: u64, at: u64, depth: u8, vocab: Vocab) {
		if depth > MAX_DEPTH
			|| self.visited.len() >= MAX_IFDS
			|| self.candidates.len() >= MAX_CANDIDATES
			|| self.visited.contains(&at)
		{
			return;
		}
		self.visited.push(at);
		let Some(dir) = read_directory(self.src, endian, base, at) else {
			return;
		};

		match vocab {
			Vocab::Tiff => self.collect_tiff(&dir, endian),
			Vocab::Maker => self.collect_maker(&dir, endian, base),
		}

		// SubIFDs: an array of directory offsets. Nikon, Sony, Samsung and
		// Adobe all park the real preview in one.
		if let Some(entry) = dir.get(0x014A) {
			for off in self.offset_array(entry, endian, base) {
				self.walk(endian, base, off, depth + 1, vocab);
			}
		}
		// The Exif IFD is only interesting as the road to the maker note.
		if vocab == Vocab::Tiff
			&& let Some(exif_at) = dir.scalar(0x8769, endian)
		{
			self.walk(endian, base, base.saturating_add(exif_at), depth + 1, vocab);
		}
		if vocab == Vocab::Tiff
			&& self.maker_note.is_none()
			&& let Some(entry) = dir.get(0x927C)
			&& let Some(total) = entry.total()
			&& total > 4
		{
			self.maker_note = Some((entry.value_at, total));
		}
		// Olympus CameraSettings, which holds the offset of the LARGE preview
		// (the maker note's own 0x0100 blob is only the 160x120 stamp). Typed
		// as an IFD pointer by newer bodies and as an inline blob by older
		// ones; both land on a directory table.
		if vocab == Vocab::Maker
			&& let Some(entry) = dir.get(0x2020)
		{
			let target = match entry.scalar(endian) {
				Some(rel) => base.saturating_add(rel),
				None => entry.value_at,
			};
			self.walk(endian, base, target, depth + 1, vocab);
		}

		if dir.next != 0 {
			// A chained directory is a sibling, not a child: PEF puts its
			// full-size preview in IFD2.
			self.walk(endian, base, base.saturating_add(dir.next), depth, vocab);
		}
	}

	/// The LONG array a SubIFDs tag points at, bounded and bounds-checked.
	fn offset_array(&mut self, entry: &Entry, endian: Endian, base: u64) -> Vec<u64> {
		if entry.typ != 4 && entry.typ != 13 {
			return Vec::new();
		}
		let count = entry.count.min(u32::try_from(MAX_IFDS).unwrap_or(u32::MAX));
		if count == 1 {
			return entry
				.scalar(endian)
				.map(|off| vec![base.saturating_add(off)])
				.unwrap_or_default();
		}
		let Some(bytes) = read_exact_at(self.src, entry.value_at, u64::from(count) * 4) else {
			return Vec::new();
		};
		(0..count as usize)
			.filter_map(|i| u32_at(&bytes, i * 4, endian))
			.map(|off| base.saturating_add(u64::from(off)))
			.collect()
	}

	fn push(&mut self, off: u64, len: u64, w: u32, h: u32, uncompressed: bool) {
		if len >= MIN_PREVIEW_BYTES && self.candidates.len() < MAX_CANDIDATES {
			self.candidates.push(Candidate {
				off,
				len,
				w,
				h,
				uncompressed,
			});
		}
	}

	fn collect_tiff(&mut self, dir: &Directory, endian: Endian) {
		if self.orientation.is_none()
			&& let Some(o @ 1..=8) = dir.scalar(0x0112, endian)
		{
			self.orientation = u8::try_from(o).ok();
		}
		let width = dir.scalar(0x0100, endian).unwrap_or(0);
		let height = dir.scalar(0x0101, endian).unwrap_or(0);
		let photometric = dir.scalar(0x0106, endian);
		// PhotometricInterpretation 32803 (CFA) and 34892 (LinearRaw) mark the
		// sensor mosaic. Some DNGs store that mosaic as a lossy JPEG, which
		// would otherwise sail through every other check and thumbnail as a
		// green grid.
		let displayable = photometric.is_none_or(|p| p == 2 || p == 6);
		let (w, h) = (
			u32::try_from(width).unwrap_or(u32::MAX),
			u32::try_from(height).unwrap_or(u32::MAX),
		);

		if displayable
			&& let (Some(off), Some(len)) = (dir.scalar(0x0201, endian), dir.scalar(0x0202, endian))
		{
			self.push(off, len, w, h, false);
		}
		// A single strip is either a whole JPEG (Canon's CR2 preview, Adobe's
		// DNG preview) or a small uncompressed RGB image (Nikon's, Adobe's
		// thumbnails). Multi-strip images are real pictures for the streaming
		// TIFF decoder, not previews.
		if displayable
			&& dir.get(0x0111).is_some_and(|e| e.count == 1)
			&& let (Some(off), Some(len)) = (dir.scalar(0x0111, endian), dir.scalar(0x0117, endian))
		{
			let pixels = u64::from(w) * u64::from(h);
			// Trusting Compression here would be a mistake: Canon tags an
			// uncompressed RGB sub-image as JPEG. The byte count matching the
			// geometry exactly is the check that cannot be talked out of.
			let uncompressed = photometric == Some(2)
				&& dir.scalar(0x00FE, endian) == Some(1)
				&& pixels.saturating_mul(3) == len
				&& pixels <= MAX_UNCOMPRESSED_PREVIEW_PIXELS;
			self.push(off, len, w, h, uncompressed);
		}
		// Panasonic keeps a full JPEG in a private tag, with no directory of
		// its own to describe it.
		if let Some(entry) = dir.get(0x002E)
			&& entry.typ == 7
			&& entry.total().is_some_and(|total| total > 4)
		{
			self.push(entry.value_at, u64::from(entry.count), 0, 0, false);
		}
	}

	fn collect_maker(&mut self, dir: &Directory, endian: Endian, base: u64) {
		// The maker note's own thumbnail blob.
		if let Some(entry) = dir.get(0x0100)
			&& entry.typ == 7
			&& entry.total().is_some_and(|total| total > 4)
		{
			self.push(entry.value_at, u64::from(entry.count), 0, 0, false);
		}
		// Olympus CameraSettings: PreviewImageStart / PreviewImageLength.
		if let (Some(off), Some(len)) = (dir.scalar(0x0101, endian), dir.scalar(0x0102, endian)) {
			self.push(base.saturating_add(off), len, 0, 0, false);
		}
	}

	/// Olympus is the one vendor in the pinned set that hides its large
	/// preview behind a maker note. Two header layouts, differing in where
	/// the directory starts and — the part that bites — what its offsets are
	/// relative to.
	fn walk_olympus(&mut self, file_endian: Endian, at: u64, len: u64) {
		let Some(head) = read_exact_at(self.src, at, 12.min(len)) else {
			return;
		};
		if head.starts_with(b"OLYMPUS\0") {
			let endian = match head.get(8..10) {
				Some(b"II") => Endian::Little,
				Some(b"MM") => Endian::Big,
				_ => return,
			};
			// Offsets rebased to the maker note itself.
			self.walk(endian, at, at + 12, 1, Vocab::Maker);
		} else if head.starts_with(b"OLYMP\0") {
			// The older layout keeps file-relative offsets.
			self.walk(file_endian, 0, at + 8, 1, Vocab::Maker);
		}
	}
}

/// Walks the container and returns the best preview it can prove is really
/// there. Reads a few KB per directory and 16 KB per candidate; nothing here
/// pulls a preview's payload, only enough of its head to believe in it.
pub(super) fn scan(src: &mut dyn ByteSource) -> Index {
	let nothing = || Index {
		orientation: 1,
		preview: None,
		jpeg: None,
	};
	let Some(header) = read_exact_at(src, 0, 8) else {
		return nothing();
	};
	let endian = match &header[0..2] {
		b"II" => Endian::Little,
		b"MM" => Endian::Big,
		_ => return nothing(),
	};
	let Some(ifd0) = u32_at(&header, 4, endian) else {
		return nothing();
	};

	let mut walk = Walk {
		src,
		visited: Vec::new(),
		candidates: Vec::new(),
		orientation: None,
		maker_note: None,
	};
	walk.walk(endian, 0, u64::from(ifd0), 0, Vocab::Tiff);
	if let Some((at, len)) = walk.maker_note {
		walk.walk_olympus(endian, at, len);
	}

	let orientation = walk.orientation.unwrap_or(1);
	let src = walk.src;
	let believed: Vec<Preview> = walk
		.candidates
		.iter()
		.filter_map(|c| believe(src, c))
		.collect();
	let preview = believed.iter().copied().max_by_key(Preview::area);
	let jpeg = believed
		.iter()
		.copied()
		.filter(|p| matches!(p, Preview::Jpeg { .. }))
		.max_by_key(Preview::area);
	Index {
		orientation,
		preview,
		jpeg,
	}
}

/// Turns a claimed candidate into one we have seen the bytes of, or nothing.
fn believe(src: &mut dyn ByteSource, candidate: &Candidate) -> Option<Preview> {
	let end = candidate.off.checked_add(candidate.len)?;
	if candidate.len < MIN_PREVIEW_BYTES || end > src.len() {
		return None;
	}
	if let Some(jpeg) = jpeg_window(src, candidate.off, candidate.len) {
		return Some(jpeg);
	}
	candidate.uncompressed.then_some(Preview::Rgb {
		off: candidate.off,
		w: candidate.w,
		h: candidate.h,
	})
}

/// A byte range believed to be a decodable JPEG, measured by its own SOF.
///
/// Shared with the containers that point straight at their preview instead of
/// describing it in a directory (Fuji's RAF header, Canon's CR3 `PRVW` box):
/// wherever the offset came from, it is checked against the source length and
/// the bytes are looked at before anything is believed.
pub(super) fn jpeg_window(src: &mut dyn ByteSource, off: u64, len: u64) -> Option<Preview> {
	if len < MIN_PREVIEW_BYTES || off.checked_add(len)? > src.len() {
		return None;
	}
	let mut window = SNIFF_BYTES;
	loop {
		let head = read_exact_at(src, off, len.min(window))?;
		if !head.starts_with(&[0xFF, 0xD8]) {
			return None;
		}
		match jpeg_sof(&head) {
			Ok(head) => return Some(Preview::Jpeg { off, len, head }),
			// Structurally not a decodable JPEG. Reading more cannot help.
			Err(Scan::Malformed) => return None,
			Err(Scan::Truncated) => {
				if window >= len || window >= MAX_SNIFF_BYTES {
					return None;
				}
				window = (window * SNIFF_GROWTH).min(MAX_SNIFF_BYTES);
			}
		}
	}
}

/// What a JPEG's SOF declares, over a bounded prefix — and, from the same
/// walk, where an EXIF APP1 could go.
///
/// Only the three SOFs a general-purpose decoder handles are accepted, and
/// that is a correctness requirement rather than caution: a RAW file's sensor
/// data is itself frequently stored as a *lossless* JPEG (SOF3), often the
/// largest JPEG-shaped thing in the file. Ranking by size without this check
/// picks the mosaic every time.
fn jpeg_sof(data: &[u8]) -> Result<JpegHead, Scan> {
	let mut at = 2usize;
	let mut exif_insert_at = Some(2usize);
	let mut stream_orientation = None;
	let mut first_segment = true;
	while at + 4 <= data.len() {
		if data[at] != 0xFF {
			return Err(Scan::Malformed);
		}
		let marker = data[at + 1];
		// Fill bytes, TEM, RSTn and a stray SOI carry no payload.
		if marker == 0xFF {
			at += 1;
			continue;
		}
		if marker == 0x01 || (0xD0..=0xD8).contains(&marker) {
			at += 2;
			continue;
		}
		// Entropy-coded data or end of image: the SOF should have come first.
		if marker == 0xD9 || marker == 0xDA {
			return Err(Scan::Malformed);
		}
		let len = usize::from(u16::from_be_bytes([data[at + 2], data[at + 3]]));
		if len < 2 {
			return Err(Scan::Malformed);
		}
		// Any other frame header — lossless (SOF3), differential, arithmetic —
		// is a JPEG we cannot decode, and saying so is what stops the scan
		// walking past it in search of a frame it likes better. A RAW's sensor
		// data is exactly this.
		if matches!(marker, 0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
			return Err(Scan::Malformed);
		}
		if matches!(marker, 0xC0..=0xC2) {
			let Some(frame) = data.get(at + 4..at + 10) else {
				return Err(Scan::Truncated);
			};
			let h = u16::from_be_bytes([frame[1], frame[2]]);
			let w = u16::from_be_bytes([frame[3], frame[4]]);
			if w == 0 || h == 0 {
				return Err(Scan::Malformed);
			}
			return Ok(JpegHead {
				w: u32::from(w),
				h: u32::from(h),
				precision: frame[0],
				components: frame[5],
				exif_insert_at: exif_insert_at.and_then(|p| u16::try_from(p).ok()),
				stream_orientation,
			});
		}
		match marker {
			// JFIF claims the slot right after the SOI, so an EXIF segment
			// goes behind it — the order every camera JPEG ships.
			0xE0 if first_segment => exif_insert_at = Some(at + 2 + len),
			// An EXIF APP1 is the stream's own say about the bytes, and the
			// slot is taken. Any other APP1 (XMP) leaves it free: EXIF ahead
			// of XMP is the order every camera JPEG with both ships. The
			// segment lies wholly ahead of the SOF, so it is in `data`.
			0xE1 => {
				if let Some(payload) = data.get(at + 4..at + 2 + len)
					&& let Some(tiff) = payload.strip_prefix(b"Exif\0\0")
				{
					exif_insert_at = None;
					stream_orientation = Some(crate::exif::orientation(tiff));
				}
			}
			_ => {}
		}
		first_segment = false;
		at = at.checked_add(2 + len).ok_or(Scan::Malformed)?;
	}
	Err(Scan::Truncated)
}

/// Why an SOF scan came back empty. The distinction is what lets a candidate
/// with an unusually fat metadata block earn a larger read while garbage is
/// dropped after the first one.
#[derive(Debug, PartialEq, Eq)]
enum Scan {
	/// The bytes ran out before the SOF; more of them may help.
	Truncated,
	/// Not a decodable JPEG. Includes the SOFs a general-purpose decoder does
	/// not handle, notably the lossless SOF3 a RAW's sensor data is stored as.
	Malformed,
}

/// Wraps the found preview as a prepared decode.
///
/// The RAW itself is never decoded, so `peak_estimate` is unaffordable by
/// construction and [`embedded_preview`](PreparedDecode::embedded_preview) is
/// the only path that produces anything — which also means a preview-only
/// caller (a large remote file) gets the same thumbnail a local one does, for
/// the cost of the preview's byte range.
pub(super) fn prepare(
	src: Box<dyn ByteSource>,
	preview: Preview,
	orientation: u8,
	spec: &ThumbSpec,
) -> Result<Box<dyn PreparedDecode>, ThumbError> {
	let dims = preview.dims();
	match preview {
		Preview::Jpeg { off, len, .. } => {
			let inner = super::JPEG.open(Box::new(SubSource::new(src, off, len)), spec)?;
			// The RAW's own orientation tag wins when it has one; a bare
			// preview stream usually carries no EXIF of its own. Taking one or
			// the other — never both — is what keeps a 90-degree rotation from
			// being applied twice.
			let orientation = if orientation > 1 {
				orientation
			} else {
				inner.orientation()
			};
			Ok(Box::new(PreparedRawJpeg {
				inner: Some(inner),
				spec: *spec,
				dims,
				orientation,
			}))
		}
		Preview::Rgb { off, w, h } => Ok(Box::new(PreparedRawRgb {
			src: Some(src),
			off,
			dims: (w, h),
			orientation,
		})),
	}
}

struct PreparedRawJpeg {
	inner: Option<Box<dyn PreparedDecode>>,
	spec: ThumbSpec,
	dims: (u32, u32),
	orientation: u8,
}

impl PreparedDecode for PreparedRawJpeg {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		let Some(inner) = self.inner.take() else {
			return Ok(None);
		};
		// Priced by the same rules as any other decode; over budget answers
		// None rather than failing, and the orchestrator reports no thumbnail.
		// The allowance this call was handed wins over the one `open` saw.
		let spec = ThumbSpec {
			mem_budget,
			..self.spec
		};
		crate::decode_bounded(inner, &spec)
	}

	fn peak_estimate(&self) -> usize {
		usize::MAX
	}

	fn decode_into(self: Box<Self>, _sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		Err(ThumbError::Decode(
			"raw: the sensor mosaic is not decodable here".into(),
		))
	}
}

struct PreparedRawRgb {
	src: Option<Box<dyn ByteSource>>,
	off: u64,
	dims: (u32, u32),
	orientation: u8,
}

impl PreparedDecode for PreparedRawRgb {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		let (Some(mut src), (w, h)) = (self.src.take(), self.dims) else {
			return Ok(None);
		};
		let row_bytes = w as usize * 3;
		// The whole RGBA strip plus the band buffer below, refused before
		// either is allocated: nothing downstream can un-spend them.
		let cost = (w as usize)
			.saturating_mul(h as usize)
			.saturating_mul(4)
			.saturating_add(row_bytes)
			.saturating_add(65536);
		if cost > mem_budget {
			return Ok(None);
		}
		let mut rgba = vec![0u8; w as usize * h as usize * 4];
		// Read in bands rather than row by row: a remote source charges per
		// request, and 64 KB at a time keeps both the request count and the
		// scratch buffer small.
		let rows_per_band = (65536 / row_bytes.max(1)).max(1);
		let mut band = vec![0u8; rows_per_band * row_bytes];
		let mut y = 0usize;
		while y < h as usize {
			let rows = rows_per_band.min(h as usize - y);
			let want = &mut band[..rows * row_bytes];
			let mut done = 0usize;
			while done < want.len() {
				match src.read_at(self.off + (y * row_bytes + done) as u64, &mut want[done..])? {
					0 => return Ok(None),
					n => done += n,
				}
			}
			for r in 0..rows {
				let src_row = &want[r * row_bytes..][..row_bytes];
				let dst_row = &mut rgba[(y + r) * w as usize * 4..][..w as usize * 4];
				for (dst, px) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(3)) {
					dst.copy_from_slice(&[px[0], px[1], px[2], 255]);
				}
			}
			y += rows;
		}
		Ok(Some(SmallImage {
			width: w,
			height: h,
			rgba,
		}))
	}

	fn peak_estimate(&self) -> usize {
		usize::MAX
	}

	fn decode_into(self: Box<Self>, _sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		Err(ThumbError::Decode(
			"raw: the sensor mosaic is not decodable here".into(),
		))
	}
}

#[cfg(test)]
mod tests {
	use super::{Scan, Vocab, Walk, jpeg_sof, scan};
	use crate::{ByteSource, LocatedPreview, MemSource, exif::Endian, locate_preview};

	/// A little-endian TIFF header pointing at `ifd0`.
	fn header(ifd0: u32) -> Vec<u8> {
		let mut out = b"II\x2a\x00".to_vec();
		out.extend_from_slice(&ifd0.to_le_bytes());
		out
	}

	fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
		let mut out = tag.to_le_bytes().to_vec();
		out.extend_from_slice(&typ.to_le_bytes());
		out.extend_from_slice(&count.to_le_bytes());
		out.extend_from_slice(&value.to_le_bytes());
		out
	}

	/// A minimal JPEG head: SOI, an APP0 to skip over, then a baseline SOF.
	fn jpeg_head(w: u16, h: u16) -> Vec<u8> {
		let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00];
		out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
		out.extend_from_slice(&h.to_be_bytes());
		out.extend_from_slice(&w.to_be_bytes());
		out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
		out
	}

	fn dims_of(data: &[u8]) -> Result<(u32, u32), Scan> {
		jpeg_sof(data).map(|head| (head.w, head.h))
	}

	#[test]
	fn sof_is_read_and_lossless_jpeg_is_rejected() {
		assert_eq!(dims_of(&jpeg_head(1600, 1200)), Ok((1600, 1200)));
		// SOF3 is a RAW's own sensor data, and often the largest JPEG-shaped
		// thing in the file. Picking it is the failure mode this check exists
		// to prevent — and it must be Malformed, not Truncated, or the scan
		// would keep reading in the hope of a better answer.
		let mut lossless = jpeg_head(4000, 3000);
		lossless[9] = 0xC3;
		assert_eq!(dims_of(&lossless), Err(Scan::Malformed));
		// Running out of bytes is distinguishable from being wrong.
		assert_eq!(dims_of(&[]), Err(Scan::Truncated));
		assert_eq!(dims_of(&[0xFF, 0xD8, 0xFF]), Err(Scan::Truncated));
		assert_eq!(dims_of(&jpeg_head(1600, 1200)[..8]), Err(Scan::Truncated));
		assert_eq!(dims_of(&jpeg_head(1600, 1200)[..14]), Err(Scan::Truncated));
		assert_eq!(dims_of(b"not a jpeg at all"), Err(Scan::Malformed));
		assert_eq!(dims_of(&jpeg_head(0, 1200)), Err(Scan::Malformed));
		// A zero-length marker segment must not loop forever.
		assert_eq!(
			dims_of(&[0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x00, 0xFF, 0xD9]),
			Err(Scan::Malformed)
		);
	}

	/// Where an EXIF APP1 could be spliced is decided by the same walk that
	/// finds the SOF, so the two can never disagree about a stream. Behind a
	/// leading JFIF, right after the SOI otherwise, and nowhere at all once
	/// the stream carries an APP1 of its own — EXIF or XMP, wherever it sits.
	#[test]
	fn the_exif_splice_point_follows_the_leading_segments() {
		// SOI, APP0 (4 bytes), SOF: behind the APP0.
		let head = jpeg_sof(&jpeg_head(1600, 1200)).unwrap();
		assert_eq!(head.exif_insert_at, Some(2 + 2 + 4));
		assert_eq!((head.precision, head.components), (8, 3));
		// SOI straight into the SOF: right after the SOI.
		let bare = [&[0xFF, 0xD8][..], &jpeg_head(640, 480)[8..]].concat();
		assert_eq!(jpeg_sof(&bare).unwrap().exif_insert_at, Some(2));
		// An APP1 first (EXIF), and an APP1 behind the APP0 (XMP): both None.
		let mut exif_first = vec![
			0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x08, b'E', b'x', b'i', b'f', 0, 0,
		];
		exif_first.extend_from_slice(&jpeg_head(640, 480)[2..]);
		assert_eq!(jpeg_sof(&exif_first).unwrap().exif_insert_at, None);
		// An XMP APP1 behind the APP0 is not the stream's say: the slot ahead
		// of it stays free, which is where EXIF goes in every camera JPEG that
		// carries both.
		let mut xmp_after_app0 = jpeg_head(640, 480)[..8].to_vec();
		xmp_after_app0.extend_from_slice(&[0xFF, 0xE1, 0x00, 0x04, 0, 0]);
		xmp_after_app0.extend_from_slice(&jpeg_head(640, 480)[8..]);
		assert_eq!(jpeg_sof(&xmp_after_app0).unwrap().exif_insert_at, Some(8));
		// A JFIF APP0 fat with its own thumbnail moves the point behind it,
		// however large: the writers stream, so nothing is copied to get there.
		let mut fat = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x04, 0x00];
		fat.resize(2 + 2 + 0x400, 0);
		fat.extend_from_slice(&jpeg_head(640, 480)[8..]);
		assert_eq!(jpeg_sof(&fat).unwrap().exif_insert_at, Some(2 + 2 + 0x400));
		// Precision and component count are reported, not judged, here.
		let mut twelve = jpeg_head(640, 480);
		twelve[12] = 12;
		assert_eq!(jpeg_sof(&twelve).unwrap().precision, 12);
		let mut cmyk = jpeg_head(640, 480);
		cmyk[17] = 4;
		assert_eq!(jpeg_sof(&cmyk).unwrap().components, 4);
	}

	/// A preview whose metadata dwarfs the 4 KB first look is still found —
	/// Fujifilm's X-S1 puts 78 KB of EXIF ahead of its SOF — while a window
	/// that only ever holds garbage is dropped after one read.
	#[test]
	fn a_deeply_buried_sof_is_reached_by_growing_the_window() {
		let mut jpeg = vec![0xFF, 0xD8];
		for _ in 0..3 {
			jpeg.extend_from_slice(&[0xFF, 0xE1, 0xFF, 0xFD]);
			jpeg.resize(jpeg.len() + 0xFFFD - 2, 0);
		}
		jpeg.extend_from_slice(&jpeg_head(4416, 2944)[2..]);
		let len = jpeg.len() as u64;
		let mut src = MemSource(jpeg);
		let found = super::jpeg_window(&mut src, 0, len).expect("the SOF must be reached");
		assert_eq!(found.dims(), (4416, 2944));
	}

	/// IFD0's word is final, upright included: a SubIFD or Exif IFD that
	/// says otherwise describes something else, and every RAW reader takes
	/// IFD0's tag as THE orientation.
	#[test]
	fn an_upright_ifd0_is_not_overruled_by_a_deeper_directory() {
		let jpeg = jpeg_head(1600, 1200);
		let mut file = header(8);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 1));
		file.extend_from_slice(&entry(0x014A, 4, 1, 60));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(60, 0);
		file.extend_from_slice(&3u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 6));
		file.extend_from_slice(&entry(0x0201, 4, 1, 200));
		file.extend_from_slice(&entry(0x0202, 4, 1, 400));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(600, 0);
		assert_eq!(scan(&mut MemSource(file)).orientation, 1);
	}

	/// The shape every TIFF-family RAW reduces to: a SubIFD carrying a JPEG
	/// the top-level directory says nothing about.
	#[test]
	fn a_subifd_jpeg_is_found_and_measured_by_its_own_sof() {
		let jpeg = jpeg_head(1600, 1200);
		let mut file = header(8);
		// IFD0: orientation 6, SubIFDs -> 60.
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 6));
		file.extend_from_slice(&entry(0x014A, 4, 1, 60));
		file.extend_from_slice(&0u32.to_le_bytes());
		assert_eq!(file.len(), 38);
		file.resize(60, 0);
		// SubIFD: JPEGInterchangeFormat at 200, length.
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 200));
		file.extend_from_slice(&entry(0x0202, 4, 1, 400));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(600, 0);

		let mut src = MemSource(file);
		let index = scan(&mut src);
		assert_eq!(index.orientation, 6);
		let preview = index.preview.expect("the SubIFD JPEG must be found");
		assert_eq!(preview.dims(), (1600, 1200));
	}

	/// A RAW with `jpeg` in a SubIFD at 200, declared as 400 bytes (a head
	/// alone is under `MIN_PREVIEW_BYTES`), and orientation `o` in IFD0 — the
	/// shape `a_subifd_jpeg_is_found_and_measured_by_its_own_sof` proves.
	fn subifd_raw(jpeg: &[u8], o: u32) -> Vec<u8> {
		let mut file = header(8);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, o));
		file.extend_from_slice(&entry(0x014A, 4, 1, 60));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(60, 0);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 200));
		file.extend_from_slice(&entry(0x0202, 4, 1, 400));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(jpeg);
		file.resize(600, 0);
		file
	}

	/// The locate path answers with the range the container declares, the
	/// dimensions the JPEG's own SOF declares, and the container's
	/// orientation — and nothing was decoded to learn any of it.
	#[test]
	fn a_preview_is_located_without_being_decoded() {
		let jpeg = jpeg_head(1600, 1200);
		let mut src = MemSource(subifd_raw(&jpeg, 6));
		assert_eq!(
			locate_preview(&mut src).unwrap(),
			Some(LocatedPreview {
				offset: 200,
				len: 400,
				width: 1600,
				height: 1200,
				orientation: 6,
				exif_insert_at: Some(8),
				stream_orientation: None,
			})
		);
		// Bytes no format claims, and a container with nothing in it.
		assert_eq!(
			locate_preview(&mut MemSource(b"nonsense".to_vec())).unwrap(),
			None
		);
		assert_eq!(locate_preview(&mut MemSource(header(8))).unwrap(), None);
	}

	/// A little-endian TIFF whose IFD0 holds one Orientation entry, wrapped
	/// as an EXIF APP1 — what a camera writes into its own preview.
	fn exif_app1(orientation: u16) -> Vec<u8> {
		let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
		tiff.extend_from_slice(&1u16.to_le_bytes());
		tiff.extend_from_slice(&0x0112u16.to_le_bytes());
		tiff.extend_from_slice(&3u16.to_le_bytes());
		tiff.extend_from_slice(&1u32.to_le_bytes());
		tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
		tiff.extend_from_slice(&0u32.to_le_bytes());
		let mut app1 = vec![0xFF, 0xE1];
		app1.extend_from_slice(&u16::try_from(2 + 6 + tiff.len()).unwrap().to_be_bytes());
		app1.extend_from_slice(b"Exif\0\0");
		app1.extend_from_slice(&tiff);
		app1
	}

	/// A stream with an EXIF of its own is left alone, and what that EXIF
	/// says is reported, because it — not the container — is what a viewer
	/// handed the bytes will apply. Fuji and Panasonic previews are this.
	#[test]
	fn a_stream_with_its_own_exif_reports_that_orientation() {
		let mut jpeg = vec![0xFF, 0xD8];
		jpeg.extend_from_slice(&exif_app1(6));
		jpeg.extend_from_slice(&jpeg_head(1600, 1200)[2..]);
		let located = locate_preview(&mut MemSource(subifd_raw(&jpeg, 1)))
			.unwrap()
			.expect("a preview");
		assert_eq!(located.exif_insert_at, None);
		assert_eq!(located.stream_orientation, Some(6));
		assert_eq!(located.orientation, 1);
		// An EXIF that declares no orientation still counts as the stream's
		// own say: upright, and nothing to splice.
		let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE1, 0x00, 0x10];
		jpeg.extend_from_slice(b"Exif\0\0II\x2a\x00\x08\x00\x00\x00");
		jpeg.extend_from_slice(&jpeg_head(1600, 1200)[2..]);
		let located = locate_preview(&mut MemSource(subifd_raw(&jpeg, 8)))
			.unwrap()
			.expect("a preview");
		assert_eq!(located.stream_orientation, Some(1));
		assert_eq!(located.orientation, 8);
	}

	/// A declared length no JPEG of that size could have is the container
	/// describing the sensor data behind the preview, not the preview; it is
	/// refused rather than streamed out. The thumbnail path, which only ever
	/// reads what the decoder asks for, still serves it.
	#[test]
	fn a_length_no_jpeg_of_that_size_could_have_is_refused() {
		let jpeg = jpeg_head(640, 480);
		let mut file = subifd_raw(&jpeg, 1);
		// Rewrite the declared length (entry 0x0202 in the SubIFD at 60) to
		// far more than 640x480 pixels could ever compress to, and pad the
		// file so the range still fits.
		let declared: u32 = 640 * 480 * 4 + 2 * 1024 * 1024;
		file[60 + 2 + 12 + 8..60 + 2 + 12 + 12].copy_from_slice(&declared.to_le_bytes());
		file.resize(200 + declared as usize, 0);
		assert!(scan(&mut MemSource(file.clone())).preview.is_some());
		assert_eq!(locate_preview(&mut MemSource(file)).unwrap(), None);
	}

	/// The two candidate kinds do not race for one slot: a bigger
	/// uncompressed strip is the better THUMBNAIL source, and the JPEG beside
	/// it is still the preview.
	#[test]
	fn a_bigger_rgb_strip_does_not_hide_the_jpeg_from_the_preview_path() {
		let jpeg = jpeg_head(640, 480);
		// IFD0: a reduced-resolution 1024x768 RGB strip in one strip, then a
		// SubIFD holding the JPEG.
		let mut file = header(8);
		file.extend_from_slice(&7u16.to_le_bytes());
		file.extend_from_slice(&entry(0x00FE, 4, 1, 1));
		file.extend_from_slice(&entry(0x0100, 4, 1, 1024));
		file.extend_from_slice(&entry(0x0101, 4, 1, 768));
		file.extend_from_slice(&entry(0x0106, 3, 1, 2));
		file.extend_from_slice(&entry(0x0111, 4, 1, 4096));
		file.extend_from_slice(&entry(0x0117, 4, 1, 1024 * 768 * 3));
		file.extend_from_slice(&entry(0x014A, 4, 1, 120));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(120, 0);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 200));
		file.extend_from_slice(&entry(0x0202, 4, 1, 400));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(4096 + 1024 * 768 * 3, 0x40);
		let index = scan(&mut MemSource(file.clone()));
		assert_eq!(index.preview.map(|p| p.dims()), Some((1024, 768)));
		assert_eq!(index.jpeg.map(|p| p.dims()), Some((640, 480)));
		let located = locate_preview(&mut MemSource(file))
			.unwrap()
			.expect("the JPEG is still the preview");
		assert_eq!((located.width, located.height), (640, 480));
	}

	/// What the preview path refuses that the thumbnail path still serves: a
	/// stamp under the floor, and JPEGs a viewer would not decode — 12-bit,
	/// or four components. Each is still a fine thumbnail source.
	#[test]
	fn a_preview_a_viewer_cannot_show_is_not_located() {
		let small = subifd_raw(&jpeg_head(400, 300), 1);
		assert!(scan(&mut MemSource(small.clone())).preview.is_some());
		assert_eq!(locate_preview(&mut MemSource(small)).unwrap(), None);

		let mut twelve_bit = jpeg_head(1600, 1200);
		twelve_bit[12] = 12;
		let twelve_bit = subifd_raw(&twelve_bit, 1);
		assert!(scan(&mut MemSource(twelve_bit.clone())).preview.is_some());
		assert_eq!(locate_preview(&mut MemSource(twelve_bit)).unwrap(), None);

		let mut cmyk = jpeg_head(1600, 1200);
		cmyk[17] = 4;
		let cmyk = subifd_raw(&cmyk, 1);
		assert!(scan(&mut MemSource(cmyk.clone())).preview.is_some());
		assert_eq!(locate_preview(&mut MemSource(cmyk)).unwrap(), None);
	}

	/// Hostile directory graphs: a self-referential SubIFD, a directory
	/// claiming millions of entries, and offsets past the end of the file.
	/// None of these may hang, panic or read out of bounds.
	#[test]
	fn forged_directories_are_refused_without_panicking() {
		// A SubIFD pointing back at IFD0, and IFD0 chaining to itself.
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x014A, 4, 1, 8));
		file.extend_from_slice(&8u32.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// An absurd entry count.
		let mut file = header(8);
		file.extend_from_slice(&60000u16.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Every offset past the end.
		let mut file = header(8);
		file.extend_from_slice(&3u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, u32::MAX));
		file.extend_from_slice(&entry(0x0202, 4, 1, u32::MAX));
		file.extend_from_slice(&entry(0x014A, 4, 1, u32::MAX - 1));
		file.extend_from_slice(&u32::MAX.to_le_bytes());
		file.resize(128, 0);
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Truncated mid-directory.
		let mut file = header(8);
		file.extend_from_slice(&4u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, 8));
		assert!(scan(&mut MemSource(file)).preview.is_none());

		// Not a TIFF at all, and empty.
		assert!(scan(&mut MemSource(b"nonsense".to_vec())).preview.is_none());
		assert!(scan(&mut MemSource(Vec::new())).preview.is_none());
	}

	/// A directory graph wide enough to exhaust the caps must terminate, and
	/// must not have read the whole file to decide that.
	#[test]
	fn a_wide_forged_graph_terminates() {
		// 256 SubIFDs, each pointing at the next, 4 KB apart.
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x014A, 4, 1, 4096));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(4096, 0);
		for i in 1..256u32 {
			let mut dir = 1u16.to_le_bytes().to_vec();
			dir.extend_from_slice(&entry(0x014A, 4, 1, (i + 1) * 4096));
			dir.extend_from_slice(&0u32.to_le_bytes());
			file.extend_from_slice(&dir);
			file.resize(((i + 1) * 4096) as usize, 0);
		}
		let mut src = MemSource(file);
		assert!(scan(&mut src).preview.is_none());
	}

	/// The uncompressed path only fires when the byte count matches the
	/// geometry exactly — the check that stops a full-size uncompressed TIFF
	/// from being mistaken for a thumbnail.
	#[test]
	fn an_uncompressed_strip_is_believed_only_when_its_size_adds_up() {
		let build = |byte_count: u32| {
			let mut file = header(8);
			file.extend_from_slice(&6u16.to_le_bytes());
			file.extend_from_slice(&entry(0x00FE, 4, 1, 1));
			file.extend_from_slice(&entry(0x0100, 4, 1, 32));
			file.extend_from_slice(&entry(0x0101, 4, 1, 8));
			file.extend_from_slice(&entry(0x0106, 3, 1, 2));
			file.extend_from_slice(&entry(0x0111, 4, 1, 200));
			file.extend_from_slice(&entry(0x0117, 4, 1, byte_count));
			file.extend_from_slice(&0u32.to_le_bytes());
			file.resize(200, 0);
			file.resize(1200, 0x40);
			MemSource(file)
		};
		let found = scan(&mut build(768)).preview.expect("32x8x3 = 768 adds up");
		assert_eq!(found.dims(), (32, 8));
		assert!(scan(&mut build(900)).preview.is_none());
	}

	/// Maker-note tags must never be read with TIFF's vocabulary: `0x0100` is
	/// `ImageWidth` in one and a JPEG blob in the other.
	#[test]
	fn maker_note_vocabulary_is_separate() {
		let jpeg = jpeg_head(640, 480);
		let mut file = header(8);
		file.extend_from_slice(&1u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0100, 7, 300, 200));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(200, 0);
		file.extend_from_slice(&jpeg);
		file.resize(600, 0);
		// Read as TIFF, 0x0100 is a width and there is no candidate at all.
		assert!(scan(&mut MemSource(file.clone())).preview.is_none());
		// Read as a maker note, it is the thumbnail.
		let mut src = MemSource(file);
		let mut walk = Walk {
			src: &mut src,
			visited: Vec::new(),
			candidates: Vec::new(),
			orientation: None,
			maker_note: None,
		};
		walk.walk(Endian::Little, 0, 8, 0, Vocab::Maker);
		assert_eq!(walk.candidates.len(), 1);
	}

	/// Finding nothing must stay cheap: a big file whose directories promise
	/// no preview is rejected after a handful of small reads.
	#[test]
	fn a_fruitless_scan_reads_only_a_prefix() {
		struct Counting(MemSource, u64);
		impl ByteSource for Counting {
			fn len(&self) -> u64 {
				self.0.len()
			}
			fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
				let n = self.0.read_at(offset, buf)?;
				self.1 += n as u64;
				Ok(n)
			}
		}
		let mut file = header(8);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, 1));
		file.extend_from_slice(&entry(0x0106, 3, 1, 32803));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(8 * 1024 * 1024, 0);
		let mut src = Counting(MemSource(file), 0);
		assert!(scan(&mut src).preview.is_none());
		assert!(src.1 < 4096, "read {} bytes to find nothing", src.1);
	}
}
