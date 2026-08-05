use std::fmt::Debug;

use filen_types::fs::{ParentUuid, StableUuid, Uuid};
use rusqlite::{
	CachedStatement, Connection, OptionalExtension, Result, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, Null, ValueRef},
};
use tracing::trace;

use crate::{
	ffi::ItemType,
	sql::{
		columns::{
			ITEMS_CHANGE_SEQ, ITEMS_ID, ITEMS_LOCAL_DATA, ITEMS_PARENT, ITEMS_PENDING_UPLOAD_AT,
			ITEMS_TRASHED, ITEMS_TYPE, ITEMS_UUID,
		},
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
	/// The change sequence the row was carrying when it was read; see [`InnerDBItem::change_seq`].
	pub(crate) change_seq: i64,
}

/// Binds the shared `items` upsert.
///
/// `stable` is a file's server-minted whole-life id, or SQL [`Null`] for a dir
/// or root — the server never re-mints those, so their `uuid` already is their
/// whole-life id. The `items` CHECK enforces exactly that split, and this
/// parameter is private so the column can only be written through the
/// type-specific wrappers below.
///
/// Returns the resolved row's id, the `local_data` it ended up holding, and the
/// pending-upload marker it was already carrying — the statement never writes
/// that column, so this is a read-back of what the upsert left alone.
#[allow(clippy::too_many_arguments)]
fn upsert_item_with_stmts(
	uuid: Uuid,
	stable: impl ToSql,
	parent: Option<ParentUuid>,
	name: Option<&str>,
	local_data: Option<JsonObject>,
	type_: ItemType,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> Result<(i64, Option<JsonObject>, Option<i64>)> {
	trace!("Upserting item: uuid = {uuid}, parent = {parent:?}, name = {name:?}, type = {type_:?}");
	let (parent_uuid, trashed) = decompose_parent(parent);
	let (id, local_data, pending_upload_at) = upsert_item_stmt.query_row(
		(uuid, parent_uuid, name, local_data, type_, trashed, stable),
		|row| {
			Ok((
				row.get(ITEMS_ID)?,
				row.get(ITEMS_LOCAL_DATA)?,
				row.get(ITEMS_PENDING_UPLOAD_AT)?,
			))
		},
	)?;
	trace!("Upserted item with id: {id}");
	Ok((id, local_data, pending_upload_at))
}

pub(crate) fn upsert_file_item_with_stmts(
	uuid: Uuid,
	stable_uuid: StableUuid,
	parent: Option<ParentUuid>,
	name: Option<&str>,
	local_data: Option<JsonObject>,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> Result<(i64, Option<JsonObject>, Option<i64>)> {
	upsert_item_with_stmts(
		uuid,
		stable_uuid,
		parent,
		name,
		local_data,
		ItemType::File,
		upsert_item_stmt,
	)
}

/// Dirs never carry a pending-upload marker (the `items` CHECK says so), so the
/// third element is dropped here rather than handed to callers that cannot use it.
pub(crate) fn upsert_dir_item_with_stmts(
	uuid: Uuid,
	parent: Option<ParentUuid>,
	name: Option<&str>,
	local_data: Option<JsonObject>,
	upsert_item_stmt: &mut CachedStatement<'_>,
) -> Result<(i64, Option<JsonObject>)> {
	upsert_item_with_stmts(
		uuid,
		Null,
		parent,
		name,
		local_data,
		ItemType::Dir,
		upsert_item_stmt,
	)
	.map(|(id, local_data, _)| (id, local_data))
}

/// The root has no parent, no name, no local data and no stable id.
pub(crate) fn upsert_root_item(conn: &Connection, uuid: Uuid) -> Result<i64> {
	let mut upsert_item_stmt = conn.prepare_cached(UPSERT_ITEM)?;
	upsert_item_with_stmts(
		uuid,
		Null,
		None,
		None,
		None,
		ItemType::Root,
		&mut upsert_item_stmt,
	)
	.map(|(id, _, _)| id)
}

impl RawDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let parent: Option<Uuid> = row.get(ITEMS_PARENT)?;
		let trashed: bool = row.get(ITEMS_TRASHED)?;
		Ok(Self {
			id: row.get(ITEMS_ID)?,
			uuid: row.get(ITEMS_UUID)?,
			parent: combine_parent(parent, trashed),
			local_data: row.get(ITEMS_LOCAL_DATA).unwrap(),
			type_: row.get(ITEMS_TYPE)?,
			change_seq: row.get(ITEMS_CHANGE_SEQ)?,
		})
	}

	pub(crate) fn select(conn: &Connection, uuid: Uuid) -> Result<Option<Self>> {
		let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_UUID)?;
		stmt.query_one([uuid], Self::from_row).optional()
	}

	/// Select by the server-minted whole-life id. Only files carry one, so this
	/// can only ever match a file row. Prefers the untrashed row when duplicate
	/// stables exist (reachable only via same-account uuid-reuse abuse).
	pub(crate) fn select_by_stable(conn: &Connection, stable_uuid: Uuid) -> Result<Option<Self>> {
		let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_STABLE_UUID)?;
		stmt.query_one([stable_uuid], Self::from_row).optional()
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
	/// The change sequence the row was carrying when it was read — the version an external
	/// replica compares its own copy against. Stamped by the triggers in `init.sql`, so it is
	/// only ever as fresh as the read that produced this struct.
	pub(crate) change_seq: i64,
}

impl InnerDBItem {
	pub(crate) fn from_row(row: &rusqlite::Row) -> Result<Self> {
		let parent: Option<Uuid> = row.get(ITEMS_PARENT)?;
		let trashed: bool = row.get(ITEMS_TRASHED)?;
		Ok(Self {
			id: row.get(ITEMS_ID)?,
			uuid: row.get(ITEMS_UUID)?,
			parent: combine_parent(parent, trashed),
			local_data: row.get(ITEMS_LOCAL_DATA).unwrap(),
			change_seq: row.get(ITEMS_CHANGE_SEQ)?,
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
			change_seq: raw.change_seq,
		}
	}
}

pub(crate) trait DBItemTrait: Sync + Send {
	fn uuid(&self) -> Uuid;
	fn parent(&self) -> Option<ParentUuid>;
	fn name(&self) -> Option<&str>;
}
