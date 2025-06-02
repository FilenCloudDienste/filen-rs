use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use ffi::{FfiDir, FfiNonRootObject, FfiObject, FfiRoot};
use filen_sdk_rs::fs::HasUUID;
use futures::try_join;
use rusqlite::Connection;
use sql::update_root;
use uuid::Uuid;

uniffi::setup_scaffolding!();

pub mod ffi;
pub mod sql;
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

#[uniffi::export]
impl FilenMobileDB {
	pub fn query_roots_info(&self, root_uuid_str: String) -> Result<Option<FfiRoot>> {
		let conn = self.conn();
		Ok(sql::select_root_item(&conn, root_uuid_str)?)
	}

	#[uniffi::constructor]
	pub fn initialize_in_memory() -> Result<FilenMobileDB> {
		let db = Connection::open_in_memory()?;
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
		dir_uuid: &str,
		order_by: Option<String>,
	) -> Result<Option<QueryChildrenResponse>> {
		let dir_uuid = Uuid::parse_str(dir_uuid)?;
		let conn = self.conn();
		let maybe_info = sql::select_dir_children(&conn, dir_uuid, order_by.as_deref())?;
		Ok(maybe_info.map(|(parent, objects)| QueryChildrenResponse { parent, objects }))
	}

	pub fn query_item(&self, uuid: &str) -> Result<Option<FfiObject>> {
		let uuid = Uuid::parse_str(uuid)?;
		let conn = self.conn();
		Ok(sql::select_item(&conn, uuid)?)
	}
}

#[filen_sdk_rs_macros::create_uniffi_wrapper]
impl FilenMobileDB {
	pub async fn update_roots_info(&self, client: &CacheClient, root_uuid: &str) -> Result<()> {
		let root_uuid = Uuid::parse_str(root_uuid)?;
		let resp = client.client.get_user_info().await?;

		let conn = self.conn();

		update_root(&conn, root_uuid, &resp)?;
		Ok(())
	}

	pub async fn update_dir_children(&self, client: &CacheClient, dir_uuid: &str) -> Result<()> {
		let dir_uuid = Uuid::parse_str(dir_uuid)?;
		let (parent, (dirs, files)) = try_join!(
			client.client.get_dir(dir_uuid),
			client.client.list_dir(&dir_uuid)
		)?;

		let mut conn = self.conn();
		sql::upsert_dir_last_listed(&mut conn, &parent).context("upsert_dir_last_listed")?;
		sql::update_children(&mut conn, parent.uuid(), &dirs, &files).context("upsert_items")?;
		Ok(())
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
