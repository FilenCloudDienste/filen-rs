use std::{path::PathBuf, str::FromStr, sync::Arc, time::Instant};

use chrono::DateTime;
use filen_sdk_rs::fs::{
	HasUUID,
	dir::{RemoteDirectory, traits::HasDirMeta},
	file::{RemoteFile, traits::HasFileMeta},
};
use filen_types::fs::{ParentUuid, UuidStr};
use log::debug;
use rusqlite::OptionalExtension;

use crate::{
	CacheError,
	auth::{AuthCacheState, FilenMobileCacheState},
	ffi::{
		CreateFileResponse, DirWithPathResponse, FfiId, FileWithPathResponse,
		ObjectWithPathResponse, ParsedFfiId, PathFfiId, QueryChildrenResponse,
		QueryNonDirChildrenResponse, UploadFileInfo,
	},
	sql::{
		self, DBDir, DBDirExt, DBDirObject, DBDirTrait, DBFile, DBItemTrait, DBNonRootObject,
		DBObject, ItemType, error::OptionalExtensionSQL,
	},
	sync::UpdateItemsInPath,
	traits::ProgressCallback,
};

// yes this should be done with macros
// no I didn't have time
#[filen_sdk_rs_macros::create_uniffi_wrapper]
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
		mime: String,
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
		let (dirs, files) = self.client.list_dir(&ParentUuid::Recents).await?;
		println!("Updating recents with {dirs:?} dirs and {files:?} files");
		sql::update_recents(&mut self.conn(), dirs, files)?;
		self.last_recents_update
			.write()
			.unwrap()
			.replace(Instant::now());
		Ok(())
	}

	pub(crate) async fn update_trash(&self) -> Result<(), CacheError> {
		let (dirs, files) = self.client.list_dir(&ParentUuid::Trash).await?;
		println!("Updating recents with {dirs:?} dirs and {files:?} files");
		sql::update_items_with_parent(&mut self.conn(), dirs, files, ParentUuid::Trash)?;
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
		let uuid = UuidStr::from_str(&uuid).unwrap();
		let file = DBFile::select(&self.conn(), uuid)
			.optional()?
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
		debug!("Uploading file at path: {}", path.0);
		let path_values = path.as_path()?;
		let remote_file = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => {
				if let Some(hash) = file.hash {
					let local_hash = self.hash_local_file(file.uuid.as_ref()).await?;
					if local_hash == Some(hash.into()) {
						return Ok(false);
					}
				}

				self.io_upload_updated_file(
					file.uuid.as_ref(),
					path_values.name,
					file.parent.try_into().map_err(|e| {
						CacheError::conversion(format!("Failed to convert parent UUID: {e}"))
					})?,
					file.mime,
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
			UpdateItemsInPath::Partial(remaining, parent) if remaining == path_values.name => {
				self.io_upload_new_file(path_values.name, parent.uuid(), None)
					.await?
					.0
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

		let mut file = self.client.make_file_builder(name, &parent.uuid());
		if let Some(creation) = info.creation {
			file = file.created(DateTime::from_timestamp_millis(creation).ok_or_else(|| {
				CacheError::conversion(format!(
					"Failed to convert creation timestamp {creation} to DateTime"
				))
			})?);
		}
		if let Some(modification) = info.modification {
			file = file.modified(DateTime::from_timestamp_millis(modification).ok_or_else(
				|| {
					CacheError::conversion(format!(
						"Failed to convert modification timestamp {modification} to DateTime"
					))
				},
			)?);
		}
		if let Some(mime) = info.mime {
			file = file.mime(mime);
		}

		let os_file = tokio::fs::File::open(&os_path).await?;

		let remote_file = self
			.io_upload_file(file.build(), os_file, progress_callback)
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
		mime: String,
	) -> Result<CreateFileResponse, CacheError> {
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
		let path = parent_path.join(&name);
		let pvs = path.as_path()?;
		let (file, os_path) = self
			.io_upload_new_file(pvs.name, parent.uuid(), Some(mime))
			.await?;
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

		let dir = match created {
			Some(time) => {
				self.client
					.create_dir_with_created(
						&parent.uuid(),
						name,
						DateTime::from_timestamp_millis(time).ok_or_else(|| {
							CacheError::conversion(format!(
								"Failed to convert timestamp {time} to DateTime"
							))
						})?,
					)
					.await?
			}
			None => self.client.create_dir(&parent.uuid(), name).await?,
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
				self.io_delete_local(remote_dir.uuid(), ItemType::Dir)
					.await?;
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				DBObject::Dir(dir)
			}
			DBObject::File(file) => {
				let mut remote_file = file.try_into()?;
				self.client.trash_file(&mut remote_file).await?;
				self.io_delete_local(remote_file.uuid(), ItemType::File)
					.await?;
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
		let uuid = UuidStr::from_str(uuid)
			.map_err(|e| CacheError::conversion(format!("Invalid UUID {uuid}, err: {e}")))?;
		let object = {
			let conn = self.conn();
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

		if !object.parent().is_some_and(|p| p == ParentUuid::Trash) {
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

		if let Some((parent, parent_path)) = parent {
			if object.certain_parent() != parent.uuid() {
				let new_path = parent_path.join(object.name());
				let item = self.inner_move_item(object, parent.uuid()).await?;
				return Ok(ObjectWithPathResponse {
					object: DBObject::from(item).into(),
					id: new_path,
				});
			}
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

		let obj = self.inner_move_item(obj, new_parent_dir.uuid()).await?;
		Ok(ObjectWithPathResponse {
			object: DBObject::from(obj).into(),
			id: new_parent.join(item_pvs.name),
		})
	}

	pub(crate) async fn rename_item(
		&self,
		item: FfiId,
		new_name: String,
	) -> Result<Option<ObjectWithPathResponse>, CacheError> {
		debug!("Renaming item {} to {}", item.0, new_name);
		let item_pvs: PathFfiId<'_> = item.as_path()?;
		if item_pvs.name.is_empty() {
			return Err(CacheError::remote(format!(
				"Cannot rename item: {}",
				item.0
			)));
		} else if item_pvs.name == new_name {
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
				let mut meta = remote_dir.get_meta();
				meta.set_name(&new_name)?;
				self.client
					.update_dir_metadata(&mut remote_dir, meta)
					.await?;
				let dir = DBDir::upsert_from_remote(&mut self.conn(), remote_dir)?;
				DBObject::Dir(dir)
			}
			DBNonRootObject::File(dbfile) => {
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				let mut meta = remote_file.get_meta();
				meta.set_name(&new_name)?;
				self.client
					.update_file_metadata(&mut remote_file, meta)
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
		let pvs = item.as_path()?;
		debug!("Clearing local cache for item: {}", pvs.full_path);
		let obj = match sql::select_object_at_path(&self.conn(), &pvs)? {
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(obj.uuid(), obj.item_type()).await?;
		Ok(())
	}

	pub(crate) async fn clear_local_cache_by_uuid(&self, uuid: &str) -> Result<(), CacheError> {
		debug!("Clearing local cache for item with uuid: {uuid}");
		let obj = match DBObject::select(
			&self.conn(),
			UuidStr::from_str(uuid)
				.map_err(|e| CacheError::conversion(format!("Invalid UUID {uuid}, err: {e}")))?,
		)
		.optional()?
		{
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(obj.uuid(), obj.item_type()).await?;
		Ok(())
	}

	pub(crate) async fn delete_item(&self, item: FfiId) -> Result<(), CacheError> {
		debug!("Deleting object at path: {}", item.0);
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
				self.io_delete_local(dir.uuid, dir.item_type()).await?;
				let remote_dir: RemoteDirectory = dir.into();
				let uuid = remote_dir.uuid();
				self.client.delete_dir_permanently(remote_dir).await?;
				sql::delete_item(&self.conn(), uuid)?;
			}
			DBObject::File(file) => {
				self.io_delete_local(file.uuid, file.item_type()).await?;
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
						.set_favorite(&mut remote_file, favorite_rank > 0)
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
						.set_favorite(&mut remote_dir, favorite_rank > 0)
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

	async fn inner_download_file_if_changed(
		&self,
		old_file: Option<DBFile>,
		file: DBFile,
		progress_callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<String, CacheError> {
		let file: RemoteFile = file.try_into()?;
		let uuid = file.uuid().to_string();
		match (file.hash, self.hash_local_file(&uuid).await) {
			(Some(remote_hash), Ok(Some(local_hash))) => {
				// Remote file has a hash and local file exists
				if remote_hash == local_hash {
					return self
						.cache_dir
						.join(uuid)
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
						.cache_dir
						.join(uuid)
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

		let path = self
			.download_file_io(&file, progress_callback)
			.await?
			.into_os_string()
			.into_string()
			.map_err(|e| {
				CacheError::conversion(format!("Failed to convert path to string: {e:?}"))
			})?;
		Ok(path)
	}

	async fn inner_move_item(
		&self,
		item: DBNonRootObject,
		new_parent_uuid: UuidStr,
	) -> Result<DBNonRootObject, CacheError> {
		match item {
			DBNonRootObject::Dir(dir) => {
				let mut remote_dir: RemoteDirectory = dir.into();
				self.client
					.move_dir(&mut remote_dir, &new_parent_uuid)
					.await?;
				let mut conn = self.conn();

				Ok(DBNonRootObject::Dir(DBDir::upsert_from_remote(
					&mut conn, remote_dir,
				)?))
			}
			DBNonRootObject::File(file) => {
				let mut remote_file: RemoteFile = file.try_into()?;
				self.client
					.move_file(&mut remote_file, &new_parent_uuid)
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
		let (dirs, files) = self.client.list_dir(&dir.uuid()).await?;
		let mut conn = self.conn();
		dir.update_dir_last_listed_now(&conn)?;
		dir.update_children(&mut conn, dirs, files)?;
		Ok(())
	}
}
