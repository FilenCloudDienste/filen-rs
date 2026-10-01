//! Non-colliding convergence & ordering tests (`CONVERGE-*`) for the two-way sync engine.
use std::{borrow::Cow, collections::BTreeSet, sync::Arc, time::Duration};

use filen_macros::shared_test_runtime;
use filen_types::fs::StableUuid;
use futures::FutureExt;

use filen_sdk_rs::fs::HasName;
use filen_sdk_rs::fs::HasUUID;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::sync::lock::ResourceLock;
use filen_sdk_rs::sync_engine::{SyncEngine, SyncMode};
use tracing::Instrument;
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

/// Upload bytes to a remote dir (by uuid) via a client, returning the created file.
async fn upload_to(
	client: &filen_sdk_rs::auth::Client,
	parent: Uuid,
	name: &str,
	data: &[u8],
) -> filen_sdk_rs::fs::file::RemoteFile {
	let builder = client.make_file_builder(name, parent).unwrap();
	client.upload_file(builder, data).await.unwrap()
}

/// List the (dirs, files) directly under a remote directory via a client.
async fn list_dir(
	client: &filen_sdk_rs::auth::Client,
	dir: &filen_sdk_rs::fs::dir::RemoteDirectory,
) -> (
	Vec<filen_sdk_rs::fs::dir::RemoteDirectory>,
	Vec<filen_sdk_rs::fs::file::RemoteFile>,
) {
	client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

fn find_file<'a>(
	files: &'a [filen_sdk_rs::fs::file::RemoteFile],
	name: &str,
) -> Option<&'a filen_sdk_rs::fs::file::RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

fn find_dir<'a>(
	dirs: &'a [filen_sdk_rs::fs::dir::RemoteDirectory],
	name: &str,
) -> Option<&'a filen_sdk_rs::fs::dir::RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
}

// ============================================================================
// CONVERGE-01 — Single local create propagates up (one-sided, local origin)
// ============================================================================
#[shared_test_runtime]
async fn converge_01_single_local_create_propagates_up() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "a.txt", b"hello");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "errors: {:?}", r1.errors);
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.downloaded, 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	// Local unchanged byte-for-byte.
	assert!(read_eq(&sc.local, "a.txt", b"hello"));

	// Remote holds it byte-exact.
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let f = find_file(&files, "a.txt").expect("a.txt missing on remote");
	assert_eq!(f.size, b"hello".len() as u64);

	// Second immediate pass: no re-upload.
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");

	sc.cleanup();
}

// ============================================================================
// CONVERGE-02 — Single remote create propagates down (one-sided, remote origin)
// ============================================================================
#[shared_test_runtime]
async fn converge_02_single_remote_create_propagates_down() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_to(&sc.cache.client, sc.remote, "b.txt", b"world").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed b.txt"
	);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "errors: {:?}", r1.errors);
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(r1.uploaded, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	assert!(read_eq(&sc.local, "b.txt", b"world"));

	let r2 = sc.sync().await;
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");

	sc.cleanup();
}

// ============================================================================
// CONVERGE-03 — One-sided modification of an already-synced file propagates
// ============================================================================
#[shared_test_runtime]
async fn converge_03_one_sided_modification_propagates() {
	// Local-origin variant.
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "m.txt", b"v1");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");
	assert!(r0.errors.is_empty(), "{r0:?}");

	write_file(&sc.local, "m.txt", b"v2-local");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.uploaded, 1,
		"one-sided edit must update, not conflict: {r1:?}"
	);
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let f = find_file(&files, "m.txt").expect("m.txt missing");
	assert_eq!(f.size, b"v2-local".len() as u64);

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	sc.cleanup();

	// Remote-origin variant (separate fixture).
	let sc2 = single_client(SyncMode::TwoWay).await;
	write_file(&sc2.local, "m.txt", b"v1");
	let s0 = sc2.sync().await;
	assert_eq!(s0.uploaded, 1, "{s0:?}");

	// One pass that LISTS our own push as the remote head, which is what records v1 as the content
	// both sides hold (the same explicit step CONVERGE-20 takes). Without it the remote edit below
	// is indistinguishable from one made CONCURRENTLY with the v1 push — a brand-new file's row
	// carries no agreed content, and an unconfirmed push is exactly what makes a foreign version a
	// conflict — so the edit would surface instead of pulling. A client's own sync loop supplies
	// this pass in practice.
	let (_d, files) = list_dir(&sc2.cache.client, &sc2.resources.dir).await;
	let v1_uuid: Uuid = find_file(&files, "m.txt").expect("m.txt missing").uuid();
	let v1_db = sc2.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || query_cached_file(
			&v1_db, v1_uuid
		)
		.is_some())
		.await,
		"cache never reflected v1"
	);
	let sconf = sc2.sync().await;
	assert!(sconf.errors.is_empty(), "{sconf:?}");
	assert_eq!(
		sconf.uploaded, 0,
		"the confirming pass must be a no-op: {sconf:?}"
	);
	assert_eq!(sconf.downloaded, 0, "{sconf:?}");

	// Modify the same name on the remote (server versions it to a new uuid).
	let new_rf = upload_to(&sc2.cache.client, sc2.remote, "m.txt", b"v2-remote").await;
	let new_uuid: Uuid = new_rf.uuid();
	let db = sc2.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(b"v2-remote".len() as i64)
		})
		.await,
		"cache never reflected the new remote version"
	);

	// Bounded eventual-consistency window for the versioning swap.
	let mut s1 = sc2.sync().await;
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
	while s1.downloaded == 0
		&& s1
			.errors
			.iter()
			.any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
		s1 = sc2.sync().await;
	}
	assert!(s1.errors.is_empty(), "{s1:?}");
	assert_eq!(
		s1.downloaded, 1,
		"remote-origin edit must re-download: {s1:?}"
	);
	assert_eq!(s1.conflicts.len(), 0, "{s1:?}");
	assert!(read_eq(&sc2.local, "m.txt", b"v2-remote"));

	let s2 = sc2.sync().await;
	assert_eq!(s2.downloaded, 0, "{s2:?}");
	assert_eq!(s2.uploaded, 0, "{s2:?}");
	sc2.cleanup();
}

// ============================================================================
// CONVERGE-04 — Disjoint simultaneous changes converge to the union (two-way)
// ============================================================================
#[shared_test_runtime]
async fn converge_04_disjoint_changes_union() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "only-local.txt", b"L");
	let rf = upload_to(&sc.cache.client, sc.remote, "only-remote.txt", b"R").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed only-remote.txt"
	);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");

	// Both sides hold the union.
	assert!(read_eq(&sc.local, "only-local.txt", b"L"));
	assert!(read_eq(&sc.local, "only-remote.txt", b"R"));
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert_eq!(files.len(), 2, "remote should hold both files");
	assert!(find_file(&files, "only-local.txt").is_some());
	assert!(find_file(&files, "only-remote.txt").is_some());

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-05 — Order independence: same change-set, different run order
// ============================================================================
async fn run_05_trial(order: Order) -> TreeMap {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = Default::default();

	write_file(&tc.local_a, "x.txt", b"X");
	std::fs::create_dir_all(tc.local_a.join("d1")).unwrap();
	write_file(&tc.local_b, "y.txt", b"Y");
	std::fs::create_dir_all(tc.local_b.join("d2")).unwrap();

	converge(
		tc.peer_a(),
		tc.peer_b(),
		order,
		&mut conflicts,
		"converge-05",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_b.join("x.txt").is_file()
				&& tc.local_a.join("y.txt").is_file()
				&& tc.local_b.join("d1").is_dir()
				&& tc.local_a.join("d2").is_dir()
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"order {order:?} surfaced conflicts: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");
	let tree = walk_tree(&tc.local_a);
	tc.cleanup();
	tree
}

#[shared_test_runtime]
async fn converge_05_order_independence_same_endstate() {
	let trial_a = run_05_trial(Order::AFirst).await;
	let trial_b = run_05_trial(Order::BFirst).await;
	assert_eq!(
		trial_a, trial_b,
		"same change-set converged to different trees by order"
	);
	assert!(
		trial_a.contains_key("x.txt")
			&& trial_a.contains_key("y.txt")
			&& trial_a.contains_key("d1")
			&& trial_a.contains_key("d2"),
		"converged tree missing an expected path: {:?}",
		trial_a.keys().collect::<Vec<_>>()
	);
}

// ============================================================================
// CONVERGE-06 — Multi-pass eventual consistency for changes arriving mid-flight
// ============================================================================
#[shared_test_runtime]
async fn converge_06_multi_pass_eventual_consistency() {
	let sc = single_client(SyncMode::TwoWay).await;

	write_file(&sc.local, "p1.txt", b"1");
	let ra = sc.sync().await;
	assert!(ra.errors.is_empty(), "{ra:?}");
	assert_eq!(ra.uploaded, 1, "{ra:?}");

	// Mid-flight: one new remote file and one new local file.
	let rf = upload_to(&sc.cache.client, sc.remote, "p2.txt", b"2").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed p2.txt"
	);
	write_file(&sc.local, "p3.txt", b"3");

	let rb = sc.sync().await;
	assert!(rb.errors.is_empty(), "{rb:?}");
	assert_eq!(rb.downloaded, 1, "p2 must download: {rb:?}");
	assert_eq!(rb.uploaded, 1, "p3 must upload: {rb:?}");

	let rc = sc.sync().await;
	assert!(rc.errors.is_empty(), "{rc:?}");
	assert_eq!(rc.uploaded, 0, "converged: {rc:?}");
	assert_eq!(rc.downloaded, 0, "converged: {rc:?}");

	assert!(read_eq(&sc.local, "p1.txt", b"1"));
	assert!(read_eq(&sc.local, "p2.txt", b"2"));
	assert!(read_eq(&sc.local, "p3.txt", b"3"));
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert_eq!(files.len(), 3, "remote should hold all 3 files");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-07 — Idempotent stability: repeated passes do nothing
// ============================================================================
#[shared_test_runtime]
async fn converge_07_idempotent_stability() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "f1.txt", b"one");
	write_file(&sc.local, "d1/f2.txt", b"two");
	write_file(&sc.local, "d1/d2/f3.txt", b"three");
	std::fs::create_dir_all(sc.local.join("emptydir")).unwrap();

	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 3, "{r0:?}");

	let before = walk_tree(&sc.local);
	for i in 0..5 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {i}: {r:?}");
		assert_eq!(r.uploaded, 0, "pass {i}: {r:?}");
		assert_eq!(r.downloaded, 0, "pass {i}: {r:?}");
		assert_eq!(r.remotely_trashed, 0, "pass {i}: {r:?}");
		assert_eq!(r.locally_deleted, 0, "pass {i}: {r:?}");
		assert_eq!(r.moved_remote, 0, "pass {i}: {r:?}");
		assert_eq!(r.moved_local, 0, "pass {i}: {r:?}");
		assert_eq!(r.local_dirs_created, 0, "pass {i}: {r:?}");
		assert_eq!(r.remote_dirs_created, 0, "pass {i}: {r:?}");
		assert_eq!(r.conflicts.len(), 0, "pass {i}: {r:?}");
	}
	assert_eq!(
		walk_tree(&sc.local),
		before,
		"local tree changed across idle passes"
	);
	// No quarantine bin created.
	assert!(
		!sc.local.join(".filen-sync-trash").exists(),
		"quarantine bin created on idle passes"
	);
	sc.cleanup();
}

// ============================================================================
// CONVERGE-08 — No oscillation on metadata-only / equal-content edge
// ============================================================================
#[shared_test_runtime]
async fn converge_08_no_oscillation_on_timestamp_jitter() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "t.txt", b"same");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	// Touch locally: rewrite identical bytes (changes mtime, not content).
	write_file(&sc.local, "t.txt", b"same");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.conflicts.len(),
		0,
		"equal-content touch must not conflict: {r1:?}"
	);

	// A few more passes: action counts must trend to zero (steady state).
	let mut last = sc.sync().await;
	for _ in 0..3 {
		last = sc.sync().await;
		assert!(last.errors.is_empty(), "{last:?}");
	}
	assert_eq!(last.uploaded, 0, "no perpetual re-upload: {last:?}");
	assert_eq!(last.downloaded, 0, "no perpetual re-download: {last:?}");
	assert_eq!(last.conflicts.len(), 0, "{last:?}");
	assert!(read_eq(&sc.local, "t.txt", b"same"));
	sc.cleanup();
}

// ============================================================================
// CONVERGE-09 — One-sided deletion mirrors in the active direction
// ============================================================================
#[shared_test_runtime]
async fn converge_09_local_delete_mirrors_to_remote() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "keep.txt", b"keep");
	write_file(&sc.local, "del.txt", b"del");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 2, "{r0:?}");

	std::fs::remove_file(sc.local.join("del.txt")).unwrap();
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.remotely_trashed, 1,
		"local delete should trash remote: {r1:?}"
	);
	assert_eq!(
		r1.held_deletions(),
		0,
		"1/2 must not trip the guard: {r1:?}"
	);

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "del.txt").is_none(),
		"remote still has del.txt"
	);
	assert!(find_file(&files, "keep.txt").is_some());

	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	sc.cleanup();
}

#[shared_test_runtime]
async fn converge_09_remote_delete_mirrors_to_local() {
	let sc = single_client(SyncMode::TwoWay).await;
	let keep = upload_to(&sc.cache.client, sc.remote, "keep.txt", b"keep").await;
	let mut del = upload_to(&sc.cache.client, sc.remote, "del.txt", b"del").await;
	assert!(poll_for_item(sc.cache.db_path(), keep.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	assert!(poll_for_item(sc.cache.db_path(), del.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let r0 = sc.sync().await;
	assert_eq!(r0.downloaded, 2, "{r0:?}");
	assert!(read_eq(&sc.local, "del.txt", b"del"));

	sc.cache.client.trash_file(&mut del).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), del.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped trashed del.txt"
	);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.locally_deleted, 1,
		"remote delete should remove local: {r1:?}"
	);
	assert!(
		!sc.local.join("del.txt").exists(),
		"local del.txt still present"
	);
	assert!(sc.local.join("keep.txt").is_file());

	let r2 = sc.sync().await;
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-10 — Backup modes are additive: one-sided delete NOT mirrored
// ============================================================================
#[shared_test_runtime]
async fn converge_10_local_backup_delete_not_mirrored() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "keep.txt", b"keep me");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	std::fs::remove_file(sc.local.join("keep.txt")).unwrap();
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.remotely_trashed, 0,
		"backup must not mirror delete: {r1:?}"
	);

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "keep.txt").is_some(),
		"remote-side survivor was trashed"
	);

	// Stable: re-pass does not re-create the deleted local side nor re-delete remote.
	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "backup never pulls: {r2:?}");
	assert!(
		!sc.local.join("keep.txt").exists(),
		"deleted local side re-created"
	);
	sc.cleanup();
}

#[shared_test_runtime]
async fn converge_10_remote_backup_delete_not_mirrored() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let mut keep = upload_to(&sc.cache.client, sc.remote, "keep2.txt", b"keep me too").await;
	assert!(poll_for_item(sc.cache.db_path(), keep.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let r0 = sc.sync().await;
	assert_eq!(r0.downloaded, 1, "{r0:?}");
	assert!(read_eq(&sc.local, "keep2.txt", b"keep me too"));

	sc.cache.client.trash_file(&mut keep).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), keep.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped trashed remote keep2.txt"
	);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.locally_deleted, 0,
		"remote-backup must not delete local: {r1:?}"
	);
	assert!(
		read_eq(&sc.local, "keep2.txt", b"keep me too"),
		"local survivor was deleted"
	);

	let r2 = sc.sync().await;
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "remote-backup never pushes: {r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-11 — Two clients sharing one remote converge to identical trees
// ============================================================================
#[shared_test_runtime]
async fn converge_11_two_clients_identical_trees() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = Default::default();

	write_file(&tc.local_a, "s1.txt", b"a");
	write_file(&tc.local_a, "subdir/s2.txt", b"b");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::AFirst,
		&mut conflicts,
		"c11-push-a",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_b.join("s1.txt").is_file()
				&& tc.local_b.join("subdir/s2.txt").is_file()
		},
	)
	.await;

	write_file(&tc.local_b, "s3.txt", b"c");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::BFirst,
		&mut conflicts,
		"c11-push-b",
		|| trees_equal(&tc.local_a, &tc.local_b) && tc.local_a.join("s3.txt").is_file(),
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"non-colliding run surfaced conflicts: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");
	assert!(read_eq(&tc.local_a, "s1.txt", b"a"));
	assert!(read_eq(&tc.local_b, "subdir/s2.txt", b"b"));
	assert!(read_eq(&tc.local_a, "s3.txt", b"c"));
	tc.cleanup();
}

// ============================================================================
// CONVERGE-12 — Three+ clients converge to the union of disjoint contributions
// ============================================================================
#[shared_test_runtime]
async fn converge_12_three_clients_union() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();

	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;
	let cache_c = TestCache::new(&resources.client, remote).await;
	let local_a = fresh_local_dir("c12a");
	let local_b = fresh_local_dir("c12b");
	let local_c = fresh_local_dir("c12c");

	let ea = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let eb = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let ec = SyncEngine::open(cache_c.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pa = ea
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pb = eb
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pc = ec
		.add_pair(local_c.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	write_file(&local_a, "c1/file1.txt", b"1");
	write_file(&local_b, "c2/file2.txt", b"2");
	write_file(&local_c, "c3/file3.txt", b"3");

	let all_present = |roots: &[&std::path::Path]| {
		roots.iter().all(|r| {
			r.join("c1/file1.txt").is_file()
				&& r.join("c2/file2.txt").is_file()
				&& r.join("c3/file3.txt").is_file()
		})
	};

	const MAX_ROUNDS: usize = 20;
	let mut converged = false;
	for _ in 0..MAX_ROUNDS {
		for (e, p) in [(&ea, pa), (&eb, pb), (&ec, pc)] {
			let r = e.sync_once(p).await.expect("sync_once");
			assert!(r.errors.is_empty(), "errors: {:?}", r.errors);
			assert!(
				r.conflicts.is_empty(),
				"unexpected conflict: {:?}",
				r.conflicts
			);
		}
		if all_present(&[&local_a, &local_b, &local_c])
			&& trees_equal(&local_a, &local_b)
			&& trees_equal(&local_b, &local_c)
		{
			converged = true;
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
	}
	assert!(
		converged,
		"three clients did not converge within {MAX_ROUNDS} rounds"
	);

	// Final no-change round reports 0 actions everywhere.
	for (e, p) in [(&ea, pa), (&eb, pb), (&ec, pc)] {
		let r = e.sync_once(p).await.expect("final sync_once");
		assert_eq!(r.uploaded, 0, "{r:?}");
		assert_eq!(r.downloaded, 0, "{r:?}");
	}

	assert_trees_identical(&local_a, &local_b, "local_a", "local_b");
	assert_trees_identical(&local_b, &local_c, "local_b", "local_c");
	assert!(read_eq(&local_a, "c1/file1.txt", b"1"));
	assert!(read_eq(&local_a, "c2/file2.txt", b"2"));
	assert!(read_eq(&local_a, "c3/file3.txt", b"3"));

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
	std::fs::remove_dir_all(&local_c).ok();
}

// ============================================================================
// CONVERGE-13 — Disjoint rename converges without duplicate or conflict
// ============================================================================
#[shared_test_runtime]
async fn converge_13_local_rename_propagates() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "old.txt", b"data");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	let (_d0, files0) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let orig_uuid = find_file(&files0, "old.txt").unwrap().uuid();

	move_file(&sc.local, "old.txt", "new.txt");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.moved_remote, 1, "rename should be a remote move: {r1:?}");
	assert_eq!(r1.uploaded, 0, "rename must not re-upload: {r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	let (_d1, files1) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert_eq!(files1.len(), 1, "exactly one file expected");
	let after = find_file(&files1, "new.txt").expect("new.txt missing");
	assert_eq!(after.uuid(), orig_uuid, "uuid preserved across rename");
	assert_eq!(after.size, b"data".len() as u64);

	let r2 = sc.sync().await;
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-14 — Disjoint adds into the same directory from two clients merge
// ============================================================================
#[shared_test_runtime]
async fn converge_14_same_dir_disjoint_adds_merge() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = Default::default();

	// Converge a shared/ dir with base.txt (seeded on A).
	write_file(&tc.local_a, "shared/base.txt", b"base");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::AFirst,
		&mut conflicts,
		"c14-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && tc.local_b.join("shared/base.txt").is_file(),
	)
	.await;

	// Disjoint adds into the same parent dir.
	write_file(&tc.local_a, "shared/fromC1.txt", b"1");
	write_file(&tc.local_b, "shared/fromC2.txt", b"2");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::AFirst,
		&mut conflicts,
		"c14-merge",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_a.join("shared/fromC2.txt").is_file()
				&& tc.local_b.join("shared/fromC1.txt").is_file()
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"disjoint adds surfaced conflicts: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");
	for root in [&tc.local_a, &tc.local_b] {
		assert!(root.join("shared/base.txt").is_file());
		assert!(root.join("shared/fromC1.txt").is_file());
		assert!(root.join("shared/fromC2.txt").is_file());
	}
	tc.cleanup();
}

// ============================================================================
// CONVERGE-15 — Deep nested tree creation propagates fully
// ============================================================================
#[shared_test_runtime]
async fn converge_15_deep_nested_tree_propagates() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "a/b/c/d/leaf.txt", b"deep");
	write_file(&sc.local, "a/b/sibling.txt", b"sib");

	// Converge within a small bounded number of passes.
	let mut passes = 0usize;
	loop {
		passes += 1;
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {passes}: {r:?}");
		if r.uploaded == 0 && r.remote_dirs_created == 0 {
			break;
		}
		assert!(passes < 6, "did not converge within 6 passes");
	}

	// Walk the remote tree.
	let (root_dirs, _root_files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let a = find_dir(&root_dirs, "a").expect("dir a missing");
	let (a_dirs, _) = list_dir(&sc.cache.client, a).await;
	let b = find_dir(&a_dirs, "b").expect("dir a/b missing");
	let (b_dirs, b_files) = list_dir(&sc.cache.client, b).await;
	assert!(
		find_file(&b_files, "sibling.txt").is_some(),
		"a/b/sibling.txt missing"
	);
	let c = find_dir(&b_dirs, "c").expect("dir a/b/c missing");
	let (c_dirs, _) = list_dir(&sc.cache.client, c).await;
	let d = find_dir(&c_dirs, "d").expect("dir a/b/c/d missing");
	let (_, d_files) = list_dir(&sc.cache.client, d).await;
	let leaf = find_file(&d_files, "leaf.txt").expect("leaf.txt missing");
	assert_eq!(leaf.size, b"deep".len() as u64);
	sc.cleanup();
}

// ============================================================================
// CONVERGE-16 — Interrupted pass re-runs cleanly  [BLOCKED]
// ============================================================================
#[ignore = "blocked: needs fault-injection harness for deterministic mid-pass interruption — see TODO"]
#[shared_test_runtime]
async fn converge_16_interrupted_pass_reruns_cleanly() {
	// plan: create 10 distinct files locally; start a sync pass and interrupt it after partial
	// application; re-run a full pass; assert all 10 exist byte-exact on both sides, no dup/loss,
	// no spurious conflict; final pass 0 actions. Requires deterministic mid-transfer interruption,
	// which the current harness/public API cannot inject.
}

// ============================================================================
// CONVERGE-17 — Baseline persists across restart; converged state stays stable
// ============================================================================
#[shared_test_runtime]
async fn converge_17_baseline_persists_across_restart() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("c17");

	// A persistent baseline DB path reused across the simulated restart.
	let baseline_db = temp_cache_path();

	write_file(&local, "r1.txt", b"one");
	write_file(&local, "d/r2.txt", b"two");

	{
		let engine = SyncEngine::open(cache.client.clone(), baseline_db.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::TwoWay)
			.await
			.unwrap();
		let r0 = engine.sync_once(pair).await.unwrap();
		assert!(r0.errors.is_empty(), "{r0:?}");
		assert_eq!(r0.uploaded, 2, "{r0:?}");
		// engine drops here -> simulates process exit with persisted baseline.
	}

	// "Restart": reopen the engine on the SAME baseline DB and re-add the same pair.
	let engine2 = SyncEngine::open(cache.client.clone(), baseline_db.clone())
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	let r1 = engine2.sync_once(pair2).await.unwrap();
	assert!(r1.errors.is_empty(), "post-restart pass errors: {r1:?}");
	assert_eq!(r1.uploaded, 0, "no re-upload after restart: {r1:?}");
	assert_eq!(r1.downloaded, 0, "no re-download after restart: {r1:?}");
	assert_eq!(
		r1.remotely_trashed, 0,
		"no mistaken deletion after restart: {r1:?}"
	);
	assert_eq!(
		r1.locally_deleted, 0,
		"no mistaken deletion after restart: {r1:?}"
	);
	assert!(read_eq(&local, "r1.txt", b"one"));
	assert!(read_eq(&local, "d/r2.txt", b"two"));

	// A single new change after restart propagates (1 action).
	write_file(&local, "r3.txt", b"three");
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 1,
		"new change after restart must propagate: {r2:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

// ============================================================================
// CONVERGE-18 — Empty directories converge and persist
// ============================================================================
#[shared_test_runtime]
async fn converge_18_empty_dir_converges_and_persists() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	std::fs::create_dir_all(sc.local.join("emptydir")).unwrap();

	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(
		r0.remote_dirs_created, 1,
		"empty dir should be created: {r0:?}"
	);
	assert_eq!(r0.uploaded, 0, "{r0:?}");

	let (dirs, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(files.is_empty(), "no phantom files");
	let ed = find_dir(&dirs, "emptydir").expect("emptydir missing on remote");
	let (ed_dirs, ed_files) = list_dir(&sc.cache.client, ed).await;
	assert!(
		ed_dirs.is_empty() && ed_files.is_empty(),
		"emptydir should be empty on remote"
	);

	// Subsequent passes do not delete/recreate.
	for _ in 0..2 {
		let r = sc.sync().await;
		assert_eq!(r.remote_dirs_created, 0, "{r:?}");
		assert_eq!(r.remotely_trashed, 0, "{r:?}");
		assert!(r.errors.is_empty(), "{r:?}");
	}
	assert!(
		sc.local.join("emptydir").is_dir(),
		"local emptydir vanished"
	);
	sc.cleanup();
}

// ============================================================================
// CONVERGE-19 — Large fan-out converges without loss; report counts accurate
// ============================================================================
#[shared_test_runtime]
async fn converge_19_large_fanout_accurate_counts() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const DIRS: usize = 10;
	const FILES: usize = 200;
	for i in 0..FILES {
		let d = i % DIRS;
		let rel = format!("d{d:02}/f{i:03}.txt");
		write_file(&sc.local, &rel, content_for(&rel).as_slice());
	}

	let mut uploaded = 0usize;
	let mut dirs_created = 0usize;
	let mut passes = 0usize;
	loop {
		passes += 1;
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {passes}: {r:?}");
		uploaded += r.uploaded;
		dirs_created += r.remote_dirs_created;
		if r.uploaded == 0 && r.remote_dirs_created == 0 {
			break;
		}
		assert!(passes < 6, "did not converge within 6 passes");
	}
	assert_eq!(
		uploaded, FILES,
		"exactly {FILES} uploads expected, got {uploaded}"
	);
	assert_eq!(
		dirs_created, DIRS,
		"exactly {DIRS} dir creates expected, got {dirs_created}"
	);

	// Verify all files present on remote with correct placement.
	let (root_dirs, _root_files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let mut total_remote = 0usize;
	for d in 0..DIRS {
		let dir = find_dir(&root_dirs, &format!("d{d:02}"))
			.unwrap_or_else(|| panic!("dir d{d:02} missing"));
		let (_sub, fs) = list_dir(&sc.cache.client, dir).await;
		total_remote += fs.len();
	}
	assert_eq!(total_remote, FILES, "remote file count mismatch");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-20 — Sequential one-sided edits on alternating sides converge
// ============================================================================
#[shared_test_runtime]
async fn converge_20_alternating_one_sided_edits() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "seq.txt", b"v0");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	// Local edit -> v1.
	write_file(&sc.local, "seq.txt", b"v1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let v1 = find_file(&files, "seq.txt").expect("seq.txt missing");
	assert_eq!(v1.size, b"v1".len() as u64);

	// One pass that LISTS our own push as the remote head, which is what records v1 as the content
	// both sides hold. Without it the v2 edit below is indistinguishable from an edit made
	// concurrently with the v1 push and surfaces as a conflict instead of pulling — the client's
	// own sync loop supplies this pass in practice, and here it is made explicit so the alternation
	// this test is about stays deterministic.
	let v1_uuid: Uuid = v1.uuid();
	let cache_db = sc.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || query_cached_file(
			&cache_db, v1_uuid
		)
		.is_some())
		.await,
		"cache never reflected v1"
	);
	let rc = sc.sync().await;
	assert!(rc.errors.is_empty(), "{rc:?}");
	assert_eq!(
		rc.uploaded, 0,
		"the confirming pass must be a no-op: {rc:?}"
	);
	assert_eq!(rc.downloaded, 0, "{rc:?}");
	assert_eq!(rc.conflicts.len(), 0, "{rc:?}");

	// Remote edit -> v2 (versioned to new uuid).
	let v2 = upload_to(&sc.cache.client, sc.remote, "seq.txt", b"v2").await;
	let v2_uuid: Uuid = v2.uuid();
	let db = sc.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, v2_uuid).map(|t| t.1) == Some(b"v2".len() as i64)
		})
		.await,
		"cache never reflected v2"
	);
	let mut r2 = sc.sync().await;
	let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
	while r2.downloaded == 0
		&& r2
			.errors
			.iter()
			.any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
		r2 = sc.sync().await;
	}
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.downloaded, 1, "remote edit must pull: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert!(read_eq(&sc.local, "seq.txt", b"v2"));

	// Local edit -> v3.
	write_file(&sc.local, "seq.txt", b"v3");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 1, "{r3:?}");
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");

	let r4 = sc.sync().await;
	assert_eq!(r4.uploaded, 0, "{r4:?}");
	assert_eq!(r4.downloaded, 0, "{r4:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-21 — Watch mode: engine's own writes do not loop
// ============================================================================
#[shared_test_runtime]
async fn converge_21_watch_no_self_induced_loop() {
	use std::sync::Arc;
	let sc = single_client(SyncMode::LocalToRemote).await;
	// Take ownership of the engine to wrap it in an Arc for watch().
	let SingleClient {
		resources,
		cache,
		engine,
		pair,
		local,
		remote: _remote,
	} = sc;
	let engine = Arc::new(engine);
	let handle = engine.clone().watch(pair).await.unwrap();

	write_file(&local, "w.txt", b"watched");

	// Wait for the file to propagate exactly once to the remote.
	let mut found = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_dir(&cache.client, &resources.dir).await;
		if find_file(&files, "w.txt").is_some() {
			found = true;
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(300)).await;
	}
	assert!(found, "watch did not propagate w.txt");

	// Quiet window: with no further user changes the remote must hold exactly one w.txt and not
	// accumulate duplicates from self-induced events.
	tokio::time::sleep(std::time::Duration::from_secs(8)).await;
	let (_d, files) = list_dir(&cache.client, &resources.dir).await;
	let count = files.iter().filter(|f| f.name() == Some("w.txt")).count();
	assert_eq!(
		count, 1,
		"self-induced loop produced duplicate w.txt: {count}"
	);
	assert_eq!(files.len(), 1, "extra files appeared during quiet window");

	drop(handle);
	std::fs::remove_dir_all(&local).ok();
}

// ============================================================================
// CONVERGE-22 — Watch mode: disjoint near-simultaneous two-client changes
// ============================================================================
#[shared_test_runtime]
async fn converge_22_watch_two_clients_disjoint_converge() {
	let tc = two_clients(SyncMode::TwoWay).await;
	watch_two_clients_disjoint_converge(tc, None).await;
}

/// CONVERGE-22 with another client holding the drive-write lock while both stacks start, and for a
/// while after: both caches' first resyncs and both engines' first passes queue behind it and race
/// for it the moment it frees — what a device sees starting up while another one is mid-sync.
#[shared_test_runtime]
async fn converge_22b_watch_two_clients_disjoint_converge_behind_a_held_lock() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let holder = derive_client(&resources.client);
	let held = holder.lock_drive().await.unwrap();
	let tc = two_clients_on(resources, SyncMode::TwoWay).await;
	watch_two_clients_disjoint_converge(tc, Some(held)).await;
}

/// Watch both clients, write one file on each side, and wait for both trees to hold both. `held`,
/// if any, is a drive-write lock another client took before either stack started; it is released
/// once both writes have had long enough to queue behind it.
async fn watch_two_clients_disjoint_converge(tc: TwoClients, held: Option<Arc<ResourceLock>>) {
	const HOLD: Duration = Duration::from_secs(20);
	let TwoClients {
		resources,
		cache_a,
		cache_b,
		engine_a,
		engine_b,
		pair_a,
		pair_b,
		local_a,
		local_b,
		remote: _remote,
	} = tc;
	let _ = (&cache_a, &cache_b);
	let ea = Arc::new(engine_a);
	let eb = Arc::new(engine_b);
	// One span per side: both engines number their pair 1, so their logs are otherwise the same.
	let ha = ea
		.clone()
		.watch(pair_a)
		.instrument(tracing::info_span!("engine_a"))
		.await
		.unwrap();
	let hb = eb
		.clone()
		.watch(pair_b)
		.instrument(tracing::info_span!("engine_b"))
		.await
		.unwrap();

	// Near-simultaneous disjoint creates.
	write_file(&local_a, "wa.txt", b"a");
	write_file(&local_b, "wb.txt", b"b");

	if let Some(held) = held {
		tokio::time::sleep(HOLD).await;
		drop(held);
	}

	// Wait for both trees to converge to the union.
	let mut converged = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		if read_eq(&local_a, "wa.txt", b"a")
			&& read_eq(&local_a, "wb.txt", b"b")
			&& read_eq(&local_b, "wa.txt", b"a")
			&& read_eq(&local_b, "wb.txt", b"b")
			&& trees_equal(&local_a, &local_b)
		{
			converged = true;
			break;
		}
		tokio::time::sleep(std::time::Duration::from_millis(500)).await;
	}
	assert!(
		converged,
		"watch-mode two-client disjoint creates did not converge\n{}",
		describe_watches(&[("A", &ha, &local_a), ("B", &hb, &local_b)])
	);
	let _ = &resources;

	// Quiescence: a quiet window must not corrupt the converged trees.
	tokio::time::sleep(std::time::Duration::from_secs(5)).await;
	assert_trees_identical(&local_a, &local_b, "local_a", "local_b");

	drop(ha);
	drop(hb);
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ============================================================================
// CONVERGE-23 — First sync against an already-populated destination unions
// ============================================================================
#[shared_test_runtime]
async fn converge_23_first_sync_populated_dest_unions() {
	let sc = single_client(SyncMode::TwoWay).await;
	// Pre-existing remote file BEFORE the first pass.
	let rb = upload_to(&sc.cache.client, sc.remote, "rb.txt", b"r").await;
	assert!(poll_for_item(sc.cache.db_path(), rb.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	// Pre-existing local file.
	write_file(&sc.local, "la.txt", b"l");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "first-sync should adopt+push local: {r1:?}");
	assert_eq!(
		r1.downloaded, 1,
		"first-sync should adopt+pull remote: {r1:?}"
	);
	assert_eq!(r1.remotely_trashed, 0, "first sync must not wipe: {r1:?}");
	assert_eq!(r1.locally_deleted, 0, "first sync must not wipe: {r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	assert!(read_eq(&sc.local, "la.txt", b"l"));
	assert!(read_eq(&sc.local, "rb.txt", b"r"));
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert_eq!(files.len(), 2, "remote should hold both");
	assert!(find_file(&files, "la.txt").is_some());
	assert!(find_file(&files, "rb.txt").is_some());

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-24 — Convergence after an offline gap: many accumulated changes
// ============================================================================
#[shared_test_runtime]
async fn converge_24_offline_gap_backlog_converges() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "keep.txt", b"k0");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	// Accumulate disjoint changes "while paused": local add, remote add, local edit, remote nested.
	write_file(&sc.local, "g1.txt", b"g1");
	let g2 = upload_to(&sc.cache.client, sc.remote, "g2.txt", b"g2").await;
	write_file(&sc.local, "keep.txt", b"k1");
	let g3sub = sc
		.cache
		.client
		.create_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			"g3",
		)
		.await
		.unwrap();
	let g4 = upload_to(&sc.cache.client, g3sub.uuid(), "g4.txt", b"g4").await;
	assert!(poll_for_item(sc.cache.db_path(), g2.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	assert!(poll_for_item(sc.cache.db_path(), g4.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	// Resume: run passes until converged.
	let mut passes = 0usize;
	loop {
		passes += 1;
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {passes}: {r:?}");
		if r.uploaded == 0
			&& r.downloaded == 0
			&& r.remote_dirs_created == 0
			&& r.local_dirs_created == 0
		{
			assert_eq!(r.conflicts.len(), 0, "no conflict expected: {r:?}");
			break;
		}
		assert!(
			passes < 8,
			"offline backlog did not converge within 8 passes"
		);
	}

	assert!(read_eq(&sc.local, "keep.txt", b"k1"));
	assert!(read_eq(&sc.local, "g1.txt", b"g1"));
	assert!(read_eq(&sc.local, "g2.txt", b"g2"));
	assert!(read_eq(&sc.local, "g3/g4.txt", b"g4"));
	sc.cleanup();
}

// ============================================================================
// CONVERGE-25 — Order independence of deletes vs creates in the same directory
// ============================================================================
async fn run_25_trial(order: Order) -> TreeMap {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = Default::default();

	// Converge dir/old.txt and dir/keep.txt (seeded on A).
	write_file(&tc.local_a, "dir/old.txt", b"old");
	write_file(&tc.local_a, "dir/keep.txt", b"keep");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::AFirst,
		&mut conflicts,
		"c25-baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_b.join("dir/old.txt").is_file()
				&& tc.local_b.join("dir/keep.txt").is_file()
		},
	)
	.await;

	// C1 deletes dir/old.txt; C2 creates dir/added.txt (disjoint paths, same parent).
	std::fs::remove_file(tc.local_a.join("dir/old.txt")).unwrap();
	write_file(&tc.local_b, "dir/added.txt", b"added");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		order,
		&mut conflicts,
		"c25-mix",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& !tc.local_a.join("dir/old.txt").exists()
				&& !tc.local_b.join("dir/old.txt").exists()
				&& tc.local_a.join("dir/added.txt").is_file()
				&& tc.local_b.join("dir/keep.txt").is_file()
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"order {order:?} surfaced conflicts: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");
	let tree = walk_tree(&tc.local_a);
	tc.cleanup();
	tree
}

#[shared_test_runtime]
async fn converge_25_delete_vs_create_order_independent() {
	let a = run_25_trial(Order::AFirst).await;
	let b = run_25_trial(Order::BFirst).await;
	assert_eq!(a, b, "delete+create converged to different trees by order");
	assert!(
		a.contains_key("dir/keep.txt"),
		"keep.txt missing: {:?}",
		a.keys().collect::<Vec<_>>()
	);
	assert!(
		a.contains_key("dir/added.txt"),
		"added.txt missing: {:?}",
		a.keys().collect::<Vec<_>>()
	);
	assert!(
		!a.contains_key("dir/old.txt"),
		"old.txt should be gone everywhere"
	);
}

// ============================================================================
// CONVERGE-A1 — One-sided deletion of a non-empty directory subtree propagates
// ============================================================================
#[shared_test_runtime]
async fn converge_a1_subtree_deletion_propagates() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "dir/sub/a.txt", b"a");
	write_file(&sc.local, "dir/sub/b.txt", b"b");
	write_file(&sc.local, "dir/c.txt", b"c");
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 3, "{r0:?}");

	// Delete the entire dir/ subtree locally.
	std::fs::remove_dir_all(sc.local.join("dir")).unwrap();
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert!(
		r1.remotely_trashed >= 1,
		"subtree deletion should trash on remote: {r1:?}"
	);

	// Remote dir/ and all descendants removed (no orphaned children).
	let (root_dirs, _root_files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(
		find_dir(&root_dirs, "dir").is_none(),
		"remote dir/ subtree not removed"
	);

	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A2 — One-sided directory move/rename converges without duplication
// ============================================================================
#[shared_test_runtime]
async fn converge_a2_directory_rename_no_duplication() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "old/x.txt", b"1");
	write_file(&sc.local, "old/sub/y.txt", b"2");
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 2, "{r0:?}");

	// Rename the directory old/ -> new/.
	move_file(&sc.local, "old", "new");
	// Converge (a directory move may take more than one pass to fully reconcile).
	let mut passes = 0usize;
	loop {
		passes += 1;
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {passes}: {r:?}");
		assert_eq!(r.uploaded, 0, "dir rename must not re-upload bytes: {r:?}");
		assert_eq!(r.conflicts.len(), 0, "{r:?}");
		if r.moved_remote == 0 && r.remote_dirs_created == 0 && r.remotely_trashed == 0 {
			break;
		}
		assert!(passes < 6, "dir rename did not converge within 6 passes");
	}

	let (root_dirs, _root_files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(find_dir(&root_dirs, "old").is_none(), "old/ should be gone");
	let new = find_dir(&root_dirs, "new").expect("new/ missing");
	let (new_dirs, new_files) = list_dir(&sc.cache.client, new).await;
	assert!(
		find_file(&new_files, "x.txt").is_some(),
		"new/x.txt missing"
	);
	let sub = find_dir(&new_dirs, "sub").expect("new/sub missing");
	let (_d, sub_files) = list_dir(&sc.cache.client, sub).await;
	assert!(
		find_file(&sub_files, "y.txt").is_some(),
		"new/sub/y.txt missing"
	);
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A3 — One-sided cross-directory file move converges
// ============================================================================
#[shared_test_runtime]
async fn converge_a3_cross_dir_file_move() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "src/m.txt", b"data");
	std::fs::create_dir_all(sc.local.join("dst")).unwrap();
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	let (root0, _f0) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let src0 = find_dir(&root0, "src").unwrap();
	let (_sd, src_files0) = list_dir(&sc.cache.client, src0).await;
	let orig_uuid = find_file(&src_files0, "m.txt").unwrap().uuid();

	move_file(&sc.local, "src/m.txt", "dst/m.txt");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.moved_remote, 1,
		"cross-dir move should be a remote move: {r1:?}"
	);
	assert_eq!(r1.uploaded, 0, "move must not re-upload: {r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	let (root1, _f1) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let dst = find_dir(&root1, "dst").expect("dst/ missing");
	let (_dd, dst_files) = list_dir(&sc.cache.client, dst).await;
	let moved = find_file(&dst_files, "m.txt").expect("dst/m.txt missing");
	assert_eq!(moved.uuid(), orig_uuid, "uuid preserved across move");
	let src1 = find_dir(&root1, "src").unwrap();
	let (_s2, src_files1) = list_dir(&sc.cache.client, src1).await;
	assert!(
		find_file(&src_files1, "m.txt").is_none(),
		"src/m.txt still present"
	);

	let r2 = sc.sync().await;
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A4 — Order independence of pure one-sided MODIFICATIONS
// ============================================================================
async fn run_a4_trial(order: Order) -> TreeMap {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = Default::default();

	// Converge p.txt and q.txt on both clients.
	write_file(&tc.local_a, "p.txt", b"p0");
	write_file(&tc.local_a, "q.txt", b"q0");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		Order::AFirst,
		&mut conflicts,
		"a4-baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc.local_b.join("p.txt").is_file()
				&& tc.local_b.join("q.txt").is_file()
		},
	)
	.await;

	// Both files reached the remote as pushes from A that no snapshot has confirmed yet: nothing has
	// listed A's own versions as the remote head, so B's edit below would read on A as an edit made
	// CONCURRENTLY with A's push and surface as a conflict instead of pulling. A running client's own
	// sync loop supplies that pass; here it is made explicit — as CONVERGE-20 does — so this stays a
	// test about ORDER rather than a race between A's push and B's edit reaching A's cache.
	let (_d, files) = list_dir(&tc.resources.client, &tc.resources.dir).await;
	let db_a = tc.cache_a.db_path().to_path_buf();
	for name in ["p.txt", "q.txt"] {
		let uuid: Uuid = find_file(&files, name)
			.unwrap_or_else(|| panic!("{name} never reached the remote"))
			.uuid();
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || query_cached_file(&db_a, uuid)
				.is_some())
			.await,
			"A's cache never listed its own push of {name}"
		);
	}
	let rconf = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	assert!(rconf.errors.is_empty(), "{rconf:?}");
	assert_eq!(
		rconf.uploaded + rconf.downloaded,
		0,
		"the confirming pass must be a no-op: {rconf:?}"
	);
	assert_eq!(rconf.conflicts.len(), 0, "{rconf:?}");

	// Disjoint modifications: A edits p, B edits q.
	write_file(&tc.local_a, "p.txt", b"p1");
	write_file(&tc.local_b, "q.txt", b"q1");
	converge(
		tc.peer_a(),
		tc.peer_b(),
		order,
		&mut conflicts,
		"a4-mods",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& read_eq(&tc.local_b, "p.txt", b"p1")
				&& read_eq(&tc.local_a, "q.txt", b"q1")
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"order {order:?} surfaced conflicts: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");
	let tree = walk_tree(&tc.local_a);
	tc.cleanup();
	tree
}

#[shared_test_runtime]
async fn converge_a4_modification_order_independent() {
	let a = run_a4_trial(Order::AFirst).await;
	let b = run_a4_trial(Order::BFirst).await;
	assert_eq!(
		a, b,
		"disjoint modifications converged to different trees by order"
	);
	assert_eq!(
		a.get("p.txt").map(|v| v.2.clone()),
		Some(b"p1".to_vec()),
		"p.txt not p1"
	);
	assert_eq!(
		a.get("q.txt").map(|v| v.2.clone()),
		Some(b"q1".to_vec()),
		"q.txt not q1"
	);
}

// ============================================================================
// CONVERGE-A5 — Mixed disjoint operations in a single two-way pass
// ============================================================================
#[shared_test_runtime]
async fn converge_a5_mixed_disjoint_ops_single_pass() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "keep.txt", b"k");
	write_file(&sc.local, "del.txt", b"d");
	write_file(&sc.local, "mod.txt", b"m0");
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 3, "{r0:?}");

	// Disjoint mix: local create new.txt, remote delete del.txt, local modify mod.txt.
	write_file(&sc.local, "new.txt", b"n");
	write_file(&sc.local, "mod.txt", b"m1");
	let (_dd, files0) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let mut del_remote = find_file(&files0, "del.txt")
		.expect("del.txt missing on remote")
		.clone();
	sc.cache.client.trash_file(&mut del_remote).await.unwrap();
	assert!(
		poll_for_item_absent(
			sc.cache.db_path(),
			del_remote.uuid(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never dropped trashed del.txt"
	);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "new.txt create + mod.txt update: {r1:?}");
	assert_eq!(r1.locally_deleted, 1, "del.txt removed locally: {r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	assert!(read_eq(&sc.local, "keep.txt", b"k"));
	assert!(read_eq(&sc.local, "new.txt", b"n"));
	assert!(read_eq(&sc.local, "mod.txt", b"m1"));
	assert!(
		!sc.local.join("del.txt").exists(),
		"del.txt still present locally"
	);

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "del.txt").is_none(),
		"del.txt still on remote"
	);
	assert_eq!(
		find_file(&files, "mod.txt").unwrap().size,
		b"m1".len() as u64
	);
	assert!(find_file(&files, "new.txt").is_some());

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A6 — local-backup pushes creates AND updates additively and stably
// ============================================================================
#[shared_test_runtime]
async fn converge_a6_local_backup_positive_convergence() {
	let sc = single_client(SyncMode::LocalBackup).await;
	// Seed an existing synced file.
	write_file(&sc.local, "lbmod.txt", b"m0");
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	// Local create + local update.
	write_file(&sc.local, "lbnew.txt", b"n");
	write_file(&sc.local, "lbmod.txt", b"m1");
	// Independent remote-only create (must NOT be pulled down).
	let ro = upload_to(&sc.cache.client, sc.remote, "rb_only.txt", b"r").await;
	assert!(poll_for_item(sc.cache.db_path(), ro.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "create + update both pushed: {r1:?}");
	assert_eq!(r1.downloaded, 0, "backup must not pull remote-only: {r1:?}");

	// Remote has the pushed create + update.
	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert!(find_file(&files, "lbnew.txt").is_some());
	assert_eq!(
		find_file(&files, "lbmod.txt").unwrap().size,
		b"m1".len() as u64
	);
	assert!(
		find_file(&files, "rb_only.txt").is_some(),
		"remote-only survivor trashed"
	);
	// Local did NOT pull the remote-only file.
	assert!(
		!sc.local.join("rb_only.txt").exists(),
		"backup pulled remote-only down"
	);

	// Stable.
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A7 — File created inside a newly-created directory (parent-before-child)
// ============================================================================
#[shared_test_runtime]
async fn converge_a7_new_dir_and_child_ordering() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "newdir/child.txt", b"c");

	let mut passes = 0usize;
	loop {
		passes += 1;
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {passes}: {r:?}");
		if r.uploaded == 0 && r.remote_dirs_created == 0 {
			break;
		}
		assert!(
			passes < 5,
			"new dir + child did not converge within 5 passes"
		);
	}

	let (root_dirs, _root_files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let newdir = find_dir(&root_dirs, "newdir").expect("newdir missing on remote");
	let (_d, files) = list_dir(&sc.cache.client, newdir).await;
	let child = find_file(&files, "child.txt").expect("newdir/child.txt missing");
	assert_eq!(child.size, b"c".len() as u64);
	sc.cleanup();
}

// ============================================================================
// CONVERGE-A8 — the file lineage survives an edit on a versioning-DISABLED
// account, where the server replaces the file in place instead of archiving a
// version: the successor keeps the lineage id, the retired row is re-stamped
// with a fresh one that names no live file, and the engine must read the two
// as ONE item.
// ============================================================================

/// The `stable_uuid` (lineage id) the cache holds for a file uuid — the field the engine's remote
/// view carries and every lineage rule reads. Read straight from the cache DB: no public helper
/// exposes it, and whether it survives a versioning-disabled edit is the whole point here.
fn cached_stable_uuid(db_path: &std::path::Path, uuid: Uuid) -> Option<StableUuid> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT f.stable_uuid FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
		rusqlite::params![uuid],
		|row| row.get(0),
	)
	.ok()
}

#[shared_test_runtime]
async fn converge_a8_versioning_disabled_edit_keeps_one_lineage() {
	let sc = single_client(SyncMode::TwoWay).await;
	// The same lock order the socket / user / cache tests use for this flag on the shared account:
	// the version-chain lock first, then the account-wide versioning flag.
	let _version_lock = sc
		.resources
		.client
		.acquire_lock_with_default("test:versions")
		.await
		.unwrap();
	let _versioning_lock = sc
		.resources
		.client
		.acquire_lock_with_default("test:user-versioning")
		.await
		.unwrap();
	let original = sc
		.resources
		.client
		.get_user_info()
		.await
		.unwrap()
		.versioning_enabled;
	sc.resources
		.client
		.set_versioning_enabled(false)
		.await
		.unwrap();

	// Put the account-wide flag back whatever the body does, and back to what it WAS: a panic that
	// left it changed would change what every later test on this shared account sees. NOTHING that
	// can fail may sit between the call above and this guard — the read-back that checks the flag
	// took is the body's first act for exactly that reason.
	let outcome = std::panic::AssertUnwindSafe(versioning_disabled_body(&sc))
		.catch_unwind()
		.await;
	sc.resources
		.client
		.set_versioning_enabled(original)
		.await
		.unwrap();
	sc.cleanup();
	if let Err(panic) = outcome {
		std::panic::resume_unwind(panic);
	}
}

async fn versioning_disabled_body(sc: &SingleClient) {
	let db = sc.cache.db_path().to_path_buf();

	assert!(
		!sc.resources
			.client
			.get_user_info()
			.await
			.unwrap()
			.versioning_enabled,
		"precondition: the account must actually be running with versioning off"
	);

	write_file(&sc.local, "vd.txt", b"v1");
	let r0 = sc.sync().await;
	assert!(r0.errors.is_empty(), "{r0:?}");
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	let v1 = find_file(&files, "vd.txt").expect("vd.txt missing").clone();
	assert!(
		poll_for_item(&db, v1.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the cache never listed the uploaded file"
	);
	assert_eq!(
		cached_stable_uuid(&db, v1.uuid()),
		Some(v1.stable_uuid()),
		"precondition: the cache carries the file's lineage id"
	);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 0, "{r1:?}");
	assert_eq!(r1.downloaded, 0, "{r1:?}");

	// backend timestamps have a resolution of one second
	tokio::time::sleep(std::time::Duration::from_secs(2)).await;

	// The edit. With versioning OFF the server does not archive a version: it trashes the old row
	// (re-stamping THAT row with a fresh lineage id, so a stable-keyed consumer cannot tombstone
	// the live file) and announces the successor separately.
	write_file(&sc.local, "vd.txt", b"v2 is longer");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "the local edit pushes: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	let (_d, files) = list_dir(&sc.cache.client, &sc.resources.dir).await;
	assert_eq!(
		files.iter().filter(|f| f.name() == Some("vd.txt")).count(),
		1,
		"a versioning-disabled edit replaces the file in place; two copies means a duplicate"
	);
	let v2 = find_file(&files, "vd.txt").expect("vd.txt missing").clone();
	assert_ne!(v2.uuid(), v1.uuid(), "the edit re-mints the uuid");
	assert_eq!(
		v2.stable_uuid(),
		v1.stable_uuid(),
		"the server keeps the lineage id on the LIVE file"
	);
	assert_eq!(
		sc.cache.client.list_file_versions(&v2).await.unwrap().len(),
		1,
		"precondition: with versioning off the edit replaces in place instead of archiving a \
		 version, which is the code path this test is about"
	);

	// What the ENGINE actually reads. If the ghost's freshly minted id ever landed on the live row,
	// every lineage rule would read the successor as a DIFFERENT file taking the path over.
	assert!(
		poll_for_item(&db, v2.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the cache never listed the successor"
	);
	assert!(
		poll_for_item_absent(&db, v1.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the retired uuid is still in the cache"
	);
	assert_eq!(
		cached_stable_uuid(&db, v2.uuid()),
		Some(v1.stable_uuid()),
		"the cache lost the lineage across a versioning-disabled edit"
	);

	// And the pass that reads it: our own version at the path, one lineage, nothing to do.
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 0, "no re-push: {r3:?}");
	assert_eq!(r3.downloaded, 0, "no spurious re-download: {r3:?}");
	assert_eq!(
		r3.conflicts.len(),
		0,
		"an edit of our own lineage is no conflict: {r3:?}"
	);
	assert_eq!(r3.locally_deleted, 0, "{r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "no delete + recreate: {r3:?}");
	assert_eq!(
		r3.deferred_paths, 0,
		"the retired ghost must not hold the path back: {r3:?}"
	);
	assert!(read_eq(&sc.local, "vd.txt", b"v2 is longer"));
}
