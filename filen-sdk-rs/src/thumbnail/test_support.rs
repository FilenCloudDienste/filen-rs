//! What the thumbnail tests and the archive reader's share: RAW-shaped containers.

use image::{ImageFormat, Rgb, RgbImage};

/// A JPEG of `width` by `height`.
pub(crate) fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
	let image = RgbImage::from_fn(width, height, |x, y| {
		Rgb([(x & 0xFF) as u8, (y & 0xFF) as u8, 0])
	});
	let mut bytes = Vec::new();
	image
		.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
		.unwrap();
	bytes
}

/// Where a SubIFD JPEG sits in [`raw_with`]: past the two directories.
pub(crate) const PREVIEW_AT: u16 = 200;

/// A RAW-shaped container: a little-endian TIFF whose IFD0 carries
/// `orientation` and whose SubIFD points at a JPEG of `declared` bytes at
/// [`PREVIEW_AT`] — the shape every TIFF-family camera uses, and the one
/// microthumb's own tests prove it walks.
pub(crate) fn raw_with(jpeg: &[u8], orientation: u32, declared: u64) -> Vec<u8> {
	fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
		let mut out = tag.to_le_bytes().to_vec();
		out.extend_from_slice(&typ.to_le_bytes());
		out.extend_from_slice(&count.to_le_bytes());
		out.extend_from_slice(&value.to_le_bytes());
		out
	}
	let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
	file.extend_from_slice(&2u16.to_le_bytes());
	file.extend_from_slice(&entry(0x0112, 3, 1, orientation));
	file.extend_from_slice(&entry(0x014A, 4, 1, 60));
	file.extend_from_slice(&0u32.to_le_bytes());
	file.resize(60, 0);
	file.extend_from_slice(&2u16.to_le_bytes());
	file.extend_from_slice(&entry(0x0201, 4, 1, u32::from(PREVIEW_AT)));
	file.extend_from_slice(&entry(0x0202, 4, 1, u32::try_from(declared).unwrap()));
	file.extend_from_slice(&0u32.to_le_bytes());
	file.resize(usize::from(PREVIEW_AT), 0);
	file.extend_from_slice(jpeg);
	file
}
