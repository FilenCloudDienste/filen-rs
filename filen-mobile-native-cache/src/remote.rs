use std::{path::PathBuf, sync::Arc, time::Instant};

use chrono::DateTime;
use filen_sdk_rs::fs::{
	HasName, HasUUID,
	categories::{DirType, Normal},
	dir::{RemoteDirectory, meta::DirectoryMetaChanges},
	file::{FileBuilderOptionalName, RemoteFile, meta::FileMetaChanges, traits::HasRemoteFileInfo},
};
use filen_types::fs::{ParentUuid, StableUuid, Uuid};
use rusqlite::OptionalExtension;
use tracing::debug;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		CreateFileResponse, DirWithPathResponse, FfiId, FileWithPathResponse,
		ObjectWithPathResponse, ParsedFfiId, PathFfiId, QueryChildrenResponse,
		QueryNonDirChildrenResponse, SearchQueryArgs, SearchQueryResponseEntry, UploadFileInfo,
	},
	local::addressed_stable_uuid,
	sql::{
		self, DBDirExt, DBDirObject, DBDirTrait, DBFileMeta, DBItemTrait,
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
				.upload_new_file(os_path, parent_path, info, progress_callback)
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

	pub(crate) async fn update_and_query_recents(
		&self,
		order_by: Option<String>,
	) -> Result<QueryNonDirChildrenResponse, CacheError> {
		debug!("Updating and querying recents with order by: {order_by:?}");
		self.update_recents().await?;
		self.query_recents(order_by)
	}

	pub(crate) async fn download_file_if_changed_by_path(
		&self,
		file_path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		debug!("Downloading file to path: {}", file_path.0);
		let file_path = self.canonicalize_id(&file_path)?;
		let path_values = file_path.as_path()?;
		let old_file = match sql::select_object_at_path(&self.conn(), &path_values)? {
			Some(DBObject::File(file)) => Some(file),
			Some(_) => None,
			None => None,
		};

		let file = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => file,
			UpdateItemsInPath::Partial(_, _) | UpdateItemsInPath::Complete(_) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a file",
					path_values.full_path
				)));
			}
		};

		self.inner_download_file_if_changed(old_file, file, progress_callback)
			.await
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
		self.inner_download_file_if_changed(Some(file.clone()), file, progress_callback)
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
		let DBFileMeta::Decoded(meta) = file.meta else {
			return Err(CacheError::remote(format!(
				"File {} does not have decoded metadata",
				file.uuid
			)));
		};
		// Held across the whole check-then-upload: io_upload_updated_file reads the cached copy
		// and then renames it away under the newly minted uuid, so without this a concurrent
		// clear or download of the same item interleaves with it.
		let _local_file_guard = self.lock_local_file(file.uuid).await;
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
			.io_upload_updated_file(
				file.uuid,
				meta.name,
				file.parent.try_into().map_err(|e| {
					CacheError::conversion(format!("Failed to convert parent UUID: {e}"))
				})?,
				meta.mime,
				progress_callback,
			)
			.await?;
		sql::clear_pending_upload(&self.conn(), file.stable_uuid)?;
		Ok(Some(uploaded))
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

		let mut conn = self.conn();
		DBFile::upsert_from_remote(&mut conn, remote_file)?;
		Ok(true)
	}

	pub(crate) async fn upload_new_file(
		&self,
		os_path: String,
		parent_path: FfiId,
		info: UploadFileInfo,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FileWithPathResponse, CacheError> {
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

		let (remote_file, _) = self
			.io_upload_file(os_path, builder, progress_callback)
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
		let mut conn = self.conn();
		let file = DBFile::upsert_from_remote(&mut conn, file)?;
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
				sql::delete_item(&self.conn(), uuid)?;
			}
			DBObject::File(file) => {
				self.io_delete_local(file.uuid).await?;
				let remote_file: RemoteFile = file.try_into()?;
				let uuid = remote_file.uuid();
				self.client.delete_file_permanently(remote_file).await?;
				sql::delete_item(&self.conn(), uuid)?;
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
		match (
			file.hash(),
			self.hash_local_file(file.uuid(), file.name()).await,
		) {
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

		self.download_file_io(&file, progress_callback)
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
