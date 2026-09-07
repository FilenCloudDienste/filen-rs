//! Baseline schema / version tests (`MIGRATE-*`) for the two-way sync engine.
//!
//! SCOPING NOTE. The engine has ONE schema version and no migration chain: nothing has shipped a
//! baseline DB, so there is nothing to migrate FROM. A DB stamped at any other version — older or
//! newer — is refused rather than read (`migrate_04_*`), because reading foreign rows under these
//! rules would misplan them into deletes. The step-by-step upgrade assertions this module used to
//! stub out describe machinery that does not exist; they are gone rather than left standing as
//! ignored placeholders, and the first released schema is what would bring them back.
//!
//! What remains is the load-bearing SAFETY substrate: a baseline persisted to a stable DB path is
//! re-used across a fresh `SyncEngine::open` (the process-restart analogue) so the next pass is a
//! clean no-op, the persisted baseline is distinguished from an absent one (no first-sync wipe of a
//! populated side), a genuine post-baseline change is still detected, move/rename identity (uuid)
//! survives the reopen, and the reopen is idempotent and durable across multiple cold starts.
use std::borrow::Cow;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::dir::RemoteDirectory;
use filen_sdk_rs::fs::file::RemoteFile;
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{SyncEngine, SyncMode, SyncReport};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ---------------------------------------------------------------------------
// Local helpers (public-API only; remote ground-truth via the cache's client).
// ---------------------------------------------------------------------------

/// List the (dirs, files) directly under a remote dir via `cache`'s client (ground truth).
async fn list_dir(
	cache: &TestCache,
	dir: &RemoteDirectory,
) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

fn find_file<'a>(files: &'a [RemoteFile], name: &str) -> Option<&'a RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

fn find_dir<'a>(dirs: &'a [RemoteDirectory], name: &str) -> Option<&'a RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
}

/// Upload a file with exact bytes directly to a remote root, returning it.
async fn upload_to(cache: &TestCache, parent: Uuid, name: &str, data: &[u8]) -> RemoteFile {
	let builder = cache.client.make_file_builder(name, parent).unwrap();
	cache.client.upload_file(builder, data).await.unwrap()
}

/// Wait until `cache` (the engine's remote view) observes `uuid`.
async fn wait_cache_has(cache: &TestCache, uuid: Uuid) {
	assert!(
		poll_for_item(cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// A read/write connection onto a baseline DB the engine persisted — the only way, black-box, to
/// plant a schema version the current build does not understand (MIGRATE-04).
fn open_read_write_db(path: &std::path::Path) -> rusqlite::Connection {
	rusqlite::Connection::open(path).unwrap()
}

/// Assert a `SyncReport` is a perfectly clean no-op (every counter zero, no conflicts/errors).
fn assert_noop(r: &SyncReport) {
	assert_eq!(r.uploaded, 0, "no-op expected, got upload: {r:?}");
	assert_eq!(r.downloaded, 0, "no-op expected, got download: {r:?}");
	assert_eq!(r.local_dirs_created, 0, "{r:?}");
	assert_eq!(r.remote_dirs_created, 0, "{r:?}");
	assert_eq!(r.locally_deleted, 0, "{r:?}");
	assert_eq!(r.remotely_trashed, 0, "{r:?}");
	assert_eq!(r.moved_remote, 0, "{r:?}");
	assert_eq!(r.moved_local, 0, "{r:?}");
	assert_eq!(r.conflicts.len(), 0, "{r:?}");
	assert_eq!(r.held_deletions(), 0, "{r:?}");
	assert!(r.errors.is_empty(), "{r:?}");
}

// ===========================================================================
// IMPLEMENTED — same-version baseline persistence/reopen substrate.
// These are the live analogues of the upgrade path: a persisted baseline is
// re-loaded by a fresh engine `open` and acted on correctly. The schema
// VERSION-CHANGE dimension itself is covered by the `#[ignore]` stubs.
// ===========================================================================

/// MIGRATE-01 (same-version analogue) — a persisted baseline re-loaded by a fresh engine `open` with
/// nothing changed yields a TRUE no-op: zero create/update/delete/move/quarantine, no transfer, no
/// destination wipe, and the remote object set + sizes are byte-for-byte unchanged. This is exactly
/// the property an in-place migration must preserve (read an older baseline without manufacturing
/// phantom diffs); only the schema-version BUMP is out of reach for the live harness (see the
/// `migrate_01_*` ignored stub).
#[shared_test_runtime]
async fn migrate_01_reopened_baseline_first_pass_is_noop() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate01");
	let db_path = temp_cache_path();

	// Converge a populated tree under engine #1 (persists a baseline at the current schema).
	write_file(&local, "a.txt", b"alpha");
	write_file(&local, "sub/b.txt", b"bravo bytes");
	write_file(&local, "sub/deep/c.txt", b"charlie");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	// Snapshot remote uuids + sizes (the byte-for-byte unchanged invariant).
	let (dirs0, files0) = list_dir(&cache, &resources.dir).await;
	let pre_count = files0.len();
	let sub0 = find_dir(&dirs0, "sub").unwrap();
	let a_uuid = find_file(&files0, "a.txt").unwrap().uuid();
	drop(engine1);

	// "Upgrade"/restart: a brand-new engine on the SAME baseline DB, nothing changed.
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert_noop(&r2);

	// Remote object set + uuids + sizes unchanged; no quarantine bin appeared locally.
	let (dirs1, files1) = list_dir(&cache, &resources.dir).await;
	assert_eq!(files1.len(), pre_count, "remote file count changed: {r2:?}");
	assert_eq!(
		find_file(&files1, "a.txt").unwrap().uuid(),
		a_uuid,
		"a.txt uuid changed across reopen"
	);
	let sub1 = find_dir(&dirs1, "sub").expect("sub dir lost");
	assert_eq!(sub0.uuid(), sub1.uuid(), "sub dir uuid changed");
	assert!(
		!local.join(".filen-sync-trash").exists(),
		"a no-op reopen pass must not create a quarantine bin"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// MIGRATE-02 (same-version analogue) — uuid identity in the persisted baseline survives a fresh
/// engine `open`, so a local move + rename done BEFORE the post-reopen pass is detected as a
/// MOVE/RENAME (uuid preserved, zero re-upload), never as delete+create. A lossy migration would
/// drop the uuid column and degrade these to delete+create; this proves the column round-trips.
#[shared_test_runtime]
async fn migrate_02_move_and_rename_detected_after_reopen() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate02");
	let db_path = temp_cache_path();

	// Converge: one file destined to move, one destined to rename.
	write_file(&local, "to_move.txt", b"stable move payload");
	write_file(&local, "to_rename.txt", b"stable rename payload");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	let (_d0, files0) = list_dir(&cache, &resources.dir).await;
	let move_uuid = find_file(&files0, "to_move.txt").unwrap().uuid();
	let rename_uuid = find_file(&files0, "to_rename.txt").unwrap().uuid();
	drop(engine1);

	// Apply a move (into a new subfolder) and a rename BEFORE the post-reopen pass.
	std::fs::create_dir_all(local.join("moved")).unwrap();
	std::fs::rename(local.join("to_move.txt"), local.join("moved/to_move.txt")).unwrap();
	std::fs::rename(local.join("to_rename.txt"), local.join("renamed.txt")).unwrap();

	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.moved_remote, 2,
		"move+rename should be 2 remote moves: {r2:?}"
	);
	assert_eq!(
		r2.uploaded, 0,
		"identity move/rename must NOT re-upload: {r2:?}"
	);
	assert_eq!(r2.remotely_trashed, 0, "no delete+create: {r2:?}");

	// uuids preserved; no duplicate remote objects.
	let (dirs1, files1) = list_dir(&cache, &resources.dir).await;
	assert!(
		find_file(&files1, "to_move.txt").is_none(),
		"old move name lingers"
	);
	let renamed = find_file(&files1, "renamed.txt").expect("renamed file missing");
	assert_eq!(renamed.uuid(), rename_uuid, "rename did not preserve uuid");
	let moved_dir = find_dir(&dirs1, "moved").expect("moved/ dir missing");
	let (_md, moved_files) = list_dir(&cache, moved_dir).await;
	let moved = find_file(&moved_files, "to_move.txt").expect("moved file missing");
	assert_eq!(moved.uuid(), move_uuid, "move did not preserve uuid");

	std::fs::remove_dir_all(&local).ok();
}

/// MIGRATE-03 (same-version analogue) — a PRESENT persisted baseline is treated as an incremental
/// pass, NOT a first sync: the post-reopen pass over an unchanged populated pair plans zero actions
/// and never wipes/empties the destination. Contrast control: a fresh engine with NO baseline file
/// (separate db path) over the SAME populated remote does engage first-sync semantics — i.e. it does
/// NOT silently wipe the populated remote either; the distinction is real, not coincidental.
#[shared_test_runtime]
async fn migrate_03_present_baseline_is_incremental_not_first_sync() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate03");
	let db_path = temp_cache_path();

	// Populate BOTH sides and converge them under engine #1 (TwoWay).
	write_file(&local, "shared1.txt", b"one");
	write_file(&local, "shared2.txt", b"two");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert_eq!(r1.uploaded, 2, "{r1:?}");
	let (_d0, files0) = list_dir(&cache, &resources.dir).await;
	let pre_count = files0.len();
	drop(engine1);

	// Reopen on the SAME baseline DB: present baseline => incremental no-op, no wipe.
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert_noop(&r2);
	let (_d1, files1) = list_dir(&cache, &resources.dir).await;
	assert_eq!(
		files1.len(),
		pre_count,
		"incremental pass changed remote: {r2:?}"
	);
	assert_eq!(
		walk_tree(&local).len(),
		2,
		"local was wiped/emptied: {r2:?}"
	);

	// Contrast control: a NEW engine with NO baseline (fresh db) over the same populated remote
	// must STILL not wipe the populated side — first-sync semantics reconcile additively here
	// (local already mirrors remote, so it converges with no destructive action).
	let local_ctrl = fresh_local_dir("migrate03ctrl");
	write_file(&local_ctrl, "shared1.txt", b"one");
	write_file(&local_ctrl, "shared2.txt", b"two");
	let engine3 = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair3 = engine3
		.add_pair(local_ctrl.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r3 = engine3.sync_once(pair3).await.unwrap();
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.remotely_trashed, 0,
		"first-sync must not trash remote: {r3:?}"
	);
	assert_eq!(
		r3.locally_deleted, 0,
		"first-sync must not delete local: {r3:?}"
	);
	let (_d2, files2) = list_dir(&cache, &resources.dir).await;
	assert_eq!(
		files2.len(),
		pre_count,
		"control first-sync changed remote count: {r3:?}"
	);

	std::fs::remove_dir_all(&local).ok();
	std::fs::remove_dir_all(&local_ctrl).ok();
}

/// MIGRATE-06 (same-version analogue) — re-running the reopened engine is idempotent: a second AND
/// third pass after the reopen no-op stay clean no-ops with no transfer activity. (The schema-bump
/// "happens exactly once" assertion needs baseline-store inspection — see the ignored stub.)
#[shared_test_runtime]
async fn migrate_06_reopen_passes_are_idempotent() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate06");
	let db_path = temp_cache_path();

	write_file(&local, "x.txt", b"ex");
	write_file(&local, "y.txt", b"why");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	assert_eq!(engine1.sync_once(pair1).await.unwrap().uploaded, 2);
	drop(engine1);

	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	assert_noop(&engine2.sync_once(pair2).await.unwrap());
	assert_noop(&engine2.sync_once(pair2).await.unwrap());
	assert_noop(&engine2.sync_once(pair2).await.unwrap());

	std::fs::remove_dir_all(&local).ok();
}

/// MIGRATE-07 (same-version analogue) — a persisted baseline does not blunt real-change detection:
/// after converging under engine #1, one genuine local edit and one genuine remote addition made
/// before the reopen are BOTH detected and propagated (TwoWay), and exactly those two changes are
/// acted on — no unrelated entry re-transferred, deleted, or quarantined.
#[shared_test_runtime]
async fn migrate_07_real_change_after_reopen_is_applied() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate07");
	let db_path = temp_cache_path();

	write_file(&local, "edit_me.txt", b"v1");
	write_file(&local, "stable.txt", b"unchanging");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	assert_eq!(engine1.sync_once(pair1).await.unwrap().uploaded, 2);
	drop(engine1);

	// One genuine local edit + one genuine remote add, both before the post-reopen pass.
	write_file(&local, "edit_me.txt", b"v2 is meaningfully longer");
	let added = upload_to(&cache, remote, "remote_add.txt", b"from the remote side").await;
	wait_cache_has(&cache, added.uuid()).await;

	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "exactly the one local edit pushes: {r2:?}");
	assert_eq!(r2.downloaded, 1, "exactly the one remote add pulls: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "no unrelated trash: {r2:?}");
	assert_eq!(r2.locally_deleted, 0, "no unrelated delete: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "no spurious conflict: {r2:?}");

	// Converged end-state: local has the remote add + the edited content; remote has the new file.
	assert!(read_eq(&local, "remote_add.txt", b"from the remote side"));
	assert!(read_eq(&local, "edit_me.txt", b"v2 is meaningfully longer"));
	let (_d, files) = list_dir(&cache, &resources.dir).await;
	assert_eq!(
		find_file(&files, "edit_me.txt").unwrap().size,
		b"v2 is meaningfully longer".len() as u64
	);
	assert!(find_file(&files, "remote_add.txt").is_some());
	assert!(
		find_file(&files, "stable.txt").is_some(),
		"stable file lost: {r2:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// MIGRATE-10 (same-version analogue) — the persisted baseline is DURABLE across multiple cold
/// starts: open #1 converges, open #2 is a no-op, open #3 (another fresh process analogue) is still
/// a no-op, with identical entry counts before and after — no re-sync, transfer, delete, or
/// quarantine re-triggered by the restart path.
#[shared_test_runtime]
async fn migrate_10_baseline_durable_across_repeated_cold_starts() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate10");
	let db_path = temp_cache_path();

	for i in 0..4 {
		write_file(&local, &format!("d{i}.txt"), format!("data-{i}").as_bytes());
	}
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	assert_eq!(engine1.sync_once(pair1).await.unwrap().uploaded, 4);
	let (_d0, files0) = list_dir(&cache, &resources.dir).await;
	let count = files0.len();
	drop(engine1);

	// Two successive cold starts, each a fresh `open` on the same baseline DB.
	for tag in ["coldstart-2", "coldstart-3"] {
		let engine = SyncEngine::open(cache.client.clone(), db_path.clone())
			.await
			.unwrap_or_else(|e| panic!("{tag} open failed: {e:?}"));
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
			.await
			.unwrap();
		assert_noop(&engine.sync_once(pair).await.unwrap());
		let (_d, files) = list_dir(&cache, &resources.dir).await;
		assert_eq!(files.len(), count, "{tag}: remote entry count drifted");
		drop(engine);
	}

	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// Version handling: one schema, and anything else refused.
// ===========================================================================

/// MIGRATE-04 — a baseline stamped at any version but this build's is refused, never silently
/// mis-read into deletes/wipes, and the file is left exactly as it was. Both directions: a NEWER
/// stamp (what a future engine would write) and an OLDER one (there is no migration chain, so an
/// older stamp is just as foreign). Restoring the real stamp brings the pair straight back.
#[shared_test_runtime]
async fn migrate_04_a_foreign_schema_version_fails_closed() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("migrate04");
	let db_path = temp_cache_path();

	// Converge a populated pair so BOTH sides hold real data the refusal must not touch.
	write_file(&local, "keep1.txt", b"one");
	write_file(&local, "keep2.txt", b"two");
	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	assert_eq!(engine1.sync_once(pair1).await.unwrap().uploaded, 2);
	let (_d0, files0) = list_dir(&cache, &resources.dir).await;
	let pre_count = files0.len();
	let pre_tree = walk_tree(&local);
	drop(engine1);

	let current: i64 = open_read_write_db(&db_path)
		.query_row("PRAGMA user_version", [], |row| row.get(0))
		.unwrap();
	assert!(current > 0, "the engine must stamp the baseline it writes");

	for planted in [current + 1, current - 1] {
		let stamped = {
			let conn = open_read_write_db(&db_path);
			conn.execute_batch(&format!("PRAGMA user_version = {planted};"))
				.unwrap();
			conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
				.unwrap()
		};
		assert_eq!(stamped, planted, "precondition: the version was planted");

		// The engine must fail CLOSED — refuse to open at all rather than read foreign rows under
		// these rules and plan deletes from them.
		let message = match SyncEngine::open(cache.client.clone(), db_path.clone()).await {
			Ok(_) => panic!("schema version {planted} must be refused"),
			Err(error) => format!("{error}"),
		};
		assert!(
			message.contains("schema version"),
			"the refusal must name the version problem: {message}"
		);

		// Nothing was destroyed on either side, and the baseline file was NOT re-stamped.
		let (_d1, files1) = list_dir(&cache, &resources.dir).await;
		assert_eq!(files1.len(), pre_count, "remote changed after the refusal");
		assert!(
			find_file(&files1, "keep1.txt").is_some(),
			"remote file lost"
		);
		assert!(
			find_file(&files1, "keep2.txt").is_some(),
			"remote file lost"
		);
		assert_eq!(walk_tree(&local), pre_tree, "local tree changed");
		assert!(
			!local.join(".filen-sync-trash").exists(),
			"a refused open must not quarantine anything"
		);
		let still: i64 = open_read_write_db(&db_path)
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap();
		assert_eq!(
			still, planted,
			"the refused baseline was rewritten in place"
		);
	}

	// The refusal is about the stamp alone: restore it and the very same DB opens and no-ops.
	open_read_write_db(&db_path)
		.execute_batch(&format!("PRAGMA user_version = {current};"))
		.unwrap();
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.expect("the untouched baseline must open again once its stamp is restored");
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	assert_noop(&engine2.sync_once(pair2).await.unwrap());

	std::fs::remove_dir_all(&local).ok();
}
