use filen_sdk_rs::{
	fs::{
		HasName, HasParent, HasUUID,
		dir::{RemoteDirectory, meta::DirectoryMeta},
		file::{RemoteFile, meta::FileMeta},
	},
	user::UserInfo,
	util::PathIteratorExt,
};
use filen_types::fs::{ParentUuid, StableUuid, Uuid, UuidStr};
use libsqlite3_sys::SQLITE_CONSTRAINT_UNIQUE;
use rusqlite::{
	Connection, OptionalExtension, ToSql,
	types::{FromSql, FromSqlError, FromSqlResult, ValueRef},
};
use tracing::{debug, trace};

pub mod error;
pub use error::SQLError;
pub(crate) mod columns;
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
		columns::{DISPLAY_NAME, ITEMS_ID, ITEMS_TYPE, ITEMS_UUID, PATH, POSITION},
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

pub(crate) fn insert_root(conn: &mut Connection, root: Uuid) -> Result<(), rusqlite::Error> {
	let tx: rusqlite::Transaction<'_> = conn.transaction()?;
	{
		let mut stmt = tx.prepare_cached(INSERT_ROOT_INTO_ITEMS)?;
		let id: i64 = match stmt.query_one((root, ItemType::Root as i8), |row| row.get(ITEMS_ID)) {
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
	let id: i64 = conn.query_one(SELECT_ID_BY_UUID, [root_uuid], |row| row.get(ITEMS_ID))?;
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
			let uuid = f.get::<_, Uuid>(ITEMS_UUID)?;
			let item_type = f.get::<_, ItemType>(ITEMS_TYPE)?;
			let name_or_uuid = f.get::<_, String>(DISPLAY_NAME)?;
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
	stmt.query_row([uuid], |row| row.get(PATH)).optional()
}

/// Replaces `local_data` outright. An empty map stores NULL rather than an empty JSON object.
///
/// The column is the app's alone — the pending-upload marker that used to share it has its own
/// column — so what lands is always what was passed in, and there is nothing to report back.
pub(crate) fn update_local_data(
	conn: &mut Connection,
	uuid: Uuid,
	local_data: Option<&JsonObject>,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(UPDATE_LOCAL_DATA_BY_UUID)?;
	let local_data = local_data.filter(|d| !d.is_empty());
	stmt.execute((local_data, uuid))?;
	Ok(())
}

/// Marks a file as having local changes that are not on the server yet, as of `marked_at_millis`.
///
/// Written before the upload is attempted, not after it fails: if the process dies mid-upload the
/// marker survives and the edit is retried, whereas marking only on failure would lose exactly the
/// edits interrupted at the worst moment.
pub(crate) fn mark_pending_upload(
	conn: &Connection,
	stable_uuid: StableUuid,
	marked_at_millis: i64,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(MARK_PENDING_UPLOAD)?;
	stmt.execute((stable_uuid, marked_at_millis))?;
	Ok(())
}

/// When the file a stable id names was marked, or `None` if it has nothing outstanding.
pub(crate) fn select_pending_upload_at(
	conn: &Connection,
	stable_uuid: StableUuid,
) -> Result<Option<i64>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_PENDING_UPLOAD_AT)?;
	stmt.query_row((stable_uuid,), |row| row.get(0))
		.optional()
		.map(Option::flatten)
}

/// Drops the pending-upload marker once the local changes have reached the server.
pub(crate) fn clear_pending_upload(
	conn: &Connection,
	stable_uuid: StableUuid,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(CLEAR_PENDING_UPLOAD)?;
	stmt.execute((stable_uuid,))?;
	Ok(())
}

/// Whether any file below `dir_uuid` still has an edit that has not reached the server.
///
/// Scoped to the descendants a delete of that directory's row would take with it, which is what
/// makes this the guard for one: see `sql/select_descendant_pending_upload.sql`.
pub(crate) fn has_descendant_pending_upload(
	conn: &Connection,
	dir_uuid: Uuid,
) -> Result<bool, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_DESCENDANT_PENDING_UPLOAD)?;
	stmt.exists([dir_uuid])
}

/// Every file still marked as having unuploaded local changes, oldest first.
pub(crate) fn select_pending_uploads(
	conn: &Connection,
) -> Result<Vec<StableUuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_PENDING_UPLOADS)?;
	let uuids = stmt
		.query_map([], |row| row.get(0))?
		.collect::<Result<Vec<StableUuid>, _>>()?;
	Ok(uuids)
}

/// Records that a file's bytes are in the local cache directory, as of `marked_at_millis`.
///
/// Keyed on the file's uuid, which is what its cache slot is named after; see
/// `sql/mark_materialised.sql`.
pub(crate) fn mark_materialised(
	conn: &Connection,
	uuid: Uuid,
	marked_at_millis: i64,
) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(MARK_MATERIALISED)?;
	stmt.execute((uuid, marked_at_millis))?;
	Ok(())
}

/// Drops the record once the bytes have left the cache directory.
pub(crate) fn clear_materialised(conn: &Connection, uuid: Uuid) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(CLEAR_MATERIALISED)?;
	stmt.execute((uuid,))?;
	Ok(())
}

/// Drops the record of every file whose cache slot is not in `uuids`, the uuid directories a
/// listing of the cache directory taken at `listed_at_millis` found.
///
/// The sweeps delete slots by path and cannot write to the database themselves; this is what puts
/// the column back in step with the directory afterwards.
pub(crate) fn clear_materialised_not_in_cache<I>(
	conn: &Connection,
	uuids: I,
	listed_at_millis: i64,
) -> Result<(), rusqlite::Error>
where
	I: ExactSizeIterator<Item = UuidStr>,
{
	let mut stmt = conn.prepare_cached(CLEAR_MATERIALISED_NOT_IN_CACHE)?;
	stmt.execute((uuids_json_array(uuids), listed_at_millis))?;
	Ok(())
}

/// The id of this incarnation of the database and the sequence it has reached, as one read.
///
/// The two belong together: a sequence only means anything against the instance that issued it,
/// which is what makes an anchor from before a wipe recognisable as expired.
pub(crate) fn select_change_meta(conn: &Connection) -> Result<([u8; 16], i64), rusqlite::Error> {
	conn.query_one(SELECT_CHANGE_META, [], |row| Ok((row.get(0)?, row.get(1)?)))
}

/// Every item stamped above `seq_floor` — what a replica holding that sequence has not been shown.
pub(crate) fn select_changed_items(
	conn: &Connection,
	seq_floor: i64,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGED_ITEMS)?;
	stmt.query_and_then([seq_floor], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Every provider id retired above `seq_floor`, oldest first.
pub(crate) fn select_retired_ids(
	conn: &Connection,
	seq_floor: i64,
) -> Result<Vec<Uuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_RETIRED_IDS)?;
	stmt.query_map([seq_floor], |row| row.get(0))?
		.collect::<Result<Vec<_>, _>>()
}

/// The items this device has a stake in: bytes on disk, an edit waiting to go out, or a favourite.
pub(crate) fn select_working_set(conn: &Connection) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_WORKING_SET)?;
	stmt.query_and_then([], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
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
		let mut select_change_seq = tx.prepare_cached(SELECT_CHANGE_SEQ)?;

		for dir in dirs {
			trace!("Updating recent directory: {}", dir.uuid());
			let dir = DBDir::upsert_from_remote_stmts(
				dir,
				&mut upsert_item_stmt,
				&mut upsert_dir,
				&mut upset_dir_meta,
				&mut delete_dir_meta,
				&mut select_change_seq,
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
				&mut select_change_seq,
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

/// Whether an incoming trashed record would land on a row that is not it.
///
/// `upsert_item` resolves identity by uuid, then stable id, then `(parent, name)` — and neither
/// of the last two tiers knows anything about `trashed`. So a trashed record whose uuid the cache
/// does not hold adopts whatever LIVE row occupies its name slot and drags that row into the
/// trash, where the pending-upload drain (untrashed rows only) can no longer reach it and the
/// stale sweep eventually deletes it outright. The ghost of a versioning-disabled edit is exactly
/// that shape: the retired uuid stays listed as trashed for ~60s, re-stamped with a stable id
/// belonging to no lineage, while the live head holding the user's edit sits in the name slot.
///
/// This is the bulk listing's copy of the rule [`crate::sync::check_local_item_matches_remote`]
/// applies when revalidating a single item. A record refused here simply does not appear in the
/// cached trash listing — a gap that closes as soon as the collision does, and one the sweep
/// cannot turn into a deletion, because it only reaps rows that are already trashed.
fn trashed_record_targets_another_row(
	conn: &Connection,
	uuid: Uuid,
	stable: Option<StableUuid>,
	parent: &ParentUuid,
	name: Option<&str>,
	type_: ItemType,
) -> Result<bool, rusqlite::Error> {
	if !parent.is_trash() {
		// A live record is resolved against live rows, which is what the tiers are for.
		return Ok(false);
	}

	// The uuid tier. For a dir a match settles it — the server never re-mints a dir's uuid, so
	// the row is the same object. For a file the stable id has to agree as well, or the uuid is
	// a corpse the server re-stamped and the row is the lineage that outlived it.
	let mut stmt = conn.prepare_cached(SELECT_STABLE_BY_UUID)?;
	if let Some(row_stable) = stmt
		.query_one((uuid, type_), |row| row.get::<_, Option<StableUuid>>(0))
		.optional()?
	{
		return Ok(row_stable != stable);
	}

	// The stable tier: a file whose uuid the server re-minted still resolves here, and that row
	// IS this record — trashing it is the update the listing came to deliver.
	if let Some(stable) = stable
		&& RawDBItem::select_by_stable(conn, stable.into())?.is_some()
	{
		return Ok(false);
	}

	// Nothing here is this record, so the `(parent, name)` tier is all that is left — and
	// whatever holds that name is live, and is somebody else.
	let (Some(parent), Some(name)) = (parent.original_parent(), name) else {
		return Ok(false);
	};
	let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_PARENT_NAME)?;
	Ok(stmt
		.query_one((parent, name), RawDBItem::from_row)
		.optional()?
		.is_some_and(|item| item.type_ == type_))
}

/// Whether a listing record already owns a row via one of the identity tiers
/// (uuid or stable id, type-scoped like the upsert). Name-tier-only records
/// return false — they must wait for the second pass.
fn resolves_by_identity(
	conn: &Connection,
	uuid: Uuid,
	stable_uuid: Option<StableUuid>,
	type_: ItemType,
) -> Result<bool, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(
		"SELECT 1 FROM items
		WHERE (items.uuid = ?1 OR (?2 IS NOT NULL AND items.stable_uuid = ?2))
		AND items.type = ?3
		LIMIT 1;",
	)?;
	stmt.exists((uuid, stable_uuid, type_))
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
	let mut select_change_seq = tx.prepare_cached(SELECT_CHANGE_SEQ)?;

	// Applied in two passes per kind: records that resolve to an existing row
	// by uuid or stable id first, everything else (name-tier matches and fresh
	// inserts) after. A single pass in listing order lets a new item that took
	// over a renamed sibling's old name reach the `(parent, name)` tier before
	// the sibling's own record has moved its row along — stealing the row and
	// its local_data (pending-upload marker included). Once every
	// identity-resolving record has been applied, the name tier can no longer
	// see a row that a later record in the same batch owns.
	//
	// Partitioned by hand rather than with `Iterator::partition`, which cannot
	// carry an error out: a lookup that fails has to fail the batch. Swallowing
	// it would silently demote the record to the name tier, which is the tier
	// that retires ids — and a retirement is an authoritative delete to every
	// replica.
	let (mut dirs_by_identity, mut dirs_by_name) = (Vec::new(), Vec::new());
	for dir in dirs {
		if resolves_by_identity(tx, dir.uuid(), None, ItemType::Dir)? {
			dirs_by_identity.push(dir);
		} else {
			dirs_by_name.push(dir);
		}
	}

	for dir in dirs_by_identity.into_iter().chain(dirs_by_name) {
		if trashed_record_targets_another_row(
			tx,
			dir.uuid(),
			None,
			dir.parent(),
			dir.name(),
			ItemType::Dir,
		)? {
			debug!(
				"Skipping trashed directory {}: it is not the row it resolves onto",
				dir.uuid()
			);
			continue;
		}
		DBDir::upsert_from_remote_stmts(
			dir,
			&mut upsert_item_stmt,
			&mut upsert_dir,
			&mut upsert_dir_meta,
			&mut delete_dir_meta,
			&mut select_change_seq,
		)?;
	}

	let mut upsert_file = tx.prepare_cached(UPSERT_FILE)?;
	let mut upsert_file_meta = tx.prepare_cached(UPSERT_FILE_META)?;
	let mut delete_file_meta = tx.prepare_cached(DELETE_FILE_META)?;

	// Same two-pass rule as the dirs above, hand-partitioned for the same reason.
	let (mut files_by_identity, mut files_by_name) = (Vec::new(), Vec::new());
	for file in files {
		if resolves_by_identity(tx, file.uuid(), Some(file.stable_uuid()), ItemType::File)? {
			files_by_identity.push(file);
		} else {
			files_by_name.push(file);
		}
	}

	for file in files_by_identity.into_iter().chain(files_by_name) {
		if trashed_record_targets_another_row(
			tx,
			file.uuid(),
			Some(file.stable_uuid()),
			file.parent(),
			file.name(),
			ItemType::File,
		)? {
			debug!(
				"Skipping trashed file {}: it is not the row it resolves onto",
				file.uuid()
			);
			continue;
		}
		DBFile::upsert_from_remote_stmts(
			file,
			&mut upsert_item_stmt,
			&mut upsert_file,
			&mut upsert_file_meta,
			&mut delete_file_meta,
			&mut select_change_seq,
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

/// The uuids as the JSON array text the statements built on `json_each` bind.
fn uuids_json_array<I>(uuids: I) -> String
where
	I: ExactSizeIterator<Item = UuidStr>,
{
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
	uuids_json_string
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
	stmt.query_and_then([uuids_json_array(uuids)], |row| Ok(row.get(POSITION)?))?
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
mod pending_upload_tests {
	use std::collections::HashMap;

	use super::*;
	use crate::sql::item::{self, combine_parent};

	const MARKED_AT: i64 = 1_700_000_000_000;

	fn db() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	/// Upserts a file row the way a remote listing would, and gives it the `files_meta` name row
	/// the `(parent, name)` fallback in `upsert_item` joins against.
	fn add_file(
		conn: &Connection,
		uuid_: Uuid,
		stable: StableUuid,
		parent: Uuid,
		name: &str,
	) -> i64 {
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (id, _, _) = item::upsert_file_item_with_stmts(
			uuid_,
			stable,
			combine_parent(Some(parent), false),
			Some(name),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);
		conn.execute(
			"INSERT INTO files (id, size, chunks, region, bucket, timestamp, metadata_state)
			VALUES (?1, 0, 0, '', '', 0, 0);",
			[id],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO files_meta (id, name, mime, file_key, file_key_version, modified)
			VALUES (?1, ?2, '', '', 3, 0);",
			rusqlite::params![id, name],
		)
		.unwrap();
		id
	}

	/// A dir row, as bare as the descendant walk needs it: that statement only ever reads `items`.
	fn add_dir(conn: &Connection, uuid_: Uuid, parent: Uuid) {
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_dir_item_with_stmts(
			uuid_,
			combine_parent(Some(parent), false),
			None,
			None,
			&mut stmt,
		)
		.unwrap();
	}

	fn local_data_of(conn: &Connection, uuid_: Uuid) -> Option<HashMap<String, String>> {
		conn.query_row(
			"SELECT local_data FROM items WHERE uuid = ?1;",
			[uuid_],
			|r| r.get::<_, Option<JsonObject>>(0),
		)
		.unwrap()
		.map(|d| d.to_map())
	}

	fn pending_upload_at_of(conn: &Connection, uuid_: Uuid) -> Option<i64> {
		conn.query_row(
			"SELECT pending_upload_at FROM items WHERE uuid = ?1;",
			[uuid_],
			|r| r.get(0),
		)
		.unwrap()
	}

	/// `local_data` is the app's alone, so a write through the FFI replaces it outright — no key
	/// of ours to merge back in. The pending-upload marker sits in its own column precisely so
	/// that plain replace cannot reach an outstanding local edit.
	#[test]
	fn a_local_data_write_replaces_it_and_leaves_the_marker_alone() {
		let mut conn = db();
		add_file(&conn, uuid(1), stable(2), uuid(9), "edited.txt");
		let mut old = HashMap::new();
		old.insert("Stale".to_string(), "overwrite me".to_string());
		update_local_data(&mut conn, uuid(1), Some(&JsonObject::new(old))).unwrap();
		mark_pending_upload(&conn, stable(2), MARKED_AT).unwrap();

		let mut tags = HashMap::new();
		tags.insert("TagData".to_string(), "keep me".to_string());
		update_local_data(&mut conn, uuid(1), Some(&JsonObject::new(tags.clone()))).unwrap();

		assert_eq!(
			local_data_of(&conn, uuid(1)),
			Some(tags),
			"the write must land exactly as sent, dropping the keys it replaced"
		);
		assert_eq!(
			pending_upload_at_of(&conn, uuid(1)),
			Some(MARKED_AT),
			"a local_data write must not reach the pending-upload marker"
		);
		assert_eq!(
			select_pending_uploads(&conn).unwrap(),
			vec![stable(2)],
			"the edit must still be drainable after the tag write"
		);
	}

	/// An outstanding upload is a files-only concept for the same reason a stable id is: the
	/// CHECK is what makes that true of the storage, not just of the code that writes it.
	#[test]
	fn a_dir_row_may_not_carry_a_pending_upload_marker() {
		let conn = db();
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_dir_item_with_stmts(
			uuid(1),
			combine_parent(Some(uuid(9)), false),
			Some("d"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert!(
			conn.execute(
				"UPDATE items SET pending_upload_at = 100 WHERE uuid = ?1;",
				[uuid(1)],
			)
			.is_err(),
			"a dir with bytes waiting to be uploaded is not a thing that exists"
		);
	}

	/// The server re-mints a file's uuid on every content edit, and the upsert reconciles the new
	/// head onto the existing row through the stable tier. The marker has to ride along — it is
	/// the record of an edit that has NOT reached the server, so dropping it here drops the edit.
	/// `upsert_item` never names the column, which is exactly what makes this hold.
	#[test]
	fn a_marker_survives_the_uuid_re_mint_that_reconciles_a_file() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "edited.txt");
		mark_pending_upload(&conn, stable(2), MARKED_AT).unwrap();

		// The same file back from a listing under a freshly minted uuid.
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (_, _, carried) = item::upsert_file_item_with_stmts(
			uuid(3),
			stable(2),
			combine_parent(Some(parent), false),
			Some("edited.txt"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert_eq!(
			pending_upload_at_of(&conn, uuid(3)),
			Some(MARKED_AT),
			"the re-minted head must still carry the outstanding edit"
		);
		assert_eq!(
			carried,
			Some(MARKED_AT),
			"and the upsert must report it, or the DBFile it builds says there is none"
		);
		assert_eq!(select_pending_uploads(&conn).unwrap(), vec![stable(2)]);
	}

	/// Oldest first, so a drain retries edits in roughly the order they were made. The ordering
	/// key is not in the select list, which is why this groups instead of using DISTINCT.
	#[test]
	fn the_drain_lists_the_oldest_marker_first() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(1), parent, "newer.txt");
		add_file(&conn, uuid(2), stable(2), parent, "older.txt");
		mark_pending_upload(&conn, stable(1), MARKED_AT + 1_000).unwrap();
		mark_pending_upload(&conn, stable(2), MARKED_AT).unwrap();

		assert_eq!(
			select_pending_uploads(&conn).unwrap(),
			vec![stable(2), stable(1)],
			"the older edit must be drained first"
		);
	}

	fn stable_uuid_of(conn: &Connection, uuid_: Uuid) -> Option<Uuid> {
		conn.query_row(
			"SELECT stable_uuid FROM items WHERE uuid = ?1;",
			[uuid_],
			|r| r.get(0),
		)
		.unwrap()
	}

	/// Stable ids are a files-only concept, and the `items` CHECK is what makes that true of the
	/// storage rather than only of the code: it is why `DBFile::stable_uuid` can be a plain
	/// `StableUuid` while the column itself is nullable.
	#[test]
	fn only_files_carry_a_stable_id() {
		let conn = db();
		let parent = uuid(9);

		add_file(&conn, uuid(1), stable(2), parent, "a.txt");
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_dir_item_with_stmts(
			uuid(3),
			combine_parent(Some(parent), false),
			Some("d"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);
		item::upsert_root_item(&conn, uuid(4)).unwrap();

		assert_eq!(
			stable_uuid_of(&conn, uuid(1)),
			Some(uuid(2)),
			"a file keeps its stable id"
		);
		assert_eq!(
			stable_uuid_of(&conn, uuid(3)),
			None,
			"a dir must not carry one"
		);
		assert_eq!(
			stable_uuid_of(&conn, uuid(4)),
			None,
			"a root must not carry one"
		);

		assert!(
			conn.execute(
				"INSERT INTO items (uuid, stable_uuid, type) VALUES (?1, ?2, 1);",
				rusqlite::params![uuid(5), uuid(6)],
			)
			.is_err(),
			"a dir row with a stable id must be rejected"
		);
		assert!(
			conn.execute("INSERT INTO items (uuid, type) VALUES (?1, 2);", [uuid(7)])
				.is_err(),
			"a file row without a stable id must be rejected"
		);
	}

	/// A uuid arriving with a different type means the server reassigned it to a
	/// new object (uuid-reuse abuse). The old row must be retired — not adopted
	/// with a flipped type — so its stable id, per-type rows and local_data die
	/// with it instead of leaking onto an unrelated object.
	#[test]
	fn a_cross_type_uuid_reuse_retires_the_old_row() {
		let conn = db();
		let file_id = add_file(&conn, uuid(1), stable(1), uuid(0), "victim.txt");
		mark_pending_upload(&conn, stable(1), 100).unwrap();
		conn.execute(
			r#"UPDATE items SET local_data = '{"Tag":"mine"}' WHERE uuid = ?1;"#,
			[uuid(1)],
		)
		.unwrap();

		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (dir_id, carried_local_data) = item::upsert_dir_item_with_stmts(
			uuid(1),
			combine_parent(Some(uuid(0)), false),
			Some("victim.txt"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert_ne!(dir_id, file_id, "the file row must be retired, not adopted");
		assert_eq!(
			carried_local_data, None,
			"the retired file's local_data must not leak onto the new object"
		);
		let (type_, stable_col): (i64, Option<StableUuid>) = conn
			.query_row(
				"SELECT type, stable_uuid FROM items WHERE uuid = ?1;",
				[uuid(1)],
				|r| Ok((r.get(0)?, r.get(1)?)),
			)
			.unwrap();
		assert_eq!(type_, 1, "the uuid now denotes a dir");
		assert_eq!(stable_col, None, "no stable id survives the retirement");
		assert_eq!(
			pending_upload_at_of(&conn, uuid(1)),
			None,
			"nor does the retired file's pending-upload marker"
		);
		let orphaned_file_rows: i64 = conn
			.query_row(
				"SELECT COUNT(*) FROM files WHERE id = ?1;",
				[file_id],
				|r| r.get(0),
			)
			.unwrap();
		assert_eq!(orphaned_file_rows, 0, "the files row must cascade away");
	}

	/// Duplicate stable ids are reachable via same-account uuid reuse, and every
	/// read path resolves them with the same trashed/id tie-break. Marking and
	/// clearing must hit exactly that row — a bare stable match would strip a
	/// sibling's genuine marker.
	#[test]
	fn marking_and_clearing_scope_to_the_preferred_duplicate() {
		let conn = db();
		let first = add_file(&conn, uuid(1), stable(9), uuid(0), "a.txt");
		// The upsert would reconcile a duplicate stable onto the first row (by
		// design), so seed the abuse-shaped sibling with a raw insert.
		conn.execute(
			"INSERT INTO items (uuid, stable_uuid, parent, type) VALUES (?1, ?2, ?3, 2);",
			rusqlite::params![uuid(2), stable(9), uuid(0)],
		)
		.unwrap();
		let second: i64 = conn
			.query_row("SELECT id FROM items WHERE uuid = ?1;", [uuid(2)], |r| {
				r.get(0)
			})
			.unwrap();
		assert_ne!(first, second);

		mark_pending_upload(&conn, stable(9), 100).unwrap();
		assert_eq!(
			pending_upload_at_of(&conn, uuid(1)),
			Some(100),
			"the tie-broken row must carry the marker"
		);
		assert_eq!(
			pending_upload_at_of(&conn, uuid(2)),
			None,
			"the sibling must not be marked"
		);

		// Give the sibling its own genuine marker, then clear: only the
		// tie-broken row may lose one.
		conn.execute(
			"UPDATE items SET pending_upload_at = 50 WHERE uuid = ?1;",
			[uuid(2)],
		)
		.unwrap();
		clear_pending_upload(&conn, stable(9)).unwrap();
		assert_eq!(
			pending_upload_at_of(&conn, uuid(1)),
			None,
			"the tie-broken row's marker is cleared"
		);
		assert_eq!(
			pending_upload_at_of(&conn, uuid(2)),
			Some(50),
			"the sibling's marker must survive the clear"
		);
	}

	/// `upsert_item` resolves the target row by uuid, then stable id, then `(parent, name)`. The
	/// `local_data` COALESCE chain re-runs those three tiers independently, and COALESCE also skips
	/// a tier whose matched row simply has a NULL `local_data` — so a row matched by stable id can
	/// fall through to the name tier and adopt a *different* file's `local_data`.
	#[test]
	fn an_upsert_matched_by_stable_id_must_not_inherit_another_rows_local_data() {
		let conn = db();
		let parent = uuid(9);

		// The row that currently holds the name "taken.txt", carrying an outstanding local edit.
		add_file(&conn, uuid(1), stable(2), parent, "taken.txt");
		mark_pending_upload(&conn, stable(2), MARKED_AT).unwrap();

		// An unrelated file with no local_data of its own.
		add_file(&conn, uuid(3), stable(4), parent, "other.txt");

		// It is renamed onto "taken.txt" and its content edited, so the server re-mints its uuid
		// while the stable id stays put. Identity resolves via the stable tier onto uuid(3)'s row.
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_file_item_with_stmts(
			uuid(5),
			stable(4),
			combine_parent(Some(parent), false),
			Some("taken.txt"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert_eq!(
			local_data_of(&conn, uuid(5)),
			None,
			"a row matched by stable id must keep its own (absent) local_data, not adopt the \
			 name-slot row's"
		);
		assert_eq!(
			pending_upload_at_of(&conn, uuid(1)),
			Some(MARKED_AT),
			"the marker must stay on the file that actually has the outstanding edit"
		);
	}

	/// A dir row is never deleted alone — the cascade takes everything under it, markers and all.
	/// The guard therefore has to see as far as the cascade does, which is every generation, not
	/// just the children.
	#[test]
	fn a_marked_descendant_two_levels_down_is_visible_to_the_dir_guard() {
		let conn = db();
		add_dir(&conn, uuid(1), uuid(9));
		add_dir(&conn, uuid(2), uuid(1));
		add_file(&conn, uuid(3), stable(4), uuid(2), "deep.txt");
		// A sibling branch, so the walk has somewhere wrong to wander off to.
		add_dir(&conn, uuid(5), uuid(9));
		add_file(&conn, uuid(6), stable(7), uuid(5), "elsewhere.txt");

		assert!(
			!has_descendant_pending_upload(&conn, uuid(1)).unwrap(),
			"nothing is outstanding yet, so the dir is free to go"
		);

		mark_pending_upload(&conn, stable(4), MARKED_AT).unwrap();

		assert!(
			has_descendant_pending_upload(&conn, uuid(1)).unwrap(),
			"the edit is two generations down, which is still inside the cascade"
		);
		assert!(
			!has_descendant_pending_upload(&conn, uuid(5)).unwrap(),
			"and a directory the cascade would never reach stays free to go"
		);
	}

	/// The cascade spares trashed rows — they hang off the trash listing rather than off their
	/// parent — so a marker on one is not a reason to keep the directory: deleting it cannot reach
	/// those bytes.
	#[test]
	fn a_trashed_descendant_is_outside_the_guard_because_it_is_outside_the_cascade() {
		let conn = db();
		add_dir(&conn, uuid(1), uuid(9));
		add_file(&conn, uuid(2), stable(3), uuid(1), "trashed.txt");
		conn.execute(
			"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
			[uuid(2)],
		)
		.unwrap();
		mark_pending_upload(&conn, stable(3), MARKED_AT).unwrap();

		assert!(!has_descendant_pending_upload(&conn, uuid(1)).unwrap());
	}
}

#[cfg(test)]
mod trashed_listing_tests {
	use std::borrow::Cow;

	use chrono::Utc;
	use filen_sdk_rs::{crypto::file::FileKey, fs::file::meta::DecryptedFileMeta};
	use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid};

	use super::*;

	const MARKED_AT: i64 = 1_700_000_000_000;

	/// Configured the way every real connection is, so the name-slot lookup the guard performs
	/// has the `uuid_text` function those statements are written against.
	fn db() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	/// A file the way a listing hands it over.
	fn remote_file(uuid_: Uuid, stable: StableUuid, parent: ParentUuid, name: &str) -> RemoteFile {
		let now = Utc::now();
		RemoteFile::from_meta(
			uuid_,
			stable,
			parent,
			0,
			0,
			"us-east-1",
			"test-bucket",
			now,
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.to_string()),
				size: 0,
				mime: Cow::Owned("text/plain".to_string()),
				key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
					.unwrap(),
				last_modified: now,
				created: Some(now),
				hash: None,
			}),
		)
	}

	/// The live head of a file with an edit that has not reached the server yet, seeded through
	/// the same bulk ingest a directory listing uses.
	fn seed_live_file(conn: &mut Connection, uuid_: Uuid, stable: StableUuid, parent: Uuid) {
		update_items_with_parent(
			conn,
			[],
			[remote_file(
				uuid_,
				stable,
				ParentUuid::Uuid(parent),
				"edited.txt",
			)],
			parent,
		)
		.unwrap();
		mark_pending_upload(conn, stable, MARKED_AT).unwrap();
	}

	/// A versioning-disabled edit leaves the retired uuid listed as trashed for ~60s, re-stamped
	/// with a stable id that belongs to no lineage. Our own upload has already moved the row to
	/// the new uuid, so that ghost matches nothing by uuid or stable id and falls through to the
	/// `(parent, name)` tier — where the live head is sitting, holding the user's edit.
	#[test]
	fn a_trash_listing_must_not_adopt_a_live_row_by_name() {
		let mut conn = db();
		let parent = uuid(9);
		seed_live_file(&mut conn, uuid(2), stable(1), parent);

		update_trashed_items(
			&mut conn,
			[],
			[remote_file(
				uuid(3),
				stable(4),
				ParentUuid::Trash(parent),
				"edited.txt",
			)],
		)
		.unwrap();

		let row = RawDBItem::select(&conn, uuid(2))
			.unwrap()
			.expect("the live row must survive the trash refresh");
		assert_eq!(
			row.parent,
			Some(ParentUuid::Uuid(parent)),
			"a ghost must not drag the live head into the trash"
		);
		assert_eq!(
			select_pending_uploads(&conn).unwrap(),
			vec![stable(1)],
			"the outstanding edit must stay drainable"
		);
	}

	/// The same ghost, seen before the cache learned about the new uuid: it still carries the
	/// retired uuid, so it matches our row outright — but its stable id is not ours, and adopting
	/// it would overwrite the lineage id the live head is about to arrive with.
	#[test]
	fn a_trash_listing_must_not_adopt_a_re_stamped_stable_id() {
		let mut conn = db();
		let parent = uuid(9);
		seed_live_file(&mut conn, uuid(1), stable(2), parent);

		update_trashed_items(
			&mut conn,
			[],
			[remote_file(
				uuid(1),
				stable(5),
				ParentUuid::Trash(parent),
				"edited.txt",
			)],
		)
		.unwrap();

		let row = RawDBItem::select(&conn, uuid(1))
			.unwrap()
			.expect("the live row must survive the trash refresh");
		assert_eq!(
			row.parent,
			Some(ParentUuid::Uuid(parent)),
			"a re-stamped corpse must not trash the row it was minted from"
		);
		assert_eq!(
			select_pending_uploads(&conn).unwrap(),
			vec![stable(2)],
			"the row must keep its own stable id, and its edit"
		);
	}

	/// The guard must only refuse records that are not what they claim to be: an item genuinely
	/// trashed elsewhere arrives with both ids intact and has to land, or the trash listing stops
	/// converging.
	#[test]
	fn a_genuinely_trashed_file_still_lands() {
		let mut conn = db();
		let parent = uuid(9);
		seed_live_file(&mut conn, uuid(1), stable(2), parent);

		update_trashed_items(
			&mut conn,
			[],
			[remote_file(
				uuid(1),
				stable(2),
				ParentUuid::Trash(parent),
				"edited.txt",
			)],
		)
		.unwrap();

		let row = RawDBItem::select(&conn, uuid(1)).unwrap().unwrap();
		assert_eq!(
			row.parent,
			Some(ParentUuid::Trash(parent)),
			"a real trashing must still be applied"
		);
	}

	/// One listing, hostile order: a new file that took over a renamed
	/// sibling's old name is enumerated before the sibling's own record. The
	/// newcomer must not reach the `(parent, name)` tier while the sibling's
	/// row still sits at the old name — that would steal the row, its stable
	/// id and its pending-upload marker.
	#[test]
	fn a_batch_newcomer_must_not_steal_a_renamed_siblings_row() {
		let mut conn = db();
		let parent = uuid(9);
		// A lives at "report.txt" with an outstanding edit marker.
		update_items_with_parent(
			&mut conn,
			[],
			[remote_file(
				uuid(1),
				stable(1),
				ParentUuid::Uuid(parent),
				"report.txt",
			)],
			parent,
		)
		.unwrap();
		mark_pending_upload(&conn, stable(1), MARKED_AT).unwrap();

		// The server listing: newcomer C now owns "report.txt", A moved to
		// "renamed.txt" — and C is enumerated first.
		update_items_with_parent(
			&mut conn,
			[],
			[
				remote_file(uuid(2), stable(2), ParentUuid::Uuid(parent), "report.txt"),
				remote_file(uuid(1), stable(1), ParentUuid::Uuid(parent), "renamed.txt"),
			],
			parent,
		)
		.unwrap();

		let a = RawDBItem::select(&conn, uuid(1))
			.unwrap()
			.expect("the renamed sibling keeps its row");
		assert_eq!(
			select_pending_uploads(&conn).unwrap(),
			vec![stable(1)],
			"the sibling's pending-upload marker must survive the batch, and only its own"
		);
		let c = RawDBItem::select(&conn, uuid(2))
			.unwrap()
			.expect("the newcomer gets its own fresh row");
		assert_ne!(a.id, c.id, "two files, two rows");
		assert_eq!(
			c.local_data, None,
			"the newcomer must not inherit the sibling's local_data"
		);
	}
}

#[cfg(test)]
mod change_tracking_tests {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_sdk_rs::{crypto::file::FileKey, fs::file::meta::DecryptedFileMeta};
	use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid};

	use super::*;
	use crate::{
		ffi::{FfiChanges, FfiObject},
		local::{changes_since, current_sync_anchor, working_set},
		sql::item::{self, combine_parent},
	};

	/// Configured the way every real connection is, which for these tests is the whole point: the
	/// triggers run under the production pragmas, `recursive_triggers` included, so a delete
	/// cascade reaches past the first generation exactly as it does in the app.
	fn db() -> Connection {
		let conn = Connection::open_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(INIT).unwrap();
		conn
	}

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	/// One fixed instant for every fixture, so two records for "the same file" really are
	/// identical — a fresh `Utc::now()` per call moves `modified` and turns a relisting into a
	/// change.
	fn fixed_time() -> DateTime<Utc> {
		DateTime::from_timestamp_millis(1_700_000_000_000).unwrap()
	}

	/// The sequence `current_sync_anchor()` would hand out at this instant.
	fn anchor(conn: &Connection) -> i64 {
		conn.query_row("SELECT counter FROM change_meta;", [], |r| r.get(0))
			.unwrap()
	}

	fn seq_of(conn: &Connection, uuid_: Uuid) -> i64 {
		conn.query_row(
			"SELECT change_seq FROM items WHERE uuid = ?1;",
			[uuid_],
			|r| r.get(0),
		)
		.unwrap()
	}

	/// Every tombstone as `(kind, id)`, oldest first.
	fn retired(conn: &Connection) -> Vec<(i64, Uuid)> {
		let mut stmt = conn
			.prepare("SELECT kind, item_id FROM tombstones ORDER BY seq;")
			.unwrap();
		stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
			.unwrap()
			.collect::<Result<Vec<_>, _>>()
			.unwrap()
	}

	/// The highest sequence anything in the database carries. The counter has to agree with it,
	/// or an anchor is not a number a diff can be built from.
	fn highest_stamp(conn: &Connection) -> i64 {
		conn.query_row(
			"SELECT max(
				(SELECT coalesce(max(change_seq), 0) FROM items),
				(SELECT coalesce(max(seq), 0) FROM tombstones)
			);",
			[],
			|r| r.get(0),
		)
		.unwrap()
	}

	/// A file row with the `files`/`files_meta` rows a real ingest would give it.
	fn add_file(conn: &Connection, uuid_: Uuid, stable_: StableUuid, parent: Uuid, name: &str) {
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (id, _, _) = item::upsert_file_item_with_stmts(
			uuid_,
			stable_,
			combine_parent(Some(parent), false),
			Some(name),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);
		conn.execute(
			"INSERT INTO files (id, size, chunks, region, bucket, timestamp, metadata_state)
			VALUES (?1, 0, 0, '', '', 0, 0);",
			[id],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO files_meta (id, name, mime, file_key, file_key_version, modified)
			VALUES (?1, ?2, '', '', 3, 0);",
			rusqlite::params![id, name],
		)
		.unwrap();
	}

	fn add_dir(conn: &Connection, uuid_: Uuid, parent: Uuid, name: &str) {
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (id, _) = item::upsert_dir_item_with_stmts(
			uuid_,
			combine_parent(Some(parent), false),
			Some(name),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);
		// `color` carries the text 'default' rather than NULL, which is what `upsert_dir` writes
		// for a dir the server gave no colour: a `DBDir` refuses to read one without it.
		conn.execute(
			"INSERT INTO dirs (id, color, timestamp, metadata_state)
			VALUES (?1, 'default', 0, 0);",
			[id],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO dirs_meta (id, name) VALUES (?1, ?2);",
			rusqlite::params![id, name],
		)
		.unwrap();
	}

	fn id_of(conn: &Connection, uuid_: Uuid) -> i64 {
		conn.query_row("SELECT id FROM items WHERE uuid = ?1;", [uuid_], |r| {
			r.get(0)
		})
		.unwrap()
	}

	/// A file the way a listing hands it over.
	fn remote_file(uuid_: Uuid, stable_: StableUuid, parent: ParentUuid, name: &str) -> RemoteFile {
		RemoteFile::from_meta(
			uuid_,
			stable_,
			parent,
			0,
			0,
			"us-east-1",
			"test-bucket",
			fixed_time(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.to_string()),
				size: 0,
				mime: Cow::Owned("text/plain".to_string()),
				key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
					.unwrap(),
				last_modified: fixed_time(),
				created: Some(fixed_time()),
				hash: None,
			}),
		)
	}

	/// The counter only climbs, every visible write lands above the last, and the counter is
	/// exactly the highest sequence ever issued — that equality is what lets an anchor be a bare
	/// number rather than a snapshot of the whole table.
	#[test]
	fn sequences_climb_and_the_counter_is_the_highest_one_issued() {
		let conn = db();
		let parent = uuid(9);

		let mut served = vec![anchor(&conn)];
		add_dir(&conn, uuid(1), parent, "sub");
		served.push(anchor(&conn));
		add_file(&conn, uuid(2), stable(3), uuid(1), "a.txt");
		served.push(anchor(&conn));

		let (dir_id, file_id) = (id_of(&conn, uuid(1)), id_of(&conn, uuid(2)));
		conn.execute(
			"UPDATE files_meta SET name = 'b.txt' WHERE id = ?1;",
			[file_id],
		)
		.unwrap();
		served.push(anchor(&conn));
		conn.execute("UPDATE items SET trashed = TRUE WHERE id = ?1;", [file_id])
			.unwrap();
		served.push(anchor(&conn));
		conn.execute("UPDATE dirs SET color = 'red' WHERE id = ?1;", [dir_id])
			.unwrap();
		served.push(anchor(&conn));
		conn.execute("DELETE FROM items WHERE id = ?1;", [file_id])
			.unwrap();
		served.push(anchor(&conn));

		assert!(
			served.windows(2).all(|w| w[1] > w[0]),
			"every visible write must move the counter: {served:?}"
		);
		assert_eq!(
			seq_of(&conn, uuid(1)),
			served[5],
			"the dir's stamp is the colour change, the last thing that touched it"
		);
		assert_eq!(
			anchor(&conn),
			highest_stamp(&conn),
			"the counter must be the highest sequence issued, not run ahead of it"
		);
	}

	/// A directory refresh re-upserts every child it already had. That has to be free, or a
	/// routine relisting stamps the whole directory and every replica gets handed a diff full of
	/// rows that did not change.
	#[test]
	fn an_unchanged_relisting_moves_nothing() {
		let mut conn = db();
		let parent = uuid(9);
		let listing = || {
			vec![
				remote_file(uuid(1), stable(2), ParentUuid::Uuid(parent), "a.txt"),
				remote_file(uuid(3), stable(4), ParentUuid::Uuid(parent), "b.txt"),
			]
		};

		update_items_with_parent(&mut conn, [], listing(), parent).unwrap();
		let after_first = anchor(&conn);
		let stamps = (seq_of(&conn, uuid(1)), seq_of(&conn, uuid(3)));

		update_items_with_parent(&mut conn, [], listing(), parent).unwrap();

		assert_eq!(
			anchor(&conn),
			after_first,
			"an identical relisting must not bump anything"
		);
		assert_eq!(
			(seq_of(&conn, uuid(1)), seq_of(&conn, uuid(3))),
			stamps,
			"nor restamp the rows it re-upserted"
		);
		assert!(retired(&conn).is_empty(), "nor retire any id");
	}

	/// Everything a replica renders has to move the sequence, and nothing else may. Each case
	/// below is a column some guard names, written through the statement that really writes it.
	#[test]
	fn every_replica_visible_change_bumps_the_item_and_local_state_does_not() {
		let conn = db();
		let parent = uuid(9);
		add_dir(&conn, uuid(1), parent, "sub");
		add_file(&conn, uuid(2), stable(3), parent, "a.txt");
		let (dir_id, file_id) = (id_of(&conn, uuid(1)), id_of(&conn, uuid(2)));

		let bumps = |sql: &str, id: i64, of: Uuid| {
			let before = seq_of(&conn, of);
			conn.execute(sql, [id]).unwrap();
			assert!(
				seq_of(&conn, of) > before,
				"`{sql}` is visible to a replica and must bump"
			);
		};
		bumps(
			"UPDATE files_meta SET name = 'renamed.txt' WHERE id = ?1;",
			file_id,
			uuid(2),
		);
		bumps(
			"UPDATE files_meta SET hash = x'AB' WHERE id = ?1;",
			file_id,
			uuid(2),
		);
		bumps(
			"UPDATE files SET favorite_rank = 5 WHERE id = ?1;",
			file_id,
			uuid(2),
		);
		bumps(
			"UPDATE files SET size = 128 WHERE id = ?1;",
			file_id,
			uuid(2),
		);
		bumps(
			"UPDATE dirs SET color = 'red' WHERE id = ?1;",
			dir_id,
			uuid(1),
		);
		bumps(
			"UPDATE dirs_meta SET name = 'renamed' WHERE id = ?1;",
			dir_id,
			uuid(1),
		);
		bumps(
			"UPDATE items SET trashed = TRUE WHERE id = ?1;",
			file_id,
			uuid(2),
		);
		bumps(
			"UPDATE items SET parent = x'08080808080808080808080808080808' WHERE id = ?1;",
			file_id,
			uuid(2),
		);

		let stamp = seq_of(&conn, uuid(2));
		let served = anchor(&conn);
		conn.execute(
			"UPDATE items
			SET is_stale = TRUE, is_recent = TRUE, local_data = '{}', materialised_at = 5
			WHERE id = ?1;",
			[file_id],
		)
		.unwrap();
		conn.execute("UPDATE dirs SET last_listed = 99 WHERE id = ?1;", [dir_id])
			.unwrap();
		assert_eq!(
			(seq_of(&conn, uuid(2)), anchor(&conn)),
			(stamp, served),
			"local state is not something a replica is ever shown"
		);
	}

	/// The stamp is only worth maintaining if it reaches the caller: out of the row, through the
	/// wide join, into the object the FFI hands over. This is the whole data path for
	/// `metadataVersion`.
	#[test]
	fn a_rename_moves_the_change_seq_the_queried_object_carries() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "before.txt");
		add_dir(&conn, uuid(3), parent, "before");

		let queried_seq =
			|uuid_: Uuid| match FfiObject::from(DBObject::select(&conn, uuid_).unwrap()) {
				FfiObject::File(file) => file.change_seq,
				FfiObject::Dir(dir) => dir.change_seq,
				FfiObject::Root(_) => panic!("a root is not part of the feed"),
			};

		let (file_before, dir_before) = (queried_seq(uuid(1)), queried_seq(uuid(3)));
		assert_eq!(
			file_before,
			seq_of(&conn, uuid(1)),
			"the object must report the row's own stamp, not something derived"
		);

		conn.execute(
			"UPDATE files_meta SET name = 'after.txt' WHERE id = ?1;",
			[id_of(&conn, uuid(1))],
		)
		.unwrap();

		assert!(
			queried_seq(uuid(1)) > file_before,
			"a rename is a change a replica has to be told about"
		);
		assert_eq!(
			queried_seq(uuid(3)),
			dir_before,
			"and the sibling nobody touched must stand still"
		);
	}

	/// The stamp a mutation hands straight back has to be the one the row ended up with. It cannot
	/// come from a RETURNING clause — those are evaluated before the AFTER triggers that do the
	/// stamping, and the per-type tables bump the item again after the `items` row is written.
	#[test]
	fn an_upserted_object_reports_the_stamp_it_ended_up_with() {
		let mut conn = db();
		let file = DBFile::upsert_from_remote(
			&mut conn,
			remote_file(uuid(1), stable(2), ParentUuid::Uuid(uuid(9)), "a.txt"),
		)
		.unwrap();

		assert!(
			file.change_seq > 0,
			"a fresh row is stamped, not left sitting at the column DEFAULT"
		);
		assert_eq!(
			file.change_seq,
			seq_of(&conn, uuid(1)),
			"and the value handed back is the one the database now holds"
		);
	}

	/// The nullable columns are the ones `!=` gets wrong: it yields NULL rather than TRUE when
	/// one side is NULL, so a hash, a creation date or a colour appearing — or vanishing — would
	/// go unreported.
	#[test]
	fn null_and_value_transitions_on_nullable_columns_bump_once_each() {
		let conn = db();
		add_dir(&conn, uuid(1), uuid(9), "sub");
		add_file(&conn, uuid(2), stable(3), uuid(9), "a.txt");
		let (dir_id, file_id) = (id_of(&conn, uuid(1)), id_of(&conn, uuid(2)));

		for (sql, id) in [
			("UPDATE files_meta SET hash = x'AB' WHERE id = ?1;", file_id),
			("UPDATE files_meta SET hash = NULL WHERE id = ?1;", file_id),
			("UPDATE files_meta SET created = 5 WHERE id = ?1;", file_id),
			(
				"UPDATE files_meta SET created = NULL WHERE id = ?1;",
				file_id,
			),
			("UPDATE dirs SET color = 'red' WHERE id = ?1;", dir_id),
			("UPDATE dirs SET color = NULL WHERE id = ?1;", dir_id),
		] {
			let before = anchor(&conn);
			conn.execute(sql, [id]).unwrap();
			assert_eq!(anchor(&conn), before + 1, "`{sql}` must bump exactly once");
			conn.execute(sql, [id]).unwrap();
			assert_eq!(anchor(&conn), before + 1, "`{sql}` again is not a change");
		}
	}

	/// The stale sweeps delete by predicate, so Rust never learns which rows went. The tombstone
	/// is written by the database or it is not written at all — which is the whole reason this
	/// lives in a trigger.
	#[test]
	fn a_positional_sweep_retires_the_rows_it_reaps() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "swept.txt");
		add_dir(&conn, uuid(3), parent, "swept");

		conn.execute(MARK_STALE_WITH_PARENT, [parent]).unwrap();
		conn.execute(DELETE_STALE_WITH_PARENT, [parent]).unwrap();

		let mut got = retired(&conn);
		got.sort();
		assert_eq!(
			got,
			vec![(1, uuid(3)), (2, uuid(2))],
			"a file is retired under its stable id, a dir under its own uuid"
		);
	}

	/// The cascade has to reach every generation, not just the children — which is what the
	/// production pragmas buy and what a reopened connection used to lose.
	#[test]
	fn a_delete_cascade_retires_every_generation() {
		let conn = db();
		add_dir(&conn, uuid(1), uuid(9), "a");
		add_dir(&conn, uuid(2), uuid(1), "b");
		add_file(&conn, uuid(3), stable(4), uuid(2), "c.txt");

		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(1)])
			.unwrap();

		let mut got = retired(&conn);
		got.sort();
		assert_eq!(got, vec![(1, uuid(1)), (1, uuid(2)), (2, uuid(4))]);
	}

	/// Duplicate stable ids are legal — `stable_uuid` is deliberately not UNIQUE — so an id is
	/// only gone once the last row carrying it is. Retiring it early tells a replica to drop a
	/// file that is still right there.
	#[test]
	fn a_stable_id_is_retired_only_when_its_last_row_goes() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(9), parent, "a.txt");
		// The upsert would reconcile a second record onto the first row by design, so seed the
		// abuse-shaped sibling with a raw insert.
		conn.execute(
			"INSERT INTO items (uuid, stable_uuid, parent, type) VALUES (?1, ?2, ?3, 2);",
			rusqlite::params![uuid(2), stable(9), parent],
		)
		.unwrap();

		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(1)])
			.unwrap();
		assert!(
			retired(&conn).is_empty(),
			"the id still resolves, so it is not retired"
		);

		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(2)])
			.unwrap();
		assert_eq!(retired(&conn), vec![(2, uuid(9))]);
	}

	/// A tombstone exists exactly while its id resolves to nothing, so anything that adopts an id
	/// clears it. The dir-uuid cascade speculatively deletes children the server may still have —
	/// this is what makes the next listing undo that.
	#[test]
	fn re_adopting_an_id_clears_its_tombstone() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "a.txt");
		add_dir(&conn, uuid(3), parent, "sub");
		conn.execute(
			"DELETE FROM items WHERE uuid IN (?1, ?2);",
			rusqlite::params![uuid(1), uuid(3)],
		)
		.unwrap();
		assert_eq!(retired(&conn).len(), 2);

		// The same file back under a re-minted uuid, and the same dir back as itself.
		add_file(&conn, uuid(4), stable(2), parent, "a.txt");
		assert_eq!(
			retired(&conn),
			vec![(1, uuid(3))],
			"the file's stable id resolves again; the dir's uuid still does not"
		);
		add_dir(&conn, uuid(3), parent, "sub");
		assert!(retired(&conn).is_empty());
	}

	/// The `(parent, name)` tier resolves an incoming record onto a row that answers to a
	/// different pair of ids and overwrites both in place. No DELETE ever fires, so without this
	/// trigger the overwritten stable id would simply go stale — a phantom the replica keeps
	/// forever.
	#[test]
	fn the_name_tier_upsert_retires_the_id_it_overwrites() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "a.txt");

		// Another file that took over the name, arriving with ids the cache has never held.
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_file_item_with_stmts(
			uuid(3),
			stable(4),
			combine_parent(Some(parent), false),
			Some("a.txt"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert_eq!(retired(&conn), vec![(2, uuid(2))]);
	}

	/// A versioning-disabled edit lands as a re-minted uuid on an unchanged stable id. That is the
	/// same provider identity with new content, not a retirement — tombstoning it would tell the
	/// replica to drop the file the user just edited.
	#[test]
	fn a_uuid_re_mint_with_an_unchanged_stable_id_is_not_a_retirement() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "edited.txt");
		let before = seq_of(&conn, uuid(1));

		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		item::upsert_file_item_with_stmts(
			uuid(3),
			stable(2),
			combine_parent(Some(parent), false),
			Some("edited.txt"),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);

		assert!(seq_of(&conn, uuid(3)) > before, "new content is a change");
		assert!(
			retired(&conn).is_empty(),
			"but the provider id never moved, so nothing was retired"
		);
	}

	/// A tombstone is an authoritative delete instruction. Handing one out for the only copy of
	/// bytes that are not on the server evicts them; the row surfaces as a phantom instead, until
	/// the next listing reconciles it.
	#[test]
	fn a_row_holding_the_only_copy_of_bytes_is_never_retired() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "unsent.txt");
		mark_pending_upload(&conn, stable(2), 1).unwrap();
		let served = anchor(&conn);

		conn.execute("DELETE FROM items WHERE parent = ?1;", [parent])
			.unwrap();

		assert!(retired(&conn).is_empty());
		assert_eq!(
			anchor(&conn),
			served,
			"and nothing was recorded, so no sequence may have been spent — the meta rows dying \
			 with their item are not a change to anything"
		);
	}

	/// Bytes in the cache directory are a COPY of what the server holds, so a delete evicting them
	/// is the whole point. Suppressing the tombstone on that marker instead would make every file
	/// the user ever opened impossible to delete from a replica's point of view — the stale sweeps
	/// take the row and the replica keeps the phantom forever.
	#[test]
	fn a_materialised_file_is_retired_like_any_other() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "downloaded.txt");
		mark_materialised(&conn, uuid(1), 1).unwrap();
		let served = anchor(&conn);

		conn.execute(MARK_STALE_WITH_PARENT, [parent]).unwrap();
		conn.execute(DELETE_STALE_WITH_PARENT, [parent]).unwrap();

		assert_eq!(retired(&conn), vec![(2, uuid(2))]);
		assert!(
			anchor(&conn) > served,
			"and a replica holding {served} must be told about it"
		);
	}

	/// A uuid arriving with a different type is the server reassigning it, and the old row is
	/// retired outright. The dir it was loses its lifetime id; the file that replaces it does not
	/// gain one from the uuid — a file's provider id is its stable id alone.
	#[test]
	fn a_cross_type_uuid_reuse_retires_the_dir_and_not_the_new_file() {
		let conn = db();
		let parent = uuid(9);
		add_dir(&conn, uuid(1), parent, "victim");

		add_file(&conn, uuid(1), stable(2), parent, "victim");

		assert_eq!(
			retired(&conn),
			vec![(1, uuid(1))],
			"the dir's uuid is retired and the file's reuse of it must not clear that"
		);
		assert!(seq_of(&conn, uuid(1)) > 0, "the new file is stamped");
	}

	/// Roots are local scaffolding — no replica ever sees one, so they are outside the feed at
	/// both ends, including the `dirs` row `insert_root` gives them.
	#[test]
	fn roots_are_outside_the_feed() {
		let mut conn = db();
		let served = anchor(&conn);

		insert_root(&mut conn, uuid(1)).unwrap();
		assert_eq!(anchor(&conn), served, "creating a root is not a change");
		assert_eq!(seq_of(&conn, uuid(1)), 0);

		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(1)])
			.unwrap();
		assert_eq!(anchor(&conn), served, "and neither is dropping one");
		assert!(retired(&conn).is_empty());
	}

	/// An anchor is only useful if nothing can slip in below it. The counter is raised before the
	/// tombstone is written precisely so a delete lands above every anchor already served.
	#[test]
	fn a_tombstone_lands_above_the_anchor_served_before_the_delete() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), uuid(9), "a.txt");
		let served = anchor(&conn);

		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(1)])
			.unwrap();

		let seq: i64 = conn
			.query_row("SELECT seq FROM tombstones;", [], |r| r.get(0))
			.unwrap();
		assert!(
			seq > served,
			"a replica holding {served} would never be told about the delete"
		);
		assert_eq!(
			anchor(&conn),
			seq,
			"and the counter must not have run ahead of it"
		);
	}

	/// The uuids a diff carries, sorted — a diff is a set, and its order is the sequence it was
	/// stamped in, which is not what any of these assertions are about.
	fn updated_uuids(changes: &FfiChanges) -> Vec<String> {
		let mut uuids: Vec<String> = changes
			.updated
			.iter()
			.map(|obj| match obj {
				FfiObject::File(file) => file.uuid.clone(),
				FfiObject::Dir(dir) => dir.uuid.clone(),
				FfiObject::Root(root) => root.uuid.clone(),
			})
			.collect();
		uuids.sort();
		uuids
	}

	fn sorted(mut ids: Vec<String>) -> Vec<String> {
		ids.sort();
		ids
	}

	/// The provider id a replica knows an item by, for both kinds.
	fn provider_id(id: Uuid) -> String {
		format!("stable/{id}")
	}

	/// The round trip the whole substrate exists for: hand out an anchor, do some work, and be
	/// told exactly that work and nothing else — then hand back the anchor that came with it and
	/// be told nothing at all.
	#[test]
	fn an_anchor_and_the_diff_that_follows_it_round_trip() {
		let conn = db();
		let parent = uuid(9);
		let served = current_sync_anchor(&conn).unwrap();

		add_dir(&conn, uuid(1), parent, "sub");
		add_file(&conn, uuid(2), stable(3), uuid(1), "a.txt");

		let changes = changes_since(&conn, Some(&served)).unwrap();
		assert_eq!(
			updated_uuids(&changes),
			vec![uuid(1).to_string(), uuid(2).to_string()],
			"both new items, and only them"
		);
		assert!(
			changes.deleted_ids.is_empty(),
			"nothing was retired: {:?}",
			changes.deleted_ids
		);
		assert!(!changes.more, "a diff is served whole");

		let served = changes.anchor;
		let changes = changes_since(&conn, Some(&served)).unwrap();
		assert!(
			changes.updated.is_empty() && changes.deleted_ids.is_empty(),
			"asking again after nothing happened must return nothing: {changes:?}"
		);
		assert_eq!(
			changes.anchor, served,
			"and the anchor must stand still with the counter"
		);

		conn.execute(
			"UPDATE files_meta SET name = 'b.txt' WHERE id = ?1;",
			[id_of(&conn, uuid(2))],
		)
		.unwrap();

		let changes = changes_since(&conn, Some(&served)).unwrap();
		assert_eq!(
			updated_uuids(&changes),
			vec![uuid(2).to_string()],
			"a rename returns the renamed item, and not the directory it sits in"
		);
		match &changes.updated[0] {
			FfiObject::File(file) => assert_eq!(
				file.meta.as_ref().unwrap().name,
				"b.txt",
				"and it carries the new name, not the one the replica already had"
			),
			other => panic!("expected the file, got {other:?}"),
		}
	}

	/// A retired id is served as an instruction to drop it, under the same `stable/` namespace
	/// every other id crosses the FFI in — a file under its stable id, a dir under its uuid. And
	/// because a tombstone lives exactly as long as its id resolves to nothing, re-adopting the id
	/// takes the instruction back within the same diff.
	#[test]
	fn a_retired_id_is_served_as_a_deletion_and_a_re_adoption_takes_it_back() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "a.txt");
		add_dir(&conn, uuid(3), parent, "sub");
		let served = current_sync_anchor(&conn).unwrap();

		conn.execute(
			"DELETE FROM items WHERE uuid IN (?1, ?2);",
			rusqlite::params![uuid(1), uuid(3)],
		)
		.unwrap();

		let changes = changes_since(&conn, Some(&served)).unwrap();
		assert_eq!(
			sorted(changes.deleted_ids),
			sorted(vec![provider_id(uuid(2)), provider_id(uuid(3))]),
			"the file by its stable id, the dir by its uuid"
		);
		assert!(changes.updated.is_empty(), "nothing is left to render");

		// The file back under a re-minted uuid, the dir back as itself.
		add_file(&conn, uuid(4), stable(2), parent, "a.txt");
		add_dir(&conn, uuid(3), parent, "sub");

		let changes = changes_since(&conn, Some(&served)).unwrap();
		assert!(
			changes.deleted_ids.is_empty(),
			"both ids resolve again, so neither may still be served as deleted: {:?}",
			changes.deleted_ids
		);
		assert_eq!(
			updated_uuids(&changes),
			vec![uuid(3).to_string(), uuid(4).to_string()],
			"they come back as items to render instead"
		);
	}

	/// A replica with no anchor holds nothing: everything live is new to it, and every retired id
	/// names something it never heard of. Telling it to delete those would be noise at best.
	#[test]
	fn a_replica_with_no_anchor_is_handed_everything_live_and_nothing_to_delete() {
		let mut conn = db();
		let parent = uuid(9);
		insert_root(&mut conn, uuid(8)).unwrap();
		add_file(&conn, uuid(1), stable(2), parent, "a.txt");
		add_dir(&conn, uuid(3), parent, "sub");
		add_file(&conn, uuid(4), stable(5), parent, "gone.txt");
		conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(4)])
			.unwrap();
		assert_eq!(retired(&conn).len(), 1, "there is a tombstone to withhold");

		let changes = changes_since(&conn, None).unwrap();

		assert_eq!(
			updated_uuids(&changes),
			vec![uuid(1).to_string(), uuid(3).to_string()],
			"every live item, and the root is not one of them"
		);
		assert!(
			changes.deleted_ids.is_empty(),
			"a replica holding nothing has nothing to drop: {:?}",
			changes.deleted_ids
		);
		assert_eq!(
			changes.anchor,
			current_sync_anchor(&conn).unwrap(),
			"and it leaves holding the current anchor"
		);
	}

	/// An anchor names a sequence in one particular incarnation of one particular database. Handed
	/// anything else, the answer has to be the typed expiry the provider turns into a full
	/// re-enumeration — never a silently under-reported diff.
	#[test]
	fn an_anchor_this_database_did_not_issue_is_expired() {
		let conn = db();
		add_file(&conn, uuid(1), stable(2), uuid(9), "a.txt");
		let ours = current_sync_anchor(&conn).unwrap();
		// A second database is a second `randomblob(16)`, which is exactly what a wipe mints.
		let theirs = current_sync_anchor(&db()).unwrap();

		for (label, anchor) in [
			("an anchor from another database", theirs.as_slice()),
			("an empty anchor", &[]),
			("a truncated anchor", &ours[..ours.len() - 1]),
			("an over-long anchor", &[ours.as_slice(), &[0]].concat()),
		] {
			assert!(
				matches!(
					changes_since(&conn, Some(anchor)),
					Err(CacheError::SyncAnchorExpired(_))
				),
				"{label} must expire rather than be honoured"
			);
		}

		assert!(
			changes_since(&conn, Some(&ours)).is_ok(),
			"and our own must still be good"
		);
	}

	fn materialised_at_of(conn: &Connection, uuid_: Uuid) -> Option<i64> {
		conn.query_row(
			"SELECT materialised_at FROM items WHERE uuid = ?1;",
			[uuid_],
			|r| r.get(0),
		)
		.unwrap()
	}

	fn favorite(conn: &Connection, uuid_: Uuid, table: &str, rank: i64) {
		conn.execute(
			&format!("UPDATE {table} SET favorite_rank = ?2 WHERE id = ?1;"),
			rusqlite::params![id_of(conn, uuid_), rank],
		)
		.unwrap();
	}

	/// Policy B, leg by leg: the working set is the items this device has a stake in — bytes on
	/// disk, an edit that has not gone out, or a favourite — and nothing else. Each leg is put in
	/// and then taken away again, because a membership rule that cannot be revoked is a leak.
	#[test]
	fn the_working_set_is_exactly_what_this_device_has_a_stake_in() {
		let mut conn = db();
		let parent = uuid(9);
		insert_root(&mut conn, uuid(8)).unwrap();
		// A favourited root would satisfy the dir leg if roots were not excluded outright.
		favorite(&conn, uuid(8), "dirs", 7);

		add_file(&conn, uuid(1), stable(11), parent, "downloaded.txt");
		add_file(&conn, uuid(2), stable(12), parent, "unsent.txt");
		add_file(&conn, uuid(3), stable(13), parent, "starred.txt");
		add_dir(&conn, uuid(4), parent, "starred");
		add_file(&conn, uuid(5), stable(15), parent, "plain.txt");
		add_dir(&conn, uuid(6), parent, "plain");

		mark_materialised(&conn, uuid(1), 1_700_000_000_000).unwrap();
		mark_pending_upload(&conn, stable(12), 1_700_000_000_000).unwrap();
		favorite(&conn, uuid(3), "files", 5);
		favorite(&conn, uuid(4), "dirs", 5);

		let members = |conn: &Connection| {
			let mut uuids: Vec<String> = working_set(conn)
				.unwrap()
				.iter()
				.map(|obj| match obj {
					FfiObject::File(file) => file.uuid.clone(),
					FfiObject::Dir(dir) => dir.uuid.clone(),
					FfiObject::Root(root) => root.uuid.clone(),
				})
				.collect();
			uuids.sort();
			uuids
		};

		assert_eq!(
			members(&conn),
			vec![
				uuid(1).to_string(),
				uuid(2).to_string(),
				uuid(3).to_string(),
				uuid(4).to_string(),
			],
			"each leg puts its item in, and the plain pair stays out"
		);

		clear_materialised(&conn, uuid(1)).unwrap();
		clear_pending_upload(&conn, stable(12)).unwrap();
		favorite(&conn, uuid(3), "files", 0);
		favorite(&conn, uuid(4), "dirs", 0);

		assert!(
			members(&conn).is_empty(),
			"every leg must be revocable, and the favourited root is still not a member: {:?}",
			members(&conn)
		);
	}

	/// The marker tracks the bytes: set while they are in the cache directory, dropped when they
	/// leave it. Clearing one that was never set is not a write — every deletion path funnels
	/// through one choke point, dirs included, and only files can carry one at all.
	#[test]
	fn a_cached_copy_is_recorded_for_exactly_as_long_as_its_bytes_are_there() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(2), parent, "a.txt");
		add_dir(&conn, uuid(3), parent, "sub");

		mark_materialised(&conn, uuid(1), 100).unwrap();
		assert_eq!(materialised_at_of(&conn, uuid(1)), Some(100));
		mark_materialised(&conn, uuid(1), 200).unwrap();
		assert_eq!(
			materialised_at_of(&conn, uuid(1)),
			Some(200),
			"re-downloading the same file moves the marker to the new copy"
		);

		clear_materialised(&conn, uuid(1)).unwrap();
		assert_eq!(materialised_at_of(&conn, uuid(1)), None);
		clear_materialised(&conn, uuid(1)).unwrap();
		clear_materialised(&conn, uuid(3)).unwrap();
		assert_eq!(
			materialised_at_of(&conn, uuid(3)),
			None,
			"clearing a directory's uuid is a no-op, not an error"
		);
	}

	/// The sweeps delete cache slots by path and cannot write to the database, so the marker is
	/// reconciled against a listing of the directory afterwards. The listing is a snapshot, which
	/// is why the timestamp is part of the rule: a file materialised after it was taken is missing
	/// from it only because it did not exist yet.
	#[test]
	fn the_reconciliation_drops_only_what_the_listing_proves_is_gone() {
		let conn = db();
		let parent = uuid(9);
		add_file(&conn, uuid(1), stable(11), parent, "kept.txt");
		add_file(&conn, uuid(2), stable(12), parent, "swept.txt");
		add_file(&conn, uuid(3), stable(13), parent, "just-arrived.txt");

		mark_materialised(&conn, uuid(1), 100).unwrap();
		mark_materialised(&conn, uuid(2), 100).unwrap();
		mark_materialised(&conn, uuid(3), 300).unwrap();

		// The listing was taken at 200 and found only the first file's slot.
		clear_materialised_not_in_cache(&conn, [UuidStr::from(uuid(1))].into_iter(), 200).unwrap();

		assert_eq!(
			materialised_at_of(&conn, uuid(1)),
			Some(100),
			"its slot is still on disk"
		);
		assert_eq!(
			materialised_at_of(&conn, uuid(2)),
			None,
			"its slot is gone, so the row must stop claiming a local copy"
		);
		assert_eq!(
			materialised_at_of(&conn, uuid(3)),
			Some(300),
			"it was materialised after the listing, which is why it is not in it"
		);

		let served = anchor(&conn);
		clear_materialised_not_in_cache(&conn, [].into_iter(), 400).unwrap();
		assert_eq!(
			(
				materialised_at_of(&conn, uuid(1)),
				materialised_at_of(&conn, uuid(3))
			),
			(None, None),
			"an empty cache directory clears everything it predates"
		);
		assert_eq!(
			anchor(&conn),
			served,
			"and none of this is a change a replica is ever shown"
		);
	}
}
