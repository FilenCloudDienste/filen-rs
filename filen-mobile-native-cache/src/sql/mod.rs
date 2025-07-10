use filen_sdk_rs::{
	fs::{HasUUID, dir::RemoteDirectory, file::RemoteFile},
	util::PathIteratorExt,
};
use filen_types::fs::{ParentUuid, UuidStr};
use libsqlite3_sys::SQLITE_CONSTRAINT_UNIQUE;
use log::{debug, trace};
use rusqlite::{Connection, OptionalExtension};

pub mod types;
pub use types::*;
pub mod error;
pub use error::SQLError;

use crate::{
	CacheError,
	ffi::{ParsedFfiId, PathFfiId},
	sql::json_object::JsonObject,
};

/// Selects object in a path starting from the root UUID.
///
/// Returns a tuple containing a vector of objects, their corresponding position in the path,
/// and a boolean indicating if the path was fully traversed.
#[allow(clippy::type_complexity)]
pub(crate) fn select_objects_in_path<'a>(
	conn: &Connection,
	path_values: &'a PathFfiId,
) -> Result<(Vec<(DBObject, &'a str)>, bool), rusqlite::Error> {
	let path_iter = path_values.inner_path.path_iter();
	let mut stmt = conn.prepare_cached(include_str!("../../sql/select_item_by_parent_name.sql"))?;
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
	path_values: &PathFfiId,
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

pub(crate) fn select_object_at_parsed_id<'a>(
	conn: &Connection,
	parsed_id: &ParsedFfiId<'a>,
) -> Result<Option<DBObject>, CacheError> {
	match parsed_id {
		ParsedFfiId::Trash(uuid_id) | ParsedFfiId::Recents(uuid_id) => Ok(DBObject::select(
			conn,
			uuid_id.uuid.ok_or_else(|| {
				CacheError::DoesNotExist(
					format!("cannot select object at path: {}", uuid_id.full_path).into(),
				)
			})?,
		)
		.optional()?),
		ParsedFfiId::Path(path_values) => Ok(select_object_at_path(conn, path_values)?),
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

pub(crate) fn update_local_data(
	conn: &mut Connection,
	uuid: UuidStr,
	local_data: Option<&JsonObject>,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached("UPDATE items SET local_data = ? WHERE uuid = ?;")?;
	let local_data = local_data
		.map(|d| if d.is_empty() { None } else { Some(d) })
		.unwrap_or(None);
	stmt.execute((local_data, uuid))?;
	Ok(())
}

pub(crate) fn update_recents(
	conn: &mut Connection,
	dirs: Vec<RemoteDirectory>,
	files: Vec<RemoteFile>,
) -> Result<(), rusqlite::Error> {
	let tx = conn.transaction()?;
	{
		debug!("Clearing recents");
		let mut stmt = tx.prepare_cached(include_str!("../../sql/clear_recents.sql"))?;
		stmt.execute([])?;

		let mut upsert_item_stmt = tx.prepare_cached(types::UPSERT_ITEM_SQL)?;
		let mut upsert_dir = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;
		let mut upsert_file = tx.prepare_cached(include_str!("../../sql/upsert_file.sql"))?;
		let mut update_recent =
			tx.prepare_cached(include_str!("../../sql/update_item_set_recent.sql"))?;

		for dir in dirs {
			trace!("Updating recent directory: {}", dir.uuid());
			let dir = DBDir::upsert_from_remote_stmts(dir, &mut upsert_item_stmt, &mut upsert_dir)?;
			trace!("Updating recent directory: {}", dir.id);
			update_recent.execute([dir.id])?;
		}

		for file in files {
			trace!("Updating recent file: {}", file.uuid());
			let file: DBFile =
				DBFile::upsert_from_remote_stmts(file, &mut upsert_item_stmt, &mut upsert_file)?;
			trace!("Updating recent file: {}", file.id);
			update_recent.execute([file.id])?;
		}
	}
	tx.commit()?;
	Ok(())
}

pub(crate) fn update_items_with_parent<I, I1>(
	conn: &mut Connection,
	dirs: I,
	files: I1,
	parent: ParentUuid,
) -> Result<(), rusqlite::Error>
where
	I: IntoIterator<Item = RemoteDirectory>,
	I1: IntoIterator<Item = RemoteFile>,
{
	let tx = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(include_str!("../../sql/mark_stale_with_parent.sql"))?;
		stmt.execute([parent])?;

		let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM_SQL)?;
		let mut upsert_dir = tx.prepare_cached(include_str!("../../sql/upsert_dir.sql"))?;

		for dir in dirs {
			DBDir::upsert_from_remote_stmts(dir, &mut upsert_item_stmt, &mut upsert_dir)?;
		}

		let mut upsert_file = tx.prepare_cached(include_str!("../../sql/upsert_file.sql"))?;

		for file in files {
			DBFile::upsert_from_remote_stmts(file, &mut upsert_item_stmt, &mut upsert_file)?;
		}

		let mut stmt = tx.prepare_cached(include_str!("../../sql/delete_stale_with_parent.sql"))?;
		stmt.execute([parent])?;
	}
	tx.commit()?;
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

pub(crate) fn select_children(
	conn: &Connection,
	order_by: Option<&str>,
	parent: ParentUuid,
) -> SQLResult<Vec<DBNonRootObject>> {
	let order_by = match order_by {
		Some(order_by) => convert_order_by(order_by),
		_ => "ORDER BY items.name ASC",
	};

	let mut stmt = conn.prepare(&format!(
		"{} {}",
		include_str!("../../sql/select_dir_children.sql"),
		order_by
	))?;
	stmt.query_and_then([parent], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

pub(crate) fn select_recents(
	conn: &Connection,
	order_by: Option<&str>,
) -> SQLResult<Vec<DBNonRootObject>> {
	let order_by = match order_by {
		Some(order_by) => convert_order_by(order_by),
		_ => "ORDER BY items.name ASC",
	};

	let mut stmt = conn.prepare(&format!(
		"{} {};",
		include_str!("../../sql/select_recents.sql"),
		order_by
	))?;
	stmt.query_and_then([], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}
