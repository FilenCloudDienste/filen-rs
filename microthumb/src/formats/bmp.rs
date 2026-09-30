//! Uncompressed BMP by direct row addressing: rows live at computed offsets,
//! so decoding is read-convert-push one row at a time with no library and no
//! frame buffer — O(width) for any size. Covers the overwhelming majority of
//! real BMPs (BI_RGB at 8-bit palette, 24- and 32-bit, either row order);
//! RLE, bitfields, OS/2 headers and other oddities fall back to the capped
//! whole-frame `image` path in `simple.rs` rather than being half-parsed.

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SmallImage, ThumbError, ThumbSpec,
};

pub struct Bmp;

fn err(msg: &str) -> ThumbError {
	ThumbError::Decode(format!("bmp: {msg}"))
}

impl FormatDecoder for Bmp {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(b"BM")
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		match parse_header(&mut *src)? {
			Some(header) => Ok(Box::new(PreparedBmp { src, header })),
			// Not the plain shape this module streams — the capped
			// whole-frame path knows all the exotica.
			None => super::simple::Simple.open(src, spec),
		}
	}
}

struct BmpHeader {
	width: u32,
	height: u32,
	top_down: bool,
	bits_per_px: u16,
	data_offset: u64,
	stride: u64,
	/// BGRA palette entries for 8-bit images.
	palette: Vec<u8>,
}

/// `Ok(None)` = valid BMP magic but not a variant this module streams.
fn parse_header(src: &mut dyn ByteSource) -> Result<Option<BmpHeader>, ThumbError> {
	let mut head = [0u8; 54];
	if src.read_at(0, &mut head)? < 54 {
		return Err(err("truncated header"));
	}
	let u16le = |o: usize| u16::from_le_bytes([head[o], head[o + 1]]);
	let u32le = |o: usize| u32::from_le_bytes([head[o], head[o + 1], head[o + 2], head[o + 3]]);
	let data_offset = u64::from(u32le(10));
	let dib_size = u32le(14);
	if dib_size < 40 {
		// BITMAPCOREHEADER and friends: rare, let the fallback deal.
		return Ok(None);
	}
	let width_raw = u32le(18) as i32;
	let height_raw = u32le(22) as i32;
	let planes = u16le(26);
	let bits_per_px = u16le(28);
	let compression = u32le(30);
	if width_raw <= 0 || height_raw == 0 || height_raw == i32::MIN || planes != 1 {
		return Err(err("malformed dimensions"));
	}
	if compression != 0 || !matches!(bits_per_px, 8 | 24 | 32) {
		return Ok(None);
	}
	let width = width_raw as u32;
	let (height, top_down) = if height_raw < 0 {
		((-height_raw) as u32, true)
	} else {
		(height_raw as u32, false)
	};
	let stride = (u64::from(width) * u64::from(bits_per_px)).div_ceil(32) * 4;

	let palette = if bits_per_px == 8 {
		// Palette sits right after the DIB header; 0 declared colors = 256.
		let declared = u32le(46);
		let colors = if declared == 0 || declared > 256 {
			256
		} else {
			declared as usize
		};
		let mut palette = vec![0u8; colors * 4];
		let at = 14 + u64::from(dib_size);
		if src.read_at(at, &mut palette)? < palette.len() {
			return Err(err("truncated palette"));
		}
		palette
	} else {
		Vec::new()
	};

	Ok(Some(BmpHeader {
		width,
		height,
		top_down,
		bits_per_px,
		data_offset,
		stride,
		palette,
	}))
}

struct PreparedBmp {
	src: Box<dyn ByteSource>,
	header: BmpHeader,
}

impl PreparedDecode for PreparedBmp {
	fn dims(&self) -> (u32, u32) {
		(self.header.width, self.header.height)
	}

	fn embedded_preview(&mut self, _mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(None)
	}

	fn peak_estimate(&self) -> usize {
		// One raw row + one RGBA row + the palette; a row wider than this
		// target's usize prices as unaffordable.
		usize::try_from(self.header.stride)
			.unwrap_or(usize::MAX)
			.saturating_add(self.header.width as usize * 4)
			.saturating_add(self.header.palette.len())
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let h = &self.header;
		let stride = usize::try_from(h.stride).map_err(|_| err("row too large"))?;
		let mut raw = vec![0u8; stride];
		let mut rgba = vec![0u8; h.width as usize * 4];
		let used = match h.bits_per_px {
			8 => h.width as usize,
			24 => h.width as usize * 3,
			_ => h.width as usize * 4,
		};
		for display_y in 0..h.height {
			let disk_row = if h.top_down {
				display_y
			} else {
				h.height - 1 - display_y
			};
			let at = h.data_offset + u64::from(disk_row) * h.stride;
			if self.src.read_at(at, &mut raw[..used])? < used {
				return Err(err("truncated pixel data"));
			}
			match h.bits_per_px {
				8 => {
					for (dst, &idx) in rgba.chunks_exact_mut(4).zip(&raw[..used]) {
						match h.palette.get(idx as usize * 4..idx as usize * 4 + 3) {
							// Palette entries are BGRA on disk.
							Some(c) => dst.copy_from_slice(&[c[2], c[1], c[0], 255]),
							None => dst.copy_from_slice(&[0, 0, 0, 255]),
						}
					}
				}
				24 => {
					for (dst, src) in rgba.chunks_exact_mut(4).zip(raw[..used].chunks_exact(3)) {
						dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
					}
				}
				_ => {
					for (dst, src) in rgba.chunks_exact_mut(4).zip(raw[..used].chunks_exact(4)) {
						// The 4th byte is only alpha in V4/V5 headers and is
						// garbage often enough that opaque is the safe read.
						dst.copy_from_slice(&[src[2], src[1], src[0], 255]);
					}
				}
			}
			sink.push(0, display_y, h.width, &rgba)?;
		}
		Ok(())
	}
}
