use super::{
	super::{
		apply::{PassStart, PlannedEntry},
		plan::SyncAction,
		side::Side,
		stage::Stage,
	},
	*,
};
use crate::cache::test_support::rewrite_behind_sqlite;

fn file_entry(rel_path: &str, hash: [u8; 32], size: u64) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(Uuid::new_v4()),
		content_hash: Some(Blake3Hash::from(hash)),
		size: Some(size),
		local_mtime: Some(1_700_000_000_123),
		remote_modified: Some(1_700_000_000_456),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(Uuid::new_v4())),
		agreed_hash: Some(Blake3Hash::from([0xAB; 32])),
	}
}

fn dir_entry(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::Dir,
		remote_uuid: Some(Uuid::new_v4()),
		content_hash: None,
		size: None,
		local_mtime: None,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// A unique temp path for a store that has to be closed and reopened.
fn temp_db_path(tag: &str) -> std::path::PathBuf {
	std::env::temp_dir().join(format!("filen_baseline_{tag}_{}.db", Uuid::new_v4()))
}

/// Wall-clock stamps for the journal tests, and the ceiling they are measured against.
const NOW: i64 = 1_800_000_000_000;
const GRACE: i64 = 180_000;

fn pending_count(store: &BaselineStore) -> i64 {
	store
		.conn
		.query_row("SELECT COUNT(*) FROM pending_writes", [], |row| row.get(0))
		.unwrap()
}

fn created(path: &str) -> PendingKind {
	PendingKind::Created {
		path: path.to_string(),
		replaced: None,
	}
}

fn journal_row(pair: PairId, uuid: Uuid, kind: PendingKind, recorded_at: i64) -> PendingRow {
	PendingRow {
		pair,
		uuid,
		kind,
		recorded_at,
	}
}

/// The `PRAGMA user_version` a store's connection reports.
fn version_of(store: &BaselineStore) -> i64 {
	store
		.conn
		.query_row("PRAGMA user_version", [], |row| row.get(0))
		.unwrap()
}

/// A pragma's value as this connection reports it.
fn pragma<T: rusqlite::types::FromSql>(store: &BaselineStore, name: &str) -> T {
	store
		.conn
		.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
		.unwrap()
}

/// A write a pass makes AFTER the act it records must FAIL once another pair's connection has
/// held the write lock past the busy timeout, rather than waiting it out: a wedged writer must not
/// hang a pass. The act then stands unrecorded, which the log and the pass's report say — and
/// which every site recovers from on a later pass (see [`record_write`]).
#[test]
fn a_journal_write_under_a_held_write_lock_fails_instead_of_waiting() {
	let path = temp_db_path("busy_journal");
	let holder = BaselineStore::open(&path).unwrap();
	let (pair, _) = holder
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	let writer = BaselineStore::open(&path).unwrap();
	// A real open waits 30 s, which no test can sit out; 50 ms reaches the same answer.
	writer.busy_timeout_for_test(std::time::Duration::from_millis(50));
	{
		let _held = holder
			.hold_write_lock(pair, &file_entry("a.txt", [1; 32], 3))
			.unwrap();
		let started = std::time::Instant::now();
		let error = writer
			.record_pending(
				&journal_row(pair, Uuid::new_v4(), created("a.txt"), NOW),
				&[],
				&[],
			)
			.expect_err("the contended journal write must fail rather than wait for the lock");
		assert!(
			is_write_lock_contention(&error),
			"the failure must be the write lock and not something else: {error}"
		);
		assert!(
			started.elapsed() < std::time::Duration::from_secs(5),
			"it waited {:?}: the bound is the busy timeout, not the holder's lifetime",
			started.elapsed()
		);
		assert_eq!(pending_count(&writer), 0, "nothing was recorded");
	}

	// With the lock free the same write lands, so the failure above was the contention and not a
	// connection this test broke.
	writer
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("a.txt"), NOW),
			&[],
			&[],
		)
		.expect("the write must land once the lock is free");
	assert_eq!(pending_count(&writer), 1);

	drop(writer);
	drop(holder);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
}

/// Every open runs in WAL with `synchronous = NORMAL` and a busy timeout — the reopen too, which
/// takes the early-returning path through `init`. Two of the three are per-connection, so an
/// open that skipped them would quietly go back to an fsync per commit, which is what made the
/// row-per-action writes of a first sync cost a quarter of a millisecond each.
#[test]
fn every_open_runs_in_wal_with_normal_sync_and_a_busy_timeout() {
	let path = temp_db_path("wal");
	let assert_pragmas = |store: &BaselineStore| {
		assert_eq!(pragma::<String>(store, "journal_mode"), "wal");
		assert_eq!(pragma::<i64>(store, "synchronous"), 1, "NORMAL is 1");
		assert_eq!(pragma::<i64>(store, "busy_timeout"), 30_000);
		assert_eq!(
			pragma::<i64>(store, "journal_size_limit"),
			WAL_SIZE_LIMIT_BYTES
		);
	};
	let entry = file_entry("a.txt", [1u8; 32], 3);
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		assert_pragmas(&store);
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store.upsert_entry(pair, &entry).unwrap();
		pair
	};

	let reopened = BaselineStore::open(&path).unwrap();
	assert_pragmas(&reopened);
	assert_eq!(
		reopened.entry(pair, "a.txt").unwrap().as_ref(),
		Some(&entry),
		"the row written under WAL must read back after the reopen"
	);
	drop(reopened);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
}

#[test]
fn a_fresh_db_is_stamped_at_the_current_schema_version() {
	let store = BaselineStore::open_in_memory().unwrap();
	assert_eq!(version_of(&store), SCHEMA_VERSION);
}

/// Both directions of a version mismatch are refused, and the DB is left exactly as it was.
/// There is no migration chain: a stamp other than the current one means the DB was not written
/// by this engine, whichever side of the current version it sits on.
#[test]
fn a_db_stamped_at_any_other_version_is_refused_not_read() {
	for planted in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
		let path = temp_db_path(&format!("version_{planted}"));
		{
			let store = BaselineStore::open(&path).unwrap();
			store
				.conn
				.execute_batch(&format!("PRAGMA user_version = {planted};"))
				.unwrap();
		}
		let message = match BaselineStore::open(&path) {
			Ok(_) => panic!("schema version {planted} must be refused"),
			Err(error) => error.to_string(),
		};
		assert!(
			message.contains("schema version") && message.contains("refusing"),
			"the refusal must name the version problem: {message}"
		);
		// Fails CLOSED: the DB is left as it was, neither migrated nor re-stamped in place.
		let raw = open_sqlite(&path).unwrap();
		let still: i64 = raw
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap();
		assert_eq!(still, planted, "the refused DB was rewritten");
		drop(raw);
		std::fs::remove_file(&path).ok();
	}
}

/// A DB this build refuses is not written to on the way out. The journal mode is the one pragma
/// that persists in the file HEADER, so setting it before the version check switched a stranger's
/// DB — one a newer build wrote — to WAL and left `-wal`/`-shm` sidecars beside it, and a
/// read-only file failed at the pragma instead of being refused with the message that explains
/// what to do about it.
#[test]
fn a_refused_db_keeps_the_journal_mode_it_arrived_in() {
	let path = temp_db_path("refused_journal_mode");
	let journal_mode = |conn: &Connection| -> String {
		conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
			.unwrap()
	};
	{
		// Not opened through `BaselineStore`: this file has never been in WAL, and it has a
		// table, so it reads as written rather than brand new.
		let raw = open_sqlite(&path).unwrap();
		raw.execute_batch(&format!(
			"CREATE TABLE something (id INTEGER PRIMARY KEY);
				 PRAGMA user_version = {};",
			SCHEMA_VERSION + 1
		))
		.unwrap();
		assert_eq!(
			journal_mode(&raw),
			"delete",
			"the fixture starts in rollback"
		);
	}

	match BaselineStore::open(&path) {
		Ok(_) => panic!("a foreign schema version must be refused"),
		Err(error) => assert!(
			error.to_string().contains("refusing"),
			"the refusal must name the version problem: {error}"
		),
	}

	let raw = open_sqlite(&path).unwrap();
	assert_eq!(
		journal_mode(&raw),
		"delete",
		"the refused DB was switched to WAL on the way to the refusal"
	);
	drop(raw);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
}

/// Creating the schema is all-or-nothing. A batch that fails partway (here: a statement SQLite
/// rejects, standing in for a disk error) must leave the file exactly as it was found — no
/// tables, version 0 — because that is the one state a later open reads as "brand new" and
/// creates from scratch. A partial set of tables would be refused as a stranger's DB forever.
#[test]
fn a_schema_creation_that_fails_partway_leaves_no_tables_behind() {
	let path = temp_db_path("partial_schema");
	let broken = format!("{SCHEMA}\nCREATE TABLE oops (id INTEGER PRIMARY KEY;");
	{
		let conn = open_sqlite(&path).unwrap();
		create_schema(&conn, &broken).expect_err("the broken batch must fail");
		assert!(
			is_unwritten(&conn).unwrap(),
			"the failed creation left tables behind"
		);
		let version: i64 = conn
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap();
		assert_eq!(version, 0, "the failed creation stamped a version");
	}

	// So the next open creates it from scratch rather than refusing it.
	let store = BaselineStore::open(&path).unwrap();
	assert_eq!(version_of(&store), SCHEMA_VERSION);
	assert!(store.list_pairs().unwrap().is_empty());
	drop(store);
	std::fs::remove_file(&path).ok();
}

/// A DB file that exists but has never been written to is a FRESH one, not a version-0 stranger:
/// `user_version` reads 0 either way, so emptiness is what tells them apart.
#[test]
fn an_empty_db_file_is_created_from_scratch_rather_than_refused() {
	let path = temp_db_path("empty_file");
	drop(open_sqlite(&path).unwrap());
	let store = BaselineStore::open(&path).unwrap();
	assert_eq!(version_of(&store), SCHEMA_VERSION);
	assert!(store.list_pairs().unwrap().is_empty());
	drop(store);
	std::fs::remove_file(&path).ok();
}

#[test]
fn a_configured_delete_guard_survives_a_reopen() {
	let path = temp_db_path("guard_roundtrip");
	let remote = Uuid::new_v4();
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		let (pair, _) = store
			.create_pair("/root", remote, SyncMode::TwoWay)
			.unwrap();
		assert_eq!(
			store.pair(pair).unwrap().unwrap().delete_guard,
			DeleteGuard::default(),
			"a new pair starts on the default"
		);
		store
			.set_delete_guard(pair, DeleteGuard::new(3, 0.25).unwrap())
			.unwrap();
		pair
	};

	let store = BaselineStore::open(&path).unwrap();
	let guard = store.pair(pair).unwrap().unwrap().delete_guard;
	assert_eq!(guard.floor(), 3);
	assert_eq!(guard.ratio(), 0.25);
	assert_eq!(
		store.list_pairs().unwrap()[0].delete_guard,
		guard,
		"list_pairs reports it too"
	);

	// `unlimited`'s usize::MAX floor has no SQLite representation; it must still come back as
	// the unlimited guard, not wrapped to a strict one nor read as a finite floor.
	store
		.set_delete_guard(pair, DeleteGuard::unlimited())
		.unwrap();
	assert_eq!(
		store.pair(pair).unwrap().unwrap().delete_guard,
		DeleteGuard::unlimited(),
		"an unlimited guard must read back as one"
	);
	drop(store);
	std::fs::remove_file(&path).ok();
}

/// A fresh DB carries the user ignore table, reads it as empty, and keeps what is set across a
/// reopen.
#[test]
fn user_ignore_patterns_start_empty_and_survive_a_reopen() {
	let path = temp_db_path("user_ignore");
	{
		let store = BaselineStore::open(&path).unwrap();
		let tables: i64 = store
			.conn
			.query_row(
				"SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'user_ignore'",
				[],
				|row| row.get(0),
			)
			.unwrap();
		assert_eq!(tables, 1, "a fresh DB has no user_ignore table");
		assert_eq!(store.user_ignore().unwrap(), "");
		store.set_user_ignore("*.psd").unwrap();
		store.set_user_ignore("*.psd\n!keep.psd").unwrap();
	}
	let store = BaselineStore::open(&path).unwrap();
	assert_eq!(store.user_ignore().unwrap(), "*.psd\n!keep.psd");
	drop(store);
	std::fs::remove_file(&path).ok();
}

/// A pair's recorded ignored roots are replaced whole, kept apart from another pair's, and go
/// with the pair.
#[test]
fn ignored_roots_are_replaced_per_pair_and_go_with_it() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/a", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/b", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	assert!(store.ignored_roots(pair).unwrap().is_empty());
	let roots = |list: &[&str]| -> BTreeSet<String> {
		list.iter().map(|root| (*root).to_string()).collect()
	};
	store
		.set_ignored_roots(pair, &roots(&["build", "ä/.DS_Store"]))
		.unwrap();
	store.set_ignored_roots(other, &roots(&["cache"])).unwrap();
	store.set_ignored_roots(pair, &roots(&["build"])).unwrap();
	assert_eq!(store.ignored_roots(pair).unwrap(), roots(&["build"]));
	assert_eq!(store.ignored_roots(other).unwrap(), roots(&["cache"]));
	store.delete_pair(pair).unwrap();
	assert!(store.ignored_roots(pair).unwrap().is_empty());
	assert_eq!(store.ignored_roots(other).unwrap(), roots(&["cache"]));
}

#[test]
fn path_failures_count_consecutively_and_reset_on_a_clear() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	assert!(store.failures(pair).unwrap().is_empty());

	store.record_failure(pair, "a.txt", "boom", 1_000).unwrap();
	store
		.record_failure(pair, "a.txt", "boom again", 2_000)
		.unwrap();
	store.record_failure(pair, "b.txt", "other", 1_500).unwrap();
	let failures = store.failures(pair).unwrap();
	assert_eq!(
		failures["a.txt"],
		PathFailure {
			attempts: 2,
			last_error: "boom again".to_string(),
			last_failure_at: 2_000,
		},
		"the streak counts up and keeps the LAST error and the LAST failure's time"
	);
	assert_eq!(failures["b.txt"].attempts, 1);
	assert_eq!(failures["b.txt"].last_failure_at, 1_500);

	// Clearing one path leaves the other alone, and a later failure starts from 1 again.
	store.clear_failure(pair, "a.txt").unwrap();
	assert!(!store.failures(pair).unwrap().contains_key("a.txt"));
	store.record_failure(pair, "a.txt", "fresh", 3_000).unwrap();
	assert_eq!(store.failures(pair).unwrap()["a.txt"].attempts, 1);
	assert_eq!(store.failures(pair).unwrap()["b.txt"].attempts, 1);

	// Clearing a path with no streak is a no-op, not an error.
	store.clear_failure(pair, "never-failed.txt").unwrap();

	// The streak is pair-scoped state: removing the pair takes it with it.
	store.delete_pair(pair).unwrap();
	assert!(store.failures(pair).unwrap().is_empty());
}

/// A removed pair's id must never be handed to a later pair. A watch loop finishes the pass it
/// is in when its pair is removed, so that pass still writes baseline and journal rows keyed by
/// the old id: with the id reused, they land in the successor — whose first sync is supposed to
/// see an EMPTY baseline — instead of failing against a pair that is gone.
/// Clearing a pass's applied paths in one go has to write what clearing them one at a time
/// wrote: those streaks and no others, a path with no streak still a no-op.
#[test]
fn clearing_several_paths_at_once_leaves_every_other_streak_alone() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	for path in ["a.txt", "b.txt", "c.txt"] {
		store.record_failure(pair, path, "boom", 1_000).unwrap();
		store.record_failure(other, path, "boom", 1_000).unwrap();
	}

	store
		.clear_failures(pair, &["a.txt", "c.txt", "never-failed.txt"])
		.unwrap();
	assert_eq!(
		store
			.failures(pair)
			.unwrap()
			.into_keys()
			.collect::<BTreeSet<_>>(),
		BTreeSet::from(["b.txt".to_string()])
	);
	assert_eq!(
		store.failures(other).unwrap().len(),
		3,
		"another pair's streaks are not this pair's to clear"
	);
}

/// Retiring a batch of journal rows retires exactly the named ones.
#[test]
fn delete_pending_retires_exactly_the_named_writes() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let uuids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
	for uuid in &uuids {
		store
			.record_pending(&journal_row(pair, *uuid, created("a.txt"), NOW), &[], &[])
			.unwrap();
	}

	store.delete_pending(pair, &uuids[..3]).unwrap();
	assert_eq!(pending_count(&store), 1);
	assert_eq!(
		store
			.load_pending(NOW, GRACE)
			.unwrap()
			.iter()
			.map(|row| row.uuid)
			.collect::<Vec<_>>(),
		vec![uuids[3]]
	);
}

/// The records the in-memory journal dropped as past their grace leave the DB in the
/// transaction that journals the next write, so the table holds the window memory does.
#[test]
fn journalling_a_write_retires_the_expired_records_it_names() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let uuids: Vec<Uuid> = (1..=4).map(Uuid::from_u128).collect();
	for uuid in &uuids[..3] {
		store
			.record_pending(&journal_row(pair, *uuid, created("a.txt"), NOW), &[], &[])
			.unwrap();
	}

	store
		.record_pending(
			&journal_row(pair, uuids[3], created("b.txt"), NOW),
			&[],
			&uuids[..2],
		)
		.unwrap();
	assert_eq!(
		store
			.load_pending(NOW, GRACE)
			.unwrap()
			.iter()
			.map(|row| row.uuid)
			.collect::<BTreeSet<_>>(),
		BTreeSet::from([uuids[2], uuids[3]])
	);
}

/// A uuid written twice minutes apart — a file moved, then moved again — names its own expired
/// record among the ones to retire. The retirement runs first, or the transaction would drop
/// the write it is journalling.
#[test]
fn a_write_whose_own_earlier_record_expired_survives_its_retirement() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let uuid = Uuid::from_u128(1);
	let moved = |from: &str, to: &str| PendingKind::Moved {
		from: from.to_string(),
		to: to.to_string(),
	};
	store
		.record_pending(
			&journal_row(pair, uuid, moved("a.txt", "b.txt"), NOW),
			&[],
			&[],
		)
		.unwrap();

	let later = NOW + GRACE + 1;
	store
		.record_pending(
			&journal_row(pair, uuid, moved("b.txt", "c.txt"), later),
			&[],
			&[uuid],
		)
		.unwrap();
	assert_eq!(
		store.load_pending(later, GRACE).unwrap(),
		vec![PendingRow {
			pair,
			uuid,
			kind: moved("b.txt", "c.txt"),
			recorded_at: later,
		}]
	);
}

#[test]
fn a_removed_pairs_id_is_never_handed_to_a_later_pair() {
	let store = BaselineStore::open_in_memory().unwrap();
	let remote = Uuid::new_v4();
	let (pair, _) = store
		.create_pair("/root", remote, SyncMode::TwoWay)
		.unwrap();
	store.delete_pair(pair).unwrap();

	let (readded, _) = store
		.create_pair("/root", remote, SyncMode::TwoWay)
		.unwrap();
	assert_ne!(readded, pair, "the removed pair's id came back");
	// So a write from the removed pair's in-flight pass has nowhere to land.
	assert!(
		store
			.upsert_entry(pair, &file_entry("f.txt", [1; 32], 3))
			.is_err(),
		"a baseline write for the removed pair was accepted"
	);
}

#[test]
fn create_pair_is_idempotent_and_reports_the_stored_mode() {
	let store = BaselineStore::open_in_memory().unwrap();
	let remote = Uuid::new_v4();
	let (id, mode) = store
		.create_pair("/home/u/sync", remote, SyncMode::TwoWay)
		.unwrap();
	assert_eq!(
		mode,
		SyncMode::TwoWay,
		"a new pair takes the asked-for mode"
	);

	// Re-registering the same roots returns the same id and the mode ALREADY stored — an
	// existing pair is never silently re-pointed by a registration call.
	let (again, stored) = store
		.create_pair("/home/u/sync", remote, SyncMode::LocalToRemote)
		.unwrap();
	assert_eq!(id, again, "re-registering a pair returns the same id");
	assert_eq!(
		stored,
		SyncMode::TwoWay,
		"the stored mode is reported back, not overwritten"
	);
	assert_eq!(store.pair(id).unwrap().unwrap().mode, SyncMode::TwoWay);

	// Changing it is an explicit, separate operation.
	assert_eq!(store.set_mode(id, SyncMode::LocalToRemote, &[]).unwrap(), 1);
	assert_eq!(
		store.pair(id).unwrap().unwrap().mode,
		SyncMode::LocalToRemote
	);
	assert_eq!(store.set_mode(9999, SyncMode::TwoWay, &[]).unwrap(), 0);
	assert_eq!(store.list_pairs().unwrap().len(), 1);
}

/// A `Backlog::AdoptDestination` switch writes its re-seeded rows and the new mode together:
/// a mode that took effect without them is the propagating behaviour the caller did not ask
/// for, and the very next pass would act on it.
#[test]
fn a_mode_change_and_the_rows_it_adopts_land_together() {
	let path = temp_db_path("reconfigure");
	let remote = Uuid::new_v4();
	let adopted = BaselineEntry {
		state: BaselineState::Adopted,
		..file_entry("gone.txt", [4; 32], 7)
	};
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		let (pair, _) = store
			.create_pair("/root", remote, SyncMode::LocalBackup)
			.unwrap();
		assert_eq!(
			store
				.set_mode(
					pair,
					SyncMode::LocalToRemote,
					std::slice::from_ref(&adopted)
				)
				.unwrap(),
			1
		);
		pair
	};

	let reopened = BaselineStore::open(&path).unwrap();
	assert_eq!(
		reopened.pair(pair).unwrap().unwrap().mode,
		SyncMode::LocalToRemote
	);
	assert_eq!(
		reopened.entry(pair, "gone.txt").unwrap().as_ref(),
		Some(&adopted),
		"the adopted row must survive with its state intact"
	);

	// An unknown pair changes nothing at all — neither a mode nor a stray baseline row.
	assert_eq!(
		reopened
			.set_mode(
				pair + 9_999,
				SyncMode::TwoWay,
				std::slice::from_ref(&adopted)
			)
			.unwrap(),
		0
	);
	assert_eq!(reopened.entries(pair + 9_999).unwrap().len(), 0);
	assert_eq!(
		reopened.pair(pair).unwrap().unwrap().mode,
		SyncMode::LocalToRemote
	);
	drop(reopened);
	std::fs::remove_file(&path).ok();
}

/// What a pass reads through [`BaselineStore::baseline`] is the rows the store holds, every
/// derived column and count with them: held to the resident tree built from the same rows, over
/// every question a pass asks. The rows go in out of path order, a child before its own
/// directory, and include every derived column's interesting case.
#[test]
fn the_store_reads_back_what_a_tree_of_its_rows_answers() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	assert_eq!(
		store.baseline(pair).unwrap().len(),
		0,
		"a pair with no rows reads no rows"
	);
	for entry in [
		file_entry("a/deep/b.txt", [9u8; 32], 4242),
		dir_entry("a"),
		file_entry("c.txt", [1u8; 32], 7),
		dir_entry("a/deep"),
		file_entry("a/.filenignore", [2u8; 32], 3),
		BaselineEntry {
			agreed_hash: None,
			..file_entry("A/pushed.txt", [3u8; 32], 3)
		},
		BaselineEntry {
			state: BaselineState::Conflicted,
			..file_entry("a/deep/held.txt", [4u8; 32], 4)
		},
		BaselineEntry {
			remote_uuid: None,
			..file_entry("\u{130}.txt", [5u8; 32], 5)
		},
	] {
		store.upsert_entry(pair, &entry).unwrap();
	}
	assert_reads_its_rows(&store, pair, "eight rows");
	assert_eq!(store.baseline(pair).unwrap().len(), 8);
}

/// The store's reader for `pair`, held to the resident tree built from what the table holds.
fn assert_reads_its_rows(store: &BaselineStore, pair: PairId, what: &str) {
	let rows = store.entries(pair).unwrap();
	let probes = super::super::rows::tests::probes(
		rows.iter()
			.map(|row| row.rel_path.clone())
			.chain(["new/path".to_string()]),
	);
	super::super::rows::tests::assert_alike(
		what,
		&store.baseline(pair).unwrap(),
		&super::super::tree::Tree::from_rows(rows),
		&probes,
	);
}

#[test]
fn entries_round_trip_with_full_fidelity() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	let f = file_entry("a/b.txt", [9u8; 32], 4242);
	let d = dir_entry("a");
	store.upsert_entry(pair, &f).unwrap();
	store.upsert_entry(pair, &d).unwrap();

	assert_eq!(store.entry(pair, "a/b.txt").unwrap().as_ref(), Some(&f));
	assert_eq!(store.entry(pair, "a").unwrap().as_ref(), Some(&d));
	assert_eq!(store.entry(pair, "missing").unwrap(), None);

	// Ordered by rel_path: "a" before "a/b.txt".
	let all = store.entries(pair).unwrap();
	assert_eq!(
		all.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(),
		vec!["a", "a/b.txt"]
	);
}

/// Both conflict states come back, in path order, and nothing else does: a synced or adopted row
/// is not a conflict, and another pair's conflict is not this pair's.
#[test]
fn conflicts_are_read_by_state_in_path_order() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let with_state = |rel_path: &str, state| BaselineEntry {
		state,
		..file_entry(rel_path, [1u8; 32], 1)
	};
	for (rel_path, state) in [
		("b.txt", BaselineState::Conflicted),
		("a.txt", BaselineState::Overwritten),
		("c.txt", BaselineState::Synced),
		("d.txt", BaselineState::Adopted),
	] {
		store
			.upsert_entry(pair, &with_state(rel_path, state))
			.unwrap();
	}
	store
		.upsert_entry(other, &with_state("z.txt", BaselineState::Conflicted))
		.unwrap();

	assert_eq!(
		store
			.conflicts(pair)
			.unwrap()
			.iter()
			.map(|entry| (entry.rel_path.as_str(), entry.state))
			.collect::<Vec<_>>(),
		vec![
			("a.txt", BaselineState::Overwritten),
			("b.txt", BaselineState::Conflicted)
		]
	);
	assert_eq!(store.conflicts(other).unwrap().len(), 1);
}

#[test]
fn upsert_replaces_and_delete_removes() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::RemoteToLocal)
		.unwrap();

	let mut f = file_entry("x.txt", [1u8; 32], 10);
	store.upsert_entry(pair, &f).unwrap();
	f.size = Some(20);
	f.content_hash = Some(Blake3Hash::from([2u8; 32]));
	store.upsert_entry(pair, &f).unwrap();
	assert_eq!(store.entry(pair, "x.txt").unwrap(), Some(f.clone()));
	assert_eq!(store.entries(pair).unwrap().len(), 1, "replace, not insert");

	store.delete_entry(pair, "x.txt").unwrap();
	assert_eq!(store.entry(pair, "x.txt").unwrap(), None);
}

/// Every path that writes a baseline row keeps the columns and counts a pass reads true to the
/// rows: a count that drifted, or a folded path or flag a move did not carry, is a pass reading
/// rows that are not there — a fabricated absence — or missing rows that are. Each write is
/// followed by the whole reader held to a tree built from the table's rows, including the two
/// `UPDATE OR REPLACE` moves, whose overwritten rows only the delete trigger counts out.
#[test]
fn every_write_path_keeps_what_a_pass_reads_true_to_the_rows() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	assert!(store.baseline(pair).unwrap().is_empty());
	let agrees = |store: &BaselineStore, what: &str| {
		assert_reads_its_rows(store, pair, &format!("after {what}"));
	};

	store
		.upsert_entry(pair, &file_entry("a/x.txt", [1; 32], 1))
		.unwrap();
	store.upsert_entry(pair, &dir_entry("a")).unwrap();
	store.upsert_entry(pair, &dir_entry("a/sub")).unwrap();
	store
		.upsert_entry(pair, &file_entry("a/sub/y.txt", [2; 32], 2))
		.unwrap();
	store.upsert_entry(pair, &dir_entry("ab")).unwrap();
	store
		.upsert_entry(pair, &file_entry("a/sub/.filenignore", [6; 32], 6))
		.unwrap();
	store
		.upsert_entry(
			pair,
			&BaselineEntry {
				agreed_hash: None,
				..file_entry("a/pushed.txt", [7; 32], 7)
			},
		)
		.unwrap();
	store
		.upsert_entry(
			pair,
			&BaselineEntry {
				remote_uuid: None,
				..file_entry("b/sub/lost.txt", [8; 32], 8)
			},
		)
		.unwrap();
	agrees(&store, "upsert_entry");
	// Rewritten in place: one-sided, then whole again — the flag and both counts follow.
	store
		.upsert_entry(
			pair,
			&BaselineEntry {
				remote_uuid: None,
				..file_entry("a/x.txt", [1; 32], 1)
			},
		)
		.unwrap();
	agrees(&store, "upsert_entry, now uncarryable");

	store.delete_entry(pair, "ab").unwrap();
	agrees(&store, "delete_entry");

	let replaced = file_entry("a/x.txt", [3; 32], 3);
	store
		.apply_changes(
			pair,
			&[
				BaselineChange::Upsert(&replaced),
				BaselineChange::Upsert(&dir_entry("b")),
			],
		)
		.unwrap();
	agrees(&store, "apply_changes(Upsert)");

	// Onto an OCCUPIED destination, which the two `UPDATE OR REPLACE` statements overwrite.
	store
		.apply_changes(pair, &[BaselineChange::MoveSubtree { from: "a", to: "b" }])
		.unwrap();
	agrees(&store, "apply_changes(MoveSubtree)");

	// A case-only rename shares no key with itself under a bytewise comparison.
	store
		.apply_changes(pair, &[BaselineChange::MoveSubtree { from: "b", to: "B" }])
		.unwrap();
	agrees(&store, "apply_changes(MoveSubtree), case only");

	// A file renamed INTO being a rule file, and one renamed out of it: the one row whose leaf
	// a move changes.
	store
		.apply_changes(
			pair,
			&[
				BaselineChange::MoveSubtree {
					from: "B/pushed.txt",
					to: "B/.filenignore",
				},
				BaselineChange::MoveSubtree {
					from: "B/sub/.filenignore",
					to: "B/sub/rules.txt",
				},
			],
		)
		.unwrap();
	agrees(&store, "apply_changes(MoveSubtree), a leaf renamed");

	store
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("B/x.txt"), NOW),
			&[
				BaselineChange::Upsert(&file_entry("B/x.txt", [4; 32], 4)),
				BaselineChange::Delete("B/sub/y.txt"),
			],
			&[],
		)
		.unwrap();
	agrees(&store, "record_pending");

	// Moves whose ends are not ASCII, which carry `folded_path` by a prefix swap counted in
	// characters: into a directory named by the dotted İ, whose folding is LONGER than it, and
	// Ä; then out of it again under Greek capitals, whose folding is not what `lower()` makes.
	// An ASCII move cannot tell a swap counted on the wrong column from the right one.
	store
		.apply_changes(
			pair,
			&[BaselineChange::MoveSubtree {
				from: "B/sub",
				to: "İ/Äsub",
			}],
		)
		.unwrap();
	agrees(&store, "apply_changes(MoveSubtree), into non-ASCII ends");
	store
		.apply_changes(
			pair,
			&[BaselineChange::MoveSubtree {
				from: "İ",
				to: "ΣΑΣ/İİ",
			}],
		)
		.unwrap();
	agrees(&store, "apply_changes(MoveSubtree), between non-ASCII ends");
	let moved = store.baseline(pair).unwrap();
	assert!(
		moved.contains_key("ΣΑΣ/İİ/Äsub") && moved.contains_key("ΣΑΣ/İİ/Äsub/rules.txt"),
		"the two moves carried nothing, so they checked nothing"
	);
	// Another spelling of the moved file finds it through the folded path the move carried.
	assert!(moved.occupied("σασ/İİ/äSUB/RULES.TXT"));
	drop(moved);

	store
		.set_mode(
			pair,
			SyncMode::LocalToRemote,
			&[
				dir_entry("adopted"),
				file_entry("adopted/z.txt", [5; 32], 5),
			],
		)
		.unwrap();
	agrees(&store, "set_mode");

	store
		.delete_subtrees(pair, &BTreeSet::from(["B".to_string()]))
		.unwrap();
	agrees(&store, "delete_subtrees");
}

/// Two snapshots open at once — a dry run beside a pass — leave ONE reader connection behind
/// when they end, not two: each idle reader is a connection held open for the life of the store.
#[test]
fn overlapping_snapshots_leave_one_idle_reader() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store.upsert_entry(pair, &dir_entry("a")).unwrap();
	let idle = || {
		store
			.readers
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.len()
	};
	let (first, second) = (store.baseline(pair).unwrap(), store.baseline(pair).unwrap());
	assert!(first.contains_key("a") && second.contains_key("a"));
	assert_eq!(idle(), 0, "both readers are in use");
	drop((first, second));
	assert_eq!(
		idle(),
		1,
		"the store keeps one idle reader, and closes the rest"
	);
	// And the next pass reuses it.
	assert!(store.baseline(pair).unwrap().contains_key("a"));
	assert_eq!(idle(), 1);
}

/// A pass pages the pair's rows through its reader's cache and parks the reader with that cache
/// full; the end of the pass gives it back, so the parked connection's next read of a page goes
/// to the file again. A release reaches every connection on this build (see
/// [`release_page_cache`]), so the writer's half alone passes this too: it fails when the end of
/// a pass gives nothing back.
#[test]
fn the_end_of_a_pass_gives_back_the_parked_readers_page_cache() {
	let store = BaselineStore::open_in_memory().unwrap();
	store
		.conn
		.execute_batch(
			"CREATE TABLE probe (v TEXT); INSERT INTO probe VALUES ('kept-page'); \
				 PRAGMA wal_checkpoint(TRUNCATE);",
		)
		.unwrap();
	let read = |conn: &Connection| -> String {
		conn.query_row("SELECT v FROM probe", [], |row| row.get(0))
			.unwrap()
	};
	let idle = || {
		store
			.readers
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.len()
	};
	let reader = store.readers.take().unwrap();
	assert_eq!(read(&reader), "kept-page");
	rewrite_behind_sqlite(&store.readers.path, b"kept-page", b"gone-page");

	store.readers.give(reader);
	assert_eq!(idle(), 1, "the reader is parked, not closed");
	store.release_page_caches();
	let reader = store.readers.take().unwrap();
	assert_eq!(idle(), 0, "the parked reader is the one taken back");

	assert_eq!(
		read(&reader),
		"gone-page",
		"the page cache was given back, so the read went to the file"
	);
}

/// The rules [`Derived`] computes its columns by, pinned against the schema version the stored
/// columns were written under. A change to any of them fails here, and the message says what
/// the change owes: a [`SCHEMA_VERSION`] bump, then this test's pins moved to match.
#[test]
fn the_derived_columns_are_pinned_to_the_schema_version() {
	let owed = "a rule the stored derived columns were computed by changed (see `Derived`): bump \
		            SCHEMA_VERSION so a file written under the old rule is refused, then move this pin";
	assert_eq!(
		(SCHEMA_VERSION, char::UNICODE_VERSION),
		(4, (17, 0, 0)),
		"{owed}"
	);
	// Confirmed: this helper's rows otherwise carry a marker that is not their content.
	let confirmed = |row: BaselineEntry| BaselineEntry {
		agreed_hash: row.content_hash,
		..row
	};
	let synced = confirmed(file_entry("d/f.txt", [1; 32], 1));
	let dir = dir_entry("d");
	// Each row with exactly one thing different from a synced file, and what each rule answers.
	let cases: [(&str, BaselineEntry, [bool; 3]); 10] = [
		("synced file", synced.clone(), [true, false, false]),
		("directory", dir.clone(), [true, false, false]),
		(
			"no remote uuid",
			BaselineEntry {
				remote_uuid: None,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"no content hash",
			BaselineEntry {
				content_hash: None,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"no size",
			BaselineEntry {
				size: None,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"no local mtime",
			BaselineEntry {
				local_mtime: None,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"no remote stamp",
			BaselineEntry {
				remote_modified: None,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"conflicted",
			BaselineEntry {
				state: BaselineState::Conflicted,
				..synced.clone()
			},
			[false, false, false],
		),
		(
			"pushed, unconfirmed",
			BaselineEntry {
				agreed_hash: None,
				..synced.clone()
			},
			[true, false, true],
		),
		(
			"rule file",
			confirmed(file_entry("d/.filenignore", [1; 32], 1)),
			[true, true, false],
		),
	];
	for (what, row, [carryable, rule_file, unconfirmed]) in cases {
		let derived = Derived::of(&row);
		assert_eq!(
			(derived.carryable, derived.rule_file, derived.unconfirmed),
			(carryable, rule_file, unconfirmed),
			"{what}: {owed}"
		);
	}
	// The foldings the collision check depends on, where `char::to_lowercase` and ASCII lowering
	// part: the dotted İ lowers LONGER than itself, a capital sigma never to the final form.
	for (path, folded) in [
		("Ä/İ.txt", "ä/i\u{307}.txt"),
		("ΣΑΣ", "σασ"),
		("Документы", "документы"),
	] {
		assert_eq!(Derived::of(&dir_entry(path)).folded_path, folded, "{owed}");
	}
}

/// A pass's read transaction ends when its plan is made. Held through the apply, it would pin
/// the WAL: no checkpoint reaches past an open reader, so every commit made meanwhile — one per
/// applied action, here and on every other pair of the file — would grow the `-wal` file, which
/// SQLite never shrinks back while a connection is open. Once its actions have taken the rows
/// they read, the pass holds no transaction, and the same commits leave the file where
/// autocheckpoint keeps it.
#[test]
fn a_pass_whose_actions_took_their_rows_leaves_the_wal_to_its_checkpoints() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let seeded: Vec<BaselineEntry> = (0..20_000)
		.map(|n| file_entry(&format!("d{}/f{n:05}.txt", n % 50), [1; 32], n))
		.collect();
	store
		.apply_changes(
			pair,
			&seeded
				.iter()
				.map(BaselineChange::Upsert)
				.collect::<Vec<_>>(),
		)
		.unwrap();
	// The seeding transaction's own frames out of the way, so what follows is the apply's.
	store
		.conn
		.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
		.unwrap();
	let mut wal = store.readers.path.clone().into_os_string();
	wal.push("-wal");
	let wal = PathBuf::from(wal);

	// A pass: its plan reads rows through the snapshot, and its apply commits one action at a
	// time while it goes on reading the rows its actions took.
	let pass = store.baseline(pair).unwrap();
	assert!(pass.get("d0/f00000.txt").is_some());
	let (_dir, stage) = Stage::in_temp_dir();
	let applying = PlannedEntry::stage_actions(
		&[
			SyncAction::DownloadFile {
				rel_path: "d0/f00000.txt".to_string(),
				remote_uuid: Uuid::new_v4(),
			},
			SyncAction::UploadFile {
				rel_path: "new/f00000.txt".to_string(),
			},
		],
		pass,
		Side::Whole(HashMap::new()),
		Side::Whole(HashMap::new()),
		stage,
	)
	.unwrap();
	for n in 0..1_500_u64 {
		let path = format!("new/f{n:05}.txt");
		store
			.record_pending(
				&journal_row(pair, Uuid::new_v4(), created(&path), NOW),
				&[BaselineChange::Upsert(&file_entry(&path, [2; 32], n))],
				&[],
			)
			.unwrap();
	}
	// Still the rows the plan was made from, not the ones the pass has since committed.
	let mut read = Vec::new();
	applying
		.pager()
		.sweep(None, |entry| {
			read.push(match entry.start {
				PassStart::Download { base, .. } | PassStart::Upload { base, .. } => {
					base.map(|row| row.rel_path)
				}
				other => panic!("a transfer staged as {other:?}"),
			})
		})
		.unwrap();
	assert_eq!(read, [Some("d0/f00000.txt".to_string()), None]);
	// Autocheckpoint lets the log reach 1000 pages before it copies them back and rewinds.
	let bound = 2 * 1000 * PAGE_SIZE as u64;
	let grown = std::fs::metadata(&wal).unwrap().len();
	assert!(
		grown <= bound,
		"1500 commits under a pass's apply grew the WAL to {:.1} MiB, past the {:.1} MiB \
			 autocheckpoint keeps it to: the pass is holding a read transaction open",
		grown as f64 / (1024.0 * 1024.0),
		bound as f64 / (1024.0 * 1024.0),
	);
}

/// A write that FAILED changed no row, so what a pass reads — the rows, and the counts the
/// triggers keep, which roll back with the statement that fired them — must still describe the
/// DB as it still is.
/// Triggers refuse one path per write shape, which is the only way to fail a write on purpose:
/// contention fails a write before it has run a statement, and what has to be covered here is a
/// write that fails HALF WAY — the second row of an adoption, whose rows land inside the mode
/// switch's own transaction, so a rollback takes back the row the loop already ran.
#[test]
fn a_failed_write_leaves_what_a_pass_reads_describing_the_db() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	for entry in [
		dir_entry("guard"),
		file_entry("guard/x.txt", [1; 32], 1),
		file_entry("kept.txt", [2; 32], 2),
	] {
		store.upsert_entry(pair, &entry).unwrap();
	}
	let rows = |store: &BaselineStore| -> HashMap<String, BaselineEntry> {
		store
			.baseline(pair)
			.unwrap()
			.iter()
			.map(|entry| (entry.rel_path.clone(), entry))
			.collect()
	};
	let before = rows(&store);
	assert_eq!(before.len(), 3);

	store
		.conn
		.execute_batch(
			"CREATE TRIGGER no_insert BEFORE INSERT ON baseline WHEN NEW.rel_path LIKE 'boom%'
				 BEGIN SELECT RAISE(ABORT, 'insert refused'); END;
				 CREATE TRIGGER no_update BEFORE UPDATE ON baseline WHEN NEW.rel_path LIKE 'boom%'
				 BEGIN SELECT RAISE(ABORT, 'update refused'); END;
				 CREATE TRIGGER no_delete BEFORE DELETE ON baseline WHEN OLD.rel_path LIKE 'guard%'
				 BEGIN SELECT RAISE(ABORT, 'delete refused'); END;",
		)
		.unwrap();

	let agrees = |what: &str| {
		let from_db: HashMap<String, BaselineEntry> = store
			.entries(pair)
			.unwrap()
			.into_iter()
			.map(|entry| (entry.rel_path.clone(), entry))
			.collect();
		assert_eq!(rows(&store), from_db, "after a failed {what}");
		assert_eq!(rows(&store), before, "after a failed {what}");
		assert_reads_its_rows(&store, pair, &format!("after a failed {what}"));
	};

	let boom = file_entry("boom.txt", [3; 32], 3);
	store
		.upsert_entry(pair, &boom)
		.expect_err("the insert trigger must refuse this row");
	agrees("upsert_entry");

	store
		.delete_entry(pair, "guard/x.txt")
		.expect_err("the delete trigger must refuse this row");
	agrees("delete_entry");

	// The refused row is the SECOND of the batch: the first one ran.
	store
		.apply_changes(
			pair,
			&[
				BaselineChange::Upsert(&file_entry("kept.txt", [4; 32], 4)),
				BaselineChange::Upsert(&boom),
			],
		)
		.expect_err("the insert trigger must refuse the second change");
	agrees("apply_changes");

	store
		.apply_changes(
			pair,
			&[BaselineChange::MoveSubtree {
				from: "kept.txt",
				to: "boom/moved",
			}],
		)
		.expect_err("the update trigger must refuse the destination");
	agrees("apply_changes(MoveSubtree)");

	store
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("boom.txt"), NOW),
			&[BaselineChange::Upsert(&boom)],
			&[],
		)
		.expect_err("the insert trigger must refuse the row this journals");
	agrees("record_pending");

	store
		.delete_subtrees(pair, &BTreeSet::from(["guard".to_string()]))
		.expect_err("the delete trigger must refuse the root");
	agrees("delete_subtrees");

	// The adoption re-seed: the first row runs, the second is refused, and the rollback takes
	// the mode change AND the first row back with it.
	store
		.set_mode(
			pair,
			SyncMode::LocalToRemote,
			&[dir_entry("adopted"), boom.clone()],
		)
		.expect_err("the insert trigger must refuse the second adopted row");
	assert_eq!(
		store.pair(pair).unwrap().unwrap().mode,
		SyncMode::TwoWay,
		"the mode switch rolled back with its rows"
	);
	agrees("set_mode");
}

/// A pass holds the copy it read while it writes the rows it advances: its own view must not
/// change underneath it, and the store's must.
#[test]
fn a_write_leaves_the_copy_a_pass_already_took_alone() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.upsert_entry(pair, &file_entry("x.txt", [1; 32], 1))
		.unwrap();

	let read_by_the_pass = store.baseline(pair).unwrap();
	store
		.upsert_entry(pair, &file_entry("later.txt", [2; 32], 2))
		.unwrap();

	assert_eq!(read_by_the_pass.len(), 1, "the pass's own view moved");
	assert_eq!(
		store.baseline(pair).unwrap().len(),
		2,
		"the store's did not"
	);
}

/// A pair that is gone takes its rows and its counts with it.
#[test]
fn deleting_a_pair_drops_its_rows_and_counts() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.upsert_entry(pair, &file_entry("x.txt", [1; 32], 1))
		.unwrap();
	assert_eq!(store.baseline(pair).unwrap().len(), 1);

	store.delete_pair(pair).unwrap();
	assert!(store.baseline(pair).unwrap().is_empty());
	let counted: i64 = store
		.conn
		.query_row(
			"SELECT COUNT(*) FROM baseline_counts WHERE pair_id = ?1",
			params![pair],
			|row| row.get(0),
		)
		.unwrap();
	assert_eq!(counted, 0, "the pair's counts row went with it");
}

/// A directory move re-keys the row at the source and every row under it, keeps everything else
/// those rows hold, and touches nothing outside: not a sibling that merely shares the prefix,
/// not another pair. A case-only rename is a move like any other — the key compares bytewise,
/// so the new spelling is free — and each step re-keys from wherever the rows are now.
#[test]
fn a_subtree_move_re_keys_the_source_and_everything_under_it_only() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	for rel in ["a", "a/x", "b"] {
		store.upsert_entry(pair, &dir_entry(rel)).unwrap();
	}
	let deep = file_entry("a/x/y.txt", [7u8; 32], 11);
	store.upsert_entry(pair, &deep).unwrap();
	for rel in ["ab", "a0", "a."] {
		store
			.upsert_entry(pair, &file_entry(rel, [1u8; 32], 1))
			.unwrap();
	}
	store.upsert_entry(other, &dir_entry("a")).unwrap();
	let paths = |pair: PairId| -> Vec<String> {
		store
			.entries(pair)
			.unwrap()
			.into_iter()
			.map(|entry| entry.rel_path)
			.collect()
	};
	let move_subtree = |from, to| {
		store
			.apply_changes(pair, &[BaselineChange::MoveSubtree { from, to }])
			.unwrap();
	};

	move_subtree("a", "b/moved");
	assert_eq!(
		paths(pair),
		[
			"a.",
			"a0",
			"ab",
			"b",
			"b/moved",
			"b/moved/x",
			"b/moved/x/y.txt"
		]
	);
	assert_eq!(
		store.entry(pair, "b/moved/x/y.txt").unwrap(),
		Some(BaselineEntry {
			rel_path: "b/moved/x/y.txt".to_string(),
			..deep
		}),
		"a moved row must keep everything but its path"
	);
	assert_eq!(paths(other), ["a"], "another pair's rows do not move");

	move_subtree("b/moved", "b/MOVED");
	assert_eq!(
		paths(pair),
		[
			"a.",
			"a0",
			"ab",
			"b",
			"b/MOVED",
			"b/MOVED/x",
			"b/MOVED/x/y.txt"
		]
	);
	move_subtree("b/MOVED/x", "b/MOVED/x2");
	assert_eq!(
		paths(pair),
		[
			"a.",
			"a0",
			"ab",
			"b",
			"b/MOVED",
			"b/MOVED/x2",
			"b/MOVED/x2/y.txt"
		]
	);
}

/// A move onto a destination that already holds rows overwrites them, which is what the
/// read-delete-upsert this replaced did. One caller can still ask for it: the case-only rename
/// of a pushing mode emits its move without checking the local end, so rows left under both
/// spellings by an earlier half-finished rename arrive here — after the directory has already
/// been renamed on disk. Failing there would leave the store describing one directory under two
/// names.
#[test]
fn a_subtree_move_overwrites_rows_sitting_at_the_destination() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	for rel in ["Docs", "docs"] {
		store.upsert_entry(pair, &dir_entry(rel)).unwrap();
	}
	store
		.upsert_entry(pair, &file_entry("Docs/a.txt", [1u8; 32], 1))
		.unwrap();
	store
		.upsert_entry(pair, &file_entry("docs/a.txt", [2u8; 32], 2))
		.unwrap();

	store
		.apply_changes(
			pair,
			&[BaselineChange::MoveSubtree {
				from: "Docs",
				to: "docs",
			}],
		)
		.unwrap();

	assert_eq!(
		store
			.entries(pair)
			.unwrap()
			.iter()
			.map(|e| e.rel_path.as_str())
			.collect::<Vec<_>>(),
		vec!["docs", "docs/a.txt"],
		"one spelling is left, and it is the one the move named"
	);
	assert_eq!(
		store
			.entry(pair, "docs/a.txt")
			.unwrap()
			.and_then(|e| e.size),
		Some(1),
		"the row that moved is the one that survived"
	);
}

/// A row with a CHOSEN uuid and whole-life id, so a lookup can be aimed at it.
fn identified(rel_path: &str, uuid: Uuid) -> BaselineEntry {
	BaselineEntry {
		remote_uuid: Some(uuid),
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		..file_entry(rel_path, [7u8; 32], 3)
	}
}

/// The id lookup answers off the store's own indexes, for both columns, for more ids than one
/// statement binds, and for this pair only.
///
/// Past [`SYNCED_CHUNK`] on purpose: the statement is a fixed-width `IN` list whose last chunk
/// is padded by repeating one of its own members, so a padding bug either loses the tail or
/// answers for a row nobody asked about. The pair filter matters just as much — an id is unique
/// to the server, not to a pair's rows, and a stranger's row answering here would place an item
/// at a path in a tree it has nothing to do with.
#[test]
fn synced_paths_answers_by_uuid_and_lineage_past_one_chunk() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let ids: Vec<Uuid> = (0..SYNCED_CHUNK + 5)
		.map(|n| Uuid::from_u128(n as u128 + 1))
		.collect();
	for (n, &uuid) in ids.iter().enumerate() {
		store
			.upsert_entry(pair, &identified(&format!("f{n}.txt"), uuid))
			.unwrap();
	}
	let stranger = Uuid::from_u128(9_000);
	store
		.upsert_entry(other, &identified("elsewhere.txt", stranger))
		.unwrap();

	let lineages: Vec<StableUuid> = ids.iter().copied().map(StableUuid::new_for_test).collect();
	let mut asked = ids.clone();
	asked.push(stranger);
	let found = store.synced_paths(pair, &asked, &lineages).unwrap();

	for (n, &uuid) in ids.iter().enumerate() {
		let want = format!("f{n}.txt");
		assert_eq!(found.path_by_uuid(uuid).as_deref(), Some(want.as_str()));
		assert_eq!(
			found
				.path_by_lineage(StableUuid::new_for_test(uuid))
				.as_deref(),
			Some(want.as_str())
		);
	}
	assert_eq!(
		found.path_by_uuid(stranger),
		None,
		"another pair's row must answer for nothing here"
	);
}

/// Two rows claiming one uuid: the seek finds BOTH and answers with the first by path.
///
/// This is a deliberate change from the resident map it replaces. That map was
/// last-writer-wins over the load order, so the id answered for whichever row was rewritten
/// last — and answered `None` as soon as that row lost the id, even though another row still
/// carried it. `None` is the dangerous answer here: `unknown_remote_paths` reads it as an item
/// that was never synced, which drops the protection that keeps a pass from deleting the local
/// copy of a remote item that is still there. Deterministic, and never absent while some row
/// carries the id, is what this pins.
#[test]
fn two_rows_claiming_one_uuid_answer_with_the_first_by_path() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let shared = Uuid::from_u128(77);
	store
		.upsert_entry(pair, &identified("b.txt", shared))
		.unwrap();
	store
		.upsert_entry(pair, &identified("a.txt", shared))
		.unwrap();

	let found = store.synced_paths(pair, &[shared], &[]).unwrap();
	assert_eq!(
		found.path_by_uuid(shared).as_deref(),
		Some("a.txt"),
		"of two claimants the first by path answers, rather than neither"
	);
}

/// An id nobody resolved is a BUG, not an absence.
///
/// The whole safety of resolving ids up front rests on the enumeration being exhaustive
/// (`remote::delta_uuids`, `plan::skipped_ids`). An id that slipped through it would otherwise
/// read as "no row records this", and on the remote side that is a move whose source is never
/// vacated — the item left sitting at two paths in a view the pass then plans against.
#[test]
#[should_panic(expected = "without that id having been resolved")]
fn asking_where_an_unresolved_id_sits_is_refused() {
	let _ = SyncedPaths::default().path_by_uuid(Uuid::from_u128(1));
}

#[test]
fn delete_subtrees_takes_each_root_and_everything_under_it_only() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	for rel in ["docs", "docs/sub", "ä", "other.txt", "a", "a/b"] {
		store.upsert_entry(pair, &dir_entry(rel)).unwrap();
	}
	for rel in [
		"docs/a.txt",
		"docs/sub/b.txt",
		"docsx",
		"docs.txt",
		"ä/b.txt",
		"äb.txt",
		// The neighbours of the subtree range: `.` (0x2E) sorts just below the `/` the range
		// starts at and `0` (0x30) just above it, so a range that is off by one byte at either
		// end takes one of these with it.
		"a/b/c",
		"ab",
		"a0",
		"a.",
	] {
		store
			.upsert_entry(pair, &file_entry(rel, [1u8; 32], 1))
			.unwrap();
	}
	store.upsert_entry(other, &dir_entry("docs")).unwrap();

	store
		.delete_subtrees(
			pair,
			&BTreeSet::from(["docs".to_string(), "ä".to_string(), "a".to_string()]),
		)
		.unwrap();
	assert_eq!(
		store
			.entries(pair)
			.unwrap()
			.iter()
			.map(|e| e.rel_path.as_str())
			.collect::<Vec<_>>(),
		vec!["a.", "a0", "ab", "docs.txt", "docsx", "other.txt", "äb.txt"]
	);
	assert!(
		store.entry(other, "docs").unwrap().is_some(),
		"another pair's rows stay"
	);
}

/// Whether the conflict-state index is in the file.
fn has_state_index(store: &BaselineStore) -> bool {
	store
		.conn
		.query_row(
			"SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'baseline_state'",
			[],
			|row| row.get::<_, i64>(0),
		)
		.unwrap()
		== 1
}

/// The conflict read must SEEK the `(pair_id, state)` index rather than walk the pair's rows, on
/// the very statement the read runs. The plan with the index dropped is asserted too, so the test
/// cannot pass on a plan that never mentions an index at all.
#[test]
fn the_conflict_read_seeks_the_state_index() {
	let store = BaselineStore::open_in_memory().unwrap();
	let plan = |store: &BaselineStore, sql: &str| -> Vec<String> {
		store
			.conn
			.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
			.unwrap()
			.query_map(
				params![
					1_i64,
					BaselineState::Conflicted.as_i64(),
					BaselineState::Overwritten.as_i64()
				],
				|row| row.get("detail"),
			)
			.unwrap()
			.collect::<rusqlite::Result<_>>()
			.unwrap()
	};

	let indexed = plan(&store, &conflict_rows_sql());
	assert!(
		indexed
			.iter()
			.any(|step| step.starts_with("SEARCH baseline USING INDEX baseline_state")),
		"the conflict read must seek the state index: {indexed:?}"
	);

	store.set_state_index(false).unwrap();
	// Without the index the read refuses to run rather than walking the pair in silence…
	assert!(
		store
			.conn
			.prepare(&format!("EXPLAIN QUERY PLAN {}", conflict_rows_sql()))
			.is_err(),
		"the conflict read must name its index, not fall back to a walk"
	);
	// …and the same read unnamed is exactly that walk.
	let walked = plan(&store, &conflict_rows_walked_sql());
	assert!(
		!walked.iter().any(|step| step.contains("baseline_state")),
		"the control plan must not name an index that is gone: {walked:?}"
	);
	// Without it the read is a walk of every row the pair holds, whichever index carries it
	// there. The primary key is no longer the only candidate: `baseline_remote_uuid` and
	// `baseline_remote_stable_uuid` are `(pair_id, ...)` indexes too, so the planner may seek
	// one of THEM on `pair_id` alone and filter the state out of the rows it finds — the same
	// row count the primary-key walk had, and the cost `baseline_state` exists to remove. What
	// must not survive is a seek that narrows by state.
	assert!(
		walked.iter().any(|step| step.contains("(pair_id=?)")),
		"without it the read walks the pair's rows on pair_id alone: {walked:?}"
	);
}

/// A DB written before the index existed gains it on the next open and is NOT refused for it: the
/// version stamp does not move, so the index is all that separates the two files.
#[test]
fn a_db_without_the_state_index_gains_it_on_open() {
	let path = temp_db_path("state_index");
	let entry = file_entry("a.txt", [5; 32], 9);
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store.upsert_entry(pair, &entry).unwrap();
		// The shape a build from before the index left behind.
		store.set_state_index(false).unwrap();
		assert!(!has_state_index(&store));
		pair
	};

	let reopened = BaselineStore::open(&path).unwrap();
	assert!(
		has_state_index(&reopened),
		"the open must create the index the file was missing"
	);
	assert_eq!(
		version_of(&reopened),
		SCHEMA_VERSION,
		"and must not restamp the file for it"
	);
	assert_eq!(
		reopened.entry(pair, "a.txt").unwrap().as_ref(),
		Some(&entry),
		"the rows it arrived with must read back"
	);
	drop(reopened);
	for suffix in ["", "-wal", "-shm"] {
		std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
	}
}

/// Every statement that works on a whole subtree must SEEK the primary-key index, and the plan
/// has to name the columns it seeks on.
///
/// `SEARCH baseline USING PRIMARY KEY (pair_id=?)` — no `rel_path` term —
/// is what an unsargable predicate produces: a walk of every index entry of the pair, with no
/// `SCAN` step to give it away. Both the `substr` form these replaced and an `OR` of the two
/// ranges plan exactly that, so asserting only "SEARCH, and not SCAN" passes for the very shape
/// this guards against.
#[test]
fn every_subtree_statement_seeks_the_index_instead_of_scanning_the_pair() {
	let store = BaselineStore::open_in_memory().unwrap();
	let assert_seeks = |sql: &str, args: &[&dyn rusqlite::ToSql], seek: &str| {
		let plan: Vec<String> = store
			.conn
			.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
			.unwrap()
			.query_map(args, |row| row.get("detail"))
			.unwrap()
			.collect::<rusqlite::Result<_>>()
			.unwrap();
		assert!(
			// A DELETE reports the same seek as `SEARCH ... USING COVERING INDEX ...`.
			plan.iter()
				.any(|step| step.starts_with("SEARCH baseline USING")
					&& step.contains("PRIMARY KEY")
					&& step.contains(seek)),
			"{sql}\nmust seek on {seek}: {plan:?}"
		);
		assert!(
			!plan.iter().any(|step| step.contains("SCAN")),
			"{sql}\nstill scans: {plan:?}"
		);
	};
	let at_path = "(pair_id=? AND rel_path=?)";
	let under_path = "(pair_id=? AND rel_path>? AND rel_path<?)";

	assert_seeks(DELETE_AT_PATH, &[&1_i64, &"docs"], at_path);
	assert_seeks(DELETE_UNDER_PATH, &[&1_i64, &"docs"], under_path);
	assert_seeks(
		MOVE_AT_PATH,
		&[&1_i64, &"docs", &"moved", &"docs", &"moved", &false],
		at_path,
	);
	assert_seeks(
		MOVE_UNDER_PATH,
		&[&1_i64, &"docs", &"moved", &"docs", &"moved"],
		under_path,
	);
}

#[test]
fn deleting_a_pair_cascades_to_its_baseline_rows() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.upsert_entry(pair, &file_entry("keep.txt", [3u8; 32], 1))
		.unwrap();
	assert_eq!(store.entries(pair).unwrap().len(), 1);

	store.delete_pair(pair).unwrap();
	assert!(store.pair(pair).unwrap().is_none());
	assert!(
		store.entries(pair).unwrap().is_empty(),
		"ON DELETE CASCADE dropped the baseline rows"
	);
}

/// The whole point of persisting the journal: the engine that made the write is gone, and the
/// one that reopens the DB has to find both halves — the record AND the baseline row that says
/// what the write left behind, which is what a fold reads.
#[test]
fn a_pending_write_and_the_row_it_produced_survive_a_reopen() {
	let path = temp_db_path("pending_reopen");
	let entry = file_entry("a.txt", [7u8; 32], 5);
	let uuid = Uuid::new_v4();
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.record_pending(
				&journal_row(pair, uuid, created("a.txt"), NOW),
				&[BaselineChange::Upsert(&entry)],
				&[],
			)
			.unwrap();
		pair
	};

	let reopened = BaselineStore::open(&path).unwrap();
	assert_eq!(
		reopened.load_pending(NOW + 1_000, GRACE).unwrap(),
		vec![PendingRow {
			pair,
			uuid,
			kind: created("a.txt"),
			recorded_at: NOW,
		}]
	);
	assert_eq!(
		reopened.entry(pair, "a.txt").unwrap().as_ref(),
		Some(&entry),
		"the row the write produced is there to be folded"
	);

	// Retiring it is what the next pass does once the cache has caught up.
	reopened.delete_pending(pair, &[uuid]).unwrap();
	assert!(
		reopened
			.load_pending(NOW + 1_000, GRACE)
			.unwrap()
			.is_empty()
	);
	std::fs::remove_file(&path).ok();
}

#[test]
fn every_write_kind_round_trips_through_the_journal() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let replaced = Uuid::new_v4();
	let kinds = [
		PendingKind::Created {
			path: "a.txt".to_string(),
			replaced: Some(replaced),
		},
		PendingKind::Moved {
			from: "a.txt".to_string(),
			to: "b.txt".to_string(),
		},
		PendingKind::Trashed {
			path: "c.txt".to_string(),
		},
	];
	let uuids: Vec<Uuid> = kinds.iter().map(|_| Uuid::new_v4()).collect();
	for (uuid, kind) in uuids.iter().zip(&kinds) {
		store
			.record_pending(&journal_row(pair, *uuid, kind.clone(), NOW), &[], &[])
			.unwrap();
	}

	let mut loaded = store.load_pending(NOW, GRACE).unwrap();
	loaded.sort_by_key(|row| uuids.iter().position(|u| *u == row.uuid).unwrap());
	assert_eq!(
		loaded.into_iter().map(|row| row.kind).collect::<Vec<_>>(),
		kinds.to_vec()
	);
}

/// Past the grace ceiling the snapshot is believed again, so the row must not come back — and
/// it is DELETED rather than filtered, or the journal would grow without bound.
#[test]
fn a_write_past_the_grace_window_is_dropped_on_load() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("old.txt"), NOW - GRACE - 1),
			&[],
			&[],
		)
		.unwrap();
	let fresh = Uuid::new_v4();
	store
		.record_pending(
			&journal_row(pair, fresh, created("new.txt"), NOW - 1_000),
			&[],
			&[],
		)
		.unwrap();

	let loaded = store.load_pending(NOW, GRACE).unwrap();
	assert_eq!(
		loaded.iter().map(|row| row.uuid).collect::<Vec<_>>(),
		vec![fresh],
		"only the write still inside the window is handed back"
	);
	assert_eq!(pending_count(&store), 1, "the expired row was deleted");
}

#[test]
fn deleting_a_pair_takes_its_journal_rows_with_it() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("a.txt"), NOW),
			&[],
			&[],
		)
		.unwrap();
	let kept = Uuid::new_v4();
	store
		.record_pending(&journal_row(other, kept, created("b.txt"), NOW), &[], &[])
		.unwrap();

	store.delete_pair(pair).unwrap();
	assert_eq!(
		store
			.load_pending(NOW, GRACE)
			.unwrap()
			.iter()
			.map(|row| row.uuid)
			.collect::<Vec<_>>(),
		vec![kept],
		"the removed pair's journal went with it, the other pair's stayed"
	);
}

/// A pair removed while foreign keys were off (an older build, a repaired DB) leaves rows
/// behind that name nothing. Loading must drop them rather than hand the engine a write it
/// could only fold against a baseline that no longer exists.
#[test]
fn a_journal_row_whose_pair_is_gone_is_dropped_on_load() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	store
		.record_pending(
			&journal_row(pair, Uuid::new_v4(), created("a.txt"), NOW),
			&[],
			&[],
		)
		.unwrap();

	store
		.conn
		.execute_batch("PRAGMA foreign_keys = OFF;")
		.unwrap();
	store.delete_pair(pair).unwrap();
	store
		.conn
		.execute_batch("PRAGMA foreign_keys = ON;")
		.unwrap();
	assert_eq!(
		pending_count(&store),
		1,
		"precondition: the row was orphaned"
	);

	assert!(store.load_pending(NOW, GRACE).unwrap().is_empty());
	assert_eq!(pending_count(&store), 0, "the orphan was deleted");
}

/// The listed record carries the flag, so a caller rendering the pairs does not have to ask per
/// pair — and reads it from the row, not from a default the record was built with.
#[test]
fn a_listed_pair_reports_whether_it_is_paused() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (running, _) = store
		.create_pair("/home/u/running", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let (paused, _) = store
		.create_pair("/home/u/paused", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	assert!(
		store.list_pairs().unwrap().iter().all(|pair| !pair.paused),
		"a new pair must list as running"
	);

	store.set_paused(paused, true).unwrap();
	let listed = store.list_pairs().unwrap();
	assert_eq!(
		listed
			.iter()
			.filter(|pair| pair.paused)
			.map(|pair| pair.id)
			.collect::<Vec<_>>(),
		vec![paused],
		"list_pairs did not report the paused pair"
	);
	assert!(store.pair(paused).unwrap().unwrap().paused);
	assert!(!store.pair(running).unwrap().unwrap().paused);

	store.set_paused(paused, false).unwrap();
	assert!(!store.pair(paused).unwrap().unwrap().paused, "the resume");
}

#[test]
fn the_paused_flag_round_trips_and_survives_a_reopen() {
	let path = temp_db_path("paused");
	let remote = Uuid::new_v4();
	let (running, paused) = {
		let store = BaselineStore::open(&path).unwrap();
		let (running, _) = store
			.create_pair("/home/u/running", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (paused, _) = store
			.create_pair("/home/u/paused", remote, SyncMode::TwoWay)
			.unwrap();
		assert!(store.paused_pairs().unwrap().is_empty(), "a new pair runs");

		assert!(store.set_paused(paused, true).unwrap());
		assert_eq!(store.paused_pairs().unwrap(), vec![paused]);
		// Idempotent, and an unknown pair is reported as unknown rather than silently ignored.
		assert!(store.set_paused(paused, true).unwrap());
		assert!(!store.set_paused(paused + 9_999, true).unwrap());

		// Re-adding the same roots must not un-pause the pair behind the caller's back.
		assert_eq!(
			store
				.create_pair("/home/u/paused", remote, SyncMode::LocalToRemote)
				.unwrap()
				.0,
			paused
		);
		assert_eq!(store.paused_pairs().unwrap(), vec![paused]);
		(running, paused)
	};

	let reopened = BaselineStore::open(&path).unwrap();
	assert_eq!(
		reopened.paused_pairs().unwrap(),
		vec![paused],
		"the paused flag did not survive the reopen"
	);
	assert!(reopened.set_paused(paused, false).unwrap());
	assert!(reopened.paused_pairs().unwrap().is_empty());
	// Removing a pair takes its flag with it.
	assert!(reopened.set_paused(running, true).unwrap());
	reopened.delete_pair(running).unwrap();
	assert!(reopened.paused_pairs().unwrap().is_empty());
	drop(reopened);
	std::fs::remove_file(&path).ok();
}

/// A directory deletion's rows go in one transaction with the rows of the deletions it carried:
/// a carried row that cannot be read back leaves every row where it was, the directory's own
/// and those already dropped included, and so does a trash's journal row.
#[test]
fn a_carried_row_that_fails_rolls_the_whole_deletion_back() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let rows = ["docs", "docs/a.txt", "docs/b.txt"].map(|path| file_entry(path, [1; 32], 1));
	for row in &rows {
		store.upsert_entry(pair, row).unwrap();
	}
	let carried_then_failed = |drop: &mut DropRow<'_>| {
		drop("docs/a.txt")?;
		Err(rusqlite::Error::InvalidQuery)
	};
	let paths = |store: &BaselineStore| -> Vec<String> {
		store
			.entries(pair)
			.unwrap()
			.into_iter()
			.map(|row| row.rel_path)
			.collect()
	};

	assert!(
		store
			.apply_changes_carrying(pair, &[BaselineChange::Delete("docs")], carried_then_failed)
			.is_err()
	);
	assert_eq!(paths(&store), ["docs", "docs/a.txt", "docs/b.txt"]);

	let trash = PendingRow {
		pair,
		uuid: Uuid::new_v4(),
		kind: PendingKind::Trashed {
			path: "docs".to_string(),
		},
		recorded_at: NOW,
	};
	assert!(
		store
			.record_pending_carrying(
				&trash,
				&[BaselineChange::Delete("docs")],
				&[],
				carried_then_failed
			)
			.is_err()
	);
	assert_eq!(paths(&store), ["docs", "docs/a.txt", "docs/b.txt"]);
	assert_eq!(pending_count(&store), 0);
}

/// Stage one file listed alike in both filen-sync trees.
fn put_synced(
	put: &mut PutLegacy<'_>,
	rel_path: &str,
	uuid: Uuid,
	size: i64,
	mtime: i64,
) -> rusqlite::Result<()> {
	put(&LegacyItem {
		rel_path,
		raw: rel_path,
		is_file: true,
		size: Some(size),
		side: LegacySide::Local { mtime: Some(mtime) },
	})?;
	put(&LegacyItem {
		rel_path,
		raw: rel_path,
		is_file: true,
		size: Some(size),
		side: LegacySide::Remote { uuid: Some(uuid) },
	})
}

fn legacy_paths(store: &BaselineStore, pair: PairId) -> Vec<String> {
	store
		.conn
		.prepare("SELECT rel_path FROM legacy_synced WHERE pair_id = ?1 ORDER BY rel_path")
		.unwrap()
		.query_map(params![pair], |row| row.get(0))
		.unwrap()
		.collect::<rusqlite::Result<_>>()
		.unwrap()
}

#[test]
fn a_second_filen_sync_import_replaces_the_first_and_leaves_other_pairs_alone() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/a", Uuid::from_u128(1), SyncMode::TwoWay)
		.unwrap();
	let (other, _) = store
		.create_pair("/b", Uuid::from_u128(2), SyncMode::TwoWay)
		.unwrap();
	let imported = store
		.replace_legacy_synced(other, |put| {
			put_synced(put, "kept.txt", Uuid::from_u128(9), 1, 1)
		})
		.unwrap();
	assert_eq!(imported, 1);

	let imported = store
		.replace_legacy_synced(pair, |put| {
			put_synced(put, "old-1.txt", Uuid::from_u128(10), 1, 1)?;
			put_synced(put, "old-2.txt", Uuid::from_u128(11), 1, 1)
		})
		.unwrap();
	assert_eq!(imported, 2);
	let imported = store
		.replace_legacy_synced(pair, |put| {
			put_synced(put, "new.txt", Uuid::from_u128(12), 1, 1)
		})
		.unwrap();
	assert_eq!(imported, 1);

	assert_eq!(legacy_paths(&store, pair), vec!["new.txt".to_owned()]);
	assert_eq!(store.legacy_synced_count(pair).unwrap(), 1);
	assert_eq!(legacy_paths(&store, other), vec!["kept.txt".to_owned()]);
}

#[test]
fn a_failed_filen_sync_import_leaves_the_earlier_one_and_no_staging_tables() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/a", Uuid::from_u128(1), SyncMode::TwoWay)
		.unwrap();
	store
		.replace_legacy_synced(pair, |put| {
			put_synced(put, "kept.txt", Uuid::from_u128(10), 1, 1)
		})
		.unwrap();

	let failed = store.replace_legacy_synced(pair, |put| {
		put_synced(put, "never.txt", Uuid::from_u128(11), 1, 1)?;
		Err(rusqlite::Error::InvalidQuery)
	});
	assert!(matches!(failed, Err(rusqlite::Error::InvalidQuery)));
	assert_eq!(legacy_paths(&store, pair), vec!["kept.txt".to_owned()]);

	let staging: i64 = store
		.conn
		.query_row(
			"SELECT COUNT(*) FROM temp.sqlite_master WHERE name LIKE 'legacy_%'",
			[],
			|row| row.get(0),
		)
		.unwrap();
	assert_eq!(
		staging, 0,
		"the staging tables are dropped however the import ends"
	);
}

#[test]
fn removing_a_pair_removes_its_imported_filen_sync_state() {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/a", Uuid::from_u128(1), SyncMode::TwoWay)
		.unwrap();
	store
		.replace_legacy_synced(pair, |put| {
			put_synced(put, "a.txt", Uuid::from_u128(10), 1, 1)
		})
		.unwrap();
	store.delete_pair(pair).unwrap();
	let left: i64 = store
		.conn
		.query_row("SELECT COUNT(*) FROM legacy_synced", [], |row| row.get(0))
		.unwrap();
	assert_eq!(left, 0);
}
