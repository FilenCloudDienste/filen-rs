use filen_sdk_rs::{
	fs::{HasUUID, NonRootFSObject, dir::RemoteDirectory, file::RemoteFile},
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
pub(crate) mod statements;
use statements::*;

use crate::{
	CacheError,
	ffi::{ItemType, ParsedFfiId, PathFfiId, SearchQueryArgs},
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
	let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_PARENT_NAME)?;
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
		let mut stmt = tx.prepare_cached(INSERT_ROOT_INTO_ITEMS)?;
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
		let mut stmt = tx.prepare_cached(INSERT_ROOT_INTO_ROOTS)?;
		stmt.execute([id])?;
		let mut stmt = tx.prepare_cached(INSERT_ROOT_INTO_DIRS)?;
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
	let id: i64 = conn.query_one(SELECT_ID_BY_UUID, [root_uuid], |row| row.get(0))?;
	let mut stmt = conn.prepare(UPDATE_ROOT)?;
	let now = chrono::Utc::now().timestamp_millis();
	stmt.execute((response.storage_used, response.max_storage, now, id))?;
	Ok(())
}

pub(crate) fn delete_item(conn: &Connection, item_uuid: UuidStr) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(DELETE_BY_UUID)?;
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
	let mut stmt = conn.prepare_cached(SELECT_UUID_NAME_TYPE_BY_PARENT)?;
	let mut paths = Vec::new();
	get_all_descendant_paths_with_stmt(uuid, current_path, &mut stmt, &mut paths)?;
	Ok(paths)
}

pub(crate) fn recursive_select_path_from_uuid(
	conn: &Connection,
	uuid: UuidStr,
) -> Result<Option<String>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(RECURSIVE_SELECT_PATH_FROM_UUID)?;
	stmt.query_row([uuid], |row| row.get(0)).optional()
}

pub(crate) fn update_local_data(
	conn: &mut Connection,
	uuid: UuidStr,
	local_data: Option<&JsonObject>,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(UPDATE_LOCAL_DATA_BY_UUID)?;
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
		let mut stmt = tx.prepare_cached(CLEAR_RECENTS)?;
		stmt.execute([])?;

		let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
		let mut upsert_dir = tx.prepare_cached(UPSERT_DIR)?;
		let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
		let mut update_recent = tx.prepare_cached(UPDATE_ITEM_SET_RECENT)?;

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

pub(crate) fn update_search_items<'a, I>(
	conn: &'a mut Connection,
	items: I,
) -> Result<Vec<DBNonRootObject>, rusqlite::Error>
where
	I: IntoIterator<Item = (NonRootFSObject<'a>, String)>,
{
	let tx = conn.transaction()?;
	let items = {
		// This should remove any orphaned items that were previously around because they were searched for
		let mut clear_search = tx.prepare_cached(CLEAR_ORPHANED_SEARCH_ITEMS)?;
		clear_search.execute([])?;

		// This should removed the search path from all items that were previously searched for
		let mut clear_search = tx.prepare_cached(CLEAR_SEARCH_FROM_ITEMS)?;
		clear_search.execute([])?;

		let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
		let mut upsert_dir = tx.prepare_cached(UPSERT_DIR)?;
		let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
		let mut update_search_path = tx.prepare_cached(UPDATE_SEARCH_PATH)?;

		items
			.into_iter()
			.map(|(item, path)| match item {
				NonRootFSObject::Dir(cow) => {
					let dir = DBDir::upsert_from_remote_stmts(
						cow.into_owned(),
						&mut upsert_item_stmt,
						&mut upsert_dir,
					)?;
					update_search_path.execute((path, dir.id))?;
					Ok(DBNonRootObject::Dir(dir))
				}
				NonRootFSObject::File(cow) => {
					let file = DBFile::upsert_from_remote_stmts(
						cow.into_owned(),
						&mut upsert_item_stmt,
						&mut upsert_file,
					)?;
					update_search_path.execute((path, file.id))?;
					Ok(DBNonRootObject::File(file))
				}
			})
			.collect::<Result<Vec<_>, rusqlite::Error>>()?
	};
	tx.commit()?;
	Ok(items)
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
		let mut stmt = tx.prepare_cached(MARK_STALE_WITH_PARENT)?;
		stmt.execute([parent])?;

		let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
		let mut upsert_dir = tx.prepare_cached(UPSERT_DIR)?;

		for dir in dirs {
			DBDir::upsert_from_remote_stmts(dir, &mut upsert_item_stmt, &mut upsert_dir)?;
		}

		let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;

		for file in files {
			DBFile::upsert_from_remote_stmts(file, &mut upsert_item_stmt, &mut upsert_file)?;
		}

		let mut stmt = tx.prepare_cached(DELETE_STALE_WITH_PARENT)?;
		stmt.execute([parent])?;
	}
	tx.commit()?;
	Ok(())
}

pub(crate) fn select_children(
	conn: &Connection,
	order_by: Option<&str>,
	parent: ParentUuid,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&select_dir_children(order_by))?;
	stmt.query_and_then([parent], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

pub(crate) fn select_recents(
	conn: &Connection,
	order_by: Option<&str>,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&statements::select_recents(order_by))?;
	stmt.query_and_then([], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

pub(crate) fn select_search(
	conn: &Connection,
	args: &SearchQueryArgs,
	root: UuidStr,
) -> SQLResult<Vec<(DBNonRootObject, String)>> {
	let mut stmt = conn.prepare_cached(SELECT_SEARCH)?;

	let mime_json_array_string = if args.mime_types.is_empty() {
		None
	} else {
		let mime_json_string_capacity =
			args.mime_types.iter().fold(2 /* for [] */, |acc, mime| {
				acc + mime.len() + 3 // 3 for the surrounding quotes and commas
			}) - 1; // -1 for the last comma

		let mut mime_json_string = String::with_capacity(mime_json_string_capacity);
		mime_json_string.push('[');
		for (i, mime) in args.mime_types.iter().enumerate() {
			if i > 0 {
				mime_json_string.push(',');
			}
			mime_json_string.push('"');
			mime_json_string.push_str(mime);
			mime_json_string.push('"');
		}
		mime_json_string.push(']');
		// SAFETY: We are mutating the string to replace '*' with '%'
		// which is safe as this is just replacing a single valid byte with another valid byte.
		unsafe {
			let bytes = mime_json_string.as_bytes_mut();
			for byte in bytes.iter_mut() {
				if *byte == b'*' {
					*byte = b'%'; // Replace '*' with '%'
				}
			}
		}
		Some(mime_json_string)
	};

	stmt.query_and_then(
		(
			args.name.as_ref().map(|n| n.trim().to_lowercase()),
			mime_json_array_string,
			args.file_size_min,
			args.last_modified_min,
			args.item_type,
		),
		|r| {
			Ok((
				DBNonRootObject::from_row(r)?,
				format!("{}{}", root.as_ref(), r.get_ref(21)?.as_str()?),
			))
		},
	)?
	.collect::<SQLResult<Vec<_>>>()
}
