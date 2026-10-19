use std::collections::HashMap;

use filen_sdk_rs::cache::open_sqlite_in_memory;

use super::*;
use crate::sql::item::{self, combine_parent};

const MARKED_AT: i64 = 1_700_000_000_000;

fn db() -> Connection {
	let conn = open_sqlite_in_memory().unwrap();
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
fn add_file(conn: &Connection, uuid_: Uuid, stable: StableUuid, parent: Uuid, name: &str) -> i64 {
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

/// The reconcile release acts on a row it has already resolved — the probed trashed
/// phantom. The stable-scoped clear tie-breaks toward the live duplicate, which is
/// exactly the row whose marker must survive; the by-uuid clear must hit only the
/// named row, whichever side of the tie-break it sits on.
#[test]
fn clearing_by_uuid_hits_exactly_the_named_row() {
	let conn = db();
	add_file(&conn, uuid(1), stable(9), uuid(0), "a.txt");
	mark_pending_upload(&conn, stable(9), 100).unwrap();
	// The abuse-shaped trashed sibling sharing the stable id, with its own marker —
	// the row the stable-scoped clear would pass over.
	conn.execute(
		"INSERT INTO items (uuid, stable_uuid, parent, type, trashed, pending_upload_at)
			 VALUES (?1, ?2, ?3, 2, TRUE, 50);",
		rusqlite::params![uuid(2), stable(9), uuid(0)],
	)
	.unwrap();

	clear_pending_upload_by_uuid(&conn, uuid(2)).unwrap();
	assert_eq!(
		pending_upload_at_of(&conn, uuid(2)),
		None,
		"the named row's marker is cleared"
	);
	assert_eq!(
		pending_upload_at_of(&conn, uuid(1)),
		Some(100),
		"the live duplicate's marker must survive"
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
