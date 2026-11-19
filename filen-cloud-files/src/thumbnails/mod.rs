//! File Explorer's thumbnails for the items of the served root, answered from the drive's bytes
//! and never by reading a placeholder's data: a path is resolved to the placeholder's identity
//! through the OS seam without opening its content, a file all on disk is thumbnailed from its
//! local bytes, and any other from the remote file's chunks through the cache. Nothing here
//! hydrates a placeholder, queues a hydration, takes the dispatcher's read permits, sets the local
//! engine's "reads in flight" hints or touches a pin: the service holds the root's file system and
//! the drive, and nothing of the engine's.
//!
//! The COM layer (`os::win::com`) calls [`ThumbnailService::thumbnail_for`] from a thread of its
//! own, one request at a time as Explorer sends them (measured).

#[cfg(test)]
mod tests;

use std::{
	collections::HashMap,
	io::Cursor,
	path::Path,
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicBool, AtomicU64, Ordering},
	},
	time::Duration,
};

use filen_mobile_native_cache::{
	CacheError,
	ffi::FfiId,
	thumbnail::{FittedOutcome, FittedThumbnail},
};
use image_webp::WebPDecoder;
use tokio::{
	runtime::Handle,
	sync::Semaphore,
	time::{Instant, timeout},
};
use tracing::{debug, warn};

use crate::{
	drive::Drive,
	failure::{Answer, Failure, cache_answer},
	helper::{HOST, RootDir, below_root, blocking},
	identity::PlaceholderIdentity,
	os::{LocalEntry, LocalError, LocalFileId, LocalFs, LocalKind},
	root_path::RootPath,
};

/// The smallest and largest thumbnail side asked for that is served as asked; others are clamped
/// into this range.
const MIN_SIDE: u32 = 16;
const MAX_SIDE: u32 = 1024;
/// The sides thumbnails are made at, smallest first: a request is made at the smallest one at
/// least as large as it, and scaled down to the request, so the cache keeps three sizes of an item
/// rather than one per size Explorer asks for (it asks for 32, 96, 256 and 1024, documented).
const BUCKETS: [u32; 3] = [96, 256, MAX_SIDE];

/// Why a thumbnail is not given. Explorer shows the type's generic icon for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ThumbnailRefusal {
	/// The path is not inside the served root, or not a path this OS spells.
	#[error("the path is not inside the served root")]
	NotInRoot,
	/// The item is not a file the drive or the disk can give bytes of: a link, a placeholder whose
	/// identity does not decode, or one standing for no file.
	#[error("the item is not a file of the drive")]
	NotAPlaceholder,
	/// The item is a directory.
	#[error("the item is a directory")]
	Directory,
	/// Not an image this build decodes, or one whose bytes the decoder refused.
	#[error("the file is not an image this build thumbnails")]
	Unsupported,
	/// An image too large to thumbnail within the memory budget, or one that would need more of a
	/// remote file than is fetched for a thumbnail.
	#[error("the image is too large to thumbnail")]
	TooLarge,
	/// The server could not be reached.
	#[error("the server could not be reached")]
	Offline,
	/// No session is signed in, or the root is not served.
	#[error("no session is signed in")]
	SignedOut,
	/// The request did not finish within its deadline, waited for a decode slot that long, or was
	/// cancelled.
	#[error("the thumbnail did not finish in time")]
	Busy,
	/// Anything else: the item vanished, or a read or the cache failed. Worth asking again.
	#[error("making the thumbnail failed")]
	Failed,
}

/// Whether a thumbnail's pixels carry transparency.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbnailAlpha {
	/// Every pixel is opaque; the alpha bytes are all 255 (`WTSAT_RGB`).
	Opaque,
	/// Some pixels are not opaque; the colour bytes are premultiplied by alpha (`WTSAT_ARGB`).
	Premultiplied,
}

/// A thumbnail as File Explorer takes it: 32-bit pixels, blue, green, red and alpha bytes in that
/// order, rows top-down with no padding, its longer side no larger than the size asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thumbnail {
	width: u32,
	height: u32,
	bgra: Vec<u8>,
	alpha: ThumbnailAlpha,
}

impl Thumbnail {
	/// Its width in pixels; at least 1.
	pub fn width(&self) -> u32 {
		self.width
	}

	/// Its height in pixels; at least 1.
	pub fn height(&self) -> u32 {
		self.height
	}

	/// Its pixels: `width * height * 4` bytes, BGRA, top-down.
	pub fn bgra(&self) -> &[u8] {
		&self.bgra
	}

	/// Whether it carries transparency.
	pub fn alpha(&self) -> ThumbnailAlpha {
		self.alpha
	}
}

/// What the COM layer is handed: the service's answer for a path and the size asked for, in
/// pixels.
pub type ThumbnailHandler =
	Arc<dyn Fn(&Path, u32) -> Result<Thumbnail, ThumbnailRefusal> + Send + Sync>;

/// How the service runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThumbnailConfig {
	/// How many requests make a thumbnail at once; at least 1. Default 1: Explorer sends one at a
	/// time (measured), and the cache's decode gate bounds memory across all of them anyway.
	pub(crate) concurrency: usize,
	/// How long a request may take, its wait for a slot included, before it is answered
	/// [`ThumbnailRefusal::Busy`] and its work cancelled. Default 10 s.
	pub(crate) deadline: Duration,
	/// How long a settled "no thumbnail" for an item at a size is answered from memory, so
	/// Explorer asking again does not download the item again. Default 60 s.
	pub(crate) remember: Duration,
}

impl Default for ThumbnailConfig {
	fn default() -> Self {
		Self {
			concurrency: 1,
			deadline: Duration::from_secs(10),
			remember: Duration::from_secs(60),
		}
	}
}

/// The root the service answers for, while it is served.
struct Target<L, D> {
	root: RootDir,
	local: Arc<L>,
	drive: Arc<D>,
}

/// What a remembered verdict is about: a version of a drive file, or local bytes as they were.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Bytes {
	/// The version `version` of a drive file, as its placeholder names it.
	Remote { version: String },
	/// The local file `id` of `size` bytes last written at `modified` (FILETIME ticks).
	Local {
		id: LocalFileId,
		size: u64,
		modified: i64,
	},
}

/// Where a thumbnail's bytes come from.
enum Plan {
	/// The local file, all on disk.
	Local {
		path: RootPath,
		id: LocalFileId,
		key: Bytes,
	},
	/// The drive's file, by its `stable/<uuid>` id.
	Remote { id: FfiId, key: Bytes },
}

impl Plan {
	fn key(&self) -> &Bytes {
		match self {
			Self::Local { key, .. } | Self::Remote { key, .. } => key,
		}
	}

	fn kind(&self) -> &'static str {
		match self {
			Self::Local { .. } => "local",
			Self::Remote { .. } => "remote",
		}
	}
}

/// Answers File Explorer's thumbnail requests for the served root (see the module's docs). One per
/// helper run; the helper tells it which root it serves, and while none is, every request is
/// [`ThumbnailRefusal::SignedOut`].
pub(crate) struct ThumbnailService<L, D> {
	config: ThumbnailConfig,
	runtime: Handle,
	slots: Arc<Semaphore>,
	target: Mutex<Option<Target<L, D>>>,
	/// Settled refusals, by the bytes and the size made, with when they were reached.
	verdicts: Mutex<HashMap<(Bytes, u32), (ThumbnailRefusal, Instant)>>,
	/// Numbers the requests in the log.
	requests: AtomicU64,
}

/// `side` clamped into [`MIN_SIDE`]`..=`[`MAX_SIDE`].
fn clamp_side(side: u32) -> u32 {
	side.clamp(MIN_SIDE, MAX_SIDE)
}

/// The size a thumbnail of no more than `side` pixels is made at: the smallest bucket at least as
/// large as the clamped side.
fn bucket(side: u32) -> u32 {
	let side = clamp_side(side);
	BUCKETS
		.into_iter()
		.find(|bucket| *bucket >= side)
		.unwrap_or(MAX_SIDE)
}

/// What the drive's `error` tells Explorer, through the same mapping the provider's own requests
/// use.
fn cache_refusal(error: &CacheError) -> ThumbnailRefusal {
	match cache_answer(error) {
		Answer::Fail(Failure::NotAuthenticated) => ThumbnailRefusal::SignedOut,
		Answer::Fail(Failure::Unreachable) => ThumbnailRefusal::Offline,
		Answer::Fail(Failure::Unsupported) => ThumbnailRefusal::Unsupported,
		Answer::Cancelled => ThumbnailRefusal::Busy,
		Answer::Fail(
			Failure::NotFound
			| Failure::ContentDamaged
			| Failure::Undecryptable
			| Failure::StalePlaceholder
			| Failure::InvalidRequest
			| Failure::Transient
			| Failure::Internal,
		)
		| Answer::Damaged => ThumbnailRefusal::Failed,
	}
}

/// Decodes `thumbnail` and scales it to fit inside a `side` pixel square.
fn to_pixels(
	thumbnail: &FittedThumbnail,
	side: u32,
) -> Result<Thumbnail, image_webp::DecodingError> {
	let mut decoder = WebPDecoder::new(Cursor::new(thumbnail.webp.as_slice()))?;
	let (width, height) = decoder.dimensions();
	let channels = if decoder.has_alpha() { 4 } else { 3 };
	let size = decoder
		.output_buffer_size()
		.ok_or(image_webp::DecodingError::ImageTooLarge)?;
	let mut decoded = vec![0; size];
	decoder.read_image(&mut decoded)?;
	let (bgra, alpha) = to_bgra(&decoded, channels);
	Ok(fit_within(
		Thumbnail {
			width,
			height,
			bgra,
			alpha,
		},
		side,
	))
}

/// RGB or RGBA pixels, `channels` bytes each, as premultiplied BGRA, and whether any is not opaque.
fn to_bgra(rgba: &[u8], channels: usize) -> (Vec<u8>, ThumbnailAlpha) {
	let mut bgra = Vec::with_capacity(rgba.len() / channels * 4);
	let mut alpha = ThumbnailAlpha::Opaque;
	for pixel in rgba.chunks_exact(channels) {
		let [red, green, blue, ref rest @ ..] = *pixel else {
			continue;
		};
		let a = rest.first().copied().unwrap_or(u8::MAX);
		if a == u8::MAX {
			bgra.extend_from_slice(&[blue, green, red, a]);
		} else {
			alpha = ThumbnailAlpha::Premultiplied;
			let premultiply = |channel: u8| {
				u8::try_from((u32::from(channel) * u32::from(a) + 127) / 255).unwrap_or(u8::MAX)
			};
			bgra.extend_from_slice(&[premultiply(blue), premultiply(green), premultiply(red), a]);
		}
	}
	(bgra, alpha)
}

/// `thumbnail` scaled down, by averaging the pixels each new one covers, so that its longer side
/// is `side`; as it is when it fits already.
fn fit_within(thumbnail: Thumbnail, side: u32) -> Thumbnail {
	let longer = thumbnail.width.max(thumbnail.height);
	if longer <= side {
		return thumbnail;
	}
	let scaled = |length: u32| {
		let length =
			(u64::from(length) * u64::from(side) + u64::from(longer) / 2) / u64::from(longer);
		u32::try_from(length).unwrap_or(side).clamp(1, side)
	};
	let (width, height) = (scaled(thumbnail.width), scaled(thumbnail.height));
	// The source pixels the new pixel `n` of `new` covers along a side of `old`.
	let span = |n: u32, new: u32, old: u32| {
		let start = u64::from(n) * u64::from(old) / u64::from(new);
		let end = (u64::from(n + 1) * u64::from(old) / u64::from(new)).max(start + 1);
		(start, end)
	};
	let source_width = u64::from(thumbnail.width);
	let mut bgra =
		Vec::with_capacity(usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(0));
	for y in 0..height {
		let (top, bottom) = span(y, height, thumbnail.height);
		for x in 0..width {
			let (left, right) = span(x, width, thumbnail.width);
			let mut sums = [0_u64; 4];
			for row in top..bottom {
				for column in left..right {
					let at =
						usize::try_from((row * source_width + column) * 4).unwrap_or(usize::MAX);
					if let Some(pixel) = thumbnail.bgra.get(at..at + 4) {
						for (sum, byte) in sums.iter_mut().zip(pixel) {
							*sum += u64::from(*byte);
						}
					}
				}
			}
			let count = (bottom - top) * (right - left);
			bgra.extend(
				sums.iter()
					.map(|sum| u8::try_from(sum / count).unwrap_or(u8::MAX)),
			);
		}
	}
	Thumbnail {
		width,
		height,
		bgra,
		alpha: thumbnail.alpha,
	}
}

impl<L: LocalFs, D: Drive> ThumbnailService<L, D> {
	/// A service running its requests on `runtime`, serving no root yet.
	pub(crate) fn new(config: ThumbnailConfig, runtime: Handle) -> Self {
		Self {
			slots: Arc::new(Semaphore::new(
				config.concurrency.clamp(1, Semaphore::MAX_PERMITS),
			)),
			config,
			runtime,
			target: Mutex::new(None),
			verdicts: Mutex::new(HashMap::new()),
			requests: AtomicU64::new(0),
		}
	}

	fn target(&self) -> MutexGuard<'_, Option<Target<L, D>>> {
		self.target.lock().unwrap_or_else(PoisonError::into_inner)
	}

	fn verdicts(&self) -> MutexGuard<'_, HashMap<(Bytes, u32), (ThumbnailRefusal, Instant)>> {
		self.verdicts.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Answers for the root at `root`, whose file system is `local`, from `drive`.
	pub(crate) fn serve(&self, root: RootDir, local: Arc<L>, drive: Arc<D>) {
		*self.target() = Some(Target { root, local, drive });
	}

	/// Answers every later request [`ThumbnailRefusal::SignedOut`], and forgets the verdicts it
	/// remembered: the next session may be another account's.
	pub(crate) fn stop_serving(&self) {
		*self.target() = None;
		self.verdicts().clear();
	}

	/// The handler the COM layer is handed: [`ThumbnailService::thumbnail_for`] on this service.
	pub(crate) fn handler(self: &Arc<Self>) -> ThumbnailHandler {
		let service = Arc::clone(self);
		Arc::new(move |path, size| service.thumbnail_for(path, size))
	}

	/// The thumbnail of the item at `path`, its longer side at most `size_px` (clamped to
	/// 16..=1024), or why none is given; within the deadline, blocking the calling thread. For a
	/// thread outside the service's runtime, as the COM layer's is: called on a thread of the
	/// runtime, it refuses with [`ThumbnailRefusal::Busy`] rather than block the runtime.
	pub(crate) fn thumbnail_for(
		&self,
		path: &Path,
		size_px: u32,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		if Handle::try_current().is_ok() {
			warn!("a thumbnail was asked for on the helper's runtime; refused");
			return Err(ThumbnailRefusal::Busy);
		}
		self.runtime.block_on(self.request(path, size_px))
	}

	/// [`ThumbnailService::thumbnail_for`], awaited: one request, within the deadline and a slot,
	/// logged with its number, timing and outcome, never with the path.
	pub(crate) async fn request(
		&self,
		path: &Path,
		size_px: u32,
	) -> Result<Thumbnail, ThumbnailRefusal> {
		let number = self.requests.fetch_add(1, Ordering::Relaxed);
		let started = Instant::now();
		let side = clamp_side(size_px);
		let cancel = Arc::new(AtomicBool::new(false));
		let answer = timeout(self.config.deadline, async {
			let _slot = self
				.slots
				.acquire()
				.await
				.expect("the slots are never closed");
			self.make(path, side, &cancel).await
		})
		.await;
		let (source, answer) = match answer {
			Ok(made) => made,
			Err(_) => {
				// The work is dropped with the future; the flag stops a decode running on detached.
				cancel.store(true, Ordering::Relaxed);
				("deadline", Err(ThumbnailRefusal::Busy))
			}
		};
		debug!(
			request = number,
			size = size_px,
			made_at = bucket(side),
			source,
			elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
			outcome = %match &answer {
				Ok(thumbnail) => format!("{}x{}", thumbnail.width, thumbnail.height),
				Err(refusal) => format!("{refusal:?}"),
			},
			"thumbnail request"
		);
		answer
	}

	/// The thumbnail of the item at `path` fitted inside a `side` square: which bytes it comes
	/// from, for the log, and the answer.
	async fn make(
		&self,
		path: &Path,
		side: u32,
		cancel: &Arc<AtomicBool>,
	) -> (&'static str, Result<Thumbnail, ThumbnailRefusal>) {
		let Some((root, local, drive)) = self.target().as_ref().map(|target| {
			(
				target.root.clone(),
				Arc::clone(&target.local),
				Arc::clone(&target.drive),
			)
		}) else {
			return ("none", Err(ThumbnailRefusal::SignedOut));
		};
		let plan = match plan(&root, &local, path).await {
			Ok(plan) => plan,
			Err(refusal) => return ("none", Err(refusal)),
		};
		let made_at = bucket(side);
		let remembered = (plan.key().clone(), made_at);
		if let Some(refusal) = self.remembered(&remembered) {
			return (plan.kind(), Err(refusal));
		}
		let kind = plan.kind();
		let outcome = match plan {
			Plan::Local { path, id, .. } => {
				let name = path
					.name()
					.map(|name| name.as_str().to_owned())
					.unwrap_or_default();
				let opened = {
					let local = Arc::clone(&local);
					blocking(move || local.open_on_disk(&path, id)).await
				};
				let source = match opened {
					Ok(source) => source,
					Err(LocalError::Busy) => return (kind, Err(ThumbnailRefusal::Busy)),
					Err(error) => {
						debug!(%error, "the local file changed before its thumbnail");
						return (kind, Err(ThumbnailRefusal::Failed));
					}
				};
				drive
					.fitted_thumbnail_of_local(source, &name, made_at, cancel)
					.await
			}
			Plan::Remote { id, .. } => drive.fitted_thumbnail(&id, made_at, cancel).await,
		};
		let refusal = match outcome {
			Ok(FittedOutcome::Thumbnail(thumbnail)) => {
				return (
					kind,
					to_pixels(&thumbnail, side).map_err(|error| {
						warn!(%error, "a thumbnail the cache made does not decode");
						ThumbnailRefusal::Failed
					}),
				);
			}
			// Settled verdicts about the bytes, remembered.
			Ok(FittedOutcome::Unsupported | FittedOutcome::Corrupt(_)) => {
				ThumbnailRefusal::Unsupported
			}
			Ok(FittedOutcome::OverBudget) => ThumbnailRefusal::TooLarge,
			Err(error) => {
				debug!(%error, "the drive could not make the thumbnail");
				return (kind, Err(cache_refusal(&error)));
			}
		};
		self.verdicts()
			.insert(remembered, (refusal, Instant::now()));
		(kind, Err(refusal))
	}

	/// The settled refusal remembered for `key`, if it is recent enough.
	fn remembered(&self, key: &(Bytes, u32)) -> Option<ThumbnailRefusal> {
		let mut verdicts = self.verdicts();
		let remember = self.config.remember;
		verdicts.retain(|_, (_, at)| at.elapsed() < remember);
		verdicts.get(key).map(|(refusal, _)| *refusal)
	}
}

/// Where the bytes of the item at `path` in the root at `root` come from, read through `local`
/// without opening the item's data: a file all on disk from its local bytes (an ordinary file
/// the provider has not taken in, or a placeholder, edited locally or not), a placeholder of a
/// drive file in sync from the drive.
async fn plan<L: LocalFs>(
	root: &RootDir,
	local: &Arc<L>,
	path: &Path,
) -> Result<Plan, ThumbnailRefusal> {
	let text = path.to_str().ok_or(ThumbnailRefusal::NotInRoot)?;
	let components = below_root(root.as_str(), text, HOST).ok_or(ThumbnailRefusal::NotInRoot)?;
	if components.is_empty() {
		return Err(ThumbnailRefusal::Directory);
	}
	// A component Windows cannot hold under its spelling is no placeholder's.
	let at = RootPath::parse(components).map_err(|_| ThumbnailRefusal::NotAPlaceholder)?;
	let entry = {
		let local = Arc::clone(local);
		let at = at.clone();
		blocking(move || local.inspect(&at)).await
	};
	let local_bytes = |id: LocalFileId, size: u64, modified: i64| Plan::Local {
		path: at.clone(),
		id,
		key: Bytes::Local { id, size, modified },
	};
	match entry {
		Ok(LocalEntry::Missing) | Err(LocalError::NotFound) => {
			debug!("the item asked for is gone");
			Err(ThumbnailRefusal::Failed)
		}
		Err(LocalError::Busy) => Err(ThumbnailRefusal::Busy),
		Err(error) => {
			debug!(%error, "reading the item's state failed");
			Err(ThumbnailRefusal::Failed)
		}
		Ok(LocalEntry::Unsupported(_)) => Err(ThumbnailRefusal::NotAPlaceholder),
		Ok(
			LocalEntry::Plain {
				kind: LocalKind::Dir { .. },
				..
			}
			| LocalEntry::Placeholder {
				kind: LocalKind::Dir { .. },
				..
			},
		) => Err(ThumbnailRefusal::Directory),
		Ok(LocalEntry::Plain {
			kind: LocalKind::File(stamp),
			id,
		}) => Ok(local_bytes(id, stamp.size, stamp.modified)),
		Ok(LocalEntry::Placeholder {
			identity: Err(_), ..
		}) => Err(ThumbnailRefusal::NotAPlaceholder),
		Ok(LocalEntry::Placeholder {
			kind: LocalKind::File(stamp),
			id,
			identity: Ok(identity),
			state,
		}) => {
			let on_disk = state.on_disk() >= stamp.size;
			match identity {
				PlaceholderIdentity::File { version, .. } if !on_disk => {
					if state.in_sync() {
						let id = identity
							.cache_id()
							.ok_or(ThumbnailRefusal::NotAPlaceholder)?;
						Ok(Plan::Remote {
							id,
							key: Bytes::Remote {
								version: version.to_string(),
							},
						})
					} else {
						// Edited locally and not all on disk: neither side holds the bytes the
						// file shows, and reading them would hydrate it.
						debug!("an edited file not all on disk has no thumbnail");
						Err(ThumbnailRefusal::Failed)
					}
				}
				PlaceholderIdentity::File { .. } | PlaceholderIdentity::Detached { .. }
					if on_disk =>
				{
					Ok(local_bytes(id, stamp.size, stamp.modified))
				}
				PlaceholderIdentity::File { .. }
				| PlaceholderIdentity::Detached { .. }
				| PlaceholderIdentity::Root { .. }
				| PlaceholderIdentity::Dir { .. } => Err(ThumbnailRefusal::NotAPlaceholder),
			}
		}
	}
}
