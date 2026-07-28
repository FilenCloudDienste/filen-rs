use std::{borrow::Cow, fmt::Debug};

use chrono::{DateTime, Utc};
use filen_sdk_rs::fs::{
	HasName, HasParent, HasRemoteInfo, HasUUID,
	categories::{DirType, Normal},
	dir::{
		DecryptedDirectoryMeta, RemoteDirectory, RootDirectory,
		meta::DirectoryMeta,
		traits::{HasDirMeta, HasRemoteDirInfo},
	},
	file::RemoteFile,
};
use filen_types::{
	api::v3::dir::color::DirColor,
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ParentUuid, Uuid},
	traits::CowHelpers,
};
use rusqlite::{CachedStatement, Connection, Result};
use tracing::trace;

use crate::{
	ffi::ItemType,
	sql::{
		MetaState, SQLError,
		columns::{
			DIR_CREATED, DIR_FAVORITE_RANK, DIR_METADATA_STATE, DIR_NAME, DIR_RAW_METADATA,
			DIR_TIMESTAMP, DIRS_COLOR, DIRS_LAST_LISTED, ROOT_LAST_LISTED, ROOTS_LAST_UPDATED,
			ROOTS_MAX_STORAGE, ROOTS_STORAGE_USED,
		},
		item::{self, DBItemTrait, InnerDBItem},
		object::{DBNonRootObject, DBObject, JsonObject},
		raw_meta_and_state_from_dir_meta,
		statements::*,
	},
};

pub(crate) type SQLResult<T> = std::result::Result<T, SQLError>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct DBDecryptedDirMeta {
	pub(crate) name: String,
	pub(crate) created: Option<i64>,
}

impl DBDecryptedDirMeta {
	fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			name: row.get(DIR_NAME)?,
			created: row.get(DIR_CREATED)?,
		})
	}
}

impl From<DecryptedDirectoryMeta<'_>> for DBDecryptedDirMeta {
	fn from(meta: DecryptedDirectoryMeta<'_>) -> Self {
		Self {
			name: meta.name.into_owned(),
			created: meta.created.map(|dt| dt.timestamp_millis()),
		}
	}
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum DBDirMeta {
	Decoded(DBDecryptedDirMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(EncryptedString<'static>),
	RSAEncrypted(RSAEncryptedString<'static>),
}

impl DBDirMeta {
	fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let metadata_state: MetaState = row.get(DIR_METADATA_STATE)?;

		match metadata_state {
			MetaState::Decrypted => match String::from_utf8(row.get(DIR_RAW_METADATA)?) {
				Ok(utf8) => Ok(Self::DecryptedUTF8(utf8)),
				Err(e) => Ok(Self::DecryptedRaw(e.into_bytes())),
			},
			MetaState::Encrypted => {
				Ok(Self::Encrypted(EncryptedString(row.get(DIR_RAW_METADATA)?)))
			}
			MetaState::RSAEncrypted => Ok(Self::RSAEncrypted(RSAEncryptedString(
				row.get(DIR_RAW_METADATA)?,
			))),
			MetaState::Decoded => Ok(Self::Decoded(DBDecryptedDirMeta::from_row(row)?)),
		}
	}
}

impl From<DirectoryMeta<'_>> for DBDirMeta {
	fn from(meta: DirectoryMeta<'_>) -> Self {
		match meta {
			DirectoryMeta::Decoded(decoded) => Self::Decoded(DBDecryptedDirMeta::from(decoded)),
			DirectoryMeta::DecryptedRaw(raw) => Self::DecryptedRaw(raw.into_owned()),
			DirectoryMeta::DecryptedUTF8(utf8) => Self::DecryptedUTF8(utf8.into_owned()),
			DirectoryMeta::Encrypted(encrypted) => Self::Encrypted(encrypted.into_owned_cow()),
			DirectoryMeta::RSAEncrypted(rsa_encrypted) => {
				Self::RSAEncrypted(rsa_encrypted.into_owned_cow())
			}
		}
	}
}

impl From<DBDirMeta> for DirectoryMeta<'static> {
	fn from(meta: DBDirMeta) -> Self {
		match meta {
			DBDirMeta::Decoded(decoded) => DirectoryMeta::Decoded(DecryptedDirectoryMeta {
				name: Cow::Owned(decoded.name),
				created: decoded
					.created
					.map(|ts| DateTime::<Utc>::from_timestamp_millis(ts).unwrap_or_default()),
			}),
			DBDirMeta::DecryptedRaw(raw) => DirectoryMeta::DecryptedRaw(Cow::Owned(raw)),
			DBDirMeta::DecryptedUTF8(utf8) => DirectoryMeta::DecryptedUTF8(Cow::Owned(utf8)),
			DBDirMeta::Encrypted(encrypted) => DirectoryMeta::Encrypted(encrypted),
			DBDirMeta::RSAEncrypted(rsa_encrypted) => DirectoryMeta::RSAEncrypted(rsa_encrypted),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DBDir {
	pub(crate) id: i64,
	pub(crate) uuid: Uuid,
	pub(crate) parent: ParentUuid,
	pub(crate) favorite_rank: i64,
	pub(crate) color: DirColor<'static>,
	pub(crate) last_listed: i64,
	pub(crate) local_data: Option<JsonObject>,
	pub(crate) meta: DBDirMeta,
	pub(crate) timestamp: i64,
}

impl DBDir {
	pub(crate) fn from_inner_and_row(item: InnerDBItem, row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: item.id,
			uuid: item.uuid,
			parent: item.parent.ok_or_else(|| {
				rusqlite::Error::FromSqlConversionFailure(
					0,
					rusqlite::types::Type::Blob,
					"Parent UUID cannot be None for DBDir".into(),
				)
			})?,
			local_data: item.local_data,
			favorite_rank: row.get(DIR_FAVORITE_RANK)?,
			color: row.get(DIRS_COLOR)?,
			timestamp: row.get(DIR_TIMESTAMP)?,
			last_listed: row.get(DIRS_LAST_LISTED)?,
			meta: DBDirMeta::from_row(row)?,
		})
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_DIR)?;
		let res = stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row))?;
		Ok(res)
	}

	pub(crate) fn upsert_from_remote_stmts(
		remote_dir: RemoteDirectory,
		upsert_item_stmt: &mut CachedStatement<'_>,
		upsert_dir: &mut CachedStatement<'_>,
		upsert_dir_meta: &mut CachedStatement<'_>,
		delete_dir_meta: &mut CachedStatement<'_>,
	) -> Result<Self> {
		let (id, local_data) = item::upsert_dir_item_with_stmts(
			remote_dir.uuid(),
			Some(*remote_dir.parent()),
			remote_dir.name(),
			None,
			upsert_item_stmt,
		)?;
		trace!("Upserting remote dir: {remote_dir:?}");

		let meta = remote_dir.get_meta();

		let (meta_state, meta) = raw_meta_and_state_from_dir_meta(meta);

		let timestamp = remote_dir.timestamp.timestamp_millis();

		let (last_listed, favorite_rank) = upsert_dir.query_one(
			(
				id,
				remote_dir.favorited() as u8,
				remote_dir.color(),
				timestamp,
				meta_state,
				meta,
			),
			|r| {
				let last_listed: i64 = r.get(DIRS_LAST_LISTED)?;
				let favorite_rank: i64 = r.get(DIR_FAVORITE_RANK)?;
				Ok((last_listed, favorite_rank))
			},
		)?;

		if let DirectoryMeta::Decoded(meta) = remote_dir.get_meta() {
			upsert_dir_meta.execute((
				id,
				&meta.name,
				meta.created.map(|dt| dt.timestamp_millis()),
			))?;
		} else {
			delete_dir_meta.execute([id])?;
		}

		trace!("Upserted remote dir with id: {id}");

		Ok(Self {
			id,
			uuid: remote_dir.uuid,
			parent: remote_dir.parent,
			favorite_rank,
			color: remote_dir.color,
			timestamp,
			last_listed,
			local_data,
			meta: DBDirMeta::from(remote_dir.meta),
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_dir: RemoteDirectory,
	) -> Result<Self> {
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
			let mut upsert_dir = tx.prepare_cached(UPSERT_DIR)?;
			let mut upsert_dir_meta = tx.prepare_cached(UPSERT_DIR_META)?;
			let mut delete_dir_meta = tx.prepare_cached(DELETE_DIR_META)?;
			Self::upsert_from_remote_stmts(
				remote_dir,
				&mut upsert_item_stmt,
				&mut upsert_dir,
				&mut upsert_dir_meta,
				&mut delete_dir_meta,
			)?
		};
		tx.commit()?;
		Ok(new)
	}

	pub(crate) fn update_favorite_rank(
		&mut self,
		conn: &Connection,
		favorite_rank: i64,
	) -> Result<()> {
		trace!(
			"Updating favorite rank for dir {} to {}",
			self.uuid, favorite_rank
		);
		let mut stmt = conn.prepare_cached(UPDATE_DIR_FAVORITE_RANK)?;
		stmt.execute((favorite_rank, self.id))?;
		self.favorite_rank = favorite_rank;
		Ok(())
	}

	fn created(&self) -> Option<i64> {
		if let DBDirMeta::Decoded(decoded) = &self.meta {
			decoded.created
		} else {
			None
		}
	}
}

impl DBDirTrait for DBDir {
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> Uuid {
		self.uuid
	}

	#[cfg(target_os = "ios")]
	fn name(&self) -> Option<&str> {
		if let DBDirMeta::Decoded(decoded) = &self.meta {
			Some(&decoded.name)
		} else {
			None
		}
	}

	fn set_last_listed(&mut self, value: i64) {
		self.last_listed = value;
	}
}

impl item::DBItemTrait for DBDir {
	fn uuid(&self) -> Uuid {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		Some(self.parent)
	}

	fn name(&self) -> Option<&str> {
		if let DBDirMeta::Decoded(decoded) = &self.meta {
			Some(&decoded.name)
		} else {
			None
		}
	}
}

impl From<DBDir> for RemoteDirectory {
	fn from(value: DBDir) -> Self {
		RemoteDirectory {
			uuid: value.uuid,
			parent: value.parent,
			color: value.color,
			timestamp: DateTime::<Utc>::from_timestamp_millis(value.timestamp).unwrap_or_default(),
			favorited: value.favorite_rank > 0,
			meta: DirectoryMeta::from(value.meta),
		}
	}
}

impl PartialEq<RemoteDirectory> for DBDir {
	fn eq(&self, other: &RemoteDirectory) -> bool {
		self.uuid == other.uuid()
			&& self.parent == *other.parent()
			&& DBItemTrait::name(self) == other.name()
			&& self.color == other.color()
			&& self.created() == other.created().map(|t| t.timestamp_millis())
			&& (self.favorite_rank > 0) == other.favorited()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DBRoot {
	pub(crate) id: i64,
	pub(crate) uuid: Uuid,
	pub(crate) storage_used: i64,
	pub(crate) max_storage: i64,
	pub(crate) last_updated: i64,
	pub(crate) last_listed: i64,
}

impl DBRoot {
	pub(crate) fn from_inner_and_row(inner: InnerDBItem, row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: inner.id,
			uuid: inner.uuid,
			storage_used: row.get(ROOTS_STORAGE_USED)?,
			max_storage: row.get(ROOTS_MAX_STORAGE)?,
			last_updated: row.get(ROOTS_LAST_UPDATED)?,
			last_listed: row.get(ROOT_LAST_LISTED)?,
		})
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_ROOT)?;
		stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row))
	}

	pub(crate) fn select(conn: &Connection, uuid: Uuid) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::Root(root) => Ok(root),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::Root)),
		}
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_root: &RootDirectory,
	) -> Result<Self> {
		trace!("Upserting remote root: {remote_root:?}");
		let tx = conn.transaction()?;
		let id = item::upsert_root_item(&tx, remote_root.uuid())?;
		let mut stmt = tx.prepare_cached(UPSERT_ROOT_EMPTY)?;
		let (storage_used, max_storage, last_updated) = stmt.query_one([id], |f| {
			let storage_used: i64 = f.get(ROOTS_STORAGE_USED)?;
			let max_storage: i64 = f.get(ROOTS_MAX_STORAGE)?;
			let last_updated: i64 = f.get(ROOTS_LAST_UPDATED)?;
			Ok((storage_used, max_storage, last_updated))
		})?;
		std::mem::drop(stmt);
		let mut stmt = tx.prepare_cached(UPSERT_DIR)?;
		let last_listed = stmt.query_one(
			(id, 0, Option::<String>::None, 0, MetaState::Decrypted, ""),
			|r| {
				let last_listed: i64 = r.get(DIRS_LAST_LISTED)?;
				Ok(last_listed)
			},
		)?;
		std::mem::drop(stmt);
		tx.commit()?;
		trace!("Upserted remote root with id: {id}");
		Ok(Self {
			id,
			uuid: remote_root.uuid(),
			storage_used,
			max_storage,
			last_updated,
			last_listed,
		})
	}
}

impl DBDirTrait for DBRoot {
	fn set_last_listed(&mut self, value: i64) {
		self.last_listed = value;
	}

	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> Uuid {
		self.uuid
	}

	#[cfg(target_os = "ios")]
	fn name(&self) -> Option<&str> {
		Some("")
	}
}

impl DBItemTrait for DBRoot {
	fn uuid(&self) -> Uuid {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		None
	}

	fn name(&self) -> Option<&str> {
		Some("")
	}
}

impl From<DBRoot> for RootDirectory {
	fn from(value: DBRoot) -> Self {
		RootDirectory::new(value.uuid)
	}
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DBDirObject {
	Dir(DBDir),
	Root(DBRoot),
}

impl From<DBDirObject> for DBObject {
	fn from(obj: DBDirObject) -> Self {
		match obj {
			DBDirObject::Dir(dir) => DBObject::Dir(dir),
			DBDirObject::Root(root) => DBObject::Root(root),
		}
	}
}

impl TryFrom<DBObject> for DBDirObject {
	type Error = SQLError;

	fn try_from(obj: DBObject) -> Result<Self, Self::Error> {
		match obj {
			DBObject::Dir(dir) => Ok(DBDirObject::Dir(dir)),
			DBObject::Root(root) => Ok(DBDirObject::Root(root)),
			DBObject::File(_) => Err(SQLError::UnexpectedType(ItemType::File, ItemType::Dir)),
		}
	}
}

impl From<DBDirObject> for DirType<'static, Normal> {
	fn from(obj: DBDirObject) -> Self {
		match obj {
			DBDirObject::Dir(dir) => DirType::Dir(Cow::Owned(dir.into())),
			DBDirObject::Root(root) => DirType::Root(Cow::Owned(root.into())),
		}
	}
}

impl From<&DBDirObject> for DirType<'static, Normal> {
	fn from(obj: &DBDirObject) -> Self {
		match obj {
			DBDirObject::Dir(dir) => DirType::Dir(Cow::Owned(dir.clone().into())),
			DBDirObject::Root(root) => DirType::Root(Cow::Owned(root.clone().into())),
		}
	}
}

impl From<DBDir> for DBDirObject {
	fn from(dir: DBDir) -> Self {
		DBDirObject::Dir(dir)
	}
}

impl From<DBRoot> for DBDirObject {
	fn from(root: DBRoot) -> Self {
		DBDirObject::Root(root)
	}
}

impl DBDirTrait for DBDirObject {
	fn set_last_listed(&mut self, value: i64) {
		match self {
			DBDirObject::Dir(dir) => dir.set_last_listed(value),
			DBDirObject::Root(root) => root.set_last_listed(value),
		}
	}

	fn id(&self) -> i64 {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::id(dir),
			DBDirObject::Root(root) => DBDirTrait::id(root),
		}
	}

	fn uuid(&self) -> Uuid {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::uuid(dir),
			DBDirObject::Root(root) => DBDirTrait::uuid(root),
		}
	}

	#[cfg(target_os = "ios")]
	fn name(&self) -> Option<&str> {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::name(dir),
			DBDirObject::Root(root) => DBDirTrait::name(root),
		}
	}
}

pub(crate) trait DBDirTrait: Sync + Send {
	fn id(&self) -> i64;
	fn uuid(&self) -> Uuid;
	// Only the iOS local path needs a directory's name (to mirror the folder on disk); off-iOS
	// nothing calls it, so it would warn as dead.
	#[cfg(target_os = "ios")]
	fn name(&self) -> Option<&str>;
	fn set_last_listed(&mut self, value: i64);
}

pub(crate) trait DBDirExt {
	fn update_dir_last_listed_now(&mut self, conn: &Connection) -> Result<()>;
	fn update_children<I, I1>(&mut self, conn: &mut Connection, dirs: I, files: I1) -> Result<()>
	where
		I: IntoIterator<Item = RemoteDirectory>,
		I1: IntoIterator<Item = RemoteFile>;
	fn select_children(
		&self,
		conn: &Connection,
		order_by: Option<&str>,
	) -> SQLResult<Vec<DBNonRootObject>>;
}

impl<T> DBDirExt for T
where
	T: DBDirTrait + Sync + Send,
{
	fn update_dir_last_listed_now(&mut self, conn: &Connection) -> Result<()> {
		let mut stmt: rusqlite::CachedStatement<'_> =
			conn.prepare_cached(UPDATE_DIR_LAST_LISTED)?;
		let now = Utc::now().timestamp_millis();
		stmt.execute((now, self.id()))?;
		self.set_last_listed(now);
		Ok(())
	}

	fn update_children<I, I1>(&mut self, conn: &mut Connection, dirs: I, files: I1) -> Result<()>
	where
		I: IntoIterator<Item = RemoteDirectory>,
		I1: IntoIterator<Item = RemoteFile>,
	{
		crate::sql::update_items_with_parent(conn, dirs, files, self.uuid())
	}

	fn select_children(
		&self,
		conn: &Connection,
		order_by: Option<&str>,
	) -> SQLResult<Vec<DBNonRootObject>> {
		crate::sql::select_children(conn, order_by, self.uuid())
	}
}
