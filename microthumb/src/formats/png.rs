//! PNG via the `png` crate's scanline API: rows fold straight into the sink,
//! so working memory is a handful of rows — the decoder's unfiltering window,
//! its scratch row and ours — no matter how tall the image is. WIDTH still
//! costs, and more than the output row suggests; see `peak_estimate`.
//! Adam7-interlaced files deliver pixels out
//! of row order across passes; those fall back to a whole-frame decode whose
//! cost is declared honestly and left to the budget check (~3 MP under the
//! default budget). APNG animation is ignored — the first frame is the image.

use std::io::BufReader;

use png::{ColorType, Transformations};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec,
};

pub struct Png;

impl FormatDecoder for Png {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
	}

	fn open(
		&self,
		src: Box<dyn ByteSource>,
		_spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let mut decoder = png::Decoder::new(BufReader::new(SeqReader::new(src)));
		// 8-bit depth and expanded palettes/greyscale: every output row is
		// then one of four fixed layouts to convert.
		decoder.set_transformations(Transformations::normalize_to_color8());
		let reader = decoder.read_info().map_err(decode_err)?;
		let info = reader.info();
		let dims = (info.width, info.height);
		let interlaced = info.interlaced;
		// PNGs carry EXIF in an `eXIf` chunk, orientation included — a phone
		// screenshot converted to PNG keeps it, and ignoring it thumbnails the
		// picture on its side.
		let orientation = info
			.exif_metadata
			.as_deref()
			.map_or(1, crate::exif::orientation);
		Ok(Box::new(PreparedPng {
			reader,
			dims,
			interlaced,
			orientation,
		}))
	}
}

struct PreparedPng {
	reader: png::Reader<BufReader<SeqReader>>,
	dims: (u32, u32),
	interlaced: bool,
	orientation: u8,
}

impl PreparedPng {
	fn row_rgba_len(&self) -> usize {
		self.dims.0 as usize * 4
	}
}

impl PreparedDecode for PreparedPng {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, _mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		// The eXIf chunk can theoretically carry a thumbnail; in practice PNGs
		// with one are vanishingly rare and rows already stream cheaply.
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		// The decoder's unfiltering buffer is sized from the RAW row — the
		// bytes as stored, 8 B/px at 16-bit RGBA — NOT from the row
		// `normalize_to_color8` hands back, which is what `output_buffer_size`
		// describes and which a 16-bit file halves. It is a `Vec` grown by
		// amortised doubling, so its capacity is a power-of-two multiple of
		// the 128 KiB it starts at, and it stops growing only once
		// `shift_back_limit` bytes of rows have been consumed (png-0.18
		// `decoder/unfiltering_buffer.rs`: `max(4 * raw_row, 128 KiB)`) — so
		// it lands somewhere past that window. Measured with a counting
		// allocator across widths 64..1_000_000, both depths and every colour
		// type, the worst landing was 3.28x the window; 4x is the charge, and
		// the margin is deliberate because that growth is a private detail of
		// the crate.
		//
		// `png::Limits` bounds none of it: `UnfilteringBuffer::new` is handed
		// the `Info` and nothing else (png-0.18 `decoder/mod.rs`), so no row
		// machinery ever consults the limit. `limits.bytes` is spent on chunk
		// DATA only — the raw-chunk accumulator's doubling and the copies
		// `parse_plte` / `parse_sbit` / `parse_trns` / the iCCP inflate / the
		// text chunks take of it (`decoder/stream.rs`).
		//
		// Charging honestly refuses some very wide PNGs that do decode today:
		// the widest the 12 MiB budget still admits drops to ~225_000 px at
		// 8-bit RGB and ~90_000 px at 16-bit RGBA (measured first refusals
		// 227_506 and 92_026), where before it was several times that.
		// `OverBudget` is the honest answer for them — a 43 KB 500_000x8 16-bit
		// RGBA file peaked at 37.6 MB under the estimate this replaces, three
		// times the budget that admitted it. No camera or export is anywhere near
		// those widths.
		// `raw_row_length` cannot overflow here even on a 32-bit target:
		// `read_info` refuses any image whose raw row does not fit a `usize`.
		let unfiltering_buffer = self
			.reader
			.info()
			.raw_row_length()
			.saturating_mul(4)
			.max(128 * 1024)
			.saturating_mul(4);
		// png's own post-transform scratch row, our RGBA conversion row, and
		// the inflate window plus odds and ends (measured at 54 KB).
		let rows = self
			.reader
			.output_line_size(self.dims.0)
			.unwrap_or(usize::MAX / 4)
			.saturating_add(self.row_rgba_len())
			.saturating_add(64 * 1024);
		let streaming = unfiltering_buffer.saturating_add(rows);
		if self.interlaced {
			// next_frame materialises the whole deinterlaced image on top of
			// all of that.
			streaming.saturating_add(self.reader.output_buffer_size().unwrap_or(usize::MAX / 4))
		} else {
			streaming
		}
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let (w, _) = self.dims;
		let color = self.reader.output_color_type().0;
		let mut rgba_row = vec![0u8; self.row_rgba_len()];
		if self.interlaced {
			let Some(size) = self.reader.output_buffer_size() else {
				return Err(ThumbError::Decode("png output size overflows".into()));
			};
			let mut buf = vec![0u8; size];
			let out = self.reader.next_frame(&mut buf).map_err(decode_err)?;
			let row_len = out.line_size;
			for (y, row) in (0..out.height).zip(buf.chunks_exact(row_len)) {
				convert_row(color, row, &mut rgba_row)?;
				sink.push(0, y, w, &rgba_row)?;
			}
			return Ok(());
		}
		let mut y = 0u32;
		while let Some(row) = self.reader.next_row().map_err(decode_err)? {
			convert_row(color, row.data(), &mut rgba_row)?;
			sink.push(0, y, w, &rgba_row)?;
			y += 1;
		}
		Ok(())
	}
}

fn decode_err(e: png::DecodingError) -> ThumbError {
	// The decoder wraps source io errors; unwrap them so transport trouble
	// stays a retryable error instead of reading as corrupt bytes (which the
	// caller caches as a permanent "no thumbnail" verdict).
	match e {
		png::DecodingError::IoError(io) => ThumbError::Io(io),
		other => ThumbError::Decode(format!("png: {other}")),
	}
}

/// After `normalize_to_color8` the row is 8-bit in one of these layouts.
fn convert_row(color: ColorType, row: &[u8], rgba: &mut [u8]) -> Result<(), ThumbError> {
	let px = rgba.len() / 4;
	let expected = match color {
		ColorType::Grayscale => px,
		ColorType::GrayscaleAlpha => px * 2,
		ColorType::Rgb => px * 3,
		ColorType::Rgba => px * 4,
		ColorType::Indexed => {
			return Err(ThumbError::Decode(
				"png palette survived normalize_to_color8".into(),
			));
		}
	};
	if row.len() < expected {
		return Err(ThumbError::Decode("png row shorter than declared".into()));
	}
	match color {
		ColorType::Grayscale => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				dst.copy_from_slice(&[*l, *l, *l, 255]);
			}
		}
		ColorType::GrayscaleAlpha => {
			for (dst, la) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				dst.copy_from_slice(&[la[0], la[0], la[0], la[1]]);
			}
		}
		ColorType::Rgb => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
			}
		}
		ColorType::Rgba => rgba.copy_from_slice(&row[..expected]),
		ColorType::Indexed => unreachable!("rejected above"),
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use png::{BitDepth, ColorType, Encoder};

	use crate::{MemSource, ThumbOutcome, ThumbSpec, generate};

	/// A wide, short RGBA strip at the given depth — the shape that makes the
	/// per-row cost the whole cost.
	fn ribbon(width: u32, depth: BitDepth) -> Vec<u8> {
		let bytes_per_px = if depth == BitDepth::Sixteen { 8 } else { 4 };
		let mut out = Vec::new();
		let mut encoder = Encoder::new(&mut out, width, 8);
		encoder.set_color(ColorType::Rgba);
		encoder.set_depth(depth);
		let mut writer = encoder.write_header().expect("png header");
		let data: Vec<u8> = (0..=250u8)
			.cycle()
			.take(width as usize * 8 * bytes_per_px)
			.collect();
		writer.write_image_data(&data).expect("png body");
		writer.finish().expect("png finish");
		out
	}

	/// Two strips with the SAME 8-bit output row: one stores 8 bits a channel,
	/// the other 16. The unfiltering buffer is sized from the STORED row, so
	/// the 16-bit file really does cost twice as much — an estimate built from
	/// `output_buffer_size` cannot tell them apart and admits both, then the
	/// 16-bit one blows the budget it was admitted under.
	#[test]
	fn the_estimate_follows_the_stored_row_not_the_output_row() {
		let spec = ThumbSpec::new(256, 256, 1024 * 1024);
		let eight = generate(Box::new(MemSource(ribbon(8000, BitDepth::Eight))), &spec)
			.expect("8-bit strip decodes");
		assert!(
			matches!(eight, ThumbOutcome::Thumbnail(_)),
			"the 8-bit strip fits this budget, got {eight:?}"
		);
		let sixteen = generate(Box::new(MemSource(ribbon(8000, BitDepth::Sixteen))), &spec)
			.expect("16-bit strip is a verdict, not an error");
		// Named rather than compared: a failing `assert_eq!` here would dump
		// the whole thumbnail into the output.
		let verdict = match &sixteen {
			ThumbOutcome::Thumbnail(thumb) => {
				format!("a {}x{} thumbnail", thumb.image.width, thumb.image.height)
			}
			other => format!("{other:?}"),
		};
		assert!(
			matches!(sixteen, ThumbOutcome::OverBudget),
			"the same picture at 16 bits a channel costs twice the rows and must be refused, got {verdict}"
		);
	}
}
