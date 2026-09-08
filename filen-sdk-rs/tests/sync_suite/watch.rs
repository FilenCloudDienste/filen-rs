//! Continuous (watch) engine tests (`WATCH-*`) for the two-way sync engine.
//!
//! These drive `SyncEngine::watch` / `watch_observed` (the always-on path) and assert observable
//! end-state convergence, byte-exactness, event coalescing (via the public `SyncEvent` stream),
//! loop-freedom (idle quiescence), and prompt clean shutdown (dropping the `WatchHandle`). Black-box:
//! PUBLIC API only. Where a test needs infrastructure the current public API/harness does not expose
//! (fault injection, watcher/notification suppression, a real process restart, or scheduler
//! introspection) it is left as an `#[ignore]`d stub describing the plan.
use std::{
	borrow::Cow,
	sync::{
		Arc, Mutex,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	cache::{CacheMessage, ResyncProgress},
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
		file::RemoteFile,
	},
	sync_engine::{SyncEngine, SyncEvent, SyncMode, WatchConfig, WatchState},
};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ----------------------------------------------------------------------------
// Local remote-setup / verification helpers (PUBLIC API only).
// ----------------------------------------------------------------------------

/// The remote sync-root `DirType` for a single-client setup.
fn sc_root(sc: &SingleClient) -> DirType<'_, Normal> {
	DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir))
}

/// Upload a file with exact bytes directly under the remote root, returning the created file.
async fn upload_remote(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
	let builder = sc
		.cache
		.client
		.make_file_builder(name, sc.resources.dir.uuid())
		.unwrap();
	sc.cache.client.upload_file(builder, data).await.unwrap()
}

/// List the (dirs, files) directly under the remote root (ground truth, via the client).
async fn list_remote_root(sc: &SingleClient) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	sc.cache
		.client
		.list_dir(&sc_root(sc), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
}

fn find_file<'a>(files: &'a [RemoteFile], name: &str) -> Option<&'a RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

fn find_dir<'a>(dirs: &'a [RemoteDirectory], name: &str) -> Option<&'a RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
}

/// How many cache convergence resyncs have STARTED so far. A resync relists a whole root under the
/// drive lock, so this counter is the cost a control operation must not silently incur.
fn resync_starts(log: &MessageLog) -> usize {
	log.lock()
		.unwrap()
		.iter()
		.filter(|m| {
			matches!(
				m,
				CacheMessage::ResyncProgress(ResyncProgress::Started { .. })
			)
		})
		.count()
}

/// Wait until the cache observes `uuid` (the engine's remote view is the cache).
async fn wait_cache_sees(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// A dedicated `Arc<SyncEngine>` (watch requires `Arc<Self>`) for the single-client's local+remote.
/// The harness's own `sc.engine`/`sc.pair` are left unused for watch tests; this opens a fresh
/// baseline DB on the SAME derived cache client so the engine's remote view is the converged cache.
async fn watch_engine(sc: &SingleClient, mode: SyncMode) -> (Arc<SyncEngine>, i64) {
	let engine = Arc::new(
		SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, mode)
		.await
		.unwrap();
	(engine, pair)
}

/// Shared, thread-safe recording of the `SyncEvent` stream a watch observer sees.
#[derive(Default)]
struct WatchLog {
	/// Number of `PassCompleted` events (i.e. completed passes).
	passes: AtomicUsize,
	/// Cumulative reported uploads / downloads across all completed passes.
	uploaded: AtomicUsize,
	downloaded: AtomicUsize,
	remotely_trashed: AtomicUsize,
	locally_deleted: AtomicUsize,
	held_deletions: AtomicUsize,
	/// Every per-action / lifecycle event, in arrival order (for fine-grained assertions).
	events: Mutex<Vec<SyncEvent>>,
}

impl WatchLog {
	fn passes(&self) -> usize {
		self.passes.load(Ordering::SeqCst)
	}
	fn uploaded(&self) -> usize {
		self.uploaded.load(Ordering::SeqCst)
	}
	fn downloaded(&self) -> usize {
		self.downloaded.load(Ordering::SeqCst)
	}
	fn remotely_trashed(&self) -> usize {
		self.remotely_trashed.load(Ordering::SeqCst)
	}
	fn locally_deleted(&self) -> usize {
		self.locally_deleted.load(Ordering::SeqCst)
	}
	fn held_deletions(&self) -> usize {
		self.held_deletions.load(Ordering::SeqCst)
	}
	fn conflicts(&self) -> Vec<String> {
		self.events
			.lock()
			.unwrap()
			.iter()
			.filter_map(|e| match e {
				SyncEvent::Conflict { rel_path } => Some(rel_path.clone()),
				_ => None,
			})
			.collect()
	}
	/// Count of a particular kind of per-action event, by a discriminating predicate.
	fn count(&self, pred: impl Fn(&SyncEvent) -> bool) -> usize {
		self.events
			.lock()
			.unwrap()
			.iter()
			.filter(|e| pred(e))
			.count()
	}
}

/// Build a `SyncObserver` closure that folds the stream into a shared [`WatchLog`].
fn observer_for(log: Arc<WatchLog>) -> Box<dyn FnMut(SyncEvent) + Send + 'static> {
	Box::new(move |event: SyncEvent| {
		if let SyncEvent::PassCompleted { report } = &event {
			log.passes.fetch_add(1, Ordering::SeqCst);
			log.uploaded.fetch_add(report.uploaded, Ordering::SeqCst);
			log.downloaded
				.fetch_add(report.downloaded, Ordering::SeqCst);
			log.remotely_trashed
				.fetch_add(report.remotely_trashed, Ordering::SeqCst);
			log.locally_deleted
				.fetch_add(report.locally_deleted, Ordering::SeqCst);
			log.held_deletions
				.fetch_add(report.held_deletions(), Ordering::SeqCst);
		}
		log.events.lock().unwrap().push(event);
	})
}

/// Poll `predicate` every 200ms up to `timeout`, returning whether it held.
async fn wait_until(timeout: Duration, predicate: impl FnMut() -> bool) -> bool {
	poll_until(timeout, predicate).await
}

/// A generous settle window for the watch to react to one burst (debounce + a pass + cache observe).
const WATCH_SETTLE: Duration = Duration::from_secs(90);
/// A quiet window over which we expect ZERO event-triggered passes (multiple debounce intervals,
/// but well under the periodic safety-net interval).
const IDLE_QUIET: Duration = Duration::from_secs(8);

// ============================================================================
// WATCH-01 — a single local create triggers an automatic pass that uploads it
// ============================================================================

#[shared_test_runtime]
async fn watch_01_local_create_auto_uploads() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Create AFTER watch started; do not invoke a pass manually.
	write_file(&sc.local, "foo.txt", b"hello");

	// The file must appear on the remote (ground truth) purely from the watcher.
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"watch never uploaded the new local file (passes={}, up={})",
		log.passes(),
		log.uploaded()
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	let f = find_file(&files, "foo.txt").expect("foo.txt missing on remote");
	assert_eq!(f.size, b"hello".len() as u64, "remote size mismatch");

	// No conflicts / deletions, and after settling the engine returns to idle.
	assert!(log.conflicts().is_empty(), "unexpected conflicts");
	assert_eq!(log.remotely_trashed(), 0, "unexpected remote trash");
	let passes_after = log.passes();
	tokio::time::sleep(IDLE_QUIET).await;
	assert!(
		log.passes() <= passes_after + 1,
		"engine kept running passes while idle ({} -> {})",
		passes_after,
		log.passes()
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-02 — a single remote change triggers an automatic pass that pulls it
// ============================================================================

#[shared_test_runtime]
async fn watch_02_remote_change_auto_pulls() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::RemoteToLocal).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Out-of-band remote create -> a remote-change notification should reach the watcher.
	let rf = upload_remote(&sc, "bar.txt", b"world").await;
	wait_cache_sees(&sc, rf.uuid()).await;

	assert!(
		wait_until(WATCH_SETTLE, || log.downloaded() >= 1).await,
		"watch never pulled the remote change (passes={}, down={})",
		log.passes(),
		log.downloaded()
	);
	assert!(
		read_eq(&sc.local, "bar.txt", b"world"),
		"local content mismatch after watch pull"
	);
	assert!(log.conflicts().is_empty(), "unexpected conflicts");
	assert_eq!(log.locally_deleted(), 0, "unexpected local delete");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-03 — a burst of local events is debounced/coalesced into a single pass
// ============================================================================

#[shared_test_runtime]
async fn watch_03_local_burst_coalesces() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	const N: usize = 50;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("f{i:03}.txt"),
			format!("c{i}").as_bytes(),
		);
	}

	// All 50 must land remotely, byte-exact.
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= N).await,
		"watch did not upload all {N} files (up={}, passes={})",
		log.uploaded(),
		log.passes()
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), N, "remote file count mismatch");
	for i in 0..N {
		let name = format!("f{i:03}.txt");
		let f = find_file(&files, &name).unwrap_or_else(|| panic!("{name} missing"));
		assert_eq!(f.size, format!("c{i}").len() as u64, "{name} size mismatch");
	}

	// Coalescing: a burst within one debounce window should be a SMALL bounded number of passes,
	// not one-per-event. Allow a couple (initial immediate pass + the burst pass).
	assert!(
		log.passes() <= 4,
		"burst was not coalesced: {} passes for {N} files",
		log.passes()
	);
	assert_eq!(
		log.uploaded(),
		N,
		"duplicate or missing uploads: {}",
		log.uploaded()
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-05 — the engine's own writes do not cause an infinite re-sync loop
// ============================================================================

#[shared_test_runtime]
async fn watch_05_no_self_trigger_loop() {
	let sc = single_client(SyncMode::TwoWay).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::TwoWay).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// A local create (engine uploads it) and a remote create (engine downloads it).
	write_file(&sc.local, "selfwrite.txt", b"local-origin");
	let rf = upload_remote(&sc, "remote_only.txt", b"remote-origin").await;
	wait_cache_sees(&sc, rf.uuid()).await;

	// Wait for both to converge on both sides.
	assert!(
		wait_until(WATCH_SETTLE, || {
			read_eq(&sc.local, "remote_only.txt", b"remote-origin") && log.uploaded() >= 1
		})
		.await,
		"watch did not converge both items (up={}, down={})",
		log.uploaded(),
		log.downloaded()
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "selfwrite.txt").is_some(),
		"selfwrite missing remote"
	);
	assert!(
		find_file(&files, "remote_only.txt").is_some(),
		"remote_only missing remote"
	);
	assert!(
		read_eq(&sc.local, "selfwrite.txt", b"local-origin"),
		"selfwrite mutated locally"
	);

	// After convergence, the engine's OWN writes must not keep triggering passes. Observe a quiet
	// window: pass count must not grow unboundedly (allow at most one trailing self-induced pass).
	let baseline = log.passes();
	tokio::time::sleep(Duration::from_secs(12)).await;
	let grew = log.passes() - baseline;
	assert!(
		grew <= 1,
		"engine looped on its own writes: {grew} extra passes in a quiet window"
	);
	// Nothing re-uploaded / re-downloaded the unchanged content.
	let up_after = log.uploaded();
	let down_after = log.downloaded();
	tokio::time::sleep(IDLE_QUIET).await;
	assert_eq!(
		log.uploaded(),
		up_after,
		"spurious re-upload of unchanged content"
	);
	assert_eq!(
		log.downloaded(),
		down_after,
		"spurious re-download of unchanged content"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-08 — a local rename/move storm coalesces and converges (no lost content)
// ============================================================================

#[shared_test_runtime]
async fn watch_08_rename_storm_converges() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// Pre-seed a converged tree of files under dir/.
	const N: usize = 12;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("dir/f{i:02}.txt"),
			format!("payload-{i}").as_bytes(),
		);
	}
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	// One manual pass to establish the baseline before watching the storm.
	let r0 = engine.sync_once(pair).await.unwrap();
	assert!(r0.errors.is_empty(), "seed pass errors: {r0:?}");
	assert_eq!(r0.uploaded, N, "seed upload count: {r0:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Rename the dir and shuffle some files inside, all within one debounce window.
	move_file(&sc.local, "dir", "dir2");
	for i in 0..(N / 2) {
		move_file(
			&sc.local,
			&format!("dir2/f{i:02}.txt"),
			&format!("dir2/renamed_{i:02}.txt"),
		);
	}

	// Final state: dir2/ with renamed_00..05 and f06..f11, byte-exact, no stale old names.
	assert!(
		wait_until(WATCH_SETTLE, || {
			// Verify via the LOCAL tree shape (final names) — the remote is asserted below.
			let mut ok = true;
			for i in 0..(N / 2) {
				ok &= sc.local.join(format!("dir2/renamed_{i:02}.txt")).is_file();
			}
			for i in (N / 2)..N {
				ok &= sc.local.join(format!("dir2/f{i:02}.txt")).is_file();
			}
			ok && log.passes() >= 1
		})
		.await,
		"local rename storm did not settle (passes={})",
		log.passes()
	);

	// Remote must reflect the final names exactly, with no content loss and no stale old names.
	// Allow a few extra reconciling passes for the cache to observe the moves.
	let mut converged = false;
	let deadline = std::time::Instant::now() + WATCH_SETTLE;
	let mut final_count = 0usize;
	while std::time::Instant::now() < deadline {
		let (dirs, root_files) = list_remote_root(&sc).await;
		if let Some(dir2) = find_dir(&dirs, "dir2") {
			let (_sd, files) = sc
				.cache
				.client
				.list_dir(
					&DirType::<Normal>::Dir(Cow::Borrowed(dir2)),
					None::<&fn(u64, Option<u64>)>,
				)
				.await
				.unwrap();
			final_count = files.len();
			let renamed_ok =
				(0..(N / 2)).all(|i| find_file(&files, &format!("renamed_{i:02}.txt")).is_some());
			let kept_ok =
				((N / 2)..N).all(|i| find_file(&files, &format!("f{i:02}.txt")).is_some());
			let no_old_dir = find_dir(&dirs, "dir").is_none();
			let no_stray_root = root_files.is_empty();
			if renamed_ok && kept_ok && no_old_dir && no_stray_root {
				converged = true;
				break;
			}
		}
		tokio::time::sleep(Duration::from_millis(1000)).await;
	}
	assert!(
		converged,
		"remote did not converge to the renamed tree (dir2 had {final_count} files)"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-09 — rapid create-then-delete of the same file nets to a no-op
// ============================================================================

#[shared_test_runtime]
async fn watch_09_create_then_delete_is_noop() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Create then immediately delete within (well within) one debounce window.
	write_file(&sc.local, "temp.txt", b"ephemeral");
	std::fs::remove_file(sc.local.join("temp.txt")).unwrap();

	// Give the watch ample time to run its coalesced pass(es).
	tokio::time::sleep(Duration::from_secs(20)).await;

	// temp.txt must never have been created remotely, and nothing trashed.
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "temp.txt").is_none(),
		"a since-deleted file was phantom-uploaded to the remote"
	);
	assert_eq!(
		log.uploaded(),
		0,
		"phantom upload occurred: {}",
		log.uploaded()
	);
	assert_eq!(log.remotely_trashed(), 0, "phantom trash occurred");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-10 — rapid successive edits coalesce; only the final content is synced
// ============================================================================

#[shared_test_runtime]
async fn watch_10_rapid_edits_only_final() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// Seed edit.txt = "v0" and converge it before watching.
	write_file(&sc.local, "edit.txt", b"v0");
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.uploaded, 1, "seed pass: {r0:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Three quick overwrites within one debounce window.
	write_file(&sc.local, "edit.txt", b"v1");
	write_file(&sc.local, "edit.txt", b"v2");
	write_file(&sc.local, "edit.txt", b"v3-final");

	assert!(
		wait_until(WATCH_SETTLE, || { log.uploaded() >= 1 }).await,
		"watch never propagated the edit (passes={})",
		log.passes()
	);
	// Remote must hold exactly the FINAL content.
	let mut ok = false;
	let deadline = std::time::Instant::now() + WATCH_SETTLE;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_remote_root(&sc).await;
		if find_file(&files, "edit.txt").map(|f| f.size) == Some(b"v3-final".len() as u64) {
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(ok, "remote did not converge to the final edit content");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-16 — concurrent independent watch pairs do not cross-trigger
// ============================================================================

#[shared_test_runtime]
async fn watch_16_independent_pairs_no_cross_trigger() {
	// Two clients on DIFFERENT remote dirs would be ideal; the harness's TwoClients shares one
	// remote. Instead use two separate single-client setups (distinct remote dirs + local dirs).
	let sc_a = single_client(SyncMode::LocalToRemote).await;
	let sc_b = single_client(SyncMode::LocalToRemote).await;
	let (engine_a, pair_a) = watch_engine(&sc_a, SyncMode::LocalToRemote).await;
	let (engine_b, pair_b) = watch_engine(&sc_b, SyncMode::LocalToRemote).await;
	let log_a = Arc::new(WatchLog::default());
	let log_b = Arc::new(WatchLog::default());
	let handle_a = engine_a
		.clone()
		.watch_observed(pair_a, observer_for(log_a.clone()))
		.await
		.unwrap();
	let handle_b = engine_b
		.clone()
		.watch_observed(pair_b, observer_for(log_b.clone()))
		.await
		.unwrap();

	// Let both reach their initial idle state.
	tokio::time::sleep(Duration::from_secs(4)).await;
	let b_passes_before = log_b.passes();

	// Change only in A's local tree.
	write_file(&sc_a.local, "only_a.txt", b"a-only");

	assert!(
		wait_until(WATCH_SETTLE, || log_a.uploaded() >= 1).await,
		"pair A did not upload its own file (passes={})",
		log_a.passes()
	);
	let (_da, files_a) = list_remote_root(&sc_a).await;
	assert!(
		find_file(&files_a, "only_a.txt").is_some(),
		"only_a missing on remoteA"
	);

	// remoteB must be unchanged; pair B must not have uploaded anything as a result of A's event.
	let (_db, files_b) = list_remote_root(&sc_b).await;
	assert!(files_b.is_empty(), "remoteB was mutated by pair A's event");
	assert_eq!(log_b.uploaded(), 0, "pair B uploaded due to pair A's event");
	// B may have run its own scheduled/idle pass, but with zero actions.
	assert!(
		log_b.passes() >= b_passes_before,
		"pair B pass count went backwards (impossible)"
	);

	drop(handle_a);
	drop(handle_b);
	sc_a.cleanup();
	sc_b.cleanup();
}

// ============================================================================
// WATCH-17 — debounced pass picks up the LATEST state, not the first-event state
// ============================================================================

#[shared_test_runtime]
async fn watch_17_debounce_uses_latest_state() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// First event starts the debounce; overwrite before it fires.
	write_file(&sc.local, "big.txt", b"A");
	write_file(&sc.local, "big.txt", b"BB-final-state");

	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"watch never uploaded big.txt (passes={})",
		log.passes()
	);
	let mut ok = false;
	let deadline = std::time::Instant::now() + WATCH_SETTLE;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_remote_root(&sc).await;
		if find_file(&files, "big.txt").map(|f| f.size) == Some(b"BB-final-state".len() as u64) {
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		ok,
		"remote did not hold the value at pass time (latest state)"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-18 — watch surfaces a true two-way conflict (near-simultaneous edits)
// ============================================================================

#[ignore = "blocked: surfacing a two-way conflict under watch requires the local FS-watcher edit and \
the remote cache edit to be observed within the SAME reconcile pass; their relative timing is \
nondeterministic live (a debounced pass may push/pull one side before the other is seen, dissolving \
the conflict), so this needs a single-step/pause control-plane or fault-injection seam to be \
deterministic. TODO"]
#[shared_test_runtime]
async fn watch_18_two_way_conflict_surfaced() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Establish a converged baseline for doc.txt = "base" on both sides.
	write_file(&sc.local, "doc.txt", b"base");
	let (engine, pair) = watch_engine(&sc, SyncMode::TwoWay).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.uploaded, 1, "seed: {r0:?}");
	// A second pass settles the baseline so a later both-sides edit is a genuine conflict.
	let r1 = engine.sync_once(pair).await.unwrap();
	assert!(r1.conflicts.is_empty(), "baseline pass conflicted: {r1:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Diverge BOTH sides within the same window.
	write_file(&sc.local, "doc.txt", b"local-edit");
	let rf = upload_remote(&sc, "doc.txt", b"remote-edit-different").await;
	wait_cache_sees(&sc, rf.uuid()).await;

	// The watch must eventually surface a conflict for doc.txt, non-destructively.
	assert!(
		wait_until(WATCH_SETTLE, || log
			.conflicts()
			.iter()
			.any(|c| c.contains("doc.txt")))
		.await,
		"watch never surfaced the doc.txt conflict (passes={}, conflicts={:?})",
		log.passes(),
		log.conflicts()
	);
	// Local side untouched (conflict = left alone).
	assert!(
		read_eq(&sc.local, "doc.txt", b"local-edit"),
		"local content was overwritten on conflict"
	);
	// The watch loop keeps running after the conflict (it can still run more passes).
	let before = log.passes();
	write_file(&sc.local, "after_conflict.txt", b"liveness");
	assert!(
		wait_until(WATCH_SETTLE, || log.passes() > before).await,
		"watch loop stopped running after surfacing a conflict"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-19 — mass-deletion via watch is held for confirmation, not auto-applied
// ============================================================================

#[shared_test_runtime]
async fn watch_19_mass_delete_held() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const TOTAL: usize = 24;
	const DELETE: usize = 24;
	for i in 0..TOTAL {
		write_file(
			&sc.local,
			&format!("m{i:02}.txt"),
			format!("c{i}").as_bytes(),
		);
	}
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.uploaded, TOTAL, "seed: {r0:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Delete a large fraction at once.
	for i in 0..DELETE {
		std::fs::remove_file(sc.local.join(format!("m{i:02}.txt"))).unwrap();
	}

	// Wait for a pass to run; the guard must HOLD the deletions (none mirrored).
	assert!(
		wait_until(WATCH_SETTLE, || log.passes() >= 1).await,
		"watch never reacted to the mass delete"
	);
	// Give it a moment to ensure the deleting pass has completed.
	tokio::time::sleep(Duration::from_secs(8)).await;
	assert!(
		log.held_deletions() > 0 || log.count(|e| matches!(e, SyncEvent::DeletionsHeld { .. })) > 0,
		"mass-delete guard did not engage (held={})",
		log.held_deletions()
	);
	assert_eq!(
		log.remotely_trashed(),
		0,
		"guard let the destructive mass-trash through"
	);
	// Remote still has all files.
	let (_d, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), TOTAL, "guard failed to keep remote files");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-20 — remote-origin deletion quarantines the local copy (recoverable)
// ============================================================================

#[shared_test_runtime]
async fn watch_20_remote_delete_quarantines_local() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let mut keep = upload_remote(&sc, "keep.txt", b"precious-bytes").await;
	wait_cache_sees(&sc, keep.uuid()).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::RemoteToLocal).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.downloaded, 1, "seed pull: {r0:?}");
	assert!(read_eq(&sc.local, "keep.txt", b"precious-bytes"));

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Trash on the remote -> notification fires -> mirror the deletion locally.
	sc.cache.client.trash_file(&mut keep).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), keep.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped the trashed file"
	);

	assert!(
		wait_until(WATCH_SETTLE, || log.locally_deleted() >= 1).await,
		"watch never mirrored the remote deletion (passes={})",
		log.passes()
	);
	// The working-tree copy is gone ...
	assert!(
		!sc.local.join("keep.txt").exists(),
		"local keep.txt still in the working tree"
	);
	// ... but its content survives in the quarantine bin (byte-exact, recoverable). The bin is
	// `.filen-sync-trash` at the sync-root top level (walk_tree ignores it; we look inside it here).
	let bin = sc.local.join(".filen-sync-trash");
	let mut recovered = false;
	if bin.is_dir() {
		let mut stack = vec![bin.clone()];
		while let Some(d) = stack.pop() {
			if let Ok(rd) = std::fs::read_dir(&d) {
				for e in rd.flatten() {
					let p = e.path();
					if p.is_dir() {
						stack.push(p);
					} else if std::fs::read(&p).ok().as_deref() == Some(b"precious-bytes") {
						recovered = true;
					}
				}
			}
		}
	}
	assert!(
		recovered,
		"deleted local content was not preserved in the recoverable quarantine"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-21 — first watch start against a populated destination does not wipe it
// ============================================================================

#[shared_test_runtime]
async fn watch_21_first_start_no_wipe() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Both sides already populated with distinct files BEFORE any baseline exists.
	write_file(&sc.local, "L1.txt", b"local-side");
	let rf = upload_remote(&sc, "R1.txt", b"remote-side").await;
	wait_cache_sees(&sc, rf.uuid()).await;

	let (engine, pair) = watch_engine(&sc, SyncMode::TwoWay).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// After the first run converges, BOTH files exist on BOTH sides (nothing wiped). The pass
	// applies its transfers concurrently, so waiting on the local side alone would let the
	// remote assertions below race the still-in-flight upload: wait for the completed pass to
	// report both directions.
	assert!(
		wait_until(WATCH_SETTLE, || {
			read_eq(&sc.local, "R1.txt", b"remote-side")
				&& sc.local.join("L1.txt").is_file()
				&& log.uploaded() >= 1
				&& log.downloaded() >= 1
		})
		.await,
		"first watch run did not converge both files (passes={}, up={}, down={})",
		log.passes(),
		log.uploaded(),
		log.downloaded()
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "L1.txt").is_some(),
		"L1 missing on remote (wiped?)"
	);
	assert!(
		find_file(&files, "R1.txt").is_some(),
		"R1 missing on remote (wiped?)"
	);
	assert!(
		read_eq(&sc.local, "L1.txt", b"local-side"),
		"L1 lost locally"
	);
	assert!(
		read_eq(&sc.local, "R1.txt", b"remote-side"),
		"R1 not pulled"
	);
	// No destructive first-run artifact.
	assert_eq!(log.remotely_trashed(), 0, "first run trashed remote data");
	assert_eq!(log.locally_deleted(), 0, "first run deleted local data");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-23 — local-backup watch does NOT mirror a local deletion to the remote
// ============================================================================

#[shared_test_runtime]
async fn watch_23_local_backup_keeps_remote_on_delete() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "archive.txt", b"backed-up");
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalBackup).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.uploaded, 1, "seed: {r0:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Delete locally; LocalBackup must NOT mirror the deletion.
	std::fs::remove_file(sc.local.join("archive.txt")).unwrap();
	// Give the watch a window to (not) react destructively, then create a new file to prove liveness.
	tokio::time::sleep(Duration::from_secs(12)).await;
	write_file(&sc.local, "new_after.txt", b"still-backing-up");

	// Liveness: the new file must be pushed (poll the remote ground truth, async).
	let mut pushed = false;
	let deadline = std::time::Instant::now() + WATCH_SETTLE;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_remote_root(&sc).await;
		if find_file(&files, "new_after.txt").is_some() {
			pushed = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(pushed, "backup did not push the new file (liveness check)");

	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "archive.txt").is_some(),
		"LocalBackup wrongly mirrored the local delete to the remote"
	);
	assert!(
		find_file(&files, "new_after.txt").is_some(),
		"new file not backed up"
	);
	assert_eq!(log.remotely_trashed(), 0, "backup trashed remote data");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-24 — remote-backup watch does NOT mirror a remote deletion locally
// ============================================================================

#[shared_test_runtime]
async fn watch_24_remote_backup_keeps_local_on_delete() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let mut cloud = upload_remote(&sc, "cloud.txt", b"cloud-data").await;
	wait_cache_sees(&sc, cloud.uuid()).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::RemoteBackup).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.downloaded, 1, "seed pull: {r0:?}");
	assert!(read_eq(&sc.local, "cloud.txt", b"cloud-data"));

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Delete on the remote -> notification fires. RemoteBackup must NOT mirror it locally.
	sc.cache.client.trash_file(&mut cloud).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), cloud.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped the trashed remote file"
	);

	// Prove the watcher saw the remote change by pushing a NEW remote file the backup should pull.
	let rf2 = upload_remote(&sc, "fresh.txt", b"fresh-remote").await;
	wait_cache_sees(&sc, rf2.uuid()).await;
	assert!(
		wait_until(WATCH_SETTLE, || read_eq(
			&sc.local,
			"fresh.txt",
			b"fresh-remote"
		))
		.await,
		"remote-backup never pulled the genuine new remote file (passes={})",
		log.passes()
	);

	// The local copy of the deleted file must remain, byte-exact, and not be quarantined.
	assert!(
		read_eq(&sc.local, "cloud.txt", b"cloud-data"),
		"RemoteBackup wrongly mirrored the remote delete to the local side"
	);
	assert_eq!(log.locally_deleted(), 0, "backup deleted local data");
	let bin = sc.local.join(".filen-sync-trash");
	assert!(
		!bin.join("cloud.txt").exists(),
		"backup quarantined the local file it was meant to retain"
	);

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// WATCH-13 — stopping the watch (dropping the handle) halts further activity
// ============================================================================

#[shared_test_runtime]
async fn watch_13_drop_handle_stops_activity() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Confirm liveness first.
	write_file(&sc.local, "live.txt", b"alive");
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"watch never confirmed liveness"
	);

	// Stop: drop the handle (the documented stop mechanism). Should return promptly.
	let t = std::time::Instant::now();
	drop(handle);
	assert!(
		t.elapsed() < Duration::from_secs(10),
		"dropping the watch handle hung"
	);

	// Let any in-flight pass drain, then snapshot the pass count.
	tokio::time::sleep(Duration::from_secs(3)).await;
	let passes_at_stop = log.passes();
	let uploaded_at_stop = log.uploaded();

	// Post-stop writes must NOT be synced.
	write_file(&sc.local, "after_stop.txt", b"should-not-upload");
	tokio::time::sleep(Duration::from_secs(12)).await;
	assert_eq!(
		log.passes(),
		passes_at_stop,
		"a pass ran after the watch was stopped"
	);
	assert_eq!(
		log.uploaded(),
		uploaded_at_stop,
		"an upload happened after the watch was stopped"
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "after_stop.txt").is_none(),
		"post-stop file was uploaded"
	);

	sc.cleanup();
}

// ============================================================================
// (add) — stop() waits for the loop, and a healthy watch reports healthy status
// ============================================================================

#[shared_test_runtime]
async fn watch_stop_awaits_loop_and_reports_health() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();
	let mut status = handle.status();

	write_file(&sc.local, "stopme.txt", b"bye");
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"watch never uploaded the new local file (passes={})",
		log.passes()
	);

	// A watch whose passes are succeeding reports no failure.
	let health = status.borrow_and_update().clone();
	assert_eq!(
		health.consecutive_failures, 0,
		"healthy watch reported failures: {health:?}"
	);
	assert!(
		health.last_error.is_none(),
		"healthy watch reported an error: {health:?}"
	);

	// stop() awaits the loop, so NO pass may complete after it returns — no drain sleep needed.
	handle.stop().await;
	let passes_at_stop = log.passes();
	write_file(&sc.local, "after_stop.txt", b"should-not-upload");
	tokio::time::sleep(Duration::from_secs(12)).await;
	assert_eq!(
		log.passes(),
		passes_at_stop,
		"a pass ran after stop() returned"
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "after_stop.txt").is_none(),
		"post-stop file was uploaded"
	);

	sc.cleanup();
}

// ============================================================================
// (add) — engine reaches and stays at idle when nothing changes after convergence
// ============================================================================

#[shared_test_runtime]
async fn watch_idle_quiescence_after_convergence() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Seed and converge a small non-empty tree before watching.
	const N: usize = 6;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("q{i}.txt"),
			format!("content-{i}").as_bytes(),
		);
	}
	let (engine, pair) = watch_engine(&sc, SyncMode::TwoWay).await;
	let r0 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r0.uploaded, N, "seed: {r0:?}");
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 0, "second pass not idle: {r1:?}");

	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Let the initial immediate watch pass run, then snapshot and stay completely idle.
	tokio::time::sleep(Duration::from_secs(5)).await;
	let passes_baseline = log.passes();
	let up = log.uploaded();
	let down = log.downloaded();

	tokio::time::sleep(IDLE_QUIET).await;

	// Over a quiet window (multiple debounce intervals, under the net interval) there must be no
	// event-triggered work: allow at most one trailing pass, and it must have planned zero actions.
	assert!(
		log.passes() <= passes_baseline + 1,
		"engine ran event-triggered passes while fully idle ({} -> {})",
		passes_baseline,
		log.passes()
	);
	assert_eq!(log.uploaded(), up, "spurious upload while idle");
	assert_eq!(log.downloaded(), down, "spurious download while idle");
	assert_eq!(
		log.remotely_trashed(),
		0,
		"spurious remote trash while idle"
	);
	assert_eq!(log.locally_deleted(), 0, "spurious local delete while idle");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// (add) — per-action progress events are emitted live during watch passes
// ============================================================================

#[shared_test_runtime]
async fn watch_per_action_events_emitted() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_observed(pair, observer_for(log.clone()))
		.await
		.unwrap();

	// Drive a few uploads via the watch (event-triggered) path.
	write_file(&sc.local, "p0.txt", b"zero");
	write_file(&sc.local, "p1.txt", b"one");
	write_file(&sc.local, "sub/p2.txt", b"two");

	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 3).await,
		"watch did not upload all three files (up={})",
		log.uploaded()
	);

	// Per-action Uploading events must have fired live (not just a final report).
	let uploading = log.count(|e| matches!(e, SyncEvent::Uploading { .. }));
	assert!(
		uploading >= 3,
		"expected >=3 live Uploading events, saw {uploading}"
	);
	// A per-pass PassStarted / PassCompleted bracket must have been delivered too.
	assert!(
		log.count(|e| matches!(e, SyncEvent::PassStarted { .. })) >= 1,
		"no PassStarted event"
	);
	assert!(log.passes() >= 1, "no PassCompleted event");
	// The CreatingRemoteDir for `sub/` should also appear.
	assert!(
		log.count(|e| matches!(e, SyncEvent::CreatingRemoteDir { .. })) >= 1,
		"no CreatingRemoteDir event for sub/"
	);

	// After stop, the event stream stops growing.
	drop(handle);
	tokio::time::sleep(Duration::from_secs(3)).await;
	let frozen = log.events.lock().unwrap().len();
	tokio::time::sleep(Duration::from_secs(6)).await;
	assert_eq!(
		log.events.lock().unwrap().len(),
		frozen,
		"events kept arriving after the watch was stopped"
	);

	sc.cleanup();
}

// ============================================================================
// (add) — adding a new pair to an already-running watch engine starts watching it
// ============================================================================

#[shared_test_runtime]
async fn watch_add_pair_to_live_engine() {
	// Use two separate single-client setups so each pair has its OWN local+remote dir, but drive
	// both through ONE Arc<SyncEngine> (the live engine that gains a pair at runtime).
	let sc_a = single_client(SyncMode::LocalToRemote).await;
	let sc_b = single_client(SyncMode::LocalToRemote).await;

	let engine = Arc::new(
		SyncEngine::open(sc_a.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair_a = engine
		.add_pair(sc_a.local.clone(), sc_a.remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let log_a = Arc::new(WatchLog::default());
	let handle_a = engine
		.clone()
		.watch_observed(pair_a, observer_for(log_a.clone()))
		.await
		.unwrap();
	// Let A idle.
	tokio::time::sleep(Duration::from_secs(3)).await;
	let a_passes_before = log_a.passes();

	// Add pair B to the already-running engine, then start watching it (no engine restart).
	let pair_b = engine
		.add_pair(sc_b.local.clone(), sc_b.remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let log_b = Arc::new(WatchLog::default());
	let handle_b = engine
		.clone()
		.watch_observed(pair_b, observer_for(log_b.clone()))
		.await
		.unwrap();

	// A change in B's local tree must converge to remoteB.
	write_file(&sc_b.local, "b_new.txt", b"b-content");
	assert!(
		wait_until(WATCH_SETTLE, || log_b.uploaded() >= 1).await,
		"newly-added pair B did not sync (passes={})",
		log_b.passes()
	);
	let (_db, files_b) = list_remote_root(&sc_b).await;
	assert!(
		find_file(&files_b, "b_new.txt").is_some(),
		"b_new missing on remoteB"
	);

	// Pair A is undisturbed: remoteA empty, no extra uploads attributable to B.
	let (_da, files_a) = list_remote_root(&sc_a).await;
	assert!(files_a.is_empty(), "remoteA was disturbed by adding pair B");
	assert_eq!(log_a.uploaded(), 0, "pair A uploaded due to pair B");
	assert!(
		log_a.passes() >= a_passes_before,
		"pair A pass count regressed"
	);

	drop(handle_a);
	drop(handle_b);
	sc_a.cleanup();
	sc_b.cleanup();
}

// ============================================================================
// WATCH-15 — pause suspends the loop's passes; resume takes the backlog in one go
// ============================================================================

/// While paused the loop runs nothing at all, and neither side moves. Resuming applies everything
/// that piled up, exactly once each — and costs NO cache resync: the pair's sync-root registration
/// is kept across the pause, so the root is never untracked and relisted under the drive lock.
#[shared_test_runtime]
async fn watch_15_pause_resume() {
	const NET: Duration = Duration::from_secs(3);
	const LOCAL: usize = 5;

	let sc = single_client(SyncMode::TwoWay).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::TwoWay).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				safety_net: NET,
			},
			observer_for(log.clone()),
		)
		.await
		.unwrap();

	assert!(
		wait_until(WATCH_SETTLE, || log.passes() >= 1).await,
		"the watch never ran its initial pass"
	);
	engine.pause_pair(pair).await.unwrap();
	// A paused pair cannot be watched at all: resuming first is the supported order.
	assert!(
		engine.clone().watch(pair).await.is_err(),
		"a paused pair must not be watchable"
	);
	// Let a pass that was already in flight when the pause landed finish.
	tokio::time::sleep(NET).await;
	let passes_at_pause = log.passes();
	let resyncs_at_pause = resync_starts(&sc.cache.messages);

	// Five local creates and two remote ones, all while paused.
	for i in 0..LOCAL {
		write_file(&sc.local, &format!("p{i}.txt"), format!("l{i}").as_bytes());
	}
	let r0 = upload_remote(&sc, "rp0.txt", b"r0").await;
	let r1 = upload_remote(&sc, "rp1.txt", b"r1").await;
	wait_cache_sees(&sc, r0.uuid()).await;
	wait_cache_sees(&sc, r1.uuid()).await;
	tokio::time::sleep(NET * 3).await;

	assert_eq!(
		log.passes(),
		passes_at_pause,
		"the paused watch ran a pass anyway"
	);
	assert_eq!(log.uploaded(), 0, "the paused watch uploaded");
	assert_eq!(log.downloaded(), 0, "the paused watch downloaded");

	engine.resume_pair(pair).await.unwrap();
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= LOCAL
			&& log.downloaded() >= 2)
		.await,
		"the backlog did not sync after resume (up={}, down={}, passes={})",
		log.uploaded(),
		log.downloaded(),
		log.passes()
	);

	// Nothing lost, nothing duplicated, both sides byte-exact.
	assert_eq!(log.uploaded(), LOCAL, "duplicate uploads after resume");
	assert_eq!(log.downloaded(), 2, "duplicate downloads after resume");
	assert!(log.conflicts().is_empty(), "unexpected conflicts");
	assert_eq!(log.remotely_trashed(), 0, "unexpected remote trash");
	assert_eq!(log.locally_deleted(), 0, "unexpected local delete");
	assert!(
		read_eq(&sc.local, "rp0.txt", b"r0") && read_eq(&sc.local, "rp1.txt", b"r1"),
		"the remote adds did not land locally"
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	for i in 0..LOCAL {
		let name = format!("p{i}.txt");
		assert!(
			find_file(&files, &name).is_some(),
			"{name} did not reach the remote"
		);
	}

	// The load-bearing half: resuming did not cost a relist of the sync root.
	assert_eq!(
		resync_starts(&sc.cache.messages),
		resyncs_at_pause,
		"pausing/resuming triggered a cache resync of the sync root"
	);

	handle.stop().await;
	sc.cleanup();
}

// ============================================================================
// (add) — the debounce is TRAILING-EDGE: every event restarts the quiet window
// ============================================================================

/// Events spaced INSIDE the debounce window must produce no pass at all until the stream stops: a
/// fixed-window debounce would fire several times mid-stream, a trailing-edge one not once. Pinned
/// with an explicit [`WatchConfig`], so the window is a known quantity rather than the production
/// 800 ms nothing can be timed against.
#[shared_test_runtime]
async fn watch_add_trailing_edge_debounce() {
	const DEBOUNCE: Duration = Duration::from_secs(10);
	const N: usize = 6;

	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: DEBOUNCE,
				// Far enough out that nothing observed here can be a safety-net pass.
				safety_net: Duration::from_secs(3600),
			},
			observer_for(log.clone()),
		)
		.await
		.unwrap();

	// Settle first: the loop's immediate initial pass, plus any trigger the registrations
	// themselves produced, must be spent before the stream's pass count means anything.
	assert!(
		wait_until(WATCH_SETTLE, || log.passes() >= 1).await,
		"the watch never ran its initial pass"
	);
	tokio::time::sleep(DEBOUNCE + Duration::from_secs(5)).await;
	let before = log.passes();

	// Write at half the debounce apart, so every event lands inside its predecessor's window.
	for i in 0..N {
		write_file(&sc.local, &format!("t{i}.txt"), format!("v{i}").as_bytes());
		tokio::time::sleep(DEBOUNCE / 2).await;
	}

	// Half a window past the last write: a fixed-window debounce would have fired N/2 times by now.
	assert_eq!(
		log.passes(),
		before,
		"a pass ran mid-stream: the debounce is not trailing-edge"
	);
	assert_eq!(
		log.uploaded(),
		0,
		"files uploaded before the stream went quiet"
	);

	// Once quiet, one pass takes the whole stream — and takes it exactly once.
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= N).await,
		"the coalesced pass never uploaded the stream (passes={}, up={})",
		log.passes(),
		log.uploaded()
	);
	assert_eq!(log.uploaded(), N, "the stream was uploaded more than once");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), N, "remote file count mismatch");
	for i in 0..N {
		let name = format!("t{i}.txt");
		let f = find_file(&files, &name).unwrap_or_else(|| panic!("{name} missing on the remote"));
		assert_eq!(f.size, format!("v{i}").len() as u64, "{name} size mismatch");
	}

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// (add) — the periodic safety-net pass never re-applies an event pass's work
// ============================================================================

/// With a safety net short enough that several ticks land on top of one just-applied change, that
/// change must still be applied exactly once — and the net pass must actually be firing, which is
/// what makes the "exactly once" meaningful. Both halves need a configured interval: the production
/// 300 s one is unobservable inside a test.
#[shared_test_runtime]
async fn watch_add_net_vs_event_pass_no_double_apply() {
	const NET: Duration = Duration::from_secs(3);

	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				safety_net: NET,
			},
			observer_for(log.clone()),
		)
		.await
		.unwrap();

	write_file(&sc.local, "once.txt", b"exactly once");
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"the watch never uploaded the change (passes={}, up={})",
		log.passes(),
		log.uploaded()
	);

	// Sit through several net ticks with nothing changing on either side.
	let before = log.passes();
	tokio::time::sleep(NET * 5).await;
	assert!(
		log.passes() >= before + 2,
		"the configured safety net did not drive passes ({before} -> {})",
		log.passes()
	);
	assert_eq!(
		log.uploaded(),
		1,
		"the change was applied again by a safety-net pass"
	);
	assert_eq!(
		log.downloaded(),
		0,
		"a safety-net pass pulled its own upload"
	);
	assert!(log.conflicts().is_empty(), "unexpected conflicts");
	assert_eq!(log.remotely_trashed(), 0, "unexpected remote trash");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 1, "remote file count mismatch");
	let f = find_file(&files, "once.txt").expect("once.txt missing on remote");
	assert_eq!(f.size, b"exactly once".len() as u64, "remote size mismatch");

	drop(handle);
	sc.cleanup();
}

// ============================================================================
// BLOCKED — need infrastructure the current public API / harness does not expose.
// Each stub documents its plan so it can be implemented once the seam exists.
// ============================================================================

// WATCH-04 — events arriving during an in-flight pass schedule exactly one follow-up pass.
// Needs deterministic control over an "in-flight" pass (a slow/large upload whose duration we can
// pin) AND exact pass-count bounding; the real network makes the timing non-deterministic and the
// public API exposes no way to gate a pass mid-flight. Plan: start a large upload, create a second
// file mid-pass, assert both converge and total passes <= 2.
#[ignore = "blocked: needs deterministic mid-pass timing control (fault/clock injection) — see TODO"]
#[shared_test_runtime]
async fn watch_04_events_during_in_flight_pass() {}

// WATCH-06 — periodic safety-net pass catches a local change the watcher missed/dropped.
// The net interval is now configurable (`WatchConfig`); what is still missing is a way to
// suppress/bypass the FS watcher for one change. The only paths the watcher deliberately ignores
// (`*.filendl`, the quarantine bin) are the same ones the SCAN ignores, so they cannot stand in for
// a change the watcher missed. Plan: create a file the watcher does not see, wait one net interval,
// assert it uploaded with no manual pass.
#[ignore = "blocked: needs watcher suppression for one local change — see TODO"]
#[shared_test_runtime]
async fn watch_06_net_pass_catches_missed_local() {}

// WATCH-07 — periodic safety-net pass catches a remote change with no notification delivered.
// Same shape as WATCH-06: the net interval is configurable now, but there is no way to make a
// remote change WITHOUT the cache subscription pinging the loop, so a pass caused by the net tick
// cannot be told apart from one caused by the notification. Plan: mutate remote out-of-band without
// a notification, wait one net interval, assert local converged with no conflict.
#[ignore = "blocked: needs remote change-notification suppression — see TODO"]
#[shared_test_runtime]
async fn watch_07_net_pass_catches_missed_remote() {}

// WATCH-11 — watch survives a transient apply error and recovers on a later pass.
// Needs fault injection (a one-shot transient transport/upload error). The public API offers no
// error-injection seam. Plan: inject one upload failure, assert the watch loop survives and the
// file uploads on a later pass after the fault clears.
#[ignore = "blocked: needs fault-injection harness (one-shot transient transport error) — see TODO"]
#[shared_test_runtime]
async fn watch_11_survives_transient_apply_error() {}

// WATCH-12 — watch survives a transient watcher/notification subscription error.
// Needs the ability to forcibly drop/break the FS watcher or cache subscription mid-watch. Not
// exposed. Plan: break the watcher once, make local+remote changes during the outage, assert both
// converge (via the net pass) and the engine did not abort.
#[ignore = "blocked: needs watcher/subscription fault-injection — see TODO"]
#[shared_test_runtime]
async fn watch_12_survives_subscription_error() {}

// WATCH-14 — stopping mid-pass does not corrupt state; restart re-plans cleanly.
// Needs deterministic "stop while actively applying" timing (a controllable mid-pass barrier) to
// reliably interrupt a real network pass at the right moment. The handle drop is async-cooperative
// and the moment of interruption is not controllable from the public API. Plan: queue 100 files,
// drop the handle mid-apply, restart watch, assert convergence with no double-upload / data loss.
#[ignore = "blocked: needs deterministic mid-pass interruption control — see TODO"]
#[shared_test_runtime]
async fn watch_14_stop_mid_pass_restart_replans() {}

// WATCH-22 — baseline persists across process restart; watch resumes without re-uploading.
// The current single-process test harness cannot terminate and relaunch a real OS process; faithful
// verification of on-disk baseline persistence across a true restart needs a process-restart seam
// (or baseline-store inspection, which is internal). A same-process re-open reuses live in-memory
// state and would not prove disk persistence. Plan: sync 20 files, kill process, relaunch, assert
// the first net pass does 0 transfers, then a single edit re-syncs only that file.
#[ignore = "blocked: needs real process restart (or baseline-store inspection) — see TODO"]
#[shared_test_runtime]
async fn watch_22_baseline_persists_across_restart() {}

// WATCH-25 — sustained high-frequency event stream stays bounded (no pass pile-up).
// Asserting "at most one active + one queued pass" and "no unbounded pending growth" requires
// introspection into the engine's internal scheduler/queue depth, which is not exposed. The
// black-box surface (PassCompleted counts) cannot distinguish a bounded queue from an unbounded one
// during the storm. Plan: touch files for 30s above the debounce rate, assert bounded concurrency
// and eventual byte-exact convergence after the storm stops.
#[ignore = "blocked: needs scheduler/queue-depth introspection for the boundedness claim — see TODO"]
#[shared_test_runtime]
async fn watch_25_sustained_load_bounded() {}

// ============================================================================
// WATCH-26 — removing a pair stops its watch, and takes nothing with it
// ============================================================================

/// A watch whose pair is removed underneath it must STOP — not tick on into the failure backoff
/// with every pass failing against a pair that no longer exists. The stop is observable on the
/// handle's status, the handle is still fine to stop afterwards, and neither side lost anything:
/// removal is a registry operation.
#[shared_test_runtime]
async fn watch_26_remove_pair_stops_events() {
	const NET: Duration = Duration::from_secs(3);

	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				safety_net: NET,
			},
			observer_for(log.clone()),
		)
		.await
		.unwrap();
	let status = handle.status();

	// Liveness first: the watch is really syncing before its pair goes away.
	write_file(&sc.local, "kept.txt", b"before the removal");
	assert!(
		wait_until(WATCH_SETTLE, || log.uploaded() >= 1).await,
		"the watch never uploaded the file (passes={})",
		log.passes()
	);

	engine.remove_pair(pair).await.unwrap();
	assert!(
		wait_until(WATCH_SETTLE, || {
			status.borrow().state == WatchState::PairRemoved
		})
		.await,
		"the watch never reported its pair removed (state={:?}, passes={})",
		status.borrow().state,
		log.passes()
	);

	// And it really is stopped: several safety-net intervals with a fresh local change in them
	// produce no pass at all.
	let passes_at_removal = log.passes();
	let uploaded_at_removal = log.uploaded();
	write_file(&sc.local, "after_removal.txt", b"must not be synced");
	tokio::time::sleep(NET * 4).await;
	assert_eq!(
		log.passes(),
		passes_at_removal,
		"a pass ran after the pair was removed"
	);
	assert_eq!(
		log.uploaded(),
		uploaded_at_removal,
		"an upload happened after the pair was removed"
	);

	// Both sides intact, and the post-removal change left alone.
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "kept.txt").is_some(),
		"removing the pair trashed the remote file"
	);
	assert!(
		find_file(&files, "after_removal.txt").is_none(),
		"the post-removal change was synced anyway"
	);
	assert!(
		read_eq(&sc.local, "kept.txt", b"before the removal"),
		"removing the pair touched the local file"
	);

	// The handle is still fine: stopping an already-stopped loop returns at once.
	let t = std::time::Instant::now();
	handle.stop().await;
	assert!(
		t.elapsed() < Duration::from_secs(5),
		"stopping a watch whose pair was removed hung"
	);

	sc.cleanup();
}

// (add) — a burst of REMOTE notifications is coalesced into a single pass (remote symmetry of -03).
// Doable in spirit (bulk-create 50 remote files, assert one coalesced pull), but reliably proving
// "exactly one pass, not 50" for the SOCKET notification path needs deterministic delivery of 50
// notifications inside one debounce window; cache convergence batching makes the per-notification
// pass count non-deterministic on the live backend. Plan: bulk-create 50 remote files, assert all
// 50 pulled byte-exact and the pass count is small/bounded.
#[ignore = "blocked: needs deterministic in-window remote-notification delivery — see TODO"]
#[shared_test_runtime]
async fn watch_add_remote_burst_coalesces() {}

// ============================================================================
// (add) — the safety net measures time since the last SUCCESSFUL pass
// ============================================================================

/// A stream of event passes, each well inside the net interval, must keep the net from firing at
/// all: every successful pass restarts it. The other half is that this cannot starve the net — once
/// the events stop, the net passes come as usual.
///
/// The trigger is a file created and deleted again inside one debounce window, so each pass wakes,
/// finds nothing to do and writes nothing: a real upload would put its own cache announcement into
/// the pass count and make the bound below meaningless.
#[shared_test_runtime]
async fn watch_add_net_interval_resets_but_not_starved() {
	const NET: Duration = Duration::from_secs(6);
	const SPACING: Duration = Duration::from_secs(3);
	const TRIGGERS: usize = 20;

	let sc = single_client(SyncMode::LocalToRemote).await;
	let (engine, pair) = watch_engine(&sc, SyncMode::LocalToRemote).await;
	let log = Arc::new(WatchLog::default());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: Duration::from_secs(1),
				safety_net: NET,
			},
			observer_for(log.clone()),
		)
		.await
		.unwrap();

	// Settle: the initial pass and whatever the registrations themselves triggered must be spent
	// before the pass count means anything.
	assert!(
		wait_until(WATCH_SETTLE, || log.passes() >= 1).await,
		"the watch never ran its initial pass"
	);
	tokio::time::sleep(NET * 2).await;
	let before = log.passes();

	// One no-op trigger every SPACING, for well over three net intervals — each waited out before
	// the next goes in. That wait is what gives the count below its meaning: two triggers landing
	// inside one in-flight pass coalesce (the dirty signal holds a single permit), and the triggered
	// passes they cost would leave exactly the room under the bound that the net passes this test is
	// looking for would take. Awaiting each one pins the triggered count at TRIGGERS, so any surplus
	// is the net.
	for i in 0..TRIGGERS {
		let expected = log.passes() + 1;
		let name = format!("blip{i}.txt");
		write_file(&sc.local, &name, b"gone before the pass");
		std::fs::remove_file(sc.local.join(&name)).unwrap();
		assert!(
			wait_until(NET * 2, || log.passes() >= expected).await,
			"trigger {i} never ran its pass"
		);
		tokio::time::sleep(SPACING).await;
	}

	// Each of those passes reset the net, so the only passes in that window are the triggered ones:
	// without the reset the net would have fired another TRIGGERS * SPACING / NET times on top.
	let during = log.passes() - before;
	assert!(
		during <= TRIGGERS + 1,
		"the safety net fired despite the event passes resetting it ({during} passes for \
		 {TRIGGERS} triggers)"
	);
	assert_eq!(log.uploaded(), 0, "a since-deleted file was uploaded");
	assert_eq!(log.remotely_trashed(), 0, "unexpected remote trash");

	// Not starvable: with the triggers stopped, the net drives passes on its own again.
	let idle_from = log.passes();
	assert!(
		wait_until(NET * 4, || log.passes() >= idle_from + 2).await,
		"the safety net stopped firing after the event passes ({idle_from} -> {})",
		log.passes()
	);

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		files.is_empty(),
		"the remote gained {} file(s) from a no-op trigger",
		files.len()
	);

	handle.stop().await;
	sc.cleanup();
}
