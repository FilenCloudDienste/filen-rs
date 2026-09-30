//! Fujifilm RAF, via the full-size JPEG its header points straight at.
//!
//! The one RAW container in the pinned set that needs no directory walk at
//! all: after the `FUJIFILMCCD-RAW` magic, a fixed header carries the preview
//! JPEG's offset and length as big-endian 32-bit words. Both are still treated
//! as claims — bounds-checked against the source and confirmed by reading the
//! JPEG's own SOI and SOF — because a fixed offset is no guarantee the bytes
//! there are what the header says.

use crate::{ByteSource, FormatDecoder, LocatedPreview, PreparedDecode, ThumbError, ThumbSpec};

use super::raw::{self, Index};

pub struct Raf;

/// Where the header keeps the preview's offset, with its length in the next
/// word. Confirmed across all ten pinned Fujifilm samples, spanning 2002-era
/// FinePix bodies through the X-S10.
const JPEG_POINTER_AT: u64 = 84;

/// What the header points at. Fuji writes a full EXIF block inside the
/// preview itself, so the orientation is the stream's own and the container
/// declares none.
fn index(src: &mut dyn ByteSource) -> Index {
	let mut pointer = [0u8; 8];
	let preview = match src.read_at(JPEG_POINTER_AT, &mut pointer) {
		Ok(8) => {
			let off = u64::from(u32::from_be_bytes([
				pointer[0], pointer[1], pointer[2], pointer[3],
			]));
			let len = u64::from(u32::from_be_bytes([
				pointer[4], pointer[5], pointer[6], pointer[7],
			]));
			raw::jpeg_window(src, off, len)
		}
		_ => None,
	};
	Index {
		orientation: 1,
		preview,
		jpeg: preview,
	}
}

impl FormatDecoder for Raf {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(b"FUJIFILMCCD-RAW")
	}

	fn locate_preview(
		&self,
		src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(raw::locate(index(src)))
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let Index {
			orientation,
			preview: Some(preview),
			..
		} = index(&mut *src)
		else {
			return Err(ThumbError::Decode(
				"raf: the header points at no usable preview; the sensor data is \
				 not a decodable image"
					.into(),
			));
		};
		raw::prepare(src, preview, orientation, spec)
	}
}

#[cfg(test)]
mod tests {
	use super::{JPEG_POINTER_AT, Raf};
	use crate::{DEFAULT_MEM_BUDGET, FormatDecoder, MemSource, ThumbSpec, locate_preview};

	/// A RAF whose header points at `off` for `len` bytes, with `payload`
	/// written at `off`.
	fn raf(off: u32, len: u32, payload: &[u8]) -> Vec<u8> {
		let mut file = b"FUJIFILMCCD-RAW 0201FF393101FinePix".to_vec();
		file.resize(usize::try_from(JPEG_POINTER_AT).unwrap(), 0);
		file.extend_from_slice(&off.to_be_bytes());
		file.extend_from_slice(&len.to_be_bytes());
		file.resize(off as usize, 0);
		file.extend_from_slice(payload);
		file
	}

	fn jpeg(w: u16, h: u16) -> Vec<u8> {
		let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0, 0x00, 0x11, 0x08];
		out.extend_from_slice(&h.to_be_bytes());
		out.extend_from_slice(&w.to_be_bytes());
		out.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
		out.resize(400, 0);
		out
	}

	fn open(file: Vec<u8>) -> bool {
		let spec = ThumbSpec::new(512, 512, DEFAULT_MEM_BUDGET);
		Raf.open(Box::new(MemSource(file)), &spec).is_ok()
	}

	/// Located as the header's own range; the orientation is the stream's to
	/// declare (Fuji writes a full EXIF into the preview), so the container
	/// says 1.
	#[test]
	fn the_pointed_at_preview_is_located() {
		let good = jpeg(1600, 1200);
		let len = u32::try_from(good.len()).unwrap();
		let located = locate_preview(&mut MemSource(raf(148, len, &good)))
			.unwrap()
			.expect("a preview");
		assert_eq!((located.offset, located.len), (148, good.len() as u64));
		assert_eq!((located.width, located.height), (1600, 1200));
		assert_eq!(located.orientation, 1);
		assert_eq!(
			locate_preview(&mut MemSource(raf(148, 400, &[0x41; 400]))).unwrap(),
			None
		);
	}

	#[test]
	fn the_header_pointer_is_a_claim_not_a_promise() {
		let good = jpeg(1600, 1200);
		assert!(open(raf(148, u32::try_from(good.len()).unwrap(), &good)));
		assert!(Raf.detect(b"FUJIFILMCCD-RAW 0201"));
		assert!(!Raf.detect(b"\xFF\xD8\xFFsomething else"));

		// Past the end, zero length, absurd length.
		assert!(!open(raf(148, 1 << 30, &good)));
		assert!(!open(raf(148, 0, &good)));
		assert!(!open(raf(u32::MAX - 16, 4096, &[])));
		// Points at bytes that are not a JPEG.
		assert!(!open(raf(148, 400, &[0x41; 400])));
		// Truncated before the pointer even exists.
		assert!(!open(b"FUJIFILMCCD-RAW 0201".to_vec()));
	}
}
