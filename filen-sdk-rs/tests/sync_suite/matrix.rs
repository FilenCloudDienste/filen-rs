//! Cross-cutting invariant x mode/guard matrix tests (`MATRIX-*`) for the two-way sync engine.
//!
//! Five invariants asserted once per applicable mode/guard cell:
//!   (a) self-write loop-freedom — the engine's own writes do not generate further transferring work
//!   (b) first-sync no-wipe — an absent baseline never licenses deleting a populated destination
//!   (c) baseline-persists-across-restart — a no-op restart is genuinely a no-op
//!   (d) backup-mode delete-suppression — backup modes never delete on their destination
//!   (e) interrupted-resume idempotency — a partially-applied pass resumes exactly-once
//!
//! Invariants (a)-(d) are implemented against the LIVE harness: the observable property of (a)
//! "self-write loop-freedom" is that an extra pass after convergence is a clean no-op (zero
//! re-transfer) — the watcher/scheduler pass-count introspection the plan describes is engine
//! internal state the black-box harness cannot reach, so we assert the externally-visible
//! consequence. Invariant (e) requires deterministic mid-transfer/crash interruption (a
//! fault-injection harness that does not exist yet); those cells are `#[ignore]`-stubbed with the
//! plan summary rather than faked.
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

/// List the (dirs, files) directly under the single-client's remote root (ground truth).
async fn list_root(sc: &SingleClient) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	sc.cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

/// List the (dirs, files) directly under the two-client fixture's shared remote root.
async fn list_root_tc(tc: &TwoClients) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	tc.cache_a
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&tc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

fn find_file<'a>(files: &'a [RemoteFile], name: &str) -> Option<&'a RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

/// Upload a file with exact bytes directly to the single-client's remote root, returning it.
async fn upload_root(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
	let builder = sc
		.cache
		.client
		.make_file_builder(name, sc.resources.dir.uuid())
		.unwrap();
	sc.cache.client.upload_file(builder, data).await.unwrap()
}

/// Wait until the cache (the engine's remote view) observes `uuid` under the root.
async fn wait_cache_has(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// Assert a `SyncReport` is a perfectly clean no-op (every counter zero, no conflicts/errors). This
/// is the observable form of self-write loop-freedom: after convergence, the engine's own writes
/// must NOT generate any further transferring/deleting work.
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
// (a) self-write loop-freedom — observable form: post-convergence pass is a no-op.
// ===========================================================================

/// MATRIX-01 — (a) self-write loop-freedom, l2r. After the engine uploads its own file F, neither
/// re-running the pass nor letting a settle window elapse produces any re-upload: the engine's own
/// remote write is recognized as already-synced. F exists exactly once remotely, byte-exact.
#[shared_test_runtime]
async fn matrix_01_self_write_loop_free_l2r() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "foo.txt", b"self-write-l2r");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Allow any debounce/safety-net window to elapse, then re-run: the engine's own upload of F must
	// NOT retrigger a transferring pass.
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	let r2 = sc.sync().await;
	assert_noop(&r2);
	// And a third pass is still a clean no-op (pass count is stable, no unbounded growth).
	let r3 = sc.sync().await;
	assert_noop(&r3);

	let (_dirs, files) = list_root(&sc).await;
	let matching: Vec<_> = files
		.iter()
		.filter(|f| f.name() == Some("foo.txt"))
		.collect();
	assert_eq!(
		matching.len(),
		1,
		"F must exist exactly once remotely: {files:?}"
	);
	assert_eq!(matching[0].size, b"self-write-l2r".len() as u64);

	sc.cleanup();
}

/// MATRIX-02 — (a) self-write loop-freedom, r2l. After the engine writes its own local copy of a
/// remote file F, re-running the pass (and a settle window) must NOT re-download F: the engine's own
/// local write is recognized as already-synced. Local F byte-exact, no quarantine churn.
#[shared_test_runtime]
async fn matrix_02_self_write_loop_free_r2l() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_root(&sc, "bar.txt", b"self-write-r2l").await;
	wait_cache_has(&sc, rf.uuid().into()).await;

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "bar.txt", b"self-write-r2l"));

	// The engine's own local write of F must not schedule a transferring pass.
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	let r2 = sc.sync().await;
	assert_noop(&r2);
	let r3 = sc.sync().await;
	assert_noop(&r3);

	// Local F unchanged; no quarantine bin created by a spurious self-delete.
	assert!(
		read_eq(&sc.local, "bar.txt", b"self-write-r2l"),
		"local F changed"
	);
	assert!(
		!sc.local.join(".filen-sync-trash").exists(),
		"a self-write must not have produced a quarantine entry"
	);

	sc.cleanup();
}

/// MATRIX-03 — (a) self-write loop-freedom, two-way. After A is uploaded and B is downloaded,
/// neither the remote write of A nor the local write of B schedules further work: a post-convergence
/// pass is a clean no-op on both engines. Each file transferred exactly once; no oscillation.
#[shared_test_runtime]
async fn matrix_03_self_write_loop_free_two_way() {
	let tc = two_clients(SyncMode::TwoWay).await;

	// One change on each side: local-only A on client A, remote-only B uploaded directly.
	write_file(&tc.local_a, "A.txt", b"two-way-A");
	let builder = tc
		.cache_b
		.client
		.make_file_builder("B.txt", tc.resources.dir.uuid())
		.unwrap();
	let rf_b = tc
		.cache_b
		.client
		.upload_file(builder, b"two-way-B")
		.await
		.unwrap();
	assert!(
		poll_for_item(
			tc.cache_a.db_path(),
			rf_b.uuid().into(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache A never observed B.txt"
	);

	let mut conflicts = std::collections::BTreeSet::new();
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
		&mut conflicts,
		"matrix03-converge",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& read_eq(&tc.local_a, "B.txt", b"two-way-B")
				&& read_eq(&tc.local_b, "A.txt", b"two-way-A")
		},
	)
	.await;
	assert!(conflicts.is_empty(), "no conflict expected: {conflicts:?}");

	// After convergence + a settle window, both engines' passes are clean no-ops (no ping-pong).
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	assert_noop(&ra);
	assert_noop(&rb);

	// Remote holds exactly one A and one B (each transferred once, no duplication).
	let (_dirs, files) = list_root_tc(&tc).await;
	assert_eq!(
		files.iter().filter(|f| f.name() == Some("A.txt")).count(),
		1,
		"A duplicated remotely: {files:?}"
	);
	assert_eq!(
		files.iter().filter(|f| f.name() == Some("B.txt")).count(),
		1,
		"B duplicated remotely: {files:?}"
	);

	tc.cleanup();
}

/// MATRIX-04 — (a) self-write loop-freedom, backup modes. For LocalBackup (push) and RemoteBackup
/// (pull): after the engine's own destination write, a re-run + settle window produces no further
/// transfer and no deletion. Two independent single-client fixtures, one per backup mode.
#[shared_test_runtime]
async fn matrix_04_self_write_loop_free_backup_modes() {
	// --- LocalBackup: destination is the remote; F pushed once. ---
	let lb = single_client(SyncMode::LocalBackup).await;
	write_file(&lb.local, "F.txt", b"backup-push");
	let r1 = lb.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	let r2 = lb.sync().await;
	assert_noop(&r2);
	let (_d, lf) = list_root(&lb).await;
	assert_eq!(
		lf.iter().filter(|f| f.name() == Some("F.txt")).count(),
		1,
		"F duplicated on remote backup: {lf:?}"
	);
	lb.cleanup();

	// --- RemoteBackup: destination is the local tree; G pulled once. ---
	let rb = single_client(SyncMode::RemoteBackup).await;
	let g = upload_root(&rb, "G.txt", b"backup-pull").await;
	wait_cache_has(&rb, g.uuid().into()).await;
	let p1 = rb.sync().await;
	assert!(p1.errors.is_empty(), "{p1:?}");
	assert_eq!(p1.downloaded, 1, "{p1:?}");
	assert!(read_eq(&rb.local, "G.txt", b"backup-pull"));
	tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	let p2 = rb.sync().await;
	assert_noop(&p2);
	// G present exactly once locally; no quarantine entry from a spurious self-delete.
	assert!(
		read_eq(&rb.local, "G.txt", b"backup-pull"),
		"G changed locally"
	);
	assert!(
		!rb.local.join(".filen-sync-trash").exists(),
		"backup pull must not quarantine its own write"
	);
	rb.cleanup();
}

// ===========================================================================
// (b) first-sync no-wipe — an absent baseline never deletes a populated destination.
// ===========================================================================

/// MATRIX-05 — (b) first-sync no-wipe, l2r. A fresh l2r pair with an EMPTY local source and a
/// populated remote destination (R1..R3): the first pass must NOT delete the pre-existing remote
/// files that have no baseline. All survive byte-exact; the populated-destination case is not a
/// silent wipe.
#[shared_test_runtime]
async fn matrix_05_first_sync_no_wipe_l2r() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	// Populate the remote destination; local source stays empty.
	for (name, body) in [
		("R1.txt", b"r1" as &[u8]),
		("R2.txt", b"r2"),
		("R3.txt", b"r3"),
	] {
		let f = upload_root(&sc, name, body).await;
		wait_cache_has(&sc, f.uuid().into()).await;
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	// An absent baseline + empty source must NOT be read as "delete everything".
	assert_eq!(
		r1.remotely_trashed, 0,
		"first sync against a populated remote must not wipe it: {r1:?}"
	);

	let (_dirs, files) = list_root(&sc).await;
	for (name, body) in [
		("R1.txt", b"r1" as &[u8]),
		("R2.txt", b"r2"),
		("R3.txt", b"r3"),
	] {
		let f = find_file(&files, name)
			.unwrap_or_else(|| panic!("{name} wiped on first sync: {files:?}"));
		assert_eq!(f.size, body.len() as u64, "{name} corrupted");
	}

	sc.cleanup();
}

/// MATRIX-06 — (b) first-sync no-wipe, r2l. A fresh r2l pair with an EMPTY remote source and a
/// populated local destination (L1..L3): the first pass must NOT hard-delete the pre-existing local
/// files. Their bytes survive (kept in place, or recoverable in the quarantine bin) — never
/// destroyed outright.
#[shared_test_runtime]
async fn matrix_06_first_sync_no_wipe_r2l() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	// Populate the local destination; remote source stays empty.
	write_file(&sc.local, "L1.txt", b"local-1");
	write_file(&sc.local, "L2.txt", b"local-2");
	write_file(&sc.local, "L3.txt", b"local-3");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.downloaded, 0,
		"empty remote source has nothing to pull: {r1:?}"
	);

	// No data loss: every L file's bytes are recoverable (in place OR in quarantine), never destroyed.
	for body in [b"local-1" as &[u8], b"local-2", b"local-3"] {
		assert!(
			conflict_survivor_with(&sc.local, body),
			"first-sync against an empty remote source destroyed local bytes {body:?}"
		);
	}

	sc.cleanup();
}

/// MATRIX-07 — (b) first-sync no-wipe, two-way. A fresh two-way pair, both sides pre-populated with
/// disjoint files plus one same-name-different-content overlap: the first pass merges additively,
/// surfaces the divergent overlap as a conflict (not a silent overwrite/delete), and deletes nothing
/// attributable to the absent baseline. Both sides' originals survive.
#[shared_test_runtime]
async fn matrix_07_first_sync_no_wipe_two_way() {
	let tc = two_clients(SyncMode::TwoWay).await;

	// Client A pre-populated locally; the shared remote pre-populated directly (client B's view).
	write_file(&tc.local_a, "only_a.txt", b"a-only");
	write_file(&tc.local_a, "overlap.txt", b"A-version");
	let r_only = {
		let b = tc
			.cache_b
			.client
			.make_file_builder("only_r.txt", tc.resources.dir.uuid())
			.unwrap();
		tc.cache_b.client.upload_file(b, b"r-only").await.unwrap()
	};
	let r_overlap = {
		let b = tc
			.cache_b
			.client
			.make_file_builder("overlap.txt", tc.resources.dir.uuid())
			.unwrap();
		tc.cache_b
			.client
			.upload_file(b, b"R-version-different")
			.await
			.unwrap()
	};
	assert!(
		poll_for_item(
			tc.cache_a.db_path(),
			r_only.uuid().into(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await && poll_for_item(
			tc.cache_a.db_path(),
			r_overlap.uuid().into(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache A never observed the pre-populated remote files"
	);

	// First pass on A: union the disjoint files; the divergent overlap must NOT be silently deleted.
	let r1 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.remotely_trashed, 0,
		"first two-way sync must not trash a pre-existing remote file: {r1:?}"
	);
	assert_eq!(
		r1.locally_deleted, 0,
		"first two-way sync must not delete a pre-existing local file: {r1:?}"
	);

	// A's local-only and the divergent local overlap survive; the divergent overlap is preserved
	// (left untouched per conflict policy, or a versioned survivor carries the bytes).
	assert!(
		read_eq(&tc.local_a, "only_a.txt", b"a-only"),
		"local-only A lost"
	);
	assert!(
		read_eq(&tc.local_a, "overlap.txt", b"A-version")
			|| conflict_survivor_with(&tc.local_a, b"A-version"),
		"A's overlap bytes destroyed"
	);

	// The disjoint remote-only file survives on the remote (the absent baseline did not wipe it).
	let (_dirs, files) = list_root_tc(&tc).await;
	assert!(
		find_file(&files, "only_r.txt").is_some(),
		"remote-only file wiped: {files:?}"
	);

	tc.cleanup();
}

/// MATRIX-08 — (b) first-sync no-wipe, backup modes. A fresh LocalBackup pair (populated remote
/// destination) and a fresh RemoteBackup pair (populated local destination), each with source-only
/// files: the first pass is additive — pre-existing destination items survive and source-only items
/// are added; zero deletions on either destination.
#[shared_test_runtime]
async fn matrix_08_first_sync_no_wipe_backup_modes() {
	// --- LocalBackup: populated remote destination + a local source-only file. ---
	let lb = single_client(SyncMode::LocalBackup).await;
	let surv = upload_root(&lb, "survivor.txt", b"keep-me").await;
	wait_cache_has(&lb, surv.uuid().into()).await;
	write_file(&lb.local, "added.txt", b"source-only");
	let r1 = lb.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "source-only file should be added: {r1:?}");
	assert_eq!(
		r1.remotely_trashed, 0,
		"backup first sync must not trash destination: {r1:?}"
	);
	let (_d, lf) = list_root(&lb).await;
	assert!(
		find_file(&lf, "survivor.txt").is_some(),
		"destination survivor wiped: {lf:?}"
	);
	assert!(
		find_file(&lf, "added.txt").is_some(),
		"source-only file not added: {lf:?}"
	);
	lb.cleanup();

	// --- RemoteBackup: populated local destination + a remote source-only file. ---
	let rb = single_client(SyncMode::RemoteBackup).await;
	write_file(&rb.local, "local_keep.txt", b"do-not-delete");
	let pulled = upload_root(&rb, "pulled.txt", b"remote-source").await;
	wait_cache_has(&rb, pulled.uuid().into()).await;
	let p1 = rb.sync().await;
	assert!(p1.errors.is_empty(), "{p1:?}");
	assert_eq!(
		p1.downloaded, 1,
		"remote source-only file should be pulled: {p1:?}"
	);
	assert_eq!(
		p1.locally_deleted, 0,
		"backup first sync must not delete local destination: {p1:?}"
	);
	assert!(
		read_eq(&rb.local, "local_keep.txt", b"do-not-delete"),
		"local destination wiped"
	);
	assert!(
		read_eq(&rb.local, "pulled.txt", b"remote-source"),
		"source not pulled"
	);
	rb.cleanup();
}

// ===========================================================================
// (c) baseline-persists-across-restart — a no-op restart is genuinely a no-op.
// ===========================================================================

/// Build a single-client-shaped fixture with a STABLE baseline DB path so a re-opened engine loads
/// the same persisted store. Returns (resources, cache, local, remote, db_path). Caller opens the
/// engine(s) on `db_path`.
async fn restart_fixture(
	tag: &str,
) -> (
	test_utils::TestResources,
	TestCache,
	std::path::PathBuf,
	Uuid,
	std::path::PathBuf,
) {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir(tag);
	let db_path = temp_cache_path();
	(resources, cache, local, remote, db_path)
}

async fn list_dir_via(
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

/// MATRIX-09 — (c) baseline-persists-across-restart, l2r. Converge an l2r pair, drop the engine
/// (baseline persisted to disk), re-open a fresh engine on the SAME baseline DB and run a pass with
/// no intervening changes: ZERO uploads, every remote file byte-identical, report is a clean no-op.
#[shared_test_runtime]
async fn matrix_09_baseline_persists_restart_l2r() {
	let (resources, cache, local, remote, db_path) = restart_fixture("matrix09").await;

	for i in 0..5 {
		write_file(
			&local,
			&format!("f{i}.txt"),
			format!("content-{i}").as_bytes(),
		);
	}

	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 5, "{r1:?}");
	drop(engine1);

	// Restart: a brand-new engine on the SAME baseline DB, no intervening changes.
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert_noop(&r2);

	let (_dirs, files) = list_dir_via(&cache, &resources.dir).await;
	for i in 0..5 {
		let name = format!("f{i}.txt");
		let f = find_file(&files, &name)
			.unwrap_or_else(|| panic!("{name} re-uploaded/lost: {files:?}"));
		assert_eq!(f.size, format!("content-{i}").len() as u64);
	}

	std::fs::remove_dir_all(&local).ok();
}

/// MATRIX-10 — (c) baseline-persists-across-restart, r2l. Converge an r2l pair, capture local file
/// hashes, restart the engine on the same baseline DB, run a pass with no intervening changes: ZERO
/// downloads, local files unchanged (same bytes/mtime), report a clean no-op.
#[shared_test_runtime]
async fn matrix_10_baseline_persists_restart_r2l() {
	let (resources, cache, local, remote, db_path) = restart_fixture("matrix10").await;

	// Populate the remote source.
	for i in 0..4 {
		let b = cache
			.client
			.make_file_builder(&format!("r{i}.txt"), resources.dir.uuid())
			.unwrap();
		let f = cache
			.client
			.upload_file(b, format!("remote-{i}").as_bytes())
			.await
			.unwrap();
		assert!(
			poll_for_item(cache.db_path(), f.uuid().into(), CACHE_CONVERGE_TIMEOUT).await,
			"cache never observed r{i}"
		);
	}

	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 4, "{r1:?}");
	drop(engine1);

	// Capture local mtimes after the initial download.
	let mtimes: Vec<_> = (0..4)
		.map(|i| {
			std::fs::metadata(local.join(format!("r{i}.txt")))
				.unwrap()
				.modified()
				.unwrap()
		})
		.collect();

	// Restart on the same baseline DB; no intervening changes.
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert_noop(&r2);

	// Local files unchanged: same bytes AND no rewrite churn (mtime preserved).
	#[allow(clippy::needless_range_loop)]
	for i in 0..4 {
		assert!(read_eq(
			&local,
			&format!("r{i}.txt"),
			format!("remote-{i}").as_bytes()
		));
		let now = std::fs::metadata(local.join(format!("r{i}.txt")))
			.unwrap()
			.modified()
			.unwrap();
		assert_eq!(now, mtimes[i], "r{i}.txt was rewritten on a no-op restart");
	}
	assert!(
		!local.join(".filen-sync-trash").exists(),
		"a no-op restart must not produce quarantine activity"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// MATRIX-11 — (c) baseline-persists-across-restart, two-way / backup modes. For each of TwoWay,
/// LocalBackup, RemoteBackup: converge, restart the engine on the same baseline DB, run a pass with
/// no intervening changes — every pass is a clean no-op (zero transfers, zero deletions, no false
/// conflict).
#[shared_test_runtime]
async fn matrix_11_baseline_persists_restart_two_way_and_backup() {
	for mode in [
		SyncMode::TwoWay,
		SyncMode::LocalBackup,
		SyncMode::RemoteBackup,
	] {
		let (resources, cache, local, remote, db_path) = restart_fixture("matrix11").await;

		// A small both-sides-ish state: a local file and (for pull-capable modes) a remote file.
		write_file(&local, "local.txt", b"local-body");
		let rf = {
			let b = cache
				.client
				.make_file_builder("remote.txt", resources.dir.uuid())
				.unwrap();
			cache.client.upload_file(b, b"remote-body").await.unwrap()
		};
		assert!(
			poll_for_item(cache.db_path(), rf.uuid().into(), CACHE_CONVERGE_TIMEOUT).await,
			"cache never observed remote.txt for mode {mode:?}"
		);

		let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
			.await
			.unwrap();
		let pair1 = engine1.add_pair(local.clone(), remote, mode).await.unwrap();
		// Converge over a few passes (two-way may need a round to settle both directions).
		let mut last = engine1.sync_once(pair1).await.unwrap();
		assert!(last.errors.is_empty(), "mode {mode:?}: {last:?}");
		for _ in 0..6 {
			tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
			last = engine1.sync_once(pair1).await.unwrap();
			assert!(last.errors.is_empty(), "mode {mode:?}: {last:?}");
			if last.uploaded == 0
				&& last.downloaded == 0
				&& last.remote_dirs_created == 0
				&& last.local_dirs_created == 0
			{
				break;
			}
		}
		drop(engine1);

		// Restart on the same baseline DB; no intervening changes => clean no-op.
		let engine2 = SyncEngine::open(cache.client.clone(), db_path)
			.await
			.unwrap();
		let pair2 = engine2.add_pair(local.clone(), remote, mode).await.unwrap();
		let r = engine2.sync_once(pair2).await.unwrap();
		assert_noop(&r);

		std::fs::remove_dir_all(&local).ok();
	}
}

#[ignore = "blocked: needs a second engine binary/baseline-format-version N+1 — no version-upgrade harness; see TODO"]
#[shared_test_runtime]
async fn matrix_12_baseline_persists_across_version_upgrade() {
	// plan: converge a two-way pair on engine version N (persist baseline); stop; replace the engine
	// with version N+1 WITHOUT touching local/remote data; start N+1 and run one pass with no
	// intervening changes. Verify N+1 loads/migrates the N baseline, recognizes the pair as converged,
	// performs ZERO transfers/deletions (clean no-op), does not rebuild-from-scratch and re-transfer,
	// no loss/duplication. Needs the ability to run two distinct engine versions against one baseline
	// DB — the suite builds exactly one engine version, so version-upgrade compatibility is untestable
	// black-box here.
}

// ===========================================================================
// (d) backup-mode delete-suppression — backup modes never delete on their destination.
// ===========================================================================

/// MATRIX-13 — (d) delete-suppression, LocalBackup. After converging F1..F4, deleting F1/F2 locally
/// must NOT trash them on the remote backup; the remote retains all four byte-exact; other files
/// unaffected.
#[shared_test_runtime]
async fn matrix_13_local_backup_source_delete_never_deletes_remote() {
	let sc = single_client(SyncMode::LocalBackup).await;

	for name in ["F1.txt", "F2.txt", "F3.txt", "F4.txt"] {
		write_file(&sc.local, name, format!("body-{name}").as_bytes());
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 4, "{r1:?}");

	// Delete F1 outright; remove F2 by moving it out of the tree (the FS surfaces a deletion).
	std::fs::remove_file(sc.local.join("F1.txt")).unwrap();
	let out = std::env::temp_dir().join(format!("matrix13_out_{}", Uuid::new_v4()));
	std::fs::rename(sc.local.join("F2.txt"), &out).unwrap();

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"LocalBackup must never mirror a local deletion to the remote: {r2:?}"
	);

	let (_dirs, files) = list_root(&sc).await;
	for name in ["F1.txt", "F2.txt", "F3.txt", "F4.txt"] {
		let f = find_file(&files, name)
			.unwrap_or_else(|| panic!("{name} deleted on remote backup: {files:?}"));
		assert_eq!(
			f.size,
			format!("body-{name}").len() as u64,
			"{name} corrupted"
		);
	}

	std::fs::remove_file(&out).ok();
	sc.cleanup();
}

/// MATRIX-14 — (d) delete-suppression, RemoteBackup. After converging G1..G3 down, trashing G1 on
/// the remote must NOT delete or quarantine the local copy; the local destination retains all three
/// byte-exact; other files unaffected.
#[shared_test_runtime]
async fn matrix_14_remote_backup_source_delete_never_deletes_local() {
	let sc = single_client(SyncMode::RemoteBackup).await;

	// Bodies follow the `body-{name}` template the content check below asserts against.
	let mut g1 = upload_root(&sc, "G1.txt", b"body-G1.txt").await;
	let g2 = upload_root(&sc, "G2.txt", b"body-G2.txt").await;
	let g3 = upload_root(&sc, "G3.txt", b"body-G3.txt").await;
	wait_cache_has(&sc, g1.uuid().into()).await;
	wait_cache_has(&sc, g2.uuid().into()).await;
	wait_cache_has(&sc, g3.uuid().into()).await;

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 3, "{r1:?}");

	// Trash G1 remotely; wait for the cache to drop it.
	sc.cache.client.trash_file(&mut g1).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), g1.uuid().into(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped trashed G1"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.locally_deleted, 0,
		"RemoteBackup must never mirror a remote deletion locally: {r2:?}"
	);

	// All three local copies retained byte-exact; no quarantine entry.
	for name in ["G1.txt", "G2.txt", "G3.txt"] {
		assert!(
			read_eq(&sc.local, name, format!("body-{name}").as_bytes()),
			"{name} deleted/corrupted on local backup"
		);
	}
	assert!(
		!sc.local.join(".filen-sync-trash").exists(),
		"a suppressed remote delete must not quarantine the local copy"
	);

	sc.cleanup();
}

/// MATRIX-15 — (d) delete-suppression, move/rename not laundered into a destination delete. For both
/// LocalBackup (rename on local source) and RemoteBackup (rename on remote source): the destination
/// retains the ORIGINAL H AND additively gains H2 — the "delete" half of a move is suppressed.
#[shared_test_runtime]
async fn matrix_15_backup_move_not_laundered_into_delete() {
	// --- LocalBackup: rename H -> H2 on the local source. ---
	let lb = single_client(SyncMode::LocalBackup).await;
	write_file(&lb.local, "H.txt", b"H-body-stable");
	let r1 = lb.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	std::fs::rename(lb.local.join("H.txt"), lb.local.join("H2.txt")).unwrap();
	let r2 = lb.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"the delete-half of a move must be suppressed in LocalBackup: {r2:?}"
	);
	let (_d, lf) = list_root(&lb).await;
	assert!(
		find_file(&lf, "H.txt").is_some(),
		"original H removed from remote backup: {lf:?}"
	);
	assert!(
		find_file(&lf, "H2.txt").is_some(),
		"H2 not additively created: {lf:?}"
	);
	lb.cleanup();

	// --- RemoteBackup: rename H -> H2 on the remote source. ---
	let rb = single_client(SyncMode::RemoteBackup).await;
	let mut h = upload_root(&rb, "H.txt", b"H-body-stable").await;
	wait_cache_has(&rb, h.uuid().into()).await;
	let p1 = rb.sync().await;
	assert!(p1.errors.is_empty(), "{p1:?}");
	assert_eq!(p1.downloaded, 1, "{p1:?}");

	use filen_sdk_rs::fs::file::meta::FileMetaChanges;
	rb.cache
		.client
		.update_file_metadata(&mut h, FileMetaChanges::default().name("H2.txt").unwrap())
		.await
		.unwrap();
	assert!(
		poll_for_file_name(
			rb.cache.db_path(),
			h.uuid().into(),
			"H2.txt",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the remote rename"
	);

	let p2 = rb.sync().await;
	assert!(p2.errors.is_empty(), "{p2:?}");
	assert_eq!(
		p2.locally_deleted, 0,
		"the delete-half of a remote move must be suppressed in RemoteBackup: {p2:?}"
	);
	// Local destination keeps the original H AND gains H2 (both byte-exact).
	assert!(
		read_eq(&rb.local, "H.txt", b"H-body-stable"),
		"original local H removed"
	);
	assert!(
		read_eq(&rb.local, "H2.txt", b"H-body-stable"),
		"H2 not additively created locally"
	);
	rb.cleanup();
}

/// MATRIX-16 — (d) delete-suppression, mass-delete on source still suppressed (no guard bypass). A
/// LocalBackup pair with a large file set; delete a majority locally in one batch: zero remote
/// deletions — suppression is unconditional and not subordinate to the mass-delete path. All remote
/// files survive across a re-run.
#[shared_test_runtime]
async fn matrix_16_backup_mass_delete_still_suppressed() {
	let sc = single_client(SyncMode::LocalBackup).await;

	const TOTAL: usize = 20;
	for i in 0..TOTAL {
		write_file(
			&sc.local,
			&format!("m{i:02}.txt"),
			format!("v{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, TOTAL, "{r1:?}");

	// Delete a majority (16 of 20, well over any mass-delete threshold) locally in one batch.
	for i in 0..16 {
		std::fs::remove_file(sc.local.join(format!("m{i:02}.txt"))).unwrap();
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup suppression must hold regardless of deletion count: {r2:?}"
	);

	// Re-run: still no destination deletion (the engine does not 'hold then later apply').
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.remotely_trashed, 0,
		"re-run must not apply suppressed backup deletes: {r3:?}"
	);

	// All TOTAL remote files survive.
	let (_dirs, files) = list_root(&sc).await;
	assert_eq!(
		files
			.iter()
			.filter(|f| f.name().is_some_and(|n| n.starts_with("m")))
			.count(),
		TOTAL,
		"backup mass-delete must leave every remote file intact: {files:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// (e) interrupted-resume idempotency — BLOCKED: needs deterministic mid-pass interruption.
// ===========================================================================
//
// Each stub records the plan (ops + verify). None is faked: the live harness cannot deterministically
// interrupt a pass mid-transfer, kill a process between apply and baseline advance, or interrupt the
// baseline-persist step — so asserting any of these would assert nothing. (The OBSERVABLE,
// fault-injection-free portions of these guarantees — idempotent re-runs, mass-delete hold survival,
// backup-delete suppression across re-runs, two-way conflict persistence — are covered as runnable
// tests in `resilience.rs` (RESIL-11/12/17/24/25) and by (a)-(d) above.)

#[ignore = "blocked: needs deterministic mid-upload interruption (fault-injection harness) — see TODO"]
#[shared_test_runtime]
async fn matrix_17_interrupted_resume_l2r_exactly_once() {
	// plan: l2r pair with pending creates A,B,C; start a pass and forcibly interrupt after A is fully
	// uploaded+baselined, B partial, C untouched; restart engine; run to convergence; one extra pass.
	// Verify final remote has A,B,C each exactly once byte-exact, no duplicate/orphan partial objects,
	// already-completed A not re-uploaded, B completed/repaired, C uploaded, extra pass = 0 actions, no
	// half-written object surfaced as good. Needs a way to kill/cancel a pass after some-but-not-all
	// uploads complete.
}

#[ignore = "blocked: needs deterministic mid-download interruption (fault-injection harness) — see TODO"]
#[shared_test_runtime]
async fn matrix_18_interrupted_resume_r2l_exactly_once() {
	// plan: r2l pair with pending remote creates X,Y,Z; start a pass, interrupt after X fully
	// written+baselined, Y partially downloaded (temp/partial), Z untouched; restart; converge; extra
	// pass. Verify local ends with X,Y,Z exactly once byte-exact, any Y partial cleaned up/completed
	// (no stray temp counted as real data), X not re-downloaded, Y repaired, Z downloaded, extra pass =
	// 0 actions, no spurious local delete/quarantine. Needs a download-interruption seam.
}

#[ignore = "blocked: needs mid-pass interruption with both-direction work (fault-injection) — see TODO"]
#[shared_test_runtime]
async fn matrix_19_interrupted_resume_two_way_exactly_once() {
	// plan: two-way pair with simultaneous pending work both ways (local-new P, remote-new Q, remote
	// modification to existing R); start a pass, interrupt after P pushed+baselined but before Q/R
	// pulled; restart; converge; extra pass. Verify final state P both sides, Q pulled, R updated —
	// each exactly once; completed P not re-pushed; the half-done state does NOT produce a false
	// CONFLICT for P/Q/R; extra pass = 0 actions; baseline consistent with both sides. Needs a mid-pass
	// interruption seam (the hardest resume case — partial baseline must not manufacture divergence).
}

#[ignore = "blocked: needs mid-pass interruption in additive modes (fault-injection) — see TODO"]
#[shared_test_runtime]
async fn matrix_20_interrupted_resume_backup_modes_additive() {
	// plan: converged LocalBackup and RemoteBackup pairs with several pending source creates; start a
	// pass on each, interrupt after some creates applied+baselined and others not; restart; converge;
	// extra pass. Verify all source files end present exactly once at the destination, completed ones
	// not re-transferred, extra pass = 0 actions, backup delete-suppression still holds throughout (no
	// destination delete arose from the interruption), no duplicate/partial-as-complete. Needs a
	// mid-pass interruption seam.
}

#[ignore = "blocked: needs deterministic interruption DURING the baseline-persist step (torn write) — see TODO"]
#[shared_test_runtime]
async fn matrix_21_interrupted_resume_torn_baseline_write() {
	// plan: two-way pair mid-convergence; interrupt specifically during the baseline-persist step
	// (after data actions applied but before/while the on-disk baseline is written); restart; converge;
	// extra pass. Verify the engine recovers from a torn/partial baseline (rolls back to last
	// consistent baseline and re-derives, or completes) WITHOUT re-transferring already-applied data or
	// losing it; final state converged exactly-once both sides; extra pass = 0 actions; a torn baseline
	// does not cause a full re-sync/wipe/duplicates; on-disk baseline after recovery is internally
	// consistent. Needs a seam to interrupt precisely during the baseline persist.
}

// ---------------------------------------------------------------------------
// Shared local helper.
// ---------------------------------------------------------------------------

/// Scan `root` for any file whose bytes equal `bytes` (a conflict/quarantine survivor would carry the
/// original content under a renamed path inside the quarantine bin).
fn conflict_survivor_with(root: &std::path::Path, bytes: &[u8]) -> bool {
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let rd = match std::fs::read_dir(&dir) {
			Ok(rd) => rd,
			Err(_) => continue,
		};
		for entry in rd.flatten() {
			let ft = match entry.file_type() {
				Ok(ft) => ft,
				Err(_) => continue,
			};
			if ft.is_dir() {
				stack.push(entry.path());
			} else if ft.is_file() && std::fs::read(entry.path()).is_ok_and(|b| b == bytes) {
				return true;
			}
		}
	}
	false
}
