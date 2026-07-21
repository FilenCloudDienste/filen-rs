use std::fmt::Debug;

use filen_types::fs::{ParentUuid, Uuid};
use rusqlite::{
	CachedStatement, Connection, OptionalExtension, Result, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use tracing::trace;

use crate::{
	ffi::ItemType,
	sql::{
		dir::{DBDir, DBRoot},
		file::DBFile,
		object::{DBObject, JsonObject},
		statements::*,
	},
};

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

/// Splits a [`ParentUuid`] into the `(parent, trashed)` pair stored on `items`.
///
/// A trashed item keeps its *original* parent in the `parent` column so it remembers where to
/// restore to; `trashed` is what distinguishes it from a live child. The virtual parents
/// (`Recents`/`Favorites`/`Links`) are never persisted as an item's parent, and the root has no
/// parent — both map to `(None, false)`.
pub(crate) fn decompose_parent(parent: Option<ParentUuid>) -> (Option<Uuid>, bool) {
	match parent {
		Some(ParentUuid::Uuid(uuid)) => (Some(uuid), false),
		Some(ParentUuid::Trash(uuid)) => (Some(uuid), true),
		Some(ParentUuid::Recents | ParentUuid::Favorites | ParentUuid::Links) | None => {
			(None, false)
		}
	}
}

/// Rebuilds the [`ParentUuid`] stored across the `parent`/`trashed` columns.
pub(crate) fn combine_parent(parent: Option<Uuid>, trashed: bool) -> Option<ParentUuid> {
	match (parent, trashed) {
		(Some(uuid), true) => Some(ParentUuid::Trash(uuid)),
		(Some(uuid), false) => Some(ParentUuid::Uuid(uuid)),
		(None, _) => None,
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawDBItem {
	pub(crate) id: i64,
	pub(crate) uuid: Uuid,
	pub(crate) parent: Option<ParentUuid>, // parent can be None for root items
	pub(crate) local_data: Option<JsonObject>, // local data is optional, used for storing additional metadata
	pub(crate) type_: ItemType,
}

pub(crate) fn upsert_item_with_stmts(
	uuid: Uuid,
	parent: Option<ParentUuid>,
	name: Option<&str>,
	local_data: Option<JsonObject>,
	type_: ItemType,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> Result<(i64, Option<JsonObject>)> {
	trace!("Upserting item: uuid = {uuid}, parent = {parent:?}, name = {name:?}, type = {type_:?}");
	let (parent_uuid, trashed) = decompose_parent(parent);
	let (id, local_data) = upsert_item_stmt.query_row(
		(uuid, parent_uuid, name, local_data, type_, trashed),
		|row| Ok((row.get(0)?, row.get(1)?)),
	)?;
	trace!("Upserted item with id: {id}");
	Ok((id, local_data))
}

pub(crate) fn upsert_item(
	conn: &Connection,
	uuid: Uuid,
	parent: Option<ParentUuid>,
	name: Option<&str>,
	local_data: Option<JsonObject>,
	type_: ItemType,
) -> Result<(i64, Option<JsonObject>)> {
	let mut upsert_item_stmt = conn.prepare_cached(UPSERT_ITEM)?;
	upsert_item_with_stmts(uuid, parent, name, local_data, type_, &mut upsert_item_stmt)
}

impl RawDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let parent: Option<Uuid> = row.get(2)?;
		let trashed: bool = row.get(3)?;
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: combine_parent(parent, trashed),
			local_data: row.get(4).unwrap(),
			type_: row.get(5)?,
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: Uuid) -> Result<Option<Self>> {
		let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_UUID)?;
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
	pub(crate) uuid: Uuid,
	pub(crate) parent: Option<ParentUuid>, // parent can be None for root items
	pub(crate) local_data: Option<JsonObject>, // local data is optional, used for storing additional metadata
}

impl InnerDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let parent: Option<Uuid> = row.get(2)?;
		let trashed: bool = row.get(3)?;
		Ok(Self {
			id: row.get(0)?,
			uuid: row.get(1)?,
			parent: combine_parent(parent, trashed),
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
			local_data: raw.local_data,
		}
	}
}

pub(crate) trait DBItemTrait: Sync + Send {
	fn uuid(&self) -> Uuid;
	fn parent(&self) -> Option<ParentUuid>;
	fn name(&self) -> Option<&str>;
}
