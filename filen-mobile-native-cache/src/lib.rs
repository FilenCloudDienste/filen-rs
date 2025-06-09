use std::{
	str::FromStr,
	sync::{Mutex, MutexGuard},
};

use ffi::{FfiDir, FfiFile, FfiNonRootObject, FfiObject, FfiRoot};
use filen_sdk_rs::{fs::HasUUID, util::PathIteratorExt};
use rusqlite::Connection;
use uuid::Uuid;

use crate::{
	ffi::PathWithRoot,
	sql::{
		DBDir, DBDirExt, DBDirObject, DBDirTrait, DBFile, DBObject, DBRoot,
		error::OptionalExtensionSQL,
	},
	sync::UpdateItemsInPath,
};

uniffi::setup_scaffolding!();

pub mod ffi;
pub mod io;
pub mod sql;
pub(crate) mod sync;
pub mod tokio;

#[derive(uniffi::Error, Debug)]
#[uniffi(flat_error)]
pub enum Error {
	Anyhow(anyhow::Error),
}
impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Error::Anyhow(err) => err.fmt(f),
		}
	}
}
impl<T> From<T> for Error
where
	anyhow::Error: From<T>,
{
	fn from(err: T) -> Self {
		Error::Anyhow(anyhow::Error::from(err))
	}
}

pub type Result<T> = std::result::Result<T, Error>;

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

fn get_root_and_path_from_path(path: &PathWithRoot) -> Result<(Uuid, &str)> {
	let (root_uuid_str, path) = path
		.0
		.path_iter()
		.next()
		.ok_or_else(|| Error::Anyhow(anyhow::anyhow!("Path must start with a root UUID")))?;
	let root_uuid = Uuid::parse_str(root_uuid_str)?;
	Ok((root_uuid, path))
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
		path: &PathWithRoot,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>> {
		let (root, dir_path) = get_root_and_path_from_path(path)?;

		let dir: DBDirObject = match sql::select_object_at_path(&self.conn(), root, dir_path)? {
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

	pub fn query_item(&self, path: &PathWithRoot) -> Result<Option<FfiObject>> {
		let (root, path) = get_root_and_path_from_path(path)?;
		let obj = sql::select_object_at_path(&self.conn(), root, path)?;
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
		path: PathWithRoot,
	) -> Result<()> {
		let (root_uuid, dir_path) = get_root_and_path_from_path(&path)?;
		if root_uuid != client.client.root().uuid() {
			return Err(
				anyhow::anyhow!("Root UUID in path does not match client's root UUID").into(),
			);
		}

		let mut dir: DBDirObject = match sync::update_items_in_path(self, &client.client, dir_path)
			.await?
		{
			UpdateItemsInPath::Complete(dbobject) => dbobject.try_into()?,
			UpdateItemsInPath::Partial(_, _) => {
				return Err(anyhow::anyhow!("Path {} does not point to a directory", path).into());
			}
		};
		let (dirs, files) = client.client.list_dir(&dir.uuid()).await?;
		let mut conn = self.conn();
		dir.update_children(&mut conn, dirs, files)?;
		dir.update_dir_last_listed_now(&conn)?;
		Ok(())
	}

	// pub async fn download_file(&self, client: &CacheClient, file_path: &str) -> Result<String> {
	// 	let file = match sync::update_items_in_path(
	// 		self,
	// 		&client.client,
	// 		client.client.root().uuid(),
	// 		file_path,
	// 	)
	// 	.await?
	// 	{
	// 		UpdateItemsInPath::Complete(DBObject::File(file)) => file,
	// 		UpdateItemsInPath::Partial(_, _) | UpdateItemsInPath::Complete(_) => {
	// 			return Err(Error::Anyhow(anyhow::anyhow!(
	// 				"Path {} does not point to a file",
	// 				file_path
	// 			)));
	// 		}
	// 	};
	// 	let file = file.try_into()?;

	// 	let path = io::download_file(&client.client, &file, file_path)
	// 		.await?
	// 		.into_os_string()
	// 		.into_string()
	// 		.map_err(|e| {
	// 			Error::Anyhow(anyhow::anyhow!("Failed to convert path to string: {:?}", e))
	// 		})?;
	// 	Ok(path)
	// }

	// pub async fn maybe_upload_file(&self, client: &CacheClient, file_path: &str) -> Result<bool> {
	// 	let found_object_or_parent = sync::update_or_create_items_in_path(
	// 		self,
	// 		&client.client,
	// 		client.client.root().uuid(),
	// 		file_path,
	// 	)
	// 	.await?;
	// 	let mut file = match found_object_or_parent {
	// 		ObjectOrParent::Object(dbobject) => match dbobject {
	// 			DBObject::File(file) => file,
	// 			_ => {
	// 				return Err(std::io::Error::new(
	// 					std::io::ErrorKind::IsADirectory,
	// 					format!("No file found in path: {}", file_path),
	// 				)
	// 				.into());
	// 			}
	// 		},
	// 		ObjectOrParent::Parent(_) => {
	// 			return Err(std::io::Error::new(
	// 				std::io::ErrorKind::NotFound,
	// 				format!("No file found in path: {}", file_path),
	// 			)
	// 			.into());
	// 		}
	// 	};

	// 	if let Some(hash) = file.hash.map(Into::into) {
	// 		let local_hash = io::hash_local_file(file_path)?;
	// 		if local_hash == hash {
	// 			return Ok(false);
	// 		}
	// 	}

	// 	let remote_file = io::upload_file(&client.client, file_path, file.item.parent).await?;

	// 	let mut conn = self.conn();
	// 	file.update_from_remote(&mut conn, remote_file)?;
	// 	Ok(true)
	// }

	// pub async fn create_empty_file(
	// 	&self,
	// 	client: &CacheClient,
	// 	parent_uuid: &str,
	// 	name: String,
	// 	mime: String,
	// ) -> Result<String> {
	// 	let parent_uuid = Uuid::parse_str(parent_uuid)?;
	// 	let file = client
	// 		.client
	// 		.make_file_builder(name, &parent_uuid)
	// 		.mime(mime)
	// 		.build();
	// 	let mut writer = client.client.get_file_writer(file);
	// 	writer.close().await?;
	// 	let file = writer.into_remote_file().ok_or_else(|| {
	// 		Error::Anyhow(anyhow::anyhow!("Failed to convert writer into remote file"))
	// 	})?;
	// 	let mut conn = self.conn();
	// 	let uuid = file.uuid().to_string();
	// 	DBFile::upsert_from_remote(&mut conn, file)?;
	// 	Ok(uuid)
	// }

	// pub async fn create_dir(
	// 	&self,
	// 	client: &CacheClient,
	// 	parent_uuid: &str,
	// 	name: String,
	// ) -> Result<String> {
	// 	let parent_uuid = Uuid::parse_str(parent_uuid)?;
	// 	let dir = client.client.create_dir(&parent_uuid, name).await?;
	// 	let mut conn = self.conn();
	// 	let uuid = dir.uuid().to_string();
	// 	DBDir::upsert_from_remote(&mut conn, dir)?;
	// 	Ok(uuid)
	// }
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
