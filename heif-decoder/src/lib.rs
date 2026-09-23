#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use std::{
	ffi::{CStr, CString, c_int, c_void},
	io::{Read, Seek, SeekFrom},
	marker::PhantomData,
};

use image::RgbaImage;

/// Runs the module's C++ static constructors, exactly once, before the first
/// libheif call.
///
/// On native targets the OS loader does this; on wasm32-unknown-unknown
/// nobody does by default — worse, when a module contains ctors and nothing
/// references `__wasm_call_ctors`, wasm-ld wraps EVERY export in "command"
/// semantics (ctors before the call, `__wasm_call_dtors` after), which both
/// re-runs the ctors on every exported call and runs the C++ atexit
/// destructors mid-life — the first wrapped export (`__wasm_init_tls`)
/// trapped inside `__funcs_on_exit`. lld's documented escape hatch is an
/// explicit reference to `__wasm_call_ctors`: its presence makes lld assume
/// ctors and dtors are our responsibility and skip the wrappers entirely.
/// libheif needs the ctors (its built-in decoder plugin is registered by a
/// global constructor); the destructors are deliberately never run — the
/// module's teardown is the page's.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn ensure_cpp_runtime_init() {
	unsafe extern "C" {
		fn __wasm_call_ctors();
	}
	static CTORS: std::sync::Once = std::sync::Once::new();
	// SAFETY: lld synthesizes __wasm_call_ctors for this module; Once makes
	// the single required invocation data-race-free.
	CTORS.call_once(|| unsafe { __wasm_call_ctors() });
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
fn ensure_cpp_runtime_init() {}

struct HeicContext<'a> {
	inner: *mut heif_context,
	_lifetime: PhantomData<&'a [u8]>,
}

impl HeicContext<'_> {
	fn from_slice(data: &[u8]) -> Result<Self, HeifError> {
		ensure_cpp_runtime_init();
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		let result = unsafe {
			heif_context_read_from_memory_without_copy(
				context,
				data.as_ptr() as *const c_void,
				data.len(),
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}

	fn from_file(path: &str) -> Result<HeicContext<'static>, HeifError> {
		ensure_cpp_runtime_init();
		let file_name = CString::new(path)
			.map_err(|_| HeifError::invalid_input("file path contains an interior NUL byte"))?;
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		let result =
			unsafe { heif_context_read_from_file(context, file_name.as_ptr(), std::ptr::null()) };
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}

	fn from_reader<T: Read + Seek>(reader: &mut HeifReader<T>) -> Result<Self, HeifError> {
		ensure_cpp_runtime_init();
		let context = unsafe { heif_context_alloc() };
		// Own the context immediately so it is freed by `Drop` on every early
		// return; otherwise a failed read below leaks it (and its buffers).
		let ctx = HeicContext {
			inner: context,
			_lifetime: PhantomData,
		};
		// The vtable pointer must be the one stored INSIDE the reader:
		// libheif keeps it and calls through it lazily on every later decode,
		// so a temporary table here would dangle the moment this call returns.
		let vtable = &raw const reader.vtable;
		let result = unsafe {
			heif_context_read_from_reader(
				context,
				vtable,
				reader as *mut _ as *mut c_void,
				std::ptr::null(),
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}

		Ok(ctx)
	}
}

impl Drop for HeicContext<'_> {
	fn drop(&mut self) {
		unsafe { heif_context_free(self.inner) };
	}
}

struct ImageHandle<'a> {
	inner: *mut heif_image_handle,
	_lifetime: PhantomData<&'a HeicContext<'a>>,
}

impl ImageHandle<'_> {
	fn new(ctx: &HeicContext) -> Result<Self, HeifError> {
		let mut handle = std::ptr::null_mut();
		let result = unsafe { heif_context_get_primary_image_handle(ctx.inner, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `ctx`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}
}

impl<'a> ImageHandle<'a> {
	fn width(&self) -> c_int {
		unsafe { heif_image_handle_get_width(self.inner) }
	}

	fn height(&self) -> c_int {
		unsafe { heif_image_handle_get_height(self.inner) }
	}

	/// Pixel count, or `None` when libheif reports non-positive dimensions.
	fn pixel_count(&self) -> Option<u64> {
		let (width, height) = (self.width(), self.height());
		if width <= 0 || height <= 0 {
			return None;
		}
		Some(width as u64 * height as u64)
	}

	fn covers(&self, target_width: u32, target_height: u32) -> bool {
		self.width() as i64 >= target_width as i64 && self.height() as i64 >= target_height as i64
	}

	/// Pixel count as DECLARED by the item's `ispe` box, before any
	/// transformation — `None` when the box is absent or non-positive.
	///
	/// This is the number libheif itself checks against
	/// `max_image_size_pixels`, and the only size a caller can learn without
	/// decoding. It is a declaration, not a measurement: see
	/// [`HeifSession::set_decode_limits`] for what that does and does not buy.
	fn ispe_pixel_count(&self) -> Option<u64> {
		let width = unsafe { heif_image_handle_get_ispe_width(self.inner) };
		let height = unsafe { heif_image_handle_get_ispe_height(self.inner) };
		if width <= 0 || height <= 0 {
			return None;
		}
		Some(width as u64 * height as u64)
	}

	/// The first embedded thumbnail, if any ("usually 0 or 1" per libheif).
	fn first_thumbnail(&self) -> Option<ImageHandle<'a>> {
		let count = unsafe { heif_image_handle_get_number_of_thumbnails(self.inner) };
		if count <= 0 {
			return None;
		}
		let mut id: heif_item_id = 0;
		let filled = unsafe { heif_image_handle_get_list_of_thumbnail_IDs(self.inner, &mut id, 1) };
		if filled < 1 {
			return None;
		}
		let mut handle = std::ptr::null_mut();
		let result = unsafe { heif_image_handle_get_thumbnail(self.inner, id, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok || handle.is_null() {
			return None;
		}
		Some(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}
}

impl Drop for ImageHandle<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_handle_release(self.inner) };
	}
}

/// Decoding options handed to every libheif decode call. Never NULL: the one
/// field that matters here is `num_codec_threads`, and libheif's default for
/// it (0) is wrong on both targets, in opposite ways.
///
/// libheif copies the field into `heif_decoder_plugin_options::num_threads`
/// (`codecs/decoder.cc`), and the two plugins read it differently:
///
/// - libde265's plugin turns 0 into 1 and hands the result to
///   `de265_start_worker_threads`, which starts a pool for any count > 0 and
///   skips pool startup entirely at <= 0.
/// - dav1d's plugin assigns a NON-ZERO value straight into
///   `Dav1dSettings::n_threads` and leaves 0 as dav1d's own default, which is
///   "ask the host for its logical CPU count". `dav1d_open` then rejects
///   anything negative or above `DAV1D_MAX_THREADS`
///   (`validate_input_or_ret(s->n_threads >= 0 && s->n_threads <=
///   DAV1D_MAX_THREADS …)`).
///
/// So on NATIVE both plugins are happy with one small POSITIVE count, and
/// pinning it is what makes the decode's memory cost a property of the file
/// rather than of the host: at libheif's default of 0, dav1d sizes its pool
/// from `hw.logicalcpu` and its setup allocation grows ~259 KB per logical
/// CPU, linearly across every count measured (libde265's is flat).
/// `microthumb`'s budget has to
/// charge for that setup up front, and it cannot charge for a term the host
/// picks. See [`NATIVE_CODEC_THREADS`].
///
/// On WASM no thread can ever start, and then the two plugins want OPPOSITE
/// things from that same field: libde265 must be told a NEGATIVE count,
/// because pool startup ignores the failed `pthread_create` while
/// `decoder_context::start_thread_pool` records the context as threaded
/// regardless — a WPP- or tile-coded bitstream would then queue work on a
/// pool with no workers and deadlock — whereas dav1d rejects negatives
/// outright and wants 1. So there the count is picked per file: -1 whenever
/// the file carries an HEVC item, 1 otherwise. A file carrying both (legal,
/// and not something any encoder produces) loses its AVIF decode rather than
/// risking the deadlock, which is the safe direction to be wrong in.
struct DecodeOptions(*mut heif_decoding_options);

/// Worker threads each codec plugin gets on native. One — which is already
/// all libde265 ever got here (its plugin turns libheif's default 0 into 1)
/// and is what takes dav1d off the host's CPU count.
///
/// The reason is memory, not parallelism: `microthumb` admits a decode by
/// charging its peak BEFORE running it, so every term of that charge has to
/// be a property of the file rather than of the machine, and at 0 dav1d's is
/// not. Peak heap of the whole thumbnail call, macOS `malloc_logger`, the
/// committed 1000×667 `parrot.avif`: 5.78 MB at 1 thread, 8.62 MB at 2,
/// 9.13 MB at 4, 10.17 MB at 8, 10.70 MB at libheif's default on a 10-CPU
/// host — and a verifier faking `hw.logicalcpu` measured that default rising
/// ~259 KB per logical CPU, linear from 4 to 32. The step past 1 thread is not a
/// fixed setup cost either: it tracks the unit decoded (+4 to 5 B per unit
/// pixel for the second thread), so it inflates the per-pixel charge too.
/// libde265 is flat across all of it — identical peaks at 1, 2, 4 and 8.
///
/// It costs next to nothing, because these are THUMBNAIL decodes: small
/// targets, one tile or one modest frame at a time, and the mobile cache
/// serialises them anyway. Best-of-5 wall clock, 1 thread vs libheif's
/// default on this 10-CPU host: `parrot.avif` 6.5 vs 6.3 ms, `parrot.heif`
/// 15.5 vs 15.6 ms, a 12 MP device HEIC through the tile path 233 vs 234 ms.
///
/// What it does forgo is HEVC speed libde265 could have had and never did:
/// at 2 threads `parrot.heif` decodes in 9.8 ms and that tile path in 161 ms.
/// Taking it would mean carrying dav1d's per-pixel inflation on the AVIF
/// side — a ~3 MiB setup charge again, and ~175k fewer pixels admitted per
/// decode. If that wall clock ever outweighs the admission limit, the upgrade
/// is per-codec counts off the same `hvc1` item probe [`wasm_codec_threads`]
/// already does: 2 for HEVC, 1 for AV1.
// ponytail: one count for both codecs; per-codec wants that probe on native too.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
const NATIVE_CODEC_THREADS: c_int = 1;

impl DecodeOptions {
	fn for_handle(handle: &ImageHandle) -> Result<Self, HeifError> {
		// SAFETY: alloc fills in defaults and the current options version;
		// num_codec_threads is a plain int field in that struct.
		let options = unsafe { heif_decoding_options_alloc() };
		if options.is_null() {
			// Falling back to NULL here would hand libheif its defaults —
			// the unbounded pool this exists to avoid — so a failed
			// allocation fails the decode instead.
			return Err(HeifError::invalid_input(
				"could not allocate decoding options",
			));
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		let threads = wasm_codec_threads(handle);
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		let threads = {
			let _ = handle;
			NATIVE_CODEC_THREADS
		};
		unsafe { (*options).num_codec_threads = threads };
		Ok(DecodeOptions(options))
	}

	fn as_ptr(&self) -> *const heif_decoding_options {
		self.0
	}
}

/// The `num_codec_threads` [`DecodeOptions`] must pass for this file: -1 while
/// any HEVC item is present, 1 otherwise. See [`DecodeOptions`] for why the
/// two plugins cannot share one value.
///
/// Asked per decode rather than per session because the tile path decodes
/// through a handle too, and a grid's own item type says nothing about the
/// codec of its tiles — the item list does.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn wasm_codec_threads(handle: &ImageHandle) -> c_int {
	/// The `infe` item type libheif dispatches to libde265
	/// (`Decoder::alloc_for_infe_type`); AV1 items are `av01`.
	const HEVC_ITEM_TYPE: u32 = u32::from_be_bytes(*b"hvc1");

	// A fresh wrapper around the same underlying context — it owns nothing but
	// itself, and has to be freed.
	// SAFETY: the handle is alive, so the context it came from is too.
	let context = unsafe { heif_image_handle_get_context(handle.inner) };
	if context.is_null() {
		// Cannot tell: keep libde265 safe and lose the AVIF.
		return -1;
	}
	let count = unsafe { heif_context_get_number_of_items(context) };
	let mut ids = vec![0 as heif_item_id; count.max(0) as usize];
	// SAFETY: `ids` has room for `count` entries, which is what libheif is
	// told; a zero count returns without writing.
	let filled = unsafe { heif_context_get_list_of_item_IDs(context, ids.as_mut_ptr(), count) };
	let filled = (filled.max(0) as usize).min(ids.len());
	let has_hevc = ids[..filled]
		.iter()
		.any(|&id| unsafe { heif_item_get_item_type(context, id) } == HEVC_ITEM_TYPE);
	unsafe { heif_context_free(context) };
	if has_hevc { -1 } else { 1 }
}

impl Drop for DecodeOptions {
	fn drop(&mut self) {
		if !self.0.is_null() {
			unsafe { heif_decoding_options_free(self.0) };
		}
	}
}

struct OutImage<'a> {
	inner: *mut heif_image,
	_lifetime: PhantomData<&'a ImageHandle<'a>>,
}

impl OutImage<'_> {
	fn new(handle: &ImageHandle) -> Result<Self, HeifError> {
		let mut heif_image_ptr = std::ptr::null_mut();
		let options = DecodeOptions::for_handle(handle)?;
		let result = unsafe {
			heif_decode_image(
				handle.inner,
				(&mut heif_image_ptr) as *mut *mut heif_image,
				heif_colorspace_heif_colorspace_RGB,
				heif_chroma_heif_chroma_interleaved_RGBA,
				options.as_ptr(),
			)
		};

		if result.code != heif_error_code_heif_error_Ok {
			// copy the message while `handle`, which owns the buffer it points into, is alive
			return Err(HeifError::from_raw(result));
		}

		Ok(OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		})
	}

	fn make_rgba(&self) -> Result<RgbaImage, HeifError> {
		let mut stride = 0usize;
		let plane_data = unsafe {
			heif_image_get_plane_readonly2(
				self.inner,
				heif_channel_heif_channel_interleaved,
				&mut stride as *mut usize,
			)
		};

		let width =
			unsafe { heif_image_get_width(self.inner, heif_channel_heif_channel_interleaved) };
		let height =
			unsafe { heif_image_get_height(self.inner, heif_channel_heif_channel_interleaved) };

		if plane_data.is_null() {
			return Err(HeifError::invalid_decoded_image());
		}
		let layout = validate_rgba_layout(width, height, stride)
			.ok_or_else(HeifError::invalid_decoded_image)?;

		let mut rgba_data = Vec::with_capacity(layout.capacity);

		for y in 0..layout.height {
			let row_start = y * stride;
			rgba_data.extend_from_slice(unsafe {
				std::slice::from_raw_parts(plane_data.add(row_start), layout.row_bytes)
			});
		}

		image::RgbaImage::from_vec(width as u32, height as u32, rgba_data)
			.ok_or_else(HeifError::invalid_decoded_image)
	}
}

impl Drop for OutImage<'_> {
	fn drop(&mut self) {
		unsafe { heif_image_release(self.inner) };
	}
}

struct RgbaLayout {
	height: usize,
	row_bytes: usize,
	capacity: usize,
}

fn validate_rgba_layout(width: c_int, height: c_int, stride: usize) -> Option<RgbaLayout> {
	if width <= 0 || height <= 0 {
		return None;
	}
	let width = width as usize;
	let height = height as usize;
	let row_bytes = width.checked_mul(4)?;
	if stride < row_bytes {
		return None;
	}
	let capacity = row_bytes.checked_mul(height)?;
	// the end of the last row must be addressable without overflowing usize
	let last_row_end = (height - 1).checked_mul(stride)?.checked_add(row_bytes)?;
	// Vec allocations and raw-pointer offsets are limited to isize::MAX bytes
	if capacity > isize::MAX as usize || last_row_end > isize::MAX as usize {
		return None;
	}
	Some(RgbaLayout {
		height,
		row_bytes,
		capacity,
	})
}

pub fn try_get_rgba_image_from_slice(data: &[u8]) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_slice(data)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_file(path: &str) -> Result<RgbaImage, HeifError> {
	let context = HeicContext::from_file(path)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

pub fn try_get_rgba_image_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
) -> Result<RgbaImage, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let image_handle = ImageHandle::new(&context)?;
	let out_image = OutImage::new(&image_handle)?;
	out_image.make_rgba()
}

/// Decodes an RGBA image suitable as a `target_width`×`target_height` thumbnail
/// source without ever decoding more than `max_pixels` pixels (the decode
/// briefly holds ~8 bytes per pixel: libheif's RGBA buffer plus the returned
/// copy). Preference order:
///
/// 1. an embedded thumbnail that covers the target (iPhone HEICs always carry
///    one, and it is orders of magnitude cheaper than the primary image),
/// 2. the primary image, when it fits the budget,
/// 3. an undersized embedded thumbnail — better than nothing when the primary
///    is over budget,
/// 4. `Ok(None)`: nothing fits the budget and the caller should skip the
///    thumbnail rather than decode an arbitrarily large image.
pub fn try_get_rgba_thumbnail_from_reader<T: Read + Seek>(
	reader: T,
	file_size: u64,
	target_width: u32,
	target_height: u32,
	max_pixels: u64,
) -> Result<Option<RgbaImage>, HeifError> {
	let mut heif_reader = HeifReader::new(reader, file_size);
	let context = HeicContext::from_reader(&mut heif_reader)?;
	let primary = ImageHandle::new(&context)?;
	let thumbnail = ImageHandle::first_thumbnail(&primary).filter(|thumb| {
		thumb
			.pixel_count()
			.is_some_and(|pixels| pixels <= max_pixels)
	});

	let handle = if let Some(thumb) = &thumbnail
		&& thumb.covers(target_width, target_height)
	{
		thumb
	} else if primary
		.pixel_count()
		.is_some_and(|pixels| pixels <= max_pixels)
	{
		&primary
	} else if let Some(thumb) = &thumbnail {
		thumb
	} else {
		return Ok(None);
	};

	let out_image = OutImage::new(handle)?;
	out_image.make_rgba().map(Some)
}

/// The tile grid of a `grid`-encoded HEIF (every Apple HEIC): decode one
/// 512²-ish tile at a time instead of the whole frame.
///
/// Everything is in display space, after the image's rotation and mirror.
/// Tile `(col, row)` covers the image from
/// `(col * tile_width - left_offset, row * tile_height - top_offset)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeifTiling {
	pub num_columns: u32,
	pub num_rows: u32,
	pub tile_width: u32,
	pub tile_height: u32,
	pub image_width: u32,
	pub image_height: u32,
	/// How far left of the image the first tile column starts. A grid only
	/// overhangs its image on the right and bottom as coded, but a rotation or
	/// mirror carries that overhang to the left or top of the displayed image —
	/// and each tile arrives already transformed, so the first column's
	/// overhang is its leading pixels.
	pub left_offset: u32,
	/// The same for the first tile row.
	pub top_offset: u32,
}

/// The overhang a transformed grid carries on its leading (left or top) edge:
/// `0` when libheif flags none there, else how far the grid runs past the
/// image along that axis. `None` when the grid's overrun is not a sliver of
/// one tile, whichever edge it sits on — a grid that does not cover its image,
/// or overruns it by a whole tile — which no placement can make sense of.
fn leading_overhang(flagged: u32, count: u32, tile: u32, image: u32) -> Option<u32> {
	let overhang = (u64::from(count) * u64::from(tile)).checked_sub(u64::from(image))?;
	let overhang = u32::try_from(overhang)
		.ok()
		.filter(|&overhang| overhang < tile)?;
	Some(if flagged == 0 { 0 } else { overhang })
}

/// A parsed HEIF whose reader stays open across calls, so the container can
/// be inspected (dims, tiling, embedded thumbnail) and then decoded tile by
/// tile without re-parsing. The one-shot `try_get_*` entry points above stay
/// for callers that want a single decode.
pub struct HeifSession<T: Read + Seek> {
	// Declared before `reader` on purpose: fields drop in declaration order,
	// and libheif must never outlive the reader its callbacks point into.
	context: HeicContext<'static>,
	// Boxed so the address libheif captured in `new` stays stable; libheif
	// calls back into it lazily on every decode.
	#[allow(dead_code)] // held for its stable address + Drop order
	reader: Box<HeifReader<T>>,
	/// Ceiling from [`set_decode_limits`](Self::set_decode_limits), kept on
	/// this side too: libheif does not apply its own `max_image_size_pixels`
	/// on the tile path, so [`decode_tile_rgba`](Self::decode_tile_rgba)
	/// enforces it against the tile item itself.
	max_image_pixels: Option<u64>,
	/// Whether the whole grid has been size-checked against
	/// `max_image_pixels` yet — the scan is per session, not per tile.
	tiles_validated: std::cell::Cell<bool>,
}

impl<T: Read + Seek> HeifSession<T> {
	pub fn new(reader: T, file_size: u64) -> Result<Self, HeifError> {
		let mut reader = Box::new(HeifReader::new(reader, file_size));
		// Mirrors `HeicContext::from_reader`, but the reader is heap-pinned so
		// the context may keep calling it (and the vtable stored inside it)
		// after this function returns. The 'static on the context is the same
		// lifetime laundering `from_file` already does; the struct's field
		// order keeps it honest.
		let context: HeicContext<'static> = HeicContext::from_reader(&mut reader)?;
		Ok(HeifSession {
			context,
			reader,
			max_image_pixels: None,
			tiles_validated: std::cell::Cell::new(false),
		})
	}

	/// Caps what any single decode through this session may materialise.
	///
	/// Two guards, with different reach — be precise about which is which:
	///
	/// - `max_image_pixels` is enforced HERE, before decoding, against each
	///   item's DECLARED (`ispe`) size: libheif applies its own check only
	///   when decoding a whole image (`image_item.cc`,
	///   `if (!decode_tile_only)`), so on the tile path
	///   [`decode_tile_rgba`](Self::decode_tile_rgba) does it instead.
	/// - `max_total_memory` is enforced by libheif at
	///   `heif_image_add_plane_safe`, i.e. when the decoded picture is copied
	///   OUT of the codec. That is a backstop, not a pre-allocation guard.
	///
	/// What neither guard covers: a bitstream whose SPS declares a picture
	/// larger than the item's `ispe` promised. libde265 (vendored, 1.0.16)
	/// takes no size limit of its own and materialises the picture in its DPB
	/// before libheif sees a single plane, so such a file is caught only on
	/// copy-out, after the codec has already allocated. That gap is libheif's
	/// on every path, tiled or not — it is not specific to this API, and it
	/// cannot be closed without bounding libde265 itself.
	///
	/// The limits live on the context and apply to every later decode call;
	/// the container parse in [`new`](Self::new) already happened under
	/// libheif's global defaults.
	pub fn set_decode_limits(&mut self, max_image_pixels: u64, max_total_memory: u64) {
		self.max_image_pixels = Some(max_image_pixels);
		self.tiles_validated.set(false);
		let limits = unsafe { heif_context_get_security_limits(self.context.inner) };
		if limits.is_null() {
			// Not reachable per the API contract; losing the cap must not
			// break decoding, the caller's own budget checks still stand.
			return;
		}
		unsafe {
			(*limits).max_image_size_pixels = max_image_pixels;
			(*limits).max_total_memory = max_total_memory;
		}
	}

	fn primary(&self) -> Result<ImageHandle<'_>, HeifError> {
		ImageHandle::new(&self.context)
	}

	pub fn primary_dims(&self) -> Result<(u32, u32), HeifError> {
		let primary = self.primary()?;
		let (width, height) = (primary.width(), primary.height());
		if width <= 0 || height <= 0 {
			return Err(HeifError::invalid_decoded_image());
		}
		Ok((width as u32, height as u32))
	}

	/// The embedded thumbnail, decoded — `None` when there is none or it is
	/// implausibly large for a thumbnail.
	pub fn embedded_thumbnail_rgba(&self, max_pixels: u64) -> Result<Option<RgbaImage>, HeifError> {
		let primary = self.primary()?;
		let Some(thumb) = primary.first_thumbnail() else {
			return Ok(None);
		};
		if thumb.pixel_count().is_none_or(|px| px > max_pixels) {
			return Ok(None);
		}
		OutImage::new(&thumb)?.make_rgba().map(Some)
	}

	/// The primary image's tile grid, or `None` when it is a single tile, is
	/// cropped, or its geometry does not add up (then only
	/// [`decode_primary_rgba`](Self::decode_primary_rgba) helps).
	/// Sizes are in the transformed (display) coordinate space, matching what
	/// [`decode_tile_rgba`](Self::decode_tile_rgba) produces.
	pub fn tiling(&self) -> Result<Option<HeifTiling>, HeifError> {
		let primary = self.primary()?;
		let mut raw: heif_image_tiling = unsafe { std::mem::zeroed() };
		let result = unsafe { heif_image_handle_get_image_tiling(primary.inner, 1, &mut raw) };
		if result.code != heif_error_code_heif_error_Ok {
			return Ok(None);
		}
		if raw.num_columns <= 1 && raw.num_rows <= 1 {
			return Ok(None);
		}
		if raw.tile_width == 0
			|| raw.tile_height == 0
			|| raw.image_width == 0
			|| raw.image_height == 0
		{
			return Ok(None);
		}
		// A clean-aperture crop (`clap`) narrows the image libheif shows but
		// not the tiling it reports, and a tile decode skips the crop, so tiles
		// cannot be placed to honour it. The whole-frame decode crops.
		if (i64::from(primary.width()), i64::from(primary.height()))
			!= (i64::from(raw.image_width), i64::from(raw.image_height))
		{
			return Ok(None);
		}
		// libheif's own offsets say which leading edges the transformations
		// carried the overhang to, but not how far it reaches: 1.23.1 stores
		// the remainder `image % tile` where its header documents the overhang
		// `tile - image % tile` (`image_item.cc`,
		// `process_image_transformations_on_tiling`). So the edge comes from
		// libheif and the size from the grid itself; once upstream stores the
		// overhang, the two agree.
		let (Some(left_offset), Some(top_offset)) = (
			leading_overhang(
				raw.left_offset,
				raw.num_columns,
				raw.tile_width,
				raw.image_width,
			),
			leading_overhang(
				raw.top_offset,
				raw.num_rows,
				raw.tile_height,
				raw.image_height,
			),
		) else {
			return Ok(None);
		};
		Ok(Some(HeifTiling {
			num_columns: raw.num_columns,
			num_rows: raw.num_rows,
			tile_width: raw.tile_width,
			tile_height: raw.tile_height,
			image_width: raw.image_width,
			image_height: raw.image_height,
			left_offset,
			top_offset,
		}))
	}

	/// The grid tile item at ORIGINAL (untransformed) grid indices, as its own
	/// handle, so its declared size can be read before anything decodes it.
	/// `None` when the primary is not a grid or the indices name no tile.
	///
	/// Untransformed on purpose. `heif_image_handle_get_grid_image_tile_id`
	/// range-checks its arguments against the untransformed grid and only
	/// THEN maps them (`heif_tiling.cc`), so passing display coordinates makes
	/// it reject the rows a rotation added — an 8x6 grid shown as 6x8 loses
	/// rows 6 and 7. Enumerating the original grid sidesteps that entirely,
	/// and for a size bound the tile ORDER does not matter.
	fn original_tile_handle(
		&self,
		primary: &ImageHandle<'_>,
		tile_x: u32,
		tile_y: u32,
	) -> Option<ImageHandle<'_>> {
		let mut tile_id: heif_item_id = 0;
		let result = unsafe {
			heif_image_handle_get_grid_image_tile_id(primary.inner, 0, tile_x, tile_y, &mut tile_id)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return None;
		}
		let mut handle = std::ptr::null_mut();
		let result =
			unsafe { heif_context_get_image_handle(self.context.inner, tile_id, &mut handle) };
		if result.code != heif_error_code_heif_error_Ok || handle.is_null() {
			return None;
		}
		Some(ImageHandle {
			inner: handle,
			_lifetime: PhantomData,
		})
	}

	/// Checks EVERY tile's declared (`ispe`) size against the ceiling, once
	/// per session. Cheap: item-property reads, no decoding.
	///
	/// Whole-grid rather than per-tile because the tiling struct reports only
	/// TILE 0's size (`grid.cc`, `get_heif_image_tiling`) — the size the
	/// caller's own budget arithmetic is built on — so a grid whose later
	/// tiles declare something far larger would otherwise sail through the
	/// budget check and reach the codec unchallenged.
	///
	/// The scan walks a square that contains the original grid whichever way
	/// round it is, and counts what libheif accepted: every coordinate it
	/// names must check out, and the count must reach the tile total, or the
	/// grid is refused rather than half-verified.
	fn validate_tile_sizes(
		&self,
		primary: &ImageHandle<'_>,
		max_pixels: u64,
	) -> Result<(), HeifError> {
		let Some(tiling) = self.tiling()? else {
			return Ok(());
		};
		let expected = u64::from(tiling.num_columns) * u64::from(tiling.num_rows);
		let span = tiling.num_columns.max(tiling.num_rows);
		let mut checked = 0u64;
		for y in 0..span {
			for x in 0..span {
				let Some(tile) = self.original_tile_handle(primary, x, y) else {
					continue;
				};
				match tile.ispe_pixel_count() {
					Some(pixels) if pixels <= max_pixels => checked += 1,
					Some(pixels) => {
						return Err(HeifError::invalid_input(&format!(
							"grid tile declares {pixels} pixels, over the {max_pixels} allowed"
						)));
					}
					None => {
						return Err(HeifError::invalid_input(
							"a grid tile does not declare its size; refusing to decode the grid",
						));
					}
				}
			}
		}
		if checked < expected {
			return Err(HeifError::invalid_input(&format!(
				"only {checked} of {expected} grid tiles could be size-checked; refusing the grid"
			)));
		}
		Ok(())
	}

	/// Decodes exactly one grid tile to RGBA (~tile-sized allocation, not
	/// image-sized). Tile coordinates are grid indices, transformed space.
	///
	/// The first call verifies every tile's declared size against
	/// [`set_decode_limits`](Self::set_decode_limits), because libheif skips
	/// its own `ispe` check for single-tile decodes
	/// (`image_item.cc`, `if (!decode_tile_only)`).
	pub fn decode_tile_rgba(&self, tile_x: u32, tile_y: u32) -> Result<RgbaImage, HeifError> {
		let primary = self.primary()?;
		if let Some(max_pixels) = self.max_image_pixels
			&& !self.tiles_validated.get()
		{
			self.validate_tile_sizes(&primary, max_pixels)?;
			self.tiles_validated.set(true);
		}
		let mut heif_image_ptr = std::ptr::null_mut();
		let options = DecodeOptions::for_handle(&primary)?;
		let result = unsafe {
			heif_image_handle_decode_image_tile(
				primary.inner,
				&mut heif_image_ptr,
				heif_colorspace_heif_colorspace_RGB,
				heif_chroma_heif_chroma_interleaved_RGBA,
				options.as_ptr(),
				tile_x,
				tile_y,
			)
		};
		if result.code != heif_error_code_heif_error_Ok {
			return Err(HeifError::from_raw(result));
		}
		let out = OutImage {
			inner: heif_image_ptr,
			_lifetime: PhantomData,
		};
		out.make_rgba()
	}

	/// Whole-frame decode of the primary image — only for sources the caller
	/// already verified are small.
	pub fn decode_primary_rgba(&self) -> Result<RgbaImage, HeifError> {
		let primary = self.primary()?;
		OutImage::new(&primary)?.make_rgba()
	}
}

struct HeifReader<T>
where
	T: Read + Seek,
{
	inner: T,
	file_size: u64,
	/// libheif does NOT copy the function table — `StreamReader_CApi` keeps
	/// the raw `const heif_reader*` and dereferences it on every later lazy
	/// read (bitstream.h). The table must therefore live exactly as long as
	/// the reader itself, not as a temporary at the registration call site.
	vtable: heif_reader,
}

impl<T: Read + Seek> HeifReader<T> {
	fn new(inner: T, file_size: u64) -> Self {
		HeifReader {
			inner,
			file_size,
			vtable: heif_reader {
				reader_api_version: 1,
				get_position: Some(get_position_impl::<T>),
				read: Some(read_impl::<T>),
				seek: Some(seek_impl::<T>),
				wait_for_file_size: Some(wait_for_file_size_impl::<T>),
				request_range: None,
				preload_range_hint: None,
				release_file_range: None,
				release_error_msg: None,
			},
		}
	}

	fn get_position(&mut self) -> Result<u64, std::io::Error> {
		self.inner.stream_position()
	}

	// Reads exactly `buffer.len()` bytes, looping over short reads (which a
	// generic `Read` may return mid-file) and treating a premature EOF as an
	// error. libheif only calls the read callback after `wait_for_file_size`
	// confirmed the bytes are available, so a short read is a genuine failure,
	// not a partial success to zero-fill.
	fn read_exact(&mut self, buffer: &mut [u8]) -> Result<(), std::io::Error> {
		self.inner.read_exact(buffer)
	}

	// Helper method to seek
	fn seek(&mut self, position: i64) -> Result<(), std::io::Error> {
		self.inner.seek(SeekFrom::Start(position as u64))?;
		Ok(())
	}

	// Helper method to check if we can read up to target_size
	fn wait_for_file_size(&mut self, target_size: i64) -> heif_reader_grow_status {
		if target_size as u64 <= self.file_size {
			heif_reader_grow_status_heif_reader_grow_status_size_reached
		} else {
			heif_reader_grow_status_heif_reader_grow_status_size_beyond_eof
		}
	}
}

unsafe extern "C" fn get_position_impl<T: Read + Seek>(userdata: *mut c_void) -> i64 {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.get_position().map(|pos| pos as i64).unwrap_or(-1)
}

unsafe extern "C" fn read_impl<T: Read + Seek>(
	data: *mut c_void,
	size: usize,
	userdata: *mut c_void,
) -> c_int {
	// A zero-length read is a no-op, and libheif does ask for them with a null
	// pointer: `std::vector::data()` on an empty vector is allowed to return
	// null, which is exactly what `Box_av1C::parse` hands over for an av1C
	// with no trailing config OBUs — i.e. for most AVIF files. Answering -1
	// there fails the whole container parse with "unexpected end of file".
	if size == 0 {
		return 0;
	}
	// from_raw_parts_mut requires a non-null pointer, and libheif treats any
	// non-zero return as a read failure. Only reads that ask for bytes reach
	// here — the zero-length case returned above.
	if data.is_null() {
		return -1;
	}
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	let buffer = unsafe { std::slice::from_raw_parts_mut(data as *mut u8, size) };

	match reader.read_exact(buffer) {
		Ok(()) => 0,  // Success — the whole buffer was filled
		Err(_) => -1, // Error, or a genuine short read / premature EOF
	}
}

unsafe extern "C" fn seek_impl<T: Read + Seek>(position: i64, userdata: *mut c_void) -> c_int {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	match reader.seek(position) {
		Ok(_) => 0,   // Success
		Err(_) => -1, // Error
	}
}

unsafe extern "C" fn wait_for_file_size_impl<T: Read + Seek>(
	target_size: i64,
	userdata: *mut c_void,
) -> heif_reader_grow_status {
	let reader = unsafe { &mut *(userdata as *mut HeifReader<T>) };
	reader.wait_for_file_size(target_size)
}

#[derive(Debug)]
pub struct HeifError {
	code: heif_error_code,
	#[allow(dead_code)] // diagnostic detail, surfaced via the Debug impl
	subcode: heif_suberror_code,
	message: String,
}

impl HeifError {
	/// Copies the message eagerly: libheif error messages point into a buffer
	/// owned by the producing context/handle, so the pointer dangles once that
	/// object is freed. Call this while the producing object is still alive.
	fn from_raw(error: heif_error) -> Self {
		let message = if error.message.is_null() {
			String::from("unknown error")
		} else {
			unsafe { CStr::from_ptr(error.message) }
				.to_string_lossy()
				.into_owned()
		};
		HeifError {
			code: error.code,
			subcode: error.subcode,
			message,
		}
	}

	fn invalid_input(message: &str) -> Self {
		HeifError {
			code: heif_error_code_heif_error_Invalid_input,
			subcode: heif_suberror_code_heif_suberror_Unspecified,
			message: String::from(message),
		}
	}

	fn invalid_decoded_image() -> Self {
		Self::invalid_input("decoded image has invalid plane, dimensions, or stride")
	}
}

impl std::fmt::Display for HeifError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(
			f,
			"heif error: code: {}, message: {}",
			self.code, self.message
		)
	}
}

impl std::error::Error for HeifError {}

// #[cfg(test)]
// mod tests {
// 	use super::*;

// 	const TEST_HEIC_FILE: &str = "/Users/end/Documents/tmp/image1.heic"; // Update this path to a valid HEIC file for testing
// 	const TEST_OUTPUT_DIR: &str = "/Users/end/Documents/tmp/"; // Update this path to a valid output directory

// 	// very basic tests for now

// 	#[test]
// 	fn test_reader() {
// 		let heic_file = std::fs::File::open(TEST_HEIC_FILE).unwrap();
// 		let file_size = heic_file.metadata().unwrap().len();
// 		let image = try_get_rgba_image_from_reader(heic_file, file_size).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_reader.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_file() {
// 		let image = try_get_rgba_image_from_file(TEST_HEIC_FILE).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_file.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}

// 	#[test]
// 	fn test_slice() {
// 		let heic_data = std::fs::read(TEST_HEIC_FILE).unwrap();
// 		let image = try_get_rgba_image_from_slice(&heic_data).unwrap();
// 		let mut file = std::fs::File::create(format!("{TEST_OUTPUT_DIR}/from_slice.png")).unwrap();
// 		image.write_to(&mut file, image::ImageFormat::Png).unwrap();
// 	}
// }

/// Link-time stubs for the host functions wasi-libc and the C++ runtime
/// expect, so the browser module ends up with ZERO imports beyond
/// wasm-bindgen's own — no JS shim to write or maintain.
///
/// wasi-libc's syscall wrappers reference `__imported_wasi_snapshot_preview1_*`
/// symbols that normally become `wasi_snapshot_preview1.*` imports;
/// wasm-bindgen's loader supplies no such module, so a live import would fail
/// instantiation. Defining the symbols here makes the linker resolve them
/// instead of importing. They back wasi-libc's stdio/stderr plumbing, its
/// preopen scan, and abort: every fd answers EBADF with its out-params
/// zeroed, the environment is empty, and the terminal paths trap, which is
/// what abort means in a wasm module anyway.
///
/// Mostly, but NOT entirely, unreachable: `fd_write` is on the malformed-input
/// path, because libde265 prints its bitstream complaints to stderr.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasi_stubs {
	use std::arch::wasm32::unreachable;

	/// WASI errno for "bad file descriptor" — also what ends wasi-libc's
	/// preopen discovery loop cleanly.
	const EBADF: i32 = 8;

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_environ_get(
		_environ: *mut u32,
		_buf: *mut u8,
	) -> i32 {
		0
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_environ_sizes_get(
		count: *mut u32,
		buf_size: *mut u32,
	) -> i32 {
		// SAFETY: wasi-libc passes pointers to two locals it reads afterwards;
		// report an empty environment through them.
		unsafe {
			*count = 0;
			*buf_size = 0;
		}
		0
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_close(_fd: i32) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_fdstat_get(_fd: i32, _stat: *mut u8) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_prestat_get(_fd: i32, _buf: *mut u8) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_prestat_dir_name(
		_fd: i32,
		_path: *mut u8,
		_len: usize,
	) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_read(
		_fd: i32,
		_iovs: *const u8,
		_iovs_len: usize,
		nread: *mut usize,
	) -> i32 {
		// SAFETY: wasi-libc passes a pointer to a local it reads back. An
		// errno does not oblige a host to write the out-param, but leaving it
		// untouched leaves a caller that ignores the errno reading whatever
		// was on the stack — so answer zero as well.
		unsafe { *nread = 0 };
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_readdir(
		_fd: i32,
		_buf: *mut u8,
		_buf_len: usize,
		_cookie: i64,
		used: *mut usize,
	) -> i32 {
		// SAFETY: as in fd_read.
		unsafe { *used = 0 };
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_seek(
		_fd: i32,
		_offset: i64,
		_whence: i32,
		new_offset: *mut u64,
	) -> i32 {
		// SAFETY: as in fd_read.
		unsafe { *new_offset = 0 };
		EBADF
	}

	/// The one stub that is genuinely reached: libde265 prints to stderr from
	/// its bitstream sanity checks, which malformed input triggers. Failing
	/// the write is the intended behaviour (there is nowhere to print), and
	/// the decode carries on regardless.
	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_fd_write(
		_fd: i32,
		_iovs: *const u8,
		_iovs_len: usize,
		nwritten: *mut usize,
	) -> i32 {
		// SAFETY: as in fd_read.
		unsafe { *nwritten = 0 };
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_path_filestat_get(
		_fd: i32,
		_flags: i32,
		_path: *const u8,
		_path_len: usize,
		_buf: *mut u8,
	) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_path_open(
		_fd: i32,
		_dirflags: i32,
		_path: *const u8,
		_path_len: usize,
		_oflags: i32,
		_rights_base: i64,
		_rights_inheriting: i64,
		_fdflags: i32,
		opened_fd: *mut i32,
	) -> i32 {
		// SAFETY: as in fd_read — never hand back an uninitialised fd.
		unsafe { *opened_fd = -1 };
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_path_unlink_file(
		_fd: i32,
		_path: *const u8,
		_path_len: usize,
	) -> i32 {
		EBADF
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_sched_yield() -> i32 {
		0
	}

	/// Referenced by the threads-libc's timed waits (pthread_cond etc.),
	/// which stay unreachable — the pool is never started. Answers with a
	/// frozen clock rather than an error so a hypothetical caller spins
	/// instead of aborting.
	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_clock_time_get(
		_clock_id: i32,
		_precision: i64,
		time: *mut u64,
	) -> i32 {
		// SAFETY: wasi-libc passes a pointer to a local timestamp it reads
		// back.
		unsafe { *time = 0 };
		0
	}

	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_snapshot_preview1_proc_exit(_code: i32) -> ! {
		unreachable()
	}

	/// wasi-threads' host spawn hook (threaded sysroot only). A negative
	/// return means "could not spawn", which pthread_create surfaces as
	/// EAGAIN — and DecodeOptions keeps both codec plugins from ever asking.
	#[unsafe(no_mangle)]
	extern "C" fn __imported_wasi_thread_spawn(_start_arg: *mut u8) -> i32 {
		-1
	}

	/// The vendored C++ compiles with exceptions on (heif_cxx.h forbids
	/// -fno-exceptions) and libheif's own code never throws deliberately — but
	/// the C++ runtime does: every `.resize()` in it can raise `length_error`
	/// or `bad_alloc`, and a malformed file whose declared sizes are absurd is
	/// how that happens. There is no catch on the decode path, so such a throw
	/// TRAPS the module — the same end state as the `abort()` a native build
	/// reaches, and the reason a HEIC decode must never be the only thing
	/// keeping a wasm instance alive.
	#[unsafe(no_mangle)]
	extern "C" fn __cxa_allocate_exception(_size: usize) -> *mut u8 {
		unreachable()
	}

	#[unsafe(no_mangle)]
	extern "C" fn __cxa_throw(_exception: *mut u8, _type_info: *mut u8, _dtor: *mut u8) -> ! {
		unreachable()
	}
}

#[cfg(test)]
mod layout_tests {
	use super::validate_rgba_layout;

	#[test]
	fn rejects_non_positive_dimensions() {
		assert!(validate_rgba_layout(0, 10, 40).is_none());
		assert!(validate_rgba_layout(10, 0, 40).is_none());
		assert!(validate_rgba_layout(-1, 10, 40).is_none());
		assert!(validate_rgba_layout(10, -1, 40).is_none());
		assert!(validate_rgba_layout(i32::MIN, i32::MIN, usize::MAX).is_none());
	}

	#[test]
	fn rejects_stride_smaller_than_row() {
		assert!(validate_rgba_layout(10, 10, 39).is_none());
		assert!(validate_rgba_layout(10, 10, 0).is_none());
	}

	#[test]
	fn rejects_row_offset_overflow() {
		assert!(validate_rgba_layout(2, 2, usize::MAX).is_none());
		assert!(validate_rgba_layout(i32::MAX, i32::MAX, usize::MAX).is_none());
	}

	#[test]
	fn accepts_dimensions_whose_byte_size_overflows_i32() {
		let side = 23_171_i32;
		let layout = validate_rgba_layout(side, side, side as usize * 4).unwrap();
		assert_eq!(layout.height, side as usize);
		assert_eq!(layout.row_bytes, side as usize * 4);
		assert_eq!(layout.capacity, side as usize * side as usize * 4);
	}

	#[test]
	fn accepts_padded_stride() {
		let layout = validate_rgba_layout(3, 2, 16).unwrap();
		assert_eq!(layout.height, 2);
		assert_eq!(layout.row_bytes, 12);
		assert_eq!(layout.capacity, 24);
	}

	#[test]
	fn rejects_capacity_exceeding_isize_max() {
		// width == height == i32::MAX with an unpadded stride: the byte counts
		// still fit in usize (so the checked_mul overflow guards above don't
		// trigger), but the resulting capacity exceeds isize::MAX, which
		// Vec::with_capacity cannot allocate.
		let side = i32::MAX;
		let row_bytes = side as usize * 4;
		assert!((row_bytes * side as usize) > isize::MAX as usize);
		assert!(validate_rgba_layout(side, side, row_bytes).is_none());
	}
}

#[cfg(test)]
mod api_tests {
	use std::io::Cursor;

	use super::*;

	/// Which codecs this build actually registered.
	///
	/// Lives in the LIB, not in `tests/`, on purpose: CI and the pre-push hook
	/// both run `cargo test --lib`, which never builds an integration target.
	/// A tripwire nothing executes is not a tripwire, and this is the only
	/// thing that notices a decoder going missing — a missing decoder looks
	/// exactly like an unsupported file everywhere else in the stack.
	#[test]
	fn hevc_and_av1_decode() {
		// SAFETY: both are pure lookups over libheif's static plugin registry.
		let (hevc, av1) = unsafe {
			(
				heif_have_decoder_for_format(heif_compression_format_heif_compression_HEVC),
				heif_have_decoder_for_format(heif_compression_format_heif_compression_AV1),
			)
		};
		assert_ne!(
			hevc, 0,
			"libde265 missing — every HEIC thumbnail would fail"
		);
		assert_ne!(
			av1, 0,
			"dav1d missing — every AVIF thumbnail would fail. If AV1 is being \
			 dropped on purpose, drop the avif/avis brands from microthumb's \
			 formats/heif.rs and `avif` from the SDK's thumbnail extension gate \
			 and mime table in the same change"
		);

		// …and that both came from the ARCHIVE, not from a directory this
		// machine happens to have. libheif defaults dav1d to a separate
		// plugin, and the answers above cannot tell a statically registered
		// decoder from one dlopened out of the build machine's `OUT_DIR` —
		// which is exactly what shipped, and what makes every AVIF thumbnail
		// fail on a device while this test passes. With plugin loading
		// compiled out there is no dlopen at all, so `heif_load_plugin` can
		// only answer `Unsupported_feature`: assert on that, and the two
		// answers above are necessarily static ones.
		//
		// SAFETY: with plugin loading disabled this returns a constant error
		// and never touches the path.
		let path = c"/nonexistent/libheif-dav1d.so";
		let mut info = std::ptr::null();
		let error = unsafe { heif_load_plugin(path.as_ptr(), &raw mut info) };
		assert_eq!(
			error.code, heif_error_code_heif_error_Unsupported_feature,
			"libheif was built with plugin loading ON — the AV1 decoder is a \
			 separate .so with the build machine's absolute OUT_DIR baked in, \
			 and every AVIF thumbnail fails silently on device. See \
			 ENABLE_PLUGIN_LOADING in build.rs"
		);
	}

	#[test]
	fn from_file_rejects_path_with_interior_nul() {
		let path = "does/not/exist\0evil.heic";
		let err = try_get_rgba_image_from_file(path).unwrap_err();
		// must fail before any filesystem access is attempted
		assert!(err.to_string().contains("interior NUL byte"));
	}

	#[test]
	fn read_impl_rejects_null_data_pointer() {
		let mut reader = HeifReader::new(Cursor::new(Vec::<u8>::new()), 0);
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				std::ptr::null_mut(),
				4,
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, -1);
	}

	#[test]
	fn read_impl_accepts_a_zero_length_read_through_a_null_pointer() {
		// What libheif's av1C parse does for a config with no trailing OBUs.
		// Refusing it fails the container parse of most AVIF files.
		let mut reader = HeifReader::new(Cursor::new(Vec::<u8>::new()), 0);
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				std::ptr::null_mut(),
				0,
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, 0);
	}

	#[test]
	fn read_impl_reports_short_read_as_failure() {
		// The reader holds only 2 bytes but libheif asks for 4. A short read must
		// be reported as a failure (-1), not zero-filled and reported as success.
		let mut reader = HeifReader::new(Cursor::new(vec![1u8, 2]), 2);
		let mut buf = [0xEEu8; 4];
		let result = unsafe {
			read_impl::<Cursor<Vec<u8>>>(
				buf.as_mut_ptr() as *mut c_void,
				buf.len(),
				&mut reader as *mut _ as *mut c_void,
			)
		};
		assert_eq!(result, -1);
	}
}
