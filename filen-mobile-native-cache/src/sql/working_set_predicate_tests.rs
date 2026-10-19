use filen_sdk_rs::cache::open_sqlite_in_memory;

use super::*;
use crate::sql::item::{self, combine_parent};

fn db() -> Connection {
	let conn = open_sqlite_in_memory().unwrap();
	crate::auth::configure_conn(&conn).unwrap();
	conn.execute_batch(INIT).unwrap();
	conn.execute(
		"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
		[uuid(ROOT)],
	)
	.unwrap();
	conn
}

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

fn stable(byte: u8) -> StableUuid {
	StableUuid::new_for_test(uuid(byte))
}

const ROOT: u8 = 9;

fn add_file(conn: &Connection, uuid_: u8, parent: u8, trashed: bool) {
	let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
	let (id, _, _) = item::upsert_file_item_with_stmts(
		uuid(uuid_),
		stable(uuid_.wrapping_add(100)),
		combine_parent(Some(uuid(parent)), trashed),
		Some("f"),
		None,
		&mut stmt,
	)
	.unwrap();
	drop(stmt);
	conn.execute(
		"INSERT INTO files (id, size, chunks, region, bucket, timestamp,
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

fn members(conn: &Connection, containers: &[u8]) -> Vec<Uuid> {
	let containers: Vec<UuidStr> = containers
		.iter()
		.map(|byte| UuidStr::from(uuid(*byte)))
		.collect();
	let mut uuids: Vec<Uuid> = select_working_set(conn, containers.into_iter())
		.unwrap()
		.into_iter()
		.map(|obj| obj.uuid())
		.collect();
	uuids.sort();
	uuids
}

/// The predicate that decides what the system is told about: the original three stakes stay,
/// and everything sitting in a reported container joins them — the root injected by the
/// caller like any other container, or top-level items would never be members.
#[test]
fn container_members_join_the_original_stakes() {
	let conn = db();
	add_dir(&conn, 8, ROOT, false); // the materialized container
	add_dir(&conn, 7, ROOT, false); // browsed once, never materialized
	add_file(&conn, 1, 8, false); // member: sits in the container
	add_file(&conn, 2, 7, false); // non-member
	add_file(&conn, 3, 7, false); // member: bytes on the device
	conn.execute(
		"UPDATE items SET materialised_at = 5 WHERE uuid = ?1;",
		[uuid(3)],
	)
	.unwrap();

	// The caller passes the root alongside the reported set.
	let members = members(&conn, &[8, ROOT]);
	assert!(members.contains(&uuid(1)), "container child is a member");
	assert!(
		!members.contains(&uuid(2)),
		"unreported container's child is not"
	);
	assert!(
		members.contains(&uuid(3)),
		"materialised bytes are still a stake"
	);
	assert!(
		members.contains(&uuid(8)) && members.contains(&uuid(7)),
		"top-level items are members through the injected root"
	);
}

/// Trash semantics, both directions: an item TRASHED OUT of a container stays a member (its
/// move into the system's trash view is a change the system must be shown), while the
/// live-looking children of a container that was itself trashed are NOT presented as live
/// members — that would resurrect a folder the user deleted.
#[test]
fn trashed_containers_hide_their_children_but_trashed_members_stay() {
	let conn = db();
	add_dir(&conn, 8, ROOT, false); // the container
	add_file(&conn, 1, 8, true); // trashed out of the container: still a member
	add_dir(&conn, 6, 8, true); // a subdir trashed remotely, itself materialized
	add_file(&conn, 5, 6, false); // its child keeps parent=6 for the restore

	let members = members(&conn, &[8, 6, ROOT]);
	assert!(
		members.contains(&uuid(1)),
		"a member trashed out of a container moved to the trash view — show it"
	);
	assert!(
		members.contains(&uuid(6)),
		"the trashed dir itself is a member of its (live) parent container"
	);
	assert!(
		!members.contains(&uuid(5)),
		"children of a remotely-trashed container must not be live members"
	);
}
