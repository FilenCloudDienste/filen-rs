use std::{
	path::{Path, PathBuf},
	str::FromStr,
	sync::{Arc, Mutex, MutexGuard},
};

use chrono::DateTime;
use ffi::{FfiDir, FfiFile, FfiNonRootObject, FfiObject, FfiRoot};
use filen_sdk_rs::{
	fs::{
		HasName, HasParent, HasUUID,
		dir::{RemoteDirectory, traits::HasDirMeta},
		file::{RemoteFile, traits::HasFileMeta},
	},
	util::PathIteratorExt,
};
use filen_types::fs::{ParentUuid, UuidStr};
use log::debug;
use rusqlite::{Connection, OptionalExtension};

use crate::{
	ffi::FfiPathWithRoot,
	sql::{
		DBDir, DBDirExt, DBDirObject, DBDirTrait, DBFile, DBItemExt, DBItemTrait, DBNonRootObject,
		DBObject, DBRoot, error::OptionalExtensionSQL,
	},
	sync::UpdateItemsInPath,
	traits::ProgressCallback,
};

uniffi::setup_scaffolding!();

pub mod env;
mod error;
pub mod ffi;
pub mod io;
pub mod sql;
pub(crate) mod sync;
pub use error::CacheError;
pub mod thumbnail;
pub mod traits;

pub type Result<T> = std::result::Result<T, CacheError>;

#[derive(uniffi::Object)]
pub struct FilenMobileCacheState {
	conn: Mutex<Connection>,
	tmp_dir: PathBuf,
	cache_dir: PathBuf,
	thumbnail_dir: PathBuf,
	client: filen_sdk_rs::auth::Client,
}

#[uniffi::export]
impl FilenMobileCacheState {
	#[uniffi::constructor(name = "login")]
	pub async fn login(
		email: String,
		password: &str,
		two_factor_code: &str,
		files_dir: &str,
	) -> Result<Self> {
		debug!("Logging in with email: {email}");
		env::init_logger();
		let db = Connection::open(AsRef::<Path>::as_ref(files_dir).join("native_cache.db"))?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		let (cache_dir, tmp_dir, thumbnail_dir) = io::init(files_dir.as_ref())?;
		let client = filen_sdk_rs::auth::Client::login(email, password, two_factor_code).await?;
		Ok(Self {
			client,
			conn: Mutex::new(db),
			cache_dir,
			tmp_dir,
			thumbnail_dir,
		})
	}

	#[uniffi::constructor(name = "from_strings_in_file")]
	pub fn from_strings_in_file(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
		files_dir: &str,
	) -> Result<Self> {
		debug!("Creating FilenMobileCacheState from strings for email: {email}");

		env::init_logger();
		let client = filen_sdk_rs::auth::Client::from_strings(
			email,
			root_uuid,
			auth_info,
			private_key,
			api_key,
			version,
		)?;

		let (cache_dir, tmp_dir, thumbnail_dir) = io::init(files_dir.as_ref())?;
		let db_path = AsRef::<Path>::as_ref(files_dir).join("native_cache.db");
		let db = Connection::open(&db_path)?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		let new = FilenMobileCacheState {
			client,
			conn: Mutex::new(db),
			cache_dir,
			tmp_dir,
			thumbnail_dir,
		};
		new.add_root(root_uuid)?;
		Ok(new)
	}

	#[uniffi::constructor(name = "from_strings")]
	pub fn from_strings_in_memory(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
		files_dir: &str,
	) -> Result<Self> {
		debug!("Creating FilenMobileCacheState from strings for email: {email}");
		env::init_logger();
		let client = filen_sdk_rs::auth::Client::from_strings(
			email,
			root_uuid,
			auth_info,
			private_key,
			api_key,
			version,
		)?;

		let (cache_dir, tmp_dir, thumbnail_dir) = io::init(files_dir.as_ref())?;
		let db = Connection::open_in_memory()?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		let new = FilenMobileCacheState {
			client,
			conn: Mutex::new(db),
			cache_dir,
			tmp_dir,
			thumbnail_dir,
		};
		new.add_root(root_uuid)?;
		Ok(new)
	}

	pub fn root_uuid(&self) -> String {
		self.client.root().uuid().to_string()
	}
}

impl FilenMobileCacheState {
	pub fn conn(&self) -> MutexGuard<Connection> {
		self.conn.lock().unwrap()
	}

	pub fn cache_dir(&self) -> &Path {
		&self.cache_dir
	}
}

#[derive(uniffi::Record, Debug)]
pub struct QueryChildrenResponse {
	pub objects: Vec<FfiNonRootObject>,
	pub parent: FfiDir,
}

#[uniffi::export]
impl FilenMobileCacheState {
	pub fn query_roots_info(&self, root_uuid_str: String) -> Result<Option<FfiRoot>> {
		debug!("Querying root info for UUID: {root_uuid_str}");
		let conn = self.conn();
		Ok(DBRoot::select(&conn, UuidStr::from_str(&root_uuid_str)?)
			.optional()?
			.map(Into::into))
	}

	pub fn add_root(&self, root: &str) -> Result<()> {
		debug!("Adding root with UUID: {root}");
		let root_uuid = UuidStr::from_str(root)?;
		let mut conn = self.conn();
		sql::insert_root(&mut conn, root_uuid)?;
		Ok(())
	}

	pub fn query_dir_children(
		&self,
		path: &FfiPathWithRoot,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>> {
		let path_values = path.as_path_values()?;
		debug!(
			"Querying directory children at path: {}",
			path_values.full_path
		);

		let dir: DBDirObject = match sql::select_object_at_path(&self.conn(), &path_values)? {
			Some(obj) => obj.try_into()?,
			None => return Ok(None),
		};

		let conn = self.conn();
		let children = dir.select_children(&conn, order_by.as_deref())?;
		Ok(Some(QueryChildrenResponse {
			parent: dir.into(),
			objects: children.into_iter().map(Into::into).collect(),
		}))
	}

	pub fn query_item(&self, path: &FfiPathWithRoot) -> Result<Option<FfiObject>> {
		debug!("Querying item at path: {}", path.0);
		let path_values = path.as_maybe_trash_values()?;
		let obj = sql::select_maybe_trashed_object_at_path(&self.conn(), &path_values)?;

		let dir_obj = match obj {
			Some(DBObject::Dir(dbdir)) => DBDirObject::Dir(dbdir),
			Some(DBObject::Root(dbroot)) => DBDirObject::Root(dbroot),
			other => return Ok(other.map(Into::into)),
		};
		// stop error for ios complaining that folder doesn't exist
		match std::fs::create_dir_all(self.cache_dir.join(dir_obj.uuid().as_ref())) {
			Ok(_) => {}
			Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
			Err(e) => {
				return Err(CacheError::io(format!(
					"Failed to create directory for {}: {e}",
					dir_obj.uuid()
				)));
			}
		}
		Ok(Some(FfiObject::from(DBObject::from(dir_obj))))
	}

	pub fn query_path_for_uuid(&self, uuid: String) -> Result<Option<FfiPathWithRoot>> {
		debug!("Querying path for UUID: {uuid}");
		if uuid == self.root_uuid() {
			return Ok(Some(uuid.into()));
		}
		let uuid = UuidStr::from_str(&uuid)?;
		let conn = self.conn();
		let path = sql::recursive_select_path_from_uuid(&conn, uuid)?;

		Ok(path.map(|s| FfiPathWithRoot(format!("{}{}", self.client.root().uuid(), s))))
	}

	pub fn get_all_descendant_paths(&self, path: &FfiPathWithRoot) -> Result<Vec<FfiPathWithRoot>> {
		debug!("Getting all descendant paths for: {}", path.0);
		let path_values = path.as_path_values()?;
		let obj = sql::select_object_at_path(&self.conn(), &path_values)?;
		Ok(match obj {
			Some(obj) => sql::get_all_descendant_paths(&self.conn(), obj.uuid(), &path.0)?
				.into_iter()
				.map(FfiPathWithRoot)
				.collect(),
			None => vec![],
		})
	}
}

#[derive(uniffi::Record)]
pub struct DownloadResponse {
	pub path: String,
	pub file: FfiFile,
}

#[filen_sdk_rs_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	pub async fn update_roots_info(&self) -> Result<()> {
		debug!(
			"Updating roots info for client: {}",
			self.client.root().uuid()
		);
		let resp = self.client.get_user_info().await?;
		let conn = self.conn();
		sql::update_root(&conn, self.client.root().uuid(), &resp)?;
		Ok(())
	}

	pub async fn update_dir_children(&self, path: FfiPathWithRoot) -> Result<()> {
		debug!("Updating directory children for path: {}", path.0);
		let path_values = path.as_path_values()?;
		let mut dir: DBDirObject = match self.update_items_in_path(&path_values).await? {
			UpdateItemsInPath::Complete(dbobject) => dbobject.try_into()?,
			UpdateItemsInPath::Partial(_, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a directory",
					path_values.full_path
				)));
			}
		};
		let (dirs, files) = self.client.list_dir(&dir.uuid()).await?;
		let mut conn = self.conn();
		dir.update_children(&mut conn, dirs, files)?;
		dir.update_dir_last_listed_now(&conn)?;
		Ok(())
	}

	pub async fn update_and_query_dir_children(
		&self,
		path: FfiPathWithRoot,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>> {
		debug!(
			"Updating and querying directory children for path: {}",
			path.0
		);
		self.update_dir_children(path.clone()).await?;
		self.query_dir_children(&path, order_by)
	}

	pub async fn download_file_if_changed_by_path(
		&self,
		file_path: FfiPathWithRoot,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<String> {
		debug!("Downloading file to path: {}", file_path.0);
		let path_values = file_path.as_path_values()?;
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

	pub async fn download_file_if_changed_by_uuid(
		&self,
		uuid: String,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<String> {
		debug!("Downloading file with UUID: {uuid}");
		let uuid = UuidStr::from_str(&uuid).unwrap();
		let file = DBFile::select(&self.conn(), uuid)
			.optional()?
			.ok_or_else(|| CacheError::remote(format!("No file found with UUID: {uuid}")))?;
		// unnecesssary clone but better than redownloading
		self.inner_download_file_if_changed(Some(file.clone()), file, progress_callback)
			.await
	}

	pub async fn upload_file_if_changed(
		&self,
		path: FfiPathWithRoot,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<bool> {
		debug!("Uploading file at path: {}", path.0);
		let path_values = path.as_path_values()?;
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
					Some(progress_callback),
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

	pub async fn upload_new_file(
		&self,
		os_path: String,
		parent_path: FfiPathWithRoot,
		info: UploadFileInfo,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<FileWithPathResponse> {
		let os_path = PathBuf::from(os_path);
		let name = info.name;
		let out_path = parent_path.join(&name);
		debug!(
			"Creating file at path: {}, importing from {}",
			out_path.0,
			os_path.display()
		);
		let parent_pvs = parent_path.as_path_values()?;
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
			.io_upload_file(file.build(), os_file, Some(progress_callback))
			.await?;

		let file = DBFile::upsert_from_remote(&mut self.conn(), remote_file)?;

		Ok(FileWithPathResponse {
			id: out_path,
			file: file.into(),
		})
	}

	pub async fn create_empty_file(
		&self,
		parent_path: FfiPathWithRoot,
		name: String,
		mime: String,
	) -> Result<CreateFileResponse> {
		let file_path = parent_path.join(&name);
		debug!("Creating empty file at path: {}", file_path.0);
		let parent_pvs = parent_path.as_path_values()?;
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
		let pvs = path.as_path_values()?;
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

	pub async fn create_dir(
		&self,
		parent_path: FfiPathWithRoot,
		name: String,
		created: Option<i64>,
	) -> Result<DirWithPathResponse> {
		let dir_path = parent_path.join(&name);
		debug!("Creating directory at path: {}", dir_path.0);
		let path_values = parent_path.as_path_values()?;
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

	pub async fn trash_item(&self, path: FfiPathWithRoot) -> Result<ObjectWithPathResponse> {
		debug!("Trashing item at path: {}", path.0);
		let path_values: PathValues<'_> = path.as_path_values()?;
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
				let remote_dir = dir.into();
				self.client.trash_dir(&remote_dir).await?;
				let dir: DBDir = remote_dir.into();
				self.io_delete_local(&dir).await?;
				dir.trash(&self.conn())?;
				DBObject::Dir(dir)
			}
			DBObject::File(file) => {
				let remote_file = file.try_into()?;
				self.client.trash_file(&remote_file).await?;
				let file: DBFile = remote_file.into();
				self.io_delete_local(&file).await?;
				file.trash(&self.conn())?;
				DBObject::File(file)
			}
		};
		Ok(ObjectWithPathResponse {
			id: FfiPathWithRoot(format!("trash/{}", obj.uuid())),
			object: obj.into(),
		})
	}

	pub async fn restore_item(
		&self,
		uuid: &str,
		to: Option<FfiPathWithRoot>,
	) -> Result<ObjectWithPathResponse> {
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
				let to_pvs: PathValues<'_> = to_path.as_path_values()?;
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
				let remote_file = file.try_into()?;
				self.client.restore_file(&remote_file).await?;
				let remote_file = self.client.get_file(remote_file.uuid()).await?;
				let mut conn = self.conn();
				let file = DBFile::upsert_from_remote(&mut conn, remote_file)?;
				DBNonRootObject::File(file)
			}
			DBNonRootObject::Dir(dir) => {
				let remote_dir: RemoteDirectory = dir.into();
				self.client.restore_dir(&remote_dir).await?;
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
				id: FfiPathWithRoot(format!("{}{}", self.client.root().uuid(), s)),
				object: DBObject::from(object).into(),
			})
	}

	pub async fn move_item(
		&self,
		item: FfiPathWithRoot,
		new_parent: FfiPathWithRoot,
	) -> Result<ObjectWithPathResponse> {
		debug!("Moving item {} to new parent {}", item.0, new_parent.0);
		let item_pvs: PathValues<'_> = item.as_path_values()?;
		let new_parent_pvs: PathValues<'_> = new_parent.as_path_values()?;

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
					UpdateItemsInPath::Partial(_, _) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to an item",
							item_pvs.full_path
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
					UpdateItemsInPath::Partial(_, _) => Err(CacheError::remote(format!(
						"Path {} does not point to an item",
						item_pvs.full_path
					))),
				}
			}
		)?;

		let obj = self.inner_move_item(obj, new_parent_dir.uuid()).await?;
		Ok(ObjectWithPathResponse {
			object: DBObject::from(obj).into(),
			id: new_parent.join(item_pvs.name),
		})
	}

	pub async fn rename_item(
		&self,
		item: FfiPathWithRoot,
		new_name: String,
	) -> Result<Option<ObjectWithPathResponse>> {
		debug!("Renaming item {} to {}", item.0, new_name);
		let item_pvs: PathValues<'_> = item.as_path_values()?;
		if item_pvs.name.is_empty() {
			return Err(CacheError::remote(format!(
				"Cannot rename item: {}",
				item.0
			)));
		} else if item_pvs.name == new_name {
			return Ok(None);
		}
		self.update_dir_children(item.parent()).await?;
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
				let id = dbdir.id;
				let mut remote_dir: RemoteDirectory = dbdir.into();
				let mut meta = remote_dir.get_meta();
				meta.set_name(&new_name)?;
				self.client
					.update_dir_metadata(&mut remote_dir, meta)
					.await?;
				sql::rename_item(&mut self.conn(), id, &new_name, remote_dir.parent())?;
				DBObject::Dir(remote_dir.into())
			}
			DBNonRootObject::File(dbfile) => {
				let id = dbfile.id;
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				let mut meta = remote_file.get_meta();
				meta.set_name(&new_name)?;
				self.client
					.update_file_metadata(&mut remote_file, meta)
					.await?;
				sql::rename_item(&mut self.conn(), id, &new_name, remote_file.parent())?;
				DBObject::File(remote_file.into())
			}
		};
		Ok(Some(ObjectWithPathResponse {
			object: obj.into(),
			id: new_path,
		}))
	}

	pub async fn clear_local_cache(&self, item: FfiPathWithRoot) -> Result<()> {
		let pvs = item.as_path_values()?;
		debug!("Clearing local cache for item: {}", pvs.full_path);
		let obj = match sql::select_object_at_path(&self.conn(), &pvs)? {
			Some(obj) => obj,
			None => return Ok(()),
		};
		self.io_delete_local(&obj).await?;
		Ok(())
	}

	pub async fn clear_local_cache_by_uuid(&self, uuid: &str) -> Result<()> {
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
		self.io_delete_local(&obj).await?;
		Ok(())
	}

	pub async fn delete_item(&self, item: FfiPathWithRoot) -> Result<()> {
		debug!("Deleting object at path: {}", item.0);
		let pvs = item.as_maybe_trash_values()?;
		let obj = match pvs {
			MaybeTrashValues::Trash(_) => {
				sql::select_maybe_trashed_object_at_path(&self.conn(), &pvs)?
			}
			MaybeTrashValues::Path(path_values) => {
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
				self.io_delete_local(&dir).await?;
				let remote_dir: RemoteDirectory = dir.into();
				let uuid = remote_dir.uuid();
				self.client.delete_dir_permanently(remote_dir).await?;
				sql::delete_item(&self.conn(), uuid)?;
			}
			DBObject::File(file) => {
				self.io_delete_local(&file).await?;
				let remote_file: RemoteFile = file.try_into()?;
				let uuid = remote_file.uuid();
				self.client.delete_file_permanently(remote_file).await?;
				sql::delete_item(&self.conn(), uuid)?;
			}
		}
		debug!("Successfully deleted item at path: {}", item.0);
		Ok(())
	}

	pub async fn set_favorite_rank(
		&self,
		item: FfiPathWithRoot,
		favorite_rank: i64,
	) -> Result<ObjectWithPathResponse> {
		let pvs = item.as_maybe_trash_values()?;
		debug!(
			"Setting favorite rank for item: {}, rank: {}",
			item.0, favorite_rank
		);
		let obj = match pvs {
			MaybeTrashValues::Trash(_) => {
				sql::select_maybe_trashed_object_at_path(&self.conn(), &pvs)?
			}
			MaybeTrashValues::Path(path_values) => {
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
					dbfile = remote_file.into();
				}
				// update local favorite rank
				dbfile.update_favorite_rank(&self.conn(), favorite_rank)?;
				DBObject::File(dbfile)
			}
			DBObject::Dir(mut dbdir) if favorite_rank != dbdir.favorite_rank => {
				if (favorite_rank > 0) != (dbdir.favorite_rank > 0) {
					// update server-side favorite status
					let mut remote_file: RemoteDirectory = dbdir.into();
					self.client
						.set_favorite(&mut remote_file, favorite_rank > 0)
						.await?;
					dbdir = remote_file.into();
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
}

impl FilenMobileCacheState {
	async fn inner_download_file_if_changed(
		&self,
		old_file: Option<DBFile>,
		file: DBFile,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<String> {
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
			.download_file_io(&file, Some(progress_callback))
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
	) -> Result<DBNonRootObject> {
		match item {
			DBNonRootObject::Dir(dir) => {
				let mut remote_dir: RemoteDirectory = dir.into();
				self.client
					.move_dir(&mut remote_dir, &new_parent_uuid)
					.await?;
				let mut conn = self.conn();
				sql::move_item(
					&mut conn,
					remote_dir.uuid(),
					remote_dir.name(),
					new_parent_uuid.into(),
				)?;
				Ok(DBNonRootObject::Dir(remote_dir.into()))
			}
			DBNonRootObject::File(file) => {
				let mut remote_file: RemoteFile = file.try_into()?;
				self.client
					.move_file(&mut remote_file, &new_parent_uuid)
					.await?;
				let mut conn = self.conn();
				sql::move_item(
					&mut conn,
					remote_file.uuid(),
					remote_file.name(),
					new_parent_uuid.into(),
				)?;
				Ok(DBNonRootObject::File(remote_file.into()))
			}
		}
	}
}

#[derive(uniffi::Record)]
pub struct CreateFileResponse {
	pub path: String,
	pub file: FfiFile,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct FileWithPathResponse {
	pub file: FfiFile,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct DirWithPathResponse {
	pub dir: FfiDir,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct ObjectWithPathResponse {
	pub object: FfiObject,
	pub id: FfiPathWithRoot,
}

#[derive(uniffi::Record, Debug)]
pub struct UploadFileInfo {
	pub name: String,
	pub creation: Option<i64>,
	pub modification: Option<i64>,
	pub mime: Option<String>,
}

#[derive(Debug)]
pub struct PathValues<'a> {
	pub root_uuid: UuidStr,
	pub full_path: &'a str,
	pub inner_path: &'a str,
	pub name: &'a str,
}

#[derive(Debug)]
pub struct TrashValues<'a> {
	pub full_path: &'a str,
	pub inner_path: &'a str,
	pub uuid: UuidStr,
}

#[derive(Debug)]
pub enum MaybeTrashValues<'a> {
	Trash(TrashValues<'a>),
	Path(PathValues<'a>),
}

impl FfiPathWithRoot {
	pub fn as_path_values(&self) -> Result<PathValues> {
		let mut iter = self.0.path_iter();
		let (root_uuid_str, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must start with a root UUID"))?;

		Ok(PathValues {
			root_uuid: UuidStr::from_str(root_uuid_str).map_err(|e| {
				CacheError::conversion(format!("Invalid root UUID: {root_uuid_str} error: {e} "))
			})?,
			full_path: self.0.as_str(),
			inner_path: remaining,
			name: iter.last().unwrap_or_default().0,
		})
	}

	pub fn as_maybe_trash_values(&self) -> Result<MaybeTrashValues> {
		let mut iter = self.0.path_iter();
		let (root_uuid_str, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must start with a root UUID"))?;

		match root_uuid_str {
			"trash" => Ok(MaybeTrashValues::Trash(TrashValues {
				full_path: self.0.as_str(),
				inner_path: remaining,
				uuid: UuidStr::from_str(iter.last().unwrap_or_default().0)?,
			})),
			_ => Ok(MaybeTrashValues::Path(self.as_path_values()?)),
		}
	}
}
