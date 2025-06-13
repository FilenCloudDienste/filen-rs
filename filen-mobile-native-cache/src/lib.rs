use std::{
	str::FromStr,
	sync::{Mutex, MutexGuard},
};

use ffi::{FfiDir, FfiFile, FfiNonRootObject, FfiObject, FfiRoot};
use filen_sdk_rs::{
	fs::{HasName, HasParent, HasUUID},
	util::PathIteratorExt,
};
use futures::AsyncWriteExt;
use rusqlite::Connection;
use uuid::Uuid;

use crate::{
	ffi::FfiPathWithRoot,
	sql::{
		DBDir, DBDirExt, DBDirObject, DBDirTrait, DBFile, DBItemExt, DBItemTrait, DBNonRootObject,
		DBObject, DBRoot, error::OptionalExtensionSQL,
	},
	sync::UpdateItemsInPath,
};

uniffi::setup_scaffolding!();

mod error;
pub mod ffi;
pub mod io;
pub mod sql;
pub(crate) mod sync;
pub mod tokio;
pub use error::CacheError;

pub type Result<T> = std::result::Result<T, CacheError>;

#[derive(uniffi::Object)]
pub struct FilenMobileDB {
	conn: Mutex<Connection>,
}

impl FilenMobileDB {
	pub fn conn(&self) -> MutexGuard<Connection> {
		self.conn.lock().unwrap()
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
		})
	}

	#[uniffi::constructor]
	pub fn initialize_from_path(path: &str) -> Result<FilenMobileDB> {
		let db = Connection::open(path)?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		Ok(FilenMobileDB {
			conn: Mutex::new(db),
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
}

#[derive(uniffi::Record)]
pub struct DownloadResponse {
	pub path: String,
	pub file: FfiFile,
}

#[filen_sdk_rs_macros::create_uniffi_wrapper]
impl FilenMobileDB {
	pub async fn update_roots_info(&self, client: &CacheClient) -> Result<()> {
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

	pub async fn download_file(
		&self,
		client: &CacheClient,
		file_path: FfiPathWithRoot,
	) -> Result<String> {
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
		let file = file.try_into()?;

		let path = io::download_file(&client.client, &file, &file_path)
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
	) -> Result<bool> {
		let path_values = path.as_path_values()?;
		let parent_uuid =
			match sync::update_items_in_path(self, &client.client, &path_values).await? {
				UpdateItemsInPath::Complete(DBObject::File(file)) => {
					if let Some(hash) = file.hash.map(Into::into) {
						let local_hash = io::hash_local_file(&path)?;
						if local_hash == hash {
							return Ok(false);
						}
					}
					file.parent
				}
				UpdateItemsInPath::Complete(_) => {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a file",
						path_values.full_path
					)));
				}
				UpdateItemsInPath::Partial(remaining, parent) if remaining == path_values.name => {
					parent.uuid()
				}
				UpdateItemsInPath::Partial(remaining, _) => {
					return Err(CacheError::remote(format!(
						"Path {} does not point to a file (remaining: {})",
						path_values.full_path, remaining
					)));
				}
			};

		let remote_file = io::upload_file(&client.client, &path, parent_uuid).await?;

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
		let path_values = parent_path.as_path_values()?;
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
		let file = client
			.client
			.make_file_builder(name, &parent.uuid())
			.mime(mime)
			.build();
		let mut writer = client.client.get_file_writer(file)?;
		writer.close().await?;
		let file = writer
			.into_remote_file()
			.ok_or_else(|| CacheError::conversion("Failed to convert writer into remote file"))?;
		let mut conn = self.conn();
		let file = DBFile::upsert_from_remote(&mut conn, file)?;
		Ok(parent_path.join(&file.name))
	}

	pub async fn create_dir(
		&self,
		client: &CacheClient,
		parent_path: FfiPathWithRoot,
		name: String,
	) -> Result<FfiPathWithRoot> {
		let path_values = parent_path.as_path_values()?;
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

		let dir = client.client.create_dir(&parent.uuid(), name).await?;
		let mut conn = self.conn();
		let dir = DBDir::upsert_from_remote(&mut conn, dir)?;
		Ok(parent_path.join(&dir.name))
	}

	pub async fn trash_item(&self, client: &CacheClient, path: FfiPathWithRoot) -> Result<()> {
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
				let mut remote_file: filen_sdk_rs::fs::file::RemoteFile = dbfile.try_into()?;
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
