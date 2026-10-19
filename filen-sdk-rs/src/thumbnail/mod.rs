use image::{DynamicImage, codecs::webp::WebPEncoder, imageops::FilterType};

use crate::{ErrorKind, error::Error};

/// Extensions worth spending bytes on, evaluated from the filename at READ
/// time. The one gate in front of the pipeline, on every target: the mobile
/// cache, `makeThumbnailInMemory` and the `canMakeThumbnail` flag on every
/// JS file shape (`File`, `SharedFile`, `LinkedFile`) all consult it, so a
/// format is admitted or refused in one place.
///
/// This list is ours on purpose. The stored mime is written once at upload by
/// whichever client uploaded the file — `mime-types` (JS), `mime_guess` (Rust)
/// or Go's stdlib `mime` — and then never revisited, so it is a fossil of that
/// library's table at that date, on that machine:
///
/// - all three collapse "unknown extension", "no extension" and "genuinely
///   binary" into `application/octet-stream`, so a NEGATIVE carries no
///   information at all;
/// - HEIC — the dominant phone format — is stored as octet-stream by every
///   Rust client before June 2024 (when `mime_guess` gained it) and by Go
///   clients on macOS or in slim containers, where the host has no
///   `/etc/mime.types`;
/// - RAW formats are absent from the JS table entirely, and from Go's unless
///   the host happens to have `shared-mime-info` installed.
///
/// Deciding from the extension now, against a table we control, makes the
/// answer the same everywhere and lets a new format be added in one line
/// instead of waiting for three upstreams and a re-upload.
///
/// The RAW entries reach for the JPEG the camera embeds beside the sensor
/// mosaic, which is the only thing the pipeline ever decodes from a RAW file.
/// microthumb's raw characterisation pins that against 100 CC0 samples: every
/// CR2/CR3/NEF/ARW/PEF/SRW/ORF/RW2/RAF sample yields one, and the DNGs that do
/// not are cinema/CFA files with no displayable image at all. The tail entries
/// (3fr, iiq, ...) walk the same TIFF directories and are attempts rather than
/// promises: a miss costs a chunk or two and answers "no thumbnail".
///
/// The cfg gates mirror what the pipeline can decode in THIS build, and must
/// stay in step with it: `heic`/`heif`/`hif`/`avif` all go through the vendored
/// libheif (HEVC and, since dav1d was vendored, AV1 — pinned by
/// `heif-decoder`'s `hevc_and_av1_decode`), and SVG through resvg, which only
/// the native target section of Cargo.toml enables (a rasteriser is a large
/// dependency for a format phones never produce). A build without the decoder
/// must answer "no" here rather than spend a chunk to be refused by the sniff.
const THUMBNAILABLE_EXTENSIONS: &[&str] = &[
	// Formats the pipeline decodes directly. `svgz` is deliberately absent:
	// the pipeline refuses gzipped SVG (no inflate), so looking would cost a
	// chunk and answer nothing.
	"bmp",
	"gif",
	"jfif",
	"jpe",
	"jpeg",
	"jpg",
	"png",
	"qoi",
	"tif",
	"tiff",
	"webp",
	#[cfg(feature = "heif-decoder")]
	"avif",
	#[cfg(feature = "heif-decoder")]
	"heic",
	#[cfg(feature = "heif-decoder")]
	"heif",
	#[cfg(feature = "heif-decoder")]
	"hif",
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	"svg",
	// RAW: TIFF/BMFF containers, reached for their embedded preview.
	"3fr",
	"arw",
	"cr2",
	"cr3",
	"dng",
	"erf",
	"iiq",
	"kdc",
	"mos",
	"mrw",
	"nef",
	"nrw",
	"orf",
	"pef",
	"raf",
	"raw",
	"rw2",
	"rwl",
	"srf",
	"srw",
	"x3f",
];

/// Whether a thumbnail is worth attempting for this file.
///
/// Magic bytes remain the authority — this only decides whether to spend the
/// bytes to look.
///
/// **Either signal admits: a known extension, or a stored mime the table
/// maps to one.** The extension rescues what the mime lost — HEIC and RAW
/// stored as octet-stream by a library that never heard of them. The mime
/// rescues what the name lost — a picker-supplied `image/jpeg` on a name whose
/// last dot-segment is no extension (`IMG_2026.06.01`), which a content
/// provider hands over without ever looking at the name. Both go through the
/// one cfg-gated table, so a mime never admits what this build cannot decode,
/// and `psd`, `dwg`, `tga` and friends stay out however their mime reads:
/// nothing here decodes them, so looking would cost a chunk and answer
/// nothing.
///
/// Filename handling, all deliberate: the extension is what follows the LAST
/// dot, matching every library that produced the stored mimes, so
/// `photo.jpg.enc` reads as `enc` and is skipped. Matching is case-insensitive
/// and tolerates surrounding whitespace. A dotfile named `.jpg` counts as a
/// JPEG — permissive, and it costs at most a chunk to be wrong.
pub fn might_be_thumbnailable(name: Option<&str>, mime: Option<&str>) -> bool {
	let extension_known = name
		.and_then(|name| name.rsplit_once('.'))
		.is_some_and(|(_, ext)| extension_is_thumbnailable(ext.trim()));
	extension_known || mime.is_some_and(mime_is_thumbnailable)
}

fn extension_is_thumbnailable(ext: &str) -> bool {
	THUMBNAILABLE_EXTENSIONS
		.iter()
		.any(|known| ext.eq_ignore_ascii_case(known))
}

/// The stored mime, read through the same cfg-gated table: `image/<sub>` is
/// `<sub>` as an extension, bar the handful of subtypes that spell it
/// differently. Anything the table does not know — `vnd.adobe.photoshop`,
/// `x-icon` — stays out, exactly as its extension would, and a mime never
/// admits what this build cannot decode.
fn mime_is_thumbnailable(mime: &str) -> bool {
	let Some(sub) = mime.strip_prefix("image/") else {
		return false;
	};
	extension_is_thumbnailable(match sub {
		"jpeg" => "jpg",
		"svg+xml" => "svg",
		"x-qoi" => "qoi",
		"tiff" => "tif",
		other => other,
	})
}

#[cfg(test)]
mod gate_tests;

// ---------------------------------------------------------------------------
// The bounded pipeline. ONE core function; everything below it is a wrapper
// that differs only in where the bytes come from and where they go.
// ---------------------------------------------------------------------------

pub use microthumb::{
	APP_PROCESS_MEM_BUDGET as APP_PROCESS_THUMBNAIL_MEM_BUDGET, ByteSource,
	DEFAULT_MEM_BUDGET as DEFAULT_THUMBNAIL_MEM_BUDGET, FileSource, MemSource, ThumbSource,
	ThumbSpec,
};

/// Default for [`ClientConfig::with_thumbnail_max_source_bytes`](crate::auth::http::ClientConfig::with_thumbnail_max_source_bytes):
/// sources bigger than this are not worth streaming end to end to make one
/// small thumbnail. Past it the pipeline is restricted to embedded previews
/// ([`ThumbSpec::allow_full_decode`] off), which cost a chunk or two whatever
/// the file size — so a 200 MB HEIC still gets its `thmb` item, and a source
/// with nothing embedded answers [`ThumbnailOutcome::OverBudget`] having read
/// almost nothing.
pub const MAX_THUMBNAIL_SOURCE_BYTES: u64 = 64 * 1024 * 1024;

/// Resident bytes a [`RemoteChunkSource`] keeps for the whole decode — callers
/// subtract this from the budget they hand [`make_thumbnail_from_source`],
/// because the pipeline's own accounting cannot see the source's memory. Its
/// two chunk slots, plus the one chunk a bulk stream (a full decode's
/// read-ahead, see `RemoteChunkSource`) parks in its hand-off channel. What
/// the stream's reader holds in flight is pool memory drawn from the client's
/// file-io budget, exactly like any download's, and is not charged here.
pub const REMOTE_SOURCE_RESIDENT_BYTES: usize = 3 * crate::consts::CHUNK_SIZE;

/// The thumbnail that was written, and how it was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThumbnailInfo {
	pub width: u32,
	pub height: u32,
	/// Cheap embedded preview vs a real decode — orders of magnitude apart in
	/// bytes fetched and CPU spent, and otherwise indistinguishable from the
	/// outside.
	pub source: ThumbSource,
}

/// How a thumbnail attempt ended, short of transport/system trouble (which
/// stays an `Err`).
///
/// Every variant here is a CACHEABLE answer about these bytes: a caller that
/// remembers "no thumbnail" per item — which both mobile platforms do — may
/// store any of them without risking a permanent wrong verdict from an offline
/// blip. They are kept apart because they mean different things: `Unsupported`
/// is final, `OverBudget` is a verdict about the spec (the same file may
/// thumbnail once its bytes are local, or under a roomier budget), and
/// `Corrupt` is about the bitstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThumbnailOutcome {
	Thumbnail(ThumbnailInfo),
	/// Not an image format this build decodes. Decided by the magic-byte
	/// sniff, not by the stored mime.
	Unsupported,
	/// A recognised image, but nothing fit: over the memory budget, over the
	/// pipeline's work ceiling, or a full decode was needed and the spec only
	/// allowed an embedded preview.
	OverBudget,
	/// A recognised format whose bitstream the decoder refused. Carries the
	/// decoder's message for logging.
	Corrupt(String),
}

/// How the bounded canvas is fitted into the requested box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbnailFit {
	/// Aspect preserved, fitted INSIDE the box — the result is usually shorter
	/// than the box on one axis.
	Contain,
	/// Cropped to fill the box exactly, which is what a fixed-size grid tile
	/// wants.
	Cover,
}

/// Encodes a webp thumbnail of `source` into `out` without ever materialising
/// a full decoded frame: microthumb streams every format it can (IDCT-scaled
/// JPEG, PNG scanlines, HEIF tiles, embedded previews) and refuses anything
/// whose honest decode peak exceeds `spec.mem_budget`
/// ([`DEFAULT_THUMBNAIL_MEM_BUDGET`] fits an iOS file-provider extension's
/// ~20 MB jetsam ceiling; [`APP_PROCESS_THUMBNAIL_MEM_BUDGET`] is for a whole
/// app process or browser tab).
///
/// Nothing is written to `out` unless the returned outcome is
/// [`ThumbnailOutcome::Thumbnail`]. The output is never upscaled past what was
/// decoded — microthumb's canvas is routinely SMALLER than the request (the
/// budget clamps it, and an embedded preview can be smaller still) and
/// blowing that back up would only mean a blurrier thumbnail in a BIGGER
/// payload.
///
/// `lossy_quality` picks the encoding: `None` is lossless WebP, `Some(q)` is
/// lossy WebP at quality `q` (0–100; anything above counts as 100). Lossy is
/// honoured even where lossless would come out smaller, as it can on a flat UI
/// graphic: the caller chose a size/quality trade, not the smallest file.
///
/// This is the single decode path in the SDK. It is synchronous by contract:
/// callers run it off their async runtime's threads and hand it a `source`
/// that knows how to fetch bytes from wherever it lives.
pub fn make_thumbnail_from_source<W>(
	source: Box<dyn ByteSource>,
	spec: &ThumbSpec,
	fit: ThumbnailFit,
	lossy_quality: Option<u8>,
	out: &mut W,
) -> Result<ThumbnailOutcome, Error>
where
	W: std::io::Write,
{
	let thumb = match microthumb::generate(source, spec) {
		Ok(microthumb::ThumbOutcome::Thumbnail(thumb)) => thumb,
		Ok(microthumb::ThumbOutcome::Unsupported) => return Ok(ThumbnailOutcome::Unsupported),
		Ok(microthumb::ThumbOutcome::OverBudget) => return Ok(ThumbnailOutcome::OverBudget),
		// Corrupt or refused bytes are a verdict about the image, not a
		// failure of the request — the caller may cache them exactly like the
		// other two. Only transport/system errors stay hard: an offline blip
		// must never stick a permanent "no thumbnail" in a platform cache.
		Err(e @ (microthumb::ThumbError::Decode(_) | microthumb::ThumbError::Geometry)) => {
			return Ok(ThumbnailOutcome::Corrupt(e.to_string()));
		}
		// A decoder that ran off the end of the bytes is describing the IMAGE,
		// not the transport, and some report it as an io error rather than a
		// decode one (the `png` crate does; `jpeg-decoder` does not). None of
		// the sources here can produce `UnexpectedEof`: a read past the end
		// answers `Ok(0)`, and a cancel or a failed fetch answers `Other`. So
		// this can only have been synthesised by a decoder
		// reading a truncated file — and left classified as transport it would
		// make a truncated PNG retry forever instead of settling.
		Err(microthumb::ThumbError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
			return Ok(ThumbnailOutcome::Corrupt(e.to_string()));
		}
		Err(microthumb::ThumbError::Io(e)) => return Err(e.into()),
	};
	let Some(rgba) =
		image::RgbaImage::from_vec(thumb.image.width, thumb.image.height, thumb.image.rgba)
	else {
		return Err(Error::custom(
			ErrorKind::ImageError,
			"thumbnail canvas has an inconsistent buffer".to_string(),
		));
	};
	let img = DynamicImage::ImageRgba8(rgba);
	let width = spec.target_width.min(img.width());
	let height = spec.target_height.min(img.height());
	let thumbnail = match fit {
		ThumbnailFit::Contain => img.resize(width, height, FilterType::CatmullRom),
		ThumbnailFit::Cover => img.resize_to_fill(width, height, FilterType::CatmullRom),
	};
	let info = ThumbnailInfo {
		width: thumbnail.width(),
		height: thumbnail.height(),
		source: thumb.source,
	};
	write_webp(thumbnail, lossy_quality, out)?;
	Ok(ThumbnailOutcome::Thumbnail(info))
}

/// Largest width or height a lossy (VP8) WebP frame header can carry: each is
/// stored in 14 bits.
const WEBP_MAX_LOSSY_DIMENSION: u32 = 16383;

/// Writes the finished thumbnail as WebP: lossless, or lossy at
/// `lossy_quality` (see [`make_thumbnail_from_source`]).
fn write_webp<W>(
	thumbnail: DynamicImage,
	lossy_quality: Option<u8>,
	out: &mut W,
) -> Result<(), Error>
where
	W: std::io::Write,
{
	let Some(quality) = lossy_quality else {
		thumbnail.write_with_encoder(WebPEncoder::new_lossless(out))?;
		return Ok(());
	};
	let (width, height) = (thumbnail.width(), thumbnail.height());
	// Checked here because zenwebp does not: a zero dimension (a `Cover` fit
	// into a 0-wide box) panics inside the encoder — on wasm that traps the
	// whole instance — and a 16384 one "encodes" into a header that masks it
	// to 14 bits, which no decoder will open. Both are refused the way the
	// lossless encoder refuses a zero dimension, as an image error.
	if !(1..=WEBP_MAX_LOSSY_DIMENSION).contains(&width)
		|| !(1..=WEBP_MAX_LOSSY_DIMENSION).contains(&height)
	{
		return Err(Error::custom(
			ErrorKind::ImageError,
			format!(
				"a {width}x{height} thumbnail is outside lossy WebP's 1..={WEBP_MAX_LOSSY_DIMENSION} range"
			),
		));
	}
	let config = zenwebp::LossyConfig::new().with_quality(f32::from(quality.min(100)));
	let rgba = thumbnail.into_rgba8();
	// An RGBA request always writes VP8X + ALPH, an alpha plane of ~100 bytes
	// even when every pixel is opaque, so opaque pixels go in as RGB and come
	// out as a plain `VP8 ` file. Real transparency keeps its alpha, which
	// zenwebp encodes losslessly by default.
	let encoded = if rgba.pixels().all(|pixel| pixel[3] == u8::MAX) {
		let rgb = DynamicImage::ImageRgba8(rgba).into_rgb8();
		zenwebp::EncodeRequest::lossy(&config, &rgb, zenwebp::PixelLayout::Rgb8, width, height)
			.encode()
	} else {
		zenwebp::EncodeRequest::lossy(&config, &rgba, zenwebp::PixelLayout::Rgba8, width, height)
			.encode()
	};
	let webp = encoded.map_err(|e| {
		Error::custom(
			ErrorKind::ImageError,
			format!("lossy webp encode failed: {e}"),
		)
	})?;
	out.write_all(&webp)?;
	Ok(())
}

// ---------------------------------------------------------------------------
// The embedded preview, handed out as it is.
// ---------------------------------------------------------------------------

pub use microthumb::{LocatedPreview, MIN_PREVIEW_LONG_SIDE};

/// Refusal ceiling on a preview's DECLARED length, checked before a single
/// payload byte is read. The largest real previews — a full-size CR2 strip, a
/// Leica or Adobe full-size DNG — run 10–25 MB, so this only ever fires on a
/// forged container length. Not a config knob: nothing legitimate needs to
/// tune it.
pub const MAX_PREVIEW_BYTES: u64 = 64 * 1024 * 1024;

/// The smallest legal EXIF APP1 carrying an orientation and nothing else:
/// marker and length, `Exif\0\0`, a TIFF header, a one-entry IFD0 and its
/// null next-IFD pointer.
pub const ORIENTATION_APP1_LEN: usize = 36;

/// A file's embedded preview, ready to be written out.
///
/// "Untouched" is the contract: the entropy-coded scan, the quantisation and
/// Huffman tables and the SOF are copied verbatim. The one edit is
/// [`app1`](Self::app1) — 36 bytes of metadata that make the bytes
/// self-describing for a viewer we do not control. Reporting the orientation
/// and leaving the rotation to the consumer would mean one implementation per
/// image stack, and an `<img>` has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddedPreview {
	pub located: LocatedPreview,
	/// Spliced at `located.exif_insert_at` when the container declares a
	/// rotation the stream does not — the Nikon/Sony SubIFD case, and CR3.
	/// `None` leaves the bytes exactly as stored: either the stream carries
	/// an EXIF of its own (Fuji, Panasonic), whose tag then wins even where
	/// the container disagrees, or nothing needs rotating.
	pub app1: Option<[u8; ORIENTATION_APP1_LEN]>,
}

/// One piece of a written preview, in order: a range of the source, or the
/// spliced APP1 between two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewSegment {
	/// `[start, end)` of the source, verbatim.
	Source {
		start: u64,
		end: u64,
	},
	App1([u8; ORIENTATION_APP1_LEN]),
}

impl EmbeddedPreview {
	/// What to emit, in order. Shared by the synchronous copy and the
	/// streaming FFI paths so they cannot disagree about the bytes.
	pub fn segments(&self) -> Vec<PreviewSegment> {
		let start = self.located.offset;
		let end = start + self.located.len;
		match (self.app1, self.located.exif_insert_at) {
			(Some(app1), Some(at)) => {
				let splice = start + u64::from(at);
				vec![
					PreviewSegment::Source { start, end: splice },
					PreviewSegment::App1(app1),
					PreviewSegment::Source { start: splice, end },
				]
			}
			_ => vec![PreviewSegment::Source { start, end }],
		}
	}

	/// Bytes a consumer receives.
	pub fn written_len(&self) -> u64 {
		self.located.len + self.app1.map_or(0, |app1| app1.len() as u64)
	}

	/// The EXIF orientation (1–8) a viewer will apply to the written bytes:
	/// the stream's own where it carries an EXIF (Fuji, Panasonic), the
	/// container's where that was spliced in, and upright where neither —
	/// the bytes then declare nothing, and nothing is what a viewer does.
	pub fn viewer_orientation(&self) -> u8 {
		match (self.app1, self.located.stream_orientation) {
			(Some(_), _) => self.located.orientation,
			(None, Some(own)) => own,
			(None, None) => 1,
		}
	}
}

/// An EXIF APP1 declaring nothing but `orientation`.
fn orientation_app1(orientation: u8) -> [u8; ORIENTATION_APP1_LEN] {
	let mut app1 = [0u8; ORIENTATION_APP1_LEN];
	// The marker, a length that counts itself and the payload, `Exif\0\0`.
	app1[..10].copy_from_slice(&[0xFF, 0xE1, 0x00, 0x22, b'E', b'x', b'i', b'f', 0, 0]);
	// A little-endian TIFF header whose IFD0 sits right behind it ...
	app1[10..18].copy_from_slice(&[b'I', b'I', 0x2A, 0x00, 0x08, 0x00, 0x00, 0x00]);
	// ... holding one SHORT entry, Orientation, with the value inline ...
	app1[18..20].copy_from_slice(&1u16.to_le_bytes());
	app1[20..22].copy_from_slice(&0x0112u16.to_le_bytes());
	app1[22..24].copy_from_slice(&3u16.to_le_bytes());
	app1[24..28].copy_from_slice(&1u32.to_le_bytes());
	app1[28..30].copy_from_slice(&u16::from(orientation).to_le_bytes());
	// ... the value's pad bytes and the next-IFD pointer both zero.
	app1
}

/// Locates a file's embedded preview and decides what to write.
///
/// Two to four bounded reads on a real file whatever its size, and on a forged
/// container only what the parsers' own caps allow, each read at most a chunk
/// or two; nothing is decoded — see [`microthumb::locate_preview`]. `None` is one answer for
/// every no-preview reason (not a container this build walks, nothing
/// embedded at [`MIN_PREVIEW_LONG_SIDE`], a declared length past
/// [`MAX_PREVIEW_BYTES`]): unlike [`ThumbnailOutcome`], whose arms exist
/// because platform caches store them differently, every one of these leads
/// to the identical fallback, the thumbnail. Only a source read fails.
pub fn locate_embedded_preview(src: &mut dyn ByteSource) -> Result<Option<EmbeddedPreview>, Error> {
	let located = match microthumb::locate_preview(src) {
		Ok(located) => located,
		Err(microthumb::ThumbError::Io(e)) => return Err(e.into()),
		Err(e) => return Err(Error::custom(ErrorKind::ImageError, e.to_string())),
	};
	Ok(located
		.filter(|located| located.len <= MAX_PREVIEW_BYTES)
		.map(|located| EmbeddedPreview {
			app1: match (located.orientation, located.exif_insert_at) {
				(2..=8, Some(_)) => Some(orientation_app1(located.orientation)),
				_ => None,
			},
			located,
		}))
}

/// Copies a located preview into `out`, splicing the APP1 where there is
/// one. Never holds more than one copy buffer; a source that runs out inside
/// the range it declared is an error, never a short file.
///
/// Synchronous like the locate, for callers whose bytes are local. The FFI
/// wrappers stream the same [`segments`](EmbeddedPreview::segments) over the
/// network with the async range reader instead of blocking on chunks.
pub fn write_embedded_preview<W: std::io::Write>(
	src: &mut dyn ByteSource,
	preview: &EmbeddedPreview,
	out: &mut W,
) -> Result<u64, Error> {
	let mut written = 0u64;
	let mut buf = vec![0u8; 64 * 1024];
	for segment in preview.segments() {
		match segment {
			PreviewSegment::Source { start, end } => {
				let mut at = start;
				while at < end {
					let want = usize::try_from(end - at).map_or(buf.len(), |n| n.min(buf.len()));
					let n = src.read_at(at, &mut buf[..want])?;
					if n == 0 {
						return Err(Error::custom(
							ErrorKind::ImageError,
							format!("embedded preview ends at {at}, {} bytes short", end - at),
						));
					}
					out.write_all(&buf[..n])?;
					at += n as u64;
					written += n as u64;
				}
			}
			PreviewSegment::App1(app1) => {
				out.write_all(&app1)?;
				written += app1.len() as u64;
			}
		}
	}
	Ok(written)
}

// Everything from here on needs a way to fetch remote bytes and a thread it may
// block. The `service-worker` profile has neither — it is built WITHOUT atomics,
// where `std`'s parker is a silent no-op rather than a real wait — so none of it
// is compiled there.
#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
mod remote_chunks {
	use super::{ByteSource, Error};
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	use super::{
		EmbeddedPreview, ErrorKind, PreviewSegment, ThumbSpec, ThumbnailFit, ThumbnailOutcome,
		locate_embedded_preview, make_thumbnail_from_source,
	};
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	use crate::auth::Client;
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	use crate::fs::file::enums::RemoteFileType;
	use crate::{
		auth::{shared_client::SharedClient, unauth::UnauthClient},
		fs::file::traits::File,
	};
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use std::time::Duration;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use wasmtimer::{std::Instant as TimerInstant, tokio::sleep_until};

	/// Reads one plaintext byte range of a remote file.
	///
	/// Shared by both bridges below: what a synchronous `read_at` awaits is the
	/// same everywhere, only HOW it gets to await it differs per target.
	///
	/// Needs no authentication, like every read: a file carries its own key
	/// whether it was listed from the drive, a shared-in folder or a public
	/// link, which is what lets an `UnauthClient` thumbnail a linked file.
	async fn fetch_range(
		client: &UnauthClient,
		file: &dyn crate::fs::file::traits::File,
		start: u64,
		end: u64,
	) -> Result<Vec<u8>, Error> {
		use futures::AsyncReadExt;

		let mut reader = crate::fs::file::read::FileReaderBuilder::new(client, file)
			.with_start(start)
			.with_end(end)
			.build();
		let mut data = Vec::with_capacity(usize::try_from(end - start).unwrap_or(0));
		reader.read_to_end(&mut data).await?;
		Ok(data)
	}

	/// The byte range of chunk `index` in a file of `len` bytes.
	pub(super) fn chunk_range(index: u64, len: u64) -> (u64, u64) {
		let start = index * crate::consts::CHUNK_SIZE_U64;
		(start, (start + crate::consts::CHUNK_SIZE_U64).min(len))
	}

	/// One chunk a [`RemoteChunkSource`] needs, and how it expects the rest to
	/// follow.
	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	pub(super) struct ChunkAsk {
		pub index: u64,
		/// The decode has committed to reading front to back from here
		/// ([`ByteSource::hint_bulk_sequential`]): serve this chunk as the
		/// head of a read-ahead stream, not as one range request.
		pub bulk: bool,
	}

	/// A read-ahead reader over the rest of the file from chunk `first`: what
	/// a bulk ask is served from. Uncapped on purpose — it reads ahead as far
	/// as the client's file-io pool lets any download: the decode has
	/// committed to every byte, so the fastest possible download is the
	/// shortest possible thumbnail, and finishing sooner is what frees the
	/// pool for everyone else.
	fn bulk_reader<'a>(
		client: &'a UnauthClient,
		file: &'a dyn File,
		first: u64,
	) -> crate::fs::file::read::FileReader<'a> {
		crate::fs::file::read::FileReaderBuilder::new(client, file)
			.with_start(first * crate::consts::CHUNK_SIZE_U64)
			.build()
	}

	/// Serves one ask on the async side of either bridge: a range request, or
	/// — for a bulk ask — the next chunk of the read-ahead reader in `stream`,
	/// positioned at `ask.index`. The reader is rebuilt whenever the ask is
	/// not the chunk it would yield next (a seek; rare, and the header chunk
	/// stays resident in the source, so a decoder restarting from the top
	/// never asks for it) and kept otherwise, its read-ahead intact.
	///
	/// A short chunk means the file ended — empty when it ended before this
	/// chunk, which the source answers as EOF like any read past the end.
	async fn serve_chunk<'a>(
		client: &'a UnauthClient,
		file: &'a dyn File,
		stream: &mut Option<(u64, crate::fs::file::read::FileReader<'a>)>,
		ask: ChunkAsk,
	) -> Result<Vec<u8>, Error> {
		use futures::AsyncReadExt;

		if !ask.bulk {
			let (start, end) = chunk_range(ask.index, file.size());
			return fetch_range(client, file, start, end).await;
		}
		if stream.as_ref().is_none_or(|(next, _)| *next != ask.index) {
			*stream = Some((ask.index, bulk_reader(client, file, ask.index)));
		}
		let (next, reader) = stream.as_mut().expect("just ensured");
		*next += 1;
		let mut chunk = Vec::with_capacity(crate::consts::CHUNK_SIZE);
		(&mut *reader)
			.take(crate::consts::CHUNK_SIZE_U64)
			.read_to_end(&mut chunk)
			.await?;
		Ok(chunk)
	}

	/// The native bulk stream: one runtime task pumping consecutive chunks
	/// from `first` to the decode thread through a one-slot channel. The slot
	/// is reserved BEFORE the next chunk is read, so no chunk ever waits
	/// outside the channel — [`REMOTE_SOURCE_RESIDENT_BYTES`] charges exactly
	/// that one slot. The receiver going away (the decode finished, was
	/// cancelled, or asked for some other chunk) ends the task at its next
	/// reserve. The file's end is handed over as an empty chunk, and only a
	/// task that dies without sending one leaves the receiver empty-handed.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	async fn pump_chunks<C, F>(
		client: Arc<C>,
		file: Arc<F>,
		first: u64,
		chunks: tokio::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
	) where
		C: SharedClient + Send + Sync + 'static,
		F: File + Send + Sync + 'static,
	{
		let mut stream = None;
		let mut index = first;
		while let Ok(slot) = chunks.reserve().await {
			let ask = ChunkAsk { index, bulk: true };
			match serve_chunk(client.get_unauth_client(), &*file, &mut stream, ask).await {
				Ok(chunk) => {
					let ended = chunk.is_empty();
					slot.send(Ok(chunk));
					if ended {
						return;
					}
				}
				Err(e) => {
					slot.send(Err(std::io::Error::other(e)));
					return;
				}
			}
			index += 1;
		}
	}

	/// [`ByteSource`] that fetches and decrypts 1 MiB chunks of a remote file on
	/// demand, keeping only the last two — so a thumbnail served by an embedded
	/// preview costs one chunk of download, not the file.
	///
	/// Two phases, switched by the pipeline's
	/// [`hint_bulk_sequential`](ByteSource::hint_bulk_sequential): until it,
	/// every miss is one range request made when the decoder stalls on it —
	/// the header and an embedded preview cost a chunk or two whatever the
	/// file size. From it on, the decode has committed to the whole file, and
	/// every miss is served from a read-ahead stream instead — as many chunks
	/// in flight as the client's file-io pool allows, like any download — so
	/// the download runs bandwidth-bound alongside the decode rather than one
	/// round trip per chunk in series with it.
	///
	/// Synchronous by contract, because the pipeline is. The bridge to the async
	/// fetch is the one genuinely per-target piece: on native a
	/// `Handle::block_on` inside `spawn_blocking`, on wasm a channel round-trip
	/// to the async runtime from a dedicated worker (see
	/// [`over_requests`](Self::over_requests)). The two-slot cache and the short
	/// reads at chunk boundaries are shared; cancellation is NOT — only the
	/// native bridge is ever handed a flag, see [`cancel`](Self::cancel).
	pub struct RemoteChunkSource {
		fetch: Box<dyn FnMut(ChunkAsk) -> std::io::Result<Vec<u8>> + Send>,
		/// Checked before every read: cancellation surfaces as an io error,
		/// which unwinds the decode through its normal error path at chunk
		/// granularity — the same points an async decoder would get to cancel
		/// at. `ErrorKind::Other`, never `Interrupted`: std's `read_exact`
		/// retries `Interrupted` forever, and every decoder but png and gif
		/// reads through it, so a cancel answered that way spun the decode at
		/// 100% CPU on a blocking thread, holding its decode permit, instead of
		/// ending it.
		///
		/// `Some` only on the native bridge. `over_requests` passes `None`, so
		/// on wasm this check is a permanent no-op: nothing there has a cancel
		/// to raise (a `wasm_bindgen` export is wrapped in `future_to_promise`,
		/// so JS holds a Promise and the task queue owns the future), and the
		/// only things that stop a running decode are its next chunk request,
		/// or the reply to one, failing once the driver is gone.
		cancel: Option<Arc<AtomicBool>>,
		len: u64,
		slots: [Option<(u64, Vec<u8>)>; 2],
		next_evict: usize,
		/// Raised by the pipeline's bulk hint; every miss from then on is a
		/// bulk ask.
		bulk: bool,
	}

	impl RemoteChunkSource {
		/// `fetch` answers one ask with that chunk's plaintext — short when the
		/// file ends inside it, empty when it ended before it.
		pub(super) fn with_fetcher(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			fetch: Box<dyn FnMut(ChunkAsk) -> std::io::Result<Vec<u8>> + Send>,
		) -> Self {
			RemoteChunkSource {
				fetch,
				cancel,
				len,
				slots: [None, None],
				next_evict: 0,
				bulk: false,
			}
		}

		/// The native bridge: a miss blocks the calling thread on the async
		/// fetch, which is legal because the pipeline runs inside
		/// `spawn_blocking`.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		pub fn new<F: File + Send + Sync + 'static>(
			client: Arc<Client>,
			file: F,
			handle: tokio::runtime::Handle,
			cancel: Option<Arc<AtomicBool>>,
		) -> Self {
			Self::with_client(client, file, handle, cancel)
		}

		/// [`new`](Self::new) for either client: the fetch only ever needs the
		/// unauthenticated half.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		pub(crate) fn with_client<C, F>(
			client: Arc<C>,
			file: F,
			handle: tokio::runtime::Handle,
			cancel: Option<Arc<AtomicBool>>,
		) -> Self
		where
			C: SharedClient + Send + Sync + 'static,
			F: File + Send + Sync + 'static,
		{
			let len = file.size();
			// `Arc`: every bulk stream is a spawned task that must own the file
			// for its lifetime, and one source may start several.
			let file = Arc::new(file);
			// The live bulk stream: the index its next chunk carries, and the
			// receiving end of `pump_chunks`.
			let mut stream: Option<(u64, tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>)> =
				None;
			Self::with_fetcher(
				len,
				cancel,
				Box::new(move |ask| {
					if !ask.bulk {
						let (start, end) = chunk_range(ask.index, len);
						return handle
							.block_on(fetch_range(client.get_unauth_client(), &*file, start, end))
							.map_err(std::io::Error::other);
					}
					// A stream yields consecutive chunks; an ask for any other
					// one replaces it, and dropping the old receiver ends the
					// old pump at its next reserve.
					if stream.as_ref().is_none_or(|(next, _)| *next != ask.index) {
						let (chunks, receiver) = tokio::sync::mpsc::channel(1);
						handle.spawn(pump_chunks(client.clone(), file.clone(), ask.index, chunks));
						stream = Some((ask.index, receiver));
					}
					let (next, receiver) = stream.as_mut().expect("just ensured");
					*next += 1;
					handle.block_on(receiver.recv()).unwrap_or_else(|| {
						Err(std::io::Error::other(
							"thumbnail chunk stream ended before the file did",
						))
					})
				}),
			)
		}

		/// The wasm bridge: a miss posts the range to the async runtime and
		/// parks this thread on the reply. `first` is the file's first chunk
		/// when the driver fetched it ahead, resident from the start.
		///
		/// Only ever driven from a [`DECODES`] worker, which is where the parking
		/// is legal; the driver side is [`thumbnail_remote_file`].
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		fn over_requests(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			first: Option<Vec<u8>>,
			requests: tokio::sync::mpsc::UnboundedSender<ChunkRequest>,
		) -> Self {
			let source = Self::with_fetcher(
				len,
				cancel,
				Box::new(move |ask| {
					// A dropped driver (the caller's future went away) closes
					// `requests`, and the decode unwinds at the same chunk
					// granularity a cancel flag would give it.
					//
					// The reply channel is per request and the driver holds
					// its only sender: a driver dropped BETWEEN taking a
					// request and answering it disconnects `recv` the same
					// way, instead of parking this worker here for good with
					// every later caller queued behind it. One allocation per
					// chunk fetch is nothing against the fetch.
					let cancelled = || std::io::Error::other("thumbnail cancelled");
					let (reply, replies) = std::sync::mpsc::channel();
					requests
						.send(ChunkRequest { ask, reply })
						.map_err(|_| cancelled())?;
					replies.recv().map_err(|_| cancelled())?
				}),
			);
			match first {
				Some(first) => source.with_first_chunk(first),
				None => source,
			}
		}

		/// Makes `first` the file's first chunk, resident from the start, so the
		/// decode never asks for it. Cut to what the declared size leaves of
		/// chunk 0, as a fetch of that range would be: a chunk that decrypts
		/// longer than the file claims never answers a read past the file's
		/// end, nor past chunk 0's into chunk 1's.
		#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
		pub(super) fn with_first_chunk(mut self, mut first: Vec<u8>) -> Self {
			let (_, end) = chunk_range(0, self.len);
			first.truncate(usize::try_from(end).unwrap_or(usize::MAX));
			self.slots[0] = Some((0, first));
			self.next_evict = 1;
			self
		}

		fn chunk(&mut self, index: u64) -> std::io::Result<&Vec<u8>> {
			// Checked on EVERY read, cache hit included. Gating this on the
			// fetch instead would mean a decode whose remaining reads all land
			// in the two resident slots — anything under ~2 MiB once warm —
			// never notices the cancel and runs to completion. Native only:
			// the wasm bridge passes no flag, so there this is a no-op and a
			// warm decode cannot be interrupted at all (see `cancel`).
			if self
				.cancel
				.as_ref()
				.is_some_and(|c| c.load(Ordering::Relaxed))
			{
				return Err(std::io::Error::other("thumbnail cancelled"));
			}
			// Two fixed slots instead of a map: header + payload locality is all
			// the demuxers need, and eviction keeps the source at ≤2 MiB.
			if let Some(slot) = self
				.slots
				.iter()
				.position(|s| s.as_ref().is_some_and(|(i, _)| *i == index))
			{
				return Ok(&self.slots[slot].as_ref().expect("just matched").1);
			}
			let data = (self.fetch)(ChunkAsk {
				index,
				bulk: self.bulk,
			})?;
			let slot = self.next_evict;
			self.next_evict = (self.next_evict + 1) % self.slots.len();
			self.slots[slot] = Some((index, data));
			Ok(&self.slots[slot].as_ref().expect("just stored").1)
		}
	}

	impl ByteSource for RemoteChunkSource {
		fn len(&self) -> u64 {
			self.len
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			if offset >= self.len {
				return Ok(0);
			}
			let index = offset / crate::consts::CHUNK_SIZE_U64;
			let within = (offset % crate::consts::CHUNK_SIZE_U64) as usize;
			let chunk = self.chunk(index)?;
			if within >= chunk.len() {
				return Ok(0);
			}
			// May be short at a chunk boundary; readers loop per the Read contract.
			let n = buf.len().min(chunk.len() - within);
			buf[..n].copy_from_slice(&chunk[within..within + n]);
			Ok(n)
		}

		fn hint_bulk_sequential(&mut self) {
			self.bulk = true;
		}
	}

	/// One chunk fetch, crossing from the decode worker to the async runtime.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	struct ChunkRequest {
		ask: ChunkAsk,
		reply: std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
	}

	/// Most decode workers a page ever runs, whatever the clients on it ask for.
	///
	/// The decode gate is per client (`thumbnailDecodeConcurrency`), the pool is
	/// per page: a job may use as many of its workers as its client's gate is
	/// wide, so a page holds no more workers than its widest gate, nor more
	/// than this. That also caps the decodes running at once on the page,
	/// however many clients it has: past it, jobs queue on a worker in turn.
	/// Each worker costs a wasm instance and its thread's stack and TLS in the
	/// shared memory, none of it ever returned, and each running decode up to
	/// its client's thumbnail memory budget, resident chunks or buffered source
	/// included: 64 MiB by default, so four at once peak at 256 MiB of the
	/// tab's 1 GiB. Four is one more than the three a thumbnail grid needs (see
	/// microthumb's `APP_PROCESS_MEM_BUDGET`).
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	const MAX_DECODE_WORKERS: usize = 4;

	/// The workers the synchronous pipeline runs on (see [`WorkerPool`](crate::blocking::WorkerPool)).
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	static DECODES: crate::blocking::WorkerPool<MAX_DECODE_WORKERS> =
		crate::blocking::WorkerPool::new();

	/// How long a wasm decode worker may go silent — no chunk request to any
	/// driver, no job finished — before the driver whose deadline expires
	/// declares it dead and retires it.
	///
	/// It measures worker-side silence ACROSS ALL DRIVERS of that worker, not
	/// the expiring caller's own progress. That distinction is the whole reason for
	/// [`WorkerSlot::note_activity`](crate::blocking::WorkerSlot::note_activity): several drivers can wait on one
	/// worker at once, and the ones queued behind another caller's decode never
	/// see a chunk reply of their own to reset on. Each driver still ARMS its
	/// deadline at submit time — deferring it to when the job starts would put a
	/// queued caller back behind a trapped worker with no deadline at all, the
	/// exact hang this exists to break — and on expiry re-reads the shared
	/// stamp: moved means the worker is demonstrably alive and busy with someone
	/// else, so the deadline is simply re-armed.
	///
	/// What is left that can spend it while the worker is perfectly healthy is
	/// one unobserved step: a single chunk fetch, or a single stretch of decode
	/// between two chunks — or after the last one, since nothing resets the
	/// deadline once the source is done — longer than the whole timeout. The decode never
	/// spends that long between reads under this budget, but the HTTP stack's
	/// read timeout is far above this one, so a single pathological chunk fetch
	/// CAN outlast a queued caller's deadline. The knob below is the answer,
	/// not another observation point — there is nothing left to observe.
	///
	/// ponytail: a calibration knob, not a proof — nothing here can tell a
	/// trapped worker from a slow one, so the ceiling is set far above any
	/// plausible single decode step on a slow phone. All it has to buy is
	/// turning an infinite silent hang into a surfaced error and a pipeline that
	/// recovers on the next call. If a real device ever reports a spurious
	/// "decode worker died", raise it; the honest upgrade is a liveness ping
	/// from inside the decode, which needs a decode loop that can be
	/// instrumented from Rust — libheif's cannot.
	/// **Status: defence-in-depth, not a load-bearing path — do not delete it as
	/// dead code, and do not trust it as battle-tested.** An audit of libheif's
	/// allocation sites found no input that reaches the trapping `__cxa_throw` in
	/// THIS build — which is not a proof that none exists. Plenty of those sites
	/// are NOT bounded by the file's byte length: `Box_stts` / `Box_ctts` /
	/// `Box_stsc::parse` (`sequences/seq_boxes.cc`) each resize to a
	/// file-declared `entry_count` BEFORE reading a single entry, capped only by
	/// `max_sequence_frames` (18'000'000), and `moov`/`trak`/`stbl` are
	/// registered unconditionally (`box.cc`). Measured through
	/// `microthumb::generate`, a 76-byte file peaks at 216 MB on the `stsc`
	/// table — the largest single pre-read allocation found, and still well under
	/// the wasm module's 1 GiB linker cap. The sites that WOULD clear that cap
	/// are the uncompressed-codec ones, which are not compiled in (see the
	/// `WITH_UNCOMPRESSED_CODEC` note in `heif-decoder/build.rs`). So the trap
	/// arm guards a libheif bump or a newly enabled codec — neither of which is
	/// live today.
	///
	/// What IS pinned by tests is the queued-caller half: the browser suite's
	/// "a queued thumbnail is not expired by another caller's decode". Worker
	/// death is NOT exercised anywhere — no test has ever trapped a decode
	/// worker. `blocking`'s native tests retire one by hand and see the next job
	/// get a fresh one, but the deadline that would retire a trapped one is
	/// unproven code on an untaken path.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	const DECODE_STALL_TIMEOUT: Duration = Duration::from_secs(60);

	/// The plaintext of `file`'s first chunk; `None` for an empty file.
	///
	/// Its memory comes out of the client's file-io budget while it downloads, like any chunk's,
	/// and goes back the moment it is in hand: the driver holds it until its decode starts, and
	/// budget held across that wait would let waiting drivers starve the decodes they wait for.
	/// Once the decode starts it is one of the source's resident slots
	/// ([`REMOTE_SOURCE_RESIDENT_BYTES`](super::REMOTE_SOURCE_RESIDENT_BYTES)).
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn first_chunk(client: &UnauthClient, file: &dyn File) -> Result<Option<Vec<u8>>, Error> {
		let chunk = crate::fs::file::read::fetch_first_chunk(client, file).await?;
		Ok(chunk.map(|chunk| chunk.into_parts().0))
	}

	/// What a decode that panicked answers its driver with, at once rather than after
	/// [`DECODE_STALL_TIMEOUT`]. Runs inside the panic hook, so it logs nothing.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn decode_panicked<R>(message: String) -> Result<R, Error> {
		Err(Error::custom(
			ErrorKind::ImageError,
			format!("thumbnail decode panicked: {message}"),
		))
	}

	/// Runs a synchronous job over a cancellable source on a blocking thread —
	/// the whole native protocol, shared by every source that has one
	/// ([`over_remote_chunks`] below, `over_local_file` in `js_impls`), which
	/// differ only in the `source` they build.
	///
	/// A dropped caller (a cancelled request, a closed view) leaves the
	/// blocking closure running detached with live reads. The guard flips the
	/// source's cancel flag so its next read fails instead.
	///
	/// Decode buffers, not downloads, are the memory hazard, and this path was
	/// bounded only by tokio's 512 blocking threads. The permit is acquired
	/// BEFORE the source and the spawn, so parked work costs a future rather
	/// than a blocked pool thread; MOVED INTO the closure so the permit
	/// outlives a cancelled caller's future the way the detached job does. A
	/// locate holds it for two to four chunk reads — worth one gate over a
	/// second one that would admit nothing more.
	#[cfg(all(
		not(all(target_family = "wasm", target_os = "unknown")),
		any(feature = "wasm-full", feature = "uniffi")
	))]
	pub(super) async fn over_cancellable_source<C, R>(
		client: Arc<C>,
		source: impl FnOnce(Arc<C>, Arc<AtomicBool>) -> Result<Box<dyn ByteSource>, Error>,
		job: impl FnOnce(Box<dyn ByteSource>) -> Result<R, Error> + Send + 'static,
	) -> Result<R, Error>
	where
		C: SharedClient + Send + Sync + 'static,
		R: Send + 'static,
	{
		struct CancelOnDrop(Option<Arc<AtomicBool>>);
		impl Drop for CancelOnDrop {
			fn drop(&mut self) {
				if let Some(flag) = self.0.take() {
					flag.store(true, Ordering::Relaxed);
				}
			}
		}
		let decode_permit = client
			.get_unauth_client()
			.thumbnails()
			.decode_permit()
			.await;
		let cancel = Arc::new(AtomicBool::new(false));
		let mut guard = CancelOnDrop(Some(cancel.clone()));
		let source = source(client, cancel)?;
		let joined = tokio::task::spawn_blocking(move || {
			let _decode_permit = decode_permit;
			job(source)
		})
		.await;
		// The job is over — nothing is left for a late drop to cancel.
		guard.0 = None;
		joined.map_err(|e| {
			Error::custom(
				ErrorKind::ImageError,
				format!("thumbnail decode task failed: {e}"),
			)
		})?
	}

	/// Runs a synchronous job over a remote file's chunks, off the async
	/// runtime's threads — a thumbnail decode, or a preview locate — without
	/// the file ever being resident in full.
	///
	/// The two arms differ only in how the synchronous work is taken off the
	/// runtime and how its source gets back on it.
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	pub(crate) async fn over_remote_chunks<C, F, R>(
		client: Arc<C>,
		file: &F,
		job: impl FnOnce(Box<dyn ByteSource>) -> Result<R, Error> + Send + 'static,
	) -> Result<R, Error>
	where
		C: SharedClient + Send + Sync + 'static,
		F: File + Clone + Send + Sync + 'static,
		R: Send + 'static,
	{
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			over_cancellable_source(
				client,
				|client, cancel| {
					Ok(Box::new(RemoteChunkSource::with_client(
						client,
						file.clone(),
						tokio::runtime::Handle::current(),
						Some(cancel),
					)))
				},
				job,
			)
			.await
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
			// Same gate as the native arm, but the permit is held HERE rather than moved into
			// the job. A trapped worker runs no destructor, so a permit living inside the job
			// would be lost for the life of the page and the gate would close for good — the
			// same wedge this deadline exists to remove, one step earlier. The accounting the
			// native arm buys by moving it (a cancelled caller must not free a slot while its
			// orphaned job still runs) costs little here: the job such a permit admits runs on
			// one of the gate's width of workers, queued behind a busy one when none is free,
			// so no more decodes run at once than the gate is wide.
			//
			// A driver that has to wait for a permit fetches the file's first chunk meanwhile:
			// every job here starts by reading the file's head, so the round trip overlaps the
			// decodes ahead of it instead of following them.
			let thumbnails = client.get_unauth_client().thumbnails();
			let (_decode_permit, first) = thumbnails
				.decode_permit_prefetching(first_chunk(client.get_unauth_client(), file))
				.await;
			let first = first.transpose()?.flatten();
			// `None`, not a flag: nothing on this target can raise one (see
			// `RemoteChunkSource::cancel`). What a dropped driver does give the
			// job is `incoming` closing, which fails its next chunk REQUEST.
			let source = RemoteChunkSource::over_requests(file.size(), None, first, requests);
			let (worker, mut done) = DECODES.submit(
				thumbnails.decode_concurrency(),
				move || job(Box::new(source)),
				decode_panicked,
			);
			let died = || {
				Error::custom(
					ErrorKind::ImageError,
					"thumbnail decode worker died".to_string(),
				)
			};
			// A panic answers `done` from the panic hook. A worker that trapped any other way
			// (libheif's stubbed `__cxa_throw`) drops nothing: `done` never resolves, `incoming`
			// never closes, and every job queued on that worker would wait behind it forever.
			// This deadline is the only thing that notices, so every wait below is under it.
			let deadline = sleep_until(TimerInstant::now() + DECODE_STALL_TIMEOUT);
			tokio::pin!(deadline);
			// The stamp as of arming. Our job may not have STARTED — a worker takes its jobs in
			// turn, and ours may be queued behind another — so what we watch for is the worker
			// serving anyone, not ourselves being served.
			let mut last_activity = worker.activity();
			// Set once the source is dropped inside the worker: no further chunk can be asked
			// for, only a result. The arm is disabled rather than left to spin on a channel
			// that now returns `None` immediately.
			let mut source_gone = false;
			// The worker's bulk asks are served from this read-ahead reader (see
			// `serve_chunk`); it lives here because the driver is the side that can await.
			let mut stream = None;
			loop {
				tokio::select! {
					biased;
					result = &mut done => return result.map_err(|_| died())?,
					request = incoming.recv(), if !source_gone => {
						let Some(request) = request else {
							// A trap can still land between the last chunk and the
							// result, so keep waiting under the deadline.
							source_gone = true;
							continue;
						};
						// The request itself is proof the worker is running, and it is proof
						// NOW rather than after a fetch that may take minutes on a bad
						// connection — which is time another driver must not count against
						// the worker.
						worker.note_activity();
						let data =
							serve_chunk(client.get_unauth_client(), file, &mut stream, request.ask)
								.await
								.map_err(std::io::Error::other);
						let _ = request.reply.send(data);
						// Handing the chunk back is the second observation of the pair: it
						// dates the worker's next silence from when the worker resumed, not
						// from when it asked. Recording both means a slow fetch and a slow
						// stretch of decode each get the full timeout instead of sharing one.
						last_activity = worker.note_activity();
						deadline.as_mut().reset(TimerInstant::now() + DECODE_STALL_TIMEOUT);
					}
					() = &mut deadline => {
						let seen = worker.activity();
						if seen != last_activity && worker.is_live() {
							// Someone else's decode moved the stamp, so the worker is alive
							// and merely busy ahead of us. Retiring it here would kill a
							// healthy generation mid-decode and cost a second wasm module.
							// Gated on our OWN generation still being live: once it is
							// retired our job is stranded in the dead worker's channel, and
							// a replacement worker's bumps would otherwise re-arm us forever.
							last_activity = seen;
							deadline.as_mut().reset(TimerInstant::now() + DECODE_STALL_TIMEOUT);
							continue;
						}
						// Nothing has heard from the worker for the whole timeout. Retire it
						// so the NEXT thumbnail spawns a fresh one instead of queueing behind
						// this one forever; a later driver's `retire` for the same generation
						// then no-ops. The permit is ours and drops with this future; the
						// job's own state (source, spec, buffers) is abandoned with the dead
						// thread's stack and cannot be reclaimed.
						worker.retire();
						return Err(died());
					}
				}
			}
		}
	}

	/// Runs a synchronous job that needs NO chunk service — its bytes are
	/// already in memory — on the same decode workers, under the same gate and
	/// the same stall deadline.
	///
	/// This is [`over_remote_chunks`]'s wasm arm minus the chunk-serving select
	/// arm: a `MemSource` job never asks for a chunk, so the only two events
	/// left are the result and the deadline. Read that loop for why each piece
	/// is here — the deadline watches worker-side silence ACROSS ALL DRIVERS of
	/// the worker, and a stamp that moved while our generation is still live
	/// means the worker is merely busy ahead of us.
	///
	/// The permit is the CALLER's, taken by reference rather than acquired
	/// here. The buffer the job decodes is as much of the page's memory as the
	/// decode is, and the caller materialises it BEFORE this is ever called, so
	/// a gate held only around the decode would bound neither. It stays in the
	/// driver rather than moving into the job for the reason the sibling gives:
	/// a trapped worker runs no destructor, so a permit inside the job would be
	/// lost for the life of the page. `workers` is the width of the caller's
	/// gate: how many decode workers its jobs may spread over.
	///
	/// With no chunk traffic to observe, nothing re-arms the deadline once the
	/// job starts: [`DECODE_STALL_TIMEOUT`] is a ceiling on the WHOLE decode
	/// here, not on one unobserved step of it. That is still far above any
	/// in-memory decode under this budget — which the caller's buffer has
	/// already been subtracted from — and a longer deadline for the chunkless
	/// case buys nothing until a real device reports otherwise.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	pub(super) async fn local_decode_job<R>(
		_decode_permit: &tokio::sync::OwnedSemaphorePermit,
		workers: usize,
		job: impl FnOnce() -> Result<R, Error> + Send + 'static,
	) -> Result<R, Error>
	where
		R: Send + 'static,
	{
		let (worker, mut done) = DECODES.submit(workers, job, decode_panicked);
		let died = || {
			Error::custom(
				ErrorKind::ImageError,
				"thumbnail decode worker died".to_string(),
			)
		};
		let deadline = sleep_until(TimerInstant::now() + DECODE_STALL_TIMEOUT);
		tokio::pin!(deadline);
		let mut last_activity = worker.activity();
		loop {
			tokio::select! {
				biased;
				result = &mut done => return result.map_err(|_| died())?,
				() = &mut deadline => {
					let seen = worker.activity();
					if seen != last_activity && worker.is_live() {
						last_activity = seen;
						deadline.as_mut().reset(TimerInstant::now() + DECODE_STALL_TIMEOUT);
						continue;
					}
					worker.retire();
					return Err(died());
				}
			}
		}
	}

	/// Thumbnails a remote file straight from its chunks: the webp bytes and
	/// the verdict, without the file ever being resident in full.
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	pub(crate) async fn thumbnail_remote_file<C, F>(
		client: Arc<C>,
		file: F,
		spec: ThumbSpec,
		lossy_quality: Option<u8>,
	) -> Result<(ThumbnailOutcome, Vec<u8>), Error>
	where
		C: SharedClient + Send + Sync + 'static,
		F: File + Clone + Send + Sync + 'static,
	{
		over_remote_chunks(client, &file, move |source| {
			let mut webp = Vec::new();
			let outcome = make_thumbnail_from_source(
				source,
				&spec,
				ThumbnailFit::Contain,
				lossy_quality,
				&mut webp,
			)?;
			Ok((outcome, webp))
		})
		.await
	}

	/// Writes a remote file's embedded preview into `writer`, as it is stored.
	///
	/// The locate runs over the chunks exactly like a thumbnail does (two to
	/// four fetches on a real file); the bytes then stream through the
	/// ordinary range reader, reading ahead within the client's file-IO
	/// memory budget like any other download, so a 25 MB full-size preview
	/// is never resident — neither here nor, on wasm, in linear memory that
	/// is never returned. The chunk the locate sniffed is fetched once more
	/// by the stream: one mebibyte, not worth plumbing around.
	///
	/// The writer is closed on every `Ok`, empty when there was nothing to
	/// write, so a consumer always sees a finished stream — and can remove the
	/// empty file it created for a `None`.
	#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
	pub(crate) async fn write_embedded_preview_remote<C, W>(
		client: Arc<C>,
		file: RemoteFileType<'static>,
		writer: &mut W,
	) -> Result<Option<EmbeddedPreview>, Error>
	where
		C: SharedClient + Send + Sync + 'static,
		W: futures::AsyncWrite + Unpin,
	{
		use futures::AsyncWriteExt;

		let located = over_remote_chunks(client.clone(), &file, |mut source| {
			locate_embedded_preview(&mut *source)
		})
		.await?;
		if let Some(preview) = &located {
			use futures::AsyncReadExt;

			// One reader over the whole range, consumed segment by segment:
			// the segments are contiguous and in order, and a reader per
			// segment would fetch and decrypt the first chunk once per
			// segment.
			let start = preview.located.offset;
			let mut reader =
				crate::fs::file::read::FileReaderBuilder::new(client.get_unauth_client(), &file)
					.with_start(start)
					.with_end(start + preview.located.len)
					.build();
			for segment in preview.segments() {
				match segment {
					PreviewSegment::Source { start, end } => {
						// A source that runs out inside the range it declared
						// — a lying uploader; the chunks still authenticate —
						// is an error, never a short file: the writer is then
						// dropped unclosed, which aborts the JS stream and
						// leaves the temp file to its guard.
						let copied =
							futures::io::copy(&mut (&mut reader).take(end - start), writer).await?;
						if copied != end - start {
							return Err(Error::custom(
								ErrorKind::ImageError,
								format!(
									"embedded preview ends at {}, {} bytes short",
									start + copied,
									end - start - copied
								),
							));
						}
					}
					PreviewSegment::App1(app1) => writer.write_all(&app1).await?,
				}
			}
		}
		writer.close().await?;
		Ok(located)
	}
}

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
pub use remote_chunks::RemoteChunkSource;

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
mod js_impls {
	use std::sync::Arc;

	use filen_macros::js_type;

	use super::{
		EmbeddedPreview, ThumbSource, ThumbnailOutcome, might_be_thumbnailable,
		remote_chunks::{thumbnail_remote_file, write_embedded_preview_remote},
	};
	// The local-source paths below (an upload stream on wasm, a path on
	// uniffi) run the pipeline themselves rather than over remote chunks.
	#[cfg(feature = "uniffi")]
	use super::{
		ByteSource, FileSource, remote_chunks::over_cancellable_source, write_embedded_preview,
	};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use super::{MemSource, PreviewSegment, remote_chunks::local_decode_job};
	#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
	use super::{ThumbnailFit, locate_embedded_preview, make_thumbnail_from_source};
	use crate::{
		Error,
		auth::{JsClient, js_impls::UnauthJsClient, shared_client::SharedClient},
		error::MetadataWasNotDecryptedError,
		fs::{
			HasName,
			file::{enums::RemoteFileType, traits::HasFileInfo},
		},
		js::AnyFile,
		runtime::do_on_commander,
	};

	// `no_ser`: `AnyFile` is an import-only shape (untagged on the way in from
	// JS) and has no `Serialize`, so this cannot derive one either.
	#[js_type(import, no_ser)]
	pub struct MakeThumbnailInMemoryParams {
		/// Any file that can be read: a drive file, a file listed from a
		/// shared-in folder or a public link (no stable id), a file shared
		/// with you directly, or a file link. Thumbnailing only reads.
		pub file: AnyFile,
		pub max_width: u32,
		pub max_height: u32,
		/// WebP quality 0–100 for a lossy thumbnail (anything above counts as
		/// 100); absent for lossless, the default. When set the thumbnail is
		/// ALWAYS lossy, even where lossless would come out smaller (a flat UI
		/// graphic can), so every thumbnail a caller asked this of is encoded
		/// the same way.
		#[cfg_attr(all(target_family = "wasm", target_os = "unknown"), serde(default))]
		#[cfg_attr(feature = "uniffi", uniffi(default = None))]
		pub lossy_quality: Option<u8>,
	}

	/// A thumbnail, and what it cost to make.
	#[js_type(export)]
	pub struct InMemoryThumbnail {
		// this is correct, ts requires the specifity
		// because of https://github.com/microsoft/typescript/issues/62546
		// not sure who to upstream this to, tsify, js_sys, or wasm-bindgen
		#[cfg(feature = "wasm-full")]
		#[tsify(type = "Uint8Array<ArrayBuffer>")]
		pub webp_data: serde_bytes::ByteBuf,
		#[cfg(feature = "uniffi")]
		pub webp_data: Vec<u8>,
		/// Dimensions of the encoded image. Never larger than the request and
		/// often smaller: the memory budget clamps the decode canvas, an
		/// embedded preview can be smaller still, and nothing is ever upscaled.
		pub width: u32,
		pub height: u32,
		/// True when this came from the image's own embedded preview (EXIF
		/// IFD1, a HEIF `thmb` item) rather than a decode of the full image —
		/// a couple of container reads against, at worst, a whole-file stream.
		/// The first question worth asking when thumbnails feel slow.
		pub from_embedded_preview: bool,
	}

	/// What came of a thumbnail request.
	///
	/// Only the transport can fail outright (that arrives as a rejected
	/// promise / a thrown error); every variant here is a settled, CACHEABLE
	/// answer about this file, and they are kept apart because they mean
	/// different things to a UI: `unsupported` is final, `overBudget` says the
	/// image was recognised but too expensive for this host to render at this
	/// size, and `corrupt` says the bytes themselves are broken.
	#[js_type(export, no_deser, tagged)]
	pub enum MakeThumbnailInMemoryResult {
		Thumbnail {
			thumbnail: InMemoryThumbnail,
		},
		/// Not an image format this build decodes — decided by the file's
		/// magic bytes, not by its stored mime.
		Unsupported,
		/// A recognised image, but no thumbnail fits: over the host's memory
		/// budget, over the pipeline's work ceiling, or too large to stream and
		/// carrying no embedded preview.
		OverBudget,
		/// A recognised format whose bitstream the decoder refused.
		Corrupt {
			message: String,
		},
	}

	impl MakeThumbnailInMemoryResult {
		/// The one place a pipeline outcome and its webp bytes become the
		/// JS-facing verdict, shared by every source the bytes can come from
		/// (a remote file, a local path, an upload stream) so none of them can
		/// disagree about what an outcome means — or about which of the two
		/// `webp_data` types this build carries.
		fn from_outcome(outcome: ThumbnailOutcome, webp_data: Vec<u8>) -> Self {
			match outcome {
				ThumbnailOutcome::Thumbnail(info) => MakeThumbnailInMemoryResult::Thumbnail {
					thumbnail: InMemoryThumbnail {
						#[cfg(feature = "wasm-full")]
						webp_data: serde_bytes::ByteBuf::from(webp_data),
						#[cfg(feature = "uniffi")]
						webp_data,
						width: info.width,
						height: info.height,
						from_embedded_preview: info.source == ThumbSource::EmbeddedPreview,
					},
				},
				ThumbnailOutcome::Unsupported => MakeThumbnailInMemoryResult::Unsupported,
				ThumbnailOutcome::OverBudget => MakeThumbnailInMemoryResult::OverBudget,
				ThumbnailOutcome::Corrupt(message) => {
					MakeThumbnailInMemoryResult::Corrupt { message }
				}
			}
		}
	}

	/// One body for both clients: nothing here needs an account, and a file
	/// from a public link is exactly what an `UnauthClient` holds.
	async fn make_thumbnail_in_memory_generic<C>(
		client: Arc<C>,
		params: MakeThumbnailInMemoryParams,
	) -> Result<MakeThumbnailInMemoryResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		do_on_commander(move || async move {
			let file = RemoteFileType::try_from(params.file)?;
			let mime = file.mime().ok_or(MetadataWasNotDecryptedError)?;
			// The extension, not the stored mime: that mime is whatever the
			// uploading client's library said years ago, and RAW is absent
			// from every such table (see `might_be_thumbnailable`) — gated on
			// it, this refused every RAW on the drive while the mobile cache
			// thumbnailed them fine.
			if !might_be_thumbnailable(file.name(), Some(mime)) {
				return Ok(MakeThumbnailInMemoryResult::Unsupported);
			}
			let spec = client.get_unauth_client().thumbnails().spec_remote(
				params.max_width,
				params.max_height,
				file.size(),
			);
			let (outcome, webp_data) =
				thumbnail_remote_file(client, file, spec, params.lossy_quality).await?;
			Ok(MakeThumbnailInMemoryResult::from_outcome(
				outcome, webp_data,
			))
		})
		.await
	}

	#[filen_macros::js_exports]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
	)]
	// Every sibling `impl JsClient` carries this and this one did not, so the
	// method compiled for uniffi (its result type even has a `Vec<u8>` field
	// written specifically for that boundary) but never reached the generated
	// Kotlin/Swift — the one consumer of the unbounded path was unable to call
	// it. Exported now that the path is bounded.
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl JsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailInMemory")
		)]
		pub async fn make_thumbnail_in_memory(
			&self,
			params: MakeThumbnailInMemoryParams,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_in_memory_generic(self.inner(), params).await
		}
	}

	#[filen_macros::js_exports]
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")
	)]
	#[cfg_attr(feature = "uniffi", uniffi::export)]
	impl UnauthJsClient {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailInMemory")
		)]
		pub async fn make_thumbnail_in_memory(
			&self,
			params: MakeThumbnailInMemoryParams,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_in_memory_generic(self.inner(), params).await
		}
	}
	/// What came of writing a file's embedded preview.
	///
	/// Only the transport can fail outright (a rejected promise / a thrown
	/// error). Both arms here are settled answers about these bytes: a file
	/// that answers `noPreview` today answers it tomorrow, so a consumer may
	/// remember it — and shows the thumbnail instead, which is the one
	/// fallback every no-preview reason leads to.
	#[js_type(export, no_deser, tagged)]
	pub enum EmbeddedPreviewResult {
		/// Written in full. The bytes are the camera's own JPEG of the shot,
		/// as stored — the only possible edit is a 36-byte EXIF orientation
		/// segment, spliced in when the container knows a rotation the stream
		/// does not declare, so the file displays upright in any viewer.
		Preview {
			/// From the JPEG's own SOF: the stored image, before any rotation
			/// its orientation implies.
			width: u32,
			height: u32,
			/// EXIF orientation (1–8) a viewer will apply to the written bytes
			/// — the stream's own where it carries an EXIF, the container's
			/// where that was spliced in, upright where the bytes declare
			/// nothing. Informational: the bytes already say it.
			orientation: u8,
			/// Bytes written. One word on purpose: a tagged variant's fields
			/// reach TypeScript under their Rust names.
			bytes: u64,
		},
		/// Nothing was written and the writer was closed empty: not a
		/// container this build walks, it embeds no JPEG a viewer could show
		/// at least 512 px across, or the length it declares is more than a
		/// JPEG of that size could hold (a forged or corrupt container).
		/// Remove the file you created for it.
		NoPreview,
	}

	impl From<Option<EmbeddedPreview>> for EmbeddedPreviewResult {
		fn from(preview: Option<EmbeddedPreview>) -> Self {
			match preview {
				Some(preview) => EmbeddedPreviewResult::Preview {
					width: preview.located.width,
					height: preview.located.height,
					orientation: preview.viewer_orientation(),
					bytes: preview.written_len(),
				},
				None => EmbeddedPreviewResult::NoPreview,
			}
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[js_type(import, wasm_all, no_ser, no_default)]
	pub struct WriteEmbeddedPreviewParams {
		pub file: AnyFile,
		/// Where the bytes go — an OPFS `FileSystemWritableFileStream` is one,
		/// which is how a web caller gets a file it can `createObjectURL`
		/// without the preview ever sitting in wasm memory. Closed on every
		/// settled answer, empty for `noPreview`; aborted on a failure so a
		/// partial preview is never saved as complete.
		#[tsify(type = "WritableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer: web_sys::WritableStream,
		// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
		#[serde(default)]
		pub managed_future: crate::js::ManagedFuture,
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn write_embedded_preview_generic<C>(
		client: Arc<C>,
		params: WriteEmbeddedPreviewParams,
	) -> Result<EmbeddedPreviewResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		// The same bridge `downloadFileToWriter` uses: frames cross to a
		// local task that owns the JS stream, so nothing here ever holds
		// more than one flush buffer of the preview.
		let (mut writer, result_receiver) = crate::js::stream_writer(
			params.writer,
			None::<fn(u64)>,
			"got error when converting to WritableStream",
		)?;

		params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let file = RemoteFileType::try_from(params.file)?;
				let preview = write_embedded_preview_remote(client, file, &mut writer).await?;
				// The close above sent the Done frame; wait for the JS
				// side to have taken every byte before answering, or a
				// caller could read the file back before it is whole.
				result_receiver.await.unwrap_or_else(|_| {
					Err(Error::custom(
						crate::ErrorKind::Cancelled,
						"preview write task cancelled",
					))
				})?;
				Ok(EmbeddedPreviewResult::from(preview))
			})?
			.await
	}

	// -----------------------------------------------------------------------
	// Bytes that are not on the drive yet: what a browser holds at upload time
	// is a `ReadableStream`, and both calls below start by draining one.
	// -----------------------------------------------------------------------

	/// Pumps a JS `ReadableStream` into a channel from the CALLING thread,
	/// which is where the stream's JS object lives — the same split
	/// `uploadFileFromReader` uses, because a `ReadableStream` cannot be moved
	/// to the commander.
	///
	/// A read error arrives in-band as the last item, so the consumer sees it
	/// where it sees the bytes. A consumer that stops reading (it settled its
	/// verdict early) drops the receiver, and the pump ends with it.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	fn spawn_stream_pump(
		reader: web_sys::ReadableStream,
	) -> Result<tokio::sync::mpsc::Receiver<Result<Vec<u8>, Error>>, Error> {
		use futures::AsyncReadExt;

		let mut reader = wasm_streams::ReadableStream::from_raw(reader)
			.try_into_async_read()
			.map_err(|(e, _)| {
				Error::custom(
					crate::ErrorKind::Conversion,
					format!("got error when converting to ReadableStream: {e:?}"),
				)
			})?;
		let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Error>>(10);
		crate::runtime::spawn_local(async move {
			let mut buffer = vec![0u8; 64 * 1024];
			loop {
				match reader.read(&mut buffer).await {
					Ok(0) => break,
					Ok(n) => {
						if sender.send(Ok(buffer[..n].to_vec())).await.is_err() {
							return;
						}
					}
					Err(e) => {
						let _ = sender
							.send(Err(Error::custom(
								crate::ErrorKind::IO,
								format!("error reading from stream: {e:?}"),
							)))
							.await;
						return;
					}
				}
			}
		});
		Ok(receiver)
	}

	/// Collects the whole stream into one buffer, or fails with
	/// `InsufficientMemory` because it would not fit under `max` — the
	/// client's `max_source_bytes`.
	///
	/// The cap is checked against `known_size` first, so a caller that declares
	/// an over-cap size is refused before the buffer is reserved, and again as
	/// the bytes arrive, because that declaration is the caller's word. Past it
	/// the receiver is dropped and the rest of the stream is never pulled.
	///
	/// A hard error, never a verdict: every arm of
	/// `MakeThumbnailInMemoryResult` and `EmbeddedPreviewResult` is documented
	/// as a settled, cacheable answer about the file, and a stream too large to
	/// buffer is exactly the file that DOES carry an embedded preview — which
	/// the remote path serves from a chunk or two once the bytes are uploaded.
	/// Settling here would cache a lie. `InsufficientMemory` rather than
	/// `ImageError`, which is what a broken bitstream answers: a caller that
	/// wants to retry over the chunks after the upload has to be able to tell
	/// the two apart from the kind alone.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn buffer_source_stream(
		chunks: &mut tokio::sync::mpsc::Receiver<Result<Vec<u8>, Error>>,
		known_size: Option<u64>,
		max: u64,
	) -> Result<Vec<u8>, Error> {
		let too_large = || {
			Error::custom(
				crate::ErrorKind::InsufficientMemory,
				format!("stream exceeds the {max}-byte thumbnail source cap"),
			)
		};
		if known_size.is_some_and(|size| size > max) {
			return Err(too_large());
		}
		// Reserved up front where the caller said how much there is: wasm linear
		// memory is never returned to the host, so a Vec doubling its way to the
		// final size costs more than the buffer itself. The cap above bounds the
		// reservation, whatever the declaration.
		let mut buf = match known_size {
			Some(size) => Vec::with_capacity(usize::try_from(size).unwrap_or(0)),
			None => Vec::new(),
		};
		while let Some(chunk) = chunks.recv().await {
			let chunk = chunk?;
			if buf.len() as u64 + chunk.len() as u64 > max {
				return Err(too_large());
			}
			buf.extend_from_slice(&chunk);
		}
		Ok(buf)
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[js_type(import, wasm_all, no_ser, no_default)]
	pub struct MakeThumbnailFromStreamParams {
		/// The bytes to thumbnail, straight from the picker — a `File`'s
		/// `stream()` is one. Read once, and only up to the client's
		/// `max_source_bytes`.
		#[tsify(type = "ReadableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub reader: web_sys::ReadableStream,
		pub max_width: u32,
		pub max_height: u32,
		/// The size, where it is known (a `File` knows it). Only an
		/// optimisation and an early refusal; the cap is enforced on the bytes
		/// regardless.
		pub known_size: Option<u64>,
		/// See [`MakeThumbnailInMemoryParams::lossy_quality`]: absent for
		/// lossless, a 0–100 quality for an always-lossy thumbnail.
		#[serde(default)]
		pub lossy_quality: Option<u8>,
		// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
		#[serde(default)]
		pub managed_future: crate::js::ManagedFuture,
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn make_thumbnail_from_stream_generic<C>(
		client: Arc<C>,
		params: MakeThumbnailFromStreamParams,
	) -> Result<MakeThumbnailInMemoryResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		let mut chunks = spawn_stream_pump(params.reader)?;
		let (max_width, max_height, known_size, lossy_quality) = (
			params.max_width,
			params.max_height,
			params.known_size,
			params.lossy_quality,
		);

		params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let thumbnails = client.get_unauth_client().thumbnails();
				// Held from BEFORE the buffering to after the decode. On wasm the
				// buffer is the bigger half of what a call costs and linear memory
				// is never returned to the host, so a gate that only covered the
				// decode would bound nothing: N concurrent callers would each park
				// `max_source_bytes` on the one commander thread.
				let decode_permit = thumbnails.decode_permit().await;
				let buf =
					buffer_source_stream(&mut chunks, known_size, thumbnails.max_source_bytes)
						.await?;
				// `spec_in_memory`, not `spec_local`: the buffer stays resident
				// under the decode, so it comes off the budget the way a remote
				// source's chunk slots do. What is left bounds the DECODE.
				let spec = thumbnails.spec_in_memory(max_width, max_height, buf.len());
				let workers = thumbnails.decode_concurrency();
				let (outcome, webp_data) = local_decode_job(&decode_permit, workers, move || {
					let mut webp = Vec::new();
					let outcome = make_thumbnail_from_source(
						Box::new(MemSource(buf)),
						&spec,
						ThumbnailFit::Contain,
						lossy_quality,
						&mut webp,
					)?;
					Ok((outcome, webp))
				})
				.await?;
				Ok(MakeThumbnailInMemoryResult::from_outcome(
					outcome, webp_data,
				))
			})?
			.await
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[js_type(import, wasm_all, no_ser, no_default)]
	pub struct WriteEmbeddedPreviewFromStreamParams {
		/// The container to look in, straight from the picker. Read once, in
		/// full: the preview is copied out of the buffer, not re-read.
		#[tsify(type = "ReadableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub reader: web_sys::ReadableStream,
		/// Where the bytes go. Closed on every settled answer, empty for
		/// `noPreview`; left unclosed on a failure so the stream aborts and a
		/// partial preview is never saved as complete.
		#[tsify(type = "WritableStream<Uint8Array>")]
		#[serde(with = "serde_wasm_bindgen::preserve")]
		pub writer: web_sys::WritableStream,
		pub known_size: Option<u64>,
		// swap to flatten when https://github.com/madonoharu/tsify/issues/68 is resolved
		#[serde(default)]
		pub managed_future: crate::js::ManagedFuture,
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn write_embedded_preview_from_stream_generic<C>(
		client: Arc<C>,
		params: WriteEmbeddedPreviewFromStreamParams,
	) -> Result<EmbeddedPreviewResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		use crate::fs::file::service_worker::MAX_BUFFER_SIZE_BEFORE_FLUSH;
		use futures::AsyncWriteExt;

		let mut chunks = spawn_stream_pump(params.reader)?;
		// The same bridge `writeEmbeddedPreview` uses: frames cross to a local
		// task that owns the JS stream, so the copy never holds more than one
		// flush buffer on top of the source buffer.
		let (mut writer, result_receiver) = crate::js::stream_writer(
			params.writer,
			None::<fn(u64)>,
			"got error when converting to WritableStream",
		)?;
		let known_size = params.known_size;

		params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let thumbnails = client.get_unauth_client().thumbnails();
				// Held from BEFORE the buffering to after the copy, for the same
				// reason the thumbnail path holds it: the buffer, not the locate,
				// is what this call costs the page.
				let decode_permit = thumbnails.decode_permit().await;
				let buf =
					buffer_source_stream(&mut chunks, known_size, thumbnails.max_source_bytes)
						.await?;
				// The locate is what the gate bounds; the buffer comes back out
				// of the source so the copy can slice it.
				let workers = thumbnails.decode_concurrency();
				let (located, buf) = local_decode_job(&decode_permit, workers, move || {
					let mut source = MemSource(buf);
					let located = locate_embedded_preview(&mut source)?;
					Ok((located, source.0))
				})
				.await?;
				if let Some(preview) = &located {
					for segment in preview.segments() {
						match segment {
							PreviewSegment::Source { start, end } => {
								// The locate answered against these very bytes, so
								// the range is inside them — checked anyway, since
								// the alternative to a message is a panic.
								let slice = usize::try_from(start)
									.ok()
									.zip(usize::try_from(end).ok())
									.and_then(|(start, end)| buf.get(start..end))
									.ok_or_else(|| {
										Error::custom(
											crate::ErrorKind::ImageError,
											format!(
												"embedded preview ends at {end}, past the {} bytes buffered",
												buf.len()
											),
										)
									})?;
								// One flush buffer at a time. `StreamWriter` never
								// splits what it is handed — it copies the whole
								// slice and ships it as one frame, which the
								// receiving task copies again — so a 25 MB
								// full-size preview handed over in one call would
								// cost two more copies of itself. The remote
								// sibling gets this from its reader; here the
								// chunking is the reader.
								for part in slice.chunks(MAX_BUFFER_SIZE_BEFORE_FLUSH) {
									writer.write_all(part).await?;
								}
							}
							PreviewSegment::App1(app1) => writer.write_all(&app1).await?,
						}
					}
				}
				writer.close().await?;
				// The close sent the Done frame; wait for the JS side to have
				// taken every byte before answering, or a caller could read the
				// file back before it is whole.
				result_receiver.await.unwrap_or_else(|_| {
					Err(Error::custom(
						crate::ErrorKind::Cancelled,
						"preview write task cancelled",
					))
				})?;
				Ok(EmbeddedPreviewResult::from(located))
			})?
			.await
	}

	#[filen_macros::js_exports]
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
	impl JsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot or a Fujifilm HIF — into `writer`, as stored. See
		/// [`EmbeddedPreviewResult`].
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreview")]
		pub async fn write_embedded_preview(
			&self,
			params: WriteEmbeddedPreviewParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_generic(self.inner(), params).await
		}

		/// Thumbnails bytes that are not on the drive yet — what a picker hands
		/// over at upload time. See [`MakeThumbnailInMemoryResult`].
		///
		/// The stream is buffered whole, up to the client's `max_source_bytes`
		/// — past that the call fails with `InsufficientMemory` rather than
		/// settling a verdict, since the uploaded file's embedded preview still
		/// costs `makeThumbnailInMemory` a chunk or two — and decoded on one of
		/// the page's decode workers, like every other decode on the page.
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailFromStream")]
		pub async fn make_thumbnail_from_stream(
			&self,
			params: MakeThumbnailFromStreamParams,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_from_stream_generic(self.inner(), params).await
		}

		/// Writes the embedded preview of bytes that are not on the drive yet.
		/// See [`EmbeddedPreviewResult`].
		///
		/// The stream is buffered whole, up to the client's `max_source_bytes`
		/// — past that the call fails with `InsufficientMemory` rather than
		/// answering `noPreview`, since the uploaded file's preview still costs
		/// `writeEmbeddedPreview` a chunk or two.
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreviewFromStream")]
		pub async fn write_embedded_preview_from_stream(
			&self,
			params: WriteEmbeddedPreviewFromStreamParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_from_stream_generic(self.inner(), params).await
		}
	}

	#[filen_macros::js_exports]
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")]
	impl UnauthJsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot or a Fujifilm HIF — into `writer`, as stored. See
		/// [`EmbeddedPreviewResult`].
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreview")]
		pub async fn write_embedded_preview(
			&self,
			params: WriteEmbeddedPreviewParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_generic(self.inner(), params).await
		}

		/// Thumbnails bytes that are not on the drive yet — what a picker hands
		/// over at upload time. See [`MakeThumbnailInMemoryResult`].
		///
		/// The stream is buffered whole, up to the client's `max_source_bytes`
		/// — past that the call fails with `InsufficientMemory` rather than
		/// settling a verdict, since the uploaded file's embedded preview still
		/// costs `makeThumbnailInMemory` a chunk or two — and decoded on one of
		/// the page's decode workers, like every other decode on the page.
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "makeThumbnailFromStream")]
		pub async fn make_thumbnail_from_stream(
			&self,
			params: MakeThumbnailFromStreamParams,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_from_stream_generic(self.inner(), params).await
		}

		/// Writes the embedded preview of bytes that are not on the drive yet.
		/// See [`EmbeddedPreviewResult`].
		///
		/// The stream is buffered whole, up to the client's `max_source_bytes`
		/// — past that the call fails with `InsufficientMemory` rather than
		/// answering `noPreview`, since the uploaded file's preview still costs
		/// `writeEmbeddedPreview` a chunk or two.
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreviewFromStream")]
		pub async fn write_embedded_preview_from_stream(
			&self,
			params: WriteEmbeddedPreviewFromStreamParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_from_stream_generic(self.inner(), params).await
		}
	}

	// -----------------------------------------------------------------------
	// Bytes that are not on the drive yet, on mobile: a path the picker or the
	// camera roll handed over.
	// -----------------------------------------------------------------------

	/// Runs a synchronous job over a local file, off the async runtime's
	/// threads — the local-source sibling of
	/// [`over_remote_chunks`](super::remote_chunks::over_remote_chunks).
	///
	/// The gate and the cancel protocol are that sibling's, shared verbatim
	/// through [`over_cancellable_source`]; only the source differs. A
	/// [`FileSource`] honours the same cancel-before-every-read contract a
	/// remote one does, so a caller that goes away leaves the detached job
	/// dying at its next read rather than decoding to completion.
	#[cfg(feature = "uniffi")]
	async fn over_local_file<C, R>(
		client: Arc<C>,
		path: String,
		job: impl FnOnce(Box<dyn ByteSource>) -> Result<R, Error> + Send + 'static,
	) -> Result<R, Error>
	where
		C: SharedClient + Send + Sync + 'static,
		R: Send + 'static,
	{
		let file = tokio::fs::File::open(path).await?.into_std().await;
		over_cancellable_source(
			client,
			|_client, cancel| Ok(Box::new(FileSource::with_cancel(file, Some(cancel))?)),
			job,
		)
		.await
	}

	#[cfg(feature = "uniffi")]
	async fn make_thumbnail_from_path_generic<C>(
		client: Arc<C>,
		file_path: String,
		max_width: u32,
		max_height: u32,
		managed_future: crate::js::ManagedFuture,
		lossy_quality: Option<u8>,
	) -> Result<MakeThumbnailInMemoryResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		managed_future
			.into_js_managed_commander_future(move || async move {
				// No `might_be_thumbnailable` gate here, unlike the remote path:
				// that gate exists to avoid spending network bytes on a name that
				// says the file is not an image. Sniffing a local file is a read
				// of its first few bytes, and the sniff is the authority anyway —
				// so a picker-supplied name with no extension is thumbnailed
				// rather than refused unseen.
				//
				// `spec_local`: the bytes are on disk, nothing to protect.
				let spec = client
					.get_unauth_client()
					.thumbnails()
					.spec_local(max_width, max_height);
				let (outcome, webp_data) = over_local_file(client, file_path, move |source| {
					let mut webp = Vec::new();
					let outcome = make_thumbnail_from_source(
						source,
						&spec,
						ThumbnailFit::Contain,
						lossy_quality,
						&mut webp,
					)?;
					Ok((outcome, webp))
				})
				.await?;
				Ok(MakeThumbnailInMemoryResult::from_outcome(
					outcome, webp_data,
				))
			})
			.await
	}

	/// Opens a fresh temp file beside `path`, guarded, for a preview that is
	/// renamed into place only once it is whole — so nothing a viewer might
	/// cache ever sits at the destination half-finished, and a failure leaves
	/// no file at all.
	#[cfg(feature = "uniffi")]
	async fn create_tmp_beside(
		path: &std::path::Path,
	) -> Result<(crate::io::client_impl::TmpFileGuard, tokio::fs::File), Error> {
		let file_name = path
			.file_name()
			.map(|name| name.to_string_lossy())
			.ok_or_else(|| {
				Error::custom(
					crate::ErrorKind::IO,
					"preview path has no file name".to_string(),
				)
			})?;
		let tmp_name = format!("{file_name}.{}.tmp", uuid::Uuid::new_v4());
		Ok(crate::io::client_impl::TmpFileGuard::create_beside(path, &tmp_name).await?)
	}

	#[cfg(feature = "uniffi")]
	async fn write_embedded_preview_from_path_generic<C>(
		client: Arc<C>,
		source_path: String,
		preview_path: String,
		managed_future: crate::js::ManagedFuture,
	) -> Result<EmbeddedPreviewResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		managed_future
			.into_js_managed_commander_future(move || async move {
				let path = std::path::PathBuf::from(preview_path);
				let (tmp_guard, tmp_file) = create_tmp_beside(&path).await?;
				let mut tmp_file = tmp_file.into_std().await;
				// One blocking job for both halves. The locate is what the decode
				// gate is for — the copy that follows rides along on the same
				// permit and the same open file, and a second acquisition between
				// them would admit no more work than this one does.
				let preview = over_local_file(client, source_path, move |mut source| {
					let Some(preview) = locate_embedded_preview(&mut *source)? else {
						return Ok(None);
					};
					write_embedded_preview(&mut *source, &preview, &mut tmp_file)?;
					Ok(Some(preview))
				})
				.await?;
				if preview.is_some() {
					tmp_guard.commit(&path).await?;
				}
				Ok(EmbeddedPreviewResult::from(preview))
			})
			.await
	}

	#[cfg(feature = "uniffi")]
	async fn write_embedded_preview_to_path_generic<C>(
		client: Arc<C>,
		file: AnyFile,
		file_path: String,
		managed_future: crate::js::ManagedFuture,
	) -> Result<EmbeddedPreviewResult, Error>
	where
		C: SharedClient + Send + Sync + 'static,
	{
		use tokio_util::compat::TokioAsyncWriteCompatExt;

		managed_future
			.into_js_managed_commander_future(move || async move {
				let file = RemoteFileType::try_from(file)?;
				let path = std::path::PathBuf::from(file_path);
				let (tmp_guard, tmp_file) = create_tmp_beside(&path).await?;
				let mut writer = tmp_file.compat_write();
				let preview = write_embedded_preview_remote(client, file, &mut writer).await?;
				drop(writer);
				if preview.is_some() {
					tmp_guard.commit(&path).await?;
				}
				Ok(EmbeddedPreviewResult::from(preview))
			})
			.await
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	impl JsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot or a Fujifilm HIF — to `file_path`, as stored. See
		/// [`EmbeddedPreviewResult`].
		///
		/// Written beside the destination and renamed into place, so nothing
		/// a viewer might cache ever sits at the path half-finished; on
		/// `noPreview` no file is left at all.
		pub async fn write_embedded_preview_to_path(
			&self,
			file: AnyFile,
			file_path: String,
			managed_future: crate::js::ManagedFuture,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_to_path_generic(self.inner(), file, file_path, managed_future)
				.await
		}

		/// Thumbnails a file on disk that is not on the drive yet — what the
		/// picker or the camera roll hands over at upload time. See
		/// [`MakeThumbnailInMemoryResult`].
		///
		/// Decoded under the client's whole memory budget (nothing is streaming
		/// behind it) and in turn with every other decode, on the same gate.
		///
		/// `lossy_quality` is [`MakeThumbnailInMemoryParams::lossy_quality`]:
		/// absent for lossless, a 0–100 quality for an always-lossy thumbnail.
		#[uniffi::method(default(lossy_quality = None))]
		pub async fn make_thumbnail_from_path(
			&self,
			file_path: String,
			max_width: u32,
			max_height: u32,
			managed_future: crate::js::ManagedFuture,
			lossy_quality: Option<u8>,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_from_path_generic(
				self.inner(),
				file_path,
				max_width,
				max_height,
				managed_future,
				lossy_quality,
			)
			.await
		}

		/// Writes the embedded preview of a file on disk that is not on the
		/// drive yet, to `preview_path`. See [`EmbeddedPreviewResult`].
		///
		/// Written beside the destination and renamed into place, so nothing a
		/// viewer might cache ever sits at the path half-finished; on
		/// `noPreview` no file is left at all.
		pub async fn write_embedded_preview_from_path(
			&self,
			source_path: String,
			preview_path: String,
			managed_future: crate::js::ManagedFuture,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_from_path_generic(
				self.inner(),
				source_path,
				preview_path,
				managed_future,
			)
			.await
		}
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	impl UnauthJsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot or a Fujifilm HIF — to `file_path`, as stored. See
		/// [`EmbeddedPreviewResult`].
		///
		/// Written beside the destination and renamed into place, so nothing
		/// a viewer might cache ever sits at the path half-finished; on
		/// `noPreview` no file is left at all.
		pub async fn write_embedded_preview_to_path(
			&self,
			file: AnyFile,
			file_path: String,
			managed_future: crate::js::ManagedFuture,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_to_path_generic(self.inner(), file, file_path, managed_future)
				.await
		}

		/// Thumbnails a file on disk that is not on the drive yet — what the
		/// picker or the camera roll hands over at upload time. See
		/// [`MakeThumbnailInMemoryResult`].
		///
		/// Decoded under the client's whole memory budget (nothing is streaming
		/// behind it) and in turn with every other decode, on the same gate.
		///
		/// `lossy_quality` is [`MakeThumbnailInMemoryParams::lossy_quality`]:
		/// absent for lossless, a 0–100 quality for an always-lossy thumbnail.
		#[uniffi::method(default(lossy_quality = None))]
		pub async fn make_thumbnail_from_path(
			&self,
			file_path: String,
			max_width: u32,
			max_height: u32,
			managed_future: crate::js::ManagedFuture,
			lossy_quality: Option<u8>,
		) -> Result<MakeThumbnailInMemoryResult, Error> {
			make_thumbnail_from_path_generic(
				self.inner(),
				file_path,
				max_width,
				max_height,
				managed_future,
				lossy_quality,
			)
			.await
		}

		/// Writes the embedded preview of a file on disk that is not on the
		/// drive yet, to `preview_path`. See [`EmbeddedPreviewResult`].
		///
		/// Written beside the destination and renamed into place, so nothing a
		/// viewer might cache ever sits at the path half-finished; on
		/// `noPreview` no file is left at all.
		pub async fn write_embedded_preview_from_path(
			&self,
			source_path: String,
			preview_path: String,
			managed_future: crate::js::ManagedFuture,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_from_path_generic(
				self.inner(),
				source_path,
				preview_path,
				managed_future,
			)
			.await
		}
	}
}

#[cfg(test)]
mod tests;
