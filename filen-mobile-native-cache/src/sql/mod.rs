use std::str::FromStr;

use anyhow::{Context, Result};
use filen_sdk_rs::{
	crypto::file::FileKey,
	fs::{
		FSObject1, HasName, HasParent, HasRemoteInfo, HasUUID,
		dir::{RemoteDirectory, RootDirectory, traits::HasRemoteDirInfo},
		file::{
			FlatRemoteFile, RemoteFile,
			traits::{HasFileInfo, HasRemoteFileInfo},
		},
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

struct Item {
	id: i64,
	uuid: Uuid,
	parent: Uuid,
	name: String,
	type_: ItemType,
}

fn select_item(conn: &Connection, uuid: Uuid) -> Result<Option<Item>> {
	let item = conn
		.query_one(
			"SELECT id, uuid, parent, name, type FROM items WHERE uuid = ? LIMIT 1;",
			[uuid],
			|row| {
				Ok(Item {
					id: row.get(0)?,
					uuid,
					parent: row.get(2)?,
					name: row.get(3)?,
					type_: row.get(4)?,
				})
			},
		)
		.optional()?;
	Ok(item)
}

pub fn select_object(conn: &Connection, uuid: Uuid) -> Result<Option<FfiObject>> {
	let maybe_item = conn
		.query_one(
			include_str!("../../sql/select_object.sql"),
			[uuid],
			FfiObject::from_row,
		)
		.optional()?;
	Ok(maybe_item)
}

pub(crate) fn select_remote_obj(conn: &Connection, uuid: Uuid) -> Result<Option<FSObject1>> {
	let item = match select_item(conn, uuid).context("select item from uuid")? {
		Some(item) => item,
		None => return Ok(None),
	};

	match item.type_ {
		ItemType::Root => Ok(Some(FSObject1::Root(RootDirectory::new(item.uuid)))),
		ItemType::Dir => {
			let dir = conn.query_one(
				"SELECT created, favorited, color FROM dirs WHERE id = ? LIMIT 1;",
				[item.id],
				|row| {
					Ok(RemoteDirectory {
						uuid: item.uuid,
						name: item.name,
						parent: item.parent,
						created: row.get::<usize, Option<i64>>(0)?.map(|t| {
							chrono::DateTime::<chrono::Utc>::from_timestamp_millis(t)
								.unwrap_or_default()
						}),
						favorited: row.get(1)?,
						color: row.get(2)?,
					})
				},
			)?;
			Ok(Some(FSObject1::Dir(dir)))
		}
		ItemType::File => {
			// I hate this
			let file = conn.query_one(
				"SELECT mime, key, created, modified, size, chunks, favorited, region, bucket, hash FROM files WHERE id = ? LIMIT 1;",
				[item.id] ,
				|row| {
					Ok(FlatRemoteFile {
						uuid: item.uuid,
						parent: item.parent,
						name: item.name,
						mime: row.get(0)?,
						key: FileKey::from_str(row.get::<usize, String>(1)?.as_str())
							.map_err(|e| {
								rusqlite::Error::FromSqlConversionFailure(
									1,
									rusqlite::types::Type::Text,
									Box::new(e),
								)
							})?,
						created: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
							row.get::<usize, i64>(2)?,
						)
						.unwrap_or_default(),
						modified: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
							row.get::<usize, i64>(3)?,
						)
						.unwrap_or_default(),
						size: row.get::<usize, i64>(4)? as u64,
						chunks: row.get::<usize, i64>(5)? as u64,
						favorited: row.get(6)?,
						region: row.get(8)?,
						bucket: row.get(9)?,
						hash: row.get::<usize, Option<[u8; 64]>>(10)?.map(Into::into),
					}.into())
				}
			)?;
			Ok(Some(FSObject1::File(file)))
		}
	}
}

pub(crate) fn select_remote_file(conn: &Connection, uuid: Uuid) -> Result<Option<RemoteFile>> {
	let obj = select_remote_obj(conn, uuid).context("select remote obj")?;
	match obj {
		None => Ok(None),
		Some(FSObject1::File(file)) => Ok(Some(file)),
		Some(_) => Err(anyhow::anyhow!(
			"Expected remote object to be a file, but got {:?}",
			obj
		)),
	}
}

pub(crate) fn select_ffi_root(conn: &Connection, root_uuid: Uuid) -> Result<Option<FfiRoot>> {
	let obj = select_object(conn, root_uuid)?;
	match obj {
		None => Ok(None),
		Some(FfiObject::Root(root)) => Ok(Some(root)),
		_ => Err(anyhow::anyhow!(
			"Expected object to be a root, but got {:?}",
			obj
		)),
	}
}

pub(crate) fn select_ffi_dir(conn: &Connection, dir_uuid: Uuid) -> Result<Option<FfiDir>> {
	let obj = select_object(conn, dir_uuid)?;
	match obj {
		None => Ok(None),
		Some(FfiObject::Dir(dir)) => Ok(Some(dir)),
		_ => Err(anyhow::anyhow!(
			"Expected object to be a directory, but got {:?}",
			obj
		)),
	}
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

	let parent = select_ffi_dir(conn, parent_uuid).context("get_dir_item")?;
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

pub(crate) fn update_file(conn: &mut Connection, old_uuid: Uuid, file: &RemoteFile) -> Result<()> {
	let tx = conn.transaction()?;
	let id = tx.query_one(
		"UPDATE items SET uuid = ?, parent = ?, name = ? WHERE uuid = ? RETURNING id LIMIT 1;",
		(file.uuid(), file.parent(), file.name(), old_uuid),
		|row| {
			let id: i64 = row.get(0)?;
			Ok(id)
		},
	)?;

	tx.execute("UPDATE files SET mime = ?, file_key = ?, created = ?, modified = ?, size = ?, chunks = ?, favorited = ?, region = ?, bucket = ?, hash = ? WHERE id = ?", (
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
		id,
	))?;
	tx.commit()?;
	Ok(())
}
