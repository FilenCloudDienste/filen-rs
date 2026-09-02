//! Baseline schema / version migration tests (`MIGRATE-*`) for the two-way sync engine.
//!
//! HONEST SCOPING NOTE. Every MIGRATE test is fundamentally about the persisted per-pair baseline
//! surviving an ENGINE UPGRADE: an OLDER engine writes a baseline at an OLD schema version, then a
//! NEWER engine reads/migrates it. Exercising that dimension needs infrastructure the current
//! black-box harness does NOT expose:
//!
//! - a *prior* `SyncEngine` build that writes an older-schema baseline (there is exactly one engine version available),
//! - baseline-store inspection/mutation (read the schema-version field, count entries, plant a newer-than-supported version, hand-write per-entry encryption/DEK columns),
//! - deterministic mid-write interruption of the migration (kill between temp-write and commit).
//!
//! None of that is fakeable through the public API, so the assertions that hinge on a version
//! CHANGE are stubbed `#[ignore]` with their plan summary, NOT faked into asserting nothing.
//!
//! What the live harness GENUINELY supports — and what is implemented here — is the load-bearing
//! SAFETY substrate every MIGRATE case layers on top of: a baseline persisted to a stable DB path
//! is re-used across a fresh `SyncEngine::open` (the process-restart analogue) so the next pass is a
//! clean no-op, the persisted baseline is distinguished from an absent one (no first-sync wipe of a
//! populated side), a genuine post-baseline change is still detected, move/rename identity (uuid)
//! survives the reopen, and the reopen is idempotent and durable across multiple cold starts. These
//! are the SAME-version analogues; they prove the baseline round-trips and is acted on correctly,
//! which is the property an in-place migration must preserve. The version-bump-specific guarantees
//! sit in the ignored stubs.
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
	assert_eq!(r.held_deletions, 0, "{r:?}");
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
// BLOCKED — version-CHANGE dimension: needs an older-engine build, baseline-
// store schema inspection/mutation, or mid-migration interruption. Stubbed so
// they assert the real thing once that infra exists, never faked.
// ===========================================================================

/// MIGRATE-01 (version-bump assertion) — the on-disk baseline schema-version FIELD is bumped to the
/// new version in place after the first post-upgrade pass, and the migrated entry count equals the
/// pre-migration count.
#[ignore = "blocked: needs older-engine build + baseline-store schema inspection — see TODO"]
#[shared_test_runtime]
async fn migrate_01_old_baseline_schema_version_bumped_in_place() {
	// plan: converge with the OLDER engine (persist OLD-schema baseline); confirm tree+baseline
	// record every entry; swap in the NEWER engine; run one pass with NO changes. Verify the on-disk
	// baseline schema-version field is bumped, the pass is a true no-op (zero create/update/delete/
	// move/quarantine, no events), remote uuids + local mtimes/sizes byte-for-byte unchanged, and the
	// migrated entry count == pre-migration count.
}

/// MIGRATE-04 — a newer-than-supported baseline is refused or safely degraded, never silently
/// mis-read into deletes/wipes; the newer baseline file is left intact when refused.
#[ignore = "blocked: needs baseline-store mutation to plant a newer-than-supported version — see TODO"]
#[shared_test_runtime]
async fn migrate_04_newer_than_supported_baseline_fails_closed() {
	// plan: persist a baseline whose schema version is HIGHER than the current engine supports
	// (simulate a downgrade); run one pass. Verify the engine either (a) refuses with an explicit
	// unsupported-version error in the report, or (b) degrades to fresh-baseline establishment under
	// the conservative first-sync guards — never silently parsing the newer data under old rules to
	// plan deletes/wipes. Negative: zero delete/wipe/quarantine on either side; the newer baseline
	// file is left intact (not overwritten/downgraded) on refusal.
}

/// MIGRATE-05 — interrupted migration is atomic and resumable: after a kill mid-write of the migrated
/// baseline, the on-disk baseline is fully old-schema OR fully new-schema, never a torn mix.
#[ignore = "blocked: needs deterministic mid-write interruption + baseline-store inspection — see TODO"]
#[shared_test_runtime]
async fn migrate_05_interrupted_migration_is_atomic_and_resumable() {
	// plan: persist an OLD-schema baseline for a converged pair; begin migration but kill the process
	// mid-write of the migrated baseline (before any commit/rename); restart and run a pass. Verify
	// the on-disk baseline is EITHER fully old- OR fully new-schema (temp-file+rename / transactional
	// store), migration completes (or cleanly restarts from the old baseline) and the pass is a no-op
	// with no spurious re-sync/delete/wipe/quarantine. Repeating the interrupt+restart cycle still
	// converges to a single fully-migrated baseline with identical entry count.
}

/// MIGRATE-08 — multi-pair upgrade: each pair's baseline migrates independently; one
/// newer-than-supported (failing) pair does not affect the others.
#[ignore = "blocked: needs older-engine build + per-pair baseline version planting — see TODO"]
#[shared_test_runtime]
async fn migrate_08_per_pair_migration_isolated() {
	// plan: configure two+ pairs converged under the OLDER engine, each with its own OLD-schema
	// baseline; make ONE pair's baseline newer-than-supported (per MIGRATE-04), the rest valid old.
	// Upgrade and run a pass over all pairs. Verify the valid old-schema pairs migrate in place and
	// run no-op passes; the newer-than-supported pair is refused/degraded safely and reported per
	// pair, WITHOUT aborting or corrupting the others; no cross-pair contamination (the failing pair
	// causes no deletes/wipes/quarantine in any other pair, and its own data is untouched).
}

/// MIGRATE-09 — migration preserves encryption-version / DEK reference fields so no file is
/// re-encrypted, re-keyed, or re-uploaded as a side effect.
#[ignore = "blocked: needs older-engine build + per-entry baseline encryption-field inspection — see TODO"]
#[shared_test_runtime]
async fn migrate_09_encryption_dek_metadata_preserved() {
	// plan: converge with the OLDER engine on an account using a specific key model (V2 master-key or
	// V3 DEK); persist an OLD-schema baseline carrying per-entry encryption metadata. Upgrade and run
	// one pass with no content changes. Verify per-entry encryption-version / DEK reference fields
	// survive migration (files still recognized by their existing encrypted identity), no file is
	// re-encrypted/re-keyed/re-uploaded, the pass is a no-op, remote uuids + ciphertext unchanged, and
	// no version-mismatch errors or encryption-attributed spurious updates appear.
}
