use std::{path::PathBuf, str::FromStr, sync::Arc, time::Instant};

use chrono::DateTime;
use filen_sdk_rs::{
	ErrorKind,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::{RemoteDirectory, meta::DirectoryMetaChanges},
		file::{
			FileBuilderOptionalName, FileWithInfo, RemoteFile, meta::FileMetaChanges,
			traits::HasRemoteFileInfo,
		},
	},
};
use filen_types::fs::{ParentUuid, Uuid, UuidStr};
use rusqlite::OptionalExtension;
use tracing::debug;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		CreateFileResponse, DirWithPathResponse, FfiChangesResponse, FfiDir, FfiDirMeta, FfiFile,
		FfiId, FfiObject, FileWithPathResponse, ObjectWithPathResponse, ParsedFfiId, PathFfiId,
		QueryChildrenResponse, QueryNonDirChildrenResponse, SearchQueryArgs,
		SearchQueryResponseEntry, UploadFileInfo,
	},
	sql::{
		self, DBDirExt, DBDirObject, DBDirTrait, DBFileMeta, DBItemTrait,
		dir::DBDir,
		error::OptionalExtensionSQL,
		file::DBFile,
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

	/// Paginated directory enumeration: one page (`offset..offset+limit`) of children. Pass
	/// `refresh = true` only for the first page (offset 0) to re-list from the server once; later
	/// pages read straight from cache. `None` if the path is not a cached directory.
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

	pub async fn set_item_metadata(
		&self,
		item: FfiId,
		created: Option<i64>,
		modified: Option<i64>,
		mime: Option<String>,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.set_item_metadata(item, created, modified, mime)
				.await
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

	pub async fn update_and_query_item_by_uuid(
		&self,
		uuid: String,
	) -> Result<Option<FfiObject>, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state.update_and_query_item_by_uuid(uuid).await
		})
		.await
	}

	pub async fn download_file_to_path_by_uuid(
		&self,
		uuid: String,
		target_path: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FfiFile, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.download_file_to_path_by_uuid(uuid, target_path, progress_callback)
				.await
		})
		.await
	}

	pub async fn modify_file_content(
		&self,
		item: FfiId,
		os_path: String,
		info: Option<UploadFileInfo>,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FileWithPathResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.modify_file_content(item, os_path, info, progress_callback)
				.await
		})
		.await
	}

	pub async fn enumerate_changes(
		&self,
		container: String,
		from_anchor: Vec<u8>,
		refresh: bool,
	) -> Result<FfiChangesResponse, CacheError> {
		self.async_execute_authed_owned(async move |auth_state| {
			auth_state
				.enumerate_changes(container, from_anchor, refresh)
				.await
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
		let path = self.canonicalize_ffi_id(path)?;
		debug!("Updating directory children for path: {}", path.0);
		let path_id = path.as_path()?;
		let mut dir: DBDirObject = match self.update_items_in_path(&path_id).await? {
			// A file is not a directory — surface the dedicated variant the Swift side catches, rather
			// than a generic conversion error.
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::NotADirectory(
					format!(
						"Path {} points to a file, not a directory",
						path_id.full_path
					)
					.into(),
				));
			}
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
		let path = self.canonicalize_ffi_id(&path)?;
		debug!(
			"Updating and querying directory children for path: {}",
			path.0
		);
		self.update_dir_children(&path).await?;
		self.query_dir_children(&path, order_by)
	}

	/// One page of a directory's children for the File Provider enumeration. `refresh` re-lists the
	/// dir from the server first — do this ONLY on the first page (offset 0): Filen's dir listing has
	/// no server-side cursor, so the full re-list happens once and later pages are served from the
	/// freshly-upserted cache. Returns `None` if the path is not a cached directory.
	pub(crate) async fn update_and_query_dir_children_page(
		&self,
		path: FfiId,
		order_by: Option<String>,
		offset: u32,
		limit: u32,
		refresh: bool,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		let path = self.canonicalize_ffi_id(&path)?;
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

	pub(crate) async fn download_file_if_changed_by_path(
		&self,
		file_path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		let file_path = self.canonicalize_ffi_id(&file_path)?;
		debug!("Downloading file to path: {}", file_path.0);
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

	/// Best-effort revalidation for the by-uuid download doors. Refresh the item through
	/// [`Self::update_and_query_item_by_uuid`] — which handles both a backend 404 and a
	/// retired-but-versioned uuid via the stable-identity parent re-list — then select the
	/// resulting (possibly re-minted) row. A revalidation *error* (e.g. offline) falls back
	/// to the local row so cached files stay readable without a network.
	async fn select_file_revalidated(&self, uuid: &str) -> Result<Option<DBFile>, CacheError> {
		let current_uuid = match self.update_and_query_item_by_uuid(uuid.to_string()).await {
			Ok(Some(FfiObject::File(f))) => f.uuid,
			// gone remotely (row already dropped), or resolved to a non-file
			Ok(_) => return Ok(None),
			Err(e) => {
				debug!("By-uuid revalidation of {uuid} failed, using the local row: {e}");
				uuid.to_string()
			}
		};
		let conn = self.conn();
		let parsed = Uuid::from_str(&current_uuid).map_err(|e| {
			CacheError::conversion(format!("Invalid UUID {current_uuid}, err: {e}"))
		})?;
		let real_uuid = self.resolve_uuid(&conn, parsed)?;
		Ok(DBFile::select(&conn, real_uuid).optional()?)
	}

	pub(crate) async fn download_file_if_changed_by_uuid(
		&self,
		uuid: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		debug!("Downloading file with UUID: {uuid}");
		let file = self
			.select_file_revalidated(&uuid)
			.await?
			.ok_or_else(|| CacheError::remote(format!("No file found with UUID: {uuid}")))?;
		// unnecesssary clone but better than redownloading
		self.inner_download_file_if_changed(Some(file.clone()), file, progress_callback)
			.await
	}

	pub(crate) async fn upload_file_if_changed(
		&self,
		path: FfiId,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<bool, CacheError> {
		let path = self.canonicalize_ffi_id(&path)?;
		debug!("Uploading file at path: {}", path.0);
		let path_values = path.as_path()?;
		let remote_file = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => {
				let DBFileMeta::Decoded(meta) = file.meta else {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a file with decoded metadata",
						path_values.full_path
					)));
				};
				if let Some(hash) = meta.hash {
					let local_hash = self.hash_local_file(file.uuid, Some(&meta.name)).await?;
					if local_hash == Some(hash.into()) {
						return Ok(false);
					}
				}

				self.io_upload_updated_file(
					&file.uuid.to_string(),
					meta.name,
					file.parent.try_into().map_err(|e| {
						CacheError::conversion(format!("Failed to convert parent UUID: {e}"))
					})?,
					meta.mime,
					progress_callback,
				)
				.await?
			}
			UpdateItemsInPath::Complete(_) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a file",
					path_values.full_path
				)));
			}
			UpdateItemsInPath::Partial(remaining, parent)
				if remaining == path_values.name_or_uuid =>
			{
				let mut builder = FileBuilderOptionalName::new(parent.uuid());
				builder.name(path_values.name_or_uuid)?;
				self.io_upload_new_file(builder).await?.0
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
		let parent_path = self.canonicalize_ffi_id(&parent_path)?;
		let os_path = PathBuf::from(os_path);
		let name = info.name;
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
		let parent_path = self.canonicalize_ffi_id(&parent_path)?;
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
		let (file, os_path) = self.io_upload_new_file(builder).await?;
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
		let parent_path = self.canonicalize_ffi_id(&parent_path)?;
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
		let path = self.canonicalize_ffi_id(&path)?;
		debug!("Trashing item at path: {}", path.0);
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
		let to = to.map(|to| self.canonicalize_ffi_id(&to)).transpose()?;
		let uuid = UuidStr::from_str(uuid)
			.map_err(|e| CacheError::conversion(format!("Invalid UUID {uuid}, err: {e}")))?;
		let object = {
			let conn = self.conn();
			let uuid = self.resolve_uuid(&conn, uuid.into())?;
			DBNonRootObject::select(&conn, uuid)?
		};

		// we do this first to make sure we have a valid restore target
		let parent = match to {
			Some(to_path) => {
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

		// recursive_select_path_from_uuid already includes the root uuid as the first segment,
		// so canonicalize the uuid-form id instead of prefixing the root uuid a second time.
		let id = self.canonicalize_ffi_id(&FfiId(format!("uuid/{}", object.uuid())))?;
		Ok(ObjectWithPathResponse {
			id,
			object: DBObject::from(object).into(),
		})
	}

	pub(crate) async fn move_item(
		&self,
		item: FfiId,
		new_parent: FfiId,
	) -> Result<ObjectWithPathResponse, CacheError> {
		let item = self.canonicalize_ffi_id(&item)?;
		let new_parent = self.canonicalize_ffi_id(&new_parent)?;
		debug!("Moving item {} to new parent {}", item.0, new_parent.0);
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
		let item = self.canonicalize_ffi_id(&item)?;
		debug!("Renaming item {} to {}", item.0, new_name);
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

	/// Edit an item's metadata timestamps / mime without re-uploading content. Same server
	/// endpoint as a rename (the whole metadata blob is re-encrypted), so this mirrors that path
	/// but sets `created`/`modified`/`mime` instead of the name. Each argument is optional; a
	/// `None` leaves that field untouched. Dirs only carry `created` — `modified`/`mime` are
	/// silently ignored for them (they don't exist in the dir schema).
	pub(crate) async fn set_item_metadata(
		&self,
		item: FfiId,
		created: Option<i64>,
		modified: Option<i64>,
		mime: Option<String>,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		if created.is_none() && modified.is_none() && mime.is_none() {
			return Ok(None);
		}
		let item = self.canonicalize_ffi_id(&item)?;
		debug!("Editing metadata of item {}", item.0);
		let item_pvs: PathFfiId<'_> = item.as_path()?;
		if item_pvs.name_or_uuid.is_empty() {
			return Err(CacheError::remote(format!(
				"Cannot edit metadata of item: {}",
				item.0
			)));
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
		let to_dt = |millis: i64, field: &str| {
			DateTime::from_timestamp_millis(millis).ok_or_else(|| {
				CacheError::conversion(format!(
					"Failed to convert {field} timestamp {millis} to DateTime"
				))
			})
		};
		let obj = match obj {
			DBNonRootObject::Dir(dbdir) => {
				let mut remote_dir: RemoteDirectory = dbdir.into();
				let mut changes = DirectoryMetaChanges::default();
				if let Some(created) = created {
					changes = changes.created(Some(to_dt(created, "created")?));
				}
				self.client
					.update_dir_metadata(&mut remote_dir, changes)
					.await?;
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				DBObject::Dir(dir)
			}
			DBNonRootObject::File(dbfile) => {
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				let mut changes = FileMetaChanges::default();
				if let Some(mime) = mime {
					changes = changes.mime(mime);
				}
				if let Some(modified) = modified {
					changes = changes.last_modified(to_dt(modified, "modified")?);
				}
				if let Some(created) = created {
					changes = changes.created(Some(to_dt(created, "created")?));
				}
				self.client
					.update_file_metadata(&mut remote_file, changes)
					.await?;
				let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;
				DBObject::File(file)
			}
		};
		Ok(Some(ObjectWithPathResponse {
			object: obj.into(),
			id: item,
		}))
	}

	pub(crate) async fn clear_local_cache(&self, item: FfiId) -> Result<(), CacheError> {
		let item = self.canonicalize_ffi_id(&item)?;
		let pvs = item.as_path()?;
		debug!("Clearing local cache for item: {}", pvs.full_path);
		let obj = match sql::select_object_at_path(&self.conn(), &pvs)? {
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(obj.uuid()).await?;
		Ok(())
	}

	pub(crate) async fn clear_local_cache_by_uuid(&self, uuid: &str) -> Result<(), CacheError> {
		debug!("Clearing local cache for item with uuid: {uuid}");
		let parsed = UuidStr::from_str(uuid)
			.map_err(|e| CacheError::conversion(format!("Invalid UUID {uuid}, err: {e}")))?;
		let obj = {
			let conn = self.conn();
			let parsed = self.resolve_uuid(&conn, parsed.into())?;
			DBObject::select(&conn, parsed).optional()?
		};
		let obj = match obj {
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(obj.uuid()).await?;
		Ok(())
	}

	pub(crate) async fn delete_item(&self, item: FfiId) -> Result<(), CacheError> {
		let item = self.canonicalize_ffi_id(&item)?;
		debug!("Deleting object at path: {}", item.0);
		let pvs = item.as_parsed()?;
		let obj = match pvs {
			ParsedFfiId::Trash(uuid_id)
			| ParsedFfiId::Recents(uuid_id)
			| ParsedFfiId::Uuid(uuid_id) => DBObject::select(
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
		let item = self.canonicalize_ffi_id(&item)?;
		let pvs = item.as_parsed()?;
		debug!(
			"Setting favorite rank for item: {}, rank: {}",
			item.0, favorite_rank
		);
		let obj = match pvs {
			ParsedFfiId::Trash(uuid_id)
			| ParsedFfiId::Recents(uuid_id)
			| ParsedFfiId::Uuid(uuid_id) => DBObject::select(
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
			id: item,
		})
	}

	/// [`Client::get_file_with_info`], mapping the backend "file not found" into `Ok(None)`
	/// (remote-deleted). A `versioned` result is passed through untouched: the archived object
	/// still resolves and is downloadable, so whether "superseded by a same-name re-upload"
	/// counts as gone is the caller's decision.
	async fn get_file_info_or_none(&self, uuid: Uuid) -> Result<Option<FileWithInfo>, CacheError> {
		match self.client.get_file_with_info(uuid).await {
			Ok(info) => Ok(Some(info)),
			Err(e) if e.kind() == ErrorKind::FileNotFound => Ok(None),
			Err(e) => Err(e.into()),
		}
	}

	/// [`Client::get_dir`], mapping the backend "folder not found" into `Ok(None)` (remote-deleted).
	async fn get_dir_or_none(&self, uuid: Uuid) -> Result<Option<RemoteDirectory>, CacheError> {
		match self.client.get_dir(uuid).await {
			Ok(dir) => Ok(Some(dir)),
			Err(e) if e.kind() == ErrorKind::FolderNotFound => Ok(None),
			Err(e) => Err(e.into()),
		}
	}

	/// Server-refreshing lookup by (stable or real) uuid for the replicated File Provider's
	/// `item(for:)` and cold `fetchContents`. The root uuid and the `"trash"` sentinel resolve
	/// locally; every other uuid is refreshed from the backend, upserted, and returned. A backend
	/// not-found (file_not_found / folder_not_found) means the item was deleted remotely: the local
	/// row (if any) is removed and `Ok(None)` is returned so the extension surfaces `.noSuchItem`.
	pub(crate) async fn update_and_query_item_by_uuid(
		&self,
		uuid: String,
	) -> Result<Option<FfiObject>, CacheError> {
		debug!("Updating and querying item by uuid: {uuid}");

		// The trash container is a synthetic local dir whose "uuid" ("trash") is not a real UuidStr.
		if uuid == "trash" {
			return Ok(Some(trash_ffi_object()));
		}

		let parsed = UuidStr::from_str(&uuid)
			.map_err(|e| CacheError::conversion(format!("Invalid uuid {uuid}, err: {e}")))?;

		let (real_uuid, local_obj) = {
			let conn = self.conn();
			let real_uuid = self.resolve_uuid(&conn, parsed.into())?;
			let local_obj = DBObject::select(&conn, real_uuid).optional()?;
			(real_uuid, local_obj)
		};

		let root_uuid = self.client.root().uuid();
		if real_uuid == root_uuid {
			// Root is local; refresh its usage info best-effort so a transient error never hides it.
			if let Err(e) = self.update_roots_info().await {
				tracing::warn!("Failed to refresh roots info for {root_uuid}: {e}");
			}
			return Ok(DBObject::select(&self.conn(), root_uuid)
				.optional()?
				.map(Into::into));
		}

		enum LocalType {
			File,
			Dir,
			Root,
			Unknown,
		}
		let had_local = local_obj.is_some();
		let local_type = match &local_obj {
			Some(DBObject::File(_)) => LocalType::File,
			Some(DBObject::Dir(_)) => LocalType::Dir,
			Some(DBObject::Root(_)) => LocalType::Root,
			None => LocalType::Unknown,
		};
		// A stray Root row for a non-root uuid (shouldn't happen) is just returned locally.
		if matches!(local_type, LocalType::Root) {
			return Ok(local_obj.map(Into::into));
		}

		// If the local row is a file, capture its stable identity + parent before refreshing: a miss
		// on a file (backend 404, or a `versioned` uuid — a content edit re-mints the uuid and keeps
		// the retired one resolving as an archived version) means we must re-list the parent and
		// recover the stable identity rather than shredding it with a delete + fresh-id re-mint on
		// the parent's next re-list.
		let file_identity: Option<(Uuid, ParentUuid)> = match &local_obj {
			Some(DBObject::File(file)) => Some((file.stable_uuid, file.parent)),
			_ => None,
		};

		let refreshed: Option<DBObject> = match local_type {
			LocalType::File => match self.get_file_info_or_none(real_uuid).await? {
				// Versioned = superseded by a same-name re-upload. Don't upsert the archived
				// snapshot as if it were the live file; treat it as a miss so the
				// stable-identity recovery below re-lists the parent and finds the re-mint.
				Some(info) if info.versioned => None,
				Some(info) => Some(DBFile::upsert_from_remote(&mut self.conn(), info.file)?.into()),
				None => None,
			},
			LocalType::Dir => match self.get_dir_or_none(real_uuid).await? {
				Some(remote_dir) => {
					Some(DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?.into())
				}
				None => None,
			},
			// Type unknown locally: a file miss may just mean it is a dir, so fall back to the dir
			// endpoint before concluding it is gone. A versioned result proves the uuid is a
			// (superseded) FILE, so skip the guaranteed-miss dir round-trip and report a miss.
			LocalType::Unknown => match self.get_file_info_or_none(real_uuid).await? {
				Some(info) if info.versioned => None,
				Some(info) => Some(DBFile::upsert_from_remote(&mut self.conn(), info.file)?.into()),
				None => match self.get_dir_or_none(real_uuid).await? {
					Some(remote_dir) => {
						Some(DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?.into())
					}
					None => None,
				},
			},
			LocalType::Root => unreachable!("handled above"),
		};

		match refreshed {
			Some(obj) => Ok(Some(obj.into())),
			None => {
				if let Some((stable_uuid, parent)) = file_identity {
					// The file 404'd or its uuid is versioned (superseded by a content edit that
					// re-minted the uuid in place on the same (parent, name) row), so re-list the
					// parent and check whether our stable identity now maps to a freshly re-minted
					// uuid before concluding the file is gone.
					if let Ok(parent_uuid) = Uuid::try_from(parent)
						&& let Err(e) = self
							.update_dir_children(&FfiId(format!("uuid/{parent_uuid}")))
							.await
					{
						debug!(
							"Failed to re-list parent {parent_uuid} for re-minted file {stable_uuid}: {e}"
						);
					}

					let conn = self.conn();
					let current_uuid = self.resolve_uuid(&conn, stable_uuid)?;
					if let Some(obj) = DBObject::select(&conn, current_uuid).optional()? {
						// The stable identity survived (re-minted in place) — return the fresh row rather
						// than deleting and letting the parent's re-list mint a brand-new stable id.
						return Ok(Some(obj.into()));
					}
					// The stable identity is truly gone; drop the stale row if the re-list left it.
					if had_local {
						sql::delete_item(&conn, real_uuid)?;
					}
					return Ok(None);
				}

				// Remote-deleted dir (or nothing local): drop the stale local row (Phase-4 tombstone
				// triggers flow from here).
				if had_local {
					sql::delete_item(&self.conn(), real_uuid)?;
				}
				Ok(None)
			}
		}
	}

	/// Ensures the file identified by `uuid` (stable or real) is cached & current, then copies the
	/// cache bytes onto `target_path` (the File Provider's temporary volume) and returns the
	/// [`FfiFile`] (its hash drives `contentVersion`). The cache copy is left in place.
	pub(crate) async fn download_file_to_path_by_uuid(
		&self,
		uuid: String,
		target_path: String,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FfiFile, CacheError> {
		debug!("Downloading file {uuid} to external path {target_path}");
		let file = self.select_file_revalidated(&uuid).await?.ok_or_else(|| {
			CacheError::DoesNotExist(format!("No file found with uuid: {uuid}").into())
		})?;

		let cache_path = self
			.inner_download_file_if_changed(Some(file.clone()), file.clone(), progress_callback)
			.await?;

		let target = PathBuf::from(&target_path);
		if let Some(parent) = target.parent() {
			tokio::fs::create_dir_all(parent).await?;
		}
		// std::fs::copy (via tokio::fs::copy) clones on APFS; overwrites any existing target.
		tokio::fs::copy(&cache_path, &target).await.map_err(|e| {
			CacheError::io(format!(
				"Failed to copy cache file {cache_path} to {target_path}: {e}"
			))
		})?;

		Ok(file.into())
	}

	/// Uploads the EXTERNAL `os_path` bytes as new content of an existing file, preserving the
	/// file's stable identity across the SDK's uuid re-mint. Returns the refreshed [`FfiFile`]
	/// (`stable_uuid` == the original) plus its canonical path id post-change.
	pub(crate) async fn modify_file_content(
		&self,
		item: FfiId,
		os_path: String,
		info: Option<UploadFileInfo>,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<FileWithPathResponse, CacheError> {
		let item = self.canonicalize_ffi_id(&item)?;
		debug!("Modifying file content for {} from {os_path}", item.0);

		// Resolve the id (path or trash/<uuid>) to the existing file row.
		let dbfile = match sql::select_object_at_parsed_id(&self.conn(), &item.as_parsed()?)? {
			Some(DBObject::File(file)) => file,
			Some(_) => {
				return Err(CacheError::conversion(format!(
					"Id {} does not point to a file",
					item.0
				)));
			}
			None => {
				return Err(CacheError::DoesNotExist(
					format!("No file found for id: {}", item.0).into(),
				));
			}
		};

		let item_id = dbfile.id;
		let old_uuid = dbfile.uuid;
		let stable_uuid = dbfile.stable_uuid;
		let (old_name, old_mime, old_created) = match &dbfile.meta {
			DBFileMeta::Decoded(meta) => (meta.name.clone(), meta.mime.clone(), meta.created),
			_ => {
				return Err(CacheError::FailedToDecrypt(
					format!("File {old_uuid} metadata is not decoded, cannot modify content")
						.into(),
				));
			}
		};
		let parent_uuid: Uuid = dbfile.parent.try_into().map_err(|e| {
			CacheError::conversion(format!(
				"File {old_uuid} parent is not a normal directory: {e}"
			))
		})?;

		// Build the upload from the existing name/mime, overridden by `info` where present.
		let mut builder = FileBuilderOptionalName::new(parent_uuid);
		let name = info.as_ref().map(|i| i.name.clone()).unwrap_or(old_name);
		builder.name(&name)?;
		let mime = info
			.as_ref()
			.and_then(|i| i.mime.clone())
			.unwrap_or(old_mime);
		builder.mime(mime);
		// Preserve the original creation date unless overridden — a content edit must not reset it.
		if let Some(created) = info.as_ref().and_then(|i| i.creation).or(old_created) {
			builder.created(DateTime::from_timestamp_millis(created).ok_or_else(|| {
				CacheError::conversion(format!("Invalid creation timestamp {created}"))
			})?);
		}
		if let Some(modification) = info.as_ref().and_then(|i| i.modification) {
			builder.modified(
				DateTime::from_timestamp_millis(modification).ok_or_else(|| {
					CacheError::conversion(format!("Invalid modification timestamp {modification}"))
				})?,
			);
		}

		// Upload the external replica bytes (NOT the cache copy). This re-mints the uuid.
		let os_path = PathBuf::from(os_path);
		let (remote_file, _) = self
			.io_upload_file(os_path.clone(), builder, progress_callback)
			.await?;
		let new_uuid = remote_file.uuid();

		// Refresh the cache copy from the external bytes, dropping the stale <old-uuid> cache dir. This
		// is a best-effort optimization: the upload already succeeded, so on failure we log and press
		// on to the DB upsert — `fetchContents` re-downloads on a hash mismatch anyway. Aborting here
		// would strand the successful upload without its stable-identity carry.
		if let Err(e) = self
			.io_refresh_cache_from_external(&os_path, &remote_file, old_uuid)
			.await
		{
			tracing::warn!(
				"Failed to refresh cache copy for {new_uuid} after upload (fetchContents will re-download): {e}"
			);
		}

		// Upsert the re-minted file, guaranteeing the original stable_uuid is carried onto its row.
		let file = DBFile::upsert_remint_preserving_stable(
			&mut self.conn(),
			remote_file,
			item_id,
			old_uuid,
			stable_uuid,
		)?;

		// Canonical path id of the file after the edit (reflects any rename carried in `info`).
		let id = self.canonicalize_ffi_id(&FfiId(format!("uuid/{new_uuid}")))?;
		Ok(FileWithPathResponse {
			file: file.into(),
			id,
		})
	}

	/// The delta feed backing the replicated extension's `enumerateChanges(for:from:)`. With no
	/// server-side delta cursor, when `refresh` is set the container is re-listed from the backend
	/// first (updating the local cache and, via the Phase-4 triggers, `seq`/`deletions`), then the
	/// change since the caller's anchor is diffed out of the local DB.
	///
	/// `container` is a dir/root uuid (stable or real), the `"trash"` sentinel, or `"workingset"`.
	/// A malformed anchor or one from a prior DB generation yields `anchor_expired = true` (with a
	/// fresh anchor and empty lists) rather than an error, so the system re-enumerates from scratch.
	pub(crate) async fn enumerate_changes(
		&self,
		container: String,
		from_anchor: Vec<u8>,
		refresh: bool,
	) -> Result<FfiChangesResponse, CacheError> {
		debug!("Enumerating changes for container {container} (refresh: {refresh})");

		// Decode the anchor and confirm it belongs to this DB generation.
		let current_epoch = {
			let conn = self.conn();
			sql::changes::current_anchor(&conn)?.epoch
		};
		let from_seq = match sql::changes::SyncAnchor::from_bytes(&from_anchor) {
			Some(anchor) if anchor.epoch == current_epoch => anchor.seq,
			// Malformed length or a stale epoch (rebuilt cache DB): expired, not an error.
			_ => {
				let new_anchor = {
					let conn = self.conn();
					sql::changes::current_anchor(&conn)?.to_bytes()
				};
				return Ok(FfiChangesResponse {
					updated: Vec::new(),
					deleted_stable_uuids: Vec::new(),
					new_anchor,
					anchor_expired: true,
				});
			}
		};

		// Classify the container. A dir/root uuid may be stable, so resolve to the real uuid that
		// the `items.parent` column stores.
		enum Container {
			Dir(Uuid),
			Trash,
			WorkingSet,
		}
		let container_kind = if container == "workingset" {
			Container::WorkingSet
		} else if container == "trash" {
			Container::Trash
		} else {
			let parsed = UuidStr::from_str(&container).map_err(|e| {
				CacheError::conversion(format!("Invalid container uuid {container}: {e}"))
			})?;
			let conn = self.conn();
			Container::Dir(self.resolve_uuid(&conn, parsed.into())?)
		};

		// A dir container that is actually a FILE has no children — return an empty, non-error response
		// at the current anchor (documents are enumerated for content, not children).
		if let Container::Dir(uuid) = &container_kind {
			let uuid = *uuid;
			let is_file = {
				let conn = self.conn();
				matches!(
					DBObject::select(&conn, uuid).optional()?,
					Some(DBObject::File(_))
				)
			};
			if is_file {
				debug!(
					"Container {uuid} is a file; documents have no children, returning empty diff"
				);
				let new_anchor = {
					let conn = self.conn();
					sql::changes::current_anchor(&conn)?.to_bytes()
				};
				return Ok(FfiChangesResponse {
					updated: Vec::new(),
					deleted_stable_uuids: Vec::new(),
					new_anchor,
					anchor_expired: false,
				});
			}
		}

		// current before diffing.
		if refresh {
			match &container_kind {
				Container::Dir(uuid) => {
					let uuid = *uuid;
					let root_uuid = self.client.root().uuid();
					// Classify under the conn lock, then drop it before awaiting (never hold the std
					// Mutex guard across an await).
					let container = {
						let conn = self.conn();
						sql::classify_item_container(&conn, uuid, root_uuid)
					};
					match container {
						// Root-reachable: `update_dir_children` canonicalizes the uuid id to a path itself.
						Ok(sql::ItemContainer::Root) => {
							self.update_dir_children(&FfiId(format!("uuid/{uuid}")))
								.await?;
						}
						// Trash-nested: can't be refreshed by path, but the local delta is still valid.
						Ok(sql::ItemContainer::Trash) => {
							debug!(
								"Container {uuid} is trash-nested; skipping refresh, serving local diff"
							);
						}
						// The ancestor chain isn't fully cached, so the path can't be canonicalized.
						// Refresh the target directly by uuid instead of serving a stale diff with no
						// server re-list (which would hide remote changes to this container).
						Err(CacheError::DoesNotExist(_)) => {
							debug!(
								"Container {uuid} has an uncached ancestor chain; refreshing by uuid"
							);
							self.refresh_dir_children_by_uuid(uuid).await?;
						}
						Err(e) => return Err(e),
					}
				}
				Container::Trash => self.update_trash().await?,
				Container::WorkingSet => {
					self.update_recents().await?;
					self.update_trash().await?;
				}
			}
		}

		// Diff and read the new anchor under a single connection lock, so a mutation that lands
		// between the query and the anchor read is not skipped by the next enumeration.
		let (updated, deleted_stable_uuids, new_anchor) = {
			let conn = self.conn();
			let (updated, deleted) = match &container_kind {
				Container::Dir(uuid) => (
					sql::changes::select_changed_children(&conn, *uuid, from_seq)?,
					sql::changes::select_deletions_by_parent(&conn, *uuid, from_seq)?,
				),
				// Trash is addressed by the `trashed` flag, not a parent uuid (trashed rows keep their
				// original `parent`). Deletions from trash can't be scoped by the tombstone's `parent`
				// column (it records the original parent, not "trash"), so the safe superset of all
				// tombstones since the anchor is used — a stable_uuid the trash enumerator never vended
				// is simply ignored by the extension.
				Container::Trash => (
					sql::changes::select_changed_trash(&conn, from_seq)?,
					sql::changes::select_deletions_all(&conn, from_seq)?,
				),
				Container::WorkingSet => (
					sql::changes::select_changed_workingset(&conn, from_seq)?,
					sql::changes::select_deletions_all(&conn, from_seq)?,
				),
			};
			let new_anchor = sql::changes::current_anchor(&conn)?.to_bytes();
			(updated, deleted, new_anchor)
		};

		Ok(FfiChangesResponse {
			updated: updated.into_iter().map(Into::into).collect(),
			deleted_stable_uuids,
			new_anchor,
			anchor_expired: false,
		})
	}

	async fn inner_download_file_if_changed(
		&self,
		old_file: Option<DBFile>,
		file: DBFile,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		let file: RemoteFile = file.try_into()?;
		match (
			file.hash(),
			self.hash_local_file(file.uuid(), file.name()).await,
		) {
			(Some(remote_hash), Ok(Some(local_hash))) => {
				// Remote file has a hash and local file exists
				if remote_hash == local_hash {
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
				if let Some(old_file) = old_file
					&& old_file == file
				{
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
				// Local file does not exist
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

	/// Re-list a directory's children addressed purely by its uuid, for when the ancestor chain isn't
	/// fully cached so `update_dir_children` can't canonicalize a path. Fetches the dir row from the
	/// backend, then lists its children. A remote-deleted dir (404) is a no-op.
	async fn refresh_dir_children_by_uuid(&self, uuid: Uuid) -> Result<(), CacheError> {
		let Some(remote_dir) = self.get_dir_or_none(uuid).await? else {
			return Ok(());
		};
		let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
		self.inner_update_dir(&mut DBDirObject::Dir(dir)).await?;
		Ok(())
	}
}

/// The synthetic local object for the trash container. The trash `items` row (`uuid = "trash"`) is
/// a constant seed (name "Trash", timestamp 0) whose "uuid" is not a real `UuidStr`, so it is built
/// directly rather than read back through the uuid-typed DB readers.
fn trash_ffi_object() -> FfiObject {
	let trash = "trash".to_string();
	FfiObject::Dir(FfiDir {
		uuid: trash.clone(),
		stable_uuid: trash,
		parent: String::new(),
		original_parent: None,
		meta: Some(FfiDirMeta {
			name: "Trash".to_string(),
			created: None,
		}),
		color: None,
		favorite_rank: 0,
		timestamp: 0,
		last_listed: 0,
		local_data: None,
	})
}
