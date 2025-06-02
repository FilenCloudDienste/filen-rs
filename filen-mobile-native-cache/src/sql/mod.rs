use anyhow::{Context, Result};
use filen_sdk_rs::fs::{
	HasName, HasParent, HasRemoteInfo, HasUUID,
	dir::{RemoteDirectory, traits::HasRemoteDirInfo},
	file::{
		RemoteFile,
		traits::{HasFileInfo, HasRemoteFileInfo},
	},
};
use rusqlite::{
	Connection, OptionalExtension, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use uuid::Uuid;

use crate::ffi::{FfiDir, FfiNonRootObject, FfiObject, FfiRoot};

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
	fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
		let i8_value: i8 = match self {
			ItemType::Root => 0,
			ItemType::Dir => 1,
			ItemType::File => 2,
		};
		Ok(rusqlite::types::ToSqlOutput::from(i8_value))
	}
}

pub(crate) fn select_root_item(
	conn: &Connection,
	root_uuid_string: String,
) -> Result<Option<FfiRoot>> {
	let uuid = Uuid::parse_str(&root_uuid_string)?;
	let mut stmt = conn.prepare("SELECT id FROM items WHERE uuid = ? LIMIT 1;")?;
	let id: i64 = match stmt.query_one([uuid], |row| row.get(0)).optional()? {
		Some(id) => id,
		None => return Ok(None),
	};

	let mut stmt = conn.prepare(
		"SELECT storage_used, max_storage, last_updated FROM roots WHERE id = ? LIMIT 1;",
	)?;
	let (storage_used, max_storage, last_updated) =
		stmt.query_one([id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;

	let mut stmt = conn.prepare("SELECT last_listed FROM dirs WHERE id = ? LIMIT 1;")?;
	let last_listed: i64 = stmt.query_one([id], |row| row.get(0))?;

	Ok(Some(FfiRoot {
		uuid: root_uuid_string,
		storage_used,
		max_storage,
		last_updated,
		last_listed,
	}))
}

pub(crate) fn insert_root(conn: &mut Connection, root: Uuid) -> Result<()> {
	let tx: rusqlite::Transaction<'_> = conn.transaction()?;
	{
		let mut stmt = tx.prepare(
			"INSERT INTO items (uuid, parent, name, type) VALUES (?, ?, ?, ?) RETURNING id;",
		)?;
		let id: i64 = stmt.query_one((root, Uuid::nil(), "", ItemType::Root as i8), |row| {
			row.get(0)
		})?;
		let mut stmt = tx.prepare("INSERT INTO roots (id) VALUES (?);")?;
		stmt.execute([id])?;
		let mut stmt = tx.prepare("INSERT INTO dirs (id) VALUES (?);")?;
		stmt.execute([id])?;
	}
	tx.commit()?;
	Ok(())
}

pub(crate) fn get_dir_item(conn: &Connection, dir_uuid: Uuid) -> Result<Option<FfiDir>> {
	let mut stmt = conn
		.prepare("SELECT id, parent, name, type FROM items WHERE uuid = ? LIMIT 1;")
		.context("prepare select item from dir_uuid")?;

	let maybe_dir = stmt
		.query_one((dir_uuid,), |row| {
			let id: i64 = row.get(0)?;
			let parent: Uuid = row.get(1)?;
			let name: String = row.get(2)?;
			let type_: ItemType = row.get(3)?;
			Ok((id, parent, name, type_))
		})
		.optional()
		.context("select item from dir_uuid")?;

	let (id, parent, name, type_) = match maybe_dir {
		None => return Ok(None),
		Some(tuple) => tuple,
	};

	if type_ != ItemType::Dir && type_ != ItemType::Root {
		return Err(anyhow::anyhow!(
			"Expected item type to be Dir or Root, but got {:?}",
			type_
		));
	}

	let mut stmt = conn
		.prepare("SELECT created, favorited, color, last_listed FROM dirs WHERE id = ? LIMIT 1;")?;
	let dir = stmt
		.query_one((id,), |row| {
			FfiDir::from_row(row, 0, dir_uuid.to_string(), parent.to_string(), name)
		})
		.context("select dir from dir_uuid")?;

	Ok(Some(dir))
}

pub(crate) fn update_root(
	conn: &Connection,
	root_uuid: Uuid,
	response: &filen_types::api::v3::user::info::Response<'_>,
) -> Result<()> {
	let id: i64 = conn.query_one("SELECT id FROM items WHERE uuid = ?;", [root_uuid], |row| {
		row.get(0)
	})?;
	let mut stmt = conn.prepare(
		"UPDATE roots SET storage_used = ?, max_storage = ?, last_updated = ? WHERE id = ?;",
	)?;
	let now = chrono::Utc::now().timestamp_millis();
	stmt.execute((response.storage_used, response.max_storage, now, id))?;
	Ok(())
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

#[allow(clippy::type_complexity)]
pub(crate) fn select_dir_children(
	conn: &Connection,
	parent_uuid: Uuid,
	order_by: Option<&str>,
) -> Result<Option<(FfiDir, Vec<FfiNonRootObject>)>> {
	let order_by = match order_by {
		Some(order_by) => convert_order_by(order_by),
		_ => "ORDER BY items.name ASC",
	};

	let parent = get_dir_item(conn, parent_uuid).context("get_dir_item")?;
	let parent = match parent {
		Some(parent) => parent,
		None => return Ok(None),
	};

	let select_query = format!(
		"{} {}",
		include_str!("../../sql/select_dir_children.sql"),
		order_by
	);

	let mut stmt = conn.prepare(&select_query).context("select_dir_children")?;

	let objects = stmt
		.query_and_then([parent_uuid], FfiNonRootObject::from_row)?
		.collect::<rusqlite::Result<Vec<_>>>()
		.context("query select_dir_children")?;

	Ok(Some((parent, objects)))
}

pub fn update_children(
	conn: &mut Connection,
	parent_uuid: Uuid,
	dirs: &[RemoteDirectory],
	files: &[RemoteFile],
) -> Result<()> {
	let tx = conn.transaction()?;
	{
		tx.execute(
			include_str!("../../sql/mark_stale_with_parent.sql"),
			[parent_uuid],
		)?;

		let mut stmt = tx.prepare(include_str!("../../sql/upsert_item.sql"))?;

		let dir_ids = dirs
			.iter()
			.map(|dir| -> anyhow::Result<i64> {
				Ok(stmt.query_one(
					(dir.uuid(), dir.parent(), dir.name(), ItemType::Dir),
					|row| {
						let id: i64 = row.get(0)?;
						Ok(id)
					},
				)?)
			})
			.collect::<Result<Vec<i64>>>()
			.context("dir upsert_item")?;

		let file_ids = files
			.iter()
			.map(|file| -> anyhow::Result<i64> {
				Ok(stmt.query_one(
					(file.uuid(), file.parent(), file.name(), ItemType::File),
					|row| {
						let id: i64 = row.get(0)?;
						Ok(id)
					},
				)?)
			})
			.collect::<Result<Vec<i64>>>()
			.context("file upsert_item")?;

		let mut stmt = tx.prepare(include_str!("../../sql/upsert_dir.sql"))?;
		dir_ids
			.into_iter()
			.zip(dirs.iter())
			.try_for_each(|(id, dir)| -> rusqlite::Result<()> {
				stmt.execute((
					id,
					dir.created().map(|t| t.timestamp_millis()),
					dir.favorited(),
					dir.color(),
					0,
				))?;
				Ok(())
			})
			.context("upsert_dir")?;

		let mut stmt = tx.prepare(include_str!("../../sql/upsert_file.sql"))?;

		file_ids
			.into_iter()
			.zip(files.iter())
			.try_for_each(|(id, file)| -> rusqlite::Result<()> {
				stmt.execute((
					id,
					file.mime(),
					file.key().to_string(),
					file.created().timestamp_millis(),
					file.last_modified().timestamp_millis(),
					file.size() as i64,
					file.chunks() as i64,
					file.favorited(),
					file.region(),
					file.bucket(),
					file.hash().map(Into::<[u8; 64]>::into),
				))?;
				Ok(())
			})
			.context("upsert_file")?;

		tx.execute(
			include_str!("../../sql/delete_stale_with_parent.sql"),
			[parent_uuid],
		)?;
	}
	tx.commit()?;
	Ok(())
}

pub fn upsert_dir_last_listed(conn: &mut Connection, dir: &RemoteDirectory) -> Result<()> {
	let tx = conn.transaction()?;
	let id = tx
		.query_one("SELECT id FROM items WHERE uuid = ?", [dir.uuid()], |row| {
			let id: i64 = row.get(0)?;
			Ok(id)
		})
		.optional()?;

	let id = match id {
		Some(id) => id,
		None => tx.query_one(
			include_str!("../../sql/insert_item.sql"),
			(dir.uuid(), dir.parent(), dir.name(), ItemType::Dir),
			|row| {
				let id: i64 = row.get(0)?;
				Ok(id)
			},
		)?,
	};

	tx.execute(
		include_str!("../../sql/upsert_dir.sql"),
		(
			id,
			dir.created().map(|t| t.timestamp_millis()),
			dir.favorited(),
			dir.color(),
			chrono::Utc::now().timestamp_millis(),
		),
	)?;
	tx.commit()?;
	Ok(())
}

pub fn select_item(conn: &Connection, uuid: Uuid) -> Result<Option<FfiObject>> {
	let mut stmt = conn.prepare(include_str!("../../sql/select_item.sql"))?;
	let maybe_item = stmt.query_one([uuid], FfiObject::from_row).optional()?;
	Ok(maybe_item)
}
