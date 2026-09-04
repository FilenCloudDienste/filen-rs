//! Sync control & lifecycle tests for the two-way sync engine.
//!
//! These cover the operational control plane — adding pairs, running many independent pairs,
//! overlapping/nested roots, baseline persistence across an engine re-open, detecting changes
//! made while the engine was down, and watch-mode lifecycle — rather than the per-file diff logic.
//!
//! The PUBLIC engine surface is intentionally small: `open`, `add_pair` (idempotent for the same
//! `(local, remote, mode)`, refused for overlapping roots), `remove_pair`, `reconfigure_pair`,
//! `sync_once`, `sync_once_observed`, `watch`, `watch_observed`, `watch_with`, and
//! `pause_pair` / `resume_pair` / `is_paused`. There is no public root reconfigure, clean-stop, or
//! baseline-corruption seam. Every plan item that depends on one of those — and every fault-
//! injection item (mid-pass stop, crash, corrupted baseline) — is written as an `#[ignore]` stub
//! that records its plan, because faking it would assert nothing real.
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{SyncEngine, SyncEvent, SyncMode, WatchConfig};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ---------------------------------------------------------------------------
// Local helpers (public-API only)
// ---------------------------------------------------------------------------

/// Open a fresh remote subfolder + derived cache + a fresh local dir, with the empty remote already
/// converged into the cache. Returns the resources, cache, remote uuid, and local path so a test
/// can drive its OWN engine(s) (e.g. to re-open on a stable db path, or run multiple pairs).
async fn raw_setup(
	tag: &str,
) -> (
	test_utils::TestResources,
	TestCache,
	Uuid,
	std::path::PathBuf,
) {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir(tag);
	(resources, cache, remote, local)
}

/// List (dirs, files) directly under the resources' remote root (ground truth via the client).
async fn list_remote(
	resources: &test_utils::TestResources,
) -> (
	Vec<filen_sdk_rs::fs::dir::RemoteDirectory>,
	Vec<filen_sdk_rs::fs::file::RemoteFile>,
) {
	resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

/// List (dirs, files) directly under an arbitrary remote dir.
async fn list_dir(
	client: &filen_sdk_rs::auth::Client,
	dir: &filen_sdk_rs::fs::dir::RemoteDirectory,
) -> (
	Vec<filen_sdk_rs::fs::dir::RemoteDirectory>,
	Vec<filen_sdk_rs::fs::file::RemoteFile>,
) {
	client
		.list_dir(
			&DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

/// Upload `data` as `name` directly under `parent` and wait until `cache` observes it, so a pass
/// run afterwards reconciles against a remote view that already contains it.
async fn seed_remote_file(cache: &TestCache, parent: Uuid, name: &str, data: &[u8]) -> Uuid {
	let builder = cache.client.make_file_builder(name, parent).unwrap();
	let file = cache.client.upload_file(builder, data).await.unwrap();
	assert!(
		poll_for_item(cache.db_path(), file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed the seeded remote file {name}"
	);
	file.uuid()
}

fn has_file(files: &[filen_sdk_rs::fs::file::RemoteFile], name: &str) -> bool {
	files.iter().any(|f| f.name() == Some(name))
}

/// A watch observer that just records every event, plus the count of completed passes — enough to
/// assert that a paused loop is idling rather than working.
type EventLog = Arc<Mutex<Vec<SyncEvent>>>;

fn recording_observer(log: EventLog) -> Box<dyn FnMut(SyncEvent) + Send + 'static> {
	Box::new(move |event| log.lock().unwrap().push(event))
}

fn passes(log: &EventLog) -> usize {
	log.lock()
		.unwrap()
		.iter()
		.filter(|e| matches!(e, SyncEvent::PassCompleted { .. }))
		.count()
}

// ===========================================================================
// CONTROL-04 — add a second pair while another is mid-operation; both converge
// independently (one-shot variant: two disjoint pairs on one engine).
// ===========================================================================

#[shared_test_runtime]
async fn control_04_add_second_pair_disjoint_both_converge() {
	// P1: localA -> remoteA (push). P2: localB <- remoteB (pull). Disjoint roots, one engine.
	let resources = test_utils::RESOURCES.get_resources().await;
	let root: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, root).await;
	wait_for_converged_resync(&cache.messages, root, 0, CACHE_CONVERGE_TIMEOUT).await;

	// A subfolder per pair, inside the same converged cache subtree. They must be SIBLINGS: a pair
	// rooted at the shared root would contain the other's folder, which `add_pair` refuses (and
	// which would have P1 mirror the absence of P2's items).
	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let sub_a = cache
		.client
		.create_dir(&root_dt, "p1_remote")
		.await
		.unwrap();
	let sub_b = cache
		.client
		.create_dir(&root_dt, "p2_remote")
		.await
		.unwrap();
	let remote_a: Uuid = sub_a.uuid();
	let remote_b: Uuid = sub_b.uuid();
	// Seed a pending remote file for P2.
	let b_builder = cache
		.client
		.make_file_builder("pulled.txt", sub_b.uuid())
		.unwrap();
	let b_file = cache
		.client
		.upload_file(b_builder, b"from remote b")
		.await
		.unwrap();
	assert!(
		poll_for_item(cache.db_path(), b_file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed P2's remote file"
	);

	let local_a = fresh_local_dir("c04a");
	let local_b = fresh_local_dir("c04b");
	write_file(&local_a, "pushed.txt", b"from local a");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair1 = engine
		.add_pair(local_a.clone(), remote_a, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let pair2 = engine
		.add_pair(local_b.clone(), remote_b, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	assert_ne!(pair1, pair2, "disjoint pairs must get distinct ids");

	let (r1, r2) = tokio::join!(engine.sync_once(pair1), engine.sync_once(pair2));
	let r1 = r1.unwrap();
	let r2 = r2.unwrap();
	assert!(r1.errors.is_empty(), "P1 errors: {r1:?}");
	assert!(r2.errors.is_empty(), "P2 errors: {r2:?}");

	// P1 uploaded its own file and NOT P2's; P2 downloaded its own and NOT P1's.
	assert_eq!(
		r1.uploaded, 1,
		"P1 should upload exactly its own file: {r1:?}"
	);
	assert_eq!(r1.downloaded, 0, "P1 (push) downloads nothing: {r1:?}");
	assert_eq!(
		r2.downloaded, 1,
		"P2 should download exactly its own file: {r2:?}"
	);
	assert_eq!(r2.uploaded, 0, "P2 (pull) uploads nothing: {r2:?}");

	// P2's file landed locally byte-exact; neither pair's content leaked into the other's root.
	assert!(
		read_eq(&local_b, "pulled.txt", b"from remote b"),
		"P2 pull missing/wrong"
	);
	assert!(
		!local_b.join("pushed.txt").exists(),
		"P1's file leaked into P2's root"
	);
	assert!(
		!local_a.join("pulled.txt").exists(),
		"P2's file leaked into P1's root"
	);

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ===========================================================================
// CONTROL-07 — multiple independent disjoint pairs in one invocation, no
// cross-contamination.
// ===========================================================================

#[shared_test_runtime]
async fn control_07_three_disjoint_pairs_no_cross_contamination() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let root: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, root).await;
	wait_for_converged_resync(&cache.messages, root, 0, CACHE_CONVERGE_TIMEOUT).await;

	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let r1 = cache.client.create_dir(&root_dt, "p1").await.unwrap();
	let r2 = cache.client.create_dir(&root_dt, "p2").await.unwrap();
	let r3 = cache.client.create_dir(&root_dt, "p3").await.unwrap();

	// P2 gets a pending remote create (it's the pull pair).
	let p2b = cache.client.make_file_builder("r2.txt", r2.uuid()).unwrap();
	let p2f = cache.client.upload_file(p2b, b"p2 remote").await.unwrap();
	assert!(poll_for_item(cache.db_path(), p2f.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let local1 = fresh_local_dir("c07p1"); // local create
	let local2 = fresh_local_dir("c07p2"); // remote create -> pull
	let local3 = fresh_local_dir("c07p3"); // two-way, nothing staged
	write_file(&local1, "l1.txt", b"p1 local");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair1 = engine
		.add_pair(local1.clone(), r1.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let pair2 = engine
		.add_pair(local2.clone(), r2.uuid(), SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let pair3 = engine
		.add_pair(local3.clone(), r3.uuid(), SyncMode::TwoWay)
		.await
		.unwrap();

	let rep1 = engine.sync_once(pair1).await.unwrap();
	let rep2 = engine.sync_once(pair2).await.unwrap();
	let rep3 = engine.sync_once(pair3).await.unwrap();
	for (label, r) in [("P1", &rep1), ("P2", &rep2), ("P3", &rep3)] {
		assert!(r.errors.is_empty(), "{label} errors: {r:?}");
	}

	assert_eq!(rep1.uploaded, 1, "P1: one upload only: {rep1:?}");
	assert_eq!(rep2.downloaded, 1, "P2: one download only: {rep2:?}");
	// P3 had nothing staged before baseline; its first pass just establishes an empty baseline.
	assert_eq!(rep3.uploaded, 0, "P3 first pass uploads nothing: {rep3:?}");
	assert_eq!(
		rep3.downloaded, 0,
		"P3 first pass downloads nothing: {rep3:?}"
	);

	// Each change applied only within its own roots.
	assert!(read_eq(&local2, "r2.txt", b"p2 remote"), "P2 pull missing");
	assert!(!local1.join("r2.txt").exists());
	assert!(!local3.join("r2.txt").exists());
	assert!(!local2.join("l1.txt").exists());

	// Verify P1's file landed under its OWN remote root (r1), not r2/r3.
	let (p1_dirs, p1_files) = list_dir(&cache.client, &r1).await;
	assert!(p1_dirs.is_empty());
	assert!(has_file(&p1_files, "l1.txt"), "P1's file not under r1");

	std::fs::remove_dir_all(&local1).ok();
	std::fs::remove_dir_all(&local2).ok();
	std::fs::remove_dir_all(&local3).ok();
}

// ===========================================================================
// CONTROL-10 — identical (L, R) for two add_pair calls is idempotent (the
// documented contract: "idempotent for the same (local_root, remote_root)").
// ===========================================================================

#[shared_test_runtime]
async fn control_10_identical_pair_add_is_idempotent() {
	let (resources, cache, remote, local) = raw_setup("c10").await;
	write_file(&local, "only.txt", b"once");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let p1 = engine
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	// Re-add the IDENTICAL (local, remote): must de-duplicate to the SAME logical pair id.
	let p2 = engine
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	assert_eq!(
		p1, p2,
		"identical (local, remote) must yield the same pair id"
	);

	// Running each id must not double-apply: only one upload total.
	let r1 = engine.sync_once(p1).await.unwrap();
	assert_eq!(r1.uploaded, 1, "first pass: one upload: {r1:?}");
	let r2 = engine.sync_once(p2).await.unwrap();
	assert_eq!(
		r2.uploaded, 0,
		"the dup id shares the baseline; no re-upload: {r2:?}"
	);

	let (_d, files) = list_remote(&resources).await;
	assert_eq!(
		files.len(),
		1,
		"exactly one remote file, not double-applied"
	);

	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// CONTROL-11 — baseline persists across an engine re-open: no redundant
// re-transfer. (Re-open the SyncEngine on the SAME db path + re-add the SAME
// roots; per the documented idempotency this rehydrates the persisted pair and
// its baseline.)
// ===========================================================================

#[shared_test_runtime]
async fn control_11_baseline_persists_across_reopen() {
	let (_resources, cache, remote, local) = raw_setup("c11").await;
	for i in 0..6 {
		write_file(
			&local,
			&format!("f{i}.txt"),
			format!("content {i}").as_bytes(),
		);
	}
	write_file(&local, "nested/deep.txt", b"deep");

	let db = temp_cache_path();

	// --- session 1: converge a baseline ---
	{
		let engine = SyncEngine::open(cache.client.clone(), db.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::TwoWay)
			.await
			.unwrap();
		let r = engine.sync_once(pair).await.unwrap();
		assert!(r.errors.is_empty(), "{r:?}");
		assert_eq!(r.uploaded, 7, "first pass uploads all files: {r:?}");
	} // engine dropped — "process shutdown"

	let before = walk_tree(&local);

	// --- session 2: re-open on the SAME db, re-add the SAME roots ---
	let engine2 = SyncEngine::open(cache.client.clone(), db.clone())
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 0,
		"persisted baseline must avoid re-upload: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "no re-download either: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "no spurious trash: {r2:?}");
	assert_eq!(r2.locally_deleted, 0, "no spurious delete: {r2:?}");

	// Local tree byte-identical to before the restart.
	assert_eq!(before, walk_tree(&local), "files changed across restart");

	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// CONTROL-12 — changes made WHILE the engine is offline are detected after a
// re-open and applied (no spurious deletions).
// ===========================================================================

#[shared_test_runtime]
async fn control_12_offline_changes_detected_after_reopen() {
	let (resources, cache, remote, local) = raw_setup("c12").await;
	write_file(&local, "shared.txt", b"v1");
	write_file(&local, "stable.txt", b"stable");

	let db = temp_cache_path();

	// --- session 1: baseline both files up to the remote ---
	{
		let engine = SyncEngine::open(cache.client.clone(), db.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::TwoWay)
			.await
			.unwrap();
		let r = engine.sync_once(pair).await.unwrap();
		assert_eq!(r.uploaded, 2, "{r:?}");
	}

	// --- offline mutations ---
	// Local: add off_local.txt, modify shared.txt.
	write_file(&local, "off_local.txt", b"new local");
	write_file(&local, "shared.txt", b"v2-modified-locally");
	// Remote: add off_remote.txt directly via the client.
	let rb = cache
		.client
		.make_file_builder("off_remote.txt", resources.dir.uuid())
		.unwrap();
	let rf = cache.client.upload_file(rb, b"new remote").await.unwrap();
	assert!(
		poll_for_item(cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed the offline remote add"
	);

	// --- session 2: re-open, converge in a bounded loop ---
	let engine2 = SyncEngine::open(cache.client.clone(), db.clone())
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	let mut converged = false;
	for _ in 0..6 {
		let r = engine2.sync_once(pair2).await.unwrap();
		assert!(r.errors.is_empty(), "post-restart pass errored: {r:?}");
		// No spurious destructive action on any pass.
		assert_eq!(
			r.remotely_trashed, 0,
			"spurious remote trash after restart: {r:?}"
		);
		assert_eq!(
			r.locally_deleted, 0,
			"spurious local delete after restart: {r:?}"
		);
		if read_eq(&local, "off_remote.txt", b"new remote") {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(1200)).await;
	}
	assert!(converged, "off_remote.txt never downloaded after restart");

	// Local off_local.txt + shared.txt's local edit propagated to remote.
	let (_d, files) = list_remote(&resources).await;
	assert!(
		has_file(&files, "off_local.txt"),
		"off_local.txt not uploaded"
	);
	assert!(has_file(&files, "shared.txt"));
	let shared = files
		.iter()
		.find(|f| f.name() == Some("shared.txt"))
		.unwrap();
	assert_eq!(
		shared.size,
		b"v2-modified-locally".len() as u64,
		"shared.txt local modification did not propagate"
	);
	// Stable file untouched on both sides.
	assert!(read_eq(&local, "stable.txt", b"stable"));
	assert!(has_file(&files, "stable.txt"));

	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// CONTROL-20 — watch mode: the engine's own writes do not cause an infinite
// re-sync loop. Observe the SyncEvent stream and assert action-events quiesce.
// ===========================================================================

#[shared_test_runtime]
async fn control_20_watch_self_writes_do_not_loop() {
	let (resources, cache, remote, local) = raw_setup("c20").await;

	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	// Count uploads observed across the whole watch lifetime.
	let uploads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
	let uploads_obs = uploads.clone();
	let observer: filen_sdk_rs::sync_engine::SyncObserver = Box::new(move |ev: SyncEvent| {
		if matches!(ev, SyncEvent::Uploading { .. }) {
			uploads_obs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
		}
	});
	let handle = engine.clone().watch_observed(pair, observer).await.unwrap();

	// Create the file AFTER watch started.
	write_file(&local, "loop.txt", b"once");

	// Wait until it lands on the remote.
	let mut landed = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_remote(&resources).await;
		if has_file(&files, "loop.txt") {
			landed = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(landed, "watch never pushed loop.txt");

	// Let several debounce + safety-net cycles elapse with NO user changes, then snapshot the
	// upload count and confirm it stops growing (no self-write feedback loop).
	tokio::time::sleep(Duration::from_secs(8)).await;
	let count_mid = uploads.load(std::sync::atomic::Ordering::SeqCst);
	tokio::time::sleep(Duration::from_secs(8)).await;
	let count_end = uploads.load(std::sync::atomic::Ordering::SeqCst);

	assert_eq!(
		count_mid, count_end,
		"upload events kept growing with no user changes ({count_mid} -> {count_end}) — self-write loop"
	);
	// And the file was uploaded a small, bounded number of times (one + a tolerated re-trigger).
	assert!(
		count_end <= 2,
		"loop.txt uploaded {count_end} times; expected at most a couple"
	);

	// Remote still has exactly one loop.txt.
	let (_d, files) = list_remote(&resources).await;
	assert_eq!(
		files
			.iter()
			.filter(|f| f.name() == Some("loop.txt"))
			.count(),
		1
	);

	drop(handle);
	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// CONTROL-22 — adding a pair with a non-existent local root fails cleanly and
// does not affect a healthy pair. (Black-box: accept either an add-time error
// or a per-pass error; the only hard requirement is no destructive side effect
// and that the healthy pair keeps converging.)
// ===========================================================================

#[shared_test_runtime]
async fn control_22_bad_root_isolated_from_healthy_pair() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote_p1: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote_p1).await;
	wait_for_converged_resync(&cache.messages, remote_p1, 0, CACHE_CONVERGE_TIMEOUT).await;

	// Healthy P1.
	let local_p1 = fresh_local_dir("c22ok");
	write_file(&local_p1, "ok.txt", b"healthy");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair1 = engine
		.add_pair(local_p1.clone(), remote_p1, SyncMode::LocalToRemote)
		.await
		.unwrap();

	// P2: a local root that does NOT exist on disk.
	let missing = std::env::temp_dir().join(format!("c22_missing_{}", Uuid::new_v4()));
	assert!(
		!missing.exists(),
		"precondition: missing root must not exist"
	);
	let add_p2 = engine
		.add_pair(missing.clone(), remote_p1, SyncMode::LocalToRemote)
		.await;
	assert!(
		add_p2.is_err(),
		"add_pair must reject a local root that does not exist"
	);

	// Whether the add itself errors or defers the failure to the pass, the bad pair must never
	// damage anything. Run P1 to completion regardless.
	if let Ok(pair2) = add_p2 {
		match engine.sync_once(pair2).await {
			Err(_) => { /* hard failure on the missing root is acceptable */ }
			Ok(report) => {
				// A bad/empty local root must NOT have trashed the remote (would be the catastrophic
				// "everything was deleted" mirror); the guard / first-sync safety must hold it.
				assert_eq!(
					report.remotely_trashed, 0,
					"missing-root pass must not trash remote files: {report:?}"
				);
			}
		}
	}

	// P1 still converges and uploads its file.
	let r1 = engine.sync_once(pair1).await.unwrap();
	assert!(r1.errors.is_empty(), "healthy P1 errored: {r1:?}");
	assert_eq!(r1.uploaded, 1, "healthy P1 must still upload: {r1:?}");
	let (_d, files) = list_remote(&resources).await;
	assert!(has_file(&files, "ok.txt"), "healthy pair's file missing");

	std::fs::remove_dir_all(&local_p1).ok();
	std::fs::remove_dir_all(&missing).ok();
}

// ===========================================================================
// (review-add) — adding a pair while watch is actively running picks it up
// live without disturbing the running pair.
// ===========================================================================

#[shared_test_runtime]
async fn control_add_during_watch_live_pickup() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let root: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, root).await;
	wait_for_converged_resync(&cache.messages, root, 0, CACHE_CONVERGE_TIMEOUT).await;

	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let r1 = cache.client.create_dir(&root_dt, "watch_p1").await.unwrap();
	let r2 = cache.client.create_dir(&root_dt, "watch_p2").await.unwrap();

	let local1 = fresh_local_dir("cwd1");
	let local2 = fresh_local_dir("cwd2");

	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair1 = engine
		.add_pair(local1.clone(), r1.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let h1 = engine.clone().watch(pair1).await.unwrap();

	// While P1's watcher is live, add P2 and start watching it too.
	let pair2 = engine
		.add_pair(local2.clone(), r2.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let h2 = engine.clone().watch(pair2).await.unwrap();

	// Stage a fresh change in EACH around the same time.
	write_file(&local1, "p1.txt", b"p1 live");
	write_file(&local2, "p2.txt", b"p2 live");

	// Both must converge to their OWN remote roots without a restart.
	let mut p1_ok = false;
	let mut p2_ok = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline && !(p1_ok && p2_ok) {
		let (_d1, f1) = list_dir(&cache.client, &r1).await;
		let (_d2, f2) = list_dir(&cache.client, &r2).await;
		p1_ok = has_file(&f1, "p1.txt");
		p2_ok = has_file(&f2, "p2.txt");
		if !(p1_ok && p2_ok) {
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
	}
	assert!(
		p1_ok,
		"P1's change did not sync while P2 was added under watch"
	);
	assert!(p2_ok, "P2 (added live under watch) did not converge");

	// No cross-contamination: P1's file is not under r2, and vice versa.
	let (_d1, f1) = list_dir(&cache.client, &r1).await;
	assert!(
		!has_file(&f1, "p2.txt"),
		"P2's file leaked under P1's remote root"
	);

	drop(h1);
	drop(h2);
	std::fs::remove_dir_all(&local1).ok();
	std::fs::remove_dir_all(&local2).ok();
}

// ===========================================================================
// Blocked tests — require control-plane verbs or fault-injection that the
// PUBLIC engine API does not expose. Recorded as #[ignore] stubs (plan kept)
// rather than faked, per the suite rules.
//
// Missing public surface (verified against the API reference + black-box use):
//   * reconfigure ROOT                   -> CONTROL-17, -24 (the MODE half is CONTROL-16, implemented)
//   * pair registry auto-load            -> auto-load, idempotent-verbs, CONTROL-25
//   * clean stop / abrupt-kill harness   -> CONTROL-14, -15, -18, remove-mid-pass
//   * baseline corruption/inspection     -> CONTROL-13, partially-corrupted-config
//   * control-transition event stream    -> control-transition-events
// ===========================================================================

/// CONTROL-01 — a paused pair does no work, and resuming it loses nothing: the change staged while
/// it was paused is applied by the first pass afterwards.
#[shared_test_runtime]
async fn control_01_pause_halts_resume_continues() {
	let (resources, cache, remote, local) = raw_setup("c01").await;
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	// A baseline first, so what follows is about the pause and not about first-sync semantics.
	write_file(&local, "base.txt", b"baseline");
	let r = engine.sync_once(pair).await.unwrap();
	assert_eq!(r.uploaded, 1, "{r:?}");
	assert!(!r.paused, "a running pair must not report paused: {r:?}");
	assert!(
		!engine.is_paused(pair).await,
		"a fresh pair must not be paused"
	);

	engine.pause_pair(pair).await.unwrap();
	assert!(engine.is_paused(pair).await, "pause_pair did not take");

	write_file(&local, "a.txt", b"made while paused");
	let held = engine.sync_once(pair).await.unwrap();
	assert!(held.paused, "a paused pass must say so: {held:?}");
	assert_eq!(
		(
			held.uploaded,
			held.downloaded,
			held.remotely_trashed,
			held.locally_deleted
		),
		(0, 0, 0, 0),
		"a paused pass must touch nothing: {held:?}"
	);
	assert!(held.errors.is_empty(), "{held:?}");
	let (_d, files) = list_remote(&resources).await;
	assert!(!has_file(&files, "a.txt"), "a paused pair uploaded anyway");

	// The verbs are idempotent, and an id nobody registered is refused rather than silently taken.
	engine.pause_pair(pair).await.unwrap();
	assert!(
		engine.pause_pair(pair + 9_999).await.is_err(),
		"an unknown pair must not be pausable"
	);

	engine.resume_pair(pair).await.unwrap();
	assert!(!engine.is_paused(pair).await, "resume_pair did not take");
	let resumed = engine.sync_once(pair).await.unwrap();
	assert!(!resumed.paused, "{resumed:?}");
	assert!(resumed.errors.is_empty(), "{resumed:?}");
	assert_eq!(
		resumed.uploaded, 1,
		"the first pass after resume must apply the staged change: {resumed:?}"
	);
	let (_d, files) = list_remote(&resources).await;
	let a = files
		.iter()
		.find(|f| f.name() == Some("a.txt"))
		.expect("a.txt missing after resume");
	assert_eq!(
		a.size,
		b"made while paused".len() as u64,
		"a.txt size mismatch"
	);
	assert!(
		has_file(&files, "base.txt"),
		"the baselined file was disturbed by the pause"
	);
	// Resuming twice is a no-op too.
	engine.resume_pair(pair).await.unwrap();

	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-02 — a pause outlives the periodic safety-net pass: the watch loop keeps ticking and
/// keeps doing nothing, for as long as the pair is paused, and picks the backlog up on resume.
#[shared_test_runtime]
async fn control_02_pause_durable_across_safety_net() {
	const NET: Duration = Duration::from_secs(3);

	let (_resources, cache, remote, local) = raw_setup("c02").await;
	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();

	let log: EventLog = Arc::new(Mutex::new(Vec::new()));
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				safety_net: NET,
			},
			recording_observer(log.clone()),
		)
		.await
		.unwrap();

	// Let the loop prove it is alive (its initial pass), then pause it and let anything already in
	// flight when the pause landed finish, so the count below is a stable floor.
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || passes(&log) >= 1).await,
		"the watch never ran its initial pass"
	);
	engine.pause_pair(pair).await.unwrap();
	tokio::time::sleep(NET).await;
	let before = passes(&log);

	// Two remote adds, then several safety-net intervals with the pair paused.
	seed_remote_file(&cache, remote, "r1.txt", b"one").await;
	seed_remote_file(&cache, remote, "r2.txt", b"two").await;
	tokio::time::sleep(NET * 4).await;

	assert_eq!(
		passes(&log),
		before,
		"a paused pair ran a safety-net pass anyway"
	);
	assert!(
		engine.is_paused(pair).await,
		"the pause did not survive the safety net"
	);
	assert!(
		!local.join("r1.txt").exists() && !local.join("r2.txt").exists(),
		"a paused pair downloaded the remote adds"
	);

	// Resuming picks both up with no manual pass and no re-listing of anything.
	engine.resume_pair(pair).await.unwrap();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || read_eq(&local, "r1.txt", b"one")
			&& read_eq(&local, "r2.txt", b"two"))
		.await,
		"the backlog did not sync after resume (passes={})",
		passes(&log)
	);

	handle.stop().await;
	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-03 — pause/resume is a scheduling change, not a baseline invalidation: a pair paused and
/// resumed with nothing changed re-transfers nothing.
#[shared_test_runtime]
async fn control_03_resume_no_redundant_retransfer() {
	const N: usize = 10;

	let (resources, cache, remote, local) = raw_setup("c03").await;
	for i in 0..N {
		write_file(
			&local,
			&format!("f{i}.txt"),
			format!("content {i}").as_bytes(),
		);
	}
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r = engine.sync_once(pair).await.unwrap();
	assert_eq!(r.uploaded, N, "{r:?}");
	let before = walk_tree(&local);

	engine.pause_pair(pair).await.unwrap();
	engine.resume_pair(pair).await.unwrap();

	let after = engine.sync_once(pair).await.unwrap();
	assert!(after.errors.is_empty(), "{after:?}");
	assert!(!after.paused, "{after:?}");
	assert_eq!(
		(
			after.uploaded,
			after.downloaded,
			after.remotely_trashed,
			after.locally_deleted,
			after.moved_remote,
			after.moved_local,
		),
		(0, 0, 0, 0, 0, 0),
		"pause/resume invalidated the baseline: {after:?}"
	);
	assert!(after.conflicts.is_empty(), "{after:?}");
	assert_eq!(
		before,
		walk_tree(&local),
		"the local tree changed across pause/resume"
	);

	// All ten still present and byte-exact on both sides.
	let (_d, files) = list_remote(&resources).await;
	assert_eq!(files.len(), N, "remote file count changed");
	for i in 0..N {
		let name = format!("f{i}.txt");
		let f = files
			.iter()
			.find(|f| f.name() == Some(name.as_str()))
			.unwrap_or_else(|| panic!("{name} missing on the remote"));
		assert_eq!(
			f.size,
			format!("content {i}").len() as u64,
			"{name} size mismatch"
		);
		assert!(read_eq(&local, &name, format!("content {i}").as_bytes()));
	}

	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-05 — a removed pair stops syncing and leaves BOTH sides exactly as they were: removal
/// is a registry operation, never a destructive one.
#[shared_test_runtime]
async fn control_05_remove_pair_leaves_both_sides_intact() {
	let (resources, cache, remote, local) = raw_setup("c05").await;
	for i in 0..5 {
		write_file(&local, &format!("f{i}.txt"), format!("v{i}").as_bytes());
	}
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 5, "{r1:?}");
	let (_d0, before) = list_remote(&resources).await;
	assert_eq!(before.len(), 5, "precondition: 5 remote files");
	let local_before = walk_tree(&local);

	// Remove the pair — and prove it is gone from the registry.
	engine.remove_pair(pair).await.unwrap();
	assert!(
		engine
			.list_pairs()
			.await
			.unwrap()
			.iter()
			.all(|p| p.id != pair),
		"the removed pair is still registered"
	);

	// Stage a change on each side that a LIVE pair would propagate.
	std::fs::remove_file(local.join("f0.txt")).unwrap();
	write_file(&local, "f1.txt", b"locally edited after removal");
	seed_remote_file(&cache, remote, "added_after_removal.txt", b"remote add").await;

	// The removed pair cannot be synced at all.
	assert!(
		engine.sync_once(pair).await.is_err(),
		"a removed pair must not sync"
	);

	// Remote: the local delete was NOT mirrored, the local edit was NOT pushed.
	let (_d1, after) = list_remote(&resources).await;
	assert!(
		has_file(&after, "f0.txt"),
		"removing a pair trashed a remote file"
	);
	let f1 = after
		.iter()
		.find(|f| f.name() == Some("f1.txt"))
		.expect("f1.txt lost");
	assert_eq!(f1.size, 2, "the post-removal local edit was pushed anyway");

	// Local: the remote add was NOT pulled and nothing was quarantined.
	assert!(
		!local.join("added_after_removal.txt").exists(),
		"the post-removal remote add was pulled anyway"
	);
	assert!(
		!local.join(".filen-sync-trash").exists(),
		"removing a pair quarantined something"
	);
	// The only local difference is the change the test itself made.
	let mut expected = local_before;
	expected.remove("f0.txt");
	expected.insert(
		"f1.txt".to_string(),
		(false, 28, b"locally edited after removal".to_vec()),
	);
	assert_eq!(walk_tree(&local), expected, "the local tree was touched");

	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-06 — remove-then-re-add on the same roots starts from an EMPTY baseline, i.e. with
/// first-sync semantics: the populated destination is not wiped, identical files reconcile with
/// zero transfer, and a genuinely divergent file is surfaced as a conflict instead of one side
/// silently winning.
#[shared_test_runtime]
async fn control_06_remove_then_readd_first_sync_semantics() {
	let (resources, cache, remote, local) = raw_setup("c06").await;
	write_file(&local, "same1.txt", b"identical one");
	write_file(&local, "same2.txt", b"identical two");
	write_file(&local, "diverge.txt", b"BASE");
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	assert_eq!(engine.sync_once(pair).await.unwrap().uploaded, 3);

	engine.remove_pair(pair).await.unwrap();

	// Diverge one path on BOTH sides while no pair owns them.
	write_file(&local, "diverge.txt", b"LOCAL-SIDE");
	seed_remote_file(&cache, remote, "diverge.txt", b"REMOTE-SIDE").await;

	// Re-add the SAME roots: a new pair with no baseline at all.
	let readded = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let r = engine.sync_once(readded).await.unwrap();
	assert!(r.errors.is_empty(), "{r:?}");

	// Nothing destroyed on either side...
	assert_eq!(r.remotely_trashed, 0, "first sync trashed remote: {r:?}");
	assert_eq!(r.locally_deleted, 0, "first sync deleted local: {r:?}");
	// ...identical files cost no transfer...
	assert_eq!(r.uploaded, 0, "identical files re-uploaded: {r:?}");
	assert_eq!(r.downloaded, 0, "identical files re-downloaded: {r:?}");
	// ...and the genuine divergence is surfaced, not silently resolved.
	assert!(
		r.conflict_paths().any(|c| c == "diverge.txt"),
		"divergence must surface on a first sync: {r:?}"
	);

	// Both versions still exist.
	assert!(
		read_eq(&local, "diverge.txt", b"LOCAL-SIDE"),
		"local side lost"
	);
	assert!(read_eq(&local, "same1.txt", b"identical one"));
	let (_d, files) = list_remote(&resources).await;
	assert!(has_file(&files, "same1.txt") && has_file(&files, "same2.txt"));
	let remote_diverge = files
		.iter()
		.find(|f| f.name() == Some("diverge.txt"))
		.expect("remote side lost");
	assert_eq!(remote_diverge.size, b"REMOTE-SIDE".len() as u64);

	std::fs::remove_dir_all(&local).ok();
}

#[ignore = "blocked: no public baseline corruption/inspection seam — needs a fault-injection harness"]
#[shared_test_runtime]
async fn control_13_corrupted_baseline_degrades_to_first_sync() {
	// plan: pair to baseline; shut down; corrupt/truncate the persisted baseline; restart with both
	// sides populated+matching; assert no wipe, first-sync reconcile, warning logged, fresh baseline.
}

#[ignore = "blocked: no clean-stop control — needs deterministic mid-pass stop infrastructure"]
#[shared_test_runtime]
async fn control_14_clean_stop_mid_pass_coherent() {
	// plan: pair with 20 uploads + 5 deletes queued; begin pass; request clean stop after a few
	// applied; assert applied files byte-exact, untouched files intact, baseline only advanced for
	// completed actions; subsequent full pass converges with no data loss.
}

#[ignore = "blocked: no crash/kill harness — needs deterministic abrupt-kill mid-transfer infrastructure"]
#[shared_test_runtime]
async fn control_15_abrupt_kill_mid_pass_recoverable() {
	// plan: begin pass with in-flight uploads + a pending delete; hard-kill mid-transfer; restart;
	// pass => no truncated files presented as complete, interrupted action fully redone or skipped,
	// byte-exact convergence, no orphaned partials in quarantine.
}

/// CONTROL-16 — reconfiguring a pair's MODE applies from the next pass and costs nothing: the
/// baseline is kept (no re-transfer, no full re-sync), and a change made AFTER the switch is
/// handled by the new mode.
#[shared_test_runtime]
async fn control_16_reconfigure_mode_prospective() {
	let sc = single_client(SyncMode::LocalBackup).await;
	for name in ["keep1.txt", "keep2.txt", "gone.txt"] {
		write_file(&sc.local, name, content_for(name).as_slice());
	}
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	sc.engine
		.reconfigure_pair(sc.pair, SyncMode::TwoWay)
		.await
		.unwrap();

	// The switch alone is a no-op: nothing is re-transferred and the baseline still holds.
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "no re-upload from the mode change: {r2:?}");
	assert_eq!(r2.downloaded, 0, "no re-download either: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	// A deletion made AFTER the switch is mirrored per the new mode (local-backup would not have).
	std::fs::remove_file(sc.local.join("gone.txt")).unwrap();
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.remotely_trashed, 1,
		"the new mode propagates the deletion: {r3:?}"
	);
	assert_eq!(r3.uploaded, 0, "the untouched files stay put: {r3:?}");

	let (_dirs, files) = list_remote(&sc.resources).await;
	let mut names: Vec<&str> = files.iter().filter_map(|f| f.name()).collect();
	names.sort_unstable();
	assert_eq!(names, vec!["keep1.txt", "keep2.txt"], "{names:?}");

	sc.cleanup();
}

#[ignore = "blocked: no public reconfigure — changing a pair's ROOT must stop the old root and safely reconcile the new"]
#[shared_test_runtime]
async fn control_17_reconfigure_root_safe() {
	// plan: pair (L_old, R) to baseline; reconfigure local root to L_new (populated); pass =>
	// L_old contents intact (not deleted from remote), L_new reconciled w/ first-sync-against-
	// populated safety, no mass-delete, coherent new baseline.
}

// `pause_pair` deliberately does NOT interrupt a pass already running — it stops the NEXT one —
// so this test's premise ("no NEW actions begin, in-flight action atomic/rolled back") is not the
// contract; whether pause should also preempt is an owner call, and asserting it needs the same
// mid-pass barrier CONTROL-14 wants.
#[ignore = "blocked: pause does not preempt an in-flight pass by design; needs an owner decision plus a mid-pass barrier"]
#[shared_test_runtime]
async fn control_18_pause_mid_pass_coherent() {
	// plan: start a many-action pass; pause mid-apply; assert no NEW actions begin, in-flight action
	// atomic/rolled back, resume converges byte-exact, baseline only reflects completed actions.
}

/// CONTROL-19 — removing ONE of several pairs affects only that pair: the others keep converging,
/// the removed pair's roots are untouched on both sides, and its baseline rows are gone.
#[shared_test_runtime]
async fn control_19_remove_one_among_several() {
	let (resources, cache, _root, local_1) = raw_setup("c19a").await;
	// Three disjoint remote roots inside the same converged cache subtree. Each pair gets its OWN
	// subfolder: a pair rooted at the shared root would see its siblings' folders as remote-only
	// items and mirror their absence.
	let mut remotes = Vec::new();
	for name in ["c19_p1", "c19_p2", "c19_p3"] {
		let dir = cache
			.client
			.create_dir(
				&DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir)),
				name,
			)
			.await
			.unwrap();
		remotes.push(dir);
	}
	let (remote_1, remote_2, remote_3) = (remotes[0].uuid(), remotes[1].uuid(), remotes[2].uuid());
	let local_2 = fresh_local_dir("c19b");
	let local_3 = fresh_local_dir("c19c");

	write_file(&local_1, "p1.txt", b"one");
	write_file(&local_2, "p2.txt", b"two");
	write_file(&local_3, "p3.txt", b"three");

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_1 = engine
		.add_pair(local_1.clone(), remote_1, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let pair_2 = engine
		.add_pair(local_2.clone(), remote_2, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let pair_3 = engine
		.add_pair(local_3.clone(), remote_3, SyncMode::LocalToRemote)
		.await
		.unwrap();
	for pair in [pair_1, pair_2, pair_3] {
		assert_eq!(engine.sync_once(pair).await.unwrap().uploaded, 1);
	}
	assert_eq!(engine.list_pairs().await.unwrap().len(), 3);

	// Remove ONLY P2.
	engine.remove_pair(pair_2).await.unwrap();
	let remaining: Vec<i64> = engine
		.list_pairs()
		.await
		.unwrap()
		.into_iter()
		.map(|p| p.id)
		.collect();
	assert_eq!(
		remaining,
		vec![pair_1, pair_3],
		"wrong registry after removal"
	);

	// Stage a change in every root, including the removed pair's.
	write_file(&local_1, "p1_new.txt", b"one more");
	write_file(&local_2, "p2_new.txt", b"must not sync");
	write_file(&local_3, "p3_new.txt", b"three more");

	assert_eq!(engine.sync_once(pair_1).await.unwrap().uploaded, 1);
	assert_eq!(engine.sync_once(pair_3).await.unwrap().uploaded, 1);
	assert!(
		engine.sync_once(pair_2).await.is_err(),
		"the removed pair must not sync"
	);

	// P1/P3 advanced; P2's roots are untouched on both sides.
	let (_d1, files_1) = list_dir(&cache.client, &remotes[0]).await;
	assert!(has_file(&files_1, "p1_new.txt"), "P1 did not advance");
	let (_d3, files_3) = list_dir(&cache.client, &remotes[2]).await;
	assert!(has_file(&files_3, "p3_new.txt"), "P3 did not advance");
	let (_d2, files_2) = list_dir(&cache.client, &remotes[1]).await;
	assert!(
		has_file(&files_2, "p2.txt"),
		"P2's already-synced file was removed"
	);
	assert!(
		!has_file(&files_2, "p2_new.txt"),
		"the removed pair uploaded anyway"
	);
	assert!(
		read_eq(&local_2, "p2.txt", b"two"),
		"P2's local root was touched"
	);
	assert!(!local_2.join(".filen-sync-trash").exists());

	// Re-adding P2 starts over from an empty baseline (its rows were cascaded away): the file
	// created while it was removed is a fresh create, uploaded on the next pass.
	let readded = engine
		.add_pair(local_2.clone(), remote_2, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r = engine.sync_once(readded).await.unwrap();
	assert!(r.errors.is_empty(), "{r:?}");
	assert_eq!(r.uploaded, 1, "only the new file needs pushing: {r:?}");

	for dir in [&local_1, &local_2, &local_3] {
		std::fs::remove_dir_all(dir).ok();
	}
}

/// CONTROL-21 — a backlog accumulated over a long pause converges in a few resumed passes, with
/// every staged change applied exactly once: nothing dropped, nothing double-applied.
#[shared_test_runtime]
async fn control_21_resume_long_pause_backlog_converges() {
	const BASE: usize = 15;
	const NEW_LOCAL: usize = 30;
	const NEW_REMOTE: usize = 10;
	const DELETED: usize = 5;
	const MODIFIED: usize = 5;

	let (resources, cache, remote, local) = raw_setup("c21").await;
	for i in 0..BASE {
		write_file(&local, &format!("b{i:02}.txt"), format!("v{i}").as_bytes());
	}
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let first = engine.sync_once(pair).await.unwrap();
	assert_eq!(first.uploaded, BASE, "{first:?}");

	engine.pause_pair(pair).await.unwrap();

	// Stage the whole backlog while paused: creates on both sides, local deletions, and remote
	// edits of already-baselined files.
	for i in 0..NEW_LOCAL {
		write_file(&local, &format!("n{i:02}.txt"), format!("n{i}").as_bytes());
	}
	for i in 0..NEW_REMOTE {
		seed_remote_file(
			&cache,
			remote,
			&format!("r{i}.txt"),
			format!("r{i}").as_bytes(),
		)
		.await;
	}
	for i in 0..DELETED {
		std::fs::remove_file(local.join(format!("b{i:02}.txt"))).unwrap();
	}
	// A same-name re-upload is how the server versions a file; the cache announces the new uuid.
	for i in DELETED..DELETED + MODIFIED {
		seed_remote_file(
			&cache,
			remote,
			&format!("b{i:02}.txt"),
			format!("edited {i}").as_bytes(),
		)
		.await;
	}

	// Nothing moved while paused.
	let paused_pass = engine.sync_once(pair).await.unwrap();
	assert!(paused_pass.paused, "{paused_pass:?}");
	let (_d, during) = list_remote(&resources).await;
	assert!(
		!has_file(&during, "n00.txt"),
		"a paused pair uploaded its backlog"
	);
	assert!(
		has_file(&during, "b00.txt"),
		"a paused pair propagated a local deletion"
	);

	engine.resume_pair(pair).await.unwrap();

	// Converge, accumulating the counts so a change applied twice is visible as an excess.
	let (mut uploaded, mut downloaded, mut trashed) = (0usize, 0usize, 0usize);
	let mut converged = false;
	for _ in 0..8 {
		let r = engine.sync_once(pair).await.unwrap();
		assert!(r.errors.is_empty(), "{r:?}");
		assert!(r.conflicts.is_empty(), "unexpected conflict: {r:?}");
		assert_eq!(r.held_deletions(), 0, "the guard held the backlog: {r:?}");
		uploaded += r.uploaded;
		downloaded += r.downloaded;
		trashed += r.remotely_trashed;
		if uploaded >= NEW_LOCAL && downloaded >= NEW_REMOTE + MODIFIED && trashed >= DELETED {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	assert!(
		converged,
		"the backlog did not converge (up={uploaded}, down={downloaded}, trashed={trashed})"
	);
	assert_eq!(uploaded, NEW_LOCAL, "local creates applied more than once");
	assert_eq!(
		downloaded,
		NEW_REMOTE + MODIFIED,
		"remote creates/edits applied more than once"
	);
	assert_eq!(trashed, DELETED, "local deletes applied more than once");

	// Both sides byte-exact afterwards.
	for i in 0..NEW_LOCAL {
		assert!(read_eq(
			&local,
			&format!("n{i:02}.txt"),
			format!("n{i}").as_bytes()
		));
	}
	for i in 0..NEW_REMOTE {
		assert!(
			read_eq(&local, &format!("r{i}.txt"), format!("r{i}").as_bytes()),
			"r{i}.txt did not land locally"
		);
	}
	for i in DELETED..DELETED + MODIFIED {
		assert!(
			read_eq(
				&local,
				&format!("b{i:02}.txt"),
				format!("edited {i}").as_bytes()
			),
			"the remote edit of b{i:02}.txt did not land locally"
		);
	}
	let (_d, after) = list_remote(&resources).await;
	for i in 0..DELETED {
		assert!(
			!has_file(&after, &format!("b{i:02}.txt")),
			"the local deletion of b{i:02}.txt was not mirrored"
		);
	}
	for i in 0..NEW_LOCAL {
		assert!(
			has_file(&after, &format!("n{i:02}.txt")),
			"n{i:02}.txt did not reach the remote"
		);
	}

	// Settled: another pass has nothing left to do.
	let settled = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		(
			settled.uploaded,
			settled.downloaded,
			settled.remotely_trashed
		),
		(0, 0, 0),
		"the resumed backlog did not settle: {settled:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-23 — the paused flag is persisted with the pair: an engine reopened on the same baseline
/// DB comes back paused, and stays paused until an explicit resume. The resume persists too.
#[shared_test_runtime]
async fn control_23_pause_status_survives_restart() {
	let (resources, cache, remote, local) = raw_setup("c23").await;
	write_file(&local, "before.txt", b"baselined");
	let db = temp_cache_path();

	// --- session 1: baseline, then pause ---
	let pair = {
		let engine = SyncEngine::open(cache.client.clone(), db.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::TwoWay)
			.await
			.unwrap();
		assert_eq!(engine.sync_once(pair).await.unwrap().uploaded, 1);
		engine.pause_pair(pair).await.unwrap();
		pair
	}; // engine dropped — "process shutdown"

	// A change staged while nothing at all is running.
	write_file(&local, "while_down.txt", b"staged");

	// --- session 2: re-open on the SAME db ---
	let engine2 = SyncEngine::open(cache.client.clone(), db.clone())
		.await
		.unwrap();
	assert!(
		engine2.is_paused(pair).await,
		"the pause did not survive the restart"
	);
	let report = engine2.sync_once(pair).await.unwrap();
	assert!(
		report.paused,
		"a reopened paused pair must still refuse to sync: {report:?}"
	);
	let (_d, files) = list_remote(&resources).await;
	assert!(
		!has_file(&files, "while_down.txt"),
		"a reopened paused pair synced anyway"
	);

	engine2.resume_pair(pair).await.unwrap();
	let resumed = engine2.sync_once(pair).await.unwrap();
	assert!(resumed.errors.is_empty(), "{resumed:?}");
	assert_eq!(
		resumed.uploaded, 1,
		"the change staged while down did not sync after resume: {resumed:?}"
	);
	let (_d, files) = list_remote(&resources).await;
	assert!(has_file(&files, "while_down.txt"));
	drop(engine2);

	// --- session 3: the resume is persisted too, so a restart does not re-pause ---
	let engine3 = SyncEngine::open(cache.client.clone(), db).await.unwrap();
	assert!(
		!engine3.is_paused(pair).await,
		"the resume did not survive a restart"
	);

	std::fs::remove_dir_all(&local).ok();
}

#[ignore = "blocked: no public reconfigure — reconfigure-while-paused must defer all effect until resume"]
#[shared_test_runtime]
async fn control_24_reconfigure_while_paused_deferred() {
	// plan: pair to baseline, pause; reconfigure mode (local-backup -> two-way) while paused; stage
	// a local delete while paused; resume; pass => no action while paused, NEW mode governs the
	// delete (mirrored), no old-mode action, baseline consistent with new mode.
}

#[ignore = "blocked: needs a concurrent control-op serialization harness (the verbs themselves now exist)"]
#[shared_test_runtime]
async fn control_25_concurrent_control_ops_serialized() {
	// plan: P1/P2 active; near-simultaneous remove(P1), pause(P2), add(P3); run all + stage changes
	// in P2/P3 => registry exactly {P1 absent, P2 paused, P3 present}; P2 change NOT synced, P3 IS;
	// P1 roots intact; no deadlock/panic/lost/dup entries.
}

#[ignore = "blocked: no public registry/auto-load — pairs must rehydrate + resume schedule on a fresh engine WITHOUT re-add"]
#[shared_test_runtime]
async fn control_add_persisted_pairs_autoload_on_fresh_start() {
	// plan: add 2 pairs (1 active, 1 paused), converge, shut down; start a NEW engine WITHOUT any
	// add() calls; query registry => both reappear w/ roots/modes/state; active pair's staged change
	// syncs without re-add; paused stays paused; baselines reused (no redundant transfer).
}

#[ignore = "blocked: no public remove_pair / clean-stop — removing a pair mid-pass must stop promptly + leave both sides intact"]
#[shared_test_runtime]
async fn control_add_remove_pair_mid_pass() {
	// plan: pair with 20 uploads + 5 deletes queued; begin pass; remove(pair) mid-apply; settle =>
	// no NEW actions, in-flight atomic/rolled back, pair gone from registry + baseline cleaned up,
	// both sides coherent, a later re-add behaves as first-sync-against-populated.
}

// The pause/resume half of this is now covered by CONTROL-01 (pause-already-paused,
// resume-already-running, an unknown id refused); what is left is the remove/re-add ordering.
#[ignore = "blocked: needs the remove/re-add half of the invalid-order verb matrix (double-remove, remove-then-sync, ...)"]
#[shared_test_runtime]
async fn control_add_invalid_order_control_verbs_idempotent() {
	// plan: add+converge a pair; remove() twice; pause() an already-paused pair; resume() a never-
	// paused pair; pause()/remove() an unknown id; run all => each redundant op a clean no-op or
	// clear error (no panic/deadlock/corruption), registry exactly correct, others sync normally,
	// no file deleted/transferred as a side effect.
}

#[ignore = "blocked: no public control-transition event stream — only per-pass SyncEvents exist, not added/paused/resumed/reconfigured/removed signals"]
#[shared_test_runtime]
async fn control_add_control_transition_events_emitted() {
	// plan: add, pause, resume, reconfigure, remove a pair while capturing the control event stream;
	// cross-check each op emits exactly one correct state-transition signal, no spurious/dup events,
	// ordering matches issued sequence, per-pass reports attributable to a pair + its control state.
}

#[ignore = "blocked: no public multi-pair config corruption seam — needs per-pair persisted-config fault injection"]
#[shared_test_runtime]
async fn control_add_partially_corrupted_multipair_config_isolated() {
	// plan: persist 3 pairs + converge; shut down; corrupt ONLY P2's persisted state; restart + run
	// all => P1/P3 auto-load + sync w/ reused baselines, P2 degrades safely (first-sync or marked
	// errored, never wipes a side), clear warning names P2, startup does not fail wholesale.
}

/// CONTROL-08 — overlapping LOCAL roots are refused, in every direction, with an error naming the
/// pair that already owns the folder. Two pairs sharing a subtree would each read the other's
/// writes as foreign changes and never converge, so the overlap is rejected at registration rather
/// than arbitrated later.
#[shared_test_runtime]
async fn control_08_nested_local_roots_deterministic() {
	let (resources, cache, _root, local) = raw_setup("c08").await;
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	std::fs::create_dir_all(local.join("sub")).unwrap();
	let outer = local.parent().unwrap().to_path_buf();

	// Two SIBLING remote folders, so the local roots are the only thing under test.
	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let mine = cache.client.create_dir(&root_dt, "c08_mine").await.unwrap();
	let other = cache
		.client
		.create_dir(&root_dt, "c08_other")
		.await
		.unwrap();

	let p1 = engine
		.add_pair(local.clone(), mine.uuid(), SyncMode::TwoWay)
		.await
		.unwrap();

	for (label, candidate, expected) in [
		(
			"the same folder",
			local.clone(),
			"already syncs the local root",
		),
		(
			"a folder inside it",
			local.join("sub"),
			"is inside sync pair",
		),
		("a folder containing it", outer, "contains sync pair"),
	] {
		let error = match engine
			.add_pair(candidate.clone(), other.uuid(), SyncMode::TwoWay)
			.await
		{
			Ok(id) => panic!("{label} ({candidate:?}) was accepted as pair {id}"),
			Err(error) => error.to_string(),
		};
		assert!(
			error.contains(expected),
			"{label}: the refusal must say how the roots overlap, got {error}"
		);
	}

	// Every refusal left the registry untouched: the original pair, and only it, is registered.
	let pairs = engine.list_pairs().await.unwrap();
	assert_eq!(pairs.len(), 1, "{pairs:?}");
	assert_eq!(pairs[0].id, p1);

	// A disjoint local root against the second remote is still accepted.
	let elsewhere = fresh_local_dir("c08b");
	engine
		.add_pair(elsewhere.clone(), other.uuid(), SyncMode::TwoWay)
		.await
		.expect("a disjoint pair is accepted");

	std::fs::remove_dir_all(&local).ok();
	std::fs::remove_dir_all(&elsewhere).ok();
}

/// (add) — the overlap refusal must hold when two registrations run CONCURRENTLY, which is how an
/// app registering its pairs at startup issues them. The check reads the registry and then writes
/// to it with an await in between, so without serialization both calls see a registry neither has
/// written to yet and both commit — leaving exactly the overlapping pair of pairs that never
/// converges.
#[shared_test_runtime]
async fn control_add_concurrent_overlapping_registrations_refused() {
	let (resources, cache, _root, local) = raw_setup("c08c").await;
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let nested = local.join("sub");
	std::fs::create_dir_all(&nested).unwrap();

	// Two SIBLING remote folders, so the nested LOCAL roots are the only overlap.
	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let mine = cache
		.client
		.create_dir(&root_dt, "c08c_mine")
		.await
		.unwrap();
	let other = cache
		.client
		.create_dir(&root_dt, "c08c_other")
		.await
		.unwrap();

	let (outer, inner) = tokio::join!(
		engine.add_pair(local.clone(), mine.uuid(), SyncMode::TwoWay),
		engine.add_pair(nested.clone(), other.uuid(), SyncMode::TwoWay),
	);
	assert!(
		outer.is_ok() != inner.is_ok(),
		"exactly one of two overlapping registrations may win: {outer:?} / {inner:?}"
	);
	let pairs = engine.list_pairs().await.unwrap();
	assert_eq!(
		pairs.len(),
		1,
		"the refused registration must leave nothing behind: {pairs:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// CONTROL-09 — overlapping REMOTE roots are refused: the same remote folder outright, and a
/// nested one whenever the cache can relate the two uuids (see `PairOverlap`'s note on that being
/// best-effort).
#[shared_test_runtime]
async fn control_09_nested_remote_roots_deterministic() {
	let (resources, cache, _root, local_a) = raw_setup("c09a").await;
	let local_b = fresh_local_dir("c09b");
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();

	// The pair's root is a SUBFOLDER, so a later sibling folder is genuinely disjoint from it.
	let root_dt = DirType::<Normal>::Dir(std::borrow::Cow::Borrowed(&resources.dir));
	let docs = cache.client.create_dir(&root_dt, "c09_docs").await.unwrap();
	let remote = docs.uuid();
	let p1 = engine
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	// The SAME remote folder from a different local root: refused without needing the cache.
	let error = engine
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.expect_err("two pairs may not share one remote root")
		.to_string();
	assert!(
		error.contains("already syncs the remote root"),
		"the refusal must name the conflict: {error}"
	);

	// A folder INSIDE the pair's remote root, once the cache has learned about it.
	let nested = cache
		.client
		.create_dir(
			&DirType::<Normal>::Dir(std::borrow::Cow::Owned(docs.clone())),
			"reports",
		)
		.await
		.unwrap();
	assert!(
		poll_for_item(cache.db_path(), nested.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed the nested remote folder"
	);
	let error = engine
		.add_pair(local_b.clone(), nested.uuid(), SyncMode::TwoWay)
		.await
		.expect_err("a remote root inside another pair's root must be refused")
		.to_string();
	assert!(
		error.contains("inside sync pair"),
		"the refusal must name the nesting: {error}"
	);

	// Only ONE pair is registered: every refusal left the registry alone.
	let pairs = engine.list_pairs().await.unwrap();
	assert_eq!(pairs.len(), 1, "{pairs:?}");
	assert_eq!(pairs[0].id, p1);

	// Disjoint roots are still fine — the check rejects overlap, not multi-pair use.
	let sibling = cache
		.client
		.create_dir(&root_dt, "c09_sibling")
		.await
		.unwrap();
	engine
		.add_pair(local_b.clone(), sibling.uuid(), SyncMode::TwoWay)
		.await
		.expect("a disjoint pair is accepted");
	assert_eq!(engine.list_pairs().await.unwrap().len(), 2);

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}
