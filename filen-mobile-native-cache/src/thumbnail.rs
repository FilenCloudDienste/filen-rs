use std::{path::PathBuf, sync::Arc};

use filen_sdk_rs::{
	fs::{
		HasName, HasUUID,
		file::{RemoteFile, traits::HasFileInfo},
	},
	io::FilenMetaExt,
	thumbnail::{
		ByteSource, FileSource, RemoteChunkSource, ThumbSpec, ThumbnailFit, ThumbnailOutcome,
		make_thumbnail_from_source,
	},
};
use futures::StreamExt;
use tokio::sync::OwnedRwLockReadGuard;
use tracing::debug;
use uuid::Uuid;

use crate::{
	CacheError,
	auth::{AuthCacheState, CacheState, FilenMobileCacheState},
	ffi::FfiId,
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

/// Removes the temp file a thumbnail encode writes into unless [`disarm`](Self::disarm)ed
/// after the rename into place — the one cleanup covering every exit, including the future
/// being dropped mid-await by a cancelled bulk batch.
struct TmpFileGuard {
	path: Option<PathBuf>,
}

impl TmpFileGuard {
	fn disarm(mut self) {
		self.path = None;
	}
}

impl Drop for TmpFileGuard {
	fn drop(&mut self) {
		if let Some(path) = self.path.take() {
			// Sync removal of one small local file; best-effort by design.
			let _ = std::fs::remove_file(path);
		}
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
		let tmp_guard = TmpFileGuard {
			path: Some(tmp_path.clone()),
		};

		let mut tmp_file = tmp_file.into_std().await;

		// A batch cancel aborts this future, but the blocking closure keeps
		// running detached — detached network fetches on the remote path, a
		// decode hogging the single gate permit on the local one. The guard
		// flips the source's cancel flag when this future drops un-disarmed,
		// and BOTH source kinds fail their next read, unwinding the orphaned
		// decode at the same granularity an async decoder would get instead of
		// letting it run to completion.
		struct CancelOnDrop(Option<Arc<std::sync::atomic::AtomicBool>>);
		impl Drop for CancelOnDrop {
			fn drop(&mut self) {
				if let Some(flag) = self.0.take() {
					flag.store(true, std::sync::atomic::Ordering::Relaxed);
				}
			}
		}
		let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
			// afford it" call for different fixes. What reaches this arm as
			// Err is transport/system trouble that must stay retryable.
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
