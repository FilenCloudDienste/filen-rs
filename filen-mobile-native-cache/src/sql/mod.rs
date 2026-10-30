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

/// Deletes an item's row, and with it — through `cascade_on_delete_delete_children` — every
/// non-trashed row below it.
///
/// The pending-upload markers of everything that delete reaches are dropped as part of it, in the
/// same transaction, because `tombstone_on_delete` reads `pending_upload_at` off each row as it
/// goes: a row that still claims an unsent edit dies without a tombstone, and every replica keeps
/// the phantom forever. Folded in here rather than left to the callers precisely so the ordering
/// cannot be reversed by a later edit — the clear has to be visible to the trigger, which means
/// before the DELETE, and the transaction is what stops a crash in between from leaving a live row
/// whose outstanding edit nothing records any more.
///
/// This is the one delete for which dropping the markers is right: it runs after the server has
/// permanently deleted the item, so there is nothing left to upload them to. The positional stale
/// sweeps go through no helper at all and must keep the suppression — a listing that stopped
/// mentioning an item is not proof the bytes are safe anywhere else — and `forget_item` checks for
/// them itself and keeps the whole row instead.
///
/// `materialised_at` is deliberately untouched: it is not part of the trigger's guard, because
/// those bytes are a copy of what the server holds and evicting them along with the item is the
/// point.
///
/// The sequences the delete spent are logged as a permanent deletion (see
/// [`is_permanently_deleted`]): every caller runs it on the server's word that the item is gone
/// for good. Nothing but this delete's triggers can move the counter inside the transaction.
pub(crate) fn delete_item(conn: &mut Connection, item_uuid: Uuid) -> Result<(), rusqlite::Error> {
	let tx = conn.transaction()?;
	tx.prepare_cached(CLEAR_PENDING_UPLOAD_SUBTREE)?
		.execute([item_uuid])?;
	let (_, before) = select_change_meta(&tx)?;
	tx.prepare_cached(DELETE_BY_UUID)?.execute([item_uuid])?;
	let (_, after) = select_change_meta(&tx)?;
	if after != before
		&& tx
			.prepare_cached(EXTEND_PERMANENT_DELETION)?
			.execute([before, after])?
			== 0
	{
		tx.prepare_cached(INSERT_PERMANENT_DELETION)?
			.execute([before, after])?;
	}
	tx.commit()
}

/// Whether `id`, a provider id of kind `kind` (a dir's uuid, a file's stable id), was retired by
/// [`delete_item`] and still names nothing, so the server's word on it is already known: gone.
/// See [`statements::SELECT_IS_PERMANENTLY_DELETED`] for what lifts it.
pub(crate) fn is_permanently_deleted(
	conn: &Connection,
	kind: ItemType,
	id: Uuid,
) -> Result<bool, rusqlite::Error> {
	conn.prepare_cached(SELECT_IS_PERMANENTLY_DELETED)?
		.query_one((kind, id), |row| row.get(0))
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

/// Drops the pending-upload marker from exactly the row `uuid` names.
///
/// The stable-scoped `clear_pending_upload` prefers the live half of a duplicate stable pair;
/// callers that have already resolved which row they mean (a probe answer, a fresh upsert) go
/// through this instead so the release cannot land on the wrong row.
pub(crate) fn clear_pending_upload_by_uuid(
	conn: &Connection,
	uuid: Uuid,
) -> Result<usize, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(CLEAR_PENDING_UPLOAD_BY_UUID)?;
	stmt.execute([uuid])
}

/// Drops the pending-upload markers from a row and everything deleting it would take with it,
/// without deleting anything.
///
/// For lineages the server has confirmed permanently gone: the descendant markers are what spare
/// the doomed row from the stale sweeps every round, so releasing the subtree is what lets the
/// sweep finally reap it. Same walk as the release folded into `delete_item`.
pub(crate) fn clear_pending_upload_subtree(
	conn: &Connection,
	item_uuid: Uuid,
) -> Result<usize, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(CLEAR_PENDING_UPLOAD_SUBTREE)?;
	stmt.execute([item_uuid])
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

/// Installs the slot-remint log (see [`statements::INSTALL_SLOT_REMINT_LOG`]) on a connection.
/// Must run AFTER the schema exists — the temp trigger references `items` — so every open path
/// calls it right after `init.sql` runs or right after reopening an existing database.
pub(crate) fn install_slot_remint_log(conn: &Connection) -> Result<(), rusqlite::Error> {
	conn.execute_batch(INSTALL_SLOT_REMINT_LOG)
}

/// Every logged remint, oldest first — insertion order is what makes a chain of re-mints
/// (a → b, then b → c) resolvable by renaming in sequence.
pub(crate) fn select_slot_remints(
	conn: &Connection,
) -> Result<Vec<(i64, Uuid, Uuid)>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_SLOT_REMINTS)?;
	stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
		.collect()
}

pub(crate) fn delete_slot_remint(conn: &Connection, id: i64) -> Result<(), rusqlite::Error> {
	let mut stmt = conn.prepare_cached(DELETE_SLOT_REMINT)?;
	stmt.execute([id])?;
	Ok(())
}

/// Whether an unrescued remint still points at this uuid — i.e. the row's bytes may be sitting
/// in a slot under a retired uuid. The drain must not read "no local copy" as "nothing left to
/// upload" while this holds.
pub(crate) fn slot_remint_targets_uuid(
	conn: &Connection,
	uuid: Uuid,
) -> Result<bool, rusqlite::Error> {
	conn.query_one(SLOT_REMINT_TARGETS_UUID, [uuid], |row| row.get(0))
}

/// The id of this incarnation of the database and the sequence it has reached, as one read.
///
/// The two belong together: a sequence only means anything against the instance that issued it,
/// which is what makes an anchor from before a wipe recognisable as expired.
pub(crate) fn select_change_meta(conn: &Connection) -> Result<([u8; 16], i64), rusqlite::Error> {
	conn.query_one(SELECT_CHANGE_META, [], |row| Ok((row.get(0)?, row.get(1)?)))
}

/// Every item stamped above `seq_floor` and at or below `seq_cutoff` — what a replica holding
/// that sequence has not been shown, bounded to one page (`i64::MAX` for the whole diff).
pub(crate) fn select_changed_items(
	conn: &Connection,
	seq_floor: i64,
	seq_cutoff: i64,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGED_ITEMS)?;
	stmt.query_and_then([seq_floor, seq_cutoff], DBNonRootObject::from_row)?
		.collect::<SQLResult<Vec<_>>>()
}

/// Every provider id retired above `seq_floor` and at or below `seq_cutoff`, oldest first.
pub(crate) fn select_retired_ids(
	conn: &Connection,
	seq_floor: i64,
	seq_cutoff: i64,
) -> Result<Vec<Uuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_RETIRED_IDS)?;
	stmt.query_map([seq_floor, seq_cutoff], |row| row.get(0))?
		.collect::<Result<Vec<_>, _>>()
}

/// Up to `limit` live sequences above `seq_floor`, items and tombstones pooled, oldest first —
/// how a paged diff picks its cutoff (see [`statements::SELECT_CHANGE_SEQS_PAGE`]).
pub(crate) fn select_change_seqs_page(
	conn: &Connection,
	seq_floor: i64,
	limit: i64,
) -> Result<Vec<i64>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_CHANGE_SEQS_PAGE)?;
	stmt.query_map([seq_floor, limit], |row| row.get(0))?
		.collect::<Result<Vec<_>, _>>()
}

/// The items this device has a stake in: bytes on disk, an edit waiting to go out, a favourite —
/// or a seat in one of `containers`, the materialized directories the system reported (the
/// account root included, injected by the caller). See `select_working_set.sql`.
pub(crate) fn select_working_set<I>(
	conn: &Connection,
	containers: I,
) -> SQLResult<Vec<DBNonRootObject>>
where
	I: ExactSizeIterator<Item = UuidStr>,
{
	let mut stmt = conn.prepare_cached(SELECT_WORKING_SET)?;
	stmt.query_and_then([uuids_json_array(containers)], DBNonRootObject::from_row)?
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
/// stale is deleted. `spare` rows are exempted from the deletion — the caller asked the server
/// what became of them and got no definitive answer (see `reconcile_missing_subdirs`).
pub(crate) fn update_items_with_parent<I, I1>(
	conn: &mut Connection,
	dirs: I,
	files: I1,
	parent: Uuid,
	spare: &[Uuid],
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

		unmark_spared(&tx, spare)?;

		// "Gone from this listing" must never destroy the only record of an
		// unsent edit: spare marker-holding rows and the dirs sheltering them
		// (they stay behind as phantoms until the drain sends the edit).
		let mut stmt = tx.prepare_cached(UNMARK_STALE_PENDING_WITH_PARENT)?;
		stmt.execute([parent])?;

		let mut stmt = tx.prepare_cached(DELETE_STALE_WITH_PARENT)?;
		stmt.execute([parent])?;
	}
	tx.commit()?;
	Ok(())
}

/// Refreshes the cached trash listing. Trashed items keep their original `parent`, so the sweep
/// is scoped by the `trashed` flag rather than by a parent uuid. Each item's own parent is
/// `ParentUuid::Trash`, which the upsert decomposes back into `(original parent, trashed = 1)`.
/// `spare` rows are exempted from the deletion, as in [`update_items_with_parent`].
pub(crate) fn update_trashed_items<I, I1>(
	conn: &mut Connection,
	dirs: I,
	files: I1,
	spare: &[Uuid],
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

		unmark_spared(&tx, spare)?;

		// "Gone from the trash listing" must never destroy the only record of an
		// unsent edit, exactly like the parent-scoped sweep above: spare
		// marker-holding rows and the trashed dirs sheltering them.
		let mut stmt = tx.prepare_cached(UNMARK_STALE_PENDING_TRASHED)?;
		stmt.execute([])?;

		let mut stmt = tx.prepare_cached(DELETE_STALE_TRASHED)?;
		stmt.execute([])?;
	}
	tx.commit()?;
	Ok(())
}

/// Clears the stale mark on rows a sweep must not delete this round.
fn unmark_spared(tx: &rusqlite::Transaction<'_>, spare: &[Uuid]) -> Result<(), rusqlite::Error> {
	if spare.is_empty() {
		return Ok(());
	}
	let mut stmt = tx.prepare_cached(UNMARK_STALE_UUID)?;
	for uuid in spare {
		stmt.execute([uuid])?;
	}
	Ok(())
}

/// The non-trashed child `name` names under `parent`, as the cache holds it.
pub(crate) fn select_child_by_name(
	conn: &Connection,
	parent: Uuid,
	name: &str,
) -> Result<Option<object::DBObject>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_ITEM_BY_PARENT_NAME)?;
	stmt.query_one((parent, name), item::RawDBItem::from_row)
		.optional()?
		.map(|item| item.into_db_object(conn))
		.transpose()
}

/// Every directory's `last_listed`, keyed by uuid — the stamps a reconcile pass
/// orders itself by (see [`crate::live`]). One query rather than one per
/// container: the whole table is a few thousand rows on a large account, and a
/// pass reads it once.
pub(crate) fn select_dir_last_listed(
	conn: &Connection,
) -> Result<std::collections::HashMap<Uuid, i64>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(statements::SELECT_DIR_LAST_LISTED)?;
	let rows = stmt.query_map([], |row| {
		Ok((row.get::<_, Uuid>(0)?, row.get::<_, i64>(1)?))
	})?;
	rows.collect()
}

/// The uuids of the non-trashed child DIRECTORIES of `parent` — the candidates a listing-driven
/// sweep would cascade through.
pub(crate) fn select_child_dir_uuids(
	conn: &Connection,
	parent: Uuid,
) -> Result<Vec<Uuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(SELECT_UUID_TYPE_NAME_BY_PARENT)?;
	// Columns are (uuid, type, display_name); type 1 = dir.
	let rows = stmt.query_map([parent], |row| {
		Ok((row.get::<_, Uuid>(0)?, row.get::<_, i64>(1)?))
	})?;
	rows.filter_map(|row| match row {
		Ok((uuid, 1)) => Some(Ok(uuid)),
		Ok(_) => None,
		Err(e) => Some(Err(e)),
	})
	.collect()
}

/// The whole-life ids of the non-trashed child FILES of `parent` — the candidates a
/// listing-driven sweep would retire. Stable ids rather than uuids, because that is the identity
/// a listing matches files on: an edited head arrives under a fresh uuid but the same stable id,
/// and must not read as missing.
pub(crate) fn select_child_file_stables(
	conn: &Connection,
	parent: Uuid,
) -> Result<Vec<StableUuid>, rusqlite::Error> {
	let mut stmt = conn.prepare_cached(
		"SELECT items.stable_uuid FROM items
		WHERE items.parent = ?1 AND items.trashed = FALSE AND items.type = 2
		AND items.stable_uuid IS NOT NULL;",
	)?;
	stmt.query_map([parent], |row| row.get(0))?.collect()
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

pub(crate) fn select_children_page(
	conn: &Connection,
	order_by: Option<&str>,
	parent: Uuid,
	limit: u32,
	offset: u32,
) -> SQLResult<Vec<DBNonRootObject>> {
	let mut stmt = conn.prepare(&statements::select_dir_children_page(order_by))?;
	stmt.query_and_then(
		rusqlite::params![parent, limit, offset],
		DBNonRootObject::from_row,
	)?
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
mod pending_upload_tests;

#[cfg(test)]
mod trashed_listing_tests;

#[cfg(test)]
mod change_tracking_tests;

#[cfg(test)]
mod slot_remint_tests;

#[cfg(test)]
mod working_set_predicate_tests;
