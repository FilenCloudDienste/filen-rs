use filen_types::fs::UuidStr;
use libsqlite3_sys::SQLITE_CONSTRAINT_UNIQUE;
use rusqlite::{Connection, OptionalExtension};

pub mod types;
pub use types::*;
pub mod error;
pub use error::SQLError;

use crate::{MaybeTrashValues, PathIteratorExt, PathValues};

/// Selects object in a path starting from the root UUID.
///
/// Returns a tuple containing a vector of objects, their corresponding position in the path,
/// and a boolean indicating if the path was fully traversed.
#[allow(clippy::type_complexity)]
pub(crate) fn select_objects_in_path<'a>(
	conn: &Connection,
	path_values: &'a PathValues,
) -> Result<(Vec<(DBObject, &'a str)>, bool), rusqlite::Error> {
	let path_iter = path_values.inner_path.path_iter();
	let mut stmt = conn.prepare_cached(
		"SELECT id, uuid, parent, name, type FROM items WHERE parent = ? AND name = ? LIMIT 1;",
	)?;
	let mut objects = Vec::new();

	match RawDBItem::select(conn, path_values.root_uuid)? {
		Some(item) => {
			objects.push((item.into_db_object(conn)?, path_values.inner_path));
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
			.optional()?;
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
	path_values: &PathValues,
) -> Result<Option<DBObject>, rusqlite::Error> {
	match select_objects_in_path(conn, path_values)? {
		(mut objects, true) => {
			// SAFETY: We know that the last item in `objects` is always present because we start with the root item.
			let (obj, _) = objects.pop().unwrap();
			Ok(Some(obj))
		}
		(_, false) => Ok(None),
	}
}

pub(crate) fn select_maybe_trashed_object_at_path<'a>(
	conn: &Connection,
	path_values: &MaybeTrashValues<'a>,
) -> Result<Option<DBObject>, rusqlite::Error> {
	match path_values {
		MaybeTrashValues::Trash(trash_values) => {
			DBObject::select(conn, trash_values.uuid).optional()
		}
		MaybeTrashValues::Path(path_values) => select_object_at_path(conn, path_values),
	}
}

pub(crate) fn insert_root(conn: &mut Connection, root: UuidStr) -> Result<(), rusqlite::Error> {
	let tx: rusqlite::Transaction<'_> = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(
			"INSERT INTO items (uuid, parent, name, type) VALUES (?, NULL, ?, ?) RETURNING id;",
		)?;
		let id: i64 = match stmt.query_one((root, "", ItemType::Root as i8), |row| row.get(0)) {
			Ok(id) => id,
			Err(rusqlite::Error::SqliteFailure(
				libsqlite3_sys::Error {
					code: libsqlite3_sys::ErrorCode::ConstraintViolation,
					extended_code: SQLITE_CONSTRAINT_UNIQUE,
				},
				_,
			)) => {
				// root was already initialized
				return Ok(());
			}
			Err(e) => return Err(e),
		};
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
	root_uuid: UuidStr,
	response: &filen_types::api::v3::user::info::Response<'_>,
) -> Result<(), rusqlite::Error> {
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

pub(crate) fn delete_item(conn: &Connection, item_uuid: UuidStr) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached("DELETE FROM items WHERE uuid = ?;")?;
	stmt.execute([item_uuid])?;
	Ok(())
}

fn get_all_descendant_paths_with_stmt(
	uuid: UuidStr,
	current_path: &str,
	stmt: &mut rusqlite::CachedStatement<'_>,
	paths: &mut Vec<String>,
) -> Result<(), rusqlite::Error> {
	let items = stmt
		.query_and_then([uuid], |f| -> Result<_, rusqlite::Error> {
			let uuid = f.get::<_, UuidStr>(0)?;
			let name = f.get::<_, String>(1)?;
			let item_type = f.get::<_, ItemType>(2)?;
			Ok((uuid, name, item_type))
		})?
		.collect::<Result<Vec<_>, rusqlite::Error>>()?;
	for (uuid, name, item_type) in items {
		let current_path = format!("{current_path}/{name}");
		if item_type == ItemType::Dir || item_type == ItemType::Root {
			get_all_descendant_paths_with_stmt(uuid, &current_path, stmt, paths)?;
		}
		paths.push(current_path);
	}
	Ok(())
}

pub(crate) fn get_all_descendant_paths(
	conn: &Connection,
	uuid: UuidStr,
	current_path: &str,
) -> Result<Vec<String>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached("SELECT uuid, name, type FROM items WHERE parent = ?;")?;
	let mut paths = Vec::new();
	get_all_descendant_paths_with_stmt(uuid, current_path, &mut stmt, &mut paths)?;
	Ok(paths)
}

pub(crate) fn recursive_select_path_from_uuid(
	conn: &Connection,
	uuid: UuidStr,
) -> Result<Option<String>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(include_str!(
		"../../sql/recursive_select_path_from_uuid.sql"
	))?;
	stmt.query_row([uuid], |row| row.get(0)).optional()
}
