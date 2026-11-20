use std::{
	io::{self, Cursor},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::FilenMetaExt,
	thumbnail::{
		ByteSource, FileSource, RemoteChunkSource, ThumbSpec, ThumbnailFit, ThumbnailOutcome,
		make_thumbnail_from_source, might_be_thumbnailable,
	},
};
use futures::StreamExt;
use image::{ImageFormat, ImageReader};
use tokio::sync::OwnedRwLockReadGuard;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::{
	CacheError,
	auth::{AuthCacheState, CacheState, FilenMobileCacheState},
	ffi::FfiId,
	io::TmpFileGuard,
	sql::{self, object::DBObject},
};

/// Files.app hands over entire grid batches at once; bound how many ITEMS are
/// in flight so a 1000-photo directory cannot queue hundreds of concurrent
/// downloads. Wider than the client's decode bound: downloads are disk-streamed
/// and cheap to overlap, and a slot waiting on a download (or a per-item lock)
/// must not starve cache hits behind it.
///
/// Not a `ClientConfig` knob — the SDK has no notion of a thumbnail batch. A host
/// overrides it through `AuthFile::thumbnail_item_concurrency`.
pub(crate) const DEFAULT_THUMBNAIL_ITEM_CONCURRENCY: usize = 4;

/// Sets the decode's cancel flag when the future awaiting the decode drops before disarming it:
/// the blocking decode it spawned runs on detached, and its source fails its next read once the
/// flag is set.
struct CancelOnDrop(Option<Arc<AtomicBool>>);

impl Drop for CancelOnDrop {
	fn drop(&mut self) {
		if let Some(flag) = self.0.take() {
			flag.store(true, Ordering::Relaxed);
		}
	}
}

/// A thumbnail fitted inside a square box with its aspect ratio kept, encoded as lossless WebP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedThumbnail {
	/// The encoded image.
	pub webp: Vec<u8>,
	/// Its width in pixels, at most the box's side.
	pub width: u32,
	/// Its height in pixels, at most the box's side.
	pub height: u32,
}

/// How a fitted thumbnail ended. Every variant is a settled verdict about the bytes, which a caller
/// may remember (see [`ThumbnailOutcome`]); transport and system trouble stays an `Err`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FittedOutcome {
	/// The thumbnail.
	Thumbnail(FittedThumbnail),
	/// Not an image this build decodes, by name or by its bytes.
	Unsupported,
	/// An image, but over the memory budget, or one that needs a full decode of more bytes than a
	/// remote file is streamed for.
	OverBudget,
	/// An image whose bitstream the decoder refused, with the decoder's message.
	Corrupt(String),
}

/// The name of the fitted thumbnail no wider or taller than `max_side` pixels, in the item's
/// directory beside the cropped `<w>x<h>.webp` sizes, so the two fits never share a file. The
/// cache's size sweep evicts it as it evicts those.
fn fitted_file_name(max_side: u32) -> String {
	format!("fit{max_side}.webp")
}

/// The directory the thumbnails of the file version `uuid` are kept in, below `thumbnail_dir`.
fn item_thumbnail_dir(thumbnail_dir: &Path, uuid: Uuid) -> PathBuf {
	thumbnail_dir.join(uuid.to_string())
}

/// The width and height a WebP image declares in its header, read without decoding it.
fn webp_dimensions(webp: &[u8]) -> Result<(u32, u32), image::ImageError> {
	ImageReader::with_format(Cursor::new(webp), ImageFormat::WebP).into_dimensions()
}

/// A source that fails every read once `cancel` is set, as [`FileSource::with_cancel`] does, for a
/// source handed in without a flag of its own.
struct CancellableSource {
	inner: Box<dyn ByteSource>,
	cancel: Arc<AtomicBool>,
}

impl ByteSource for CancellableSource {
	fn len(&self) -> u64 {
		self.inner.len()
	}

	fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
		if self.cancel.load(Ordering::Relaxed) {
			// `Other`, never `Interrupted`, which `read_exact` retries forever.
			return Err(io::Error::other("thumbnail cancelled"));
		}
		self.inner.read_at(offset, buf)
	}

	fn hint_bulk_sequential(&mut self) {
		self.inner.hint_bulk_sequential();
	}
}

/// Decodes `source` into a lossless WebP fitted inside `spec`'s box, its aspect ratio kept and
/// never upscaled. Synchronous, as [`make_thumbnail_from_source`] is: run it off the async workers.
pub fn encode_fitted(
	source: Box<dyn ByteSource>,
	spec: &ThumbSpec,
) -> Result<FittedOutcome, filen_sdk_rs::error::Error> {
	let mut webp = Vec::new();
	Ok(
		match make_thumbnail_from_source(source, spec, ThumbnailFit::Contain, None, &mut webp)? {
			ThumbnailOutcome::Thumbnail(info) => FittedOutcome::Thumbnail(FittedThumbnail {
				webp,
				width: info.width,
				height: info.height,
			}),
			ThumbnailOutcome::Unsupported => FittedOutcome::Unsupported,
			ThumbnailOutcome::OverBudget => FittedOutcome::OverBudget,
			ThumbnailOutcome::Corrupt(message) => FittedOutcome::Corrupt(message),
		},
	)
}

/// The aborted error a cancelled fitted thumbnail fails with.
fn fitted_cancelled() -> CacheError {
	CacheError::Aborted("the thumbnail was cancelled".into())
}

/// Runs [`encode_fitted`] on the blocking pool under `client`'s decode gate. Dropping the returned
/// future sets `cancel`; a decode that failed after `cancel` was set fails as aborted.
///
/// The gate is the client's, not the process's: the cache state is not held while it decodes, so
/// a client rebuilt meanwhile, as an auth re-check does once auth.json changed, starts its next
/// decode under a gate of its own while this one still runs on the old client. The process then
/// holds two decodes, up to twice the thumbnail memory budget, until this one ends.
async fn decode_fitted(
	client: &Client,
	source: Box<dyn ByteSource>,
	spec: ThumbSpec,
	cancel: &Arc<AtomicBool>,
) -> Result<FittedOutcome, CacheError> {
	let mut cancel_guard = CancelOnDrop(Some(Arc::clone(cancel)));
	// Acquired before the blocking task and moved into it, as `get_or_make_thumbnail` does: a
	// dropped caller must not release the permit while its detached decode still runs.
	let decode_permit = client.thumbnails().decode_permit().await;
	let decoded = tokio::task::spawn_blocking(move || {
		let _decode_permit = decode_permit;
		encode_fitted(source, &spec)
	})
	.await;
	cancel_guard.0 = None;
	match decoded {
		Err(e) => Err(CacheError::image(format!(
			"thumbnail decode task failed: {e}"
		))),
		Ok(Err(_)) if cancel.load(Ordering::Relaxed) => Err(fitted_cancelled()),
		Ok(decoded) => decoded.map_err(CacheError::from),
	}
}

/// The fitted thumbnail cached at `path`, if one is there and reads; one that does not is made
/// again.
async fn cached_fitted(path: &Path) -> Result<Option<FittedThumbnail>, CacheError> {
	let webp = match tokio::fs::read(path).await {
		Ok(webp) => webp,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e.into()),
	};
	match webp_dimensions(&webp) {
		Ok((width, height)) => Ok(Some(FittedThumbnail {
			webp,
			width,
			height,
		})),
		Err(e) => {
			debug!(
				"the cached thumbnail {} does not read ({e}); making it again",
				path.display()
			);
			Ok(None)
		}
	}
}

/// Writes `webp` to `dir`/`name` through a temporary file renamed into place, so a reader never
/// sees part of it.
async fn store_fitted(dir: &Path, name: &str, webp: &[u8]) -> Result<(), CacheError> {
	tokio::fs::create_dir_all(dir).await?;
	let tmp_path = dir.join(format!("{name}.{}.tmp", Uuid::new_v4()));
	let tmp_guard = TmpFileGuard::new(tmp_path.clone());
	tokio::fs::write(&tmp_path, webp).await?;
	tokio::fs::rename(&tmp_path, dir.join(name)).await?;
	tmp_guard.disarm();
	Ok(())
}

// Rust-only: not exported through UniFFI. The cropped `get_thumbnail` above is unchanged.
impl FilenMobileCacheState {
	/// The thumbnail of the file `item` names, fitted inside a `max_side` pixel square with its
	/// aspect ratio kept (where [`FilenMobileCacheState::get_thumbnail`] crops to fill the box), made
	/// from the remote file's chunks and kept in the thumbnail cache beside the cropped sizes.
	///
	/// The cache state is locked only while the id is resolved, as in
	/// [`FilenMobileCacheState::read_file_range`]: the decode runs on the client the state held then,
	/// so a slow thumbnail never holds up an auth re-check. `cancel` stops the decode at its next
	/// chunk; the call then fails with [`CacheError::Aborted`]. Dropping the future sets `cancel`.
	pub async fn fitted_thumbnail(
		&self,
		item: &FfiId,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> Result<FittedOutcome, CacheError> {
		if cancel.load(Ordering::Relaxed) {
			return Err(fitted_cancelled());
		}
		let (client, file, dir) = self
			.async_execute_authed_owned(async |auth_state| {
				let file = RemoteFile::try_from(auth_state.select_file_by_id(item)?)?;
				let dir = item_thumbnail_dir(&auth_state.thumbnail_dir, file.uuid());
				Ok((Arc::clone(&auth_state.client), file, dir))
			})
			.await?;
		// From the name and the stored mime, as `get_or_make_thumbnail` decides.
		if !might_be_thumbnailable(file.name(), file.mime()) {
			return Ok(FittedOutcome::Unsupported);
		}
		let name = fitted_file_name(max_side);
		if let Some(cached) = cached_fitted(&dir.join(&name)).await? {
			return Ok(FittedOutcome::Thumbnail(cached));
		}
		let spec = client
			.thumbnails()
			.spec_remote(max_side, max_side, file.size());
		let source = Box::new(RemoteChunkSource::new(
			Arc::clone(&client),
			file.clone(),
			tokio::runtime::Handle::current(),
			Some(Arc::clone(cancel)),
		));
		let outcome = decode_fitted(&client, source, spec, cancel).await?;
		match &outcome {
			FittedOutcome::Thumbnail(thumbnail) => {
				// The thumbnail is made; failing to keep it costs a later request a decode.
				if let Err(e) = store_fitted(&dir, &name, &thumbnail.webp).await {
					warn!(
						"keeping the fitted thumbnail of {} failed: {e}",
						file.uuid()
					);
				}
			}
			FittedOutcome::Unsupported | FittedOutcome::OverBudget | FittedOutcome::Corrupt(_) => {
				debug!("no fitted thumbnail for {}: {outcome:?}", file.uuid());
			}
		}
		Ok(outcome)
	}

	/// The thumbnail of local bytes, `source`, of a file named `name`, fitted as
	/// [`FilenMobileCacheState::fitted_thumbnail`] fits it, with the client's budget for bytes
	/// already on disk; nothing is cached, as the bytes have no remote version to key them by.
	/// Signed out, it fails as the cache does, since the budget is the client's.
	pub async fn fitted_thumbnail_of_local(
		&self,
		source: Box<dyn ByteSource>,
		name: &str,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> Result<FittedOutcome, CacheError> {
		if cancel.load(Ordering::Relaxed) {
			return Err(fitted_cancelled());
		}
		if !might_be_thumbnailable(Some(name), None) {
			return Ok(FittedOutcome::Unsupported);
		}
		let client = self
			.async_execute_authed_owned(async |auth_state| Ok(Arc::clone(&auth_state.client)))
			.await?;
		let spec = client.thumbnails().spec_local(max_side, max_side);
		let source = Box::new(CancellableSource {
			inner: source,
			cancel: Arc::clone(cancel),
		});
		decode_fitted(&client, source, spec, cancel).await
	}

	/// The thumbnail of the file version `uuid` fitted inside a `max_side` pixel square, if the
	/// thumbnail cache holds it, as [`FilenMobileCacheState::fitted_thumbnail`] kept it: read from
	/// disk with no server round trip, nothing decoded and nothing made. Keyed by the version, so a
	/// thumbnail of another version is never answered. Signed out, it fails as the cache does.
	pub async fn cached_fitted_thumbnail(
		&self,
		uuid: Uuid,
		max_side: u32,
	) -> Result<Option<FittedThumbnail>, CacheError> {
		let thumbnail_dir = self
			.async_execute_authed_owned(async |auth_state| Ok(auth_state.thumbnail_dir.clone()))
			.await?;
		cached_fitted(&item_thumbnail_dir(&thumbnail_dir, uuid).join(fitted_file_name(max_side)))
			.await
	}
}

impl AuthCacheState {
	async fn get_or_make_thumbnail(
		&self,
		file: &RemoteFile,
		target_width: u32,
		target_height: u32,
	) -> Result<Option<PathBuf>, CacheError> {
		// Decided from the filename, not the stored mime: that mime was written
		// once at upload by whichever client uploaded the file and is a fossil
		// of that library's table on that machine — HEIC in particular is
		// stored as application/octet-stream by Go clients on macOS and by any
		// Rust client predating June 2024. See `might_be_thumbnailable`.
		if !filen_sdk_rs::thumbnail::might_be_thumbnailable(file.name(), file.mime()) {
			debug!(
				"Not a thumbnailable file, skipping: {:?} ({:?})",
				file.name(),
				file.mime()
			);
			return Ok(None);
		}
		let file_path = self.get_cached_file_path(file);
		let file_thumbnails_path = self.thumbnail_dir.join(file.uuid().to_string());
		tokio::fs::create_dir_all(&file_thumbnails_path).await?;
		let thumbnail_path =
			file_thumbnails_path.join(format!("{target_width}x{target_height}.webp"));
		match tokio::fs::metadata(&thumbnail_path).await {
			// Zero-size files are leftovers from the pre-atomic writer; regenerate them.
			Ok(meta) if FilenMetaExt::size(&meta) != 0 => {
				return Ok(Some(thumbnail_path));
			}
			Ok(_) => {}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
			Err(e) => return Err(e.into()),
		}
		// Bytes come through a microthumb ByteSource either way: the cached
		// local file when one exists, the remote chunks directly otherwise.
		// The remote source fetches lazily, so an embedded-preview hit costs
		// one or two chunks of download — and thumbnail work no longer
		// populates the file cache at all (a Files.app grid burst used to
		// download every photo in full just to shrink it). The deliberate
		// cost of that trade: browsing a folder and then OPENING a photo now
		// downloads its bytes twice — once (partially) for the thumbnail,
		// once in full for the open — where the old path had them cached.
		// Do not "fix" this back without re-weighing the burst case.
		let local_file = match tokio::fs::File::open(&file_path).await {
			Ok(file) => Some(file),
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				debug!(
					"No cached bytes; thumbnailing off the remote chunks: {}",
					file_path.display()
				);
				None
			}
			Err(e) => {
				debug!(
					"Failed to open file for thumbnail: {} at path {}",
					e,
					file_path.display()
				);
				return Err(e.into());
			}
		};

		// Encode into a unique temp file and rename into place: the thumbnail
		// only becomes visible once complete, so neither a cancelled task nor a
		// concurrent request for the same size can surface (and cache) a torn
		// file.
		let tmp_path = file_thumbnails_path.join(format!(
			"{target_width}x{target_height}.{}.tmp",
			Uuid::new_v4()
		));
		let create_tmp = || async {
			tokio::fs::OpenOptions::new()
				.write(true)
				.create_new(true)
				.open(&tmp_path)
				.await
		};
		let tmp_file = match create_tmp().await {
			Ok(file) => file,
			// The sweep removes an item's directory with its last evicted size, and landing
			// between the create_dir_all above and here it takes the directory this encode
			// just made. Once the temp file exists the directory is not empty and the sweep
			// leaves it alone, so making the directory once more is enough.
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				tokio::fs::create_dir_all(&file_thumbnails_path).await?;
				create_tmp().await?
			}
			Err(e) => return Err(e.into()),
		};
		// One cleanup for every exit: the guard removes the temp file on drop unless
		// the rename below disarmed it — covering the error arms, a failed rename,
		// AND an aborted batch (cancel() drops this future mid-await while the
		// detached blocking closure keeps writing to the already-open, now-unlinked
		// file, which is harmless).
		let tmp_guard = TmpFileGuard::new(tmp_path.clone());

		let mut tmp_file = tmp_file.into_std().await;

		// A batch cancel aborts this future, but the blocking closure keeps
		// running detached — detached network fetches on the remote path, a
		// decode hogging the single gate permit on the local one. The guard
		// flips the source's cancel flag when this future drops un-disarmed,
		// and BOTH source kinds fail their next read, unwinding the orphaned
		// decode at the same granularity an async decoder would get instead of
		// letting it run to completion.
		let cancel = Arc::new(AtomicBool::new(false));
		let mut cancel_guard = CancelOnDrop(Some(cancel.clone()));

		// `mem_budget` bounds memory; `allow_full_decode` bounds how many bytes
		// of SOURCE we are willing to pull. Reading a huge file off the network
		// to make a 256 px thumbnail is the waste this guards — but refusing
		// outright on size, as this used to, threw away the cheap paths too: a
		// 200 MB HEIC's `thmb` item and a JPEG's EXIF thumbnail both live in
		// the header region and cost a chunk or two. Probe those, and settle on
		// `OverBudget` having read almost nothing when there is nothing to find.
		// Both budgets are the client's, configured in `AuthCacheState::from_sdk_config`.
		let thumbnails = self.client.thumbnails();
		let (source, spec): (Box<dyn ByteSource>, ThumbSpec) = match local_file {
			Some(local) => (
				Box::new(
					FileSource::with_cancel(local.into_std().await, Some(cancel))
						.map_err(CacheError::from)?,
				),
				// The bytes are already on disk; nothing to protect.
				thumbnails.spec_local(target_width, target_height),
			),
			None => {
				let spec = thumbnails.spec_remote(target_width, target_height, file.size());
				if !spec.allow_full_decode {
					debug!(
						"File too large to stream for a thumbnail ({} bytes); embedded previews only: {}",
						file.size(),
						file_path.display()
					);
				}
				(
					Box::new(RemoteChunkSource::new(
						self.client.clone(),
						file.clone(),
						tokio::runtime::Handle::current(),
						Some(cancel),
					)),
					spec,
				)
			}
		};

		// Decode buffers, not downloads, are the memory hazard — the client's gate
		// bounds how many run at once. Acquired before the blocking task is spawned, so
		// parked work costs a future, not a blocked pool thread; MOVED INTO the closure
		// so the permit is released when the decode actually ends — a cancelled batch
		// drops this future while the blocking closure keeps running detached, and a
		// permit held by the dropped future would let fresh decodes stack on the orphans.
		let decode_permit = thumbnails.decode_permit().await;
		let decode_result = tokio::task::spawn_blocking(
			move || -> Result<ThumbnailOutcome, filen_sdk_rs::error::Error> {
				let _decode_permit = decode_permit;
				make_thumbnail_from_source(
					source,
					&spec,
					// Files.app draws a fixed-size grid tile, so crop to the
					// requested box rather than letter-boxing into it.
					ThumbnailFit::Cover,
					// Lossless, as every cached thumbnail has always been.
					None,
					&mut tmp_file,
				)
			},
		)
		.await;
		// The decode is over — nothing is left for a late drop to cancel.
		cancel_guard.0 = None;

		let decode_result = match decode_result {
			Ok(result) => result,
			// A panicking decode must fail this item alone, not tear down the
			// whole bulk batch and leave its callback unanswered.
			Err(e) => {
				return Err(CacheError::image(format!(
					"thumbnail decode task failed: {e}"
				)));
			}
		};

		match decode_result {
			Ok(ThumbnailOutcome::Thumbnail(info)) => {
				// Which path served this is the difference between a couple of
				// container reads and fetching the whole file, and it is
				// invisible from the outside — log it so a silently broken
				// fast path cannot masquerade as "working, just slower".
				debug!(
					"thumbnail for {} produced {}x{} from {}",
					file.uuid(),
					info.width,
					info.height,
					info.source
				);
				tokio::fs::rename(&tmp_path, &thumbnail_path).await?;
				tmp_guard.disarm();
				Ok(Some(thumbnail_path))
			}
			// Over budget, unsupported, or undecodable bytes — all settled
			// verdicts about this file, which the platform may cache. Logged
			// apart because "we never decode this" and "this host could not
			// afford it" call for different fixes. An Err is not one of these:
			// transport/system trouble must stay retryable, and a content hash
			// mismatch is its own error.
			Ok(
				outcome @ (ThumbnailOutcome::Unsupported
				| ThumbnailOutcome::OverBudget
				| ThumbnailOutcome::Corrupt(_)),
			) => {
				debug!("no thumbnail for {}: {:?}", file.uuid(), outcome);
				Ok(None)
			}
			Err(e) => Err(CacheError::from(e)),
		}
	}

	async fn make_thumbnail_for_path(
		&self,
		path: &FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> ThumbnailResult {
		// Every refusal below is otherwise invisible: the system caches a "no thumbnail"
		// answer per item and stops asking, so a silent early return reads on-device as
		// "thumbnails just do not work" with nothing in the log to say why.
		debug!(
			"thumbnail requested for {} at {requested_width}x{requested_height}",
			path.0
		);
		// An identity-form (`stable/`) id selects its row directly: rebuilding a display path
		// and re-resolving it can hand back a same-named sibling's bytes as this id's thumbnail.
		let selected = if path.0.starts_with(crate::local::STABLE_PREFIX) {
			self.select_object_by_id(path)
		} else {
			let path = match self.canonicalize_id(path) {
				Ok(path) => path,
				Err(e) => {
					debug!("thumbnail id {} could not be canonicalized: {e}", path.0);
					return ThumbnailResult::Err(e);
				}
			};
			match path.as_parsed() {
				Ok(pvs) => sql::select_object_at_parsed_id(&self.conn(), &pvs),
				Err(e) => {
					debug!("thumbnail id could not be parsed: {e}");
					return ThumbnailResult::Err(e);
				}
			}
		};
		let file = match selected {
			Ok(Some(DBObject::File(file))) => file,
			Ok(Some(_)) => {
				debug!("thumbnail target {} is not a file; no thumbnail", path.0);
				return ThumbnailResult::NoThumbnail;
			}
			Ok(None) => {
				debug!("thumbnail target {} resolves to no row; not found", path.0);
				return ThumbnailResult::NotFound;
			}
			Err(e) => {
				debug!("thumbnail target {} failed to resolve: {e}", path.0);
				return ThumbnailResult::Err(e);
			}
		};

		let remote_file = match file.try_into() {
			Ok(remote_file) => remote_file,
			Err(e) => {
				debug!("thumbnail target {} has undecoded metadata: {e}", path.0);
				return ThumbnailResult::Err(CacheError::from(e));
			}
		};
		match self
			.get_or_make_thumbnail(&remote_file, requested_width, requested_height)
			.await
		{
			Ok(Some(path)) => ThumbnailResult::Ok(path.to_string_lossy().to_string()),
			Ok(None) => {
				debug!("no thumbnail produced for {}", path.0);
				ThumbnailResult::NoThumbnail
			}
			Err(e) => {
				debug!("thumbnail generation failed for {}: {e}", path.0);
				ThumbnailResult::Err(e)
			}
		}
	}
}

#[derive(uniffi::Enum)]
pub enum ThumbnailResult {
	Ok(String),
	Err(CacheError),
	NotFound,
	NoThumbnail,
}

impl From<CacheError> for ThumbnailResult {
	fn from(e: CacheError) -> Self {
		ThumbnailResult::Err(e)
	}
}

#[uniffi::export(with_foreign)]
pub trait ThumbnailCallback: Send + Sync {
	fn process(&self, id: FfiId, result: ThumbnailResult);
	fn complete(&self);
}

#[derive(uniffi::Object)]
pub struct BulkThumbnailResponse {
	task: tokio::task::JoinHandle<()>,
}

#[uniffi::export]
impl BulkThumbnailResponse {
	pub fn cancel(&self) {
		if !self.task.is_finished() {
			self.task.abort();
		}
	}
}

impl AuthCacheState {
	pub(crate) fn get_thumbnails(
		this: OwnedRwLockReadGuard<CacheState, Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> BulkThumbnailResponse {
		let item_concurrency = this.thumbnail_item_concurrency;
		let arc = Arc::new(this);
		let handle = crate::env::get_runtime().spawn(async move {
			futures::stream::iter(items)
				.for_each_concurrent(item_concurrency, |item| {
					let self_ref = arc.clone();
					let callback_ref = callback.clone();
					async move {
						let result = self_ref
							.make_thumbnail_for_path(&item, requested_width, requested_height)
							.await;
						callback_ref.process(item, result);
					}
				})
				.await;
			callback.complete();
		});

		BulkThumbnailResponse { task: handle }
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	pub fn get_thumbnails(
		self: Arc<Self>,
		items: Vec<FfiId>,
		requested_width: u32,
		requested_height: u32,
		callback: Arc<dyn ThumbnailCallback>,
	) -> Result<BulkThumbnailResponse, CacheError> {
		self.sync_execute_authed_owned(move |auth_state| {
			Ok(AuthCacheState::get_thumbnails(
				auth_state,
				items,
				requested_width,
				requested_height,
				callback,
			))
		})
	}
}

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	/// A thumbnail made from the remote file, when a read of it that covers the whole file does
	/// not hash to the BLAKE3 its metadata declares, answers with
	/// [`CacheError::ContentHashMismatch`]. Which reads cover the whole file is documented on
	/// [`RemoteChunkSource`].
	// not sure why this is necessary for this specific function,
	// but otherwise it seems like the macro wasn't adding this
	#[uniffi::method(name = "get_thumbnail")]
	pub async fn get_thumbnail(
		self: Arc<Self>,
		item: FfiId,
		requested_width: u32,
		requested_height: u32,
	) -> Result<ThumbnailResult, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			Ok(auth_state
				.make_thumbnail_for_path(&item, requested_width, requested_height)
				.await)
		})
		.await
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::thumbnail::MemSource;

	use super::*;

	/// A 24-bit uncompressed BMP of `width` x `height` pixels, every one `bgr`.
	fn bmp(width: u32, height: u32, bgr: [u8; 3]) -> Vec<u8> {
		let row = (width * 3).div_ceil(4) * 4;
		let pixels = row * height;
		let mut out = Vec::new();
		out.extend_from_slice(b"BM");
		out.extend_from_slice(&(54 + pixels).to_le_bytes());
		out.extend_from_slice(&[0; 4]);
		out.extend_from_slice(&54_u32.to_le_bytes());
		out.extend_from_slice(&40_u32.to_le_bytes());
		out.extend_from_slice(&i32::try_from(width).unwrap().to_le_bytes());
		out.extend_from_slice(&i32::try_from(height).unwrap().to_le_bytes());
		out.extend_from_slice(&1_u16.to_le_bytes());
		out.extend_from_slice(&24_u16.to_le_bytes());
		out.extend_from_slice(&0_u32.to_le_bytes());
		out.extend_from_slice(&pixels.to_le_bytes());
		out.extend_from_slice(&[0; 16]);
		for _ in 0..height {
			for _ in 0..width {
				out.extend_from_slice(&bgr);
			}
			out.resize(out.len() + usize::try_from(row - width * 3).unwrap(), 0);
		}
		out
	}

	/// A new empty directory under the system's temporary directory, removed on drop.
	struct Scratch(PathBuf);

	impl Scratch {
		fn new() -> Self {
			let dir = std::env::temp_dir().join(format!("fitted-thumbnail-{}", Uuid::new_v4()));
			std::fs::create_dir_all(&dir).unwrap();
			Self(dir)
		}

		fn path(&self) -> &Path {
			&self.0
		}
	}

	impl Drop for Scratch {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn spec(max_side: u32) -> ThumbSpec {
		ThumbSpec::new(max_side, max_side, 16 * 1024 * 1024)
	}

	#[test]
	fn a_fitted_thumbnail_keeps_the_aspect_ratio_inside_the_box() {
		let outcome =
			encode_fitted(Box::new(MemSource(bmp(200, 100, [0, 0, 255]))), &spec(96)).unwrap();
		let FittedOutcome::Thumbnail(thumbnail) = outcome else {
			panic!("no thumbnail: {outcome:?}");
		};
		assert_eq!((thumbnail.width, thumbnail.height), (96, 48));
		assert_eq!(webp_dimensions(&thumbnail.webp).unwrap(), (96, 48));
	}

	#[test]
	fn a_fitted_thumbnail_is_never_upscaled() {
		let outcome =
			encode_fitted(Box::new(MemSource(bmp(40, 20, [0, 255, 0]))), &spec(256)).unwrap();
		let FittedOutcome::Thumbnail(thumbnail) = outcome else {
			panic!("no thumbnail: {outcome:?}");
		};
		assert_eq!((thumbnail.width, thumbnail.height), (40, 20));
	}

	#[test]
	fn bytes_of_no_known_format_are_unsupported() {
		let outcome = encode_fitted(
			Box::new(MemSource(b"plain text, no image".to_vec())),
			&spec(96),
		)
		.unwrap();
		assert_eq!(outcome, FittedOutcome::Unsupported);
	}

	#[test]
	fn a_cancelled_source_fails_the_decode() {
		let cancel = Arc::new(AtomicBool::new(true));
		let source = CancellableSource {
			inner: Box::new(MemSource(bmp(20, 20, [1, 2, 3]))),
			cancel,
		};
		assert!(encode_fitted(Box::new(source), &spec(96)).is_err());
	}

	#[test]
	fn a_fitted_size_never_shares_a_file_with_a_cropped_one() {
		assert_eq!(fitted_file_name(256), "fit256.webp");
		assert_ne!(fitted_file_name(256), "256x256.webp");
	}

	#[tokio::test]
	async fn a_cached_fitted_thumbnail_reads_back_with_its_dimensions() {
		let dir = Scratch::new();
		let FittedOutcome::Thumbnail(made) =
			encode_fitted(Box::new(MemSource(bmp(30, 60, [9, 9, 9]))), &spec(96)).unwrap()
		else {
			panic!("no thumbnail");
		};
		store_fitted(dir.path(), "fit96.webp", &made.webp)
			.await
			.unwrap();
		assert_eq!(
			cached_fitted(&dir.path().join("fit96.webp")).await.unwrap(),
			Some(made)
		);
		let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
		assert_eq!(left.len(), 1, "the temporary file was renamed into place");
	}

	#[tokio::test]
	async fn a_cached_file_that_does_not_read_is_made_again() {
		let dir = Scratch::new();
		let path = dir.path().join("fit96.webp");
		std::fs::write(&path, b"torn").unwrap();
		assert_eq!(cached_fitted(&path).await.unwrap(), None);
		assert_eq!(
			cached_fitted(&dir.path().join("missing.webp"))
				.await
				.unwrap(),
			None
		);
	}
}
