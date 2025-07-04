#![allow(dead_code)]

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
use libsqlite3_sys::SQLITE_CONSTRAINT_UNIQUE;
use log::trace;
use rusqlite::{
	CachedStatement, Connection, OptionalExtension, Result, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use sha2::Digest;

use super::SQLError;

type SQLResult<T> = std::result::Result<T, SQLError>;

const UPSERT_ITEM_CONFLICT_UUID_SQL: &str = include_str!("../../sql/upsert_item_conflict_uuid.sql");
const UPSERT_ITEM_CONFLICT_NAME_PARENT_SQL: &str =
	include_str!("../../sql/upsert_item_conflict_name_parent.sql");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i8)]
pub enum ItemType {
	Root,
	Dir,
	File,
}

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
	pub(crate) type_: ItemType,
}

fn upsert_item_with_stmts(
	uuid: UuidStr,
	parent: Option<ParentUuid>,
	name: &str,
	type_: ItemType,
	upsert_item_conflict_uuid: &mut CachedStatement<'_>,
	upsert_item_conflict_name_parent: &mut CachedStatement<'_>,
) -> Result<i64> {
	trace!("Upserting item: uuid = {uuid}, parent = {parent:?}, name = {name}, type = {type_:?}");
	let id = match upsert_item_conflict_uuid.query_one((uuid, parent, name, type_), |row| {
		let id: i64 = row.get(0)?;
		Ok(id)
	}) {
		Ok(id) => id,
		Err(rusqlite::Error::SqliteFailure(
			libsqlite3_sys::Error {
				code: libsqlite3_sys::ErrorCode::ConstraintViolation,
				extended_code: SQLITE_CONSTRAINT_UNIQUE,
			},
			_,
		)) => {
			trace!("Conflict on UUID, trying to resolve by name and parent");
			// might be a (parent, name, is_stale) conflict, so try to set the UUID and type
			upsert_item_conflict_name_parent.query_one((uuid, parent, name, type_), |row| {
				let id: i64 = row.get(0)?;
				Ok(id)
			})?
		}
		Err(e) => return Err(e),
	};
	Ok(id)
}

fn upsert_item(
	conn: &Connection,
	uuid: UuidStr,
	parent: Option<ParentUuid>,
	name: &str,
	type_: ItemType,
) -> Result<i64> {
	let mut upsert_item_conflict_uuid = conn.prepare_cached(UPSERT_ITEM_CONFLICT_UUID_SQL)?;
	let mut upsert_item_conflict_name_parent =
		conn.prepare_cached(UPSERT_ITEM_CONFLICT_NAME_PARENT_SQL)?;
	upsert_item_with_stmts(
		uuid,
		parent,
		name,
		type_,
		&mut upsert_item_conflict_uuid,
		&mut upsert_item_conflict_name_parent,
	)
}

impl RawDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: row.get(2)?,
			name: row.get(3)?,
			type_: row.get(4)?,
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
}

impl InnerDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: row.get(2)?,
			name: row.get(3)?,
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
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Self::from_inner_and_row(InnerDBItem::from_row(row)?, row, 4)
	}

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
		upsert_item_conflict_uuid: &mut CachedStatement<'_>,
		upsert_item_conflict_name_parent: &mut CachedStatement<'_>,
		upsert_file: &mut CachedStatement<'_>,
	) -> Result<Self> {
		trace!("Upserting remote file: {remote_file:?}");
		let id = upsert_item_with_stmts(
			remote_file.uuid(),
			Some(remote_file.parent()),
			remote_file.name(),
			ItemType::File,
			upsert_item_conflict_uuid,
			upsert_item_conflict_name_parent,
		)?;
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
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_file: RemoteFile,
	) -> Result<Self> {
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_conflict_uuid = tx.prepare_cached(UPSERT_ITEM_CONFLICT_UUID_SQL)?;
			let mut upsert_item_conflict_name_parent =
				tx.prepare_cached(UPSERT_ITEM_CONFLICT_NAME_PARENT_SQL)?;
			let mut upsert_file = tx.prepare_cached(include_str!("../../sql/upsert_file.sql"))?;
			Self::upsert_from_remote_stmts(
				remote_file,
				&mut upsert_item_conflict_uuid,
				&mut upsert_item_conflict_name_parent,
				&mut upsert_file,
			)?
		};
		tx.commit()?;
		Ok(new)
	}

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

// for testing only
impl From<RemoteFile> for DBFile {
	fn from(value: RemoteFile) -> Self {
		Self {
			id: 0,
			uuid: value.uuid(),
			parent: value.parent(),
			file_key: value.key().to_str().to_string(),
			created: value.created().timestamp_millis(),
			modified: value.last_modified().timestamp_millis(),
			size: value.size() as i64,
			chunks: value.chunks() as i64,
			// this might cause issues, but I'm going to go with it for now
			favorite_rank: if value.favorited() { 1 } else { 0 },
			hash: value.hash().map(Into::<[u8; 64]>::into),
			version: value.key().version() as u8,
			mime: value.file.mime,
			name: value.file.name,
			bucket: value.bucket,
			region: value.region,
		}
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
}

impl DBDir {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Self::from_inner_and_row(InnerDBItem::from_row(row)?, row, 4)
	}

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

	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> SQLResult<Self> {
		match DBObject::select(conn, uuid)? {
			DBObject::Dir(dir) => Ok(dir),
			obj => Err(SQLError::UnexpectedType(obj.item_type(), ItemType::Dir)),
		}
	}

	pub(crate) fn upsert_from_remote_stmts(
		remote_dir: RemoteDirectory,
		upsert_item_conflict_uuid: &mut CachedStatement<'_>,
		upsert_item_conflict_name_parent: &mut CachedStatement<'_>,
		upsert_dir: &mut CachedStatement<'_>,
	) -> Result<Self> {
		let id = upsert_item_with_stmts(
			remote_dir.uuid(),
			Some(remote_dir.parent()),
			remote_dir.name(),
			ItemType::Dir,
			upsert_item_conflict_uuid,
			upsert_item_conflict_name_parent,
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
		})
	}

	pub(crate) fn upsert_from_remote(
		conn: &mut Connection,
		remote_dir: RemoteDirectory,
	) -> Result<Self> {
		trace!("Upserting remote dir: {remote_dir:?}");
		let tx = conn.transaction()?;
		let new = {
			let mut upsert_item_conflict_uuid = tx.prepare_cached(UPSERT_ITEM_CONFLICT_UUID_SQL)?;
			let mut upsert_item_conflict_name_parent =
				tx.prepare_cached(UPSERT_ITEM_CONFLICT_NAME_PARENT_SQL)?;
			let mut upsert_dir = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;
			Self::upsert_from_remote_stmts(
				remote_dir,
				&mut upsert_item_conflict_uuid,
				&mut upsert_item_conflict_name_parent,
				&mut upsert_dir,
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

// for testing only
impl From<RemoteDirectory> for DBDir {
	fn from(value: RemoteDirectory) -> Self {
		Self {
			id: 0,
			uuid: value.uuid(),
			parent: value.parent(),
			created: value.created().map(|t| t.timestamp_millis()),
			// this might cause issues, but I'm going to go with it for now
			favorite_rank: if value.favorited() { 1 } else { 0 },
			last_listed: 0,
			color: value.color,
			name: value.name,
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
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		Self::from_inner_and_row(InnerDBItem::from_row(row)?, row, 4)
	}

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
		let id = upsert_item(
			&tx,
			remote_root.uuid(),
			None, // root has no parent
			"",
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
				ItemType::Dir => Self::Dir(DBDir::from_inner_and_row(item.into(), row, 5)?),
				ItemType::File => Self::File(DBFile::from_inner_and_row(item.into(), row, 9)?),
				ItemType::Root => Self::Root(DBRoot::from_inner_and_row(item.into(), row, 20)?),
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

impl DBDirObject {
	pub(crate) fn last_listed(&self) -> i64 {
		match self {
			DBDirObject::Dir(dir) => dir.last_listed,
			DBDirObject::Root(root) => root.last_listed,
		}
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
		let type_: ItemType = row.get(4)?;
		Ok(match type_ {
			ItemType::Dir => DBNonRootObject::Dir(DBDir::from_inner_and_row(item, row, 5)?),
			ItemType::File => DBNonRootObject::File(DBFile::from_inner_and_row(item, row, 9)?),
			_ => return Err(SQLError::UnexpectedType(type_, ItemType::Dir)),
		})
	}

	pub(crate) fn certain_parent(&self) -> ParentUuid {
		match self {
			DBNonRootObject::Dir(dir) => dir.parent,
			DBNonRootObject::File(file) => file.parent,
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
	fn find_child_file(&self, conn: &Connection, name: &str) -> Result<Option<DBFile>>;
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
		let tx = conn.transaction()?;
		{
			let mut stmt =
				tx.prepare_cached(include_str!("../../sql/mark_stale_with_parent.sql"))?;
			stmt.execute([self.uuid()])?;

			let mut upsert_item_conflict_uuid = tx.prepare_cached(UPSERT_ITEM_CONFLICT_UUID_SQL)?;
			let mut upsert_item_conflict_name_parent =
				tx.prepare_cached(UPSERT_ITEM_CONFLICT_NAME_PARENT_SQL)?;
			let mut upsert_dir = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;

			dirs.into_iter().try_for_each(|d| -> Result<()> {
				DBDir::upsert_from_remote_stmts(
					d,
					&mut upsert_item_conflict_uuid,
					&mut upsert_item_conflict_name_parent,
					&mut upsert_dir,
				)?;
				Ok(())
			})?;

			let mut upsert_file = tx.prepare_cached(include_str!("../../sql/upsert_file.sql"))?;

			files.into_iter().try_for_each(|f| -> Result<()> {
				DBFile::upsert_from_remote_stmts(
					f,
					&mut upsert_item_conflict_uuid,
					&mut upsert_item_conflict_name_parent,
					&mut upsert_file,
				)?;
				Ok(())
			})?;

			let mut stmt =
				tx.prepare_cached(include_str!("../../sql/delete_stale_with_parent.sql"))?;
			stmt.execute([self.uuid()])?;
		}
		tx.commit()?;
		Ok(())
	}

	fn find_child_file(&self, conn: &Connection, name: &str) -> Result<Option<DBFile>> {
		let mut stmt = conn.prepare_cached(include_str!(
			"../../sql/select_child_file_joined_by_name.sql"
		))?;
		stmt.query_one((self.uuid(), name), DBFile::from_row)
			.optional()
	}

	fn select_children(
		&self,
		conn: &Connection,
		order_by: Option<&str>,
	) -> SQLResult<Vec<DBNonRootObject>> {
		let order_by = match order_by {
			Some(order_by) => convert_order_by(order_by),
			_ => "ORDER BY items.name ASC",
		};

		let select_query = format!(
			"{} {}",
			include_str!("../../sql/select_dir_children.sql"),
			order_by
		);
		let mut stmt = conn.prepare(&select_query)?;
		stmt.query_and_then([self.uuid()], DBNonRootObject::from_row)?
			.collect::<SQLResult<Vec<_>>>()
	}
}

pub(crate) trait DBItemTrait: Sync + Send {
	fn id(&self) -> i64;
	fn uuid(&self) -> UuidStr;
	fn parent(&self) -> Option<ParentUuid>;
	fn name(&self) -> &str;
	fn item_type(&self) -> ItemType;
}

pub(crate) trait DBItemExt: DBItemTrait {
	fn trash(&self, conn: &Connection) -> Result<bool>;
	fn delete(&self, conn: &Connection) -> Result<bool>;
}

impl<T> DBItemExt for T
where
	T: DBItemTrait + Sync + Send,
{
	fn trash(&self, conn: &Connection) -> Result<bool> {
		trace!(
			"Trashing item: uuid = {}, name = {}",
			self.uuid(),
			self.name()
		);
		let mut stmt = conn.prepare_cached(include_str!("../../sql/trash_item.sql"))?;
		let num_rows = stmt.execute([self.id()])?;
		if num_rows == 0 {
			trace!(
				"No rows updated for item: uuid = {}, name = {}",
				self.uuid(),
				self.name()
			);
			return Ok(false);
		}
		Ok(true)
	}

	fn delete(&self, conn: &Connection) -> Result<bool> {
		trace!(
			"Removing item: uuid = {}, name = {}",
			self.uuid(),
			self.name()
		);
		let mut stmt = conn.prepare_cached(include_str!("../../sql/delete_item.sql"))?;
		let num_rows = stmt.execute([self.id()])?;
		if num_rows == 0 {
			trace!(
				"No rows deleted for item: uuid = {}, name = {}",
				self.uuid(),
				self.name()
			);
			return Ok(false);
		}
		Ok(true)
	}
}

fn convert_order_by(order_by: &str) -> &'static str {
	if order_by.contains("display_name") {
		if order_by.contains("ASC") {
			return "ORDER BY items.name ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY items.name DESC";
		}
	} else if order_by.contains("last_modified") {
		if order_by.contains("ASC") {
			return "ORDER BY files.modified + 0 ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY files.modified + 0 DESC";
		}
	} else if order_by.contains("size") {
		if order_by.contains("ASC") {
			return "ORDER BY files.size + 0 ASC";
		} else if order_by.contains("DESC") {
			return "ORDER BY files.size + 0 DESC";
		}
	}
	"ORDER BY items.name ASC"
}
