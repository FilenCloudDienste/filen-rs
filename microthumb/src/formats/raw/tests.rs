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
