use std::fmt::Debug;

use filen_sdk_rs::fs::{UnsharedFSObject, dir::RemoteDirectory};
use filen_types::fs::{ParentUuid, UuidStr};
use log::trace;
use rusqlite::{Connection, Result};

use crate::{
	ffi::ItemType,
	sql::{
		SQLResult,
		dir::{DBDir, DBRoot},
		file::DBFile,
		item::{DBItemTrait, InnerDBItem, RawDBItem},
	},
};

use super::SQLError;
use super::statements::*;

pub(crate) use json_object::JsonObject;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DBObject {
	File(DBFile),
	Dir(DBDir),
	Root(DBRoot),
}

impl DBObject {
	pub(crate) fn select(conn: &Connection, uuid: UuidStr) -> Result<Self> {
		let mut stmt = conn.prepare_cached(SELECT_OBJECT_BY_UUID)?;
		stmt.query_one([uuid], |row| {
			let item = RawDBItem::from_row(row)?;
			Ok(match item.type_ {
				ItemType::Dir => Self::Dir(DBDir::from_inner_and_row(
					item.into(),
					row,
					ITEM_COLUMN_COUNT_NO_EXTRA,
				)?),
				ItemType::File => Self::File(DBFile::from_inner_and_row(
					item.into(),
					row,
					ITEM_COLUMN_COUNT_NO_EXTRA + DIRS_COLUMN_COUNT + DIRS_META_COLUMN_COUNT,
				)?),
				ItemType::Root => {
					Self::Root(DBRoot::from_inner_and_row(
						item.into(),
						row,
						ITEM_COLUMN_COUNT_NO_EXTRA
							+ DIRS_COLUMN_COUNT + DIRS_META_COLUMN_COUNT
							+ FILES_COLUMN_COUNT + FILES_META_COLUMN_COUNT,
					)?)
				}
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

	fn name(&self) -> Option<&str> {
		match self {
			DBObject::File(file) => file.name(),
			DBObject::Dir(dir) => dir.name(),
			// Root has no name, but this is different than having a not decrypted name
			DBObject::Root(_) => Some(""),
		}
	}

	fn item_type(&self) -> ItemType {
		self.item_type()
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
		let type_: ItemType = row.get(ITEM_COLUMN_COUNT_NO_EXTRA - 1)?;
		trace!("Creating DBNonRootObject from row, item: {item:?}");
		let obj = match type_ {
			ItemType::Dir => DBNonRootObject::Dir(DBDir::from_inner_and_row(
				item,
				row,
				ITEM_COLUMN_COUNT_NO_EXTRA,
			)?),
			ItemType::File => DBNonRootObject::File(DBFile::from_inner_and_row(
				item,
				row,
				ITEM_COLUMN_COUNT_NO_EXTRA + DIRS_COLUMN_COUNT + DIRS_META_COLUMN_COUNT,
			)?),
			_ => return Err(SQLError::UnexpectedType(type_, ItemType::Dir)),
		};
		trace!("Created DBNonRootObject: {obj:?}");
		Ok(obj)
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

	fn name(&self) -> Option<&str> {
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
