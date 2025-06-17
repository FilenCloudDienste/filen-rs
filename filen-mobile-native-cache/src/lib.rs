use std::{
	path::{Path, PathBuf},
	str::FromStr,
	sync::{Arc, Mutex, MutexGuard},
};

use ffi::{FfiDir, FfiFile, FfiNonRootObject, FfiObject, FfiRoot};
use filen_sdk_rs::{
	fs::{
		HasName, HasParent, HasUUID,
		dir::{RemoteDirectory, traits::HasDirMeta},
		file::{RemoteFile, traits::HasFileMeta},
	},
	util::PathIteratorExt,
};
use log::debug;
use rusqlite::Connection;
use uuid::Uuid;

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

mod error;
pub mod ffi;
pub mod io;
pub mod sql;
pub(crate) mod sync;
pub mod tokio;
pub use error::CacheError;
pub mod traits;

pub type Result<T> = std::result::Result<T, CacheError>;

#[derive(uniffi::Object)]
pub struct FilenMobileDB {
	conn: Mutex<Connection>,
	files_dir: Mutex<PathBuf>,
}

impl FilenMobileDB {
	pub fn conn(&self) -> MutexGuard<Connection> {
		self.conn.lock().unwrap()
	}

	pub fn files(&self) -> PathBuf {
		self.files_dir.lock().unwrap().clone()
	}
}

#[derive(uniffi::Record)]
pub struct QueryChildrenResponse {
	pub objects: Vec<FfiNonRootObject>,
	pub parent: FfiDir,
}

#[uniffi::export]
impl FilenMobileDB {
	pub fn query_roots_info(&self, root_uuid_str: String) -> Result<Option<FfiRoot>> {
		let conn = self.conn();
		Ok(DBRoot::select(&conn, Uuid::from_str(&root_uuid_str)?)
			.optional()?
			.map(Into::into))
	}

	#[uniffi::constructor]
	pub fn initialize_in_memory() -> Result<FilenMobileDB> {
		let db = Connection::open_in_memory()?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		Ok(FilenMobileDB {
			conn: Mutex::new(db),
			files_dir: Mutex::new(PathBuf::from("")),
		})
	}

	#[uniffi::constructor]
	pub fn initialize_from_files_dir(path: &str) -> Result<FilenMobileDB> {
		let db = Connection::open(AsRef::<Path>::as_ref(path).join("native_cache/cache.db"))?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		Ok(FilenMobileDB {
			conn: Mutex::new(db),
			files_dir: Mutex::new(PathBuf::from(path)),
		})
	}

	pub fn add_root(&self, root: &str) -> Result<()> {
		let root_uuid = Uuid::parse_str(root)?;
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
		let path_values = path.as_path_values()?;
		let obj = sql::select_object_at_path(&self.conn(), &path_values)?;
		Ok(obj.map(Into::into))
	}

	pub fn get_all_descendant_paths(&self, path: &FfiPathWithRoot) -> Result<Vec<FfiPathWithRoot>> {
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

	pub fn set_files_dir(&self, files_dir: String) -> Result<()> {
		let files_dir = PathBuf::from(files_dir);
		if !files_dir.exists() {
			return Err(CacheError::IO(
				format!("Files directory does not exist: {}", files_dir.display()).into(),
			));
		}

		let mut lock = self.files_dir.lock().unwrap_or_else(|e| e.into_inner());
		*lock = files_dir;
		Ok(())
	}
}

#[derive(uniffi::Record)]
pub struct DownloadResponse {
	pub path: String,
	pub file: FfiFile,
}

#[filen_sdk_rs_macros::create_uniffi_wrapper]
impl FilenMobileDB {
	pub async fn update_roots_info(&self, client: &CacheClient) -> Result<()> {
		debug!(
			"Updating roots info for client: {}",
			client.client.root().uuid()
		);
		let resp = client.client.get_user_info().await?;
		let conn = self.conn();
		sql::update_root(&conn, client.client.root().uuid(), &resp)?;
		Ok(())
	}

	pub async fn update_dir_children(
		&self,
		client: &CacheClient,
		path: FfiPathWithRoot,
	) -> Result<()> {
		debug!("Updating directory children for path: {}", path.0);
		let path_values = path.as_path_values()?;
		let mut dir: DBDirObject =
			match sync::update_items_in_path(self, &client.client, &path_values).await? {
				UpdateItemsInPath::Complete(dbobject) => dbobject.try_into()?,
				UpdateItemsInPath::Partial(_, _) => {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a directory",
						path_values.full_path
					)));
				}
			};
		let (dirs, files) = client.client.list_dir(&dir.uuid()).await?;
		let mut conn = self.conn();
		dir.update_children(&mut conn, dirs, files)?;
		dir.update_dir_last_listed_now(&conn)?;
		Ok(())
	}

	pub async fn update_and_query_dir_children(
		&self,
		client: &CacheClient,
		path: FfiPathWithRoot,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>> {
		debug!(
			"Updating and querying directory children for path: {}",
			path.0
		);
		self.update_dir_children(client, path.clone()).await?;
		self.query_dir_children(&path, order_by)
	}

	pub async fn download_file(
		&self,
		client: &CacheClient,
		file_path: FfiPathWithRoot,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<String> {
		debug!("Downloading file at path: {}", file_path.0);
		let path_values = file_path.as_path_values()?;
		let file = match sync::update_items_in_path(self, &client.client, &path_values).await? {
			UpdateItemsInPath::Complete(DBObject::File(file)) => file,
			UpdateItemsInPath::Partial(_, _) | UpdateItemsInPath::Complete(_) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a file",
					path_values.full_path
				)));
			}
		};
		let file: RemoteFile = file.try_into()?;

		let files_path = self.files();
		let path = io::download_file(
			&client.client,
			&file,
			&path_values,
			Some(progress_callback),
			&files_path,
		)
		.await?
		.into_os_string()
		.into_string()
		.map_err(|e| {
			CacheError::conversion(format!("Failed to convert path to string: {:?}", e))
		})?;
		Ok(path)
	}

	pub async fn upload_file_if_changed(
		&self,
		client: &CacheClient,
		path: FfiPathWithRoot,
		progress_callback: Arc<dyn ProgressCallback>,
	) -> Result<bool> {
		debug!("Uploading file at path: {}", path.0);
		let path_values = path.as_path_values()?;
		let files_path = self.files();
		let (parent_uuid, mime) =
			match sync::update_items_in_path(self, &client.client, &path_values).await? {
				UpdateItemsInPath::Complete(DBObject::File(file)) => {
					if let Some(hash) = file.hash.map(Into::into) {
						let local_hash = io::hash_local_file(&path_values, &files_path).await?;
						if local_hash == hash {
							return Ok(false);
						}
					}
					(file.parent, Some(file.mime))
				}
				UpdateItemsInPath::Complete(_) => {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a file",
						path_values.full_path
					)));
				}
				UpdateItemsInPath::Partial(remaining, parent) if remaining == path_values.name => {
					(parent.uuid(), None)
				}
				UpdateItemsInPath::Partial(remaining, _) => {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a file (remaining: {})",
						path_values.full_path, remaining
					)));
				}
			};

		let remote_file = io::upload_file(
			&client.client,
			&path_values,
			parent_uuid,
			mime,
			Some(progress_callback),
			&files_path,
		)
		.await?;

		let mut conn = self.conn();
		DBFile::upsert_from_remote(&mut conn, remote_file)?;
		Ok(true)
	}

	pub async fn create_empty_file(
		&self,
		client: &CacheClient,
		parent_path: FfiPathWithRoot,
		name: String,
		mime: String,
	) -> Result<FfiPathWithRoot> {
		let file_path = parent_path.join(&name);
		debug!("Creating empty file at path: {}", file_path.0);
		let parent_pvs = parent_path.as_path_values()?;
		let files_path = self.files();
		let parent = match sync::update_items_in_path(self, &client.client, &parent_pvs).await? {
			UpdateItemsInPath::Complete(DBObject::Dir(dir)) => DBDirObject::Dir(dir),
			UpdateItemsInPath::Complete(DBObject::Root(root)) => DBDirObject::Root(root),
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::remote(format!(
					"Path {} points to a file",
					parent_path
				)));
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a directory (remaining: {})",
					parent_path, remaining
				)));
			}
		};
		let path = parent_path.join(&name);
		let pvs = path.as_path_values()?;
		let file = io::create_file(&client.client, &pvs, parent.uuid(), mime, &files_path).await?;
		let mut conn = self.conn();
		DBFile::upsert_from_remote(&mut conn, file)?;
		Ok(file_path)
	}

	pub async fn create_dir(
		&self,
		client: &CacheClient,
		parent_path: FfiPathWithRoot,
		name: String,
	) -> Result<FfiPathWithRoot> {
		let dir_path = parent_path.join(&name);
		debug!("Creating directory at path: {}", dir_path.0);
		let path_values = parent_path.as_path_values()?;
		let files_path = self.files();
		let parent = match sync::update_items_in_path(self, &client.client, &path_values).await? {
			UpdateItemsInPath::Complete(DBObject::Dir(dir)) => DBDirObject::Dir(dir),
			UpdateItemsInPath::Complete(DBObject::Root(root)) => DBDirObject::Root(root),
			UpdateItemsInPath::Complete(DBObject::File(_)) => {
				return Err(CacheError::remote(format!(
					"Path {} points to a file",
					parent_path
				)));
			}
			UpdateItemsInPath::Partial(remaining, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to a directory (remaining: {})",
					parent_path, remaining
				)));
			}
		};

		let dir = io::create_dir(
			&client.client,
			&path_values,
			parent.uuid(),
			name,
			&files_path,
		)
		.await?;

		let mut conn = self.conn();
		DBDir::upsert_from_remote(&mut conn, dir)?;
		Ok(dir_path)
	}

	pub async fn trash_item(&self, client: &CacheClient, path: FfiPathWithRoot) -> Result<()> {
		debug!("Trashing item at path: {}", path.0);
		let path_values: PathValues<'_> = path.as_path_values()?;
		let obj = match sync::update_items_in_path(self, &client.client, &path_values).await? {
			UpdateItemsInPath::Complete(dbobject) => dbobject,
			UpdateItemsInPath::Partial(_, _) => {
				return Err(CacheError::remote(format!(
					"Path {} does not point to an item",
					path_values.full_path
				)));
			}
		};

		match obj {
			DBObject::Root(root) => {
				return Err(CacheError::remote(format!(
					"Cannot remove root directory: {}",
					root.uuid
				)));
			}
			DBObject::Dir(dir) => {
				let remote_dir = dir.clone().into();
				client.client.trash_dir(&remote_dir).await?;
				dir.delete(&self.conn())?;
			}
			DBObject::File(file) => {
				let remote_file = file.clone().try_into()?;
				client.client.trash_file(&remote_file).await?;
				file.delete(&self.conn())?;
			}
		}
		Ok(())
	}

	pub async fn move_item(
		&self,
		client: &CacheClient,
		item: FfiPathWithRoot,
		parent: FfiPathWithRoot,
		new_parent: FfiPathWithRoot,
	) -> Result<FfiPathWithRoot> {
		debug!(
			"Moving item {} from parent {} to new parent {}",
			item.0, parent.0, new_parent.0
		);
		let item_pvs: PathValues<'_> = item.as_path_values()?;
		let parent_pvs: PathValues<'_> = parent.as_path_values()?;
		let new_parent_pvs: PathValues<'_> = new_parent.as_path_values()?;

		let (obj, new_parent_dir) = futures::try_join!(
			async {
				let obj = match sync::update_items_in_path(self, &client.client, &item_pvs).await? {
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
				let parent = match sql::select_object_at_path(&self.conn(), &parent_pvs)?
					.map(DBDirObject::try_from)
				{
					None | Some(Err(_)) => {
						return Err(CacheError::remote(format!(
							"Path {} does not point to a parent directory",
							parent_pvs.full_path
						)));
					}
					Some(Ok(obj)) => obj,
				};
				if Some(parent.uuid()) != obj.parent() {
					return Err(CacheError::remote(format!(
						"Path {} does not point to the parent of obj {} got {:?} (should be {})",
						parent_pvs.full_path,
						obj.uuid(),
						obj.parent(),
						parent.uuid()
					)));
				}
				Ok(obj)
			},
			async {
				match sync::update_items_in_path(self, &client.client, &new_parent_pvs).await? {
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

		match obj {
			DBNonRootObject::Dir(dbdir) => {
				let mut remote_dir = dbdir.into();
				client
					.client
					.move_dir(&mut remote_dir, &new_parent_dir.uuid())
					.await?;
				sql::move_item(
					&mut self.conn(),
					remote_dir.uuid(),
					remote_dir.name(),
					remote_dir.parent(),
				)?;
			}
			DBNonRootObject::File(dbfile) => {
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				client
					.client
					.move_file(&mut remote_file, &new_parent_dir.uuid())
					.await?;
				sql::move_item(
					&mut self.conn(),
					remote_file.uuid(),
					remote_file.name(),
					remote_file.parent(),
				)?;
			}
		}
		Ok(new_parent.join(item_pvs.name))
	}

	pub async fn rename_item(
		&self,
		client: &CacheClient,
		item: FfiPathWithRoot,
		new_name: String,
	) -> Result<Option<FfiPathWithRoot>> {
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
		self.update_dir_children(client, item.parent()).await?;
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
		match obj {
			DBNonRootObject::Dir(dbdir) => {
				let id = dbdir.id;
				let mut remote_dir: RemoteDirectory = dbdir.into();
				let mut meta = remote_dir.get_meta();
				meta.set_name(&new_name)?;
				client
					.client
					.update_dir_metadata(&mut remote_dir, meta)
					.await?;
				sql::rename_item(&mut self.conn(), id, &new_name, remote_dir.parent())?;
			}
			DBNonRootObject::File(dbfile) => {
				let id = dbfile.id;
				let mut remote_file: RemoteFile = dbfile.try_into()?;
				let mut meta = remote_file.get_meta();
				meta.set_name(&new_name)?;
				client
					.client
					.update_file_metadata(&mut remote_file, meta)
					.await?;
				sql::rename_item(&mut self.conn(), id, &new_name, remote_file.parent())?;
			}
		}
		Ok(Some(new_path))
	}
}

#[derive(uniffi::Object)]
pub struct CacheClient {
	client: filen_sdk_rs::auth::Client,
}

#[uniffi::export]
impl CacheClient {
	#[uniffi::constructor(name = "login")]
	pub async fn login(email: String, password: &str, two_factor_code: &str) -> Result<Self> {
		Ok(
			filen_sdk_rs::auth::Client::login(email, password, two_factor_code)
				.await
				.map(|client| Self { client })?,
		)
	}

	#[uniffi::constructor(name = "from_strings")]
	pub fn from_strings(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
	) -> Result<Self> {
		Ok(filen_sdk_rs::auth::Client::from_strings(
			email,
			root_uuid,
			auth_info,
			private_key,
			api_key,
			version,
		)
		.map(|client| Self { client })?)
	}

	pub fn root_uuid(&self) -> String {
		self.client.root().uuid().to_string()
	}
}

pub struct PathValues<'a> {
	pub root_uuid: uuid::Uuid,
	pub full_path: &'a str,
	pub inner_path: &'a str,
	pub name: &'a str,
}

impl FfiPathWithRoot {
	pub fn as_path_values(&self) -> Result<PathValues> {
		let mut iter = self.0.path_iter();
		let (root_uuid_str, remaining) = iter
			.next()
			.ok_or_else(|| CacheError::conversion("Path must start with a root UUID"))?;

		Ok(PathValues {
			root_uuid: Uuid::parse_str(root_uuid_str)
				.map_err(|e| CacheError::conversion(format!("Invalid root UUID: {}", e)))?,
			full_path: self.0.as_str(),
			inner_path: remaining,
			name: iter.last().unwrap_or_default().0,
		})
	}
}
