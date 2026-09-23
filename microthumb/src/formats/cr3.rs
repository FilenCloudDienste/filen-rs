//! Canon CR3, via the preview image in its ISO-BMFF box tree.
//!
//! CR3 broke with the TIFF lineage every other Canon RAW follows: it is an
//! MP4-shaped container, so none of the directory machinery in [`super::raw`]
//! applies. What survives is the principle — find the JPEG Canon stored
//! alongside the mosaic and decode that — so all this module does is walk to
//! the box holding it.
//!
//! Three JPEGs are in there. Two sit in boxes nested inside `uuid` boxes
//! Canon defines: `PRVW` (1620x1080 across every pinned body, from the EOS R
//! to the PowerShot V1) and `THMB` (160x120). The third is the whole picture
//! at sensor resolution, stored as the single sample of the first track —
//! `moov` > `trak` > `mdia` > `minf` > `stbl`, where `stsd` names the codec
//! (a `JPEG` box inside the `CRAW` sample entry; the sensor tracks carry
//! `CMP1` there instead), `stsz` the sample's size and `co64` its offset in
//! `mdat`. All three are bare JPEGs — SOI straight into the quantisation
//! tables, no APP1 — so the shot's orientation is not in any stream. It is in
//! `CMT1`, the box beside them that carries the file's EXIF IFD0 as a plain
//! TIFF.
//!
//! Which JPEG serves depends on the caller. A thumbnail takes the largest
//! box: an IDCT-scaled decode still reads every byte of the entropy stream,
//! so decoding the full picture would cost twenty times PRVW's bytes for the
//! same 256 px tile. A preview takes the largest of all, the full picture
//! included — it is the only rendering of the shot a viewer will ever get.
//!
//! The walk is its own bounded parser rather than a media library: box sizes
//! are attacker-controlled, so every one is checked against its parent's
//! extent, recursion is depth-capped, the total number of boxes visited is
//! capped, and no read is ever sized from a header without that check.

use crate::{
	ByteSource, FormatDecoder, LocatedPreview, PreparedDecode, ThumbError, ThumbSpec, exif,
};

use super::raw::{self, Index, Preview};

pub struct Cr3;

/// Boxes visited across the whole tree.
const MAX_BOXES: usize = 256;
/// How deep the containers nest before the walk stops. `moov` -> `trak` ->
/// `mdia` -> `minf` is the deepest real path; `stbl`'s own children are read
/// flat by [`Walk::sample_table`].
const MAX_DEPTH: u8 = 4;
/// A box header is 8 bytes, or 16 when the size word is the `1` escape.
const BOX_HEADER: u64 = 8;
/// Bytes of a preview box's payload searched for the SOI. Both box types put
/// their JPEG at offset 16, behind a small fixed descriptor.
const PREVIEW_PROBE: u64 = 32;
/// Most of `CMT1` read for the orientation tag. The box is IFD0 alone — make,
/// model, timestamps, a few hundred bytes on every pinned body — so this caps
/// a forged box size rather than budgeting a real one.
const CMT1_MAX_BYTES: u64 = 64 * 1024;
/// Most of an `stsd` read to name a track's codec. A `CRAW` sample entry is
/// ~120–240 bytes on every pinned body.
const STSD_MAX_BYTES: u64 = 4096;

/// The JPEGs the file carries, and its orientation.
///
/// `with_tracks` says whether the full picture in the first track is looked
/// at as well: its head is a read at an `mdat` offset, which over the network
/// is a chunk the thumbnail path never needs.
///
/// The JPEGs carry no EXIF of their own, so a portrait shot would come out
/// sideways without the file-level tag. `CMT1` is a TIFF, which is exactly the
/// payload the EXIF walker takes.
fn index(src: &mut dyn ByteSource, with_tracks: bool) -> Index {
	let mut walk = Walk {
		boxes: 0,
		previews: Vec::new(),
		tracks: Vec::new(),
		cmt1: None,
	};
	let end = src.len();
	walk.boxes_in(src, 0, end, 0);
	let mut believe = |claims: Vec<(u64, u64)>| {
		claims
			.into_iter()
			.filter_map(|(off, len)| raw::jpeg_window(src, off, len))
			.max_by_key(Preview::area)
	};
	let boxed = believe(walk.previews);
	let track = if with_tracks {
		believe(walk.tracks)
	} else {
		None
	};
	let orientation = walk
		.cmt1
		.and_then(|(body, body_end)| {
			raw::read_exact_at(src, body, (body_end - body).min(CMT1_MAX_BYTES))
		})
		.map_or(1, |tiff| exif::orientation(&tiff));
	Index {
		orientation,
		// The box first: see the module doc for why a thumbnail never wants
		// the full picture. The track only when there is no box at all —
		// which `open` asks for only once it has seen that there is none.
		preview: boxed.or(track),
		jpeg: [boxed, track]
			.into_iter()
			.flatten()
			.max_by_key(Preview::area),
	}
}

impl FormatDecoder for Cr3 {
	fn detect(&self, prefix: &[u8]) -> bool {
		// `ftyp` with Canon's `crx ` brand. Checking the brand as well as the
		// box keeps ordinary MP4s — which nothing here decodes — from being
		// claimed and then refused.
		prefix.get(4..12) == Some(b"ftypcrx ")
	}

	fn locate_preview(
		&self,
		src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(raw::locate(index(src, true)))
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut found = index(&mut *src, false);
		if found.preview.is_none() {
			// No preview box at all: the picture track is the only JPEG
			// left, and its head is worth the chunk when the alternative is
			// no thumbnail.
			found = index(&mut *src, true);
		}
		let Index {
			orientation,
			preview: Some(preview),
			..
		} = found
		else {
			return Err(ThumbError::Decode(
				"cr3: no preview box; the sensor data is not a decodable image".into(),
			));
		};
		raw::prepare(src, preview, orientation, spec)
	}
}

struct Walk {
	boxes: usize,
	/// Byte ranges the preview boxes claim hold a JPEG, unverified.
	previews: Vec<(u64, u64)>,
	/// Byte ranges the sample tables claim hold a JPEG-coded track's one
	/// sample, unverified.
	tracks: Vec<(u64, u64)>,
	/// Payload range of the first `CMT1` box: the file's EXIF IFD0.
	cmt1: Option<(u64, u64)>,
}

/// One box header at `at`, as `(kind, body, body_end)`, or `None` when it
/// does not fit inside `end` — a tree that is forged or truncated, where the
/// caller stops rather than guessing where the next box starts.
pub(super) fn read_box(src: &mut dyn ByteSource, at: u64, end: u64) -> Option<([u8; 4], u64, u64)> {
	let header = raw::read_exact_at(src, at, BOX_HEADER)?;
	let kind: [u8; 4] = header[4..8].try_into().expect("four bytes");
	let mut size = u64::from(u32::from_be_bytes(
		header[0..4].try_into().expect("four bytes"),
	));
	let mut header_len = BOX_HEADER;
	match size {
		// The 64-bit escape.
		1 => {
			let large = raw::read_exact_at(src, at + BOX_HEADER, 8)?;
			size = u64::from_be_bytes(large.try_into().expect("eight bytes"));
			header_len = 16;
		}
		// "To the end of the enclosing box."
		0 => size = end - at,
		_ => {}
	}
	if size < header_len || at.checked_add(size).is_none_or(|box_end| box_end > end) {
		return None;
	}
	Some((kind, at + header_len, at + size))
}

impl Walk {
	fn boxes_in(&mut self, src: &mut dyn ByteSource, start: u64, end: u64, depth: u8) {
		if depth > MAX_DEPTH {
			return;
		}
		let mut at = start;
		while at + BOX_HEADER <= end && self.boxes < MAX_BOXES {
			self.boxes += 1;
			let Some((kind, body, body_end)) = read_box(src, at, end) else {
				return;
			};
			match &kind {
				b"PRVW" | b"THMB" => self.preview_box(src, body, body_end),
				b"CMT1" => {
					if self.cmt1.is_none() {
						self.cmt1 = Some((body, body_end));
					}
				}
				b"moov" | b"trak" | b"mdia" | b"minf" => {
					self.boxes_in(src, body, body_end, depth + 1)
				}
				b"stbl" => self.sample_table(src, body, body_end),
				// A `uuid` box names itself with 16 bytes and then, in Canon's
				// case, either starts its children immediately or first writes
				// a version word. Rather than hard-code which UUID does which,
				// look at whether a box header is actually there.
				b"uuid" => {
					if let Some(child) = child_start(src, body + 16, body_end) {
						self.boxes_in(src, child, body_end, depth + 1);
					}
				}
				_ => {}
			}
			at = body_end;
		}
	}

	/// A `PRVW` / `THMB` payload: a short fixed descriptor, then the JPEG. The
	/// descriptor's own width and height fields differ in layout between the
	/// two box types, so the SOI is found by looking rather than by trusting
	/// either, and the dimensions come from the JPEG itself.
	fn preview_box(&mut self, src: &mut dyn ByteSource, body: u64, body_end: u64) {
		let probe_len = PREVIEW_PROBE.min(body_end.saturating_sub(body));
		let Some(probe) = raw::read_exact_at(src, body, probe_len) else {
			return;
		};
		let Some(soi) = probe.windows(3).position(|w| w == [0xFF, 0xD8, 0xFF]) else {
			return;
		};
		let off = body + soi as u64;
		self.previews.push((off, body_end - off));
	}

	/// A track's sample table, read flat: the codec its sample entry names,
	/// the size of its first sample and where that sample sits. Only a track
	/// whose entry carries a `JPEG` box becomes a candidate — the sensor
	/// tracks (`CMP1`) would cost a read at their `mdat` offset just to be
	/// refused for having no SOI, and over the network that read is a chunk.
	fn sample_table(&mut self, src: &mut dyn ByteSource, body: u64, end: u64) {
		let (mut jpeg, mut size, mut offset) = (false, None, None);
		let mut at = body;
		while at + BOX_HEADER <= end && self.boxes < MAX_BOXES {
			self.boxes += 1;
			let Some((kind, body, body_end)) = read_box(src, at, end) else {
				return;
			};
			let len = body_end - body;
			match &kind {
				b"stsd" => {
					// Version and flags, an entry count, then the `CRAW`
					// sample entry: the standard 78-byte visual entry, a few
					// bytes Canon adds, then the codec box. The tail makes a
					// fixed offset a guess, so the codec box is found by its
					// name; the stream is verified by its own SOI and SOF
					// either way.
					if let Some(stsd) = raw::read_exact_at(src, body, len.min(STSD_MAX_BYTES)) {
						jpeg = stsd
							.get(8..)
							.is_some_and(|entries| entries.windows(4).any(|w| w == b"JPEG"));
					}
				}
				// Version and flags, a sample size that applies to every
				// sample when non-zero, a count, then per-sample sizes.
				b"stsz" => {
					if let Some(stsz) = raw::read_exact_at(src, body, len.min(16)) {
						let shared = be_u32(&stsz, 4);
						let count = be_u32(&stsz, 8);
						size = match (shared, count) {
							(Some(0), Some(n)) if n >= 1 => be_u32(&stsz, 12).map(u64::from),
							(Some(shared), _) if shared > 0 => Some(u64::from(shared)),
							_ => None,
						};
					}
				}
				// Version and flags, a count, then chunk offsets — 64-bit in
				// `co64`, 32-bit in `stco`. One sample per chunk here.
				b"co64" => {
					if let Some(co64) = raw::read_exact_at(src, body, len.min(16))
						&& be_u32(&co64, 4).is_some_and(|n| n >= 1)
						&& let Some(bytes) = co64.get(8..16)
					{
						offset = Some(u64::from_be_bytes(bytes.try_into().expect("eight bytes")));
					}
				}
				b"stco" => {
					if let Some(stco) = raw::read_exact_at(src, body, len.min(12))
						&& be_u32(&stco, 4).is_some_and(|n| n >= 1)
					{
						offset = be_u32(&stco, 8).map(u64::from);
					}
				}
				_ => {}
			}
			at = body_end;
		}
		if jpeg && let (Some(offset), Some(size)) = (offset, size) {
			self.tracks.push((offset, size));
		}
	}
}

fn be_u32(data: &[u8], at: usize) -> Option<u32> {
	data.get(at..at + 4)
		.map(|b| u32::from_be_bytes(b.try_into().expect("four bytes")))
}

/// Where a `uuid` box's children begin: right after the UUID, or 8 bytes
/// further on when what sits there is a version word rather than a box.
fn child_start(src: &mut dyn ByteSource, at: u64, end: u64) -> Option<u64> {
	for candidate in [at, at + 8] {
		if candidate + BOX_HEADER > end {
			continue;
		}
		let header = raw::read_exact_at(src, candidate, BOX_HEADER)?;
		// Every box type in this container is four printable characters; a
		// version word is not.
		if header[4..8].iter().all(u8::is_ascii_graphic) {
			return Some(candidate);
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::{Cr3, Walk};
	use crate::{ByteSource, FormatDecoder, MemSource, ThumbSpec, locate_preview};

	fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
		let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
		out.extend_from_slice(kind);
		out.extend_from_slice(body);
		out
	}

	/// A baseline JPEG head, enough for the SOF scan to measure it.
	fn jpeg(w: u16, h: u16) -> Vec<u8> {
		let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
		out.extend_from_slice(&h.to_be_bytes());
		out.extend_from_slice(&w.to_be_bytes());
		out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
		out.resize(256, 0);
		out
	}

	/// `PRVW` / `THMB` bodies: a 16-byte descriptor, then the JPEG.
	fn preview_body(w: u16, h: u16) -> Vec<u8> {
		let mut body = vec![0u8; 16];
		body.extend_from_slice(&jpeg(w, h));
		body
	}

	fn found(file: Vec<u8>) -> Option<(u32, u32)> {
		let mut src = MemSource(file);
		let mut walk = Walk {
			boxes: 0,
			previews: Vec::new(),
			tracks: Vec::new(),
			cmt1: None,
		};
		let end = src.len();
		walk.boxes_in(&mut src, 0, end, 0);
		walk.previews
			.into_iter()
			.filter_map(|(off, len)| super::raw::jpeg_window(&mut src, off, len))
			.max_by_key(super::Preview::area)
			.map(|p| p.dims())
	}

	/// The real layout: a `uuid` box whose payload starts with a version word
	/// before its children, and a `moov` whose Canon `uuid` starts with a box
	/// straight away. Both must be walked, and the larger preview must win.
	#[test]
	fn both_canon_uuid_layouts_are_walked_and_the_larger_preview_wins() {
		let mut versioned = vec![0u8; 16];
		versioned.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
		versioned.extend_from_slice(&boxed(b"PRVW", &preview_body(1620, 1080)));

		let mut immediate = vec![0u8; 16];
		immediate.extend_from_slice(&boxed(b"THMB", &preview_body(160, 120)));

		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"moov", &boxed(b"uuid", &immediate)));
		file.extend_from_slice(&boxed(b"uuid", &versioned));
		assert!(Cr3.detect(&file));
		assert_eq!(found(file), Some((1620, 1080)));
	}

	/// Forged box trees: sizes that do not fit their parent, the "rest of the
	/// parent" and 64-bit escapes, and a descriptor with no JPEG behind it.
	/// None may panic, hang or read out of bounds.
	#[test]
	fn forged_box_trees_are_refused_without_panicking() {
		// A child claiming to be bigger than its parent.
		let mut oversized = vec![0u8; 16];
		let mut lying = boxed(b"PRVW", &preview_body(1620, 1080));
		lying[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
		oversized.extend_from_slice(&lying);
		assert_eq!(found(boxed(b"uuid", &oversized)), None);

		// A size word smaller than the header it sits in.
		let mut runt = boxed(b"uuid", &[0u8; 32]);
		runt[0..4].copy_from_slice(&4u32.to_be_bytes());
		assert_eq!(found(runt), None);

		// The 64-bit escape, pointing past the end.
		let mut escape = 1u32.to_be_bytes().to_vec();
		escape.extend_from_slice(b"PRVW");
		escape.extend_from_slice(&u64::MAX.to_be_bytes());
		escape.extend_from_slice(&preview_body(800, 600));
		assert_eq!(found(escape), None);

		// A preview box whose descriptor is not followed by a JPEG.
		assert_eq!(found(boxed(b"PRVW", &[0x41u8; 200])), None);

		// A preview box with nothing in it at all.
		assert_eq!(found(boxed(b"PRVW", &[])), None);

		// Truncated headers and empty input.
		assert_eq!(found(b"\x00\x00\x00".to_vec()), None);
		assert_eq!(found(Vec::new()), None);
	}

	/// A tree wide enough to exhaust the box budget must terminate. A
	/// zero-size box would otherwise never advance the cursor.
	#[test]
	fn a_forged_tree_terminates() {
		// `size == 0` means "to the end of the parent", so the walk must treat
		// it as consuming the rest rather than looping on a zero-width box.
		let mut zero = 0u32.to_be_bytes().to_vec();
		zero.extend_from_slice(b"free");
		zero.extend_from_slice(&[0u8; 64]);
		assert_eq!(found(zero), None);

		// Thousands of empty boxes: capped, not walked.
		let mut many = Vec::new();
		for _ in 0..4096 {
			many.extend_from_slice(&boxed(b"free", &[]));
		}
		assert_eq!(found(many), None);

		// Deeply nested uuid boxes: depth-capped.
		let mut nested = boxed(b"PRVW", &preview_body(320, 240));
		for _ in 0..16 {
			let mut wrapper = vec![0u8; 16];
			wrapper.extend_from_slice(&nested);
			nested = boxed(b"uuid", &wrapper);
		}
		assert_eq!(found(nested), None);
	}

	/// Only Canon's brand is claimed. An ordinary MP4 has to fall through to
	/// the "unsupported" answer, not be claimed and then refused.
	#[test]
	fn only_the_canon_brand_is_claimed() {
		assert!(!Cr3.detect(b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00"));
		assert!(!Cr3.detect(b"not a container"));
		assert!(!Cr3.detect(b""));
		// Claimed but empty: an error, and no panic.
		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"mdat", &[0u8; 64]));
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		assert!(Cr3.open(Box::new(MemSource(file)), &spec).is_err());
	}

	/// A real, decodable JPEG: `raw::prepare` opens the preview with the JPEG
	/// decoder, which needs more than an SOF to say yes.
	fn real_jpeg(w: u16, h: u16) -> Vec<u8> {
		let mut out = Vec::new();
		let enc = jpeg_encoder::Encoder::new(&mut out, 80);
		enc.encode(
			&vec![0x80u8; usize::from(w) * usize::from(h) * 3],
			w,
			h,
			jpeg_encoder::ColorType::Rgb,
		)
		.expect("encode");
		out
	}

	/// `CMT1`'s payload: a little-endian TIFF whose IFD0 holds one
	/// Orientation entry.
	fn cmt1(orientation: u16) -> Vec<u8> {
		let mut tiff = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
		tiff.extend_from_slice(&1u16.to_le_bytes());
		tiff.extend_from_slice(&0x0112u16.to_le_bytes());
		tiff.extend_from_slice(&3u16.to_le_bytes());
		tiff.extend_from_slice(&1u32.to_le_bytes());
		tiff.extend_from_slice(&u32::from(orientation).to_le_bytes());
		tiff.extend_from_slice(&0u32.to_le_bytes());
		tiff
	}

	/// The real layout: `moov` -> Canon `uuid` -> `CMT1` beside `THMB`, and
	/// the `PRVW` in its own versioned `uuid`.
	fn cr3(cmt1_body: Option<&[u8]>) -> Vec<u8> {
		cr3_with_track(cmt1_body, None)
	}

	/// A `trak` whose one sample is `sample_size` bytes at `offset`, coded as
	/// `codec` — the `stsd` layout of every pinned body: version and flags,
	/// a count, a `CRAW` entry with its 78 standard bytes, four of Canon's,
	/// and the codec box.
	fn track(codec: &[u8; 4], offset: u64, sample_size: u32) -> Vec<u8> {
		let mut entry = vec![0u8; 78 + 4];
		entry.extend_from_slice(&boxed(codec, &[0u8; 4]));
		let mut stsd = vec![0, 0, 0, 0, 0, 0, 0, 1];
		stsd.extend_from_slice(&boxed(b"CRAW", &entry));
		let mut stsz = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
		stsz.extend_from_slice(&sample_size.to_be_bytes());
		let mut co64 = vec![0, 0, 0, 0, 0, 0, 0, 1];
		co64.extend_from_slice(&offset.to_be_bytes());
		let mut stbl = boxed(b"stsd", &stsd);
		stbl.extend_from_slice(&boxed(b"stsz", &stsz));
		stbl.extend_from_slice(&boxed(b"co64", &co64));
		boxed(
			b"trak",
			&boxed(b"mdia", &boxed(b"minf", &boxed(b"stbl", &stbl))),
		)
	}

	/// [`cr3`], plus a first track whose sample is `sample` in an `mdat`
	/// after everything else, described as `codec`. The track's boxes hold
	/// the sample's absolute offset, so the file is laid out twice: once to
	/// measure, once for real.
	fn cr3_with_track(
		cmt1_body: Option<&[u8]>,
		track_sample: Option<(&[u8; 4], &[u8])>,
	) -> Vec<u8> {
		cr3_layout(cmt1_body, track_sample, true)
	}

	/// `with_boxes` false leaves out both preview boxes, PRVW and THMB.
	fn cr3_layout(
		cmt1_body: Option<&[u8]>,
		track_sample: Option<(&[u8; 4], &[u8])>,
		with_boxes: bool,
	) -> Vec<u8> {
		let build = |mdat_at: u64| {
			let mut canon = vec![0u8; 16];
			if let Some(body) = cmt1_body {
				canon.extend_from_slice(&boxed(b"CMT1", body));
			}
			if with_boxes {
				let mut thmb = vec![0u8; 16];
				thmb.extend_from_slice(&real_jpeg(160, 120));
				canon.extend_from_slice(&boxed(b"THMB", &thmb));
			}
			let mut moov = boxed(b"uuid", &canon);
			if let Some((codec, sample)) = track_sample {
				moov.extend_from_slice(&track(codec, mdat_at, sample.len() as u32));
			}

			let mut file = boxed(b"ftyp", b"crx iso");
			file.extend_from_slice(&boxed(b"moov", &moov));
			if with_boxes {
				let mut versioned = vec![0u8; 16];
				versioned.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
				let mut prvw = vec![0u8; 16];
				prvw.extend_from_slice(&real_jpeg(640, 480));
				versioned.extend_from_slice(&boxed(b"PRVW", &prvw));
				file.extend_from_slice(&boxed(b"uuid", &versioned));
			}
			file
		};
		let head = build(0);
		let mdat_at = head.len() as u64 + 8;
		let mut file = build(mdat_at);
		if let Some((_, sample)) = track_sample {
			file.extend_from_slice(&boxed(b"mdat", sample));
		}
		file
	}

	fn orientation_of(file: Vec<u8>) -> u8 {
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		let prepared = Cr3
			.open(Box::new(MemSource(file)), &spec)
			.expect("a preview");
		assert_eq!(prepared.dims(), (640, 480), "the PRVW must win over THMB");
		prepared.orientation()
	}

	/// The preview JPEGs are bare — no APP1 — so the only orientation is the
	/// file's, in `CMT1`. Reading it is what keeps a portrait shot upright:
	/// before it was read, every portrait CR3 came out on its side.
	#[test]
	fn the_orientation_comes_from_cmt1_not_the_bare_preview() {
		assert_eq!(orientation_of(cr3(Some(&cmt1(6)))), 6);
		assert_eq!(orientation_of(cr3(Some(&cmt1(1)))), 1);
		assert_eq!(orientation_of(cr3(None)), 1);
	}

	/// Located rather than decoded: the PRVW's own range and SOF dimensions,
	/// the `CMT1` orientation, and a splice point behind the JFIF APP0 the
	/// encoder wrote — the preview carries no EXIF, so the rotation has to be
	/// spliced in for a viewer to see it.
	#[test]
	fn the_prvw_is_located_with_the_cmt1_orientation() {
		let file = cr3(Some(&cmt1(8)));
		let prvw = real_jpeg(640, 480);
		let offset = file
			.windows(prvw.len())
			.position(|w| w == prvw.as_slice())
			.expect("the PRVW JPEG is in the file") as u64;
		let located = locate_preview(&mut MemSource(file))
			.unwrap()
			.expect("a preview");
		assert_eq!((located.offset, located.len), (offset, prvw.len() as u64));
		assert_eq!((located.width, located.height), (640, 480));
		assert_eq!(located.orientation, 8);
		// SOI (2), then the 16-byte JFIF APP0 with its marker and length.
		assert_eq!(located.exif_insert_at, Some(2 + 2 + 16));
		// THMB alone is under the preview floor: a thumbnail, not a preview.
		let mut thmb_only = vec![0u8; 16];
		let mut thmb = vec![0u8; 16];
		thmb.extend_from_slice(&real_jpeg(160, 120));
		thmb_only.extend_from_slice(&boxed(b"THMB", &thmb));
		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"moov", &boxed(b"uuid", &thmb_only)));
		let spec = ThumbSpec::new(64, 64, crate::DEFAULT_MEM_BUDGET);
		assert!(Cr3.open(Box::new(MemSource(file.clone())), &spec).is_ok());
		assert_eq!(locate_preview(&mut MemSource(file)).unwrap(), None);
	}

	/// The whole picture in the first track is the preview; PRVW stays the
	/// thumbnail source, because an IDCT-scaled decode still reads every byte
	/// of the stream it shrinks. Both take the `CMT1` orientation.
	#[test]
	fn the_full_size_track_is_the_preview_and_prvw_the_thumbnail() {
		let full = real_jpeg(1600, 1200);
		let file = cr3_with_track(Some(&cmt1(6)), Some((b"JPEG", &full)));
		let offset = file
			.windows(full.len())
			.position(|w| w == full.as_slice())
			.expect("the track JPEG is in the file") as u64;
		let located = locate_preview(&mut MemSource(file.clone()))
			.unwrap()
			.expect("a preview");
		assert_eq!((located.offset, located.len), (offset, full.len() as u64));
		assert_eq!((located.width, located.height), (1600, 1200));
		assert_eq!(located.orientation, 6);
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		let prepared = Cr3
			.open(Box::new(MemSource(file)), &spec)
			.expect("a thumbnail");
		assert_eq!(
			prepared.dims(),
			(640, 480),
			"the thumbnail must come from PRVW"
		);
		assert_eq!(prepared.orientation(), 6);
	}

	/// A file with no preview box at all still thumbnails: the picture track
	/// is the only JPEG left, and `open` walks it only then — a normal CR3
	/// never has its `mdat` read for a thumbnail.
	#[test]
	fn a_box_less_cr3_thumbnails_from_its_track() {
		let full = real_jpeg(1600, 1200);
		let file = cr3_layout(Some(&cmt1(6)), Some((b"JPEG", &full)), false);
		let spec = ThumbSpec::new(512, 512, crate::DEFAULT_MEM_BUDGET);
		let prepared = Cr3
			.open(Box::new(MemSource(file.clone())), &spec)
			.expect("the track serves");
		assert_eq!(prepared.dims(), (1600, 1200));
		assert_eq!(prepared.orientation(), 6);
		let located = locate_preview(&mut MemSource(file))
			.unwrap()
			.expect("a preview");
		assert_eq!((located.width, located.height), (1600, 1200));
	}

	/// A sensor track — `CMP1` in its sample entry — is never a candidate,
	/// even when the bytes at its offset happen to be a JPEG: its head would
	/// otherwise cost a read at an `mdat` offset, a whole chunk over the
	/// network, to be refused. And a track that claims `JPEG` over bytes that
	/// are not one is refused by those bytes.
	#[test]
	fn only_a_jpeg_coded_track_is_a_candidate_and_its_bytes_have_the_last_word() {
		let full = real_jpeg(1600, 1200);
		let crx = cr3_with_track(Some(&cmt1(1)), Some((b"CMP1", &full)));
		let located = locate_preview(&mut MemSource(crx)).unwrap().expect("PRVW");
		assert_eq!((located.width, located.height), (640, 480));

		let lying = cr3_with_track(Some(&cmt1(1)), Some((b"JPEG", &[0x41u8; 4096])));
		let located = locate_preview(&mut MemSource(lying))
			.unwrap()
			.expect("PRVW");
		assert_eq!((located.width, located.height), (640, 480));
	}

	/// Forged sample tables: a size past the file, an offset past the file,
	/// a table with no entries. None may panic; PRVW still serves.
	#[test]
	fn forged_sample_tables_are_refused_without_panicking() {
		let full = real_jpeg(1600, 1200);
		let good = cr3_with_track(Some(&cmt1(1)), Some((b"JPEG", &full)));
		let forge = |stsz_tail: &[u8], co64_tail: &[u8]| {
			let mut file = good.clone();
			// The tables are unique byte strings in the synthetic file.
			let stsz = file.windows(4).position(|w| w == b"stsz").expect("stsz");
			file[stsz + 4..stsz + 4 + stsz_tail.len()].copy_from_slice(stsz_tail);
			let co64 = file.windows(4).position(|w| w == b"co64").expect("co64");
			file[co64 + 4..co64 + 4 + co64_tail.len()].copy_from_slice(co64_tail);
			file
		};
		let mut huge = vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
		huge.extend_from_slice(&u32::MAX.to_be_bytes());
		let mut past = vec![0, 0, 0, 0, 0, 0, 0, 1];
		past.extend_from_slice(&u64::MAX.to_be_bytes());
		// Version and flags, a shared size of 0, a count of 0.
		let no_sizes = vec![0u8; 12];
		// Version and flags, a count of 0.
		let no_offsets = vec![0u8; 8];
		for file in [
			forge(&huge, &[]),
			forge(&[], &past),
			forge(&no_sizes, &[]),
			forge(&[], &no_offsets),
		] {
			let located = locate_preview(&mut MemSource(file)).unwrap().expect("PRVW");
			assert_eq!((located.width, located.height), (640, 480));
		}
	}

	/// `CMT1` is attacker-controlled bytes like every other box: garbage, a
	/// value out of range, an IFD0 pointer past the read cap and an empty box
	/// all fall back to upright, never to a panic.
	#[test]
	fn a_forged_cmt1_falls_back_to_upright() {
		assert_eq!(orientation_of(cr3(Some(&[0x41u8; 64]))), 1);
		assert_eq!(orientation_of(cr3(Some(&cmt1(9)))), 1);
		assert_eq!(orientation_of(cr3(Some(&[]))), 1);
		let mut far = b"II\x2a\x00".to_vec();
		far.extend_from_slice(&u32::MAX.to_le_bytes());
		far.resize(80 * 1024, 0);
		assert_eq!(orientation_of(cr3(Some(&far))), 1);
	}

	/// A source whose reads fail once they reach past `limit`, the way a
	/// remote file's later chunks do when the network drops mid-file.
	struct FailsPast(MemSource, u64);

	impl ByteSource for FailsPast {
		fn len(&self) -> u64 {
			self.0.len()
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			if offset + buf.len() as u64 > self.1 {
				return Err(std::io::Error::other("the network dropped"));
			}
			self.0.read_at(offset, buf)
		}
	}

	/// A read that fails while the preview decodes says nothing about the
	/// file: it has to reach the caller as the io error it is, which the SDK
	/// retries, not as `OverBudget`, which it caches as "no thumbnail". The
	/// preview is noise, so its entropy data runs well past the 8 KiB the JPEG
	/// decoder reads up front, and the walk and the header reads all succeed.
	#[test]
	fn a_read_failure_in_the_preview_is_an_error_not_a_verdict() {
		let mut state = 1u32;
		let noise: Vec<u8> = (0..640 * 480 * 3)
			.map(|_| {
				state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
				(state >> 16) as u8
			})
			.collect();
		let mut preview = Vec::new();
		jpeg_encoder::Encoder::new(&mut preview, 80)
			.encode(&noise, 640, 480, jpeg_encoder::ColorType::Rgb)
			.expect("encode");
		let mut prvw = vec![0u8; 16];
		prvw.extend_from_slice(&preview);
		let mut versioned = vec![0u8; 16];
		versioned.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]);
		versioned.extend_from_slice(&boxed(b"PRVW", &prvw));
		let mut file = boxed(b"ftyp", b"crx iso");
		file.extend_from_slice(&boxed(b"uuid", &versioned));
		let preview_at = (file.len() - preview.len()) as u64;
		assert!(
			preview.len() > 4 * 8192,
			"the preview must outrun the reads ahead of the decode"
		);

		for allow_full_decode in [true, false] {
			let spec = ThumbSpec {
				allow_full_decode,
				..ThumbSpec::new(256, 256, crate::DEFAULT_MEM_BUDGET)
			};
			let source = FailsPast(MemSource(file.clone()), preview_at + 8192);
			match crate::generate(Box::new(source), &spec) {
				Err(crate::ThumbError::Io(_)) => {}
				Err(e) => panic!("full decode {allow_full_decode}: {e}"),
				Ok(outcome) => panic!(
					"full decode {allow_full_decode}: answered {:?} for a failed read",
					outcome.thumbnail().map(|thumb| thumb.source)
				),
			}
		}
	}
}
