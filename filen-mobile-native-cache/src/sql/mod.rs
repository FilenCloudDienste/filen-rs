use filen_sdk_rs::{
	fs::{
		HasUUID,
		dir::{RemoteDirectory, meta::DirectoryMeta},
		file::{RemoteFile, meta::FileMeta},
	},
	user::UserInfo,
	util::PathIteratorExt,
};
use filen_types::fs::{ParentUuid, Uuid, UuidStr};
use libsqlite3_sys::SQLITE_CONSTRAINT_UNIQUE;
use rusqlite::{
	Connection, OptionalExtension, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use tracing::{debug, trace};

pub mod error;
pub use error::SQLError;
pub mod changes;
pub mod dir;
pub mod file;
pub mod item;
pub mod object;
pub(crate) mod statements;
use statements::*;

use crate::{
	CacheError,
	ffi::{ItemType, ParsedFfiId, PathFfiId},
	sql::{
		dir::DBDir,
		file::DBFile,
		item::RawDBItem,
		object::{DBNonRootObject, DBObject, JsonObject},
	},
};

pub(crate) use dir::*;
pub(crate) use file::*;
pub(crate) use item::*;
pub(crate) use object::*;

pub(crate) type SQLResult<T> = std::result::Result<T, SQLError>;

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
		// Uuid-form ids are normally canonicalized before reaching here; resolving by the trailing
		// uuid keeps real-uuid ids working as a fallback.
		ParsedFfiId::Trash(uuid_id)
		| ParsedFfiId::Recents(uuid_id)
		| ParsedFfiId::Uuid(uuid_id) => Ok(DBObject::select(
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

pub(crate) fn insert_root(conn: &mut Connection, root: Uuid) -> Result<(), rusqlite::Error> {
	let tx: rusqlite::Transaction<'_> = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(INSERT_ROOT_INTO_ITEMS)?;
		let id: i64 = match stmt.query_one((root, ItemType::Root as i8), |row| row.get(0)) {
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
	root_uuid: Uuid,
	response: &UserInfo,
) -> Result<(), rusqlite::Error> {
	let id: i64 = conn.query_one(SELECT_ID_BY_UUID, [root_uuid], |row| row.get(0))?;
	let mut stmt = conn.prepare(UPDATE_ROOT)?;
	let now = chrono::Utc::now().timestamp_millis();
	stmt.execute((response.storage_used, response.max_storage, now, id))?;
	Ok(())
}

pub(crate) fn delete_item(conn: &Connection, item_uuid: Uuid) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(DELETE_BY_UUID)?;
	stmt.execute([item_uuid])?;
	Ok(())
}

fn get_all_descendant_paths_with_stmt(
	uuid: Uuid,
	current_path: &str,
	stmt: &mut rusqlite::CachedStatement<'_>,
	paths: &mut Vec<String>,
) -> Result<(), rusqlite::Error> {
	let items = stmt
		.query_and_then([uuid], |f| -> Result<_, rusqlite::Error> {
			let uuid = f.get::<_, Uuid>(0)?;
			let item_type = f.get::<_, ItemType>(1)?;
			let name_or_uuid = f.get::<_, String>(2)?;
			Ok((uuid, name_or_uuid, item_type))
		})?
		.collect::<Result<Vec<_>, rusqlite::Error>>()?;
	for (uuid, name_or_uuid, item_type) in items {
		let current_path = format!("{current_path}/{name_or_uuid}");
		if item_type == ItemType::Dir || item_type == ItemType::Root {
			get_all_descendant_paths_with_stmt(uuid, &current_path, stmt, paths)?;
		}
		paths.push(current_path);
	}
	Ok(())
}

pub(crate) fn get_all_descendant_paths(
	conn: &Connection,
	uuid: Uuid,
	current_path: &str,
) -> Result<Vec<String>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_UUID_TYPE_NAME_BY_PARENT)?;
	let mut paths = Vec::new();
	get_all_descendant_paths_with_stmt(uuid, current_path, &mut stmt, &mut paths)?;
	Ok(paths)
}

pub(crate) fn recursive_select_path_from_uuid(
	conn: &Connection,
	uuid: Uuid,
) -> Result<Option<String>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(RECURSIVE_SELECT_PATH_FROM_UUID)?;
	stmt.query_row([uuid], |row| row.get(0)).optional()
}

/// The container an item ultimately lives in, determined by climbing `items.parent`.
#[derive(Debug)]
pub(crate) enum ItemContainer {
	/// The ancestor chain terminates at the drive root — the item is addressed by its full path id.
	Root,
	/// The ancestor chain terminates in trash (at any nesting depth) — addressed by `trash/<uuid>`.
	Trash,
}

/// Walks `items.parent` upward from `uuid` to find the container it belongs to.
///
/// Unlike string-matching the first segment of a recursively-built path, this distinguishes a genuine
/// trash item from one whose ancestor chain is simply broken/uncached (normal for `update_recents`,
/// which inserts items whose parent rows were never cached): a parent reference with no matching
/// `items` row — or an over-long chain (cycle guard) — yields [`CacheError::DoesNotExist`] naming the
/// broken ancestor, rather than being misclassified as trash-parented.
pub(crate) fn classify_item_container(
	conn: &Connection,
	uuid: Uuid,
	root_uuid: Uuid,
) -> Result<ItemContainer, CacheError> {
	const MAX_ANCESTOR_DEPTH: usize = 256;
	if uuid == root_uuid {
		return Ok(ItemContainer::Root);
	}
	let mut current = uuid;
	for _ in 0..MAX_ANCESTOR_DEPTH {
		let item = RawDBItem::select(conn, current)?.ok_or_else(|| {
			CacheError::DoesNotExist(
				format!("broken ancestor chain: no cached item for {current}").into(),
			)
		})?;
		match item.parent {
			Some(ParentUuid::Trash(_)) => return Ok(ItemContainer::Trash),
			Some(ParentUuid::Uuid(parent)) if parent == root_uuid => {
				return Ok(ItemContainer::Root);
			}
			Some(ParentUuid::Uuid(parent)) => current = parent,
			// A NULL parent (non-root/non-trash row) or a recents/favorites/links sentinel has no
			// addressable drive/trash container.
			other => {
				return Err(CacheError::DoesNotExist(
					format!("item {current} has no drive/trash container (parent: {other:?})")
						.into(),
				));
			}
		}
	}
	Err(CacheError::DoesNotExist(
		format!("ancestor chain for {uuid} exceeds depth {MAX_ANCESTOR_DEPTH}").into(),
	))
}

/// Resolves a stable uuid to the current real uuid of the row it identifies. Returns `None` when the
/// argument is not any row's `stable_uuid` (i.e. it is already a real uuid, or is unknown).
pub(crate) fn select_uuid_by_stable_uuid(
	conn: &Connection,
	stable_uuid: Uuid,
) -> Result<Option<Uuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_UUID_BY_STABLE_UUID)?;
	stmt.query_row([stable_uuid], |row| row.get(0)).optional()
}

pub(crate) fn update_local_data(
	conn: &mut Connection,
	uuid: Uuid,
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
		let mut upset_dir_meta = tx.prepare_cached(UPSERT_DIR_META)?;
		let mut delete_dir_meta = tx.prepare_cached(DELETE_DIR_META)?;
		let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
		let mut update_recent = tx.prepare_cached(UPDATE_ITEM_SET_RECENT)?;
		let mut upsert_file_meta = tx.prepare_cached(UPSERT_FILE_META)?;
		let mut delete_file_meta = tx.prepare_cached(DELETE_FILE_META)?;

		for dir in dirs {
			trace!("Updating recent directory: {}", dir.uuid());
			let dir = DBDir::upsert_from_remote_stmts(
				dir,
				&mut upsert_item_stmt,
				&mut upsert_dir,
				&mut upset_dir_meta,
				&mut delete_dir_meta,
			)?;
			trace!("Updating recent directory: {}", dir.id);
			update_recent.execute([dir.id])?;
		}

		for file in files {
			trace!("Updating recent file: {}", file.uuid());
			let file: DBFile = DBFile::upsert_from_remote_stmts(
				file,
				&mut upsert_item_stmt,
				&mut upsert_file,
				&mut upsert_file_meta,
				&mut delete_file_meta,
			)?;
			trace!("Updating recent file: {}", file.id);
			update_recent.execute([file.id])?;
		}
	}
	tx.commit()?;
	Ok(())
}

/// Refreshes the cached children of a single directory: everything currently under `parent`
/// (excluding trashed items) is marked stale, the fresh listing is upserted, and whatever stayed
/// stale is deleted.
pub(crate) fn update_items_with_parent<I, I1>(
	conn: &mut Connection,
	dirs: I,
	files: I1,
	parent: Uuid,
) -> Result<(), rusqlite::Error>
where
	I: IntoIterator<Item = RemoteDirectory>,
	I1: IntoIterator<Item = RemoteFile>,
{
	let tx = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(MARK_STALE_WITH_PARENT)?;
		stmt.execute([parent])?;

		upsert_dirs_and_files(&tx, dirs, files)?;

		let mut stmt = tx.prepare_cached(DELETE_STALE_WITH_PARENT)?;
		stmt.execute([parent])?;
	}
	tx.commit()?;
	Ok(())
}

/// Refreshes the cached trash listing. Trashed items keep their original `parent`, so the sweep
/// is scoped by the `trashed` flag rather than by a parent uuid. Each item's own parent is
/// `ParentUuid::Trash`, which the upsert decomposes back into `(original parent, trashed = 1)`.
pub(crate) fn update_trashed_items<I, I1>(
	conn: &mut Connection,
	dirs: I,
	files: I1,
) -> Result<(), rusqlite::Error>
where
	I: IntoIterator<Item = RemoteDirectory>,
	I1: IntoIterator<Item = RemoteFile>,
{
	let tx = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(MARK_STALE_TRASHED)?;
		stmt.execute([])?;

		upsert_dirs_and_files(&tx, dirs, files)?;

		let mut stmt = tx.prepare_cached(DELETE_STALE_TRASHED)?;
		stmt.execute([])?;
	}
	tx.commit()?;
	Ok(())
}

fn upsert_dirs_and_files<I, I1>(
	tx: &rusqlite::Transaction<'_>,
	dirs: I,
	files: I1,
) -> Result<(), rusqlite::Error>
where
	I: IntoIterator<Item = RemoteDirectory>,
	I1: IntoIterator<Item = RemoteFile>,
{
	let mut upsert_item_stmt = tx.prepare_cached(UPSERT_ITEM)?;
	let mut upsert_dir = tx.prepare_cached(UPSERT_DIR)?;
	let mut upsert_dir_meta = tx.prepare_cached(UPSERT_DIR_META)?;
	let mut delete_dir_meta = tx.prepare_cached(DELETE_DIR_META)?;

	for dir in dirs {
		DBDir::upsert_from_remote_stmts(
			dir,
			&mut upsert_item_stmt,
			&mut upsert_dir,
			&mut upsert_dir_meta,
			&mut delete_dir_meta,
		)?;
	}

	let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
	let mut upsert_file_meta = tx.prepare_cached(UPSERT_FILE_META)?;
	let mut delete_file_meta = tx.prepare_cached(DELETE_FILE_META)?;

	for file in files {
		DBFile::upsert_from_remote_stmts(
			file,
			&mut upsert_item_stmt,
			&mut upsert_file,
			&mut upsert_file_meta,
			&mut delete_file_meta,
		)?;
	}
	Ok(())
}

pub(crate) fn select_children(
	conn: &Connection,
	order_by: Option<&str>,
	parent: Uuid,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&select_dir_children(order_by))?;
	stmt.query_and_then([parent], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// One page of `parent`'s children: rows `[offset, offset + limit)` in `order_by` order.
pub(crate) fn select_children_page(
	conn: &Connection,
	order_by: Option<&str>,
	parent: Uuid,
	limit: u32,
	offset: u32,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&select_dir_children_page(order_by))?;
	stmt.query_and_then((parent, limit, offset), DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Selects the cached trashed items (the trash listing).
pub(crate) fn select_trash(
	conn: &Connection,
	order_by: Option<&str>,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&statements::select_trash_children(order_by))?;
	stmt.query_and_then([], DBNonRootObject::from_row)?
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

/// Accepts an iterator over UUIDs
/// and returns a vector of positions (usize)
/// which correspond to the indices of the passed UUIDs
/// which are not in the database.
pub(crate) fn select_positions_not_in_uuids<I>(conn: &Connection, uuids: I) -> SQLResult<Vec<usize>>
where
	I: ExactSizeIterator<Item = UuidStr>,
{
	let mut stmt = conn.prepare_cached(SELECT_POS_NOT_IN_UUIDS)?;
	let mut uuids_json_string = String::with_capacity(
		uuids.len() * (UuidStr::LENGTH + 3) + if uuids.len() == 0 { 2 } else { 1 },
	); // 3 for the surrounding quotes and comma, 2 for the brackets - 1 for the last comma
	uuids_json_string.push('[');
	for (i, uuid) in uuids.enumerate() {
		if i > 0 {
			uuids_json_string.push(',');
		}
		uuids_json_string.push('"');
		uuids_json_string.push_str(uuid.as_ref());
		uuids_json_string.push('"');
	}
	uuids_json_string.push(']');
	stmt.query_and_then([uuids_json_string], |row| Ok(row.get(0)?))?
		.collect::<SQLResult<Vec<_>>>()
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetaState {
	Decoded,
	Decrypted,
	Encrypted,
	RSAEncrypted,
}

impl FromSql for MetaState {
	fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
		match value {
			ValueRef::Integer(i) => match i {
				0 => Ok(Self::Decoded),
				1 => Ok(Self::Decrypted),
				2 => Ok(Self::Encrypted),
				3 => Ok(Self::RSAEncrypted),
				_ => Err(FromSqlError::OutOfRange(i)),
			},
			_ => Err(FromSqlError::InvalidType),
		}
	}
}

impl ToSql for MetaState {
	fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput<'_>, rusqlite::Error> {
		Ok(rusqlite::types::ToSqlOutput::from(*self as u8))
	}
}

enum RawMeta<'a> {
	Decoded,
	Decrypted(&'a [u8]),
	Encrypted(&'a str),
	RSAEncrypted(&'a str),
}

fn raw_meta_and_state_from_dir_meta<'a>(dir_meta: &'a DirectoryMeta) -> (MetaState, RawMeta<'a>) {
	match dir_meta {
		DirectoryMeta::Decoded(_) => (MetaState::Decoded, RawMeta::Decoded),
		DirectoryMeta::DecryptedRaw(cow) => (MetaState::Decrypted, RawMeta::Decrypted(cow)),
		DirectoryMeta::DecryptedUTF8(cow) => {
			(MetaState::Decrypted, RawMeta::Decrypted(cow.as_bytes()))
		}
		DirectoryMeta::Encrypted(cow) => (MetaState::Encrypted, RawMeta::Encrypted(&cow.0)),
		DirectoryMeta::RSAEncrypted(cow) => {
			(MetaState::RSAEncrypted, RawMeta::RSAEncrypted(&cow.0))
		}
	}
}

fn raw_meta_and_state_from_file_meta<'a>(dir_meta: &'a FileMeta) -> (MetaState, RawMeta<'a>) {
	match dir_meta {
		FileMeta::Decoded(_) => (MetaState::Decoded, RawMeta::Decoded),
		FileMeta::DecryptedRaw(cow) => (MetaState::Decrypted, RawMeta::Decrypted(cow)),
		FileMeta::DecryptedUTF8(cow) => (MetaState::Decrypted, RawMeta::Decrypted(cow.as_bytes())),
		FileMeta::Encrypted(cow) => (MetaState::Encrypted, RawMeta::Encrypted(&cow.0)),
		FileMeta::RSAEncrypted(cow) => (MetaState::RSAEncrypted, RawMeta::RSAEncrypted(&cow.0)),
	}
}

impl ToSql for RawMeta<'_> {
	fn to_sql(&self) -> Result<rusqlite::types::ToSqlOutput<'_>, rusqlite::Error> {
		match self {
			RawMeta::Decoded => Ok(rusqlite::types::ToSqlOutput::Owned(
				rusqlite::types::Value::Null,
			)),
			RawMeta::Decrypted(bytes) => Ok(rusqlite::types::ToSqlOutput::Borrowed(
				ValueRef::Blob(bytes),
			)),
			RawMeta::Encrypted(s) => Ok(rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Text(
				s.as_bytes(),
			))),
			RawMeta::RSAEncrypted(s) => Ok(rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Text(
				s.as_bytes(),
			))),
		}
	}
}

#[cfg(test)]
mod stable_uuid_tests {
	use filen_types::fs::{ParentUuid, Uuid};
	use rusqlite::Connection;

	use crate::{ffi::ItemType, sql::item, sql::statements::INIT};

	fn setup() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	fn add_file_meta(conn: &Connection, id: i64, name: &str) {
		conn.execute(
			"INSERT INTO files (id, size, chunks, region, bucket, timestamp, metadata_state) VALUES (?1, 0, 0, 'r', 'b', 0, 0)",
			[id],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO files_meta (id, name, mime, file_key, file_key_version, modified) VALUES (?1, ?2, 'text/plain', 'k', 3, 0)",
			rusqlite::params![id, name],
		)
		.unwrap();
	}

	// A fresh insert sets stable_uuid = uuid; a content re-mint (new uuid, same parent+name) reuses
	// the same row, swaps `uuid` in place, and PRESERVES the original stable_uuid.
	#[test]
	fn stable_uuid_defaults_to_uuid_then_survives_remint() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());

		let uuid_a = Uuid::new_v4();
		let (id_a, _, stable_a) = item::upsert_item(
			&conn,
			uuid_a,
			Some(parent),
			Some("f.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		assert_eq!(stable_a, uuid_a, "fresh insert sets stable_uuid = uuid");
		add_file_meta(&conn, id_a, "f.txt");

		let uuid_b = Uuid::new_v4();
		let (id_b, _, stable_b) = item::upsert_item(
			&conn,
			uuid_b,
			Some(parent),
			Some("f.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		assert_eq!(id_b, id_a, "re-mint reuses the same row");
		assert_eq!(
			stable_b, uuid_a,
			"stable_uuid is preserved across a uuid re-mint"
		);

		let (row_uuid, row_stable): (Uuid, Uuid) = conn
			.query_row(
				"SELECT uuid, stable_uuid FROM items WHERE id = ?1",
				[id_a],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.unwrap();
		assert_eq!(row_uuid, uuid_b, "uuid was swapped in place");
		assert_eq!(row_stable, uuid_a, "stored stable_uuid unchanged");
	}

	// The `modify_file_content` fallback: when a content re-mint lands on a DIFFERENT row (e.g. a
	// concurrent rename changed (parent, name), so the upsert matched neither the uuid nor
	// (parent, name) and inserted a fresh row), the original stable_uuid must be transferred to the
	// new row and the stale original row deleted — without tripping UNIQUE(stable_uuid). This locks
	// in the delete-first ordering and the statements used by
	// `DBFile::upsert_remint_preserving_stable`.
	#[test]
	fn remint_fallback_transfers_stable_uuid_and_deletes_old_row() {
		use crate::sql::statements::{DELETE_BY_UUID, UPDATE_ITEM_STABLE_UUID};
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());

		// Original file row.
		let uuid_a = Uuid::new_v4();
		let (id_a, _, stable_a) = item::upsert_item(
			&conn,
			uuid_a,
			Some(parent),
			Some("a.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		add_file_meta(&conn, id_a, "a.txt");
		assert_eq!(stable_a, uuid_a);

		// A re-mint whose (parent, name) no longer matches the original row -> a NEW row is inserted
		// with stable_uuid defaulting to its own uuid.
		let uuid_b = Uuid::new_v4();
		let (id_b, _, stable_b) = item::upsert_item(
			&conn,
			uuid_b,
			Some(parent),
			Some("b.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		add_file_meta(&conn, id_b, "b.txt");
		assert_ne!(id_b, id_a, "a different (parent,name) yields a new row");
		assert_eq!(stable_b, uuid_b);

		// Fallback: delete the stale row FIRST (frees the UNIQUE slot), then adopt the original stable.
		conn.prepare_cached(DELETE_BY_UUID)
			.unwrap()
			.execute([uuid_a])
			.unwrap();
		conn.prepare_cached(UPDATE_ITEM_STABLE_UUID)
			.unwrap()
			.execute((stable_a, id_b))
			.unwrap();

		// The old row is gone; the new row now carries the original stable identity.
		let old_exists: bool = conn
			.query_row(
				"SELECT EXISTS(SELECT 1 FROM items WHERE uuid = ?1)",
				[uuid_a],
				|r| r.get(0),
			)
			.unwrap();
		assert!(!old_exists, "the stale original row is deleted");
		let (row_uuid, row_stable): (Uuid, Uuid) = conn
			.query_row(
				"SELECT uuid, stable_uuid FROM items WHERE id = ?1",
				[id_b],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.unwrap();
		assert_eq!(row_uuid, uuid_b);
		assert_eq!(
			row_stable, uuid_a,
			"the original stable id is carried onto the re-minted row"
		);
	}

	// Re-upserting the SAME uuid (an ordinary metadata refresh) keeps stable_uuid == uuid.
	#[test]
	fn stable_uuid_unchanged_on_same_uuid_upsert() {
		let conn = setup();
		let parent = ParentUuid::Uuid(Uuid::new_v4());
		let uuid = Uuid::new_v4();
		let (id, _, _) = item::upsert_item(
			&conn,
			uuid,
			Some(parent),
			Some("g.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		add_file_meta(&conn, id, "g.txt");
		let (_, _, stable) = item::upsert_item(
			&conn,
			uuid,
			Some(parent),
			Some("g.txt"),
			None,
			ItemType::File,
		)
		.unwrap();
		assert_eq!(stable, uuid);
	}
}

#[cfg(test)]
mod container_tests {
	use filen_types::fs::{ParentUuid, Uuid};
	use rusqlite::Connection;

	use super::{
		ItemContainer, classify_item_container, insert_root, recursive_select_path_from_uuid,
	};
	use crate::{
		CacheError, auth::configure_connection, ffi::ItemType, sql::item, sql::statements::INIT,
	};

	fn setup() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		// Register the `uuid_text` SQL function (and PRAGMAs) exactly as a real connection does, so the
		// recursive path query — which renders BLOB uuids via `uuid_text` — resolves.
		configure_connection(&conn).unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	// (b) A root-reachable item classifies to Root and canonicalizes to its full path id, which
	// includes the drive root uuid.
	#[test]
	fn root_reachable_item_classifies_to_root_full_path() {
		let mut conn = setup();
		let root = Uuid::new_v4();
		insert_root(&mut conn, root).unwrap();
		let dir = Uuid::new_v4();
		item::upsert_item(
			&conn,
			dir,
			Some(ParentUuid::Uuid(root)),
			Some("d"),
			None,
			ItemType::Dir,
		)
		.unwrap();
		let file = Uuid::new_v4();
		item::upsert_item(
			&conn,
			file,
			Some(ParentUuid::Uuid(dir)),
			Some("f"),
			None,
			ItemType::File,
		)
		.unwrap();

		assert!(matches!(
			classify_item_container(&conn, file, root).unwrap(),
			ItemContainer::Root
		));
		// Full path is "<root>/<dir>/<file>" (names fall back to uuids without cached metadata).
		assert_eq!(
			recursive_select_path_from_uuid(&conn, file).unwrap(),
			Some(format!("{root}/{dir}/{file}"))
		);
	}

	// (a) A trash-NESTED item (a dir under trash, a file under that dir) classifies to Trash, so it
	// canonicalizes to the `trash/<uuid>` form regardless of nesting depth.
	#[test]
	fn trash_nested_item_classifies_to_trash() {
		let conn = setup();
		let root = Uuid::new_v4();
		let dir = Uuid::new_v4();
		// Trashed items keep their ORIGINAL parent in `parent`; the payload here (its former parent,
		// the root) is what `decompose_parent` stores alongside `trashed = TRUE`.
		item::upsert_item(
			&conn,
			dir,
			Some(ParentUuid::Trash(root)),
			Some("d"),
			None,
			ItemType::Dir,
		)
		.unwrap();
		let file = Uuid::new_v4();
		item::upsert_item(
			&conn,
			file,
			Some(ParentUuid::Uuid(dir)),
			Some("f"),
			None,
			ItemType::File,
		)
		.unwrap();

		assert!(matches!(
			classify_item_container(&conn, file, root).unwrap(),
			ItemContainer::Trash
		));
	}

	// (c) An orphan (its parent row was never cached, as happens for `update_recents` inserts) is a
	// DoesNotExist error — NOT silently misclassified as trash-parented.
	#[test]
	fn orphaned_item_is_does_not_exist_not_trash() {
		let conn = setup();
		let root = Uuid::new_v4();
		let missing_parent = Uuid::new_v4();
		let file = Uuid::new_v4();
		item::upsert_item(
			&conn,
			file,
			Some(ParentUuid::Uuid(missing_parent)),
			Some("f"),
			None,
			ItemType::File,
		)
		.unwrap();

		let err = classify_item_container(&conn, file, root).unwrap_err();
		assert!(
			matches!(err, CacheError::DoesNotExist(_)),
			"an orphan chain must be DoesNotExist, got {err:?}"
		);
	}
}
