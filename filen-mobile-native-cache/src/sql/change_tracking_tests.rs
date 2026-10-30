use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	cache::open_sqlite_in_memory, crypto::file::FileKey, fs::file::meta::DecryptedFileMeta,
};
use filen_types::{auth::FileEncryptionVersion, fs::ParentUuid};

use super::*;
use crate::{
	ffi::{FfiChanges, FfiObject},
	local::{changes_since, current_sync_anchor},
	sql::item::{self, combine_parent},
};

/// Configured the way every real connection is, which for these tests is the whole point: the
/// triggers run under the production pragmas, `recursive_triggers` included, so a delete
/// cascade reaches past the first generation exactly as it does in the app.
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

	update_items_with_parent(&mut conn, [], listing(), parent, &[]).unwrap();
	let after_first = anchor(&conn);
	let stamps = (seq_of(&conn, uuid(1)), seq_of(&conn, uuid(3)));

	update_items_with_parent(&mut conn, [], listing(), parent, &[]).unwrap();

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

	let queried_seq = |uuid_: Uuid| match FfiObject::from(DBObject::select(&conn, uuid_).unwrap()) {
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

/// The `(parent, name)` tier is a last resort for records carrying NO stable id. A record
/// that HAS one and missed both id tiers is a DIFFERENT item that happens to share the name
/// (the server permits exact-name collisions — the dedup hash is client-supplied): it must
/// insert fresh. Adopting the same-named row stole its identity, flipped the row's ids on
/// every subsequent listing of the twins, and emitted an authoritative tombstone for a live
/// item each time.
#[test]
fn a_stable_id_record_never_steals_a_same_named_row() {
	let conn = db();
	let parent = uuid(9);
	add_file(&conn, uuid(1), stable(2), parent, "a.txt");

	// Another file at the same name, arriving with ids the cache has never held.
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

	// Both rows exist — id_of panics on a missing one.
	id_of(&conn, uuid(1));
	id_of(&conn, uuid(3));
	assert_eq!(
		retired(&conn),
		[],
		"no identity was stolen, so nothing is retired; the old row's fate belongs to the sweep"
	);
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

/// The suppression above protects bytes the server has never seen — and a permanent delete has
/// already taken them, on the server and on disk both, by the time the row goes. Leaving the
/// marker standing there would suppress the tombstone of an item nothing can bring back: the
/// marker dies with its row either way, so no drain would ever resolve it and every replica
/// would keep the phantom for good. `delete_item` clears the markers of exactly what its
/// cascade reaches, in the same transaction, which is what puts the ordering out of a future
/// caller's hands.
#[test]
fn a_permanent_delete_retires_a_subtree_that_still_held_an_unsent_edit() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "sub");
	add_file(&conn, uuid(2), stable(3), uuid(1), "unsent.txt");
	mark_pending_upload(&conn, stable(3), 1).unwrap();
	// Trashed rows stay keyed off their original parent and the cascade spares them, so this
	// one outlives the delete — and its marker has to outlive it too.
	conn.execute(
		"INSERT INTO items (uuid, stable_uuid, parent, type, trashed, pending_upload_at)
			VALUES (?1, ?2, ?3, 2, TRUE, 1);",
		rusqlite::params![uuid(4), stable(5), uuid(1)],
	)
	.unwrap();
	let served = anchor(&conn);

	delete_item(&mut conn, uuid(1)).unwrap();

	let mut got = retired(&conn);
	got.sort();
	assert_eq!(
		got,
		vec![(1, uuid(1)), (2, uuid(3))],
		"the dir and the file under it are both gone for good, so both are retired"
	);
	assert!(
		anchor(&conn) > served,
		"and a replica holding {served} must be told about it"
	);
	assert_eq!(
		select_pending_upload_at(&conn, stable(5)).unwrap(),
		Some(1),
		"the trashed row the cascade spares keeps its marker"
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

	let changes = changes_since(&conn, Some(&served), 0).unwrap();
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
	let changes = changes_since(&conn, Some(&served), 0).unwrap();
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

	let changes = changes_since(&conn, Some(&served), 0).unwrap();
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

	let changes = changes_since(&conn, Some(&served), 0).unwrap();
	assert_eq!(
		sorted(changes.deleted_ids),
		sorted(vec![provider_id(uuid(2)), provider_id(uuid(3))]),
		"the file by its stable id, the dir by its uuid"
	);
	assert!(changes.updated.is_empty(), "nothing is left to render");

	// The file back under a re-minted uuid, the dir back as itself.
	add_file(&conn, uuid(4), stable(2), parent, "a.txt");
	add_dir(&conn, uuid(3), parent, "sub");

	let changes = changes_since(&conn, Some(&served), 0).unwrap();
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

	let changes = changes_since(&conn, None, 0).unwrap();

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
				changes_since(&conn, Some(anchor), 0),
				Err(CacheError::SyncAnchorExpired(_))
			),
			"{label} must expire rather than be honoured"
		);
	}

	assert!(
		changes_since(&conn, Some(&ours), 0).is_ok(),
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

	// No reported containers: this test pins the three original stakes on their own.
	let members = |conn: &Connection| {
		let mut uuids: Vec<String> = select_working_set(conn, Vec::<UuidStr>::new().into_iter())
			.unwrap()
			.iter()
			.map(|obj| obj.uuid().to_string())
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

#[test]
fn a_page_is_a_window_into_the_same_ordered_listing() {
	let conn = db();
	let parent = uuid(0xA0);
	// Insertion order is deliberately not name order — the pages must follow the
	// listing's ordering, not the rowids'.
	add_file(&conn, uuid(3), stable(3), parent, "c.txt");
	add_file(&conn, uuid(1), stable(1), parent, "a.txt");
	add_dir(&conn, uuid(5), parent, "e");
	add_file(&conn, uuid(4), stable(4), parent, "d.txt");
	add_file(&conn, uuid(2), stable(2), parent, "b.txt");

	let all: Vec<crate::ffi::FfiNonRootObject> = select_children(&conn, None, parent)
		.unwrap()
		.into_iter()
		.map(Into::into)
		.collect();
	assert_eq!(all.len(), 5);

	let page = |offset: u32, limit: u32| -> Vec<crate::ffi::FfiNonRootObject> {
		select_children_page(&conn, None, parent, limit, offset)
			.unwrap()
			.into_iter()
			.map(Into::into)
			.collect()
	};
	assert_eq!(page(0, 2), all[0..2]);
	assert_eq!(page(2, 2), all[2..4]);
	assert_eq!(page(4, 2), all[4..5], "the short page ends the listing");
	assert_eq!(
		page(5, 2),
		[],
		"an offset past the end is empty, not an error"
	);
}

/// A file with an unsent edit that vanished from its parent's listing (deleted or moved
/// remotely by another device) must survive the stale sweep: the row and its marker are the
/// only record linking those bytes to an upload obligation, and the launch drain finds them
/// nowhere else. The tombstone trigger's guard alone only suppresses the feed record — it
/// does not keep the row alive.
#[test]
fn the_stale_sweep_spares_a_pending_row_missing_from_the_listing() {
	let mut conn = db();
	let parent = uuid(9);
	add_file(&conn, uuid(1), stable(1), parent, "report.txt");
	mark_pending_upload(&conn, stable(1), 12_345).unwrap();

	update_items_with_parent(&mut conn, [], [], parent, &[]).unwrap();

	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(1)],
		"the pending-upload marker must survive the sweep"
	);
	assert_eq!(
		retired(&conn),
		[],
		"a spared phantom must not be tombstoned"
	);
}

/// The ancestor variant: the pending file sits below a directory that vanished from the
/// swept listing. Deleting the directory would cascade to the whole subtree and take the
/// marker with it, so the sheltering chain is kept as phantoms instead — `forget_item`
/// already refuses the equivalent single-item deletion for exactly this reason.
#[test]
fn the_stale_sweep_spares_a_directory_sheltering_a_pending_descendant() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "docs");
	add_dir(&conn, uuid(2), uuid(1), "reports");
	add_file(&conn, uuid(3), stable(3), uuid(2), "draft.txt");
	add_file(&conn, uuid(4), stable(4), uuid(2), "clean.txt");
	mark_pending_upload(&conn, stable(3), 12_345).unwrap();

	update_items_with_parent(&mut conn, [], [], parent, &[]).unwrap();

	// The whole sheltering chain survives — id_of panics on a missing row.
	id_of(&conn, uuid(1));
	id_of(&conn, uuid(2));
	id_of(&conn, uuid(3));
	id_of(&conn, uuid(4));
	assert_eq!(select_pending_uploads(&conn).unwrap(), vec![stable(3)]);
	assert_eq!(retired(&conn), []);
}

/// The parent-scoped twin of `a_trashed_childs_marker_spares_itself_but_not_its_dead_parent`:
/// a marker under a TRASHED intermediate directory must not spare the untrashed ancestor
/// above it. Deleting that ancestor cascades only through untrashed children, so it could
/// never have reached the marker — and sparing it anyway made a permanently-deleted
/// directory unreapable, re-probed by the reconcile on every refresh.
#[test]
fn a_marker_under_a_trashed_dir_does_not_spare_the_ancestor_above_it() {
	let mut conn = db();
	let grandparent = uuid(9);
	add_dir(&conn, uuid(1), grandparent, "doomed_parent");
	add_dir(&conn, uuid(2), uuid(1), "trashed_middle");
	add_file(&conn, uuid(3), stable(3), uuid(2), "edited.txt");
	mark_pending_upload(&conn, stable(3), 12_345).unwrap();
	// Only the intermediate directory is trashed; the marker-holder is not.
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(2)],
	)
	.unwrap();

	// A listing of the grandparent no longer mentions the parent.
	update_items_with_parent(&mut conn, [], [], grandparent, &[]).unwrap();

	assert_eq!(
		conn.query_row(
			"SELECT COUNT(*) FROM items WHERE uuid = ?1;",
			[uuid(1)],
			|r| r.get::<_, i64>(0)
		)
		.unwrap(),
		0,
		"the ancestor must be swept: its cascade stops at the trashed child"
	);
	// The marker itself is untouched — it was never in that cascade's path.
	id_of(&conn, uuid(3));
	assert_eq!(select_pending_uploads(&conn).unwrap(), vec![stable(3)]);
}

/// The guard is scoped: a subtree without a pending marker anywhere below it is swept and
/// tombstoned exactly as before.
#[test]
fn the_stale_sweep_still_deletes_clean_subtrees() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "docs");
	add_file(&conn, uuid(2), stable(2), uuid(1), "clean.txt");

	update_items_with_parent(&mut conn, [], [], parent, &[]).unwrap();

	let mut tombstones = retired(&conn);
	tombstones.sort();
	assert_eq!(
		tombstones,
		[(1, uuid(1)), (2, uuid(2))],
		"a clean subtree is swept and tombstoned as before"
	);
}

/// The TRASH sweep's counterpart of the pending guard: a file trashed remotely (the flag
/// flips via the upsert, which never touches `pending_upload_at`, so the marker rides into
/// the trash) and then absent from the fresh trash listing must survive the sweep — the
/// tombstone trigger's own pending guard would suppress the tombstone too, so deletion here
/// erased the only record of the unsent edit with no replica ever told.
#[test]
fn the_trash_sweep_spares_a_pending_row_missing_from_the_listing() {
	let mut conn = db();
	let parent = uuid(9);
	add_file(&conn, uuid(1), stable(1), parent, "edited.txt");
	mark_pending_upload(&conn, stable(1), 12_345).unwrap();
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(1)],
	)
	.unwrap();

	update_trashed_items(&mut conn, [], [], &[]).unwrap();

	id_of(&conn, uuid(1));
	assert_eq!(
		retired(&conn),
		[],
		"a spared phantom must not be tombstoned"
	);
}

/// The inverse scoping: a marker on an independently-TRASHED child spares the child itself,
/// but not its ancestors — the cascade a delete takes leaves trashed children alone, so the
/// parent can be reaped without ever reaching that marker. Climbing anyway left a
/// permanently-deleted dir spared forever: re-probed, re-failed, unreapable, while the
/// trashed child's own fate belongs to the trash-file probe.
#[test]
fn a_trashed_childs_marker_spares_itself_but_not_its_dead_parent() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "doomed");
	add_file(&conn, uuid(2), stable(2), uuid(1), "edited.txt");
	mark_pending_upload(&conn, stable(2), 12_345).unwrap();
	// Both trashed independently: the file first, then its old parent.
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid IN (?1, ?2);",
		rusqlite::params![uuid(1), uuid(2)],
	)
	.unwrap();

	// Neither appears in the fresh trash listing: the dir was permanently
	// deleted elsewhere; the file's absence is not yet explained.
	update_trashed_items(&mut conn, [], [], &[]).unwrap();

	assert!(
		conn.query_row(
			"SELECT COUNT(*) FROM items WHERE uuid = ?1;",
			[uuid(1)],
			|r| r.get::<_, i64>(0)
		)
		.unwrap() == 0,
		"the dead dir must be reaped — no cascade from it can reach the trashed child"
	);
	id_of(&conn, uuid(2));
	// Asserted on the column, not via select_pending_uploads: the drain's
	// selection deliberately skips trashed rows.
	let marker: Option<i64> = conn
		.query_row(
			"SELECT pending_upload_at FROM items WHERE uuid = ?1;",
			[uuid(2)],
			|r| r.get(0),
		)
		.unwrap();
	assert_eq!(
		marker,
		Some(12_345),
		"the trashed child and its marker survive on their own"
	);
}

/// The ancestor variant for the trash: a trashed directory sheltering an unsent edit below
/// it survives the sweep whole — deleting it would cascade to the marker.
#[test]
fn the_trash_sweep_spares_a_trashed_dir_sheltering_a_pending_descendant() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "docs");
	add_file(&conn, uuid(2), stable(2), uuid(1), "draft.txt");
	mark_pending_upload(&conn, stable(2), 12_345).unwrap();
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(1)],
	)
	.unwrap();

	update_trashed_items(&mut conn, [], [], &[]).unwrap();

	id_of(&conn, uuid(1));
	id_of(&conn, uuid(2));
	assert_eq!(select_pending_uploads(&conn).unwrap(), vec![stable(2)]);
	assert_eq!(retired(&conn), []);
}

/// The guard is scoped: a clean trashed row absent from the listing is swept and tombstoned
/// exactly as before.
#[test]
fn the_trash_sweep_still_deletes_clean_rows() {
	let mut conn = db();
	let parent = uuid(9);
	add_file(&conn, uuid(1), stable(1), parent, "clean.txt");
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(1)],
	)
	.unwrap();

	update_trashed_items(&mut conn, [], [], &[]).unwrap();

	assert_eq!(
		retired(&conn),
		[(2, uuid(1))],
		"a clean trashed row is swept and tombstoned as before"
	);
}

/// A row the caller spared — its server-side fate could not be verified before the sweep —
/// survives the round whole, cascade and all, instead of being destructively guessed about.
#[test]
fn the_stale_sweep_spares_rows_the_caller_could_not_verify() {
	let mut conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "maybe-moved");
	add_file(&conn, uuid(2), stable(2), uuid(1), "child.txt");

	update_items_with_parent(&mut conn, [], [], parent, &[uuid(1)]).unwrap();

	id_of(&conn, uuid(1));
	id_of(&conn, uuid(2));
	assert_eq!(retired(&conn), [], "a spared subtree emits no tombstones");
}

/// The delete cascade fires from paths with no per-row visibility in Rust (it is a trigger
/// for a reason), so it needs its own guard: a pending child survives its parent's deletion
/// as an orphan phantom — the marker keeps the drain able to find it — instead of dying with
/// the subtree.
#[test]
fn the_delete_cascade_spares_pending_children() {
	let conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "docs");
	add_file(&conn, uuid(2), stable(2), uuid(1), "dirty.txt");
	add_file(&conn, uuid(3), stable(3), uuid(1), "clean.txt");
	mark_pending_upload(&conn, stable(2), 12_345).unwrap();

	conn.execute("DELETE FROM items WHERE uuid = ?1;", [uuid(1)])
		.unwrap();

	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(2)],
		"the pending child must survive the cascade"
	);
	id_of(&conn, uuid(2));
	let mut tombstones = retired(&conn);
	tombstones.sort();
	assert_eq!(
		tombstones,
		[(1, uuid(1)), (2, uuid(3))],
		"the dir and its clean child are tombstoned; the pending child is not"
	);
}

/// Same guard for the uuid-overwrite cascade (the server reassigned a directory's uuid).
#[test]
fn the_uuid_overwrite_cascade_spares_pending_children() {
	let conn = db();
	let parent = uuid(9);
	add_dir(&conn, uuid(1), parent, "docs");
	add_file(&conn, uuid(2), stable(2), uuid(1), "dirty.txt");
	add_file(&conn, uuid(3), stable(3), uuid(1), "clean.txt");
	mark_pending_upload(&conn, stable(2), 12_345).unwrap();

	conn.execute(
		"UPDATE items SET uuid = ?1 WHERE uuid = ?2;",
		rusqlite::params![uuid(7), uuid(1)],
	)
	.unwrap();

	assert_eq!(
		select_pending_uploads(&conn).unwrap(),
		vec![stable(2)],
		"the pending child must survive the cascade"
	);
	id_of(&conn, uuid(2));
	let mut tombstones = retired(&conn);
	tombstones.sort();
	assert_eq!(
		tombstones,
		[(1, uuid(1)), (2, uuid(3))],
		"the dir's retired uuid and its clean child are tombstoned; the pending child is not"
	);
}

/// Whether resolving `id` would be answered as gone without asking the server, for each form the
/// unknown-id probe checks: a dir's uuid, a file's stable id.
fn known_gone(conn: &Connection, kind: ItemType, id: Uuid) -> bool {
	is_permanently_deleted(conn, kind, id).unwrap()
}

/// After a permanent delete the server reports a descendant as trashed, though no trash listing
/// holds it: asked, it would come back as a ghost in the trash. Every id the cascade retired,
/// at any depth, has to be answered from the cache as gone.
#[test]
fn a_permanent_delete_answers_for_its_whole_subtree() {
	let mut conn = db();
	add_dir(&conn, uuid(1), uuid(9), "deleted");
	add_file(&conn, uuid(2), stable(3), uuid(1), "child.txt");
	add_dir(&conn, uuid(4), uuid(1), "nested");
	add_file(&conn, uuid(5), stable(6), uuid(4), "grandchild.txt");

	delete_item(&mut conn, uuid(1)).unwrap();

	assert!(known_gone(&conn, ItemType::Dir, uuid(1)), "the deleted dir");
	assert!(known_gone(&conn, ItemType::File, uuid(3)), "its file");
	assert!(known_gone(&conn, ItemType::Dir, uuid(4)), "its subdir");
	assert!(
		known_gone(&conn, ItemType::File, uuid(6)),
		"the file below that"
	);
	assert!(
		!known_gone(&conn, ItemType::Dir, uuid(3)),
		"a file's stable id is not a dir's uuid"
	);
}

/// A trash is not a permanent delete: the row stays, trashed, and an id the trash relisting
/// swept is still asked about, so the server's trashed answer stands. A trashed child the
/// cascade spares keeps its row too.
#[test]
fn a_trashed_item_is_never_taken_for_permanently_deleted() {
	let mut conn = db();
	add_dir(&conn, uuid(1), uuid(9), "deleted");
	add_file(&conn, uuid(2), stable(3), uuid(1), "trashed-before.txt");
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(2)],
	)
	.unwrap();
	add_file(&conn, uuid(4), stable(5), uuid(9), "trashed-elsewhere.txt");
	conn.execute(
		"UPDATE items SET trashed = TRUE WHERE uuid = ?1;",
		[uuid(4)],
	)
	.unwrap();

	delete_item(&mut conn, uuid(1)).unwrap();
	let spared: bool = conn
		.query_row(
			"SELECT trashed FROM items WHERE uuid = ?1;",
			[uuid(2)],
			|r| r.get(0),
		)
		.unwrap();
	assert!(
		spared,
		"the cascade spares a trashed child, and it stays trashed"
	);
	assert!(!known_gone(&conn, ItemType::File, uuid(3)));

	assert!(
		!known_gone(&conn, ItemType::File, uuid(5)),
		"a trashed item is not gone"
	);
	conn.execute(MARK_STALE_TRASHED, []).unwrap();
	conn.execute(DELETE_STALE_TRASHED, []).unwrap();
	assert!(
		retired(&conn).contains(&(2, uuid(5))),
		"the trash relisting retired it"
	);
	assert!(
		!known_gone(&conn, ItemType::File, uuid(5)),
		"but a sweep is not the server saying gone, so it is still asked about"
	);
}

/// A listing that stopped mentioning a dir is no proof it is gone: it may have moved somewhere
/// this cache never listed. Its id stays one the server is asked about.
#[test]
fn a_listing_sweep_is_not_a_permanent_delete() {
	let conn = db();
	add_dir(&conn, uuid(1), uuid(9), "moved-away");
	conn.execute(MARK_STALE_WITH_PARENT, [uuid(9)]).unwrap();
	conn.execute(DELETE_STALE_WITH_PARENT, [uuid(9)]).unwrap();

	assert_eq!(retired(&conn), vec![(1, uuid(1))]);
	assert!(!known_gone(&conn, ItemType::Dir, uuid(1)));
}

/// An id that resolves again loses its tombstone, and with it the permanent-delete answer; a
/// later retirement stamps a fresh sequence, outside the range the delete logged.
#[test]
fn an_id_that_resolves_again_is_no_longer_permanently_deleted() {
	let mut conn = db();
	add_dir(&conn, uuid(1), uuid(9), "deleted");
	add_dir(&conn, uuid(4), uuid(1), "nested");
	delete_item(&mut conn, uuid(1)).unwrap();
	assert!(known_gone(&conn, ItemType::Dir, uuid(4)));

	add_dir(&conn, uuid(4), uuid(8), "nested");
	assert!(
		!known_gone(&conn, ItemType::Dir, uuid(4)),
		"it resolves again"
	);

	conn.execute(MARK_STALE_WITH_PARENT, [uuid(8)]).unwrap();
	conn.execute(DELETE_STALE_WITH_PARENT, [uuid(8)]).unwrap();
	assert!(retired(&conn).contains(&(1, uuid(4))));
	assert!(
		!known_gone(&conn, ItemType::Dir, uuid(4)),
		"retired again, by a sweep this time"
	);
	assert!(
		known_gone(&conn, ItemType::Dir, uuid(1)),
		"the deleted dir itself is untouched"
	);
}

/// A cascade can reach a dir that had moved away remotely without this cache knowing. A live
/// row landing under it (from a search or recents answer) shows it exists, so the server is asked
/// again. A trashed row filed under it shows nothing: trashed rows keep their original parent.
#[test]
fn a_live_row_under_a_permanently_deleted_dir_lifts_the_answer() {
	let mut conn = db();
	add_dir(&conn, uuid(1), uuid(9), "deleted");
	add_dir(&conn, uuid(4), uuid(1), "moved-away");
	delete_item(&mut conn, uuid(1)).unwrap();

	conn.execute(
		"INSERT INTO items (uuid, stable_uuid, parent, type, trashed)
			VALUES (?1, ?2, ?3, 2, TRUE);",
		rusqlite::params![uuid(5), stable(6), uuid(4)],
	)
	.unwrap();
	assert!(known_gone(&conn, ItemType::Dir, uuid(4)));

	add_file(&conn, uuid(7), stable(8), uuid(4), "found-in-recents.txt");
	assert!(!known_gone(&conn, ItemType::Dir, uuid(4)));
	assert!(known_gone(&conn, ItemType::Dir, uuid(1)));
}

/// One range per run of back-to-back deletes, so the log grows with the deletes that something
/// else came between, not with every delete. A delete that found nothing logs nothing.
#[test]
fn back_to_back_permanent_deletes_share_one_range() {
	let mut conn = db();
	let ranges = |conn: &Connection| -> i64 {
		conn.query_row("SELECT COUNT(*) FROM permanent_deletions;", [], |r| {
			r.get(0)
		})
		.unwrap()
	};
	add_dir(&conn, uuid(1), uuid(9), "a");
	add_dir(&conn, uuid(2), uuid(9), "b");
	add_dir(&conn, uuid(3), uuid(9), "c");

	delete_item(&mut conn, uuid(1)).unwrap();
	delete_item(&mut conn, uuid(2)).unwrap();
	assert_eq!(ranges(&conn), 1);

	delete_item(&mut conn, uuid(7)).unwrap();
	assert_eq!(ranges(&conn), 1, "nothing was deleted");

	add_dir(&conn, uuid(4), uuid(9), "d");
	delete_item(&mut conn, uuid(3)).unwrap();
	assert_eq!(ranges(&conn), 2);
	for dir in [1, 2, 3] {
		assert!(known_gone(&conn, ItemType::Dir, uuid(dir)), "dir {dir}");
	}
	assert!(!known_gone(&conn, ItemType::Dir, uuid(4)));
}
