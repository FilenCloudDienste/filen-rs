//! HEIC/HEIF via the vendored libheif, and AVIF too where its AV1 backend
//! exists (native — see `AV1_BRANDS`). Two memory-bounded paths, in order:
//! the embedded `thmb` item (every iPhone HEIC carries one — the whole probe
//! costs a couple of container reads), then tile-wise decode of the grid
//! (Apple encodes 512×512 tiles), each tile pushed into the sink and freed
//! before the next. Un-tiled non-Apple HEIFs only offer a whole-frame decode,
//! priced like one big tile and left to the budget check.

use heif_decoder::{ChromaFormat, HeifSession, HeifTiling};
use image::RgbaImage;

use crate::{
	ByteSource, FormatDecoder, PixelSink, PreparedDecode, SeqReader, SmallImage, ThumbError,
	ThumbSpec,
};

pub struct Heif;

/// Embedded HEIF thumbnails are ~320 px; anything bigger is not a thumbnail.
const MAX_PREVIEW_PIXELS: u64 = 1024 * 1024;

/// A HEIF decode is charged three things: this setup, a rate per pixel of
/// what is decoded in one go, and the compressed input libheif holds — see
/// [`input_bytes`].
///
/// The setup is container state and the codec's own, its thread pool pinned
/// to one worker (see `heif-decoder`'s `NATIVE_CODEC_THREADS`).
const DECODE_SETUP_BYTES: usize = 1024 * 1024;
/// Per pixel of what is decoded in one go — a tile, or the whole frame: the
/// codec's picture, libheif's conversion to RGBA and the copy handed over —
/// for any 8-bit image, and a deeper one whose chroma is 4:2:0 or absent.
/// Measured on the peak of a whole `generate` call (macOS `malloc_logger`,
/// the retained input taken out): 8.3-8.7 B/px for 8-bit 4:2:0 HEVC and AV1,
/// 9.7 at 4:2:2, 10.7 at 4:4:4, 11.8 at 4:4:4 with alpha, and 8.8-9.2 for
/// 10- and 12-bit 4:2:0: past 8 bits only the samples the codec and the
/// conversion hold double in width, and at 4:2:0 those are half of luma.
const DECODE_BYTES_PER_PIXEL: usize = 12;
/// Past 8 bits with chroma at 4:2:2 or 4:4:4, or when the format is unknown:
/// 13.6 B/px at 10-bit 4:2:2 (Fujifilm, Sony and Canon HIF tiles alike),
/// about 16 on a rotated tile, which is turned in one more buffer, and
/// 15.6-19.8 from 4:4:4 up to 4:4:4 with alpha.
const DECODE_BYTES_PER_PIXEL_DEEP_CHROMA: usize = 20;

/// HEVC-backed brands, decoded by the vendored libde265 on every target.
///
/// `mif1` is the generic ISO-BMFF image brand and turns up on both HEVC and
/// AV1 payloads, so it stays unconditional and the decoder answers whichever
/// it can — which is both of them, on every target this builds for.
const BRANDS: &[&[u8; 4]] = &[
	b"heic", b"heix", b"heim", b"heis", b"hevc", b"hevx", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// AVIF, decoded by the vendored dav1d on every target — including wasm,
/// which is the reason the AV1 backend is dav1d and not libaom
/// (`heif-decoder`'s `build_dav1d` documents the `setjmp` wall libaom hits
/// there).
///
/// Pinned by `heif-decoder`'s `hevc_and_av1_decode`: if AV1 ever disappears
/// from the build, these brands come out in the same change.
///
/// `avis` is animated AVIF; libheif hands back its primary still image, and a
/// file that has none simply fails the container parse and answers "no
/// thumbnail". UNTESTED — no animated fixture in the repo.
const AV1_BRANDS: &[&[u8; 4]] = &[b"avif", b"avis"];

impl FormatDecoder for Heif {
	fn detect(&self, prefix: &[u8]) -> bool {
		prefix.len() >= 12
			&& &prefix[4..8] == b"ftyp"
			&& BRANDS
				.iter()
				.chain(AV1_BRANDS)
				.any(|brand| &prefix[8..12] == *brand)
	}

	fn open(
		&self,
		src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError> {
		let len = src.len();
		let session = HeifSession::new(SeqReader::new(src), len).map_err(decode_err)?;
		let bytes_per_pixel = decode_bytes_per_pixel(
			session.primary_bit_depth().map_err(decode_err)?,
			session.primary_chroma().map_err(decode_err)?,
		);
		let dims = session.primary_dims().map_err(decode_err)?;
		let tiling = session.tiling().map_err(decode_err)?;
		let input_bytes = input_bytes(
			usize::try_from(len).unwrap_or(usize::MAX),
			tiling.map_or(1, |tiling| {
				(tiling.num_columns as usize).saturating_mul(tiling.num_rows as usize)
			}),
		);
		Ok(Box::new(PreparedHeif {
			session,
			dims,
			tiling,
			bytes_per_pixel,
			input_bytes,
			mem_budget: spec.mem_budget,
		}))
	}
}

struct PreparedHeif {
	session: HeifSession<SeqReader>,
	dims: (u32, u32),
	tiling: Option<HeifTiling>,
	/// What each pixel decoded in one go costs, by the image's bit depth and
	/// chroma format.
	bytes_per_pixel: usize,
	/// What libheif holds of the compressed input: see [`input_bytes`].
	input_bytes: usize,
	mem_budget: usize,
}

impl PreparedDecode for PreparedHeif {
	fn dims(&self) -> (u32, u32) {
		self.dims
	}

	fn output_dims(&self) -> (u32, u32) {
		// The tiling reports the transformed (display) size, which is also
		// the space decode_image_tile produces; primary_dims matches it for
		// the un-tiled path.
		match &self.tiling {
			Some(tiling) => (tiling.image_width, tiling.image_height),
			None => self.dims,
		}
	}

	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError> {
		// Priced like any other decode, and the cap has to bind before libheif
		// decodes, not after. The thumbnail is charged at the deeper rate
		// whatever its own depth: real `thmb` items are ~320 px, far below
		// where that binds, and its compressed item is a few kilobytes.
		let max_pixels = MAX_PREVIEW_PIXELS.min(
			(mem_budget.saturating_sub(DECODE_SETUP_BYTES) / DECODE_BYTES_PER_PIXEL_DEEP_CHROMA)
				as u64,
		);
		// libheif's limits bind every decode in the session, and the ones the
		// primary image gets would refuse its thumbnail on a large file: each
		// decode is handed its own.
		self.session
			.set_decode_limits(max_pixels, mem_budget as u64);
		let rgba = self
			.session
			.embedded_thumbnail_rgba(max_pixels)
			.map_err(decode_err)?;
		// A corrupt thumbnail item must still not fail the tile path — but
		// that call belongs to the orchestrator, which knows whether there is
		// a tile path left to fall back on. Swallowing it here also swallowed
		// it on the preview-only spec, where the answer then reached the
		// caller as a settled "no thumbnail".
		Ok(rgba.map(small_image))
	}

	fn peak_estimate(&self) -> usize {
		// Saturating like simple.rs: these factors are container-declared —
		// a wrapped multiply must saturate into a guaranteed refusal, never
		// into a small estimate that passes the budget gate.
		let (width, height) = match &self.tiling {
			// One tile decoded at a time, independent of image size.
			Some(tiling) => (tiling.tile_width, tiling.tile_height),
			None => self.dims,
		};
		(width as usize)
			.saturating_mul(height as usize)
			.saturating_mul(self.bytes_per_pixel)
			.saturating_add(DECODE_SETUP_BYTES)
			.saturating_add(self.input_bytes)
	}

	fn decode_into(mut self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError> {
		// The container's declared tile dims (our peak_estimate) and the HEVC
		// bitstreams' actual picture sizes are unrelated — and libheif skips
		// its own whole-image size check on the tile path. These limits are
		// the pre-allocation guard for a lying container: no decoded picture
		// past what the budget could ever admit at this rate, no total past
		// the budget itself.
		let max_pixels = self
			.mem_budget
			.saturating_sub(DECODE_SETUP_BYTES)
			.saturating_sub(self.input_bytes)
			/ self.bytes_per_pixel;
		self.session
			.set_decode_limits(max_pixels as u64, self.mem_budget as u64);
		let Some(tiling) = self.tiling else {
			let rgba = self.session.decode_primary_rgba().map_err(decode_err)?;
			return push_clipped(sink, &rgba, Area::whole(self.dims));
		};
		for row in 0..tiling.num_rows {
			for col in 0..tiling.num_columns {
				let area = Area {
					x: Span {
						start: u64::from(col) * u64::from(tiling.tile_width),
						image_start: tiling.left_offset,
						image_len: tiling.image_width,
					},
					y: Span {
						start: u64::from(row) * u64::from(tiling.tile_height),
						image_start: tiling.top_offset,
						image_len: tiling.image_height,
					},
				};
				// A grid may be wider than the image it crops to: a tile past its
				// edge is never decoded.
				if area.x.visible(tiling.tile_width).is_none()
					|| area.y.visible(tiling.tile_height).is_none()
				{
					continue;
				}
				let tile = self
					.session
					.decode_tile_rgba(col, row)
					.map_err(decode_err)?;
				push_clipped(sink, &tile, area)?;
			}
		}
		Ok(())
	}
}

/// Where a decoded block lies, per axis, in the grid's coordinates — which
/// start `image_start` before the image does when a rotation or mirror put
/// the grid's overhang on the leading edge.
#[derive(Clone, Copy)]
struct Area {
	x: Span,
	y: Span,
}

#[derive(Clone, Copy)]
struct Span {
	/// The block's first pixel.
	start: u64,
	/// The image's first pixel.
	image_start: u32,
	image_len: u32,
}

impl Area {
	/// A block that is the image itself.
	fn whole((width, height): (u32, u32)) -> Self {
		let span = |image_len| Span {
			start: 0,
			image_start: 0,
			image_len,
		};
		Area {
			x: span(width),
			y: span(height),
		}
	}
}

impl Span {
	/// The part of a `len`-pixel block that lands on the image, as `(first
	/// block pixel, first image pixel, count)`, or `None` when none does.
	fn visible(&self, len: u32) -> Option<(usize, u32, u32)> {
		let image_start = u64::from(self.image_start);
		let from = self.start.max(image_start);
		let to = (self.start + u64::from(len)).min(image_start + u64::from(self.image_len));
		if from >= to {
			return None;
		}
		// All three are bounded by `len` or `image_len`, both u32.
		Some((
			(from - self.start) as usize,
			(from - image_start) as u32,
			(to - from) as u32,
		))
	}
}

/// Pushes the part of an RGBA block that lies inside the image. A tile can
/// overhang any edge: the right and bottom as coded, the left and top once
/// the image is rotated or mirrored.
fn push_clipped(sink: &mut dyn PixelSink, block: &RgbaImage, area: Area) -> Result<(), ThumbError> {
	let (Some((src_x, dst_x, width)), Some((src_y, dst_y, height))) = (
		area.x.visible(block.width()),
		area.y.visible(block.height()),
	) else {
		return Ok(());
	};
	let stride = block.width() as usize * 4;
	let data = block.as_raw();
	let row_bytes = width as usize * 4;
	for r in 0..height {
		let start = (src_y + r as usize) * stride + src_x * 4;
		sink.push(dst_x, dst_y + r, width, &data[start..start + row_bytes])?;
	}
	Ok(())
}

/// The per-pixel rate for a decode of this depth and chroma format; an image
/// that declares neither is charged the deeper rate.
fn decode_bytes_per_pixel(bit_depth: Option<u8>, chroma: Option<ChromaFormat>) -> usize {
	match (bit_depth, chroma) {
		(Some(depth), _) if depth <= 8 => DECODE_BYTES_PER_PIXEL,
		(Some(_), Some(ChromaFormat::Yuv420 | ChromaFormat::Monochrome)) => DECODE_BYTES_PER_PIXEL,
		_ => DECODE_BYTES_PER_PIXEL_DEEP_CHROMA,
	}
}

/// What libheif holds of a file's compressed input while it decodes one unit
/// of `units` — the tiles of a grid, or 1 for a whole frame. It reads each
/// tile's bitstream into a buffer of its own and keeps it until the session
/// ends, so a grid's peak grows by its whole bitstream as the tiles go by;
/// and the unit being decoded is held twice more on its way to the codec. A
/// whole frame is one unit the size of the file: measured at 3x the item on
/// bitstream-heavy files, where it dwarfs the pixels.
///
/// The file's length stands in for its bitstream, so anything else it carries
/// (a motion photo's video) is charged too, and a grid's bytes are assumed
/// spread across its tiles. One that piles them into a single tile is caught
/// by libheif's own block and total limits instead, set from the same budget.
fn input_bytes(file_len: usize, units: usize) -> usize {
	file_len.saturating_add(file_len.div_ceil(units.max(1)).saturating_mul(2))
}

fn small_image(rgba: RgbaImage) -> SmallImage {
	let (width, height) = rgba.dimensions();
	SmallImage {
		width,
		height,
		rgba: rgba.into_raw(),
	}
}

fn decode_err(e: heif_decoder::HeifError) -> ThumbError {
	// Always `Decode`, never `Io`: libheif reports a failed reader callback as
	// an error code of its own, and `HeifError` does not carry the io error
	// that callback saw. So a transport blip inside libheif is not
	// distinguishable here from corrupt bytes, and settles as a verdict about
	// the file rather than staying retryable. Fixing that means teaching
	// `heif-decoder` to carry the reader's error out.
	//
	// A libheif security limit tripping mid-decode is `Decode` too, and is
	// not an over-budget answer. libheif counts two things against its
	// ceilings, the decoded planes and the compressed input it holds, and
	// `peak_estimate` charges more than it for each, so nothing the charge
	// admits reaches them honestly. A trip means the bitstream decodes larger
	// than its container declared — libheif caps a tile at its grid's
	// declared size — and no budget would ever admit that file.
	ThumbError::Decode(format!("{e}"))
}
