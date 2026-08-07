use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::Instant,
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
use rusqlite::OptionalExtension;
use tracing::debug;

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
		let response = self
			.async_execute_authed_owned(async move |auth_state| {
				auth_state
					.download_file_if_changed_with_item(id, progress_callback, abort)
					.await
			})
			.await;
		// Bytes on the device are a stake, so the file just joined the working set.
		crate::working_set::schedule_refresh(&self.state);
		response
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
		let response = self
			.async_execute_authed_owned(async move |auth_state| {
				auth_state
					.modify_file_content(id, os_path, progress_callback, abort)
					.await
			})
			.await;
		// Edited bytes are a stake whether or not they reached the server.
		crate::working_set::schedule_refresh(&self.state);
		response
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
		let response = self
			.async_execute_authed_owned(async move |auth_state| {
				auth_state.set_favorite_rank(item, favorite_rank).await
			})
			.await;
		// A favourite is a stake in itself, and stops being one the moment it is cleared.
		crate::working_set::schedule_refresh(&self.state);
		response
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
		let path = self.canonicalize_id(path)?;
		let path_id = path.as_path()?;
		let mut dir: DBDirObject = match self.update_items_in_path(&path_id).await? {
			UpdateItemsInPath::Complete(dbobject) => dbobject.try_into()?,
			UpdateItemsInPath::Partial(_, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a directory",
					path_id.full_path
				)));
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
		let (dirs, files) = self
			.client
			.list_trash(None::<&fn(u64, Option<u64>)>)
			.await?;
		debug!("Updating trash with {dirs:?} dirs and {files:?} files");
		sql::update_trashed_items(&mut self.conn(), dirs, files)?;
		self.last_trash_update
			.write()
			.unwrap()
			.replace(Instant::now());
		Ok(())
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
		let file_path = self.canonicalize_id(id)?;
		let path_values = file_path.as_path()?;
		let old_file = match sql::select_object_at_path(&self.conn(), &path_values)? {
			Some(DBObject::File(file)) => Some(file),
			Some(_) => None,
			None => None,
		};

		match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => Ok((old_file, file)),
			UpdateItemsInPath::Partial(_, _) | UpdateItemsInPath::Complete(_) => {
				Err(CacheError::remote(format!(
					"Path {} does not point to a file",
					path_values.full_path
				)))
			}
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
	fn select_object_by_id(&self, id: &FfiId) -> Result<Option<DBObject>, CacheError> {
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
		debug!("Updating and querying item: {}", id.0);
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
		let obj = match self.select_object_by_id(&id)? {
			Some(obj) => obj,
			None => return self.resolve_unknown_dir_uuid(&id).await,
		};
		match obj {
			DBObject::File(file) => self.refresh_file(file).await,
			DBObject::Dir(dir) => self.refresh_dir(dir).await,
			// A root is not something the server retires or moves; keeping its usage figures
			// current is update_roots_info's job.
			DBObject::Root(root) => Ok(Some(DBObject::Root(root).into())),
		}
	}

	/// The directory a bare-uuid id names, learned from the server (see
	/// [`AuthCacheState::update_and_query_item`], the only caller).
	///
	/// Nothing else is probed: `stable/<id>` and the path forms describe locations only this cache
	/// can translate, so an unknown one stays unknown. The directory lands through the same upsert
	/// a refresh uses, which leaves it exactly as a listing would have — including a parent we may
	/// still know nothing about, and which the next query resolves the same way, one level per ask.
	async fn resolve_unknown_dir_uuid(&self, id: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		let Ok(uuid) = id.0.parse::<Uuid>() else {
			return Ok(None);
		};
		let remote_dir = match self.client.get_dir(uuid).await {
			Ok(remote_dir) => remote_dir,
			// The one answer that means gone. Every other failure is reported as itself: a
			// connectivity problem must never read as a deletion.
			Err(e) if e.kind() == ErrorKind::FolderNotFound => return Ok(None),
			Err(e) => return Err(e.into()),
		};
		debug!("Learned unlisted dir {uuid} from the server");
		let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
		Ok(Some(DBObject::Dir(dir).into()))
	}

	/// The file behind a row, refreshed to its live head.
	///
	/// Asked by the stable id, not the uuid: a content edit made elsewhere re-mints the uuid, and
	/// a by-uuid question can only answer with the version this cache already holds (or with the
	/// short-lived trashed ghost a versioning-disabled edit leaves behind). By the whole-life id
	/// there is only ever one answer, and it is the head — so it is upserted unconditionally, and
	/// the stable tier lands it on this very row. A trashed head is still this item and comes back
	/// as one, carrying the parent a restore would put it back in.
	async fn refresh_file(&self, file: DBFile) -> Result<Option<FfiObject>, CacheError> {
		let head = match self.client.get_file_by_stable_uuid(file.stable_uuid).await {
			Ok(head) => head,
			Err(e) if e.kind() == ErrorKind::FileNotFound => {
				return self.forget_item(DBObject::File(file)).await;
			}
			Err(e) => return Err(e.into()),
		};
		let file = DBFile::upsert_from_remote(&mut self.conn(), head)?;
		Ok(Some(DBObject::File(file).into()))
	}

	async fn refresh_dir(&self, dir: DBDir) -> Result<Option<FfiObject>, CacheError> {
		let remote_dir = match self.client.get_dir(dir.uuid).await {
			Ok(remote_dir) => remote_dir,
			Err(e) if e.kind() == ErrorKind::FolderNotFound => {
				return self.forget_item(DBObject::Dir(dir)).await;
			}
			Err(e) => return Err(e.into()),
		};
		let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
		Ok(Some(DBObject::Dir(dir).into()))
	}

	/// Drops an item the server says it no longer has, bytes first.
	///
	/// Same order as the tail of [`AuthCacheState::delete_item`], and for the same reason: the
	/// bytes go first, then the row. Those bytes are a copy of the server's own content, so
	/// dropping them along with the item is the point — the row delete that follows is what
	/// retires the id for every replica, and a slot left behind would be a cached copy of
	/// something nothing names any more.
	pub(crate) async fn forget_item(&self, obj: DBObject) -> Result<Option<FfiObject>, CacheError> {
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
			return Ok(Some(obj.into()));
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
		let path = self.canonicalize_id(&path)?;
		let path_values: PathFfiId<'_> = path.as_path()?;
		let obj = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(dbobject) => dbobject,
			UpdateItemsInPath::Partial(_, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to an item",
					path_values.full_path
				)));
			}
		};

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
				sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
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
			DBNonRootObject::select(&conn, uuid)?
		};

		// we do this first to make sure we have a valid restore target
		let parent = match to {
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

		if let Some((parent, parent_path)) = parent
			&& object.certain_parent() != parent.uuid()
		{
			let new_path = parent_path.join(&object.uuid().to_string());
			let item = self.inner_move_item(object, parent).await?;
			return Ok(ObjectWithPathResponse {
				object: DBObject::from(item).into(),
				id: new_path,
			});
		}

		sql::recursive_select_path_from_uuid(&self.conn(), object.uuid())?
			.ok_or_else(|| {
				CacheError::remote(format!("Failed to get path for object with UUID {uuid}"))
			})
			.map(|s| ObjectWithPathResponse {
				id: FfiId(format!("{}{}", self.client.root().uuid(), s)),
				object: DBObject::from(object).into(),
			})
	}

	pub(crate) async fn move_item(
		&self,
		item: FfiId,
		new_parent: FfiId,
	) -> Result<ObjectWithPathResponse, CacheError> {
		debug!("Moving item {} to new parent {}", item.0, new_parent.0);
		let item = self.canonicalize_id(&item)?;
		let new_parent = self.canonicalize_id(&new_parent)?.into_owned();
		let item_pvs: PathFfiId<'_> = item.as_path()?;
		let new_parent_pvs: PathFfiId<'_> = new_parent.as_path()?;

		let (obj, new_parent_dir) = futures::try_join!(
			async {
				let obj = match self.update_items_in_path(&item_pvs).await? {
					UpdateItemsInPath::Complete(obj) => {
						DBNonRootObject::try_from(obj).map_err(|e| {
							CacheError::remote(format!(
								"Path {} does not point to a non-root item: {}",
								item_pvs.full_path, e
							))
						})?
					}
					UpdateItemsInPath::Partial(remaining_path, _) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to an item, remaining: {}",
							item_pvs.full_path, remaining_path
						)));
					}
				};
				Ok(obj)
			},
			async {
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

		let obj = self.inner_move_item(obj, new_parent_dir).await?;
		Ok(ObjectWithPathResponse {
			object: DBObject::from(obj).into(),
			id: new_parent.join(item_pvs.name_or_uuid),
		})
	}

	pub(crate) async fn rename_item(
		&self,
		item: FfiId,
		new_name: String,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		debug!("Renaming item {} to {}", item.0, new_name);
		let item = self.canonicalize_id(&item)?.into_owned();
		let item_pvs: PathFfiId<'_> = item.as_path()?;
		if item_pvs.name_or_uuid.is_empty() {
			return Err(CacheError::remote(format!(
				"Cannot rename item: {}",
				item.0
			)));
		} else if item_pvs.name_or_uuid == new_name {
			return Ok(None);
		}
		self.update_dir_children(&item.parent()).await?;
		let obj = match sql::select_object_at_path(&self.conn(), &item_pvs)? {
			Some(obj) => DBNonRootObject::try_from(obj).map_err(|e| {
				CacheError::remote(format!(
					"Path {} does not point to a non-root item: {}",
					item_pvs.full_path, e
				))
			})?,
			None => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to an item",
					item_pvs.full_path
				)));
			}
		};
		let new_path = item.parent().join(&new_name);
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
		Ok(Some(ObjectWithPathResponse {
			object: obj.into(),
			id: new_path,
		}))
	}

	pub(crate) async fn clear_local_cache(&self, item: FfiId) -> Result<(), CacheError> {
		let item = self.canonicalize_id(&item)?;
		let pvs = item.as_path()?;
		debug!("Clearing local cache for item: {}", pvs.full_path);
		let obj = match sql::select_object_at_path(&self.conn(), &pvs)? {
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(obj.uuid()).await?;
		Ok(())
	}

	/// Retries every file still marked as having unuploaded local changes.
	///
	/// Best effort and independent per file: one that still fails keeps its marker for the next
	/// drain rather than aborting the rest. Returns how many reached the server.
	pub(crate) async fn retry_pending_uploads(&self) -> Result<u32, CacheError> {
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
		Ok(sql::select_pending_uploads(&self.conn())?.len() as u32)
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
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to an item",
							item.0
						)));
					}
				})
			}
		};
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
			}
			DBObject::File(file) => {
				self.io_delete_local(file.uuid).await?;
				let remote_file: RemoteFile = file.try_into()?;
				let uuid = remote_file.uuid();
				self.client.delete_file_permanently(remote_file).await?;
				sql::delete_item(&mut self.conn(), uuid)?;
			}
		}
		debug!("Successfully deleted item at path: {}", item.0);
		Ok(())
	}

	pub(crate) async fn set_favorite_rank(
		&self,
		item: FfiId,
		favorite_rank: i64,
	) -> Result<ObjectWithPathResponse, CacheError> {
		let item = self.canonicalize_id(&item)?;
		let pvs = item.as_parsed()?;
		debug!(
			"Setting favorite rank for item: {}, rank: {}",
			item.0, favorite_rank
		);
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
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to an item",
							item.0
						)));
					}
				})
			}
		}
		.ok_or_else(|| CacheError::remote(format!("No item found at path: {}", item.0)))?;
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
		Ok(ObjectWithPathResponse {
			object: obj.into(),
			id: item.into_owned(),
		})
	}

	async fn inner_download_file_if_changed(
		&self,
		old_file: Option<DBFile>,
		file: DBFile,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
		abort: Option<Arc<FfiAbortSignal>>,
	) -> Result<String, CacheError> {
		let file: RemoteFile = file.try_into()?;
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

	async fn inner_update_dir(&self, dir: &mut DBDirObject) -> Result<(), CacheError> {
		let (dirs, files) = self
			.client
			.list_dir(&DirType::from(&*dir), None::<&fn(u64, Option<u64>)>)
			.await?;
		let mut conn = self.conn();
		dir.update_dir_last_listed_now(&conn)?;
		dir.update_children(&mut conn, dirs, files)?;
		Ok(())
	}
}
