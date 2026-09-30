use std::{
	collections::HashSet,
	path::{Path, PathBuf},
	sync::{Arc, RwLock},
	time::{Duration, Instant},
};

use chrono::DateTime;
use filen_sdk_rs::{
	ErrorKind,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::{RemoteDirectory, meta::DirectoryMetaChanges},
		file::{
			FileBuilderOptionalName, RemoteFile, meta::FileMetaChanges, traits::HasRemoteFileInfo,
		},
	},
};
use filen_types::fs::{ParentUuid, StableUuid, Uuid};
use futures::StreamExt;
use rusqlite::OptionalExtension;
use tracing::debug;

/// How many reconcile probes (`reconcile_missing_subdirs` / `reconcile_missing_trashed`) run
/// concurrently. Small: these fire on enumeration paths that gate a listing.
const RECONCILE_PROBE_CONCURRENCY: usize = 8;

/// How recently a parent's listing must have run for `update_and_query_child` to reuse it
/// instead of relisting. Short on purpose: it exists to coalesce the per-child relists of a
/// reimport burst, not to serve stale listings.
const ADOPT_RELIST_TTL_MS: i64 = 10_000;

/// How recently the recents or trash listing must have run for
/// [`FilenMobileCacheState::update_recents_if_stale`] / [`FilenMobileCacheState::update_trash_if_stale`]
/// to reuse it instead of re-fetching.
///
/// Same value as [`ADOPT_RELIST_TTL_MS`] and for the same reason: it exists to coalesce the
/// repeated asks of ONE presentation, not to serve stale listings. What it can cost is how
/// quickly those two views pick up a change the live socket did not already apply.
const LISTING_TTL: Duration = Duration::from_millis(ADOPT_RELIST_TTL_MS as u64);

/// Above this many missing rows, probing each is abandoned and the fresh listing is trusted
/// as-is (the pre-probe behaviour): hundreds of rows vanishing at once is a genuine bulk
/// delete or emptied trash, and a per-row round-trip storm would gate every presentation of
/// the container — with a killed run making no durable progress and repeating the storm.
/// (`reconcile_missing_trashed` instead probes a bounded chunk and SPARES the remainder, so
/// repeated presentations converge — a bulk restore must never read as an emptied trash.)
const RECONCILE_PROBE_CAP: usize = 64;

/// Whether `stamp` was last set within `ttl`.
fn stamped_within(stamp: &RwLock<Option<Instant>>, ttl: Duration) -> bool {
	stamp
		.read()
		.unwrap_or_else(|poisoned| poisoned.into_inner())
		.is_some_and(|at| at.elapsed() < ttl)
}

use crate::{
	CacheError,
	abort::{FfiAbortSignal, check_not_aborted, with_abort},
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		CreateFileResponse, DirWithPathResponse, DownloadResponse, FfiId, FfiObject,
		FileWithPathResponse, ObjectWithPathResponse, ParsedFfiId, PathFfiId,
		QueryChildrenResponse, QueryNonDirChildrenResponse, SearchQueryArgs,
		SearchQueryResponseEntry, UploadFileInfo,
	},
	local::{STABLE_PREFIX, addressed_stable_uuid, resolve_uuid_or_stable},
	sql::{
		self, DBDecryptedFileMeta, DBDirExt, DBDirObject, DBDirTrait, DBFileMeta, DBItemTrait,
		dir::DBDir,
		error::OptionalExtensionSQL,
		file::DBFile,
		item::RawDBItem,
		object::{DBNonRootObject, DBObject},
	},
	sync::UpdateItemsInPath,
	traits::{ProgressCallback, SearchUpdateCallback},
};

// yes this should be done with macros
// no I didn't have time
#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	pub async fn update_roots_info(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async |auth_state| auth_state.update_roots_info().await)
			.await
	}

	pub async fn update_dir_children(&self, path: FfiId) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.update_dir_children(&path).await
		})
		.await
	}

	pub async fn update_recents(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| auth_state.update_recents().await)
			.await
	}

	pub async fn update_trash(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| auth_state.update_trash().await)
			.await
	}

	/// [`Self::update_recents`], skipped when one ran within [`LISTING_TTL`].
	///
	/// What a RE-PRESENTATION wants. The listing is a full download plus a decrypt of every item,
	/// and it sat unconditionally at the top of the working-set `enumerateChanges`, which the
	/// system re-invokes once per PAGE — so one diff paid for it several times over, for rows the
	/// diff does not even read (the working-set predicate and the change feed key off
	/// `change_seq`, never `is_recent`).
	///
	/// Its own entry point rather than a throttle bolted onto [`Self::update_recents`], because
	/// the two callers mean different things: a caller that has just changed the drive and wants
	/// the listing to reflect it must still be able to say so.
	pub async fn update_recents_if_stale(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			if stamped_within(&auth_state.last_recents_update, LISTING_TTL) {
				return Ok(());
			}
			auth_state.update_recents().await
		})
		.await
	}

	/// [`Self::update_trash`], skipped when one ran within [`LISTING_TTL`]. See
	/// [`Self::update_recents_if_stale`]; the trash listing additionally pays up to
	/// [`RECONCILE_PROBE_CAP`] per-row probes, on every presentation of the trash view.
	pub async fn update_trash_if_stale(&self) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			if stamped_within(&auth_state.last_trash_update, LISTING_TTL) {
				return Ok(());
			}
			auth_state.update_trash().await
		})
		.await
	}

	/// Search the subtree rooted at `root_id` (the documents-provider root id, i.e. the drive-root
	/// uuid) via the live cache-search engine. Returns the current page immediately; `on_update`
	/// fires as the on-demand resync converges so the caller can re-query.
	pub async fn query_search(
		&self,
		root_id: String,
		args: SearchQueryArgs,
		on_update: Arc<dyn SearchUpdateCallback>,
	) -> Result<Vec<SearchQueryResponseEntry>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.query_search(root_id, args, on_update).await
		})
		.await
	}

	pub async fn update_and_query_dir_children(
		&self,
		path: FfiId,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.update_and_query_dir_children(path, order_by)
				.await
		})
		.await
	}

	pub async fn update_and_query_dir_children_page(
		&self,
		path: FfiId,
		order_by: Option<String>,
		offset: u32,
		limit: u32,
		refresh: bool,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.update_and_query_dir_children_page(path, order_by, offset, limit, refresh)
				.await
		})
		.await
	}

	pub async fn update_and_query_recents(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.update_and_query_recents(order_by).await
		})
		.await
	}

	pub async fn download_file_if_changed_by_path(
		&self,
		file_path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.download_file_if_changed_by_path(file_path, progress_callback)
				.await
		})
		.await
	}

	pub async fn download_file_if_changed_by_uuid(
		&self,
		uuid: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.download_file_if_changed_by_uuid(uuid, progress_callback)
				.await
		})
		.await
	}

	/// Downloads the file if the cached copy is not the server's, and hands back both the local
	/// path and the item it belongs to.
	///
	/// Same work as [`FilenMobileCacheState::download_file_if_changed_by_path`], which returns the
	/// path alone; a replicated provider has to answer with the item as well, and re-querying it
	/// separately would race the download it just did.
	pub async fn download_file_if_changed_with_item(
		&self,
		id: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<DownloadResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.download_file_if_changed_with_item(id, progress_callback, abort)
				.await
		})
		.await
	}

	/// Replaces a file's content with the bytes at `os_path`, and hands back what it became.
	///
	/// The provider's `modifyItem(contents:)`: the new bytes arrive as a file the caller owns
	/// rather than as something already in this cache, which is the one thing
	/// [`FilenMobileCacheState::upload_file_if_changed`] cannot take. Bytes identical to the
	/// server's are not re-uploaded, but the item still comes back — the question asked was what
	/// the file now is.
	///
	/// A failed upload leaves the bytes in the cache and the edit marked, for
	/// [`FilenMobileCacheState::retry_pending_uploads`] to deliver later.
	///
	/// `abort` cancels the upload, and only the upload: the bytes are already in the slot and the
	/// edit already marked by then, and cancelling means "stop working now", never "undo the
	/// edit". So an aborted call leaves exactly what a dropped connection leaves — marker set,
	/// bytes in the slot, [`CacheError::Aborted`] — and the drain delivers it later.
	pub async fn modify_file_content(
		&self,
		id: FfiId,
		os_path: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<FileWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.modify_file_content(id, os_path, progress_callback, abort)
				.await
		})
		.await
	}

	/// The item an id names, refreshed from the server first.
	///
	/// `None` means it is gone — the server does not have it and neither do we any more, which is
	/// the provider's `.noSuchItem`. Only a not-found from the server means that: every other
	/// failure is reported as itself, so a connectivity problem never reads as a deletion.
	///
	/// The identifiers are provider-oriented, and so is that answer — with one exception, a BARE
	/// uuid. That form names a directory, and a uuid is the one question the server takes, so an
	/// unknown one is asked about once before it may read as a deletion: a directory this cache has
	/// never listed is not a directory that does not exist. Every other unknown id returns
	/// `Ok(None)` without going near the network. There is nothing to ask the server about —
	/// `stable/<id>` is a namespace only this cache hands out, and the row that would translate it
	/// into something the server knows is exactly what is missing. A path-form id against a cold
	/// cache lands the same way: the walk stops at the first name it has never listed.
	pub async fn update_and_query_item(&self, id: FfiId) -> Result<Option<FfiObject>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.update_and_query_item(id).await
		})
		.await
	}

	/// The child `name` names under `parent`, refreshed from the server — `None` when the fresh
	/// listing has no such child. This is the match step a reimport needs: the system re-creates
	/// every cached item with `.mayAlreadyExist`, and the provider must ADOPT the existing server
	/// item for that (parent, name) instead of re-uploading whatever bytes this replica holds —
	/// an upload silently replaces the server head, rolling back edits made elsewhere.
	pub async fn update_and_query_child(
		&self,
		parent: FfiId,
		name: String,
	) -> Result<Option<FfiObject>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.update_and_query_child(parent, name).await
		})
		.await
	}

	/// The BLAKE3 hash of the file at `os_path`, streamed off the blocking pool. The server
	/// stamps every uploaded file with the BLAKE3 of its plaintext bytes
	/// ([`FfiFileMeta::hash`](crate::ffi::FfiFileMeta)), so this is how a provider compares a
	/// local copy against a server head — the reimport adopt path — without moving any bytes.
	pub async fn blake3_hash_file(&self, os_path: String) -> Result<Vec<u8>, CacheError> {
		crate::env::get_runtime()
			.spawn_blocking(move || -> Result<Vec<u8>, CacheError> {
				let file = std::fs::File::open(&os_path)?;
				let mut hasher = blake3::Hasher::new();
				std::io::copy(&mut std::io::BufReader::new(file), &mut hasher)?;
				Ok(hasher.finalize().as_bytes().to_vec())
			})
			.await
			.map_err(|e| CacheError::io(format!("hashing task failed: {e}")))?
	}

	/// Retries every local edit that has not reached the server yet.
	///
	/// A failed upload leaves the edit marked in the cache, so it survives the extension being
	/// torn down. Nothing drains those markers on its own — call this when the provider or the app
	/// starts up, and after regaining connectivity. Returns how many uploads succeeded.
	pub async fn retry_pending_uploads(&self) -> Result<u32, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.retry_pending_uploads().await
		})
		.await
	}

	pub async fn upload_file_if_changed(
		&self,
		path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<bool, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.upload_file_if_changed(path, progress_callback)
				.await
		})
		.await
	}

	pub async fn upload_new_file(
		&self,
		os_path: String,
		parent_path: FfiId,
		info: UploadFileInfo,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FileWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.upload_new_file(os_path, parent_path, info, progress_callback, None)
				.await
		})
		.await
	}

	/// [`FilenMobileCacheState::upload_new_file`], cancellable.
	///
	/// A sibling rather than a parameter on the original, whose signature the shipped Android
	/// bindings call. Aborting gives up on the upload, which may leave chunks on the server that
	/// belong to no file — those the server collects — and never leaves a row here: nothing is
	/// written until the upload has actually produced a file.
	pub async fn upload_new_file_abortable(
		&self,
		os_path: String,
		parent_path: FfiId,
		info: UploadFileInfo,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<FileWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.upload_new_file(os_path, parent_path, info, progress_callback, abort)
				.await
		})
		.await
	}

	pub async fn create_empty_file(
		&self,
		parent_path: FfiId,
		name: String,
		mime: Option<String>,
	) -> Result<CreateFileResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.create_empty_file(parent_path, name, mime).await
		})
		.await
	}

	pub async fn create_dir(
		&self,
		parent_path: FfiId,
		name: String,
		created: Option<i64>,
	) -> Result<DirWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.create_dir(parent_path, name, created).await
		})
		.await
	}

	pub async fn trash_item(&self, path: FfiId) -> Result<ObjectWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| auth_state.trash_item(path).await)
			.await
	}

	pub async fn restore_item(
		&self,
		uuid: &str,
		to: Option<FfiId>,
	) -> Result<ObjectWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.restore_item(uuid, to).await
		})
		.await
	}

	pub async fn move_item(
		&self,
		item: FfiId,
		new_parent: FfiId,
	) -> Result<ObjectWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.move_item(item, new_parent).await
		})
		.await
	}

	pub async fn rename_item(
		&self,
		item: FfiId,
		new_name: String,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.rename_item(item, new_name).await
		})
		.await
	}

	pub async fn clear_local_cache(&self, item: FfiId) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.clear_local_cache(item).await
		})
		.await
	}

	pub async fn clear_local_cache_by_uuid(&self, uuid: &str) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.clear_local_cache_by_uuid(uuid).await
		})
		.await
	}

	pub async fn delete_item(&self, item: FfiId) -> Result<(), CacheError> {
		self.async_execute_authed_owned(async move |auth_state| auth_state.delete_item(item).await)
			.await
	}

	pub async fn set_favorite_rank(
		&self,
		item: FfiId,
		favorite_rank: i64,
	) -> Result<ObjectWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.set_favorite_rank(item, favorite_rank).await
		})
		.await
	}
}

impl AuthCacheState {
	pub(crate) async fn update_roots_info(&self) -> Result<(), CacheError> {
		debug!(
			"Updating roots info for client: {}",
			self.client.root().uuid()
		);
		let resp = self.client.get_user_info().await?;
		let conn = self.conn();
		sql::update_root(&conn, self.client.root().uuid(), &resp)?;
		Ok(())
	}

	pub(crate) async fn update_dir_children(&self, path: &FfiId) -> Result<(), CacheError> {
		debug!("Updating directory children for path: {}", path.0);
		// `stable/<id>` names ONE row: resolve it row-direct (see `resolve_stable_object`),
		// exactly as the destructive and byte-serving operations already do. The display-path
		// walk this replaces re-validated every ANCESTOR with a round trip of its own —
		// `get_dir` per cached component — on every presentation of the folder, warm or cold,
		// because the enumerator hands back a nil sync anchor and re-presentation is its only
		// refresh channel. Depth D cost D+1 requests; this costs 2.
		//
		// The authoritative not-found the walk produced survives: `resolve_stable_object`
		// refreshes the container through its own `get_dir`, and only a typed `FolderNotFound`
		// (via `forget_item`) resolves to `None` — which raises the same `DoesNotExist` the
		// `Partial` arm below does, i.e. `.noSuchItem` on iOS. The one case `forget_item` keeps
		// is a container sheltering an unsent edit; the relist below then fails as a remote
		// error, which is right — those bytes exist nowhere else, and `.noSuchItem` would have
		// the system delete the folder holding them.
		let mut dir: DBDirObject = if path.0.starts_with(STABLE_PREFIX) {
			self.resolve_stable_object(path).await?.try_into()?
		} else {
			let path = self.canonicalize_id(path)?;
			let path_id = path.as_path()?;
			match self.update_items_in_path(&path_id).await? {
				UpdateItemsInPath::Complete(dbobject) => dbobject.try_into()?,
				// Same reasoning as `resolve_file_to_download`'s Partial arm: the walk
				// REACHED the server and the path stopped resolving, so the directory is
				// gone rather than unreachable (a transport failure propagates as its own
				// error above). A remote error here read as `.serverUnreachable` on iOS,
				// and since the enumerator's existence probe is a local cache read, every
				// enumeration of a remotely-deleted directory took this path and retried
				// instead of learning `.noSuchItem`.
				UpdateItemsInPath::Partial(_, _) => {
					return Err(CacheError::DoesNotExist(
						format!("Path {} no longer resolves to an item", path_id.full_path).into(),
					));
				}
			}
		};
		self.inner_update_dir(&mut dir).await?;
		Ok(())
	}

	pub(crate) async fn update_recents(&self) -> Result<(), CacheError> {
		let (dirs, files) = self
			.client
			.list_recents(None::<&fn(u64, Option<u64>)>)
			.await?;
		debug!("Updating recents with {dirs:?} dirs and {files:?} files");
		sql::update_recents(&mut self.conn(), dirs, files)?;
		self.last_recents_update
			.write()
			.unwrap()
			.replace(Instant::now());
		Ok(())
	}

	pub(crate) async fn update_trash(&self) -> Result<(), CacheError> {
		// Held across fetch AND apply — see `AuthCacheState::listing_barrier`.
		let _listing = self.listing_barrier.read().await;
		let (dirs, files) = self
			.client
			.list_trash(None::<&fn(u64, Option<u64>)>)
			.await?;
		debug!("Updating trash with {dirs:?} dirs and {files:?} files");
		// A cached trashed item absent from the fresh trash listing was either RESTORED on
		// another device or permanently deleted. The sweep treats absence as deletion — an
		// authoritative tombstone for a file that may be alive and well under its restored
		// parent. Disambiguate first: an item the server still knows is upserted under its live
		// parent (the restore lands immediately), a definitive not-found is left for the sweep,
		// and an unverifiable one is spared this round.
		let spare = self.reconcile_missing_trashed(&dirs, &files).await?;
		sql::update_trashed_items(&mut self.conn(), dirs, files, &spare)?;
		self.last_trash_update
			.write()
			.unwrap()
			.replace(Instant::now());
		Ok(())
	}

	/// The trash-listing counterpart of [`Self::reconcile_missing_subdirs`]: cached trashed rows
	/// missing from the fresh trash listing are asked about by identity — a dir by its uuid, a
	/// file by its whole-life id (a restore does not re-mint either).
	async fn reconcile_missing_trashed(
		&self,
		listed_dirs: &[RemoteDirectory],
		listed_files: &[RemoteFile],
	) -> Result<Vec<Uuid>, CacheError> {
		let cached = sql::select_trash(&self.conn(), None)?;
		if cached.is_empty() {
			return Ok(Vec::new());
		}
		let listed: HashSet<Uuid> = listed_dirs
			.iter()
			.map(|dir| dir.uuid())
			.chain(listed_files.iter().map(|file| file.uuid()))
			.collect();
		let mut missing: Vec<DBNonRootObject> = cached
			.into_iter()
			.filter(|item| !listed.contains(&item.uuid()))
			.collect();
		if missing.is_empty() {
			return Ok(Vec::new());
		}
		// Above the cap, probe a BOUNDED chunk and SPARE the remainder rather than trusting the
		// listing: from here, a bulk restore-from-trash and an emptied trash produce the same
		// signature, and trusting the listing would tombstone every restored item. Sparing
		// converges instead — each presentation resolves another chunk.
		let mut spare: Vec<Uuid> = Vec::new();
		if missing.len() > RECONCILE_PROBE_CAP {
			tracing::warn!(
				"{} cached trashed rows vanished from one trash listing; probing {} of them and \
				 sparing the rest this round",
				missing.len(),
				RECONCILE_PROBE_CAP
			);
			spare.extend(
				missing[RECONCILE_PROBE_CAP..]
					.iter()
					.map(|item| item.uuid()),
			);
			missing.truncate(RECONCILE_PROBE_CAP);
		}
		enum Probe {
			Dir(Result<RemoteDirectory, filen_sdk_rs::error::Error>),
			File(Result<RemoteFile, filen_sdk_rs::error::Error>),
		}
		let probed: Vec<(Uuid, Probe)> =
			futures::stream::iter(missing.into_iter().map(|item| async move {
				let uuid = item.uuid();
				match item {
					DBNonRootObject::Dir(_) => (uuid, Probe::Dir(self.client.get_dir(uuid).await)),
					DBNonRootObject::File(file) => (
						uuid,
						Probe::File(self.client.get_file_by_stable_uuid(file.stable_uuid).await),
					),
				}
			}))
			.buffer_unordered(RECONCILE_PROBE_CONCURRENCY)
			.collect()
			.await;
		for (uuid, probe) in probed {
			match probe {
				// Every Ok answer is SPARED as well as applied: the sweep that follows re-marks
				// everything in its scope and deletes what the fresh listing does not contain —
				// which is exactly these rows. Without the spare, the probe's own proof of life
				// was upserted and then swept, tombstoning an item the server just confirmed
				// exists. A row the upsert moved out of the sweep's scope ignores the unmark.
				Probe::Dir(Ok(remote_dir)) => {
					debug!("trashed dir {uuid} is alive on the server; keeping it");
					DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
					spare.push(uuid);
				}
				Probe::File(Ok(head)) => {
					debug!("trashed file {uuid} is alive on the server; keeping it");
					// Spare the row the upsert just wrote, not the uuid we probed with: the
					// upsert's stable tier can re-mint the row's uuid, and the unmark matches
					// by uuid only — sparing the stale uuid would sweep the row the probe just
					// proved alive.
					let file = DBFile::upsert_from_remote(&mut self.conn(), head)?;
					spare.push(file.uuid);
				}
				Probe::Dir(Err(e)) if e.kind() == ErrorKind::FolderNotFound => {
					// The lineage is permanently gone, and any unsent edit below it has lost
					// its upload target. The descendant markers are also what spare this dir
					// from the sweep every round — left in place, the phantom never converges:
					// re-spared, re-probed, and re-failed on every refresh forever.
					let released = sql::clear_pending_upload_subtree(&self.conn(), uuid)?;
					debug!(
						"trashed dir {uuid} is permanently gone; released {released} pending \
						 marker(s) below it for the sweep"
					);
				}
				Probe::File(Err(e)) if e.kind() == ErrorKind::FileNotFound => {
					// The lineage is permanently gone — an unsent edit's upload target no longer
					// exists, so the marker's promise is undeliverable. Release it, or the
					// pending guard would spare this row every round forever: a phantom nothing
					// can drain (the drain skips trashed rows), restore, or delete. The staged
					// bytes stay in the slot until the budget sweep reclaims them. Released on
					// the probed row itself: the stable-scoped clear prefers a live duplicate,
					// which is exactly the row whose marker must survive.
					let released = sql::clear_pending_upload_by_uuid(&self.conn(), uuid)?;
					debug!(
						"trashed file {uuid} is permanently gone; released {released} pending \
						 marker(s) for the sweep"
					);
				}
				Probe::Dir(Err(e)) | Probe::File(Err(e)) => {
					debug!("could not verify missing trashed item {uuid} ({e}); sparing it");
					spare.push(uuid);
				}
			}
		}
		Ok(spare)
	}

	pub(crate) async fn update_and_query_dir_children(
		&self,
		path: FfiId,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		debug!(
			"Updating and querying directory children for path: {}",
			path.0
		);
		self.update_dir_children(&path).await?;
		self.query_dir_children(&path, order_by)
	}

	/// One page of a directory's children. `refresh` gates the server relist — Filen's dir
	/// listing has no server-side cursor, so a paging enumerator relists once on its first
	/// page and serves the rest from the cache the relist just wrote.
	pub(crate) async fn update_and_query_dir_children_page(
		&self,
		path: FfiId,
		order_by: Option<String>,
		offset: u32,
		limit: u32,
		refresh: bool,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		debug!(
			"Updating and querying directory children page for path: {} (offset {offset}, limit {limit}, refresh {refresh})",
			path.0
		);
		if refresh {
			self.update_dir_children(&path).await?;
		}
		self.query_dir_children_page(&path, order_by, offset, limit)
	}

	pub(crate) async fn update_and_query_recents(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		debug!("Updating and querying recents with order by: {order_by:?}");
		self.update_recents().await?;
		self.query_recents(order_by)
	}

	/// The file an id names, as the cache held it before this call and as the server has it now —
	/// the shared front half of the download calls.
	async fn resolve_file_to_download(
		&self,
		id: &FfiId,
	) -> Result<(Option<DBFile>, DBFile), CacheError> {
		// Identity-form (`stable/`) ids resolve row-direct (see `resolve_stable_object`):
		// serving bytes through a rebuilt display path can hand out a same-named sibling's
		// content.
		if id.0.starts_with(STABLE_PREFIX) {
			let old_file = match self.select_object_by_id(id)? {
				Some(DBObject::File(file)) => Some(file),
				_ => None,
			};
			return match self.resolve_stable_object(id).await? {
				DBObject::File(file) => Ok((old_file, file)),
				_ => Err(CacheError::remote(format!(
					"Id {id} does not point to a file"
				))),
			};
		}
		let file_path = self.canonicalize_id(id)?;
		let path_values = file_path.as_path()?;
		let old_file = match sql::select_object_at_path(&self.conn(), &path_values)? {
			Some(DBObject::File(file)) => Some(file),
			Some(_) => None,
			None => None,
		};

		match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => Ok((old_file, file)),
			// The walk reached the server and the path stops resolving before the file: the item
			// is GONE, not unreachable (a transport failure propagates as its own error above).
			// Reporting this as a remote error read as `.serverUnreachable` on iOS, and the
			// system retried the doomed download forever instead of learning `.noSuchItem`.
			UpdateItemsInPath::Partial(_, _) => Err(CacheError::DoesNotExist(
				format!(
					"Path {} no longer resolves to an item",
					path_values.full_path
				)
				.into(),
			)),
			UpdateItemsInPath::Complete(_) => Err(CacheError::remote(format!(
				"Path {} does not point to a file",
				path_values.full_path
			))),
		}
	}

	pub(crate) async fn download_file_if_changed_by_path(
		&self,
		file_path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		debug!("Downloading file to path: {}", file_path.0);
		let (old_file, file) = self.resolve_file_to_download(&file_path).await?;
		self.inner_download_file_if_changed(old_file, file, progress_callback, None)
			.await
	}

	pub(crate) async fn download_file_if_changed_with_item(
		&self,
		id: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<DownloadResponse, CacheError> {
		debug!("Downloading file with item at: {}", id.0);
		// Before the refresh below, which is already a server round trip: a call cancelled before
		// it started must not go near the network at all.
		check_not_aborted(abort.as_ref())?;
		let (old_file, file) = self.resolve_file_to_download(&id).await?;
		let uuid = file.uuid;
		let path = self
			.inner_download_file_if_changed(old_file, file, progress_callback, abort)
			.await?;
		// Re-read rather than answer from the row we walked in with: a download that finds the
		// local bytes gone drops the pending-upload marker, and the item handed back must not
		// still claim an edit that is no longer outstanding.
		let file = DBFile::select(&self.conn(), uuid)
			.optional()?
			.ok_or_else(|| {
				CacheError::DoesNotExist(
					format!("No file with UUID {uuid} after downloading it").into(),
				)
			})?;
		Ok(DownloadResponse {
			path,
			file: file.into(),
		})
	}

	pub(crate) async fn download_file_if_changed_by_uuid(
		&self,
		uuid: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		debug!("Downloading file with UUID: {uuid}");
		let uuid = self.resolve_uuid_or_stable(&uuid)?;
		let file = DBFile::select(&self.conn(), uuid)
			.optional()?
			.ok_or_else(|| CacheError::remote(format!("No file found with UUID: {uuid}")))?;
		// unnecesssary clone but better than redownloading
		self.inner_download_file_if_changed(Some(file.clone()), file, progress_callback, None)
			.await
	}

	/// Sends a cached file's local bytes to the server as a new version of itself.
	///
	/// `None` means there was nothing to send — the server already holds these bytes — and any
	/// marker left by an earlier failed attempt has been dropped. The guard that comes back covers
	/// the uuid the file now lives under on disk, which nothing knows about until the caller
	/// writes its row; the sweep deletes exactly those, so it has to outlive that write.
	async fn upload_edited_file(
		&self,
		file: DBFile,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<Option<(RemoteFile, Option<tokio::sync::OwnedMutexGuard<()>>)>, CacheError> {
		let DBFileMeta::Decoded(meta) = &file.meta else {
			return Err(CacheError::remote(format!(
				"File {} does not have decoded metadata",
				file.uuid
			)));
		};
		// Held across the whole check-then-upload: io_upload_updated_file reads the cached copy
		// and then renames it away under the newly minted uuid, so without this a concurrent
		// clear or download of the same item interleaves with it.
		let local_file_guard = self.lock_local_file(file.uuid).await;
		if let Some(hash) = meta.hash {
			let local_hash = self.hash_local_file(file.uuid, Some(&meta.name)).await?;
			if local_hash == Some(hash.into()) {
				// Already on the server: clear any marker a previous failed attempt left, so a
				// drain does not keep retrying an edit that has since landed.
				sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
				return Ok(None);
			}
		}

		// Marked BEFORE the attempt, so an upload interrupted by the process dying is still known
		// to be outstanding. Cleared only once the bytes are on the server.
		sql::mark_pending_upload(
			&self.conn(),
			file.stable_uuid,
			chrono::Utc::now().timestamp_millis(),
		)?;

		let uploaded = self
			.upload_marked_edit(&file, meta, &local_file_guard, progress_callback)
			.await?;
		sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
		Ok(Some(uploaded))
	}

	/// Sends the bytes sitting in a file's cache slot to the server as a new version of it.
	///
	/// The tail both edit paths share, and it takes the guard because the caller must already
	/// hold it: these locks are not reentrant, and the path that copies external bytes in has to
	/// hold one lock across copy, hash check and upload alike. The file's pending marker must
	/// already be set for the same reason — from the moment the slot holds bytes the server does
	/// not have, that marker is the only record of them. Clearing it belongs to the caller, once
	/// the row it upserts says where those bytes now live.
	async fn upload_marked_edit(
		&self,
		file: &DBFile,
		meta: &DBDecryptedFileMeta,
		_local_file_guard: &tokio::sync::OwnedMutexGuard<()>,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<(RemoteFile, Option<tokio::sync::OwnedMutexGuard<()>>), CacheError> {
		Ok(self
			.io_upload_updated_file(
				file.uuid,
				meta.name.clone(),
				file.parent.try_into().map_err(|e| {
					CacheError::conversion(format!("Failed to convert parent UUID: {e}"))
				})?,
				meta.mime.clone(),
				progress_callback,
			)
			.await?)
	}

	/// The file a stable id names, as the cache currently holds it.
	fn select_file_by_stable(&self, stable_uuid: Uuid) -> Result<DBFile, CacheError> {
		let conn = self.conn();
		let item = RawDBItem::select_by_stable(&conn, stable_uuid)?.ok_or_else(|| {
			CacheError::DoesNotExist(format!("No item for stable id: {stable_uuid}").into())
		})?;
		DBFile::select(&conn, item.uuid).optional()?.ok_or_else(|| {
			CacheError::DoesNotExist(format!("No file for stable id: {stable_uuid}").into())
		})
	}

	/// The item an FFI id names, as the cache currently holds it — no server round-trip.
	///
	/// A `stable/<id>` is resolved straight to its row, rather than through the display path
	/// [`AuthCacheState::canonicalize_id`] would build from it: that id names one row, while a
	/// path names a place, and same-named siblings share a place. Every other id form describes a
	/// location and is walked as one.
	pub(crate) fn select_object_by_id(&self, id: &FfiId) -> Result<Option<DBObject>, CacheError> {
		let conn = self.conn();
		if id.0.starts_with(STABLE_PREFIX) {
			let uuid = resolve_uuid_or_stable(&conn, &id.0)?;
			return Ok(DBObject::select(&conn, uuid).optional()?);
		}
		sql::select_object_at_parsed_id(&conn, &id.as_parsed()?)
	}

	/// Replaces a file's content with the bytes at `os_path` (see
	/// [`FilenMobileCacheState::modify_file_content`]).
	///
	/// The order below is the crash-safety contract, not a preference:
	/// take the item's lock once, mark the edit, only then let the bytes into the slot, and clear
	/// the marker only once a row records where they went. Between the marker and the upload, the
	/// bytes in the cache are the only copy of the user's edit, and the marker is what makes a
	/// drain retry them instead of a later cache probe silently concluding there was nothing to
	/// send.
	///
	/// Only the upload is abortable, and an abort there leaves the marker set and the new bytes in
	/// the slot — the same state a crash leaves, and the state the drain exists to resolve. The
	/// prefix that marks the edit and copies the bytes in is local and fast, and is not
	/// interruptible: cancelling a save must never mean reverting one.
	pub(crate) async fn modify_file_content(
		&self,
		id: FfiId,
		os_path: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<FileWithPathResponse, CacheError> {
		debug!("Modifying content of {} with the bytes at {os_path}", id.0);
		// Ahead of the marker and the copy, so a call cancelled before it started leaves nothing
		// of itself behind at all.
		check_not_aborted(abort.as_ref())?;
		let mut file = match self.select_object_by_id(&id)? {
			Some(DBObject::File(file)) => file,
			Some(_) => {
				return Err(CacheError::Unsupported(
					format!("Id {id} does not point to a file").into(),
				));
			}
			None => {
				return Err(CacheError::DoesNotExist(
					format!("No item found for id: {id}").into(),
				));
			}
		};
		let DBFileMeta::Decoded(meta) = &file.meta else {
			return Err(CacheError::remote(format!(
				"File {} does not have decoded metadata",
				file.uuid
			)));
		};

		// One guard for the whole edit. The copy, the hash check and the upload that renames the
		// slot away under a freshly minted uuid all touch this item's cache slot, and these locks
		// are not reentrant — which is why the upload is handed this guard instead of taking one.
		let local_file_guard = self.lock_local_file(file.uuid).await;

		// Read under the lock, before the marker moves: an import that fails has to leave behind
		// exactly what it found, and what it found may be an edit an earlier attempt marked and
		// could not deliver.
		let was_pending = self.has_pending_upload(file.stable_uuid)?;
		// Marked BEFORE the bytes land, and this is the whole reason this cannot be a copy
		// followed by upload_file_if_changed: in between, the only copy of the edit would sit in a
		// directory the size sweep evicts without taking any lock, with nothing recording that the
		// server is missing it — a later drain would find no local file and drop the edit as
		// though it had been dealt with.
		sql::mark_pending_upload(
			&self.conn(),
			file.stable_uuid,
			chrono::Utc::now().timestamp_millis(),
		)?;

		if let Err(e) = self
			.io_import_cached_file(file.uuid, Some(&meta.name), Path::new(&os_path))
			.await
		{
			// The slot never took the new bytes, so this call left nothing outstanding. A marker
			// standing over content the server already has costs a drain a pointless upload of a
			// byte-identical file — hash-less legacy files cannot short-circuit it. The crash
			// window the marker exists for is untouched: a crash never reaches this line, which is
			// exactly why the marker goes on first.
			if !was_pending {
				sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
			}
			return Err(e.into());
		}

		// The caller may well be handing back bytes it never changed — a provider saves on close,
		// edited or not. Files stored before the server recorded hashes have nothing to compare
		// against, so they always upload: a spared upload is worth less than a delivered edit.
		let unchanged = match meta.hash {
			Some(hash) => {
				self.hash_local_file(file.uuid, Some(&meta.name)).await? == Some(hash.into())
			}
			None => false,
		};
		if unchanged {
			// Nothing to send, so nothing is outstanding. The item still comes back whole: the
			// question asked was what the file now is, and "unchanged" is an answer about bytes.
			sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
			// Carried into the row we hand back, which was read before the marker moved — the
			// caller must not be told an edit is outstanding when the database says otherwise.
			file.pending_upload_at = None;
			return Ok(FileWithPathResponse {
				file: file.into(),
				id,
			});
		}

		let (remote_file, _new_uuid_guard) = with_abort(
			abort.as_ref(),
			self.upload_marked_edit(&file, meta, &local_file_guard, progress_callback),
		)
		.await?;
		// An upload that failed or was aborted returns above with the marker still set and the
		// bytes still in the slot: that pair IS the record of an edit the server has not got, and
		// the drain is what resolves it.
		let mut file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
		// The upload renamed the slot under the uuid the server minted for this version, so the
		// bytes belong to the row as it now stands.
		self.record_materialised(file.uuid);
		sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
		// The upsert read the row while it was still marked — this edit HAS reached the server.
		file.pending_upload_at = None;
		Ok(FileWithPathResponse {
			file: file.into(),
			// The id the caller named it by still names it: a content edit re-mints the file's
			// uuid but moves nothing and renames nothing.
			id,
		})
	}

	/// The item an id names, refreshed from the server (see
	/// [`FilenMobileCacheState::update_and_query_item`]).
	pub(crate) async fn update_and_query_item(
		&self,
		id: FfiId,
	) -> Result<Option<FfiObject>, CacheError> {
		Ok(self.resolve_object_refreshed(&id).await?.map(Into::into))
	}

	/// The item an id names as a [`DBObject`], refreshed from the server — the body of
	/// [`Self::update_and_query_item`], reused row-direct by the id-addressed operations.
	pub(crate) async fn resolve_object_refreshed(
		&self,
		id: &FfiId,
	) -> Result<Option<DBObject>, CacheError> {
		debug!("Updating and querying item: {}", id.0);
		// Held across fetch AND apply — see `AuthCacheState::listing_barrier`. Every branch below
		// upserts a server snapshot unconditionally, and `upsert_item.sql` overwrites the row's
		// parent, name and trashed flag with it: a snapshot fetched before a `folderMove` and
		// written after it silently reverts the move. Unlike a listing, that revert does not heal
		// — relisting the directory refreshes its CHILDREN, and nothing re-lists its parent.
		let _listing = self.listing_barrier.read().await;
		// An id that resolves to no row of ours is usually one we retired: the providers only ever
		// ask about identifiers they learned from this cache, so it named an item that has since
		// been deleted (or it is garbage, which answers the same way). There is nothing to refresh
		// either — an id is not something the server can be asked about, only a uuid is, and the
		// row that would say which one is exactly what is missing.
		//
		// A BARE uuid is the exception, because it IS a uuid: the providers use that form for
		// directories, and a directory can be one we have never listed rather than one we retired.
		// A tracked file moved remotely into a fresh directory lands a row whose parent has none,
		// and the replica then asks about that parent by uuid — answering `None` there would
		// report a directory that plainly exists as deleted. So ask the server, once, and let only
		// its not-found stand as the deletion.
		let obj = match self.select_object_by_id(id)? {
			Some(obj) => obj,
			None => return self.resolve_unknown_id(id).await,
		};
		match obj {
			DBObject::File(file) => self.refresh_file(file).await,
			DBObject::Dir(dir) => self.refresh_dir(dir).await,
			// A root is not something the server retires or moves; keeping its usage figures
			// current is update_roots_info's job.
			DBObject::Root(root) => Ok(Some(DBObject::Root(root))),
		}
	}

	/// The one row an identity-form (`stable/`) id names, resolved ROW-DIRECT and refreshed from
	/// the server — never through a rebuilt display path. A path names a place, and same-named
	/// siblings share a place: the server permits exact-name collisions (the dedup hash is
	/// client-supplied), and a decrypted name can be empty or contain '/' — a display-path walk
	/// then lands the operation on the WRONG row (or fails the round-trip entirely). The
	/// id-addressed destructive and byte-serving operations resolve through this instead.
	async fn resolve_stable_object(&self, id: &FfiId) -> Result<DBObject, CacheError> {
		self.resolve_object_refreshed(id)
			.await?
			.ok_or_else(|| CacheError::DoesNotExist(format!("No item found for id: {id}").into()))
	}

	/// The item an unknown identity-form id names, learned from the server (see
	/// [`AuthCacheState::update_and_query_item`], the only caller).
	///
	/// Both identity namespaces are probed — a bare uuid AND `stable/<id>`. The iOS provider
	/// issues every identifier in the `stable/` form, and for a directory that stable id IS its
	/// uuid (only files carry a distinct one), so refusing to probe the prefixed form made the
	/// probe unreachable for every id that provider actually sends: an unknown parent answered
	/// `None`, which the provider must treat as an authoritative deletion. The dir probe comes
	/// first (the common case is a tracked file's never-listed parent); a stable id the server
	/// does not know as a dir is then asked as a file lineage. Only the path forms stay unprobed —
	/// they describe locations only this cache can translate. The item lands through the same
	/// upsert a refresh uses, which leaves it exactly as a listing would have — including a parent
	/// we may still know nothing about, and which the next query resolves the same way, one level
	/// per ask.
	async fn resolve_unknown_id(&self, id: &FfiId) -> Result<Option<DBObject>, CacheError> {
		let (addressed_stable, bare) = match id.0.strip_prefix(STABLE_PREFIX) {
			Some(rest) => (true, rest),
			None => (false, id.0.as_str()),
		};
		let Ok(uuid) = bare.parse::<Uuid>() else {
			return Ok(None);
		};
		match self.client.get_dir(uuid).await {
			Ok(remote_dir) => {
				debug!("Learned unlisted dir {uuid} from the server");
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				return Ok(Some(DBObject::Dir(dir)));
			}
			// Not a directory the server knows — for the stable namespace that does not yet mean
			// gone: the id may name a file lineage, probed below.
			Err(e) if e.kind() == ErrorKind::FolderNotFound => {}
			// Every other failure is reported as itself: a connectivity problem must never read
			// as a deletion.
			Err(e) => return Err(e.into()),
		}
		if !addressed_stable {
			return Ok(None);
		}
		// Deserialization, not construction: the `stable/<id>` string is a value the foreign
		// caller previously received from this cache, arriving back through the FFI inside an
		// `FfiId` — the same boundary `StableUuid`'s uniffi lift sanctions, which cannot run here
		// because the id is embedded in a larger string.
		let stable_uuid: StableUuid =
			serde_json::from_value(serde_json::Value::String(bare.to_string()))
				.map_err(|e| CacheError::conversion(format!("invalid stable id in {id}: {e}")))?;
		let head = match self.client.get_file_by_stable_uuid(stable_uuid).await {
			Ok(head) => head,
			// The one answer that means gone: the server knows the id as neither dir nor file.
			Err(e) if e.kind() == ErrorKind::FileNotFound => return Ok(None),
			Err(e) => return Err(e.into()),
		};
		debug!("Learned unlisted file lineage {stable_uuid} from the server");
		let file = DBFile::upsert_from_remote(&mut self.conn(), head)?;
		Ok(Some(DBObject::File(file)))
	}

	/// The child `name` names under `parent` as of a fresh-enough listing of `parent` (see
	/// [`FilenMobileCacheState::update_and_query_child`]).
	///
	/// Row-direct, like every other id-addressed operation: the parent id resolves straight to
	/// its row (probing the server for an unknown one — a reimport typically runs against a
	/// FRESH cache, where a display-path build would throw instead of asking), and the child is
	/// selected by `(parent uuid, name)` rather than re-walking a rebuilt display path whose
	/// LIMIT-1 components land on same-named siblings.
	pub(crate) async fn update_and_query_child(
		&self,
		parent: FfiId,
		name: String,
	) -> Result<Option<FfiObject>, CacheError> {
		let Some(parent_obj) = self.resolve_object_refreshed(&parent).await? else {
			return Ok(None);
		};
		let mut parent_dir = DBDirObject::try_from(parent_obj).map_err(|e| {
			CacheError::remote(format!("Id {parent} does not point to a directory: {e}"))
		})?;
		// Skip the relist when one just ran: a reimport fires one create per cached child, and
		// relisting the same parent for each would be O(N²) network work — with every extra
		// relist another sweep window. A just-written listing is current enough for the adopt
		// decision, whose own hash comparison still guards content.
		let last_listed = match &parent_dir {
			DBDirObject::Dir(dir) => dir.last_listed,
			DBDirObject::Root(root) => root.last_listed,
		};
		if chrono::Utc::now().timestamp_millis() - last_listed > ADOPT_RELIST_TTL_MS {
			self.inner_update_dir(&mut parent_dir).await?;
		}
		Ok(sql::select_child_by_name(&self.conn(), parent_dir.uuid(), &name)?.map(Into::into))
	}

	/// The file behind a row, refreshed to its live head.
	///
	/// Asked by the stable id, not the uuid: a content edit made elsewhere re-mints the uuid, and
	/// a by-uuid question can only answer with the version this cache already holds (or with the
	/// short-lived trashed ghost a versioning-disabled edit leaves behind). By the whole-life id
	/// there is only ever one answer, and it is the head — so it is upserted unconditionally, and
	/// the stable tier lands it on this very row. A trashed head is still this item and comes back
	/// as one, carrying the parent a restore would put it back in.
	async fn refresh_file(&self, file: DBFile) -> Result<Option<DBObject>, CacheError> {
		let head = match self.client.get_file_by_stable_uuid(file.stable_uuid).await {
			Ok(head) => head,
			Err(e) if e.kind() == ErrorKind::FileNotFound => {
				return self.forget_item(DBObject::File(file)).await;
			}
			Err(e) => return Err(e.into()),
		};
		let file = DBFile::upsert_from_remote(&mut self.conn(), head)?;
		Ok(Some(DBObject::File(file)))
	}

	async fn refresh_dir(&self, dir: DBDir) -> Result<Option<DBObject>, CacheError> {
		let remote_dir = match self.client.get_dir(dir.uuid).await {
			Ok(remote_dir) => remote_dir,
			Err(e) if e.kind() == ErrorKind::FolderNotFound => {
				return self.forget_item(DBObject::Dir(dir)).await;
			}
			Err(e) => return Err(e.into()),
		};
		let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
		Ok(Some(DBObject::Dir(dir)))
	}

	/// Drops an item the server says it no longer has, bytes first.
	///
	/// Same order as the tail of [`AuthCacheState::delete_item`], and for the same reason: the
	/// bytes go first, then the row. Those bytes are a copy of the server's own content, so
	/// dropping them along with the item is the point — the row delete that follows is what
	/// retires the id for every replica, and a slot left behind would be a cached copy of
	/// something nothing names any more.
	pub(crate) async fn forget_item(&self, obj: DBObject) -> Result<Option<DBObject>, CacheError> {
		// An edit that has not reached the server outranks the server's answer: those bytes exist
		// nowhere else, and a drain can still land them. Deleting the row here would take the only
		// copy with it. The item stays until the drain resolves it, one way or the other.
		let holds_unsent_bytes = match &obj {
			DBObject::File(file) => self.has_pending_upload(file.stable_uuid)?,
			// A directory has no bytes of its own, but its row cannot go alone: the delete cascades
			// to everything under it, markers included, so an unsent edit anywhere below it is just
			// as much a reason to keep the directory as its own would be.
			DBObject::Dir(dir) => sql::has_descendant_pending_upload(&self.conn(), dir.uuid)?,
			// Unreachable — the root is never refreshed through here — and it is not something the
			// server retires anyway.
			DBObject::Root(_) => false,
		};
		if holds_unsent_bytes {
			tracing::warn!(
				"Item {} is gone from the server but still holds an unuploaded edit, keeping it",
				obj.uuid()
			);
			return Ok(Some(obj));
		}
		debug!("Item {} is gone from the server, dropping it", obj.uuid());
		self.io_delete_local(obj.uuid()).await?;
		sql::delete_item(&mut self.conn(), obj.uuid())?;
		Ok(None)
	}

	pub(crate) async fn upload_file_if_changed(
		&self,
		path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<bool, CacheError> {
		debug!("Uploading file at path: {}", path.0);
		// Read before canonicalization, which turns a stable id into a path and loses the fact
		// that the caller named a row rather than a location.
		let addressed_stable = addressed_stable_uuid(&path);
		let path = self.canonicalize_id(&path)?;
		let path_values = path.as_path()?;
		let (remote_file, _new_uuid_guard) = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => {
				match self.upload_edited_file(file, progress_callback).await? {
					Some(uploaded) => uploaded,
					None => return Ok(false),
				}
			}
			UpdateItemsInPath::Complete(_) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a file",
					path_values.full_path
				)));
			}
			// A stable id names an existing row, so there is nothing here to create: the path just
			// failed to resolve, because the name it was built from is a snapshot of a row the
			// server has since renamed, moved or dropped. Creating a file for it would upload the
			// EMPTY slot io_upload_new_file makes under that stale name, and the upsert's
			// `(parent, name)` tier would then merge that empty file onto the very row whose
			// unuploaded edit we were sent here to deliver — clearing its marker and reporting
			// success for bytes that were thrown away. Send the row's own bytes instead.
			UpdateItemsInPath::Partial(remaining, _)
				if remaining == path_values.name_or_uuid
					&& let Some(stable_uuid) = addressed_stable =>
			{
				let file = self.select_file_by_stable(stable_uuid)?;
				match self.upload_edited_file(file, progress_callback).await? {
					Some(uploaded) => uploaded,
					None => return Ok(false),
				}
			}
			UpdateItemsInPath::Partial(remaining, parent)
				if remaining == path_values.name_or_uuid =>
			{
				let mut builder = FileBuilderOptionalName::new(parent.uuid());
				builder.name(path_values.name_or_uuid)?;
				let (file, _, uuid_guard) = self.io_upload_new_file(builder).await?;
				(file, Some(uuid_guard))
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a file (remaining: {})",
					path_values.full_path, remaining
				)));
			}
		};

		let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
		// Whichever branch got here left the bytes in this file's own cache slot: a new file's is
		// where `io_upload_new_file` created it, an edited one's is where the rename moved it under
		// the freshly minted uuid.
		self.record_materialised(file.uuid);
		Ok(true)
	}

	pub(crate) async fn upload_new_file(
		&self,
		os_path: String,
		parent_path: FfiId,
		info: UploadFileInfo,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<FileWithPathResponse, CacheError> {
		check_not_aborted(abort.as_ref())?;
		let os_path = PathBuf::from(os_path);
		let name = info.name;
		let parent_path = self.canonicalize_id(&parent_path)?.into_owned();
		let out_path = parent_path.join(&name);
		debug!(
			"Creating file at path: {}, importing from {}",
			out_path.0,
			os_path.display()
		);
		let parent_pvs = parent_path.as_path()?;
		let parent = match self.update_items_in_path(&parent_pvs).await? {
			UpdateItemsInPath::Complete(DBObject::Dir(dir)) => DBDirObject::Dir(dir),
			UpdateItemsInPath::Complete(DBObject::Root(root)) => DBDirObject::Root(root),
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} points to a file"
				)));
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} does not point to a directory (remaining: {remaining})"
				)));
			}
		};

		let mut builder = FileBuilderOptionalName::new(parent.uuid());
		builder.name(&name)?;
		if let Some(creation) = info.creation {
			builder.created(DateTime::from_timestamp_millis(creation).ok_or_else(|| {
				CacheError::conversion(format!(
					"Failed to convert creation timestamp {creation} to DateTime"
				))
			})?);
		}
		if let Some(modification) = info.modification {
			builder.modified(
				DateTime::from_timestamp_millis(modification).ok_or_else(|| {
					CacheError::conversion(format!(
						"Failed to convert modification timestamp {modification} to DateTime"
					))
				})?,
			);
		}
		if let Some(mime) = info.mime {
			builder.mime(mime);
		}

		// The row below is the first and only trace this call leaves, and it is written from what
		// the upload returned — so giving up on the upload cannot leave a row for a file that
		// never came into being. The bytes are the caller's own file, not a cache slot, so there
		// is nothing on disk here to leave half-done either.
		let (remote_file, _) = with_abort(
			abort.as_ref(),
			self.io_upload_file(os_path, builder, progress_callback),
		)
		.await?;

		let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;

		Ok(FileWithPathResponse {
			id: out_path,
			file: file.into(),
		})
	}

	pub(crate) async fn create_empty_file(
		&self,
		parent_path: FfiId,
		name: String,
		mime: Option<String>,
	) -> Result<CreateFileResponse, CacheError> {
		let parent_path = self.canonicalize_id(&parent_path)?.into_owned();
		let file_path = parent_path.join(&name);
		debug!("Creating empty file at path: {}", file_path.0);
		let parent_pvs = parent_path.as_path()?;
		let parent = match self.update_items_in_path(&parent_pvs).await? {
			UpdateItemsInPath::Complete(DBObject::Dir(dir)) => DBDirObject::Dir(dir),
			UpdateItemsInPath::Complete(DBObject::Root(root)) => DBDirObject::Root(root),
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} points to a file"
				)));
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} does not point to a directory (remaining: {remaining})"
				)));
			}
		};

		let mut builder = FileBuilderOptionalName::new(parent.uuid());
		builder.name(&name)?;
		if let Some(mime) = mime {
			builder.mime(mime);
		}
		// Held until the row exists, so the sweep cannot mistake the new slot on disk for garbage.
		let (file, os_path, _uuid_guard) = self.io_upload_new_file(builder).await?;
		let file = DBFile::upsert_from_remote(&mut self.conn(), file)?;
		// The slot is the empty file the caller is about to write into, and it is in the cache
		// directory like any downloaded copy.
		self.record_materialised(file.uuid);
		Ok(CreateFileResponse {
			id: file_path,
			file: file.into(),
			path: os_path.into_os_string().into_string().map_err(|e| {
				CacheError::conversion(format!("Failed to convert path to string: {e:?}"))
			})?,
		})
	}

	pub(crate) async fn create_dir(
		&self,
		parent_path: FfiId,
		name: String,
		created: Option<i64>,
	) -> Result<DirWithPathResponse, CacheError> {
		let parent_path = self.canonicalize_id(&parent_path)?.into_owned();
		let dir_path = parent_path.join(&name);
		debug!("Creating directory at path: {}", dir_path.0);
		let path_values = parent_path.as_path()?;
		let parent = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::Dir(dir)) => DBDirObject::Dir(dir),
			UpdateItemsInPath::Complete(DBObject::Root(root)) => DBDirObject::Root(root),
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} points to a file"
				)));
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {parent_path} does not point to a directory (remaining: {remaining})"
				)));
			}
		};

		let parent_dir_type = DirType::<'static, Normal>::from(parent);
		let dir = match created {
			Some(time) => {
				self.client
					.create_dir_with_created(
						&parent_dir_type,
						&name,
						DateTime::from_timestamp_millis(time).ok_or_else(|| {
							CacheError::conversion(format!(
								"Failed to convert timestamp {time} to DateTime"
							))
						})?,
					)
					.await?
			}
			None => self.client.create_dir(&parent_dir_type, &name).await?,
		};

		let mut conn = self.conn();
		let dir = DBDir::upsert_from_remote(&mut conn, dir)?;
		Ok(DirWithPathResponse {
			dir: dir.into(),
			id: dir_path,
		})
	}

	pub(crate) async fn trash_item(
		&self,
		path: FfiId,
	) -> Result<ObjectWithPathResponse, CacheError> {
		debug!("Trashing item at path: {}", path.0);
		// `stable/<id>` names ONE row: resolve it row-direct (see `resolve_stable_object`)
		// instead of walking a rebuilt display path that can land on a same-named sibling.
		let obj = if path.0.starts_with(STABLE_PREFIX) {
			self.resolve_stable_object(&path).await?
		} else {
			let path = self.canonicalize_id(&path)?;
			let path_values: PathFfiId<'_> = path.as_path()?;
			match self.update_items_in_path(&path_values).await? {
				UpdateItemsInPath::Complete(dbobject) => dbobject,
				// Server-confirmed gone (transport failures propagate above): trashing an item
				// that no longer exists must answer `.noSuchItem`, not a retried "unreachable".
				UpdateItemsInPath::Partial(_, _) => {
					return Err(CacheError::DoesNotExist(
						format!(
							"Path {} no longer resolves to an item",
							path_values.full_path
						)
						.into(),
					));
				}
			}
		};

		// Already in the trash: the replay of a trash another device (or a prior attempt)
		// applied — the stable branch above refreshes to the live head, so a trashed item
		// arrives here as one. Idempotent success, mirroring restore_item's guard in the
		// opposite direction; erroring instead read as a transient failure the system
		// retried without bound.
		if obj.parent().is_some_and(|parent| parent.is_trash()) {
			return Ok(ObjectWithPathResponse {
				id: FfiId(format!("trash/{}", obj.uuid())),
				object: obj.into(),
			});
		}
		let obj = match obj {
			DBObject::Root(root) => {
				return Err(CacheError::remote(format!(
					"Cannot remove root directory: {}",
					root.uuid
				)));
			}
			DBObject::Dir(dir) => {
				let mut remote_dir = dir.into();
				self.client.trash_dir(&mut remote_dir).await?;
				self.io_delete_local(remote_dir.uuid()).await?;
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				DBObject::Dir(dir)
			}
			DBObject::File(file) => {
				let mut remote_file = file.try_into()?;
				self.client.trash_file(&mut remote_file).await?;
				self.io_delete_local(remote_file.uuid()).await?;
				let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
				// The local bytes are gone, so there is nothing left to upload — and the drain
				// skips trashed rows, so a marker left here would never be retried nor cleared.
				// Cleared on the row the upsert just wrote: the stable-scoped clear prefers a
				// live duplicate over this freshly-trashed row.
				sql::clear_pending_upload_by_uuid(&self.conn(), file.uuid)?;
				DBObject::File(file)
			}
		};
		Ok(ObjectWithPathResponse {
			id: FfiId(format!("trash/{}", obj.uuid())),
			object: obj.into(),
		})
	}

	pub(crate) async fn restore_item(
		&self,
		uuid: &str,
		to: Option<FfiId>,
	) -> Result<ObjectWithPathResponse, CacheError> {
		debug!("Untrashing item with UUID: {uuid} to parent: {to:?}");
		let uuid = self.resolve_uuid_or_stable(uuid)?;
		let object = {
			let conn = self.conn();
			// `.optional()`: a bare no-rows read used to surface as a raw SQL error, which iOS
			// maps to a transient retry — restoring an item the cache no longer knows then
			// retried forever instead of answering `.noSuchItem`.
			DBNonRootObject::select(&conn, uuid)
				.optional()?
				.ok_or_else(|| {
					CacheError::DoesNotExist(format!("No item to restore for uuid: {uuid}").into())
				})?
		};

		// we do this first to make sure we have a valid restore target
		let parent = match to {
			// An identity-form target resolves row-direct, like every other id-addressed
			// operation.
			Some(to_id) if to_id.0.starts_with(STABLE_PREFIX) => {
				let dir = DBDirObject::try_from(self.resolve_stable_object(&to_id).await?)
					.map_err(|e| {
						CacheError::remote(format!("Id {to_id} does not point to a directory: {e}"))
					})?;
				Some((dir, to_id))
			}
			Some(to_path) => {
				let to_path = self.canonicalize_id(&to_path)?.into_owned();
				let to_pvs: PathFfiId<'_> = to_path.as_path()?;
				match self.update_items_in_path(&to_pvs).await? {
					UpdateItemsInPath::Complete(DBObject::Dir(dir)) => {
						Some((DBDirObject::Dir(dir), to_path))
					}
					UpdateItemsInPath::Complete(DBObject::Root(root)) => {
						Some((DBDirObject::Root(root), to_path))
					}
					UpdateItemsInPath::Complete(DBObject::File(_)) => {
						return Err(CacheError::remote(format!(
							"Path {} points to a file",
							to_pvs.full_path
						)));
					}
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to a directory",
							to_pvs.full_path
						)));
					}
				}
			}
			None => None,
		};

		if !object.parent().is_some_and(|p| p.is_trash()) {
			return Err(CacheError::remote(format!(
				"Object with UUID {uuid} is not in the trash"
			)));
		}

		let object = match object {
			DBNonRootObject::File(file) => {
				let mut remote_file = file.try_into()?;
				self.client.restore_file(&mut remote_file).await?;
				let remote_file = self.client.get_file(remote_file.uuid()).await?;
				let mut conn = self.conn();
				let file = DBFile::upsert_from_remote(&mut conn, remote_file)?;
				DBNonRootObject::File(file)
			}
			DBNonRootObject::Dir(dir) => {
				let mut remote_dir: RemoteDirectory = dir.into();
				self.client.restore_dir(&mut remote_dir).await?;
				let remote_dir = self.client.get_dir(remote_dir.uuid()).await?;
				let mut conn = self.conn();
				let dir = DBDir::upsert_from_remote(&mut conn, remote_dir)?;
				DBNonRootObject::Dir(dir)
			}
		};

		if let Some((parent, _parent_path)) = parent
			&& object.certain_parent() != parent.uuid()
		{
			let item = self.inner_move_item(object, parent).await?;
			// A display path for the restored location when the cache can build one (the path
			// CTE already starts at the root). Joining the caller's target id with a uuid
			// produced a string valid in NO id namespace for identity-form targets
			// (`stable/<dir>/<uuid>`); the fallback is the item's own identity form instead.
			let id = sql::recursive_select_path_from_uuid(&self.conn(), item.uuid())?
				.map(FfiId)
				.unwrap_or_else(|| match &item {
					DBNonRootObject::File(file) => {
						FfiId(format!("{STABLE_PREFIX}{}", file.stable_uuid))
					}
					DBNonRootObject::Dir(dir) => FfiId(format!("{STABLE_PREFIX}{}", dir.uuid)),
				});
			return Ok(ObjectWithPathResponse {
				object: DBObject::from(item).into(),
				id,
			});
		}

		// The path CTE already starts at the root; prepending the root uuid again produced a
		// doubled first component.
		sql::recursive_select_path_from_uuid(&self.conn(), object.uuid())?
			.ok_or_else(|| {
				CacheError::remote(format!("Failed to get path for object with UUID {uuid}"))
			})
			.map(|s| ObjectWithPathResponse {
				id: FfiId(s),
				object: DBObject::from(object).into(),
			})
	}

	pub(crate) async fn move_item(
		&self,
		item: FfiId,
		new_parent: FfiId,
	) -> Result<ObjectWithPathResponse, CacheError> {
		debug!("Moving item {} to new parent {}", item.0, new_parent.0);
		// Identity-form (`stable/`) ids resolve row-direct on BOTH sides (see
		// `resolve_stable_object`); only the legacy path forms still walk a display path.
		let (obj, new_parent_dir) = futures::try_join!(
			async {
				if item.0.starts_with(STABLE_PREFIX) {
					return DBNonRootObject::try_from(self.resolve_stable_object(&item).await?)
						.map_err(|e| {
							CacheError::remote(format!(
								"Id {item} does not point to a non-root item: {e}"
							))
						});
				}
				let item = self.canonicalize_id(&item)?;
				let item_pvs: PathFfiId<'_> = item.as_path()?;
				let obj = match self.update_items_in_path(&item_pvs).await? {
					UpdateItemsInPath::Complete(obj) => {
						DBNonRootObject::try_from(obj).map_err(|e| {
							CacheError::remote(format!(
								"Path {} does not point to a non-root item: {}",
								item_pvs.full_path, e
							))
						})?
					}
					// Server-confirmed gone: moving an item that no longer exists answers
					// `.noSuchItem`, not a retried "unreachable".
					UpdateItemsInPath::Partial(remaining_path, _) => {
						return Err(CacheError::DoesNotExist(
							format!(
								"Path {} no longer resolves to an item, remaining: {}",
								item_pvs.full_path, remaining_path
							)
							.into(),
						));
					}
				};
				Ok(obj)
			},
			async {
				if new_parent.0.starts_with(STABLE_PREFIX) {
					return DBDirObject::try_from(self.resolve_stable_object(&new_parent).await?)
						.map_err(|e| {
							CacheError::remote(format!(
								"Id {new_parent} does not point to a directory: {e}"
							))
						});
				}
				let new_parent = self.canonicalize_id(&new_parent)?;
				let new_parent_pvs: PathFfiId<'_> = new_parent.as_path()?;
				match self.update_items_in_path(&new_parent_pvs).await? {
					UpdateItemsInPath::Complete(obj) => DBDirObject::try_from(obj).map_err(|e| {
						CacheError::remote(format!(
							"Path {} does not point to a directory: {}",
							new_parent_pvs.full_path, e
						))
					}),
					UpdateItemsInPath::Partial(remaining_path, _) => {
						Err(CacheError::remote(format!(
							"Path {} does not point to an item, remaining: {}",
							new_parent_pvs.full_path, remaining_path
						)))
					}
				}
			}
		)?;

		// A move of a TRASHED item is a restore: iOS decides .move vs .restore(to:) from an
		// unrefreshed local read, so a drag on a device that has not yet learned the item was
		// trashed elsewhere lands here. The plain move endpoint is not defined for trashed
		// items — route through the restore flow, which restores and then moves.
		if obj.parent().is_some_and(|parent| parent.is_trash()) {
			// Delegate with the RESOLVED row's uuid, not the caller's raw id: restore_item
			// parses a bare uuid (or stable/ form), and a legacy path-form id would die in the
			// parse after this branch already resolved it.
			return self
				.restore_item(&obj.uuid().to_string(), Some(new_parent))
				.await;
		}
		let obj = self.inner_move_item(obj, new_parent_dir).await?;
		// A display path for the new location when the cache can build one (the path CTE
		// already starts at the root); else the caller's own id, which stays valid — identity
		// survives a move.
		let id = sql::recursive_select_path_from_uuid(&self.conn(), obj.uuid())?
			.map(FfiId)
			.unwrap_or(item);
		Ok(ObjectWithPathResponse {
			object: DBObject::from(obj).into(),
			id,
		})
	}

	pub(crate) async fn rename_item(
		&self,
		item: FfiId,
		new_name: String,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		debug!("Renaming item {} to {}", item.0, new_name);
		// Identity-form (`stable/`) ids resolve row-direct (see `resolve_stable_object`): the
		// old walk relisted the parent and re-selected by display path, which lands on a
		// same-named sibling when names collide.
		let obj = if item.0.starts_with(STABLE_PREFIX) {
			let obj = DBNonRootObject::try_from(self.resolve_stable_object(&item).await?).map_err(
				|e| CacheError::remote(format!("Id {item} does not point to a non-root item: {e}")),
			)?;
			// The replay of an already-applied rename: nothing to do, and not an error.
			if obj.name() == Some(new_name.as_str()) {
				return Ok(None);
			}
			obj
		} else {
			let canonical = self.canonicalize_id(&item)?.into_owned();
			let item_pvs: PathFfiId<'_> = canonical.as_path()?;
			if item_pvs.name_or_uuid.is_empty() {
				return Err(CacheError::remote(format!(
					"Cannot rename item: {}",
					canonical.0
				)));
			} else if item_pvs.name_or_uuid == new_name {
				return Ok(None);
			}
			self.update_dir_children(&canonical.parent()).await?;
			match sql::select_object_at_path(&self.conn(), &item_pvs)? {
				Some(obj) => DBNonRootObject::try_from(obj).map_err(|e| {
					CacheError::remote(format!(
						"Path {} does not point to a non-root item: {}",
						item_pvs.full_path, e
					))
				})?,
				None => {
					// The parent was relisted just above, so a missing row is server-confirmed
					// gone from this path — `.noSuchItem`, not a retried "unreachable".
					return Err(CacheError::DoesNotExist(
						format!("Path {} no longer resolves to an item", item_pvs.full_path).into(),
					));
				}
			}
		};
		let obj = match obj {
			DBNonRootObject::Dir(dbdir) => {
				let mut remote_dir: RemoteDirectory = dbdir.into();
				let changes = DirectoryMetaChanges::default().name(&new_name)?;
				self.client
					.update_dir_metadata(&mut remote_dir, changes)
					.await?;
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				DBObject::Dir(dir)
			}
			DBNonRootObject::File(dbfile) => {
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				let changes = FileMetaChanges::default().name(&new_name)?;
				self.client
					.update_file_metadata(&mut remote_file, changes)
					.await?;
				let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
				DBObject::File(file)
			}
		};
		// A display path for the renamed item when the cache can build one (the path CTE
		// already starts at the root); else the caller's own id, which stays valid — identity
		// survives a rename.
		let id = sql::recursive_select_path_from_uuid(&self.conn(), obj.uuid())?
			.map(FfiId)
			.unwrap_or(item);
		Ok(Some(ObjectWithPathResponse {
			object: obj.into(),
			id,
		}))
	}

	pub(crate) async fn clear_local_cache(&self, item: FfiId) -> Result<(), CacheError> {
		debug!("Clearing local cache for item: {}", item.0);
		// A purely local operation: an identity-form id selects its row directly (no server
		// refresh needed and no display-path rebuild that could hit a same-named sibling).
		let obj = if item.0.starts_with(STABLE_PREFIX) {
			self.select_object_by_id(&item)?
		} else {
			let item = self.canonicalize_id(&item)?;
			let pvs = item.as_path()?;
			sql::select_object_at_path(&self.conn(), &pvs)?
		};
		let Some(obj) = obj else {
			return Ok(());
		};
		self.io_delete_local(obj.uuid()).await?;
		Ok(())
	}

	/// Retries every file still marked as having unuploaded local changes.
	///
	/// Best effort and independent per file: one that still fails keeps its marker for the next
	/// drain rather than aborting the rest. Returns how many reached the server.
	pub(crate) async fn retry_pending_uploads(&self) -> Result<u32, CacheError> {
		// A remote edit may have re-minted a marked row's uuid, leaving its bytes in a slot the
		// row no longer names. Reunite them first, or the "no local copy" check below reads a
		// recoverable edit as a resolved one.
		self.rescue_reminted_slots().await;
		let pending = sql::select_pending_uploads(&self.conn())?;
		if pending.is_empty() {
			return Ok(0);
		}
		debug!("Retrying {} pending upload(s)", pending.len());

		let mut uploaded = 0;
		for stable_uuid in pending {
			// A marked file whose local copy is gone has nothing left to upload — the cache was
			// cleared, or the item was evicted. Without this it would take the "content differs"
			// branch below and fail forever trying to read a file that is not there, keeping its
			// marker and its log noise for good.
			if !self.has_local_copy(stable_uuid).await? {
				// Unless the rescue above could not finish: an unrescued remint means the bytes
				// are still on disk under a retired uuid, and dropping the marker here is
				// exactly the silent loss the log exists to prevent. Keep it for the next drain.
				// Two SEPARATE statements: the connection guard is a temporary, and a second
				// `conn()` inside the same statement deadlocks on the non-reentrant mutex.
				let row = RawDBItem::select_by_stable(&self.conn(), stable_uuid.into())?;
				let unrescued = match row {
					Some(item) => sql::slot_remint_targets_uuid(&self.conn(), item.uuid)?,
					None => false,
				};
				if unrescued {
					tracing::warn!(
						"Pending upload for {stable_uuid} still has an unrescued reminted slot; \
						 keeping the marker"
					);
					continue;
				}
				debug!(
					"Pending upload for {stable_uuid} has no local file left, dropping the marker"
				);
				sql::clear_pending_upload(&self.conn(), stable_uuid)?;
				continue;
			}

			// Addressed through the stable namespace: the file may have been renamed or moved
			// since the edit, and a name path would no longer find it.
			let id = FfiId(format!("stable/{stable_uuid}"));
			match self.upload_file_if_changed(id, None).await {
				Ok(_) => uploaded += 1,
				Err(e) => {
					// Trashing clears the marker and deletes the local bytes, so an item trashed
					// between the check above and here fails the upload for a reason that is the
					// system working. Warning about it would report an edit at risk that is not.
					if self.was_trashed(stable_uuid) {
						debug!(
							"Pending upload for {stable_uuid} was trashed mid-drain, not retrying"
						);
					} else {
						tracing::warn!(
							"Pending upload for {stable_uuid} failed again, still marked: {e}"
						);
					}
				}
			}
		}
		Ok(uploaded)
	}

	/// Whether the file behind a stable id still has an edit that has not reached the server.
	///
	/// Queried rather than read off a cached row: callers act on this under the per-item lock, and
	/// a row read before that lock was taken cannot see a marker written while it was held.
	fn has_pending_upload(&self, stable_uuid: StableUuid) -> Result<bool, CacheError> {
		Ok(sql::select_pending_upload_at(&self.conn(), stable_uuid)?.is_some())
	}

	/// Whether the item behind a stable id has since been trashed. Best effort: a lookup failure
	/// reports `false`, which only means the caller keeps its louder message.
	fn was_trashed(&self, stable_uuid: StableUuid) -> bool {
		RawDBItem::select_by_stable(&self.conn(), stable_uuid.into())
			.ok()
			.flatten()
			.is_some_and(|item| matches!(item.parent, Some(ParentUuid::Trash(_))))
	}

	/// Whether a cached copy of the file is still on disk.
	///
	/// Keyed on the stable id, because that is what the pending markers are keyed on — while the
	/// cached copy lives under the file's CURRENT uuid, which an edit re-mints. Looking the row up
	/// by the stable id first is what keeps the two in step.
	async fn has_local_copy(&self, stable_uuid: StableUuid) -> Result<bool, CacheError> {
		let Some(item) = RawDBItem::select_by_stable(&self.conn(), stable_uuid.into())? else {
			return Ok(false);
		};
		let Some(file) = DBFile::select(&self.conn(), item.uuid).optional()? else {
			return Ok(false);
		};
		let name = match &file.meta {
			DBFileMeta::Decoded(meta) => Some(meta.name.clone()),
			_ => None,
		};
		Ok(self
			.hash_local_file(file.uuid, name.as_deref())
			.await?
			.is_some())
	}

	/// How many files have local changes that have not reached the server.
	pub(crate) fn pending_upload_count(&self) -> Result<u32, CacheError> {
		Ok(u32::try_from(sql::select_pending_uploads(&self.conn())?.len()).unwrap_or(u32::MAX))
	}

	pub(crate) async fn clear_local_cache_by_uuid(&self, uuid: &str) -> Result<(), CacheError> {
		debug!("Clearing local cache for item with uuid: {uuid}");
		let obj =
			match DBObject::select(&self.conn(), self.resolve_uuid_or_stable(uuid)?).optional()? {
				Some(obj) => obj,
				None => return Ok(()),
			};
		self.io_delete_local(obj.uuid()).await?;
		Ok(())
	}

	pub(crate) async fn delete_item(&self, item: FfiId) -> Result<(), CacheError> {
		debug!("Deleting object at path: {}", item.0);
		// Identity-form (`stable/`) ids resolve row-direct (see `resolve_stable_object`) — a
		// PERMANENT delete through a rebuilt display path landing on a same-named sibling is the
		// worst possible wrong-row outcome. An id the server no longer knows deletes nothing and
		// succeeds, matching the Trash/Recents arm's tolerance below.
		if item.0.starts_with(STABLE_PREFIX) {
			let obj = match self.resolve_stable_object(&item).await {
				Ok(obj) => Some(obj),
				Err(CacheError::DoesNotExist(_)) => None,
				Err(e) => return Err(e),
			};
			return self.inner_delete_item(obj).await;
		}
		let item = self.canonicalize_id(&item)?;
		let pvs = item.as_parsed()?;
		let obj = match pvs {
			ParsedFfiId::Trash(uuid_id) | ParsedFfiId::Recents(uuid_id) => DBObject::select(
				&self.conn(),
				uuid_id.uuid.ok_or_else(|| {
					CacheError::Unsupported(
						format!("Cannot delete item at path: {}", item.0).into(),
					)
				})?,
			)
			.optional()?,
			ParsedFfiId::Path(path_values) => {
				Some(match self.update_items_in_path(&path_values).await? {
					UpdateItemsInPath::Complete(obj) => obj,
					// Server-confirmed gone: a delete of an already-gone item answers
					// `.noSuchItem` (which the system treats as "already deleted"), matching
					// the Trash/Recents arm's tolerance above instead of retrying forever.
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::DoesNotExist(
							format!("Path {} no longer resolves to an item", item.0).into(),
						));
					}
				})
			}
		};
		self.inner_delete_item(obj).await
	}

	/// The delete every resolution branch funnels into; `None` (nothing to delete) succeeds.
	async fn inner_delete_item(&self, obj: Option<DBObject>) -> Result<(), CacheError> {
		let Some(obj) = obj else {
			return Ok(());
		};

		match obj {
			DBObject::Root(_) => {
				return Err(CacheError::remote("Cannot delete root directory"));
			}
			DBObject::Dir(dir) => {
				self.io_delete_local(dir.uuid).await?;
				let remote_dir: RemoteDirectory = dir.into();
				let uuid = remote_dir.uuid();
				self.client.delete_dir_permanently(remote_dir).await?;
				sql::delete_item(&mut self.conn(), uuid)?;
				debug!("Successfully deleted dir {uuid}");
			}
			DBObject::File(file) => {
				self.io_delete_local(file.uuid).await?;
				let remote_file: RemoteFile = file.try_into()?;
				let uuid = remote_file.uuid();
				self.client.delete_file_permanently(remote_file).await?;
				sql::delete_item(&mut self.conn(), uuid)?;
				debug!("Successfully deleted file {uuid}");
			}
		}
		Ok(())
	}

	pub(crate) async fn set_favorite_rank(
		&self,
		item: FfiId,
		favorite_rank: i64,
	) -> Result<ObjectWithPathResponse, CacheError> {
		debug!(
			"Setting favorite rank for item: {}, rank: {}",
			item.0, favorite_rank
		);
		// Identity-form (`stable/`) ids select their row directly — a display-path rebuild can
		// hit a same-named sibling. No server refresh: the favorite call itself is the
		// authoritative operation, keyed on the row's uuid.
		if item.0.starts_with(STABLE_PREFIX) {
			let obj = self.select_object_by_id(&item)?.ok_or_else(|| {
				CacheError::DoesNotExist(format!("No item found for id: {item}").into())
			})?;
			let obj = self.inner_set_favorite_rank(obj, favorite_rank).await?;
			// Echo the same id family the display-path resolution produced: the trash form for
			// a trashed item (further operations address it there), the bare uuid for a root,
			// the caller's own identity form otherwise.
			let id = match &obj {
				DBObject::File(file) if file.parent.is_trash() => {
					FfiId(format!("trash/{}", file.uuid))
				}
				DBObject::Dir(dir) if dir.parent.is_trash() => FfiId(format!("trash/{}", dir.uuid)),
				DBObject::Root(root) => FfiId(root.uuid.to_string()),
				_ => item,
			};
			return Ok(ObjectWithPathResponse {
				object: obj.into(),
				id,
			});
		}
		let item = self.canonicalize_id(&item)?;
		let pvs = item.as_parsed()?;
		let obj = match pvs {
			ParsedFfiId::Trash(uuid_id) | ParsedFfiId::Recents(uuid_id) => DBObject::select(
				&self.conn(),
				uuid_id.uuid.ok_or_else(|| {
					CacheError::Unsupported(
						format!("Cannot set favorite rank for item at path: {}", item.0).into(),
					)
				})?,
			)
			.optional()?,
			ParsedFfiId::Path(path_values) => {
				Some(match self.update_items_in_path(&path_values).await? {
					UpdateItemsInPath::Complete(obj) => obj,
					// Server-confirmed gone — `.noSuchItem`, not a retried "unreachable".
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::DoesNotExist(
							format!("Path {} no longer resolves to an item", item.0).into(),
						));
					}
				})
			}
		}
		.ok_or_else(|| CacheError::remote(format!("No item found at path: {}", item.0)))?;
		let obj = self.inner_set_favorite_rank(obj, favorite_rank).await?;
		Ok(ObjectWithPathResponse {
			object: obj.into(),
			id: item.into_owned(),
		})
	}

	/// The favorite update every resolution branch funnels into.
	async fn inner_set_favorite_rank(
		&self,
		obj: DBObject,
		favorite_rank: i64,
	) -> Result<DBObject, CacheError> {
		let obj = match obj {
			DBObject::File(mut dbfile) if favorite_rank != dbfile.favorite_rank => {
				if (favorite_rank > 0) != (dbfile.favorite_rank > 0) {
					// update server-side favorite status
					let mut remote_file: RemoteFile = dbfile.try_into()?;
					self.client
						.set_file_favorite(&mut remote_file, favorite_rank > 0)
						.await?;
					dbfile = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
				}
				// update local favorite rank
				dbfile.update_favorite_rank(&self.conn(), favorite_rank)?;
				DBObject::File(dbfile)
			}
			DBObject::Dir(mut dbdir) if favorite_rank != dbdir.favorite_rank => {
				if (favorite_rank > 0) != (dbdir.favorite_rank > 0) {
					// update server-side favorite status
					let mut remote_dir: RemoteDirectory = dbdir.into();
					self.client
						.set_dir_favorite(&mut remote_dir, favorite_rank > 0)
						.await?;
					dbdir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				}
				// update local favorite rank
				dbdir.update_favorite_rank(&self.conn(), favorite_rank)?;
				DBObject::Dir(dbdir)
			}
			DBObject::Root(_) => {
				return Err(CacheError::remote(
					"Cannot set favorite rank for root directory",
				));
			}
			obj => obj,
		};
		Ok(obj)
	}

	async fn inner_download_file_if_changed(
		&self,
		old_file: Option<DBFile>,
		file: DBFile,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<String, CacheError> {
		let file: RemoteFile = file.try_into()?;
		// Before the lock (the rescue takes per-item locks of its own): a remote edit may have
		// re-minted this row's uuid with an unsent local edit in the old slot, and the freshness
		// check below must see those bytes under the CURRENT uuid — or it reads "no local copy",
		// drops the marker, and downloads the server head over the only record of the edit.
		self.rescue_reminted_slots().await;
		// Held for the whole check-then-download: without it a concurrent clear can delete the
		// file between the freshness check and the write, or evict what we just downloaded.
		let _local_file_guard = self.lock_local_file(file.uuid()).await;
		// An edit that has not reached the server yet is indistinguishable, to the freshness
		// check below, from a stale cache entry: the bytes simply differ. Overwriting it would
		// destroy the edit, and the drain would then find local and server agreeing and clear the
		// marker as though it had uploaded. Serve what is on disk instead and leave the
		// divergence to retry_pending_uploads, which is what exists to resolve it.
		//
		// Read under the lock rather than from the row we were handed: an upload marks the file
		// while holding this same lock, so a snapshot taken before it says "no marker" for
		// precisely the edit worth protecting — the one whose upload just failed.
		let has_pending_upload = self.has_pending_upload(file.stable_uuid())?;
		let local_hash = self.hash_local_file(file.uuid(), file.name()).await;
		// This probe is the authoritative answer to whether the file's bytes are in the cache
		// directory, which is exactly what the materialisation marker records — so record it here,
		// where it is known, rather than in each of the branches below. It is also the one place
		// that notices bytes disappearing without any of our own deletion paths having run.
		match &local_hash {
			Ok(Some(_)) => self.record_materialised(file.uuid()),
			Ok(None) => self.drop_materialised(file.uuid()),
			Err(_) => {}
		}
		match (file.hash(), local_hash) {
			(Some(remote_hash), Ok(Some(local_hash))) => {
				// Remote file has a hash and local file exists
				if remote_hash == local_hash || has_pending_upload {
					return self
						.get_cached_file_path(&file)
						.into_os_string()
						.into_string()
						.map_err(|e| {
							CacheError::conversion(format!(
								"Failed to convert path to string: {e:?}"
							))
						});
				}
			}
			(None, Ok(Some(_))) => {
				// Remote file does not have a hash but local file exists
				if has_pending_upload || old_file.is_some_and(|old_file| old_file == file) {
					return self
						.get_cached_file_path(&file)
						.into_os_string()
						.into_string()
						.map_err(|e| {
							CacheError::conversion(format!(
								"Failed to convert path to string: {e:?}"
							))
						});
				}
			}
			(_, Ok(None)) => {
				// Local file does not exist. Anything the marker described went with it — a cache
				// clear, or the size-budget sweep — so drop it rather than leave the freshness
				// bypass above armed over bytes that are gone. Same rule the drain applies.
				if has_pending_upload {
					sql::clear_pending_upload(&self.conn(), file.stable_uuid())?;
				}
			}
			(_, Err(e)) => {
				return Err(e.into());
			}
		}

		// Only the fetch is abortable, and it is drop-safe: the bytes land in the tmp directory and
		// are renamed into the cache slot as the last thing a download does, so giving up leaves
		// the slot exactly as it was — and the materialisation marker, written after that rename,
		// unwritten.
		with_abort(
			abort.as_ref(),
			self.download_file_io(&file, progress_callback),
		)
		.await?
		.into_os_string()
		.into_string()
		.map_err(|e| CacheError::conversion(format!("Failed to convert path to string: {e:?}")))
	}

	async fn inner_move_item(
		&self,
		item: DBNonRootObject,
		new_parent: DBDirObject,
	) -> Result<DBNonRootObject, CacheError> {
		match item {
			DBNonRootObject::Dir(dir) => {
				let mut remote_dir: RemoteDirectory = dir.into();
				self.client
					.move_dir(&mut remote_dir, &new_parent.into())
					.await?;
				let mut conn = self.conn();

				Ok(DBNonRootObject::Dir(DBDir::upsert_from_remote(
					&mut conn, remote_dir,
				)?))
			}
			DBNonRootObject::File(file) => {
				let mut remote_file: RemoteFile = file.try_into()?;
				self.client
					.move_file(&mut remote_file, &new_parent.into())
					.await?;
				let mut conn = self.conn();
				Ok(DBNonRootObject::File(DBFile::upsert_from_remote(
					&mut conn,
					remote_file,
				)?))
			}
		}
	}

	pub(crate) async fn inner_update_dir(&self, dir: &mut DBDirObject) -> Result<(), CacheError> {
		// Held across fetch AND apply — see `AuthCacheState::listing_barrier`.
		let _listing = self.listing_barrier.read().await;
		let (dirs, files) = self
			.client
			.list_dir(&DirType::from(&*dir), None::<&fn(u64, Option<u64>)>)
			.await?;
		// A child absent from this listing is not necessarily deleted: another device may have
		// MOVED it. The sweep treats absence as deletion — for a dir that cascades an
		// authoritative tombstone through the entire subtree, and for a file it retires a row
		// whose `local_data` (Finder/Files tags) nothing can rebuild. Ask the server which it
		// was before the sweep runs.
		let spare = self
			.reconcile_missing_children(dir.uuid(), &dirs, &files)
			.await?;
		let mut conn = self.conn();
		dir.update_dir_last_listed_now(&conn)?;
		dir.update_children(&mut conn, dirs, files, &spare)?;
		Ok(())
	}

	/// Reconciles the cached children of `parent` that a fresh listing no longer contains,
	/// before the listing's sweep runs: a child the server still knows is re-parented in place
	/// (it was moved — a dir's subtree keeps hanging off it, a file keeps its `local_data`, no
	/// tombstones), a child the server definitively does not know is left for the sweep to
	/// delete, and a child that cannot be verified (network/server failure) is returned for the
	/// sweep to SPARE this round — the destructive guess is the one this exists to avoid.
	///
	/// Dirs are asked about by uuid (never re-minted), files by their whole-life id — an edit
	/// re-mints a file's uuid, so a head that is merely newer than our row is still IN the
	/// listing under its stable id and is never probed. Missing files, then, are exactly the
	/// moved/trashed/deleted ones, which is what keeps the common case at zero probes.
	///
	/// A re-parented child whose new parent the cache does not know yet resolves like any
	/// unknown id, one level per ask.
	async fn reconcile_missing_children(
		&self,
		parent: Uuid,
		listed_dirs: &[RemoteDirectory],
		listed_files: &[RemoteFile],
	) -> Result<Vec<Uuid>, CacheError> {
		let mut spare = self.reconcile_missing_subdirs(parent, listed_dirs).await?;
		spare.extend(self.reconcile_missing_files(parent, listed_files).await?);
		Ok(spare)
	}

	async fn reconcile_missing_subdirs(
		&self,
		parent: Uuid,
		listed: &[RemoteDirectory],
	) -> Result<Vec<Uuid>, CacheError> {
		let cached = sql::select_child_dir_uuids(&self.conn(), parent)?;
		if cached.is_empty() {
			return Ok(Vec::new());
		}
		let listed_uuids: HashSet<Uuid> = listed.iter().map(|dir| dir.uuid()).collect();
		let missing: Vec<Uuid> = cached
			.into_iter()
			.filter(|uuid| !listed_uuids.contains(uuid))
			.collect();
		if missing.is_empty() {
			return Ok(Vec::new());
		}
		if missing.len() > RECONCILE_PROBE_CAP {
			tracing::warn!(
				"{} subdirectories vanished from one listing of {parent}; trusting the listing \
				 (bulk delete) instead of probing each",
				missing.len()
			);
			return Ok(Vec::new());
		}
		let probed: Vec<(Uuid, Result<RemoteDirectory, filen_sdk_rs::error::Error>)> =
			futures::stream::iter(
				missing
					.into_iter()
					.map(|uuid| async move { (uuid, self.client.get_dir(uuid).await) }),
			)
			.buffer_unordered(RECONCILE_PROBE_CONCURRENCY)
			.collect()
			.await;
		let mut spare = Vec::new();
		for (uuid, result) in probed {
			match result {
				// Spared as well as applied: the sweep that follows re-marks everything under
				// `parent` and deletes what the fresh listing lacks. A dir the probe just proved
				// alive UNDER THIS SAME PARENT (created or moved in after the listing snapshot)
				// would otherwise be upserted and then swept — an authoritative tombstone for a
				// live subtree. A dir the upsert re-parented elsewhere ignores the unmark.
				Ok(remote_dir) => {
					debug!("dir {uuid} is alive on the server; keeping it");
					DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
					spare.push(uuid);
				}
				// The one answer that means gone — the sweep deletes and tombstones it.
				// Release any pending markers below it first, exactly as the trashed
				// counterpart does: those markers are what spare this dir from the
				// sweep every round, so leaving them makes the phantom permanent —
				// re-probed and re-failed on every refresh, and re-minted onto a new
				// directory if the name is ever reused. The edits they stood for lost
				// their upload target when the server dropped the lineage.
				Err(e) if e.kind() == ErrorKind::FolderNotFound => {
					let released = sql::clear_pending_upload_subtree(&self.conn(), uuid)?;
					debug!(
						"dir {uuid} is permanently gone; released {released} pending marker(s) \
						 below it for the sweep"
					);
				}
				Err(e) => {
					debug!("could not verify missing dir {uuid} ({e}); sparing it this round");
					spare.push(uuid);
				}
			}
		}
		Ok(spare)
	}

	/// The file half of [`Self::reconcile_missing_children`]. Matched and probed by the
	/// whole-life id (see there), so a moved file is re-parented in place — row, materialised
	/// bytes and `local_data` intact — instead of being retired with a tombstone the system
	/// obeys by deleting a live file from disk.
	async fn reconcile_missing_files(
		&self,
		parent: Uuid,
		listed: &[RemoteFile],
	) -> Result<Vec<Uuid>, CacheError> {
		let cached = sql::select_child_file_stables(&self.conn(), parent)?;
		if cached.is_empty() {
			return Ok(Vec::new());
		}
		let listed_stables: HashSet<StableUuid> =
			listed.iter().map(|file| file.stable_uuid()).collect();
		let missing: Vec<StableUuid> = cached
			.into_iter()
			.filter(|stable| !listed_stables.contains(stable))
			.collect();
		if missing.is_empty() {
			return Ok(Vec::new());
		}
		if missing.len() > RECONCILE_PROBE_CAP {
			// A bulk delete or emptied folder: trust the listing rather than gate its
			// presentation on a probe storm. Rows holding an unsent edit are spared by the
			// sweep's own pending guard, so there is no data loss down this branch.
			tracing::warn!(
				"{} files vanished from one listing of {parent}; trusting the listing (bulk \
				 delete) instead of probing each",
				missing.len()
			);
			return Ok(Vec::new());
		}
		let probed: Vec<(StableUuid, Result<RemoteFile, filen_sdk_rs::error::Error>)> =
			futures::stream::iter(missing.into_iter().map(|stable| async move {
				(stable, self.client.get_file_by_stable_uuid(stable).await)
			}))
			.buffer_unordered(RECONCILE_PROBE_CONCURRENCY)
			.collect()
			.await;
		let mut spare = Vec::new();
		for (stable, result) in probed {
			match result {
				// Alive: moved, trashed, or edited-and-listed-late. The upsert lands the head on
				// this very row (stable tier), re-parenting or trashing it in place. Spare the
				// row the upsert just wrote — the sweep re-marks everything under `parent`, and
				// a head still under this parent would otherwise be upserted and then swept.
				Ok(head) => {
					debug!("file lineage {stable} is alive on the server; keeping it");
					let file = DBFile::upsert_from_remote(&mut self.conn(), head)?;
					spare.push(file.uuid);
				}
				// Definitively gone — whole lineage, not just a version (`v3/file/stable`
				// answers for the lineage). Release any pending marker so the phantom converges
				// instead of being re-spared and re-probed forever; the edit it stood for lost
				// its upload target with the lineage.
				Err(e) if e.kind() == ErrorKind::FileNotFound => {
					let row = RawDBItem::select_by_stable(&self.conn(), stable.into())?;
					if let Some(row) = row {
						let released = sql::clear_pending_upload_by_uuid(&self.conn(), row.uuid)?;
						if released > 0 {
							debug!(
								"file lineage {stable} is permanently gone; released its pending \
								 marker for the sweep"
							);
						}
					}
				}
				Err(e) => {
					debug!("could not verify missing file {stable} ({e}); sparing it this round");
					if let Some(row) = RawDBItem::select_by_stable(&self.conn(), stable.into())? {
						spare.push(row.uuid);
					}
				}
			}
		}
		Ok(spare)
	}
}
