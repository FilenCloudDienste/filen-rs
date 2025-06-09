use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use uuid::Uuid;

pub mod types;
pub use types::*;
pub mod error;
pub use error::SQLError;

use crate::PathIteratorExt;

/// Selects object in a path starting from the root UUID.
///
/// Returns a tuple containing a vector of objects, their corresponding position in the path,
/// and a boolean indicating if the path was fully traversed.
#[allow(clippy::type_complexity)]
pub(crate) fn select_objects_in_path<'a>(
	conn: &Connection,
	root_uuid: Uuid,
	path: &'a str,
) -> Result<(Vec<(DBObject, &'a str)>, bool)> {
	let path_iter = path.path_iter();
	let mut stmt = conn.prepare_cached(
		"SELECT id, uuid, parent, name, type FROM items WHERE parent = ? AND name = ? LIMIT 1;",
	)?;
	let mut objects = Vec::new();

	match RawDBItem::select(conn, root_uuid)? {
		Some(item) => {
			objects.push((item.into_db_object(conn)?, path));
		}
		None => return Ok((objects, false)),
	}
	for (component, remaining) in path_iter {
		let item: Option<RawDBItem> = stmt
			// SAFETY: We know that the last item in `items` is always present because we start with the root item.
			.query_one(
				(objects.last().unwrap().0.uuid(), component),
				RawDBItem::from_row,
			)
			.optional()
			.context("select item in path")?;
		match item {
			Some(item) => {
				objects.push((item.into_db_object(conn)?, remaining));
			}
			None => return Ok((objects, false)),
		}
	}
	Ok((objects, true))
}

pub(crate) fn select_object_at_path(
	conn: &Connection,
	root_uuid: Uuid,
	path: &str,
) -> Result<Option<DBObject>> {
	match select_objects_in_path(conn, root_uuid, path)? {
		(mut objects, true) => {
			// SAFETY: We know that the last item in `objects` is always present because we start with the root item.
			let (obj, _) = objects.pop().unwrap();
			Ok(Some(obj))
		}
		(_, false) => Ok(None),
	}
}

pub(crate) fn insert_root(conn: &mut Connection, root: Uuid) -> Result<()> {
	let tx: rusqlite::Transaction<'_> = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(
			"INSERT INTO items (uuid, parent, name, type) VALUES (?, NULL, ?, ?) RETURNING id;",
		)?;
		let id: i64 = stmt.query_one((root, "", ItemType::Root as i8), |row| row.get(0))?;
		let mut stmt = tx.prepare_cached("INSERT INTO roots (id) VALUES (?);")?;
		stmt.execute([id])?;
		let mut stmt = tx.prepare_cached("INSERT INTO dirs (id) VALUES (?);")?;
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
