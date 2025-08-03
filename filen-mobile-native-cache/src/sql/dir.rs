use std::{borrow::Cow, fmt::Debug};

use chrono::{DateTime, Utc};
use filen_sdk_rs::fs::{
	HasName, HasParent, HasRemoteInfo, HasUUID,
	dir::{
		DecryptedDirectoryMeta, RemoteDirectory, RootDirectory,
		meta::DirectoryMeta,
		traits::{HasDirMeta, HasRemoteDirInfo},
	},
	file::RemoteFile,
};
use filen_types::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::{ParentUuid, UuidStr},
};
use log::trace;
use rusqlite::{CachedStatement, Connection, Result};

use crate::{
	ffi::ItemType,
	sql::{
		MetaState, SQLError,
		item::{self, DBItemTrait, InnerDBItem},
		object::{DBNonRootObject, DBObject, JsonObject},
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
	fn from_row(row: &rusqlite::Row, idx: usize) -> Result<Self> {
		Ok(Self {
			name: row.get(idx)?,
			created: row.get(idx + 1)?,
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
	Encrypted(EncryptedString),
	RSAEncrypted(RSAEncryptedString),
}

impl DBDirMeta {
	fn from_row(row: &rusqlite::Row, idx: usize) -> Result<Self> {
		let metadata_state: MetaState = row.get(idx)?;

		match metadata_state {
			MetaState::Decrypted => match String::from_utf8(row.get(idx + 1)?) {
				Ok(utf8) => Ok(Self::DecryptedUTF8(utf8)),
				Err(e) => Ok(Self::DecryptedRaw(e.into_bytes())),
			},
			MetaState::Encrypted => Ok(Self::Encrypted(EncryptedString(row.get(idx + 1)?))),
			MetaState::RSAEncrypted => {
				Ok(Self::RSAEncrypted(RSAEncryptedString(row.get(idx + 1)?)))
			}
			MetaState::Decoded => Ok(Self::Decoded(DBDecryptedDirMeta::from_row(row, idx + 2)?)),
		}
	}
}

impl From<DirectoryMeta<'_>> for DBDirMeta {
	fn from(meta: DirectoryMeta<'_>) -> Self {
		match meta {
			DirectoryMeta::Decoded(decoded) => Self::Decoded(DBDecryptedDirMeta::from(decoded)),
			DirectoryMeta::DecryptedRaw(raw) => Self::DecryptedRaw(raw),
			DirectoryMeta::DecryptedUTF8(utf8) => Self::DecryptedUTF8(utf8),
			DirectoryMeta::Encrypted(encrypted) => Self::Encrypted(encrypted),
			DirectoryMeta::RSAEncrypted(rsa_encrypted) => Self::RSAEncrypted(rsa_encrypted),
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
			DBDirMeta::DecryptedRaw(raw) => DirectoryMeta::DecryptedRaw(raw),
			DBDirMeta::DecryptedUTF8(utf8) => DirectoryMeta::DecryptedUTF8(utf8),
			DBDirMeta::Encrypted(encrypted) => DirectoryMeta::Encrypted(encrypted),
			DBDirMeta::RSAEncrypted(rsa_encrypted) => DirectoryMeta::RSAEncrypted(rsa_encrypted),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DBDir {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: ParentUuid,
	pub(crate) favorite_rank: i64,
	pub(crate) color: Option<String>,
	pub(crate) last_listed: i64,
	pub(crate) local_data: Option<JsonObject>,
	pub(crate) meta: DBDirMeta,
}

impl DBDir {
	pub(crate) fn from_inner_and_row(
		item: InnerDBItem,
		row: &rusqlite::Row,
		idx: usize,
	) -> Result<Self> {
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
			favorite_rank: row.get(idx)?,
			color: row.get(idx + 1)?,
			last_listed: row.get(idx + 2)?,
			meta: DBDirMeta::from_row(row, idx + 3)?,
		})
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_DIR)?;
		let res = stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row, 0))?;
		Ok(res)
	}

	#[allow(dead_code)]
	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::Dir(dir) => Ok(dir),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::Dir)),
		}
	}

	pub(crate) fn upsert_from_remote_stmts(
		remote_dir: RemoteDirectory,
		upsert_item_stmt: &mut CachedStatement<'_>,
		upsert_dir: &mut CachedStatement<'_>,
		upsert_dir_meta: &mut CachedStatement<'_>,
		delete_dir_meta: &mut CachedStatement<'_>,
	) -> Result<Self> {
		let (id, local_data) = item::upsert_item_with_stmts(
			remote_dir.uuid(),
			Some(remote_dir.parent()),
			remote_dir.name(),
			None,
			ItemType::Dir,
			upsert_item_stmt,
		)?;
		trace!("Upserting remote dir: {remote_dir:?}");

		let meta = remote_dir.get_meta();
		let (meta_state, meta) = match meta {
			DirectoryMeta::Decoded(_) => (MetaState::Decoded, None),
			DirectoryMeta::DecryptedRaw(cow) => (MetaState::Decrypted, Some(cow.as_ref())),
			DirectoryMeta::DecryptedUTF8(cow) => (MetaState::Decrypted, Some(cow.as_bytes())),
			DirectoryMeta::Encrypted(cow) => (MetaState::Encrypted, Some(cow.0.as_bytes())),
			DirectoryMeta::RSAEncrypted(cow) => (MetaState::RSAEncrypted, Some(cow.0.as_bytes())),
		};

		let (last_listed, favorite_rank) = upsert_dir.query_one(
			(
				id,
				remote_dir.favorited() as u8,
				remote_dir.color(),
				meta_state,
				meta,
			),
			|r| {
				let last_listed: i64 = r.get(0)?;
				let favorite_rank: i64 = r.get(1)?;
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

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

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
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
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

	fn item_type(&self) -> ItemType {
		ItemType::Dir
	}
}

impl From<DBDir> for RemoteDirectory {
	fn from(value: DBDir) -> Self {
		RemoteDirectory {
			uuid: value.uuid,
			parent: value.parent,
			color: value.color,
			favorited: value.favorite_rank > 0,
			meta: DirectoryMeta::from(value.meta),
		}
	}
}

impl PartialEq<RemoteDirectory> for DBDir {
	fn eq(&self, other: &RemoteDirectory) -> bool {
		self.uuid == other.uuid()
			&& self.parent == other.parent()
			&& DBItemTrait::name(self) == other.name()
			&& self.color.as_deref() == other.color()
			&& self.created() == other.created().map(|t| t.timestamp_millis())
			&& (self.favorite_rank > 0) == other.favorited()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DBRoot {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) storage_used: i64,
	pub(crate) max_storage: i64,
	pub(crate) last_updated: i64,
	pub(crate) last_listed: i64,
}

impl DBRoot {
	pub(crate) fn from_inner_and_row(
		inner: InnerDBItem,
		row: &rusqlite::Row,
		idx: usize,
	) -> Result<Self> {
		Ok(Self {
			id: inner.id,
			uuid: inner.uuid,
			storage_used: row.get(idx)?,
			max_storage: row.get(idx + 1)?,
			last_updated: row.get(idx + 2)?,
			last_listed: row.get(idx + 3)?,
		})
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_ROOT)?;
		stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row, 0))
	}

	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
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
		let (id, _) = item::upsert_item(
			&tx,
			remote_root.uuid(),
			None, // root has no parent
			None, // root has no name
			None,
			ItemType::Root,
		)
		.unwrap();
		let mut stmt = tx.prepare_cached(UPSERT_ROOT_EMPTY).unwrap();
		let (storage_used, max_storage, last_updated) = stmt
			.query_one([id], |f| {
				let storage_used: i64 = f.get(0)?;
				let max_storage: i64 = f.get(1)?;
				let last_updated: i64 = f.get(2)?;
				Ok((storage_used, max_storage, last_updated))
			})
			.unwrap();
		std::mem::drop(stmt);
		let mut stmt = tx.prepare_cached(UPSERT_DIR).unwrap();
		let last_listed = stmt
			.query_one(
				(id, 0, Option::<String>::None, MetaState::Decrypted, ""),
				|r| {
					let last_listed: i64 = r.get(0)?;
					Ok(last_listed)
				},
			)
			.unwrap();
		std::mem::drop(stmt);
		tx.commit().unwrap();
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

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

	fn name(&self) -> Option<&str> {
		Some("")
	}
}

impl DBItemTrait for DBRoot {
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		None
	}

	fn name(&self) -> Option<&str> {
		Some("")
	}

	fn item_type(&self) -> ItemType {
		ItemType::Root
	}
}

impl From<DBRoot> for RootDirectory {
	fn from(value: DBRoot) -> Self {
		RootDirectory::new(value.uuid)
	}
}
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

	fn uuid(&self) -> UuidStr {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::uuid(dir),
			DBDirObject::Root(root) => DBDirTrait::uuid(root),
		}
	}

	fn name(&self) -> Option<&str> {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::name(dir),
			DBDirObject::Root(root) => DBDirTrait::name(root),
		}
	}
}

#[allow(dead_code)]
pub(crate) trait DBDirTrait: Sync + Send {
	fn id(&self) -> i64;
	fn uuid(&self) -> UuidStr;
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
		crate::sql::update_items_with_parent(conn, dirs, files, ParentUuid::Uuid(self.uuid()))
	}

	fn select_children(
		&self,
		conn: &Connection,
		order_by: Option<&str>,
	) -> SQLResult<Vec<DBNonRootObject>> {
		crate::sql::select_children(conn, order_by, ParentUuid::Uuid(self.uuid()))
	}
}
