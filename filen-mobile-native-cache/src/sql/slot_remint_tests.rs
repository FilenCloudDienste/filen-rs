use filen_sdk_rs::cache::open_sqlite_in_memory;

use super::*;
use crate::sql::item::{self, combine_parent};

fn db() -> Connection {
	let conn = open_sqlite_in_memory().unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	conn.execute_batch(INIT).unwrap();
	install_slot_remint_log(&conn).unwrap();
	conn
}

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn stable(byte: u8) -> StableUuid {
	StableUuid::new_for_test(uuid(byte))
}

/// The real re-mint path: the same stable id arriving under a fresh uuid resolves onto the
/// existing row through the stable tier and updates `uuid` in place.
fn upsert_file(conn: &Connection, uuid_: Uuid, stable_: StableUuid, parent: Uuid, name: &str) {
	let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
	item::upsert_file_item_with_stmts(
		uuid_,
		stable_,
		combine_parent(Some(parent), false),
		Some(name),
		None,
		&mut stmt,
	)
	.unwrap();
}

/// The log records exactly the re-mints that can strand bytes: a row carrying a
/// pending-upload marker whose uuid changes. A clean row's re-mint records nothing — its
/// slot holds a superseded server copy the sweep is right to take.
#[test]
fn only_a_pending_rows_remint_is_logged() {
	let conn = db();
	add_root(&conn, uuid(9));
	upsert_file(&conn, uuid(1), stable(2), uuid(9), "pending.txt");
	upsert_file(&conn, uuid(3), stable(4), uuid(9), "clean.txt");
	mark_pending_upload(&conn, stable(2), 1_000).unwrap();

	// Both rows re-mint; only the marked one may be logged.
	upsert_file(&conn, uuid(11), stable(2), uuid(9), "pending.txt");
	upsert_file(&conn, uuid(13), stable(4), uuid(9), "clean.txt");

	assert_eq!(
		select_slot_remints(&conn)
			.unwrap()
			.into_iter()
			.map(|(_, old, new)| (old, new))
			.collect::<Vec<_>>(),
		vec![(uuid(1), uuid(11))]
	);
}

/// A re-upsert that keeps the uuid must not log anything, marker or no marker — the trigger
/// fires on every `SET uuid = ...`, changed or not, so the WHEN clause has to tell the two
/// apart.
#[test]
fn an_upsert_that_keeps_the_uuid_logs_nothing() {
	let conn = db();
	add_root(&conn, uuid(9));
	upsert_file(&conn, uuid(1), stable(2), uuid(9), "pending.txt");
	mark_pending_upload(&conn, stable(2), 1_000).unwrap();

	upsert_file(&conn, uuid(1), stable(2), uuid(9), "pending.txt");

	assert!(select_slot_remints(&conn).unwrap().is_empty());
}

/// The identity sweep must spare a slot whose uuid the log names as retired: that slot can
/// hold the only copy of an unsent edit. A genuinely unknown uuid is still reported.
#[test]
fn the_identity_sweep_spares_a_logged_slot() {
	let conn = db();
	add_root(&conn, uuid(9));
	upsert_file(&conn, uuid(1), stable(2), uuid(9), "pending.txt");
	mark_pending_upload(&conn, stable(2), 1_000).unwrap();
	upsert_file(&conn, uuid(11), stable(2), uuid(9), "pending.txt");

	// On disk: the retired slot (spared), the current slot (known), and a true orphan.
	let positions = select_positions_not_in_uuids(
		&conn,
		[
			UuidStr::from(uuid(1)),
			UuidStr::from(uuid(11)),
			UuidStr::from(uuid(7)),
		]
		.into_iter(),
	)
	.unwrap();
	assert_eq!(positions, vec![2], "only the true orphan may be reported");
}

/// The drain's guard: an unrescued remint targeting the row's current uuid means the bytes
/// are on disk under a retired uuid, so "no local copy" must not read as "nothing left".
#[test]
fn slot_remint_targets_uuid_answers_for_the_new_uuid_only() {
	let conn = db();
	add_root(&conn, uuid(9));
	upsert_file(&conn, uuid(1), stable(2), uuid(9), "pending.txt");
	mark_pending_upload(&conn, stable(2), 1_000).unwrap();
	upsert_file(&conn, uuid(11), stable(2), uuid(9), "pending.txt");

	assert!(slot_remint_targets_uuid(&conn, uuid(11)).unwrap());
	assert!(!slot_remint_targets_uuid(&conn, uuid(1)).unwrap());
}

fn add_root(conn: &Connection, uuid_: Uuid) {
	conn.execute(
		"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
		[uuid_],
	)
	.unwrap();
}
