//! TIFF via the `tiff` crate's chunk API: strips (or tiles) decode one at a
//! time into the sink, so the peak is one chunk, not the image. A TIFF is
//! itself the EXIF container format, so the existing IFD walker probes the
//! file's own directory chain for an embedded JPEG thumbnail and the
//! orientation tag before any real decode. A TIFF written as one huge strip
//! offers no streaming boundary — its honest one-chunk peak is the whole
//! image, and the budget check refuses it like any other full-frame decode.
//!
//! Sample support: 8- and 16-bit Gray / GrayA / RGB / RGBA (16-bit is
//! downscaled). Everything else — palettes, CMYK, floats, planar exotica —
//! refuses with a Decode error naming the type rather than mangling pixels.

use tiff::ColorType;
use tiff::tags::Tag;

use crate::{
	ByteSource, FormatDecoder, LocatedPreview, PixelSink, PreparedDecode, SeqReader, SmallImage,
	ThumbError, ThumbSpec, exif,
};

use super::jpeg::exif_preview;
use super::raw;

pub struct Tiff;

fn err(msg: &str) -> ThumbError {
	ThumbError::Decode(format!("tiff: {msg}"))
}

fn decode_err(e: tiff::TiffError) -> ThumbError {
	// Io stays Io: transport trouble must not read as corrupt bytes.
	match e {
		tiff::TiffError::IoError(io) => ThumbError::Io(io),
		other => err(&other.to_string()),
	}
}

/// How much of the file the EXIF probe reads: enough for the IFD chain and a
/// front-loaded thumbnail, bounded so a hostile offset cannot balloon it.
const EXIF_PROBE_BYTES: usize = 256 * 1024;

/// Second-read window for an IFD0 that sits past the probe — covers a ~5000
/// entry directory, bounded for the same hostile-offset reason.
const IFD_WINDOW_BYTES: usize = 64 * 1024;

/// What one declared strip/tile costs before any pixel is decoded: the
/// `tiff` crate materialises StripOffsets and StripByteCounts as `u64` vecs
/// (8 B each) and builds them through an intermediate `Vec<Value>`, so ~48 B
/// per chunk is the honest peak of merely LOOKING at the table.
const BYTES_PER_DECLARED_CHUNK: u64 = 48;

/// Most strips or tiles a file may declare before it is refused unread —
/// whatever keeps those tables inside an eighth of the caller's budget.
///
/// Generous where it counts: at the 12 MB default that is ~32k chunks, and a
/// 100 MP scan at the one-row-per-strip worst case declares ~10k, tiled maps
/// a few thousand. What it stops is the forged millions, whose offset tables
/// alone outweigh everything this process is allowed.
fn max_declared_chunks(mem_budget: usize) -> u64 {
	(mem_budget as u64 / 8 / BYTES_PER_DECLARED_CHUNK).max(1)
}

/// Whether these bytes are a plain TIFF the `tiff` crate can be handed, as
/// opposed to a vendor container that merely borrows the byte order mark.
fn plain_tiff(src: &mut dyn ByteSource) -> bool {
	let mut head = [0u8; 4];
	matches!(src.read_at(0, &mut head), Ok(4)) && (head == *b"II\x2a\x00" || head == *b"MM\x00\x2a")
}

impl FormatDecoder for Tiff {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(b"II\x2a\x00")
			|| prefix.starts_with(b"MM\x00\x2a")
			|| raw::detect_vendor_tiff(prefix)
	}

	/// The same walk `open` starts with; a plain TIFF's IFD1 thumbnail is
	/// under the preview floor, so only the RAW containers answer.
	fn locate_preview(
		&self,
		src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(raw::locate(raw::scan(src)))
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		// Camera RAW files wear TIFF's magic (and Olympus' and Panasonic's
		// wear a near-miss of it), but IFD0 describes a sensor mosaic rather
		// than a picture. Look for the real JPEG the camera embedded before
		// letting the TIFF decoder anywhere near the file: it would otherwise
		// either refuse the colour type or, worse, decode a 160x120 sub-image
		// and pass it off as the photograph.
		let index = raw::scan(&mut *src);
		if let Some(preview) = index.preview {
			return raw::prepare(src, preview, index.orientation, spec);
		}
		if !plain_tiff(&mut *src) {
			return Err(err(
				"vendor RAW container with no embedded preview; the sensor \
				 mosaic is not a decodable image",
			));
		}

		// The file IS a TIFF/EXIF payload: walk its own IFDs for orientation
		// and an embedded JPEG thumbnail before spinning up the decoder.
		let probe_len = usize::try_from(src.len())
			.unwrap_or(usize::MAX)
			.min(EXIF_PROBE_BYTES);
		let mut probe = vec![0u8; probe_len];
		let n = src.read_at(0, &mut probe)?;
		probe.truncate(n);
		// IFD0 usually sits inside the probe; an IFD-last file (the layout the
		// `tiff` crate's own encoder and most scanner software write) puts it
		// beyond, so fetch one bounded window at its offset. Both the
		// orientation tag and the chunk-count tags are read from whichever
		// buffer actually holds the directory.
		let ifd_window = match exif::ifd0_offset(&probe) {
			Some(off) if off.saturating_add(IFD_WINDOW_BYTES as u64) > probe.len() as u64 => {
				let mut window = vec![0u8; IFD_WINDOW_BYTES];
				let n = src.read_at(off, &mut window)?;
				window.truncate(n);
				Some(window)
			}
			_ => None,
		};
		// Rebased to 0 in a fetched window, at its real offset in the probe.
		let (directory, ifd_at) = match &ifd_window {
			Some(window) => (window.as_slice(), 0usize),
			None => (
				probe.as_slice(),
				exif::ifd0_offset(&probe)
					.and_then(|off| usize::try_from(off).ok())
					.unwrap_or(0),
			),
		};
		let orientation = match &ifd_window {
			// Directories longer than the window lose the tag — a wrong
			// default only costs a rotated thumbnail.
			Some(window) => exif::orientation_in_window(&probe, window),
			None => exif::orientation(&probe),
		};

		// Bound the chunk table BEFORE the decoder exists. `Decoder::new`
		// reads every StripOffsets/StripByteCounts entry eagerly, under the
		// crate's own 256 MiB default rather than our budget, so a narrow
		// RowsPerStrip=1 file with a huge ImageLength costs tens of MB inside
		// a constructor no budget check has reached yet — more than this
		// process is allowed in total. `with_limits` cannot help: it applies
		// to decoding, after that read. The count is the file's own claim,
		// which is exactly the point — nothing is sized from it until it has
		// been found plausible.
		let declared_chunks = exif::tiff_chunk_count(&probe, directory, ifd_at);
		if declared_chunks.is_none_or(|chunks| chunks > max_declared_chunks(spec.mem_budget)) {
			return Err(err(
				"strip/tile count is unreadable or implausibly large; refusing before the \
				 decoder reads its offset tables",
			));
		}

		let mut decoder = tiff::decoder::Decoder::new(SeqReader::new(src)).map_err(decode_err)?;
		let (width, height) = decoder.dimensions().map_err(decode_err)?;
		if width == 0 || height == 0 {
			return Err(err("zero dimensions"));
		}
		let color = decoder.colortype().map_err(decode_err)?;
		let per_px = match color {
			ColorType::Gray(8) | ColorType::Gray(16) => 1usize,
			ColorType::GrayA(8) | ColorType::GrayA(16) => 2,
			ColorType::RGB(8) | ColorType::RGB(16) => 3,
			ColorType::RGBA(8) | ColorType::RGBA(16) => 4,
			other => return Err(err(&format!("unsupported color type {other:?}"))),
		};
		// Tiles and strips share the chunk API; only the count differs.
		let tiled = decoder.get_tag_unsigned::<u64>(Tag::TileWidth).is_ok();
		let chunk_count = if tiled {
			decoder.tile_count().map_err(decode_err)?
		} else {
			decoder.strip_count().map_err(decode_err)?
		};
		let chunk_dims = decoder.chunk_dimensions();
		if chunk_dims.0 == 0 || chunk_dims.1 == 0 || chunk_count == 0 {
			return Err(err("inconsistent chunk layout"));
		}
		let bytes_per_sample = match color {
			ColorType::Gray(16)
			| ColorType::GrayA(16)
			| ColorType::RGB(16)
			| ColorType::RGBA(16) => 2usize,
			_ => 1,
		};
		Ok(Box::new(PreparedTiff {
			decoder,
			probe,
			dims: (width, height),
			orientation,
			color,
			per_px,
			chunk_dims,
			chunk_count,
			chunk_peak: chunk_dims.0 as usize * chunk_dims.1 as usize * per_px * bytes_per_sample,
		}))
	}
}

struct PreparedTiff {
	decoder: tiff::decoder::Decoder<SeqReader>,
	probe: Vec<u8>,
	dims: (u32, u32),
	orientation: u8,
	color: ColorType,
	per_px: usize,
	chunk_dims: (u32, u32),
	chunk_count: u32,
	chunk_peak: usize,
}

impl PreparedDecode for PreparedTiff {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(exif_preview(Some(&self.probe), mem_budget))
	}

	fn peak_estimate(&self) -> usize {
		// One decoded chunk (the crate materialises it as a Vec) plus our
		// RGBA conversion row. A single-huge-strip TIFF reports the whole
		// image here — honestly — and the budget refuses it.
		self.chunk_peak + self.chunk_dims.0 as usize * 4 + 64 * 1024
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let (width, height) = self.dims;
		let (chunk_w, chunk_h) = self.chunk_dims;
		let chunks_per_row = width.div_ceil(chunk_w);
		let mut rgba_row = vec![0u8; chunk_w as usize * 4];
		for i in 0..self.chunk_count {
			let (data_w, data_h) = self.decoder.chunk_data_dimensions(i);
			if data_w == 0 || data_h == 0 {
				continue;
			}
			let x0 = (i % chunks_per_row) * chunk_w;
			let y0 = (i / chunks_per_row) * chunk_h;
			if x0 + data_w > width || y0 + data_h > height {
				return Err(err("chunk exceeds the image bounds"));
			}
			let result = self.decoder.read_chunk(i).map_err(decode_err)?;
			let row_samples = data_w as usize * self.per_px;
			match result {
				tiff::decoder::DecodingResult::U8(data) => {
					if data.len() < row_samples * data_h as usize {
						return Err(err("chunk shorter than its dimensions"));
					}
					for r in 0..data_h {
						let row = &data[r as usize * row_samples..][..row_samples];
						convert_row_u8(self.color, row, &mut rgba_row[..data_w as usize * 4])?;
						sink.push(x0, y0 + r, data_w, &rgba_row[..data_w as usize * 4])?;
					}
				}
				tiff::decoder::DecodingResult::U16(data) => {
					if data.len() < row_samples * data_h as usize {
						return Err(err("chunk shorter than its dimensions"));
					}
					for r in 0..data_h {
						let row = &data[r as usize * row_samples..][..row_samples];
						convert_row_u16(self.color, row, &mut rgba_row[..data_w as usize * 4])?;
						sink.push(x0, y0 + r, data_w, &rgba_row[..data_w as usize * 4])?;
					}
				}
				_ => return Err(err("unsupported sample format")),
			}
		}
		Ok(())
	}
}

fn convert_row_u8(color: ColorType, row: &[u8], rgba: &mut [u8]) -> Result<(), ThumbError> {
	match color {
		ColorType::Gray(_) => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				dst.copy_from_slice(&[*l, *l, *l, 255]);
			}
		}
		ColorType::GrayA(_) => {
			for (dst, la) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				dst.copy_from_slice(&[la[0], la[0], la[0], la[1]]);
			}
		}
		ColorType::RGB(_) => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
			}
		}
		ColorType::RGBA(_) => rgba.copy_from_slice(&row[..rgba.len()]),
		_ => return Err(err("unsupported color type")),
	}
	Ok(())
}

fn convert_row_u16(color: ColorType, row: &[u16], rgba: &mut [u8]) -> Result<(), ThumbError> {
	let down = |v: u16| (v >> 8) as u8;
	match color {
		ColorType::Gray(_) => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				let l = down(*l);
				dst.copy_from_slice(&[l, l, l, 255]);
			}
		}
		ColorType::GrayA(_) => {
			for (dst, la) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				let l = down(la[0]);
				dst.copy_from_slice(&[l, l, l, down(la[1])]);
			}
		}
		ColorType::RGB(_) => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[down(src[0]), down(src[1]), down(src[2]), 255]);
			}
		}
		ColorType::RGBA(_) => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
				dst.copy_from_slice(&[down(src[0]), down(src[1]), down(src[2]), down(src[3])]);
			}
		}
		_ => return Err(err("unsupported color type")),
	}
	Ok(())
}
