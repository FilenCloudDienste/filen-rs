use super::{Cr3, Walk};
use crate::{ByteSource, FormatDecoder, MemSource, ThumbSpec, locate_preview};

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
	let mut out = u32::try_from(body.len() + 8)
		.unwrap()
		.to_be_bytes()
		.to_vec();
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
fn cr3_with_track(cmt1_body: Option<&[u8]>, track_sample: Option<(&[u8; 4], &[u8])>) -> Vec<u8> {
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
			let sample_size = u32::try_from(sample.len()).unwrap();
			moov.extend_from_slice(&track(codec, mdat_at, sample_size));
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
			(state >> 16).to_le_bytes()[0]
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
