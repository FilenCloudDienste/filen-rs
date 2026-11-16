//! The formats the `image` crate can only decode whole-frame (WebP, GIF,
//! BMP, TIFF, QOI, ICO). No streaming exists for them here, so the honest answer
//! is a full-frame peak estimate and the budget check drawing the line —
//! ~2.5 MP under the default budget. Photos rarely travel in these formats
//! at sensor resolutions; web-sized images sail through.

use std::io::BufReader;

use image::{ImageDecoder, ImageReader};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SmallImage, ThumbError, ThumbSpec,
	source::BorrowedSeqReader,
};

pub struct Simple;

/// An ICO directory holds one 16-byte entry per size. Real icons carry a
/// dozen at most; the cap keeps a forged count (up to 65535) from making the
/// `image` crate read and allocate a directory nobody ships.
const MAX_ICO_ENTRIES: u16 = 256;

/// The format caps an entry at 256 px a side, but its PNG entries may declare
/// more, and the `image` crate only matches the directory against the header
/// up to 256. Anything past this is no icon; the full-frame budget check would
/// refuse most of it anyway, this refuses it by name before any decode work.
const MAX_ICO_SIDE: u32 = 1024;

/// `00 00 01 00` plus a non-zero entry count. Cursors (`00 00 02 00`) stay out:
/// their hotspot fields are read as planes and depth, and nobody keeps a
/// photo in one.
fn ico_entry_count(prefix: &[u8]) -> Option<u16> {
	let header = prefix.get(..6)?;
	if header[..4] != [0, 0, 1, 0] {
		return None;
	}
	let count = u16::from_le_bytes([header[4], header[5]]);
	(count != 0).then_some(count)
}

fn sniff_format(prefix: &[u8]) -> Option<image::ImageFormat> {
	if prefix.len() >= 12 && &prefix[..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
		return Some(image::ImageFormat::WebP);
	}
	if prefix.starts_with(b"GIF8") {
		return Some(image::ImageFormat::Gif);
	}
	if prefix.starts_with(b"BM") {
		return Some(image::ImageFormat::Bmp);
	}
	if prefix.starts_with(b"II\x2a\x00") || prefix.starts_with(b"MM\x00\x2a") {
		return Some(image::ImageFormat::Tiff);
	}
	if prefix.starts_with(b"qoif") {
		return Some(image::ImageFormat::Qoi);
	}
	if ico_entry_count(prefix).is_some() {
		return Some(image::ImageFormat::Ico);
	}
	None
}

impl FormatDecoder for Simple {
	fn detect(&self, prefix: &[u8]) -> bool {
		sniff_format(prefix).is_some()
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		_spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut prefix = [0u8; 16];
		let n = src.read_at(0, &mut prefix)?;
		let format = sniff_format(&prefix[..n])
			.ok_or_else(|| ThumbError::Decode("format vanished between sniff and open".into()))?;
		if ico_entry_count(&prefix[..n]).is_some_and(|count| count > MAX_ICO_ENTRIES) {
			return Err(ThumbError::Decode(format!(
				"ICO directory lists more than {MAX_ICO_ENTRIES} entries"
			)));
		}
		// Header-only parse over a borrowed reader; `into_decoder` consumes
		// its reader with no way back out, so the owned source stays here and
		// decode_into builds a second decoder over the same bytes.
		let (dims, orientation) = {
			let reader =
				ImageReader::with_format(BufReader::new(BorrowedSeqReader::new(&mut *src)), format);
			let mut decoder = reader.into_decoder().map_err(decode_err)?;
			let dims = decoder.dimensions();
			let orientation = decoder.orientation().map(|o| o.to_exif()).unwrap_or(1);
			(dims, orientation)
		};
		// The `image` crate decodes the entry it ranks best (colour depth,
		// then area); only that entry's header is checked here.
		if format == image::ImageFormat::Ico && dims.0.max(dims.1) > MAX_ICO_SIDE {
			return Err(ThumbError::Decode(format!(
				"ICO entry is larger than {MAX_ICO_SIDE} px a side"
			)));
		}
		Ok(Box::new(PreparedSimple {
			src,
			format,
			dims,
			orientation,
		}))
	}
}

struct PreparedSimple {
	src: Box<dyn ByteSource>,
	format: image::ImageFormat,
	dims: (u32, u32),
	orientation: u8,
}

impl PreparedDecode for PreparedSimple {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, _mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		// Full RGBA frame plus roughly one more copy inside the decoder /
		// DynamicImage conversion. Honest, so the budget can refuse it.
		//
		// Measured against the counting allocator (7 MP sources): 7.1 B/px
		// when the frame arrives as RGB and `into_rgba8` copies it, 4.1 B/px
		// when it already is RGBA. 8 covers both with a little room — worth
		// keeping, since this is the estimate an iOS extension is held to and
		// under-charging there is an OOM, not a missing thumbnail.
		let px = self.dims.0 as usize * self.dims.1 as usize;
		px.saturating_mul(8)
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let reader = ImageReader::with_format(
			BufReader::new(BorrowedSeqReader::new(&mut *self.src)),
			self.format,
		);
		let decoder = reader.into_decoder().map_err(decode_err)?;
		let image = image::DynamicImage::from_decoder(decoder).map_err(decode_err)?;
		let rgba = image.into_rgba8();
		let (w, h) = rgba.dimensions();
		if (w, h) != self.dims {
			return Err(ThumbError::Decode(
				"decoded dimensions differ from the header".into(),
			));
		}
		let data = rgba.into_raw();
		let row_len = w as usize * 4;
		for (y, row) in (0..h).zip(data.chunks_exact(row_len)) {
			sink.push(0, y, w, row)?;
		}
		Ok(())
	}
}

fn decode_err(e: image::ImageError) -> ThumbError {
	// Io stays Io: transport trouble must not read as corrupt bytes.
	match e {
		image::ImageError::IoError(io) => ThumbError::Io(io),
		other => ThumbError::Decode(format!("{other}")),
	}
}
