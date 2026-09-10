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
mod gate_tests {
	use super::might_be_thumbnailable;

	#[test]
	fn the_extension_rescues_what_the_stored_mime_lost() {
		// RAW, absent from the JS table entirely.
		assert!(might_be_thumbnailable(
			Some("DSC_0001.NEF"),
			Some("application/octet-stream")
		));
		assert!(might_be_thumbnailable(
			Some("IMG_0001.CR3"),
			Some("application/octet-stream")
		));
		// Uppercase is the norm straight off a camera.
		assert!(might_be_thumbnailable(Some("IMG_1234.JPG"), None));
		// Gzipped SVG is refused by the pipeline (no inflate), so svgz is not
		// in the table. (A stored mime naming plain SVG still admits it — a
		// chunk spent on a hand-supplied mime that lied, and the sniff's no.)
		assert!(!might_be_thumbnailable(
			Some("logo.svgz"),
			Some("application/octet-stream")
		));
	}

	/// The gate admits a format only when this build decodes it; a mime
	/// cannot say otherwise. Without the decoder the answer is "no" up front,
	/// not a chunk spent to be refused by the sniff.
	#[test]
	fn the_gate_follows_the_decoders_this_build_carries() {
		// The case the extension gate exists for: HEIC stored as octet-stream
		// by a Go client on macOS, or by any Rust client predating mime_guess
		// 2.0.5. AVIF rides the same libheif path since dav1d was vendored —
		// if either ever goes back to refusing, the decoder went away with it.
		let heif = cfg!(feature = "heif-decoder");
		for name in ["IMG_0042.heic", "IMG_0042.HIF", "photo.avif"] {
			assert_eq!(
				might_be_thumbnailable(Some(name), Some("application/octet-stream")),
				heif
			);
		}
		assert_eq!(
			might_be_thumbnailable(Some("photo.avif"), Some("image/avif")),
			heif
		);
		// SVG rasterises through resvg, which only the native target carries.
		let svg = cfg!(not(all(target_family = "wasm", target_os = "unknown")));
		assert_eq!(
			might_be_thumbnailable(Some("logo.svg"), Some("image/svg+xml")),
			svg
		);
		// The mime goes through the same gates as the extension: a rename
		// that dropped the extension does not smuggle a format past them.
		assert_eq!(
			might_be_thumbnailable(Some("logo"), Some("image/svg+xml")),
			svg
		);
		assert_eq!(
			might_be_thumbnailable(Some("scan"), Some("image/heic")),
			heif
		);
	}

	#[test]
	fn a_known_mime_admits_what_the_name_does_not() {
		// No extension at all, or a last dot-segment that is none: the
		// picker-supplied mime is the only signal, and it is honoured.
		assert!(might_be_thumbnailable(Some("scan"), Some("image/jpeg")));
		assert!(might_be_thumbnailable(None, Some("image/png")));
		assert!(might_be_thumbnailable(
			Some("IMG_2026.06.01"),
			Some("image/jpeg")
		));
		assert!(might_be_thumbnailable(
			Some("photo.final"),
			Some("image/jpeg")
		));
		// A mime the table does not know admits nothing, extension or not —
		// looking would cost a chunk to learn nothing.
		assert!(!might_be_thumbnailable(
			Some("art.psd"),
			Some("image/vnd.adobe.photoshop")
		));
		assert!(!might_be_thumbnailable(Some("icon"), Some("image/x-icon")));
	}

	#[test]
	fn everything_else_is_left_alone() {
		assert!(!might_be_thumbnailable(
			Some("archive.zip"),
			Some("application/zip")
		));
		// A wrapper suffix hides the real extension from every mime library
		// too, so the stored mime is octet-stream and this stays skipped.
		assert!(!might_be_thumbnailable(
			Some("photo.jpg.enc"),
			Some("application/octet-stream")
		));
		// No dot, nothing to go on.
		assert!(!might_be_thumbnailable(
			Some("IMG_1234"),
			Some("application/octet-stream")
		));
		assert!(!might_be_thumbnailable(None, None));
		// A dotfile is treated as its extension, deliberately.
		assert!(might_be_thumbnailable(Some(".jpg"), None));
	}
}

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

/// Resident bytes a [`RemoteChunkSource`] keeps for the whole decode (its two
/// chunk slots) — callers subtract this from the budget they hand
/// [`make_thumbnail_from_source`], because the pipeline's own accounting
/// cannot see the source's memory.
pub const REMOTE_SOURCE_RESIDENT_BYTES: usize = 2 * crate::consts::CHUNK_SIZE_U64 as usize;

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
/// This is the single decode path in the SDK. It is synchronous by contract:
/// callers run it off their async runtime's threads and hand it a `source`
/// that knows how to fetch bytes from wherever it lives.
pub fn make_thumbnail_from_source<W>(
	source: Box<dyn ByteSource>,
	spec: &ThumbSpec,
	fit: ThumbnailFit,
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
	thumbnail.write_with_encoder(WebPEncoder::new_lossless(out))?;
	Ok(ThumbnailOutcome::Thumbnail(ThumbnailInfo {
		width: thumbnail.width(),
		height: thumbnail.height(),
		source: thumb.source,
	}))
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
		let mut data = Vec::with_capacity((end - start) as usize);
		reader.read_to_end(&mut data).await?;
		Ok(data)
	}

	/// [`ByteSource`] that fetches and decrypts 1 MiB chunks of a remote file on
	/// demand, keeping only the last two — so a thumbnail served by an embedded
	/// preview costs one chunk of download, not the file.
	///
	/// Synchronous by contract, because the pipeline is. The bridge to the async
	/// fetch is the one genuinely per-target piece: on native a
	/// `Handle::block_on` inside `spawn_blocking`, on wasm a channel round-trip
	/// to the async runtime from a dedicated worker (see
	/// [`over_requests`](Self::over_requests)). The two-slot cache and the short
	/// reads at chunk boundaries are shared; cancellation is NOT — only the
	/// native bridge is ever handed a flag, see [`cancel`](Self::cancel).
	pub struct RemoteChunkSource {
		fetch: Box<dyn FnMut(u64, u64) -> std::io::Result<Vec<u8>> + Send>,
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
	}

	impl RemoteChunkSource {
		pub(super) fn with_fetcher(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			fetch: Box<dyn FnMut(u64, u64) -> std::io::Result<Vec<u8>> + Send>,
		) -> Self {
			RemoteChunkSource {
				fetch,
				cancel,
				len,
				slots: [None, None],
				next_evict: 0,
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
			Self::with_fetcher(
				len,
				cancel,
				Box::new(move |start, end| {
					handle
						.block_on(fetch_range(client.get_unauth_client(), &file, start, end))
						.map_err(std::io::Error::other)
				}),
			)
		}

		/// The wasm bridge: a miss posts the range to the async runtime and
		/// parks this thread on the reply.
		///
		/// Only ever driven from [`decode_worker`], which is where the parking
		/// is legal; the driver side is [`thumbnail_remote_file`].
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		fn over_requests(
			len: u64,
			cancel: Option<Arc<AtomicBool>>,
			requests: tokio::sync::mpsc::UnboundedSender<ChunkRequest>,
		) -> Self {
			Self::with_fetcher(
				len,
				cancel,
				Box::new(move |start, end| {
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
						.send(ChunkRequest { start, end, reply })
						.map_err(|_| cancelled())?;
					replies.recv().map_err(|_| cancelled())?
				}),
			)
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
			let start = index * crate::consts::CHUNK_SIZE_U64;
			let end = (start + crate::consts::CHUNK_SIZE_U64).min(self.len);
			let data = (self.fetch)(start, end)?;
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
	}

	/// One chunk fetch, crossing from the decode worker to the async runtime.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	struct ChunkRequest {
		start: u64,
		end: u64,
		reply: std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
	}

	/// Runs the synchronous pipeline on ONE long-lived dedicated wasm worker.
	///
	/// Which thread this is matters, in three ways:
	/// - **Not the commander.** It drives every async task in the SDK, the very
	///   chunk fetches a blocked decode waits for included.
	/// - **Not a rayon worker.** `runtime::do_cpu_intensive` IS the rayon pool,
	///   and chunk DECRYPTION runs there too — a decode parked on a rayon worker
	///   would be waiting on the pool that has to run to unblock it.
	/// - **One worker at a time, not one per call.** Each `runtime::spawn`
	///   instantiates a fresh wasm module and its thread state in the shared
	///   linear memory that is never returned to the host, and retains the
	///   worker's JS wrapper for the life of the spawning thread. A worker per
	///   thumbnail would grow exactly the memory this pipeline exists to bound.
	///   Serialising decodes is what the budget assumes anyway. A replacement is
	///   spawned only when the previous worker is retired for going silent (see
	///   `retire`).
	///
	/// Parking here is legal: `wasm-full` builds with `+atomics`, where `std`
	/// selects the futex parker, and this is a dedicated worker rather than the
	/// JS main thread.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	mod decode_worker {
		use std::sync::{
			Mutex, PoisonError,
			atomic::{AtomicU64, Ordering},
			mpsc,
		};

		type Job = Box<dyn FnOnce() + Send>;

		/// The live worker's job sender, tagged with the generation it belongs
		/// to.
		///
		/// Re-settable rather than a `OnceLock` because the worker CAN stop
		/// draining this channel without ever exiting. Two ways:
		///
		/// - a trap. The wasm build is `panic=abort` and `heif-decoder` stubs
		///   `__cxa_throw` as `unreachable`, so a malformed HEIC turns libheif's
		///   `length_error`/`bad_alloc` into a trap on this worker. A trap
		///   abandons the thread's stack without running a single destructor, so
		///   the `Receiver` is leaked rather than closed;
		/// - a park, in an earlier shape of `RemoteChunkSource::over_requests`
		///   that shared one reply channel across a decode: a driver dropped
		///   between taking a request and answering it left the worker waiting
		///   for a reply that never came. The reply channel is per request now
		///   and closes with the driver, so that shape is gone — the machinery
		///   below stays for the trap.
		///
		/// Either way `send` below keeps SUCCEEDING into a channel nothing
		/// drains, and every later thumbnail on the page queues behind a worker
		/// that is gone. [`retire`] is how the driver's stall deadline gets out
		/// of that.
		static JOBS: Mutex<Option<(u64, mpsc::Sender<Job>)>> = Mutex::new(None);
		static NEXT_GENERATION: AtomicU64 = AtomicU64::new(0);

		/// Bumped every time the worker is observed doing something: asking a
		/// driver for a chunk, taking that chunk back, or finishing a job.
		///
		/// Process-global because a driver's own `select!` is NOT a liveness
		/// signal for the worker. `thumbnail_decode_concurrency` is 2 by default
		/// and each call is its own task, so several drivers can be waiting on the
		/// one worker at once while it runs their jobs strictly in turn — and a
		/// driver queued behind someone else's decode never sees a reply of its
		/// own, however healthy that decode is. Without this it would time out on
		/// the other caller's wall clock and retire a live worker, which costs a
		/// wasm instantiation that is never given back.
		///
		/// `Relaxed` at both ends, deliberately: nothing is published through this
		/// counter — it guards no data, and the driver reads it only to choose
		/// between re-arming a timer and returning an error. All it needs is that
		/// a bump eventually becomes visible, which every ordering gives. That
		/// window is nanoseconds against a 60 s deadline; a check-read that did
		/// miss one would retire a live worker, costing an instantiation — never
		/// a wrong result, and never a missed hang.
		static ACTIVITY: AtomicU64 = AtomicU64::new(0);

		/// Records progress, returning the stamp to compare against when the
		/// caller's deadline expires.
		pub(super) fn note_activity() -> u64 {
			ACTIVITY.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
		}

		/// The current stamp. Equal to what a driver last recorded means nothing
		/// anywhere has heard from the worker since.
		pub(super) fn activity() -> u64 {
			ACTIVITY.load(Ordering::Relaxed)
		}

		/// Queues `job`, spawning a worker if there is none.
		///
		/// The returned generation names the worker that took the job; hand it
		/// to [`retire`] if it stops answering.
		pub(super) fn submit<T: Send + 'static>(
			job: impl FnOnce() -> T + Send + 'static,
		) -> (u64, tokio::sync::oneshot::Receiver<T>) {
			let (result_tx, result_rx) = tokio::sync::oneshot::channel();
			// Poisoning cannot happen under `panic=abort`; taking the guard
			// anyway keeps a hypothetical one from wedging the very pipeline
			// this module exists to un-wedge.
			let mut slot = JOBS.lock().unwrap_or_else(PoisonError::into_inner);
			let (generation, jobs) = slot.get_or_insert_with(|| {
				let (tx, rx) = mpsc::channel::<Job>();
				crate::runtime::spawn(move || {
					// `recv` parks this worker between jobs; it fails only once
					// `retire` has dropped this generation's sender, which is
					// the worker's cue to let its thread go.
					while let Ok(job) = rx.recv() {
						job();
					}
				});
				(NEXT_GENERATION.fetch_add(1, Ordering::Relaxed), tx)
			});
			// Succeeds whether or not anything is still draining the channel —
			// see `JOBS`. A dead worker is detected by the driver's deadline,
			// never by this send.
			let _ = jobs.send(Box::new(move || {
				let _ = result_tx.send(job());
				// Not about speed — the chunk traffic already covers a decode that
				// is merely slow. This is the only event a job that asks for NO
				// chunk at all produces (a zero-length source never reads), so
				// bumping here is what makes the invariant total: every job the
				// worker takes and returns from moves the stamp at least once.
				note_activity();
			}));
			(*generation, result_rx)
		}

		/// Drops `generation`'s sender, so the next [`submit`] spawns a fresh
		/// worker and the old thread exits if it ever comes back.
		///
		/// Generation-checked because several callers can be queued behind the
		/// same dead worker and each times out on its own schedule: unchecked,
		/// the late ones would retire the healthy replacement, and every respawn
		/// costs a wasm module instantiation that is never returned to the host.
		pub(super) fn retire(generation: u64) {
			let mut slot = JOBS.lock().unwrap_or_else(PoisonError::into_inner);
			if slot.as_ref().is_some_and(|(live, _)| *live == generation) {
				*slot = None;
			}
		}

		/// Whether `generation` is still the worker [`submit`] queues to.
		///
		/// A driver whose generation has been retired will never be served — its
		/// job is stranded in the dead worker's leaked channel — so activity from
		/// the REPLACEMENT worker must not keep re-arming its deadline. Without
		/// this the stamp, which is process-global, would read as movement
		/// forever and that driver would hang holding its permit.
		pub(super) fn is_live(generation: u64) -> bool {
			JOBS.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.as_ref()
				.is_some_and(|(live, _)| *live == generation)
		}
	}

	/// How long the wasm decode worker may go silent — no chunk request to any
	/// driver, no job finished — before the driver whose deadline expires
	/// declares it dead and retires it.
	///
	/// It measures worker-side silence ACROSS ALL DRIVERS, not the expiring
	/// caller's own progress. That distinction is the whole reason for
	/// [`decode_worker::note_activity`]: several drivers can wait on the one
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
	/// death and respawn are NOT exercised anywhere — no test has ever trapped
	/// this worker, so [`decode_worker::retire`] and the respawn after it are
	/// unproven code on an untaken path.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	const DECODE_STALL_TIMEOUT: Duration = Duration::from_secs(60);

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
			// A dropped caller (a cancelled request, a closed view) leaves the
			// blocking closure running detached with live network fetches. The
			// guard flips the source's cancel flag so its next read fails
			// instead.
			struct CancelOnDrop(Option<Arc<AtomicBool>>);
			impl Drop for CancelOnDrop {
				fn drop(&mut self) {
					if let Some(flag) = self.0.take() {
						flag.store(true, Ordering::Relaxed);
					}
				}
			}
			let cancel = Arc::new(AtomicBool::new(false));
			let mut guard = CancelOnDrop(Some(cancel.clone()));
			// Decode buffers, not downloads, are the memory hazard, and this path was bounded
			// only by tokio's 512 blocking threads. Acquired BEFORE the source and the spawn, so
			// parked work costs a future rather than a blocked pool thread; MOVED INTO the
			// closure so the permit outlives a cancelled caller's future the way the detached
			// job does. A locate holds it for two to four chunk reads — worth one gate over a
			// second one that would admit nothing more.
			let decode_permit = client
				.get_unauth_client()
				.thumbnails()
				.decode_permit()
				.await;
			let source = RemoteChunkSource::with_client(
				client,
				file.clone(),
				tokio::runtime::Handle::current(),
				Some(cancel),
			);
			let joined = tokio::task::spawn_blocking(move || {
				let _decode_permit = decode_permit;
				job(Box::new(source))
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
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let (requests, mut incoming) = tokio::sync::mpsc::unbounded_channel();
			// `None`, not a flag: nothing on this target can raise one (see
			// `RemoteChunkSource::cancel`). What a dropped driver does give the
			// job is `incoming` closing, which fails its next chunk REQUEST.
			let source = RemoteChunkSource::over_requests(file.size(), None, requests);
			// Same gate as the native arm, but the permit is held HERE rather than moved into
			// the job. A trapped worker runs no destructor, so a permit living inside the job
			// would be lost for the life of the page and the gate would close for good — the
			// same wedge this deadline exists to remove, one step earlier. The accounting the
			// native arm buys by moving it (a cancelled caller must not free a slot while its
			// orphaned job still runs) is worth nothing here: every job goes through the one
			// worker in turn, so a spare permit buys no extra concurrency, only an earlier
			// place in that queue.
			let _decode_permit = client
				.get_unauth_client()
				.thumbnails()
				.decode_permit()
				.await;
			let (generation, mut done) = decode_worker::submit(move || job(Box::new(source)));
			let died = || {
				Error::custom(
					ErrorKind::ImageError,
					"thumbnail decode worker died".to_string(),
				)
			};
			// A worker that trapped drops nothing: `done` never resolves, `incoming` never
			// closes, and the next `submit` would queue behind it forever. This deadline is the
			// only thing that notices, so every wait below is under it.
			let deadline = sleep_until(TimerInstant::now() + DECODE_STALL_TIMEOUT);
			tokio::pin!(deadline);
			// The stamp as of arming. Our job may not have STARTED — the worker takes jobs in
			// turn and other drivers hold the other decode permits — so what we watch for is
			// the worker serving anyone, not ourselves being served.
			let mut last_activity = decode_worker::activity();
			// Set once the source is dropped inside the worker: no further chunk can be asked
			// for, only a result. The arm is disabled rather than left to spin on a channel
			// that now returns `None` immediately.
			let mut source_gone = false;
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
						decode_worker::note_activity();
						let data = fetch_range(
							client.get_unauth_client(),
							file,
							request.start,
							request.end,
						)
						.await
						.map_err(std::io::Error::other);
						let _ = request.reply.send(data);
						// Handing the chunk back is the second observation of the pair: it
						// dates the worker's next silence from when the worker resumed, not
						// from when it asked. Recording both means a slow fetch and a slow
						// stretch of decode each get the full timeout instead of sharing one.
						last_activity = decode_worker::note_activity();
						deadline.as_mut().reset(TimerInstant::now() + DECODE_STALL_TIMEOUT);
					}
					() = &mut deadline => {
						let seen = decode_worker::activity();
						if seen != last_activity && decode_worker::is_live(generation) {
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
						decode_worker::retire(generation);
						return Err(died());
					}
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
	) -> Result<(ThumbnailOutcome, Vec<u8>), Error>
	where
		C: SharedClient + Send + Sync + 'static,
		F: File + Clone + Send + Sync + 'static,
	{
		over_remote_chunks(client, &file, move |source| {
			let mut webp = Vec::new();
			let outcome =
				make_thumbnail_from_source(source, &spec, ThumbnailFit::Contain, &mut webp)?;
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
			let (outcome, webp_data) = thumbnail_remote_file(client, file, spec).await?;
			Ok(match outcome {
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
			})
		})
		.await
	}

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
		use crate::fs::file::service_worker::{StreamWriter, WriteFrame};

		// The same bridge `downloadFileToWriter` uses: frames cross to a
		// local task that owns the JS stream, so nothing here ever holds
		// more than one flush buffer of the preview.
		let (data_sender, data_receiver) = tokio::sync::mpsc::channel::<WriteFrame>(10);
		let writer = wasm_streams::WritableStream::from_raw(params.writer)
			.try_into_async_write()
			.map_err(|(e, _)| {
				Error::custom(
					crate::ErrorKind::Conversion,
					format!("got error when converting to WritableStream: {:?}", e),
				)
			})?;
		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<Result<(), Error>>();
		crate::js::spawn_buffered_write_future(
			data_receiver,
			writer,
			None::<fn(u64)>,
			result_sender,
		);

		params
			.managed_future
			.into_js_managed_commander_future(move || async move {
				let file = RemoteFileType::try_from(params.file)?;
				let mut writer = StreamWriter::new(data_sender);
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

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")]
	impl JsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot — into `writer`, as stored. See [`EmbeddedPreviewResult`].
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreview")]
		pub async fn write_embedded_preview(
			&self,
			params: WriteEmbeddedPreviewParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_generic(self.inner(), params).await
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "UnauthClient")]
	impl UnauthJsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot — into `writer`, as stored. See [`EmbeddedPreviewResult`].
		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "writeEmbeddedPreview")]
		pub async fn write_embedded_preview(
			&self,
			params: WriteEmbeddedPreviewParams,
		) -> Result<EmbeddedPreviewResult, Error> {
			write_embedded_preview_generic(self.inner(), params).await
		}
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
				let file_name = path
					.file_name()
					.map(|name| name.to_string_lossy().into_owned())
					.ok_or_else(|| {
						Error::custom(
							crate::ErrorKind::IO,
							"preview path has no file name".to_string(),
						)
					})?;
				let tmp_path =
					path.with_file_name(format!("{file_name}.{}.tmp", uuid::Uuid::new_v4()));
				let tmp_file = tokio::fs::OpenOptions::new()
					.write(true)
					.create_new(true)
					.open(&tmp_path)
					.await?;
				let mut tmp_guard = crate::io::client_impl::TmpFileGuard::new(tmp_path.clone());
				let mut writer = tmp_file.compat_write();
				let preview = write_embedded_preview_remote(client, file, &mut writer).await?;
				drop(writer);
				if preview.is_some() {
					tokio::fs::rename(&tmp_path, &path).await?;
					tmp_guard.disarm();
				}
				Ok(EmbeddedPreviewResult::from(preview))
			})
			.await
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	impl JsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot — to `file_path`, as stored. See [`EmbeddedPreviewResult`].
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
	}

	#[cfg(feature = "uniffi")]
	#[uniffi::export]
	impl UnauthJsClient {
		/// Writes a file's embedded preview — the camera's own JPEG of a RAW
		/// shot — to `file_path`, as stored. See [`EmbeddedPreviewResult`].
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
	}
}

#[cfg(test)]
mod tests {
	use microthumb::MemSource;

	use super::{
		DEFAULT_THUMBNAIL_MEM_BUDGET, REMOTE_SOURCE_RESIDENT_BYTES, RemoteChunkSource, ThumbSpec,
		ThumbnailFit, ThumbnailOutcome, make_thumbnail_from_source,
	};
	use crate::auth::http::{ClientConfig, SharedClientState};

	fn png_bytes(width: u32, height: u32) -> Vec<u8> {
		use image::{ImageFormat, RgbImage};
		let image = RgbImage::from_fn(width, height, |x, y| {
			image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
		});
		let mut bytes = Vec::new();
		image
			.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Png)
			.unwrap();
		bytes
	}

	/// The shape every caller uses: bytes in, webp out, plus a verdict.
	fn thumbnail(
		bytes: Vec<u8>,
		target: u32,
		mem_budget: usize,
		fit: ThumbnailFit,
	) -> (ThumbnailOutcome, Vec<u8>) {
		let mut out = Vec::new();
		let outcome = make_thumbnail_from_source(
			Box::new(MemSource(bytes)),
			&ThumbSpec::new(target, target, mem_budget),
			fit,
			&mut out,
		)
		.unwrap();
		(outcome, out)
	}

	#[test]
	fn refuses_sources_above_the_memory_budget() {
		// A budget too small even for PNG's row buffers: the pipeline must
		// refuse rather than decode past its ceiling — and say that the FORMAT
		// was fine, which is what makes the verdict retryable elsewhere.
		let (outcome, out) = thumbnail(png_bytes(100, 80), 32, 1024, ThumbnailFit::Cover);
		assert_eq!(outcome, ThumbnailOutcome::OverBudget);
		assert!(out.is_empty());
	}

	#[test]
	fn makes_a_thumbnail_within_the_default_budget() {
		let (outcome, out) = thumbnail(
			png_bytes(100, 80),
			32,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn cover_crops_to_the_box_and_contain_keeps_the_aspect() {
		// The one thing the two wrappers ask for differently: a grid tile wants
		// an exact square, an in-memory preview wants the picture's shape.
		let (cover, _) = thumbnail(
			png_bytes(400, 200),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(cover) = cover else {
			panic!("expected a thumbnail");
		};
		assert_eq!((cover.width, cover.height), (64, 64));

		let (contain, _) = thumbnail(
			png_bytes(400, 200),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Contain,
		);
		let ThumbnailOutcome::Thumbnail(contain) = contain else {
			panic!("expected a thumbnail");
		};
		assert_eq!((contain.width, contain.height), (64, 32));
	}

	#[test]
	fn never_upscales_small_sources() {
		let (outcome, _) = thumbnail(
			png_bytes(16, 16),
			64,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (16, 16));
	}

	use std::sync::Arc;
	use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

	/// A [`microthumb::ByteSource`] with [`super::RemoteChunkSource`]'s
	/// observable behavior, minus the network: short reads at every chunk
	/// boundary, and a cancel flag answered with an io error — optionally
	/// flipped by the source itself after N reads, standing in for a batch
	/// cancel landing mid-decode.
	struct ChunkySource {
		data: Vec<u8>,
		chunk: usize,
		cancel: Arc<AtomicBool>,
		cancel_after_reads: Option<usize>,
		reads: AtomicUsize,
	}

	impl microthumb::ByteSource for ChunkySource {
		fn len(&self) -> u64 {
			self.data.len() as u64
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			let reads = self.reads.fetch_add(1, Ordering::Relaxed) + 1;
			if self
				.cancel_after_reads
				.is_some_and(|threshold| reads > threshold)
			{
				self.cancel.store(true, Ordering::Relaxed);
			}
			if self.cancel.load(Ordering::Relaxed) {
				return Err(std::io::Error::other("thumbnail cancelled"));
			}
			let Ok(offset) = usize::try_from(offset) else {
				return Ok(0);
			};
			if offset >= self.data.len() {
				return Ok(0);
			}
			let to_boundary = self.chunk - (offset % self.chunk);
			let n = buf.len().min(to_boundary).min(self.data.len() - offset);
			buf[..n].copy_from_slice(&self.data[offset..offset + n]);
			Ok(n)
		}
	}

	#[test]
	fn short_reads_at_chunk_boundaries_still_thumbnail() {
		// A 1 KiB chunk size forces hundreds of boundary-shortened reads
		// through the same read_at contract RemoteChunkSource answers with.
		let source = ChunkySource {
			data: png_bytes(300, 200),
			chunk: 1024,
			cancel: Arc::new(AtomicBool::new(false)),
			cancel_after_reads: None,
			reads: AtomicUsize::new(0),
		};
		let mut out = Vec::new();
		let outcome = make_thumbnail_from_source(
			Box::new(source),
			&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			&mut out,
		)
		.unwrap();
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn a_cancel_mid_decode_is_a_hard_error_not_a_cached_verdict() {
		// The flag flips after the header reads succeed, the way a batch
		// cancel lands mid-decode. A cancel must surface as Err — never as a
		// settled verdict, which the platform would cache forever.
		let source = ChunkySource {
			data: png_bytes(300, 200),
			chunk: 1024,
			cancel: Arc::new(AtomicBool::new(false)),
			cancel_after_reads: Some(3),
			reads: AtomicUsize::new(0),
		};
		let mut out = Vec::new();
		let result = make_thumbnail_from_source(
			Box::new(source),
			&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			&mut out,
		);
		assert!(result.is_err(), "expected a hard error, got {result:?}");
	}

	/// The real remote source, minus the network: one fetch serves the whole
	/// JPEG, and the cancel flag flips after the header reads. Reads are
	/// counted so a decoder that keeps retrying the refusal fails here rather
	/// than spinning the test: `jpeg-decoder` reads through std's
	/// `read_exact`, which retries `ErrorKind::Interrupted` forever.
	#[test]
	fn a_cancelled_remote_source_stops_a_jpeg_decode() {
		struct CancelAfter {
			inner: RemoteChunkSource,
			cancel: Arc<AtomicBool>,
			after: usize,
			reads: usize,
		}
		impl ByteSource for CancelAfter {
			fn len(&self) -> u64 {
				self.inner.len()
			}
			fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
				self.reads += 1;
				assert!(
					self.reads <= self.after + 8,
					"the decoder kept reading after the cancel: {} reads",
					self.reads
				);
				if self.reads > self.after {
					self.cancel.store(true, Ordering::Relaxed);
				}
				self.inner.read_at(offset, buf)
			}
		}
		let data = jpeg_bytes(600, 400);
		let len = data.len() as u64;
		let cancel = Arc::new(AtomicBool::new(false));
		let inner = RemoteChunkSource::with_fetcher(
			len,
			Some(cancel.clone()),
			Box::new(move |start, end| Ok(data[start as usize..end as usize].to_vec())),
		);
		let mut out = Vec::new();
		let result = make_thumbnail_from_source(
			Box::new(CancelAfter {
				inner,
				cancel,
				after: 3,
				reads: 0,
			}),
			&ThumbSpec::new(32, 32, DEFAULT_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			&mut out,
		);
		assert!(result.is_err(), "expected a hard error, got {result:?}");
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[test]
	fn svg_rasterises_through_the_bounded_pipeline() {
		let bytes = br#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><rect width="100" height="100" fill="red"/></svg>"#.to_vec();
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected an svg thumbnail, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());
	}

	#[test]
	fn corrupt_bytes_are_a_settled_verdict_not_an_error() {
		// Sniffable as JPEG, undecodable past the marker: a verdict the caller
		// may cache, not an error it retries forever — and distinguishable
		// from "we do not decode this format".
		let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0];
		bytes.extend_from_slice(&[0x00; 64]);
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		assert!(
			matches!(outcome, ThumbnailOutcome::Corrupt(_)),
			"got {outcome:?}"
		);
		assert!(out.is_empty());
	}

	#[test]
	fn a_truncated_image_is_a_verdict_too_even_when_its_decoder_calls_it_io() {
		// `png` reports running off the end as an io error, not a decode one.
		// That must not be mistaken for the network dropping: only the sources
		// can speak for the transport, and none of them produce UnexpectedEof.
		let mut bytes = png_bytes(300, 200);
		bytes.truncate(bytes.len() / 3);
		let (outcome, out) =
			thumbnail(bytes, 32, DEFAULT_THUMBNAIL_MEM_BUDGET, ThumbnailFit::Cover);
		assert!(
			matches!(outcome, ThumbnailOutcome::Corrupt(_)),
			"got {outcome:?}"
		);
		assert!(out.is_empty());
	}

	/// The point of the whole config: what a host configures is what the decode runs under.
	///
	/// A real `Client` needs auth material a unit test has no business minting, so this stops one
	/// layer short at `SharedClientState` — which is where the config actually lands and what
	/// `Client::thumbnails()` hands straight back.
	#[test]
	fn a_configured_budget_reaches_the_decode() {
		let bytes = png_bytes(300, 200);
		let decode_under = |config: ClientConfig| {
			let state = SharedClientState::new(config).expect("valid config");
			let mut out = Vec::new();
			let outcome = make_thumbnail_from_source(
				Box::new(MemSource(bytes.clone())),
				&state.thumbnails().spec_local(32, 32),
				ThumbnailFit::Cover,
				&mut out,
			)
			.unwrap();
			(outcome, out)
		};

		// Same image, same call path — only the configured budget differs.
		let (outcome, out) = decode_under(ClientConfig::default());
		let ThumbnailOutcome::Thumbnail(info) = outcome else {
			panic!("expected a thumbnail under the default budget, got {outcome:?}");
		};
		assert_eq!((info.width, info.height), (32, 32));
		assert!(!out.is_empty());

		let (outcome, out) = decode_under(ClientConfig::default().with_thumbnail_mem_budget(1024));
		assert_eq!(outcome, ThumbnailOutcome::OverBudget);
		assert!(out.is_empty());
	}

	/// The two corrections `spec_remote` absorbed from its (formerly duplicated) callers.
	#[test]
	fn spec_remote_pays_for_the_sources_resident_chunks_and_caps_the_stream() {
		let state = SharedClientState::new(ClientConfig::default()).expect("valid config");
		let thumbs = state.thumbnails();

		assert_eq!(
			thumbs.spec_remote(32, 32, 1).mem_budget,
			thumbs.mem_budget - REMOTE_SOURCE_RESIDENT_BYTES
		);
		assert!(
			thumbs
				.spec_remote(32, 32, thumbs.max_source_bytes)
				.allow_full_decode
		);
		assert!(
			!thumbs
				.spec_remote(32, 32, thumbs.max_source_bytes + 1)
				.allow_full_decode,
			"past the cap only the embedded-preview probe is worth the bytes"
		);

		// A budget under the subtraction saturates to 0 rather than wrapping to a huge one.
		let starved =
			SharedClientState::new(ClientConfig::default().with_thumbnail_mem_budget(1024))
				.expect("valid config");
		assert_eq!(starved.thumbnails().spec_remote(32, 32, 1).mem_budget, 0);
	}

	/// The permit is `acquire_owned`ed so it can be MOVED INTO the detached blocking closure and
	/// released when the decode really ends. This pins the other half of that contract: an
	/// ordinary drop does give the slot back, so a cancelled caller cannot leak the gate shut.
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[tokio::test(start_paused = true)]
	async fn a_dropped_decode_permit_gives_its_slot_back() {
		use std::time::Duration;

		let state =
			SharedClientState::new(ClientConfig::default().with_thumbnail_decode_concurrency(1))
				.expect("valid config");
		let thumbs = state.thumbnails();

		let held = thumbs.decode_permit().await;
		// Paused time auto-advances once every task is idle, so a gate that never opens elapses
		// this deterministically instead of sleeping.
		assert!(
			tokio::time::timeout(Duration::from_secs(30), thumbs.decode_permit())
				.await
				.is_err(),
			"the only slot is held; a second decode must wait"
		);

		drop(held);
		let _reacquired = tokio::time::timeout(Duration::from_secs(30), thumbs.decode_permit())
			.await
			.expect("dropping a decode permit must return it to the gate");
	}

	#[test]
	fn bytes_that_are_not_an_image_are_unsupported() {
		let (outcome, out) = thumbnail(
			b"this is not an image at all, not even close".to_vec(),
			32,
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			ThumbnailFit::Cover,
		);
		assert_eq!(outcome, ThumbnailOutcome::Unsupported);
		assert!(out.is_empty());
	}

	// -----------------------------------------------------------------------
	// The embedded preview, written out as it is.
	// -----------------------------------------------------------------------

	use super::{
		EmbeddedPreview, MAX_PREVIEW_BYTES, ORIENTATION_APP1_LEN, locate_embedded_preview,
		write_embedded_preview,
	};
	use microthumb::ByteSource;

	fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
		use image::{ImageFormat, RgbImage};
		let image = RgbImage::from_fn(width, height, |x, y| {
			image::Rgb([(x % 256) as u8, (y % 256) as u8, 0])
		});
		let mut bytes = Vec::new();
		image
			.write_to(&mut std::io::Cursor::new(&mut bytes), ImageFormat::Jpeg)
			.unwrap();
		bytes
	}

	/// Where a SubIFD JPEG sits in [`raw_with`]: past the two directories.
	const PREVIEW_AT: u64 = 200;

	/// A RAW-shaped container: a little-endian TIFF whose IFD0 carries
	/// `orientation` and whose SubIFD points at a JPEG of `declared` bytes at
	/// [`PREVIEW_AT`] — the shape every TIFF-family camera uses, and the one
	/// microthumb's own tests prove it walks.
	fn raw_with(jpeg: &[u8], orientation: u32, declared: u32) -> Vec<u8> {
		fn entry(tag: u16, typ: u16, count: u32, value: u32) -> Vec<u8> {
			let mut out = tag.to_le_bytes().to_vec();
			out.extend_from_slice(&typ.to_le_bytes());
			out.extend_from_slice(&count.to_le_bytes());
			out.extend_from_slice(&value.to_le_bytes());
			out
		}
		let mut file = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0112, 3, 1, orientation));
		file.extend_from_slice(&entry(0x014A, 4, 1, 60));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(60, 0);
		file.extend_from_slice(&2u16.to_le_bytes());
		file.extend_from_slice(&entry(0x0201, 4, 1, PREVIEW_AT as u32));
		file.extend_from_slice(&entry(0x0202, 4, 1, declared));
		file.extend_from_slice(&0u32.to_le_bytes());
		file.resize(PREVIEW_AT as usize, 0);
		file.extend_from_slice(jpeg);
		file
	}

	fn write(container: Vec<u8>) -> (EmbeddedPreview, Vec<u8>) {
		let mut src = MemSource(container);
		let preview = locate_embedded_preview(&mut src)
			.unwrap()
			.expect("a preview");
		let mut out = Vec::new();
		let written = write_embedded_preview(&mut src, &preview, &mut out).unwrap();
		assert_eq!(written, out.len() as u64);
		assert_eq!(written, preview.written_len());
		(preview, out)
	}

	/// The one test that fails if anything starts transcoding: what comes out
	/// is byte for byte what the camera stored.
	#[test]
	fn the_written_preview_is_the_stored_jpeg_and_nothing_else() {
		let jpeg = jpeg_bytes(640, 480);
		let (preview, out) = write(raw_with(&jpeg, 1, jpeg.len() as u32));
		assert_eq!(out, jpeg);
		assert_eq!(preview.app1, None);
		assert_eq!((preview.located.width, preview.located.height), (640, 480));
	}

	/// A rotation only the container knows is spliced in as the smallest
	/// legal EXIF segment, behind the JFIF APP0 the encoder wrote, and a
	/// decoder reads it back as the rotation it is. Everything either side of
	/// those 36 bytes is untouched.
	#[test]
	fn a_container_rotation_is_spliced_in_as_exif() {
		use image::{ImageDecoder, metadata::Orientation};

		let jpeg = jpeg_bytes(640, 480);
		let (preview, out) = write(raw_with(&jpeg, 6, jpeg.len() as u32));
		let at = usize::from(preview.located.exif_insert_at.expect("a splice point"));
		// SOI, then the 16-byte JFIF APP0 with its marker and length.
		assert_eq!(at, 2 + 2 + 16);
		let app1 = preview.app1.expect("the container knows a rotation");
		assert_eq!(out.len(), jpeg.len() + ORIENTATION_APP1_LEN);
		assert_eq!(&out[..at], &jpeg[..at]);
		assert_eq!(&out[at..at + ORIENTATION_APP1_LEN], &app1);
		assert_eq!(&out[at + ORIENTATION_APP1_LEN..], &jpeg[at..]);

		let mut decoder = image::ImageReader::new(std::io::Cursor::new(&out))
			.with_guessed_format()
			.unwrap()
			.into_decoder()
			.unwrap();
		assert_eq!(decoder.dimensions(), (640, 480));
		assert_eq!(decoder.orientation().unwrap(), Orientation::Rotate90);
	}

	/// A stream that carries an EXIF of its own — Fuji, Panasonic — is left
	/// alone whatever the container says, and the orientation reported is the
	/// one that EXIF declares, because that is what a viewer will apply.
	#[test]
	fn a_stream_with_its_own_exif_is_left_alone_and_speaks_for_itself() {
		let plain = jpeg_bytes(640, 480);
		let mut jpeg = vec![0xFF, 0xD8];
		jpeg.extend_from_slice(&super::orientation_app1(6));
		jpeg.extend_from_slice(&plain[2..]);
		// The container claims 8; the stream says 6 and the stream wins.
		let (preview, out) = write(raw_with(&jpeg, 8, jpeg.len() as u32));
		assert_eq!(out, jpeg);
		assert_eq!(preview.app1, None);
		assert_eq!(preview.located.stream_orientation, Some(6));
		assert_eq!(preview.viewer_orientation(), 6);
		// Neither side declares anything: upright, and nothing spliced.
		let (preview, out) = write(raw_with(&plain, 1, plain.len() as u32));
		assert_eq!(out, plain);
		assert_eq!(preview.viewer_orientation(), 1);
		// Only the container knows: spliced, and reported.
		let (preview, _) = write(raw_with(&plain, 3, plain.len() as u32));
		assert!(preview.app1.is_some());
		assert_eq!(preview.viewer_orientation(), 3);
	}

	/// A source that hands out a fixed prefix and claims to be much longer:
	/// every read past the prefix answers zeros, and the furthest offset asked
	/// for is recorded.
	struct Sparse {
		prefix: Vec<u8>,
		len: u64,
		furthest: Arc<AtomicUsize>,
	}

	impl ByteSource for Sparse {
		fn len(&self) -> u64 {
			self.len
		}

		fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
			if offset >= self.len {
				return Ok(0);
			}
			self.furthest
				.fetch_max(offset as usize + buf.len(), Ordering::Relaxed);
			let n = (self.len - offset).min(buf.len() as u64) as usize;
			for (i, b) in buf[..n].iter_mut().enumerate() {
				*b = self.prefix.get(offset as usize + i).copied().unwrap_or(0);
			}
			Ok(n)
		}
	}

	/// `jpeg` with its SOF0 rewritten to claim `w`x`h`; nothing here decodes
	/// it, and microthumb's own per-pixel cap scales with what the SOF says.
	fn with_sof_dims(mut jpeg: Vec<u8>, w: u16, h: u16) -> Vec<u8> {
		let at = jpeg
			.windows(2)
			.position(|m| m == [0xFF, 0xC0])
			.expect("a baseline SOF");
		jpeg[at + 5..at + 7].copy_from_slice(&h.to_be_bytes());
		jpeg[at + 7..at + 9].copy_from_slice(&w.to_be_bytes());
		jpeg
	}

	/// A forged length is refused on the container's word, before a single
	/// payload byte is read — what stops a hostile file turning one preview
	/// into a 60 MB stream. The SOF claims 48 Mpx so that microthumb's own
	/// per-pixel cap (4 B/px) clears and this ceiling is the one deciding;
	/// both sides of it are pinned.
	#[test]
	fn a_forged_length_is_refused_before_any_payload_is_read() {
		let jpeg = with_sof_dims(jpeg_bytes(640, 480), 8000, 6000);
		let sparse = |declared: u32| {
			let furthest = Arc::new(AtomicUsize::new(0));
			let src = Sparse {
				prefix: raw_with(&jpeg, 1, declared),
				len: u64::from(declared) + PREVIEW_AT + 1024,
				furthest: furthest.clone(),
			};
			(src, furthest)
		};
		let (mut src, furthest) = sparse(MAX_PREVIEW_BYTES as u32 + 1);
		assert_eq!(locate_embedded_preview(&mut src).unwrap(), None);
		assert!(
			furthest.load(Ordering::Relaxed) < 64 * 1024,
			"read {} bytes into a preview it was going to refuse",
			furthest.load(Ordering::Relaxed)
		);
		// One byte under the ceiling is served — and locating it reads no
		// payload either.
		let (mut src, furthest) = sparse(MAX_PREVIEW_BYTES as u32);
		assert!(locate_embedded_preview(&mut src).unwrap().is_some());
		assert!(furthest.load(Ordering::Relaxed) < 64 * 1024);
	}

	/// A source that runs out inside the range it declared is an error: a
	/// short file at the consumer's path would be cached as the whole preview.
	#[test]
	fn a_source_that_runs_short_is_an_error_not_a_short_file() {
		let jpeg = jpeg_bytes(640, 480);
		let declared = jpeg.len() as u32 + 4096;
		let container = raw_with(&jpeg, 1, declared);
		let real_len = container.len() as u64;
		struct Truncated(MemSource, u64);
		impl ByteSource for Truncated {
			fn len(&self) -> u64 {
				self.1
			}
			fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
				self.0.read_at(offset, buf)
			}
		}
		let mut src = Truncated(MemSource(container), real_len + 4096);
		let preview = locate_embedded_preview(&mut src)
			.unwrap()
			.expect("the declared range fits the claimed length");
		let mut out = Vec::new();
		assert!(write_embedded_preview(&mut src, &preview, &mut out).is_err());
	}
}
