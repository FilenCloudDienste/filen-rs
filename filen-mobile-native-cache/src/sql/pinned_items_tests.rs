use std::collections::HashSet;

use filen_sdk_rs::cache::{open_sqlite, open_sqlite_in_memory};

use super::*;
use crate::sql::item::{self, combine_parent};

const ROOT: u8 = 9;

fn db() -> Connection {
	let conn = open_sqlite_in_memory().unwrap();
	init(&conn);
	conn
}

fn init(conn: &Connection) {
	crate::auth::configure_conn(conn).unwrap();
	conn.execute_batch(INIT).unwrap();
	conn.execute(
		"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
		[uuid(ROOT)],
	)
	.unwrap();
}

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn stable(byte: u8) -> StableUuid {
	StableUuid::new_for_test(uuid(byte))
}

/// A file row; a second call with the same `stable_` and a new `uuid_` is a re-mint.
fn add_file(conn: &Connection, uuid_: u8, stable_: u8, parent: u8, trashed: bool) {
	let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
	let (id, _, _) = item::upsert_file_item_with_stmts(
		uuid(uuid_),
		stable(stable_),
		combine_parent(Some(uuid(parent)), trashed),
		Some("f"),
		None,
		&mut stmt,
	)
	.unwrap();
	drop(stmt);
	conn.execute(
		"INSERT OR IGNORE INTO files (id, size, chunks, region, bucket, timestamp,
				metadata_state, raw_metadata)
			VALUES (?1, 1, 1, 'de-1', 'b', 1, 2, 'enc');",
		[id],
	)
	.unwrap();
}

fn add_dir(conn: &Connection, uuid_: u8, parent: u8, trashed: bool) {
	let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
	let (id, _) = item::upsert_dir_item_with_stmts(
		uuid(uuid_),
		combine_parent(Some(uuid(parent)), trashed),
		Some("d"),
		None,
		&mut stmt,
	)
	.unwrap();
	drop(stmt);
	conn.execute(
		"INSERT INTO dirs (id, color, timestamp, metadata_state, raw_metadata)
			VALUES (?1, 'default', 1, 2, 'enc');",
		[id],
	)
	.unwrap();
}

fn covered(conn: &Connection) -> HashSet<Uuid> {
	select_pinned_file_uuids(conn).unwrap()
}

fn set(bytes: &[u8]) -> HashSet<Uuid> {
	bytes.iter().map(|byte| uuid(*byte)).collect()
}

/// A dir pin covers every file below it, at any depth, and nothing beside it.
#[test]
fn a_dir_pin_covers_its_whole_subtree() {
	let conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_dir(&conn, 2, 1, false);
	add_file(&conn, 10, 110, 1, false);
	add_file(&conn, 11, 111, 2, false);
	add_file(&conn, 12, 112, ROOT, false);

	set_pinned(&conn, uuid(1), true).unwrap();

	assert_eq!(covered(&conn), set(&[10, 11]));
	assert_eq!(select_pinned_item_ids(&conn).unwrap(), vec![uuid(1)]);
}

/// A file pin is keyed by the stable id, so the re-mint a remote content edit makes keeps it,
/// and the sweep exemption follows the slot to the new uuid.
#[test]
fn a_file_pin_follows_a_remint() {
	let conn = db();
	add_file(&conn, 10, 110, ROOT, false);
	set_pinned(&conn, uuid(10), true).unwrap();

	add_file(&conn, 20, 110, ROOT, false);

	assert_eq!(covered(&conn), set(&[20]));
	assert_eq!(select_pinned_item_ids(&conn).unwrap(), vec![uuid(110)]);
}

/// The trash is not something to keep: a trashed pin covers nothing, and a trashed child ends
/// the walk below a live pin.
#[test]
fn trashed_rows_end_the_walk() {
	let conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_dir(&conn, 2, 1, true);
	add_file(&conn, 10, 110, 2, false);
	add_file(&conn, 11, 111, 1, true);
	add_file(&conn, 12, 112, ROOT, true);
	set_pinned(&conn, uuid(1), true).unwrap();
	set_pinned(&conn, uuid(12), true).unwrap();

	assert!(covered(&conn).is_empty());
}

/// Pinning twice, unpinning twice and nesting a pin inside another are all harmless, and a root
/// row pins nothing.
#[test]
fn pins_are_idempotent_and_a_root_pins_nothing() {
	let conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_file(&conn, 10, 110, 1, false);

	set_pinned(&conn, uuid(1), true).unwrap();
	set_pinned(&conn, uuid(1), true).unwrap();
	set_pinned(&conn, uuid(10), true).unwrap();
	set_pinned(&conn, uuid(ROOT), true).unwrap();
	assert_eq!(covered(&conn), set(&[10]));
	assert_eq!(select_pinned_item_ids(&conn).unwrap().len(), 2);

	set_pinned(&conn, uuid(1), false).unwrap();
	set_pinned(&conn, uuid(1), false).unwrap();
	assert_eq!(
		covered(&conn),
		set(&[10]),
		"the file's own pin outlives the dir's"
	);

	set_pinned(&conn, uuid(10), false).unwrap();
	assert!(covered(&conn).is_empty());
	assert!(select_pinned_item_ids(&conn).unwrap().is_empty());
}

/// What is left to download: covered files with no bytes on the device and no outstanding edit.
#[test]
fn files_to_fetch_skip_what_is_on_the_device() {
	let conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_file(&conn, 10, 110, 1, false);
	add_file(&conn, 11, 111, 1, false);
	add_file(&conn, 12, 112, 1, false);
	add_file(&conn, 13, 113, ROOT, false);
	mark_materialised(&conn, uuid(11), 1).unwrap();
	mark_pending_upload(&conn, stable(112), 1).unwrap();
	set_pinned(&conn, uuid(1), true).unwrap();

	assert_eq!(
		select_pinned_files_to_fetch(&conn).unwrap(),
		vec![uuid(110)]
	);
}

/// Only covered dirs the cache has never listed need a listing.
#[test]
fn dirs_to_list_are_the_unlisted_covered_ones() {
	let conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_dir(&conn, 2, 1, false);
	add_dir(&conn, 3, 1, false);
	add_dir(&conn, 4, ROOT, false);
	conn.execute(
		"UPDATE dirs SET last_listed = 1 WHERE id IN (SELECT id FROM items WHERE uuid IN (?1, ?2));",
		[uuid(1), uuid(3)],
	)
	.unwrap();
	set_pinned(&conn, uuid(1), true).unwrap();

	assert_eq!(select_pinned_dirs_to_list(&conn).unwrap(), vec![uuid(2)]);
}

/// A permanent delete drops the pins of the deleted item and of everything under it, and of
/// nothing else.
#[test]
fn a_permanent_delete_drops_the_subtrees_pins() {
	let mut conn = db();
	add_dir(&conn, 1, ROOT, false);
	add_dir(&conn, 2, 1, false);
	add_file(&conn, 10, 110, 2, false);
	add_file(&conn, 11, 111, ROOT, false);
	for pinned in [1, 2, 10, 11] {
		set_pinned(&conn, uuid(pinned), true).unwrap();
	}

	delete_item(&mut conn, uuid(1)).unwrap();

	assert_eq!(select_pinned_item_ids(&conn).unwrap(), vec![uuid(111)]);
}

/// Pins live outside `init.sql`, so they are on disk and come back on the next connection.
#[test]
fn pins_survive_reopening_the_database() {
	let dir = std::env::temp_dir().join(format!("filen-pins-{}", std::process::id()));
	std::fs::create_dir_all(&dir).unwrap();
	let path = dir.join("cache.db");
	{
		let conn = open_sqlite(&path).unwrap();
		init(&conn);
		add_file(&conn, 10, 110, ROOT, false);
		set_pinned(&conn, uuid(10), true).unwrap();
	}

	let conn = open_sqlite(&path).unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	assert_eq!(covered(&conn), set(&[10]));

	drop(conn);
	std::fs::remove_dir_all(&dir).unwrap();
}
