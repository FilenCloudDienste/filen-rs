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

use crate::ffi::{FfiDir, FfiFile, FfiRoot};

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
			let created: Option<i64> = row.get(0)?;
			let favorited: bool = row.get(1)?;
			let color: Option<String> = row.get(2)?;
			let last_listed: i64 = row.get(3)?;
			Ok(FfiDir {
				uuid: dir_uuid.to_string(),
				name,
				parent: parent.to_string(),
				color,
				created,
				favorited,
				last_listed,
			})
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

#[allow(clippy::type_complexity)]
pub(crate) fn select_dir_children(
	conn: &Connection,
	parent_uuid: Uuid,
) -> Result<Option<(FfiDir, Vec<FfiDir>, Vec<FfiFile>)>> {
	let parent = get_dir_item(conn, parent_uuid).context("get_dir_item")?;
	let parent = match parent {
		Some(parent) => parent,
		None => return Ok(None),
	};

	let mut stmt = conn
		.prepare(include_str!("../../sql/select_dir_dir_children.sql"))
		.context("select_dir_dir_children")?;
	let dirs = stmt
		.query_and_then([parent_uuid], |row| {
			let uuid: Uuid = row.get(0)?;
			let name: String = row.get(1)?;
			let created: Option<i64> = row.get(2)?;
			let favorited: bool = row.get(3)?;
			let color: Option<String> = row.get(4)?;
			let last_listed: i64 = row.get(5)?;
			Ok(FfiDir {
				uuid: uuid.to_string(),
				name,
				parent: parent.uuid.clone(),
				color,
				created,
				favorited,
				last_listed,
			})
		})?
		.collect::<Result<Vec<_>>>()
		.context("query select_dir_dir_children")?;

	let mut stmt = conn
		.prepare(include_str!("../../sql/select_dir_file_children.sql"))
		.context("select_dir_file_children")?;
	let files = stmt
		.query_and_then([parent_uuid], |row| {
			let uuid: Uuid = row.get(0)?;
			let name: String = row.get(1)?;
			let mime: String = row.get(2)?;
			let created: i64 = row.get(3)?;
			let modified: i64 = row.get(4)?;
			let size: i64 = row.get(5)?;
			let chunks: i64 = row.get(6)?;
			let favorited: bool = row.get(7)?;

			Ok(FfiFile {
				uuid: uuid.to_string(),
				name,
				parent: parent.uuid.clone(),
				mime,
				created,
				modified,
				size,
				chunks,
				favorited,
			})
		})?
		.collect::<Result<Vec<_>>>()
		.context("query select_dir_file_children")?;

	Ok(Some((parent, dirs, files)))
}

pub fn upsert_items(
	conn: &mut Connection,
	dirs: &[RemoteDirectory],
	files: &[RemoteFile],
) -> Result<()> {
	let tx = conn.transaction()?;
	{
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
