//! JPEG via `jpeg-decoder`, chosen over the `image` crate's zune-jpeg backend
//! for one capability: IDCT-scaled decode (1/8, 1/4, 1/2), which makes the
//! output buffer — not the source resolution — the allocation. Baseline
//! images of any size decode in a few MB. Progressive images structurally
//! need whole-image coefficient storage (~3 B/px); that cost goes into
//! `peak_estimate` and the budget check decides, which lands at roughly a
//! 4 MP progressive cap under the default budget.

use std::io::BufReader;

use jpeg_decoder::{CodingProcess, Decoder, PixelFormat};

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec, exif, source::BorrowedSeqReader,
};

use super::jpeg_dc::PreparedDcScan;

pub struct Jpeg;

/// Embedded EXIF thumbnails are ~160 px; anything claiming to be bigger than
/// this is not a thumbnail and not worth decoding on the preview path.
const MAX_PREVIEW_PIXELS: u64 = 1024 * 1024;

impl FormatDecoder for Jpeg {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.starts_with(&[0xFF, 0xD8, 0xFF])
	}

	fn open(
		&self,
		mut src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		// Header probe over a borrow, so the source is still ours to route:
		// a big progressive image goes to the DC-scan path instead.
		let (src_dims, progressive, exif) = {
			let mut probe = Decoder::new(BufReader::new(BorrowedSeqReader::new(&mut *src)));
			probe.read_info().map_err(decode_err)?;
			let info = probe
				.info()
				.ok_or_else(|| ThumbError::Decode("jpeg header parsed but no info".into()))?;
			(
				(u32::from(info.width), u32::from(info.height)),
				info.coding_process == CodingProcess::DctProgressive,
				probe.exif_data().map(<[u8]>::to_vec),
			)
		};
		let orientation = exif.as_deref().map_or(1, exif::orientation);

		if progressive {
			// The full decode needs whole-image coefficient planes; when
			// that (plus the canvas the orchestrator will charge for) cannot
			// fit the budget, the DC-scan parser produces a native 1/8-scale
			// preview instead of falling back to Ok(None). Same arithmetic
			// the orchestrator applies, so the routing never disagrees.
			let out_dims = scaled_output_upper_bound(src_dims, spec);
			let full_peak = full_decode_peak(src_dims, out_dims, true);
			let canvas = crate::canvas_dims(
				out_dims,
				(spec.target_width, spec.target_height),
				spec.mem_budget.saturating_sub(full_peak),
			);
			if full_peak.saturating_add(crate::sink::accumulator_bytes(canvas)) > spec.mem_budget {
				// A refusal here must DEGRADE, not abort: `open` runs before
				// the orchestrator has looked at the embedded preview, so
				// propagating the error would throw away a perfectly good
				// EXIF thumbnail we have already parsed. Sources the DC parser
				// will not touch (planes past its block cap, unsupported
				// precision, a header it cannot walk) fall back to
				// preview-only, which the orchestrator then serves or turns
				// into an honest Ok(None).
				return match PreparedDcScan::open(src, exif.clone(), orientation) {
					Ok(dc) => Ok(Box::new(dc)),
					Err(_) => Ok(Box::new(PreviewOnly {
						src_dims,
						exif,
						orientation,
					})),
				};
			}
		}

		let mut decoder = Decoder::new(BufReader::new(SeqReader::new(src)));
		decoder.read_info().map_err(decode_err)?;
		// Ask the IDCT for the target itself: the decoder picks the smallest
		// 1/8..1 factor covering it in at least one axis, which keeps the
		// decode buffer at ~1–2× the target instead of 4× (a 2× ask made a
		// 24 MP source pick 1/4 and blow the budget). The short side can land
		// a few percent under the target; the final fill-resize absorbs it.
		let req_w = u16::try_from(spec.target_width).unwrap_or(u16::MAX);
		let req_h = u16::try_from(spec.target_height).unwrap_or(u16::MAX);
		let (out_w, out_h) = decoder.scale(req_w, req_h).map_err(decode_err)?;

		Ok(Box::new(PreparedJpeg {
			decoder,
			src_dims,
			out_dims: (u32::from(out_w), u32::from(out_h)),
			progressive,
			exif,
			orientation,
		}))
	}
}

/// The largest output the IDCT scale can pick for this request: the smallest
/// 1/8..1 factor covering the target in one axis. Used only for the routing
/// estimate before a real decoder exists.
fn scaled_output_upper_bound(src: (u32, u32), spec: &ThumbSpec) -> (u32, u32) {
	for shift in [3u32, 2, 1, 0] {
		let (w, h) = (src.0 >> shift, src.1 >> shift);
		if w >= spec.target_width || h >= spec.target_height {
			return (w.max(1), h.max(1));
		}
	}
	src
}

/// Peak allocation of a full jpeg-decoder decode, shared between the routing
/// decision and `PreparedJpeg::peak_estimate` so they can never diverge.
fn full_decode_peak(src_dims: (u32, u32), out_dims: (u32, u32), progressive: bool) -> usize {
	let (out_w, out_h) = out_dims;
	let out_px = out_w as usize * out_h as usize;
	// Three buffers are alive at once at the peak, not one:
	//   * the interleaved RGB image jpeg-decoder returns  — 3 B/out px;
	//   * its per-component sample planes, which it fills FIRST and still
	//     holds while building that image (`worker/immediate.rs`, the
	//     `results` vecs; handed to `compute_image` in `decode_planes`) —
	//     1.5 B/out px at 4:2:0, 3 B at 4:4:4, so 3 is the honest bound;
	//   * our own per-row RGBA conversion buffer.
	// Counting only the first two is what let a 24 MP source pass a 12 MB
	// budget and then allocate ~18 MB. Block-size rounding adds under a
	// block per axis on top, far inside the row-buffer slack.
	let out = out_px * 3 + out_px * 3 + out_w as usize * 4;
	if progressive {
		// Whole-image coefficient planes: i16 per sample, so 3 B/src px at
		// 4:2:0 and 6 B at 4:4:4 — structural for progressive, no scale
		// escapes it, and the estimate must survive the unsubsampled case.
		out + src_dims.0 as usize * src_dims.1 as usize * 6
	} else {
		// One MCU row band (≤16 px tall) of the FULL-width image. ONE — which
		// holds only because jpeg-decoder's `rayon` feature makes
		// `Worker::append_row` run the IDCT inline. Without it the baseline path
		// posts every row to a spawned worker over an unbounded mpsc channel,
		// the entropy decoder outruns the IDCT, and the queued rows make this
		// term a fiction under load. See microthumb's Cargo.toml.
		out + src_dims.0 as usize * 16 * 4
	}
}

/// Decodes an EXIF IFD1 embedded thumbnail (shared by the full-decode and
/// DC-scan prepared states).
///
/// The EXIF block is already in memory by the time this runs, so nothing here
/// touches the source and an error can only mean corrupt bytes — hence
/// `Option`, not `Result`.
pub(super) fn exif_preview(exif: Option<&[u8]>, mem_budget: usize) -> Option<SmallImage> {
	let bytes = exif.and_then(exif::embedded_thumbnail)?;
	let mut decoder = Decoder::new(bytes);
	if decoder.read_info().is_err() {
		return None;
	}
	let sane = decoder.info().is_some_and(|info| {
		let px = u64::from(info.width) * u64::from(info.height);
		// Two ceilings, and the budget one has to be checked HERE: the
		// decoder's output frame (3 B/px, 4 for CMYK) and the RGBA copy
		// `rgba_image` builds are live together — 8 B/px, the rate the
		// whole-frame formats charge, and 7.17 MiB measured for a 1 Mpx
		// preview. The orchestrator can only filter one already paid for.
		px <= MAX_PREVIEW_PIXELS && px.saturating_mul(8) <= mem_budget as u64
	});
	if !sane {
		return None;
	}
	// A corrupt embedded thumbnail must not fail the real decode.
	let pixels = decoder.decode().ok()?;
	let info = decoder.info().expect("info available after decode");
	rgba_image(
		u32::from(info.width),
		u32::from(info.height),
		info.pixel_format,
		&pixels,
	)
}

struct PreparedJpeg {
	decoder: Decoder<BufReader<SeqReader>>,
	src_dims: (u32, u32),
	out_dims: (u32, u32),
	progressive: bool,
	exif: Option<Vec<u8>>,
	orientation: u8,
}

/// A source nothing can decode within the budget, but whose embedded thumbnail
/// may still serve. `peak_estimate` is deliberately unaffordable, so the
/// orchestrator skips straight to the preview it already has in hand.
struct PreviewOnly {
	src_dims: (u32, u32),
	exif: Option<Vec<u8>>,
	orientation: u8,
}

impl PreparedDecode for PreviewOnly {
	fn dims(&self) -> (u32, u32) {
		self.src_dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(exif_preview(self.exif.as_deref(), mem_budget))
	}

	fn peak_estimate(&self) -> usize {
		usize::MAX
	}

	fn decode_into(self: Box<Self>, _sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		Err(ThumbError::Decode(
			"this source has no affordable decode path".into(),
		))
	}
}

impl PreparedDecode for PreparedJpeg {
	fn dims(&self) -> (u32, u32) {
		self.src_dims
	}

	fn output_dims(&self) -> (u32, u32) {
		self.out_dims
	}

	fn orientation(&self) -> u8 {
		self.orientation
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		Ok(exif_preview(self.exif.as_deref(), mem_budget))
	}

	fn peak_estimate(&self) -> usize {
		full_decode_peak(self.src_dims, self.out_dims, self.progressive)
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		let pixels = self.decoder.decode().map_err(decode_err)?;
		let info = self
			.decoder
			.info()
			.ok_or_else(|| ThumbError::Decode("jpeg decoded but no info".into()))?;
		let (w, h) = self.out_dims;
		push_converted(sink, w, h, info.pixel_format, &pixels)
	}
}

fn decode_err(e: jpeg_decoder::Error) -> ThumbError {
	// Io stays Io: transport trouble must not read as corrupt bytes.
	match e {
		jpeg_decoder::Error::Io(io) => ThumbError::Io(io),
		other => ThumbError::Decode(format!("jpeg: {other}")),
	}
}

/// Converts one decoded frame to RGBA row by row and pushes it — the row
/// buffer is the only allocation.
fn push_converted(
	sink: &mut dyn PixelSink,
	w: u32,
	h: u32,
	format: PixelFormat,
	pixels: &[u8],
) -> Result<(), ThumbError> {
	let per_px = format.pixel_bytes();
	let row_len = w as usize * per_px;
	if row_len == 0 || pixels.len() < row_len * h as usize {
		return Err(ThumbError::Decode("jpeg output buffer too small".into()));
	}
	let mut rgba_row = vec![0u8; w as usize * 4];
	for y in 0..h {
		let row = &pixels[y as usize * row_len..][..row_len];
		convert_row(format, row, &mut rgba_row);
		sink.push(0, y, w, &rgba_row)?;
	}
	Ok(())
}

fn convert_row(format: PixelFormat, row: &[u8], rgba: &mut [u8]) {
	match format {
		PixelFormat::L8 => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row) {
				dst.copy_from_slice(&[*l, *l, *l, 255]);
			}
		}
		PixelFormat::L16 => {
			for (dst, l) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(2)) {
				dst.copy_from_slice(&[l[0], l[0], l[0], 255]);
			}
		}
		PixelFormat::RGB24 => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(3)) {
				dst.copy_from_slice(&[src[0], src[1], src[2], 255]);
			}
		}
		PixelFormat::CMYK32 => {
			for (dst, src) in rgba.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
				// jpeg-decoder hands CMYK through unconverted (Adobe inverted).
				let k = src[3];
				let px = [
					scale_by_k(src[0], k),
					scale_by_k(src[1], k),
					scale_by_k(src[2], k),
					255,
				];
				dst.copy_from_slice(&px);
			}
		}
	}
}

/// `channel * k / 255`: a byte times a byte over 255 is itself a byte.
fn scale_by_k(channel: u8, k: u8) -> u8 {
	u8::try_from(u16::from(channel) * u16::from(k) / 255)
		.expect("a byte times a byte over 255 is at most 255 (should be impossible)")
}

fn rgba_image(w: u32, h: u32, format: PixelFormat, pixels: &[u8]) -> Option<SmallImage> {
	let per_px = format.pixel_bytes();
	let row_len = w as usize * per_px;
	if w == 0 || h == 0 || pixels.len() < row_len * h as usize {
		return None;
	}
	let mut rgba = vec![0u8; w as usize * h as usize * 4];
	for y in 0..h as usize {
		convert_row(
			format,
			&pixels[y * row_len..][..row_len],
			&mut rgba[y * w as usize * 4..][..w as usize * 4],
		);
	}
	Some(SmallImage {
		width: w,
		height: h,
		rgba,
	})
}
