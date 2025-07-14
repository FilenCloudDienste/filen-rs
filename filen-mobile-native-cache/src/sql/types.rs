use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	crypto::{error::ConversionError, file::FileKey},
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID, UnsharedFSObject,
		dir::{RemoteDirectory, RootDirectory, traits::HasRemoteDirInfo},
		file::{
			FlatRemoteFile, RemoteFile,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
	},
};
use filen_types::fs::{ParentUuid, UuidStr};
use log::trace;
use rusqlite::{
	CachedStatement, Connection, OptionalExtension, Result, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use sha2::Digest;

use crate::{ffi::ItemType, sql::json_object::JsonObject};

use super::SQLError;

pub(crate) type SQLResult<T> = std::result::Result<T, SQLError>;

pub(crate) const UPSERT_ITEM_SQL: &str = include_str!("../../sql/upsert_item.sql");

impl FromSql for ItemType {
	fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
		let i8 = i8::column_result(value)?;
		Ok(match i8 {
			0 => ItemType::Root,
			1 => ItemType::Dir,
			2 => ItemType::File,
			_ => return Err(FromSqlError::InvalidType),
		})
	}
}

impl ToSql for ItemType {
	fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput<'_>, rusqlite::Error> {
		let i8_value: i8 = match self {
			ItemType::Root => 0,
			ItemType::Dir => 1,
			ItemType::File => 2,
		};
		Ok(rusqlite::types::ToSqlOutput::from(i8_value))
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDBItem {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: Option<ParentUuid>, // parent can be None for root items
	pub(crate) name: String,
	pub(crate) local_data: Option<JsonObject>, // local data is optional, used for storing additional metadata
	pub(crate) type_: ItemType,
}

fn upsert_item_with_stmts(
	uuid: UuidStr,
	parent: Option<ParentUuid>,
	name: &str,
	local_data: Option<JsonObject>,
	type_: ItemType,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> Result<(i64, Option<JsonObject>)> {
	trace!("Upserting item: uuid = {uuid}, parent = {parent:?}, name = {name}, type = {type_:?}");
	let (id, local_data) = upsert_item_stmt
		.query_row((uuid, parent, name, local_data, type_), |row| {
			Ok((row.get(0)?, row.get(1)?))
		})?;
	Ok((id, local_data))
}

fn upsert_item(
	conn: &Connection,
	uuid: UuidStr,
	parent: Option<ParentUuid>,
	name: &str,
	local_data: Option<JsonObject>,
	type_: ItemType,
) -> Result<(i64, Option<JsonObject>)> {
	let mut upsert_item_stmt = conn.prepare_cached(UPSERT_ITEM_SQL)?;
	upsert_item_with_stmts(uuid, parent, name, local_data, type_, &mut upsert_item_stmt)
}

impl RawDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: row.get(2)?,
			name: row.get(3)?,
			local_data: row.get(4).unwrap(),
			type_: row.get(5)?,
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> Result<Option<Self>> {
		let mut stmt = conn.prepare_cached(include_str!("../../sql/select_item.sql"))?;
		stmt.query_one([uuid], Self::from_row).optional()
	}

	pub(crate) fn into_db_object(self, conn: &Connection) -> Result<DBObject> {
		match self.type_ {
			ItemType::File => Ok(DBObject::File(DBFile::from_item(self.into(), conn)?)),
			ItemType::Dir => Ok(DBObject::Dir(DBDir::from_item(self.into(), conn)?)),
			ItemType::Root => Ok(DBObject::Root(DBRoot::from_item(self.into(), conn)?)),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InnerDBItem {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: Option<ParentUuid>, // parent can be None for root items
	pub(crate) name: String,
	pub(crate) local_data: Option<JsonObject>, // local data is optional, used for storing additional metadata
}

impl InnerDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: row.get(2)?,
			name: row.get(3)?,
			local_data: row.get(4).unwrap(),
		})
	}
}

impl From<RawDBItem> for InnerDBItem {
	fn from(raw: RawDBItem) -> Self {
		Self {
			id: raw.id,
			uuid: raw.uuid,
			parent: raw.parent,
			name: raw.name,
			local_data: raw.local_data,
		}
	}
}

#[derive(Clone, PartialEq, Eq)]
pub struct DBFile {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: ParentUuid,
	pub(crate) name: String,
	pub(crate) mime: String,
	pub(crate) file_key: String,
	pub(crate) created: i64,
	pub(crate) modified: i64,
	pub(crate) size: i64,
	pub(crate) chunks: i64,
	pub(crate) favorite_rank: i64,
	pub(crate) region: String,
	pub(crate) bucket: String,
	pub(crate) hash: Option<[u8; 64]>,
	pub(crate) version: u8,
	pub(crate) local_data: Option<JsonObject>,
}

impl std::fmt::Debug for DBFile {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let key_hash_str = faster_hex::hex_string(&sha2::Sha512::digest(self.file_key.as_bytes()));
		f.debug_struct("DBFile")
			.field("id", &self.id)
			.field("uuid", &self.uuid)
			.field("parent", &self.parent)
			.field("name", &self.name)
			.field("mime", &self.mime)
			.field("file_key (hashed)", &key_hash_str)
			.field("created", &self.created)
			.field("modified", &self.modified)
			.field("size", &self.size)
			.field("chunks", &self.chunks)
			.field("favorite_rank", &self.favorite_rank)
			.field("region", &self.region)
			.field("bucket", &self.bucket)
			.field("hash", &self.hash.map(|h| faster_hex::hex_string(&h)))
			.finish()
	}
}

impl DBFile {
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
					"Parent UUID cannot be None for DBFile".into(),
				)
			})?,
			name: item.name,
			local_data: item.local_data,
			mime: row.get(idx)?,
			file_key: row.get(idx + 1)?,
			created: row.get(idx + 2)?,
			modified: row.get(idx + 3)?,
			size: row.get(idx + 4)?,
			chunks: row.get(idx + 5)?,
			favorite_rank: row.get(idx + 6)?,
			region: row.get(idx + 7)?,
			bucket: row.get(idx + 8)?,
			hash: row.get(idx + 9)?,
			version: row.get(idx + 10)?,
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::File(file) => Ok(file),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::File)),
		}
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(include_str!("../../sql/select_file.sql"))?;
		stmt.query_one([item.id], |row| Self::from_inner_and_row(item, row, 0))
	}

	pub(crate) fn upsert_from_remote_stmts(
		remote_file: RemoteFile,
		upsert_item_stmt: &mut CachedStatement<'_>,
		upsert_file: &mut CachedStatement<'_>,
	) -> Result<Self> {
		trace!("Upserting remote file: {remote_file:?}");
		let (id, local_data) = upsert_item_with_stmts(
			remote_file.uuid(),
			Some(remote_file.parent()),
			remote_file.name(),
			None,
			ItemType::File,
			upsert_item_stmt,
		)?;
		trace!(
			"Upserted item with id: {id} for remote file: {}",
			remote_file.uuid()
		);
		let file_key = remote_file.key().to_str();
		let version = remote_file.key().version();
		let favorite_rank = upsert_file.query_one(
			(
				id,
				remote_file.mime(),
				&file_key,
				remote_file.created().timestamp_millis(),
				remote_file.last_modified().timestamp_millis(),
				remote_file.size() as i64,
				remote_file.chunks() as i64,
				remote_file.favorited() as u8,
				remote_file.region(),
				remote_file.bucket(),
				remote_file.hash().map(Into::<[u8; 64]>::into),
				version as u8,
			),
			|r| r.get(0),
		)?;
		Ok(Self {
			id,
			uuid: remote_file.uuid(),
			parent: remote_file.parent(),
			file_key: file_key.to_string(),
			created: remote_file.created().timestamp_millis(),
			modified: remote_file.last_modified().timestamp_millis(),
			size: remote_file.size() as i64,
			chunks: remote_file.chunks() as i64,
			favorite_rank,
			hash: remote_file.hash().map(|h| h.into()),
			name: remote_file.file.name,
			mime: remote_file.file.mime,
			region: remote_file.region,
			bucket: remote_file.bucket,
			version: version as u8,
			local_data,
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_file: RemoteFile,
	) -> Result<Self> {
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM_SQL)?;
			let mut upsert_file = tx.prepare_cached(include_str!("../../sql/upsert_file.sql"))?;
			Self::upsert_from_remote_stmts(remote_file, &mut upsert_item_stmt, &mut upsert_file)?
		};
		tx.commit()?;
		Ok(new)
	}

	#[allow(dead_code)]
	pub(crate) fn update_from_remote(
		&mut self,
		conn: &mut Connection,
		file: RemoteFile,
	) -> Result<()> {
		let tx = conn.transaction()?;
		let file_key = file.key().to_str();
		let created = file.created().timestamp_millis();
		let modified = file.last_modified().timestamp_millis();
		let size = file.size() as i64;
		let chunks = file.chunks() as i64;
		let hash = file.hash().map(Into::<[u8; 64]>::into);
		let favorite_rank = {
			let mut stmt = tx.prepare_cached(
				"
		UPDATE items SET uuid = ?, parent = ?, name = ? WHERE uuid = ? RETURNING id LIMIT 1;",
			)?;
			stmt.execute((file.uuid(), file.parent(), file.name(), self.uuid))?;
			let mut stmt = tx.prepare_cached(include_str!("../../sql/update_file.sql"))?;

			stmt.query_one(
				(
					file.mime(),
					&file_key,
					created,
					modified,
					size,
					chunks,
					file.favorited() as u8,
					file.region(),
					file.bucket(),
					hash,
					self.id,
				),
				|r| r.get(0),
			)?
		};

		tx.commit()?;
		self.uuid = file.uuid();
		self.parent = file.parent();
		self.favorite_rank = favorite_rank;
		self.file_key = file_key.to_string();
		self.name = file.file.name;
		self.mime = file.file.mime;
		self.created = created;
		self.modified = modified;
		self.size = size;
		self.chunks = chunks;
		self.region = file.region;
		self.bucket = file.bucket;
		self.hash = hash;
		Ok(())
	}

	pub(crate) fn update_favorite_rank(
		&mut self,
		conn: &Connection,
		favorite_rank: i64,
	) -> Result<()> {
		let mut stmt =
			conn.prepare_cached(include_str!("../../sql/update_file_favorite_rank.sql"))?;
		stmt.execute((favorite_rank, self.id))?;
		self.favorite_rank = favorite_rank;
		Ok(())
	}
}

impl DBItemTrait for DBFile {
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		Some(self.parent)
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn item_type(&self) -> ItemType {
		ItemType::File
	}
}

impl TryFrom<DBFile> for RemoteFile {
	type Error = ConversionError;
	fn try_from(value: DBFile) -> Result<Self, Self::Error> {
		Ok(FlatRemoteFile {
			uuid: value.uuid,
			parent: value.parent,
			name: value.name,
			mime: value.mime,
			created: DateTime::<Utc>::from_timestamp_millis(value.created).unwrap_or_default(),
			modified: DateTime::<Utc>::from_timestamp_millis(value.modified).unwrap_or_default(),
			size: value.size as u64,
			chunks: value.chunks as u64,
			favorited: value.favorite_rank > 0,
			key: FileKey::from_str_with_version(&value.file_key, value.version.into())?,
			region: value.region,
			bucket: value.bucket,
			hash: value.hash.map(|h| h.into()),
		}
		.into())
	}
}

impl PartialEq<RemoteFile> for DBFile {
	fn eq(&self, other: &RemoteFile) -> bool {
		self.uuid == other.uuid()
			&& self.parent == other.parent()
			&& self.name == other.name()
			&& self.mime == other.mime()
			&& self.created == other.created().timestamp_millis()
			&& self.modified == other.last_modified().timestamp_millis()
			&& self.size as u64 == other.size()
			&& self.chunks as u64 == other.chunks()
			&& (self.favorite_rank > 0) == other.favorited()
			&& self.file_key == other.key().to_str()
			&& self.region == other.region()
			&& self.bucket == other.bucket()
			&& self.hash == other.hash.map(|h| h.into())
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DBDir {
	pub(crate) id: i64,
	pub(crate) uuid: UuidStr,
	pub(crate) parent: ParentUuid,
	pub(crate) name: String,
	pub(crate) created: Option<i64>,
	pub(crate) favorite_rank: i64,
	pub(crate) color: Option<String>,
	pub(crate) last_listed: i64,
	pub(crate) local_data: Option<JsonObject>,
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
			name: item.name,
			local_data: item.local_data,
			created: row.get(idx)?,
			favorite_rank: row.get(idx + 1)?,
			color: row.get(idx + 2)?,
			last_listed: row.get(idx + 3)?,
		})
	}

	pub(crate) fn from_item(item: InnerDBItem, conn: &Connection) -> Result<Self> {
		let mut stmt = conn.prepare_cached(include_str!("../../sql/select_dir.sql"))?;
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
	) -> Result<Self> {
		let (id, local_data) = upsert_item_with_stmts(
			remote_dir.uuid(),
			Some(remote_dir.parent()),
			remote_dir.name(),
			None,
			ItemType::Dir,
			upsert_item_stmt,
		)?;
		trace!("Upserting remote dir: {remote_dir:?}");
		let (last_listed, favorite_rank) = upsert_dir.query_one(
			(
				id,
				remote_dir.created().map(|t| t.timestamp_millis()),
				remote_dir.favorited() as u8,
				remote_dir.color().map(ToString::to_string),
			),
			|r| {
				let last_listed: i64 = r.get(0)?;
				let favorite_rank: i64 = r.get(1)?;
				Ok((last_listed, favorite_rank))
			},
		)?;
		Ok(Self {
			id,
			uuid: remote_dir.uuid(),
			parent: remote_dir.parent(),
			favorite_rank,
			created: remote_dir.created().map(|t| t.timestamp_millis()),
			name: remote_dir.name,
			color: remote_dir.color,
			last_listed,
			local_data,
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_dir: RemoteDirectory,
	) -> Result<Self> {
		trace!("Upserting remote dir: {remote_dir:?}");
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM_SQL)?;
			let mut upsert_dir = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;
			Self::upsert_from_remote_stmts(remote_dir, &mut upsert_item_stmt, &mut upsert_dir)?
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
		let mut stmt =
			conn.prepare_cached(include_str!("../../sql/update_dir_favorite_rank.sql"))?;
		stmt.execute((favorite_rank, self.id))?;
		self.favorite_rank = favorite_rank;
		Ok(())
	}
}

impl DBDirTrait for DBDir {
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

	fn name(&self) -> &str {
		&self.name
	}

	fn set_last_listed(&mut self, value: i64) {
		self.last_listed = value;
	}
}

impl DBItemTrait for DBDir {
	fn id(&self) -> i64 {
		self.id
	}

	fn uuid(&self) -> UuidStr {
		self.uuid
	}

	fn parent(&self) -> Option<ParentUuid> {
		Some(self.parent)
	}

	fn name(&self) -> &str {
		&self.name
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
			name: value.name,
			color: value.color,
			created: value
				.created
				.map(|t| DateTime::<Utc>::from_timestamp_millis(t).unwrap_or_default()),
			favorited: value.favorite_rank > 0,
		}
	}
}

impl PartialEq<RemoteDirectory> for DBDir {
	fn eq(&self, other: &RemoteDirectory) -> bool {
		self.uuid == other.uuid()
			&& self.parent == other.parent()
			&& self.name == other.name()
			&& self.color.as_deref() == other.color()
			&& self.created == other.created().map(|t| t.timestamp_millis())
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
		let mut stmt = conn.prepare_cached(include_str!("../../sql/select_root.sql"))?;
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
		let (id, _) = upsert_item(
			&tx,
			remote_root.uuid(),
			None, // root has no parent
			"",
			None,
			ItemType::Root,
		)?;
		let mut stmt = tx.prepare_cached(include_str!("../../sql/upsert_root_empty.sql"))?;
		let (storage_used, max_storage, last_updated) = stmt.query_one([id], |f| {
			let storage_used: i64 = f.get(0)?;
			let max_storage: i64 = f.get(1)?;
			let last_updated: i64 = f.get(2)?;
			Ok((storage_used, max_storage, last_updated))
		})?;
		std::mem::drop(stmt);
		let mut stmt = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;
		let last_listed = stmt.query_one((id, 0, false, Option::<String>::None), |r| {
			let last_listed: i64 = r.get(0)?;
			Ok(last_listed)
		})?;
		std::mem::drop(stmt);
		tx.commit()?;
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

	fn name(&self) -> &str {
		""
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

	fn name(&self) -> &str {
		""
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DBObject {
	File(DBFile),
	Dir(DBDir),
	Root(DBRoot),
}

impl DBObject {
	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> Result<Self> {
		let mut stmt = conn.prepare_cached(include_str!("../../sql/select_object.sql"))?;
		stmt.query_one([uuid], |row| {
			let item = RawDBItem::from_row(row)?;
			Ok(match item.type_ {
				ItemType::Dir => Self::Dir(DBDir::from_inner_and_row(item.into(), row, 6)?),
				ItemType::File => Self::File(DBFile::from_inner_and_row(item.into(), row, 10)?),
				ItemType::Root => Self::Root(DBRoot::from_inner_and_row(item.into(), row, 21)?),
			})
		})
	}

	pub(crate) fn item_type(&self) -> ItemType {
		match self {
			DBObject::File(_) => ItemType::File,
			DBObject::Dir(_) => ItemType::Dir,
			DBObject::Root(_) => ItemType::Root,
		}
	}

	pub(crate) fn uuid(&self) -> UuidStr {
		match self {
			DBObject::File(file) => file.uuid,
			DBObject::Dir(dir) => dir.uuid,
			DBObject::Root(root) => root.uuid,
		}
	}

	pub(crate) fn upsert_from_remote(conn: &mut Connection, obj: UnsharedFSObject) -> Result<Self> {
		match obj {
			UnsharedFSObject::File(file) => {
				Ok(DBFile::upsert_from_remote(conn, file.into_owned())?.into())
			}
			UnsharedFSObject::Dir(dir) => {
				Ok(DBDir::upsert_from_remote(conn, dir.into_owned())?.into())
			}
			UnsharedFSObject::Root(root) => Ok(DBRoot::upsert_from_remote(conn, &root)?.into()),
		}
	}
}

impl From<DBDir> for DBObject {
	fn from(dir: DBDir) -> Self {
		DBObject::Dir(dir)
	}
}

impl From<DBFile> for DBObject {
	fn from(file: DBFile) -> Self {
		DBObject::File(file)
	}
}

impl From<DBRoot> for DBObject {
	fn from(root: DBRoot) -> Self {
		DBObject::Root(root)
	}
}

impl PartialEq<DBObject> for RemoteDirectory {
	fn eq(&self, other: &DBObject) -> bool {
		match other {
			DBObject::Dir(dir) => dir == self,
			DBObject::File(_) => false,
			DBObject::Root(_) => false,
		}
	}
}

impl DBItemTrait for DBObject {
	fn id(&self) -> i64 {
		match self {
			DBObject::File(file) => file.id,
			DBObject::Dir(dir) => dir.id,
			DBObject::Root(root) => root.id,
		}
	}

	fn uuid(&self) -> UuidStr {
		match self {
			DBObject::File(file) => file.uuid,
			DBObject::Dir(dir) => dir.uuid,
			DBObject::Root(root) => root.uuid,
		}
	}

	fn parent(&self) -> Option<ParentUuid> {
		match self {
			DBObject::File(file) => Some(file.parent),
			DBObject::Dir(dir) => Some(dir.parent),
			DBObject::Root(_) => None,
		}
	}

	fn name(&self) -> &str {
		match self {
			DBObject::File(file) => &file.name,
			DBObject::Dir(dir) => &dir.name,
			DBObject::Root(_) => "",
		}
	}

	fn item_type(&self) -> ItemType {
		self.item_type()
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

	fn name(&self) -> &str {
		match self {
			DBDirObject::Dir(dir) => DBDirTrait::name(dir),
			DBDirObject::Root(root) => DBDirTrait::name(root),
		}
	}
}

#[derive(Debug)]
pub enum DBNonRootObject {
	Dir(DBDir),
	File(DBFile),
}

impl DBNonRootObject {
	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
		Ok(match DBObject::select(conn, uuid)? {
			DBObject::Dir(dir) => DBNonRootObject::Dir(dir),
			DBObject::File(file) => DBNonRootObject::File(file),
			DBObject::Root(_) => {
				return Err(SQLError::UnexpectedType(ItemType::Root, ItemType::Dir));
			}
		})
	}

	pub(crate) fn from_row(row: &rusqlite::Row) -> SQLResult<Self> {
		let item = InnerDBItem::from_row(row)?;
		let type_: ItemType = row.get(5)?;
		Ok(match type_ {
			ItemType::Dir => DBNonRootObject::Dir(DBDir::from_inner_and_row(item, row, 6)?),
			ItemType::File => DBNonRootObject::File(DBFile::from_inner_and_row(item, row, 10)?),
			_ => return Err(SQLError::UnexpectedType(type_, ItemType::Dir)),
		})
	}

	pub(crate) fn certain_parent(&self) -> ParentUuid {
		match self {
			DBNonRootObject::Dir(dir) => dir.parent,
			DBNonRootObject::File(file) => file.parent,
		}
	}

	pub(crate) fn local_data(&self) -> Option<&JsonObject> {
		match self {
			DBNonRootObject::Dir(dir) => dir.local_data.as_ref(),
			DBNonRootObject::File(file) => file.local_data.as_ref(),
		}
	}

	pub(crate) fn set_local_data(&mut self, local_data: Option<JsonObject>) {
		match self {
			DBNonRootObject::Dir(dir) => dir.local_data = local_data,
			DBNonRootObject::File(file) => file.local_data = local_data,
		}
	}
}

impl DBItemTrait for DBNonRootObject {
	fn id(&self) -> i64 {
		match self {
			DBNonRootObject::Dir(dir) => DBItemTrait::id(dir),
			DBNonRootObject::File(file) => DBItemTrait::id(file),
		}
	}

	fn uuid(&self) -> UuidStr {
		match self {
			DBNonRootObject::Dir(dir) => DBItemTrait::uuid(dir),
			DBNonRootObject::File(file) => DBItemTrait::uuid(file),
		}
	}

	fn parent(&self) -> Option<ParentUuid> {
		match self {
			DBNonRootObject::Dir(dir) => Some(dir.parent),
			DBNonRootObject::File(file) => Some(file.parent),
		}
	}

	fn name(&self) -> &str {
		match self {
			DBNonRootObject::Dir(dir) => DBItemTrait::name(dir),
			DBNonRootObject::File(file) => DBItemTrait::name(file),
		}
	}

	fn item_type(&self) -> ItemType {
		match self {
			DBNonRootObject::Dir(_) => ItemType::Dir,
			DBNonRootObject::File(_) => ItemType::File,
		}
	}
}

impl TryFrom<DBObject> for DBNonRootObject {
	type Error = SQLError;
	fn try_from(obj: DBObject) -> Result<Self, Self::Error> {
		match obj {
			DBObject::Dir(dir) => Ok(DBNonRootObject::Dir(dir)),
			DBObject::File(file) => Ok(DBNonRootObject::File(file)),
			DBObject::Root(_) => Err(SQLError::UnexpectedType(ItemType::Root, ItemType::Dir)),
		}
	}
}

impl From<DBNonRootObject> for DBObject {
	fn from(obj: DBNonRootObject) -> Self {
		match obj {
			DBNonRootObject::Dir(dir) => DBObject::Dir(dir),
			DBNonRootObject::File(file) => DBObject::File(file),
		}
	}
}

#[allow(dead_code)]
pub(crate) trait DBDirTrait: Sync + Send {
	fn id(&self) -> i64;
	fn uuid(&self) -> UuidStr;
	fn name(&self) -> &str;
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
			conn.prepare_cached(include_str!("../../sql/update_dir_last_listed.sql"))?;
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

#[allow(dead_code)]
pub(crate) trait DBItemTrait: Sync + Send {
	fn id(&self) -> i64;
	fn uuid(&self) -> UuidStr;
	fn parent(&self) -> Option<ParentUuid>;
	fn name(&self) -> &str;
	fn item_type(&self) -> ItemType;
}

pub(crate) mod json_object {
	use std::collections::HashMap;

	use rusqlite::{
		ToSql,
		types::{FromSql, FromSqlError, ToSqlOutput, ValueRef},
	};

	#[derive(Debug, Clone, PartialEq, Eq)]
	pub(crate) struct JsonObject(String);

	impl JsonObject {
		pub fn new(map: HashMap<String, String>) -> Self {
			JsonObject(serde_json::to_string(&map).unwrap_or_default())
		}

		pub fn is_empty(&self) -> bool {
			self.0 == "{}"
		}

		pub fn to_map(&self) -> HashMap<String, String> {
			serde_json::from_str(&self.0).unwrap_or_default()
		}
	}

	impl FromSql for JsonObject {
		fn column_result(value: ValueRef<'_>) -> Result<Self, FromSqlError> {
			match value {
				ValueRef::Text(s) => Ok(JsonObject(
					String::from_utf8(s.to_vec()).map_err(|e| FromSqlError::Other(e.into()))?,
				)),
				_ => Err(FromSqlError::InvalidType),
			}
		}
	}

	impl ToSql for JsonObject {
		fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
			Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0.as_bytes())))
		}
	}
}
