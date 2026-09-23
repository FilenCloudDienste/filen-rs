//! Memory-bounded thumbnail decoding.
//!
//! Built for hosts with hard memory ceilings (an iOS file-provider extension
//! gets ~20 MB before jetsam). Nothing in this crate ever materialises a full
//! decoded frame of an arbitrarily large image: every format either streams
//! (rows, tiles, IDCT-scaled output) into a fixed-size accumulator canvas, or
//! is refused up front by an honest per-decode peak estimate checked against
//! the caller's budget. Fully synchronous — async bridging and any notion of
//! where bytes come from live in the caller's [`ByteSource`].

mod exif;
mod formats;
pub(crate) mod sink;
mod source;

use std::sync::atomic::Ordering;

pub use sink::{BoxAccumulator, PixelSink, SmallImage};
pub(crate) use source::SubSource;
pub use source::{ByteSource, FileSource, MemSource, SeqReader};

use crate::{sink::accumulator_bytes, source::BulkHintRelay};

/// Default decode budget: what is realistically left of a 20 MB jetsam limit
/// after the host process' own baseline.
pub const DEFAULT_MEM_BUDGET: usize = 12 * 1024 * 1024;

/// Decode budget for a host that is a whole application process — a browser
/// tab, the phone app itself — rather than a memory-capped extension: there is
/// no 20 MB jetsam ceiling, so the number to bound is the process high-water
/// mark, not a per-decode kill threshold. (Sized against the tightest such
/// host, a wasm tab, whose linear memory is capped at 1 GiB by the linker and
/// is never returned to the OS.) 64 MiB admits the whole-frame formats (WebP, QOI,
/// BMP, GIF, TIFF — see `formats::simple`, which charges 8 bytes per source
/// pixel) up to ~8.4 MP, where the 12 MiB default refuses anything past
/// ~1.5 MP. Anything larger in those formats still answers
/// [`ThumbOutcome::OverBudget`]: 8 B/px
/// is what they really cost (measured: 7.1 B/px for a 7 MP lossless WebP or
/// QOI whose frame arrives as RGB and is copied to RGBA, 4.1 B/px when it
/// arrives as RGBA), and buying a bigger ceiling would mean spending it.
///
/// The streaming formats (JPEG, PNG, HEIF) do NOT stay flat across the two
/// budgets: their decoders do, but [`canvas_dims`] spends what the decoder
/// leaves, so the accumulator grows until the 2× oversample of the request or
/// [`MAX_CANVAS_LONG_SIDE`] binds. A 24 MP PNG at a 512 px request measured
/// 10.3 MiB at the 12 MiB budget and 24.4 MiB at this one — a better
/// thumbnail for the memory, and still a third of what its whole frame would
/// have cost. Three concurrent decodes — the practical ceiling on a thumbnail
/// grid — are bounded at ~192 MiB of that 1 GiB.
pub const APP_PROCESS_MEM_BUDGET: usize = 64 * 1024 * 1024;

/// The accumulator canvas never exceeds this on its long side — oversampling
/// past ~2× the largest sane thumbnail target buys nothing. (The budget cap
/// in [`canvas_dims`] usually binds first, at the per-pixel canvas cost
/// `accumulator_bytes` documents.)
const MAX_CANVAS_LONG_SIDE: u32 = 1600;

/// Anti-DoS ceiling on total decode WORK, which the per-unit memory budget
/// cannot bound on its own: the streaming decoders' peaks are per row / strip
/// / tile, so a forged header (a 1×2³⁰ PNG, a `RowsPerStrip=1` TIFF with a
/// huge height) passes the memory check and then runs an effectively unbounded
/// loop. Source area caps the number of units every streaming format can emit.
/// Deliberately GENEROUS — ~20× a 24 MP photo, far above any camera or export;
/// this refuses degenerate forgeries, never real pictures.
const MAX_SOURCE_AREA_PIXELS: u64 = 512_000_000;

/// Companion to [`MAX_SOURCE_AREA_PIXELS`] for degenerate shapes: a
/// 1×500-million-pixel ribbon passes an area cap alone while still driving
/// hundreds of millions of per-row iterations. No real image has a side
/// beyond a million pixels.
const MAX_SOURCE_SIDE_PIXELS: u32 = 1 << 20;

/// The largest square target `mem_budget` could hold a canvas for, leaving
/// half the budget for the decoder.
///
/// Callers ask for what their UI would like — iOS requests 2048×2048 for every
/// item regardless of the display size, because the system caches one
/// thumbnail per item and rescales it for every use. Treating that as a decode
/// target made the whole pipeline refuse: an in-format scaler (JPEG's IDCT
/// scale) has no room to shrink when the target is near the source size, so
/// the estimate stayed at full resolution and blew the budget. The requested
/// size is an upper bound, not a demand — clamp it to what is affordable and
/// return the best image that fits.
fn affordable_target(mem_budget: usize) -> u32 {
	let max_px = (mem_budget / 2 / 20).max(1);
	(max_px as f64).sqrt() as u32
}

#[derive(Debug, Clone, Copy)]
pub struct ThumbSpec {
	pub target_width: u32,
	pub target_height: u32,
	/// Hard ceiling on decoder peak allocation + accumulator canvas, bytes.
	pub mem_budget: usize,
	/// Whether a real decode may run, or only the cheap embedded-preview probe.
	///
	/// `mem_budget` bounds MEMORY; this bounds how many BYTES OF SOURCE the
	/// pipeline is willing to pull. They are not the same axis: an uncompressed
	/// 81 MP TIFF in 3-row strips decodes in ~60 KB of memory but has to be
	/// read almost end to end, which is fine from disk and not fine over the
	/// network for one 256 px thumbnail. Callers reading a huge remote file
	/// clear this: the header and any embedded thumbnail still cost a chunk or
	/// two, and a source with neither answers `Ok(None)` having read almost
	/// nothing.
	pub allow_full_decode: bool,
}

impl ThumbSpec {
	/// A spec that may decode — the ordinary case (local bytes, or a remote
	/// file small enough to stream).
	pub fn new(target_width: u32, target_height: u32, mem_budget: usize) -> Self {
		Self {
			target_width,
			target_height,
			mem_budget,
			allow_full_decode: true,
		}
	}

	/// A spec restricted to embedded previews; see [`allow_full_decode`](Self::allow_full_decode).
	pub fn preview_only(target_width: u32, target_height: u32, mem_budget: usize) -> Self {
		Self {
			allow_full_decode: false,
			..Self::new(target_width, target_height, mem_budget)
		}
	}
}

/// A complete JPEG living inside another container — the camera's own
/// rendering of the shot beside the sensor mosaic — found by
/// [`locate_preview`] and never decoded: the point is that these bytes can go
/// to a viewer untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocatedPreview {
	/// Absolute offset of the SOI in the source.
	pub offset: u64,
	/// As the container claims it, bounds-checked against the source but NOT
	/// trimmed to the JPEG's own EOI: a CR3 preview box's is the box's own
	/// extent, so a few bytes of padding may follow, and Panasonic pads too.
	/// Every decoder stops at the EOI.
	pub len: u64,
	/// From the preview's own SOF, not from any directory field claiming to
	/// describe it.
	pub width: u32,
	pub height: u32,
	/// The CONTAINER's EXIF orientation (1–8), 1 when it declares none.
	/// Whether that applies to the bytes is the caller's call: the stream may
	/// carry an EXIF of its own, in which case [`exif_insert_at`](Self::exif_insert_at)
	/// is `None` and the stream's tag is the one a viewer will read.
	pub orientation: u8,
	/// Byte offset within the preview where an EXIF APP1 may legally be
	/// inserted — 2, right after the SOI, or past a leading APP0/JFIF. `None`
	/// when the stream already carries an APP1 and must not be touched.
	///
	/// Computed by the same marker walk that found the SOF, so no second
	/// parser gets to disagree with the first about one stream.
	pub exif_insert_at: Option<u16>,
	/// What the stream's own EXIF declares, when it carries one: `Some(1)`
	/// for upright, `None` for no EXIF at all. Fuji and Panasonic write a
	/// full EXIF into the preview; Nikon, Sony and Canon do not. This — not
	/// [`orientation`](Self::orientation) — is what a viewer handed the bytes
	/// untouched will apply.
	pub stream_orientation: Option<u8>,
}

/// Below this on the long side an embedded image is a thumbnail, not a
/// preview: the stamps several containers carry (CR3 `THMB` 160x120, the
/// Olympus maker-note blob, every EXIF IFD1 thumb, the Nikon D1H strip) would
/// be upscaled mush full-screen. 512 clears 81 of the 100 pinned raw.pixls.us
/// samples; the rest keep the thumbnail path.
pub const MIN_PREVIEW_LONG_SIDE: u32 = 512;

/// Which path produced a thumbnail.
///
/// Reported rather than logged: this crate stays free of a logging dependency,
/// and the caller is the one with a tracing subscriber and an item id to name.
/// Worth surfacing because the two paths differ by orders of magnitude in cost
/// — an embedded preview is a couple of container reads, a decode can be a
/// whole-file fetch — so "are we hitting the fast path?" is the first question
/// asked whenever this pipeline is touched, and it is invisible from the
/// outside otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbSource {
	/// The image's own embedded thumbnail (EXIF IFD1, HEIF `thmb` item).
	EmbeddedPreview,
	/// Decoded from the full image, downscaled into the canvas.
	Decoded,
}

impl std::fmt::Display for ThumbSource {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ThumbSource::EmbeddedPreview => f.write_str("embedded preview"),
			ThumbSource::Decoded => f.write_str("full decode"),
		}
	}
}

/// A thumbnail and the path that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
	pub image: SmallImage,
	pub source: ThumbSource,
}

/// How a [`generate`] call ended when it did not fail outright.
///
/// The two no-thumbnail verdicts are deliberately distinct. They mean opposite
/// things to a caller: `Unsupported` is final for these bytes (nothing here
/// will ever decode them), while `OverBudget` is a verdict about the SPEC —
/// the same source may well thumbnail under a roomier budget, or once its
/// bytes are local and a full decode is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbOutcome {
	Thumbnail(Thumbnail),
	/// The magic-byte sniff matched no format this build decodes.
	Unsupported,
	/// A recognised image, but nothing fit the spec: over `mem_budget`, over
	/// the work ceiling ([`MAX_SOURCE_AREA_PIXELS`] /
	/// [`MAX_SOURCE_SIDE_PIXELS`]), or a full decode was needed and
	/// [`ThumbSpec::allow_full_decode`] is off.
	OverBudget,
}

impl ThumbOutcome {
	/// The thumbnail, if there is one — for callers that do not care WHY there
	/// isn't.
	pub fn thumbnail(self) -> Option<Thumbnail> {
		match self {
			ThumbOutcome::Thumbnail(thumb) => Some(thumb),
			ThumbOutcome::Unsupported | ThumbOutcome::OverBudget => None,
		}
	}
}

#[derive(Debug)]
pub enum ThumbError {
	Io(std::io::Error),
	/// The bitstream is corrupt or the decoder refused it.
	Decode(String),
	/// A decoder pushed pixels outside the geometry it declared.
	Geometry,
}

impl std::fmt::Display for ThumbError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			ThumbError::Io(e) => write!(f, "thumbnail source io error: {e}"),
			ThumbError::Decode(msg) => write!(f, "thumbnail decode error: {msg}"),
			ThumbError::Geometry => write!(f, "decoder pushed pixels outside its declared bounds"),
		}
	}
}

impl std::error::Error for ThumbError {}

impl From<std::io::Error> for ThumbError {
	fn from(e: std::io::Error) -> Self {
		ThumbError::Io(e)
	}
}

/// One image format's entry point.
pub trait FormatDecoder: Send + Sync {
	/// Magic-byte sniff on the first bytes of the source. Mime types are a
	/// hint upstream; bytes are the truth.
	fn detect(&self, prefix: &[u8]) -> bool;

	/// The large JPEG this container embeds, located but not decoded. The
	/// default is the honest answer for every format that IS the image.
	fn locate_preview(
		&self,
		_src: &mut dyn ByteSource,
	) -> Result<Option<LocatedPreview>, ThumbError> {
		Ok(None)
	}

	/// Parses the header only — reads a few KB. The spec is available so
	/// formats with in-decoder scaling (JPEG's IDCT scale) can commit to an
	/// output resolution here and report it via
	/// [`PreparedDecode::output_dims`].
	fn open(
		&self,
		src: Box<dyn ByteSource>,
		spec: &ThumbSpec,
	) -> Result<Box<dyn PreparedDecode>, ThumbError>;
}

/// A source with its header parsed, ready to either hand over an embedded
/// preview or decode for real.
pub trait PreparedDecode {
	/// Source dimensions, as declared by the header.
	fn dims(&self) -> (u32, u32);

	/// The coordinate space [`decode_into`](Self::decode_into) will push —
	/// differs from [`dims`](Self::dims) when the decoder scales during
	/// decode.
	fn output_dims(&self) -> (u32, u32) {
		self.dims()
	}

	/// EXIF orientation (1–8) to apply to whatever this decode produces.
	fn orientation(&self) -> u8 {
		1
	}

	/// A cheap embedded preview (EXIF IFD1 thumb, HEIF `thmb` item), read
	/// from the header region only.
	///
	/// `mem_budget` is the caller's whole allowance — on this path the preview
	/// IS the output, so there is no accumulator canvas to leave room for.
	/// Anything that would not fit must be refused BEFORE it is materialised:
	/// the orchestrator can only filter a preview that has already been paid
	/// for, and these are not free (a 1 Mpx EXIF thumbnail measured 7.17 MiB,
	/// most of a remote decode's budget, on the path that is supposed to be
	/// the cheap one).
	fn embedded_preview(&mut self, mem_budget: usize) -> Result<Option<SmallImage>, ThumbError>;

	/// Contract: [`decode_into`](Self::decode_into) never allocates more than
	/// this, including the decoder's internal state. The orchestrator refuses
	/// decodes whose estimate (plus the accumulator canvas) exceeds budget.
	fn peak_estimate(&self) -> usize;

	fn decode_into(self: Box<Self>, sink: &mut dyn PixelSink) -> Result<(), ThumbError>;
}

/// Canvas dimensions: the decoder-output aspect, scaled so the canvas still
/// covers a 2× oversampled aspect-fill crop of the target, capped at
/// [`MAX_CANVAS_LONG_SIDE`] and at `canvas_budget`, and never upscaled above
/// the output.
///
/// `canvas_budget` is what is left of the caller's budget once the decoder's
/// own peak is subtracted, rather than a flat half: a row-streaming decoder
/// costs a few hundred KB, and charging it half the budget shrank the canvas —
/// and so the thumbnail — for no reason.
pub(crate) fn canvas_dims(
	output: (u32, u32),
	target: (u32, u32),
	canvas_budget: usize,
) -> (u32, u32) {
	let (ow, oh) = (u64::from(output.0.max(1)), u64::from(output.1.max(1)));
	let (tw, th) = (u64::from(target.0.max(1)), u64::from(target.1.max(1)));
	// Fill semantics: scale by the larger target/output ratio, oversampled 2×.
	// Work in per-mille to stay in integers; saturate at 1:1.
	let mut scale_mille = ((2000 * tw).div_ceil(ow))
		.max((2000 * th).div_ceil(oh))
		.min(1000);
	// Area cap from the budget; see `accumulator_bytes` for the per-pixel cost.
	let max_px = (canvas_budget as u64 / 20).max(1);
	while scale_mille > 1
		&& (ow * scale_mille / 1000).max(1) * (oh * scale_mille / 1000).max(1) > max_px
	{
		// Integer bisection beats floating sqrt here: a handful of steps, no
		// rounding surprises.
		scale_mille = scale_mille * 9 / 10;
	}
	let mut cw = (ow * scale_mille / 1000).max(1);
	let mut ch = (oh * scale_mille / 1000).max(1);
	let long = cw.max(ch);
	if long > u64::from(MAX_CANVAS_LONG_SIDE) {
		cw = (cw * u64::from(MAX_CANVAS_LONG_SIDE) / long).max(1);
		ch = (ch * u64::from(MAX_CANVAS_LONG_SIDE) / long).max(1);
	}
	(cw.min(ow) as u32, ch.min(oh) as u32)
}

/// Whether a preview is big enough to serve the request: its long side covers
/// at least half the requested long side.
fn preview_suffices(preview: &SmallImage, spec: &ThumbSpec) -> bool {
	u64::from(preview.width.max(preview.height)) * 2
		>= u64::from(spec.target_width.max(spec.target_height))
}

fn apply_orientation(image: SmallImage, orientation: u8) -> SmallImage {
	if orientation <= 1 {
		return image;
	}
	let Some(buffer) = image::RgbaImage::from_vec(image.width, image.height, image.rgba) else {
		return SmallImage {
			width: 0,
			height: 0,
			rgba: Vec::new(),
		};
	};
	let Some(orientation) = image::metadata::Orientation::from_exif(orientation) else {
		let (width, height) = buffer.dimensions();
		return SmallImage {
			width,
			height,
			rgba: buffer.into_raw(),
		};
	};
	let mut dynamic = image::DynamicImage::ImageRgba8(buffer);
	dynamic.apply_orientation(orientation);
	let rgba = dynamic.into_rgba8();
	let (width, height) = rgba.dimensions();
	SmallImage {
		width,
		height,
		rgba: rgba.into_raw(),
	}
}

/// Runs a prepared decode into a canvas the budget can hold, or answers `None`
/// when it cannot — the shared arithmetic behind every real decode.
///
/// `None` is not a failure: the caller falls back to whatever cheaper image it
/// already has. Two independent ceilings apply, memory (the decoder's own peak
/// plus the accumulator) and total work ([`MAX_SOURCE_AREA_PIXELS`] /
/// [`MAX_SOURCE_SIDE_PIXELS`], which a per-unit memory bound cannot express).
///
/// Nested decodes go through here too: a container that finds a real image
/// inside itself (a RAW file's embedded JPEG preview) prices that inner decode
/// with the same rules rather than reinventing them.
pub(crate) fn decode_bounded(
	prepared: Box<dyn PreparedDecode>,
	spec: &ThumbSpec,
) -> Result<Option<SmallImage>, ThumbError> {
	let source_dims = prepared.dims();
	let output = prepared.output_dims();
	if output.0 == 0 || output.1 == 0 {
		return Ok(None);
	}
	if u64::from(source_dims.0) * u64::from(source_dims.1) > MAX_SOURCE_AREA_PIXELS
		|| source_dims.0.max(source_dims.1) > MAX_SOURCE_SIDE_PIXELS
	{
		return Ok(None);
	}
	let peak = prepared.peak_estimate();
	let canvas = canvas_dims(
		output,
		(spec.target_width, spec.target_height),
		spec.mem_budget.saturating_sub(peak),
	);
	if peak
		.checked_add(accumulator_bytes(canvas))
		.is_none_or(|total| total > spec.mem_budget)
	{
		return Ok(None);
	}
	let mut acc = BoxAccumulator::new(output, canvas);
	prepared.decode_into(&mut acc)?;
	Ok(Some(acc.finish()))
}

/// The format that claims these bytes, from their first kilobyte.
///
/// 1 KB rather than the 16 bytes the magic numbers need: SVG has no magic and
/// its `<svg` root can sit behind an XML declaration, comments and a DOCTYPE.
/// One read either way; a short read only narrows the sniff.
fn sniff(src: &mut dyn ByteSource) -> Result<Option<&'static dyn FormatDecoder>, ThumbError> {
	let mut prefix = [0u8; 1024];
	let n = src.read_at(0, &mut prefix)?;
	Ok(formats::sniff(&prefix[..n]))
}

/// Finds the large JPEG a container embeds, decoding nothing.
///
/// On a real file, two to four bounded reads whatever its size — the same
/// directory walk the thumbnail path runs, stopped before it hands the bytes
/// to a decoder. On a forged tree the walk's own caps (boxes, directories,
/// candidates) bound the reads instead, each at most a chunk or two. `None` when the bytes are not a container this build walks, when
/// it hides no JPEG a viewer could show (see [`LocatedPreview`]), when the
/// largest one is under [`MIN_PREVIEW_LONG_SIDE`], or when its declared
/// length is more than a JPEG of its size could hold — a container
/// describing the sensor data behind the preview. Only a source read fails.
pub fn locate_preview(src: &mut dyn ByteSource) -> Result<Option<LocatedPreview>, ThumbError> {
	match sniff(src)? {
		Some(format) => format.locate_preview(src),
		None => Ok(None),
	}
}

/// Produces a downscaled (never exact-size, never upscaled) RGBA image for
/// the request, or says why it could not — see [`ThumbOutcome`]. The caller
/// does the final exact resize + encode — both operate on buffers this crate
/// already bounded.
pub fn generate(
	mut src: Box<dyn ByteSource>,
	spec: &ThumbSpec,
) -> Result<ThumbOutcome, ThumbError> {
	let Some(format) = sniff(&mut *src)? else {
		return Ok(ThumbOutcome::Unsupported);
	};
	// Clamped before `open`, because a decoder with an in-format scaler commits
	// to its output resolution there.
	let cap = affordable_target(spec.mem_budget);
	let spec = &ThumbSpec {
		target_width: spec.target_width.min(cap),
		target_height: spec.target_height.min(cap),
		..*spec
	};
	// `open` takes the source for good; the bulk hint below reaches it
	// through the relay's flag instead.
	let (src, bulk_read) = BulkHintRelay::new(src);
	let mut prepared = format.open(Box::new(src), spec)?;
	let orientation = prepared.orientation();

	// A broken preview must not fail a decodable image — previews are an
	// optimisation, the real decode is the contract. On a preview-only spec
	// there IS no real decode behind it, and swallowing there would answer a
	// settled `OverBudget` — "this file has no thumbnail", which callers cache
	// as final — for what may be a transient read failure. A failed read is
	// never swallowed at all: it says nothing about the image, and where the
	// preview is all there is (a RAW's embedded JPEG) swallowing it answers
	// that same settled `OverBudget`.
	let preview = match prepared.embedded_preview(spec.mem_budget) {
		Ok(preview) => preview,
		Err(e @ ThumbError::Io(_)) => return Err(e),
		Err(_) if spec.allow_full_decode => None,
		Err(e) => return Err(e),
	};
	// Backstop to the budget the format was handed: the preview plus the one
	// rotated copy `apply_orientation` may hold concurrently must fit (2× its
	// RGBA — 8 bytes per pixel); an oversized "preview" falls through to the
	// bounded decode instead.
	let mut preview = preview.filter(|p| p.rgba.len().saturating_mul(2) <= spec.mem_budget);
	let as_preview = |preview: SmallImage| {
		ThumbOutcome::Thumbnail(Thumbnail {
			image: apply_orientation(preview, orientation),
			source: ThumbSource::EmbeddedPreview,
		})
	};

	if let Some(preview) = preview.take_if(|p| preview_suffices(p, spec)) {
		return Ok(as_preview(preview));
	}

	if !spec.allow_full_decode {
		// The preview was already tried above; without a decode there is
		// nothing else to offer, and we have read only the header region.
		return Ok(preview.map_or(ThumbOutcome::OverBudget, as_preview));
	}

	// From here the source is read front to back: a lazy source may stream
	// ahead instead of fetching each chunk when the decoder stalls on it.
	bulk_read.store(true, Ordering::Relaxed);
	if let Some(image) = decode_bounded(prepared, spec)? {
		return Ok(ThumbOutcome::Thumbnail(Thumbnail {
			image: apply_orientation(image, orientation),
			source: ThumbSource::Decoded,
		}));
	}

	// Over budget (or over the work ceiling): an undersized preview beats no
	// thumbnail at all.
	Ok(preview.map_or(ThumbOutcome::OverBudget, as_preview))
}

#[cfg(test)]
mod canvas_tests {
	use super::{DEFAULT_MEM_BUDGET, canvas_dims};
	use crate::sink::accumulator_bytes;

	// `canvas_dims` is handed what is LEFT for the canvas, not the whole
	// budget: the orchestrator subtracts the decoder's own peak first.
	#[test]
	fn covers_the_target_and_fits_the_canvas_budget() {
		// 8000×6000 source, 512² target: the canvas keeps the source aspect,
		// covers the target, and its accumulator fits what it was given.
		// A cheap decoder leaves most of the budget for the canvas, and then
		// the canvas covers the target outright.
		let canvas_budget = DEFAULT_MEM_BUDGET;
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), canvas_budget);
		assert!(ch >= 512, "short side must cover the target, got {ch}");
		assert!(cw <= 1600 && ch <= 1600);
		assert!(accumulator_bytes((cw, ch)) <= canvas_budget + 20);
		// Small sources are never upscaled.
		assert_eq!(canvas_dims((100, 50), (512, 512), canvas_budget), (100, 50));
		// A decoder that eats most of the budget still gets a canvas, just a
		// coarser one — degrading beats refusing.
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 8);
		assert!(cw >= 1 && ch >= 1);
		assert!(accumulator_bytes((cw, ch)) <= DEFAULT_MEM_BUDGET / 8 + 20);
	}

	#[test]
	fn a_bigger_canvas_budget_buys_a_bigger_canvas() {
		// The point of spending what the decoder does not need: the same
		// source and target yield a larger canvas when more is available.
		let small = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 8);
		let large = canvas_dims((8000, 6000), (512, 512), DEFAULT_MEM_BUDGET / 2);
		assert!(
			large.0 > small.0 && large.1 > small.1,
			"{large:?} should exceed {small:?}"
		);
	}

	#[test]
	fn caps_the_long_side_and_survives_extreme_aspect_ratios() {
		let (cw, ch) = canvas_dims((100_000, 400), (1024, 1024), DEFAULT_MEM_BUDGET / 2);
		assert!(cw <= 1600);
		assert!(ch >= 1);
		// A tiny budget still yields a usable (if coarse) canvas.
		let (cw, ch) = canvas_dims((8000, 6000), (512, 512), 100_000);
		assert!(cw >= 1 && ch >= 1);
		assert!(accumulator_bytes((cw, ch)) <= 100_000 + 20);
	}

	#[test]
	fn an_oversized_request_is_clamped_to_what_the_budget_can_serve() {
		// iOS asks every provider for 2048x2048 regardless of display size.
		// Honouring that literally made the pipeline refuse every real photo;
		// the request is an upper bound, so it clamps instead.
		let cap = super::affordable_target(DEFAULT_MEM_BUDGET);
		assert!(cap < 2048, "a 12 MB budget cannot serve a 2048 canvas");
		assert!(cap >= 256, "but it must still serve a useful thumbnail");
	}
}
