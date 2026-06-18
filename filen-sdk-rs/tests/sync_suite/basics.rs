//! Basics & primitives tests (`BASIC-*`) for the two-way sync engine.
//!
//! These establish that the simplest sync operations work exactly and losslessly: single
//! files/dirs, empty files/dirs, deeply nested trees, byte-exact content, idempotency, initial
//! sync into empty/non-empty destinations, mixed batches, assorted sizes, ordinary names,
//! updates, deletions, renames, path reuse, and the round-trip. Black-box: PUBLIC API only.
use std::{borrow::Cow, sync::Arc, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
		file::RemoteFile,
	},
	sync_engine::{SyncEngine, SyncMode},
};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ----------------------------------------------------------------------------
// Local remote-setup / verification helpers (PUBLIC API only).
//
// `single_client` exposes `cache.client` (the derived client whose cache the engine reads) and
// `resources.dir` (the `RemoteDirectory` sync root). We drive remote state directly via that client
// and verify ground truth by listing the remote dir.
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

/// List the (dirs, files) directly under a remote dir.
async fn list_remote_dir(
	sc: &SingleClient,
	dir: &RemoteDirectory,
) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	sc.cache
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

/// Wait until the cache observes `uuid` (the engine's remote view is the cache).
async fn wait_cache_sees(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// Deterministic pseudo-random bytes of length `n`, seeded by `seed` (for byte-exact size tests).
fn prng_bytes(seed: u64, n: usize) -> Vec<u8> {
	let mut out = Vec::with_capacity(n);
	let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
	for _ in 0..n {
		state ^= state << 13;
		state ^= state >> 7;
		state ^= state << 17;
		out.push((state & 0xFF) as u8);
	}
	out
}

// ============================================================================
// BASIC-01 — single small file pushes/pulls byte-exact
// ============================================================================

#[shared_test_runtime]
async fn basic_01_single_small_file_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let content = b"hello world";
	write_file(&sc.local, "foo.txt", content);
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 1, "{report:?}");
	assert_eq!(report.remote_dirs_created, 0, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	assert!(dirs.is_empty(), "no dirs expected");
	assert_eq!(files.len(), 1, "exactly one remote file");
	let f = find_file(&files, "foo.txt").expect("foo.txt missing on remote");
	assert_eq!(f.size, content.len() as u64, "remote size mismatch");
	// Source unchanged.
	assert!(read_eq(&sc.local, "foo.txt", content), "source mutated");
	sc.cleanup();
}

#[shared_test_runtime]
async fn basic_01_single_small_file_pull() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let content = b"hello world";
	let rf = upload_remote(&sc, "foo.txt", content).await;
	wait_cache_sees(&sc, rf.uuid().into()).await;
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.downloaded, 1, "{report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert!(
		read_eq(&sc.local, "foo.txt", content),
		"local content mismatch"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-02 — zero-byte file
// ============================================================================

#[shared_test_runtime]
async fn basic_02_empty_file_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "empty.dat", b"");
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(
		report.uploaded, 1,
		"zero-byte file must count as 1 upload: {report:?}"
	);

	let (_dirs, files) = list_remote_root(&sc).await;
	let f = find_file(&files, "empty.dat").expect("empty.dat missing on remote");
	assert_eq!(f.size, 0, "zero-byte file should be 0 bytes on remote");
	sc.cleanup();
}

#[shared_test_runtime]
async fn basic_02_empty_file_pull() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_remote(&sc, "empty.dat", b"").await;
	wait_cache_sees(&sc, rf.uuid().into()).await;
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.downloaded, 1, "{report:?}");
	assert!(
		read_eq(&sc.local, "empty.dat", b""),
		"local empty file missing/non-empty"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-03 — single empty directory
// ============================================================================

#[shared_test_runtime]
async fn basic_03_empty_directory_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	std::fs::create_dir_all(sc.local.join("dir1")).unwrap();
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert_eq!(report.remote_dirs_created, 1, "{report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	assert!(files.is_empty(), "no files expected");
	let d = find_dir(&dirs, "dir1").expect("dir1 missing on remote");
	let (sub_dirs, sub_files) = list_remote_dir(&sc, d).await;
	assert!(
		sub_dirs.is_empty() && sub_files.is_empty(),
		"dir1 should be empty"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-04 — deeply nested empty tree
// ============================================================================

#[shared_test_runtime]
async fn basic_04_deep_empty_tree_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	std::fs::create_dir_all(sc.local.join("a/b/c/d/e/f")).unwrap();
	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert_eq!(report.remote_dirs_created, 6, "6 levels: {report:?}");

	// Walk the full chain.
	let (dirs, _files) = list_remote_root(&sc).await;
	let a = find_dir(&dirs, "a").expect("a missing");
	let (ad, _) = list_remote_dir(&sc, a).await;
	let b = find_dir(&ad, "b").expect("a/b missing");
	let (bd, _) = list_remote_dir(&sc, b).await;
	let c = find_dir(&bd, "c").expect("a/b/c missing");
	let (cd, _) = list_remote_dir(&sc, c).await;
	let d = find_dir(&cd, "d").expect("a/b/c/d missing");
	let (dd, _) = list_remote_dir(&sc, d).await;
	let e = find_dir(&dd, "e").expect("a/b/c/d/e missing");
	let (ed, _) = list_remote_dir(&sc, e).await;
	let f = find_dir(&ed, "f").expect("a/b/c/d/e/f missing");
	let (fd, ff) = list_remote_dir(&sc, f).await;
	assert!(fd.is_empty() && ff.is_empty(), "leaf f should be empty");
	sc.cleanup();
}

// ============================================================================
// BASIC-05 — nested tree with files at multiple levels
// ============================================================================

#[shared_test_runtime]
async fn basic_05_nested_files_and_dirs_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "top.txt", content_for("top.txt").as_slice());
	write_file(&sc.local, "a/mid.txt", content_for("a/mid.txt").as_slice());
	write_file(
		&sc.local,
		"a/b/c/leaf.txt",
		content_for("a/b/c/leaf.txt").as_slice(),
	);
	std::fs::create_dir_all(sc.local.join("a/b/empty")).unwrap();

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 3, "3 files: {report:?}");
	// dirs: a, a/b, a/b/c, a/b/empty == 4
	assert_eq!(report.remote_dirs_created, 4, "4 dirs: {report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	let top = find_file(&files, "top.txt").expect("top.txt missing");
	assert_eq!(top.size, content_for("top.txt").len() as u64);
	let a = find_dir(&dirs, "a").expect("a missing");
	let (a_dirs, a_files) = list_remote_dir(&sc, a).await;
	assert!(
		find_file(&a_files, "mid.txt").is_some(),
		"a/mid.txt missing"
	);
	let b = find_dir(&a_dirs, "b").expect("a/b missing");
	let (b_dirs, _) = list_remote_dir(&sc, b).await;
	let empty = find_dir(&b_dirs, "empty").expect("a/b/empty missing");
	let (ed, ef) = list_remote_dir(&sc, empty).await;
	assert!(ed.is_empty() && ef.is_empty(), "a/b/empty should be empty");
	let c = find_dir(&b_dirs, "c").expect("a/b/c missing");
	let (_, c_files) = list_remote_dir(&sc, c).await;
	assert!(
		find_file(&c_files, "leaf.txt").is_some(),
		"a/b/c/leaf.txt missing"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-06 — idempotency: a second no-change pass is a strict no-op
// ============================================================================

#[shared_test_runtime]
async fn basic_06_idempotent_second_pass_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "f1.txt", b"one");
	write_file(&sc.local, "sub/f2.txt", b"two");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.local_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.moved_local, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert_eq!(r2.held_deletions, 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// BASIC-07 — initial sync of a fully empty pair is a clean no-op
// ============================================================================

#[shared_test_runtime]
async fn basic_07_empty_pair_noop_local_to_remote() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 0, "{r1:?}");
	assert_eq!(r1.downloaded, 0, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 0, "{r1:?}");
	assert_eq!(r1.local_dirs_created, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	// Both sides empty; baseline established so a second pass is also a no-op.
	assert!(walk_tree(&sc.local).is_empty(), "spurious local content");
	let (dirs, files) = list_remote_root(&sc).await;
	assert!(
		dirs.is_empty() && files.is_empty(),
		"spurious remote content"
	);

	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");
	sc.cleanup();
}

#[shared_test_runtime]
async fn basic_07_empty_pair_noop_two_way() {
	let sc = single_client(SyncMode::TwoWay).await;
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 0, "{r1:?}");
	assert_eq!(r1.downloaded, 0, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 0, "{r1:?}");
	assert_eq!(r1.local_dirs_created, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	assert!(walk_tree(&sc.local).is_empty(), "spurious local content");
	let (dirs, files) = list_remote_root(&sc).await;
	assert!(
		dirs.is_empty() && files.is_empty(),
		"spurious remote content"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-08 — initial sync into a non-empty destination does not wipe it
// ============================================================================

// LocalToRemote: source = local (A.txt, B.txt); destination = remote which already independently
// holds pre-existing.txt and dir keep/. The first/baseline-less pass must NOT trash the
// pre-existing remote items just because they are absent from the source.
#[shared_test_runtime]
async fn basic_08_nonempty_destination_not_wiped_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	// Pre-existing remote content (independent of the source).
	let pre = upload_remote(&sc, "pre-existing.txt", b"precious data").await;
	let keep = sc
		.cache
		.client
		.create_dir(&sc_root(&sc), "keep")
		.await
		.unwrap();
	wait_cache_sees(&sc, pre.uuid().into()).await;
	wait_cache_sees(&sc, keep.uuid().into()).await;

	// Source-side files.
	write_file(&sc.local, "A.txt", b"aaa");
	write_file(&sc.local, "B.txt", b"bbb");

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	// Crucial: the absent-from-source pre-existing items must NOT be destroyed.
	assert_eq!(
		report.remotely_trashed, 0,
		"first sync must not trash pre-existing destination content: {report:?}"
	);

	let (dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "pre-existing.txt").is_some(),
		"pre-existing.txt wiped!"
	);
	assert_eq!(
		find_file(&files, "pre-existing.txt").unwrap().size,
		b"precious data".len() as u64,
		"pre-existing content changed"
	);
	assert!(find_dir(&dirs, "keep").is_some(), "keep/ wiped!");
	assert!(find_file(&files, "A.txt").is_some(), "A.txt not pushed");
	assert!(find_file(&files, "B.txt").is_some(), "B.txt not pushed");
	sc.cleanup();
}

// RemoteToLocal: source = remote (A.txt, B.txt); destination = local which already holds
// pre-existing.txt and dir keep/. The baseline-less pull must NOT delete them.
#[shared_test_runtime]
async fn basic_08_nonempty_destination_not_wiped_pull() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	// Pre-existing LOCAL destination content.
	write_file(&sc.local, "pre-existing.txt", b"precious data");
	std::fs::create_dir_all(sc.local.join("keep")).unwrap();

	// Source-side remote files.
	let a = upload_remote(&sc, "A.txt", b"aaa").await;
	let b = upload_remote(&sc, "B.txt", b"bbb").await;
	wait_cache_sees(&sc, a.uuid().into()).await;
	wait_cache_sees(&sc, b.uuid().into()).await;

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(
		report.locally_deleted, 0,
		"first sync must not delete pre-existing local content: {report:?}"
	);

	assert!(
		read_eq(&sc.local, "pre-existing.txt", b"precious data"),
		"pre-existing.txt wiped!"
	);
	assert!(sc.local.join("keep").is_dir(), "keep/ wiped!");
	assert!(read_eq(&sc.local, "A.txt", b"aaa"), "A.txt not pulled");
	assert!(read_eq(&sc.local, "B.txt", b"bbb"), "B.txt not pulled");
	sc.cleanup();
}

// ============================================================================
// BASIC-09 — mixed batch of creates in a single pass
// ============================================================================

#[shared_test_runtime]
async fn basic_09_mixed_batch_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// 3 root files
	write_file(&sc.local, "r1.txt", b"r1");
	write_file(&sc.local, "r2.txt", b"r2");
	write_file(&sc.local, "r3.txt", b"r3");
	// 2 dirs each with a file
	write_file(&sc.local, "d1/in1.txt", b"in1");
	write_file(&sc.local, "d2/in2.txt", b"in2");
	// 1 empty dir
	std::fs::create_dir_all(sc.local.join("d3_empty")).unwrap();

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 5, "5 files: {report:?}");
	assert_eq!(report.remote_dirs_created, 3, "3 dirs: {report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	for n in ["r1.txt", "r2.txt", "r3.txt"] {
		assert!(find_file(&files, n).is_some(), "{n} missing");
	}
	for n in ["d1", "d2", "d3_empty"] {
		assert!(find_dir(&dirs, n).is_some(), "{n} missing");
	}
	let d1 = find_dir(&dirs, "d1").unwrap();
	assert!(find_file(&list_remote_dir(&sc, d1).await.1, "in1.txt").is_some());
	let d3 = find_dir(&dirs, "d3_empty").unwrap();
	let (ed, ef) = list_remote_dir(&sc, d3).await;
	assert!(ed.is_empty() && ef.is_empty(), "d3_empty should be empty");
	sc.cleanup();
}

// ============================================================================
// BASIC-10 — assorted file sizes incl. chunk boundaries (byte-exact)
// ============================================================================

#[shared_test_runtime]
async fn basic_10_assorted_sizes_roundtrip() {
	// Push from local A, then pull on a separate remote->local pair and compare byte-exact.
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();

	let sizes: &[usize] = &[
		1,
		1023,
		1024,
		(1 << 20) - 1,
		1 << 20,
		(1 << 20) + 1,
		3 << 20,
	];
	let local_a = fresh_local_dir("sizes_a");
	for (i, &n) in sizes.iter().enumerate() {
		write_file(
			&local_a,
			&format!("size_{i}.bin"),
			&prng_bytes(i as u64 + 1, n),
		);
	}

	// Push side.
	let cache_a = TestCache::new(&resources.client, remote).await;
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let rpush = engine_a.sync_once(pair_a).await.unwrap();
	assert!(rpush.errors.is_empty(), "push errors: {rpush:?}");
	assert_eq!(rpush.uploaded, sizes.len(), "all sizes uploaded: {rpush:?}");

	// Pull side (separate client).
	let cache_b = TestCache::new(&resources.client, remote).await;
	let expected_items = 1 + sizes.len();
	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		count_items(cache_b.db_path()) >= expected_items
	})
	.await;

	let local_b = fresh_local_dir("sizes_b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let rpull = engine_b.sync_once(pair_b).await.unwrap();
	assert!(rpull.errors.is_empty(), "pull errors: {rpull:?}");
	assert_eq!(
		rpull.downloaded,
		sizes.len(),
		"all sizes downloaded: {rpull:?}"
	);

	assert_trees_identical(&local_a, &local_b, "local_a", "local_b");
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ============================================================================
// BASIC-11 — ordinary names with spaces, dots, unicode round-trip exactly
// ============================================================================

// Note: the case-collision sub-case ('Folder One' vs 'folder one') needs the `malformed`-feature
// mismatched-hash bypass to even materialize two case-colliding items on the real (case-insensitive
// dedup) remote — that is exercised in the security/paths categories with that feature. Here we
// assert the non-colliding name-fidelity round-trip, which the current harness supports.
#[shared_test_runtime]
async fn basic_11_ordinary_names_roundtrip() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();

	let names: &[&str] = &[
		"my file.txt",
		"archive.tar.gz",
		".hidden",
		"résumé.txt",
		"日本語.txt",
		"README",
	];
	let dirnames: &[&str] = &["Folder One"];

	let local_a = fresh_local_dir("names_a");
	for n in names {
		write_file(&local_a, n, content_for(n).as_slice());
	}
	for d in dirnames {
		std::fs::create_dir_all(local_a.join(d)).unwrap();
	}

	let cache_a = TestCache::new(&resources.client, remote).await;
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let rpush = engine_a.sync_once(pair_a).await.unwrap();
	assert!(rpush.errors.is_empty(), "push errors: {rpush:?}");
	assert_eq!(rpush.uploaded, names.len(), "{rpush:?}");
	assert_eq!(rpush.remote_dirs_created, dirnames.len(), "{rpush:?}");

	let cache_b = TestCache::new(&resources.client, remote).await;
	let expected_items = 1 + names.len() + dirnames.len();
	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		count_items(cache_b.db_path()) >= expected_items
	})
	.await;

	let local_b = fresh_local_dir("names_b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let rpull = engine_b.sync_once(pair_b).await.unwrap();
	assert!(rpull.errors.is_empty(), "pull errors: {rpull:?}");

	// Every name appears byte-identically on the pulled side with byte-exact content.
	for n in names {
		assert!(
			read_eq(&local_b, n, content_for(n).as_slice()),
			"name {n:?} not round-tripped exactly"
		);
	}
	for d in dirnames {
		assert!(local_b.join(d).is_dir(), "dir {d:?} not round-tripped");
	}
	assert_trees_identical(&local_a, &local_b, "local_a", "local_b");
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ============================================================================
// BASIC-12 — modify existing synced file -> update propagates (not duplicated)
// ============================================================================

#[shared_test_runtime]
async fn basic_12_modify_propagates_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "foo.txt", b"version one");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	write_file(&sc.local, "foo.txt", b"version two is much longer than one");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "modified file re-uploads: {r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(
		files.len(),
		1,
		"exactly one foo.txt (versioned in place): {}",
		files.len()
	);
	let f = find_file(&files, "foo.txt").expect("foo.txt missing");
	assert_eq!(
		f.size,
		b"version two is much longer than one".len() as u64,
		"stale content"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-13 — truncate-to-empty update
// ============================================================================

#[shared_test_runtime]
async fn basic_13_truncate_to_empty_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let content = prng_bytes(7, 4096);
	write_file(&sc.local, "data.bin", &content);
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	write_file(&sc.local, "data.bin", b"");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "truncate is an update: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "truncate is not a delete: {r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	let f = find_file(&files, "data.bin").expect("data.bin gone (treated as delete?)");
	assert_eq!(f.size, 0, "should be 0 bytes after truncate");
	sc.cleanup();
}

// ============================================================================
// BASIC-14 — grow an empty file to non-empty
// ============================================================================

#[shared_test_runtime]
async fn basic_14_grow_empty_file_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "empty.dat", b"");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	let content = prng_bytes(11, 2048);
	write_file(&sc.local, "empty.dat", &content);
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "grown file re-uploads: {r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 1, "still one file");
	let f = find_file(&files, "empty.dat").expect("empty.dat missing");
	assert_eq!(f.size, content.len() as u64, "grown content not propagated");
	sc.cleanup();
}

// ============================================================================
// BASIC-15 — baseline persists across process restart (no re-transfer)
// ============================================================================

// Simulate a clean restart by dropping the engine and opening a NEW SyncEngine on the SAME baseline
// DB path and re-adding the same pair. A no-change pass after restart must be a strict no-op,
// proving the baseline was loaded from persistence (not reset to empty).
#[shared_test_runtime]
async fn basic_15_baseline_persists_across_restart() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;

	let local = fresh_local_dir("restart");
	write_file(&local, "x.txt", b"x");
	write_file(&local, "y/y.txt", b"y");
	write_file(&local, "z.txt", b"z");

	let baseline_db = temp_cache_path();
	{
		let engine = SyncEngine::open(cache.client.clone(), baseline_db.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
			.await
			.unwrap();
		let r1 = engine.sync_once(pair).await.unwrap();
		assert!(r1.errors.is_empty(), "{r1:?}");
		assert_eq!(r1.uploaded, 3, "{r1:?}");
		// engine drops here -> "process exit"
	}

	let tree_before = walk_tree(&local);

	// "Restart": fresh engine on the SAME baseline DB.
	let engine2 = SyncEngine::open(cache.client.clone(), baseline_db.clone())
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "no re-upload after restart: {r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	assert_eq!(
		walk_tree(&local),
		tree_before,
		"local tree changed across restart"
	);
	std::fs::remove_dir_all(&local).ok();
}

// ============================================================================
// BASIC-16 — interrupted pass re-run reaches converged end-state (resumability)
// ============================================================================

#[ignore = "blocked: needs fault-injection harness to deterministically interrupt a pass mid-apply (after some but not all actions, before baseline advance) — no public API for it; see TODO"]
#[shared_test_runtime]
async fn basic_16_interrupted_pass_resumes() {
	// plan: empty baseline + a source tree of several files incl. a large one; begin a pass and
	// forcibly interrupt mid-apply; re-run a fresh pass; assert destination == source byte-exact,
	// nothing duplicated/corrupted/half-written, and a subsequent pass is a no-op.
}

// ============================================================================
// BASIC-17 — idempotency after a real change: pass N applies, N+1 is no-op
// ============================================================================

#[ignore = "blocked: create-idempotency under cache lag (deferred review finding #8). After the \
engine uploads a file it advances its baseline, but the next pass rebuilds the remote snapshot from \
the cache, which may not yet reflect the just-created item — so in a pushes()+propagates_deletes() \
mode the file reads as a remote-side deletion and is re-uploaded. Needs the snapshot to fold in \
just-applied creates (or wait for the cache to catch up) before the next pass. TODO"]
#[shared_test_runtime]
async fn basic_17_idempotent_after_real_change_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "existing.txt", b"already here");
	let r0 = sc.sync().await;
	assert_eq!(r0.uploaded, 1, "{r0:?}");

	// Single change: add one file.
	write_file(&sc.local, "added.txt", b"newly added");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "exactly one create: {r1:?}");

	let r2 = sc.sync().await;
	assert_eq!(
		r2.uploaded, 0,
		"just-added file must not re-transfer: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "added.txt").is_some(),
		"added.txt missing"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-18 — file and directory of distinct names coexist at same parent
// ============================================================================

#[shared_test_runtime]
async fn basic_18_file_and_dir_distinct_names_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "item", b"i am a file");
	std::fs::create_dir_all(sc.local.join("item.d")).unwrap();
	write_file(&sc.local, "report", b"a report file");
	std::fs::create_dir_all(sc.local.join("reports")).unwrap();

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 2, "two files: {report:?}");
	assert_eq!(report.remote_dirs_created, 2, "two dirs: {report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	let item = find_file(&files, "item").expect("file 'item' missing");
	assert_eq!(item.size, b"i am a file".len() as u64);
	assert!(find_dir(&dirs, "item.d").is_some(), "dir 'item.d' missing");
	let rep = find_file(&files, "report").expect("file 'report' missing");
	assert_eq!(rep.size, b"a report file".len() as u64);
	assert!(
		find_dir(&dirs, "reports").is_some(),
		"dir 'reports' missing"
	);
	// No type confusion.
	assert!(
		find_dir(&dirs, "item").is_none(),
		"'item' should not be a dir"
	);
	assert!(
		find_file(&files, "item.d").is_none(),
		"'item.d' should not be a file"
	);
	sc.cleanup();
}

// ============================================================================
// BASIC-19 — identical content at distinct paths each sync independently
// ============================================================================

#[shared_test_runtime]
async fn basic_19_identical_content_distinct_paths_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let shared = prng_bytes(42, 1500);
	write_file(&sc.local, "copy1.bin", &shared);
	write_file(&sc.local, "sub/copy2.bin", &shared);
	write_file(&sc.local, "sub/deep/copy3.bin", &shared);

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(
		report.uploaded, 3,
		"3 distinct files (no dedup-collapse): {report:?}"
	);
	assert_eq!(report.remote_dirs_created, 2, "sub, sub/deep: {report:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	let c1 = find_file(&files, "copy1.bin").expect("copy1.bin missing");
	let sub = find_dir(&dirs, "sub").expect("sub missing");
	let (sub_dirs, sub_files) = list_remote_dir(&sc, sub).await;
	let c2 = find_file(&sub_files, "copy2.bin").expect("copy2.bin missing");
	let deep = find_dir(&sub_dirs, "deep").expect("sub/deep missing");
	let (_, deep_files) = list_remote_dir(&sc, deep).await;
	let c3 = find_file(&deep_files, "copy3.bin").expect("copy3.bin missing");
	for f in [c1, c2, c3] {
		assert_eq!(f.size, shared.len() as u64, "size mismatch on a copy");
	}
	sc.cleanup();
}

// ============================================================================
// BASIC-20 — large directory with many small files in one pass (fan-out)
// ============================================================================

#[shared_test_runtime]
async fn basic_20_fanout_many_files_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const N: usize = 500;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("fan/f{i:04}.txt"),
			format!("content-{i}").as_bytes(),
		);
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "errors: {r1:?}");
	assert_eq!(r1.uploaded, N, "all {N} files uploaded: {r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "one dir: {r1:?}");

	let (dirs, _files) = list_remote_root(&sc).await;
	let fan = find_dir(&dirs, "fan").expect("fan dir missing");
	let (_, fan_files) = list_remote_dir(&sc, fan).await;
	assert_eq!(
		fan_files.len(),
		N,
		"all {N} files present on remote, got {}",
		fan_files.len()
	);
	// Spot-check a few.
	for i in [0usize, 123, 499] {
		let f = find_file(&fan_files, &format!("f{i:04}.txt")).expect("a fan file missing");
		assert_eq!(f.size, format!("content-{i}").len() as u64);
	}

	// Second pass is a no-op.
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "fan-out second pass must be no-op: {r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	sc.cleanup();
}

// ============================================================================
// BASIC-21 — binary content with nulls and high bytes preserved exactly
// ============================================================================

#[shared_test_runtime]
async fn basic_21_binary_nulls_high_bytes_roundtrip() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();

	// All 256 byte values repeated to ~64 KiB.
	let mut blob = Vec::with_capacity(64 * 1024);
	while blob.len() < 64 * 1024 {
		blob.extend(0u8..=255);
	}
	blob.truncate(64 * 1024);

	let local_a = fresh_local_dir("blob_a");
	write_file(&local_a, "blob.bin", &blob);

	let cache_a = TestCache::new(&resources.client, remote).await;
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let rpush = engine_a.sync_once(pair_a).await.unwrap();
	assert!(rpush.errors.is_empty(), "{rpush:?}");
	assert_eq!(rpush.uploaded, 1, "{rpush:?}");

	let cache_b = TestCache::new(&resources.client, remote).await;
	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		count_items(cache_b.db_path()) >= 2
	})
	.await;

	let local_b = fresh_local_dir("blob_b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let rpull = engine_b.sync_once(pair_b).await.unwrap();
	assert!(rpull.errors.is_empty(), "{rpull:?}");
	assert_eq!(rpull.downloaded, 1, "{rpull:?}");

	// Byte-for-byte identical, full comparison (not just length).
	let pulled = std::fs::read(local_b.join("blob.bin")).unwrap();
	assert_eq!(pulled.len(), blob.len(), "length drift");
	assert_eq!(
		pulled, blob,
		"binary content altered (NUL truncation / encoding translation?)"
	);
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ============================================================================
// BASIC-22 — single directory rename preserves children without re-upload
// ============================================================================

#[shared_test_runtime]
async fn basic_22_dir_rename_preserves_children_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(
		&sc.local,
		"old/c0.txt",
		content_for("old/c0.txt").as_slice(),
	);
	write_file(
		&sc.local,
		"old/c1.txt",
		content_for("old/c1.txt").as_slice(),
	);
	write_file(
		&sc.local,
		"old/c2.txt",
		content_for("old/c2.txt").as_slice(),
	);
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	// Capture original child uuids.
	let (dirs0, _) = list_remote_root(&sc).await;
	let old0 = find_dir(&dirs0, "old").expect("old missing");
	let (_, old_files0) = list_remote_dir(&sc, old0).await;
	let orig_uuids: Vec<_> = old_files0.iter().map(|f| f.uuid()).collect();
	assert_eq!(orig_uuids.len(), 3);

	// Rename old/ -> new/ (children unchanged).
	move_file(&sc.local, "old", "new");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Children content must NOT be re-uploaded.
	assert_eq!(
		r2.uploaded, 0,
		"rename must not re-upload child bytes: {r2:?}"
	);

	let (dirs1, _) = list_remote_root(&sc).await;
	assert!(find_dir(&dirs1, "old").is_none(), "old/ still exists");
	let new1 = find_dir(&dirs1, "new").expect("new/ missing");
	let (_, new_files) = list_remote_dir(&sc, new1).await;
	assert_eq!(new_files.len(), 3, "all 3 children must survive the rename");
	for c in ["c0.txt", "c1.txt", "c2.txt"] {
		let f = find_file(&new_files, c).expect("child missing after rename");
		assert_eq!(f.size, content_for(&format!("old/{c}")).len() as u64);
	}

	// Subsequent pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	sc.cleanup();
}

// ============================================================================
// BASIC-23 — backup modes do NOT mirror a source deletion (additive)
// ============================================================================

#[shared_test_runtime]
async fn basic_23_local_backup_does_not_mirror_delete() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "a.txt", b"aaa");
	write_file(&sc.local, "b.txt", b"bbb");
	write_file(&sc.local, "c.txt", b"ccc");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	// Delete one local file; LocalBackup must NOT mirror the deletion to the remote.
	std::fs::remove_file(sc.local.join("b.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not trash on source delete: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions, 0,
		"backup absorbs the delete, not holds it: {r2:?}"
	);

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 3, "remote must retain all 3 files");
	let b = find_file(&files, "b.txt").expect("deleted-locally file must survive on remote backup");
	assert_eq!(b.size, b"bbb".len() as u64, "retained content changed");

	// Subsequent pass is a no-op (the deletion was absorbed into baseline).
	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	sc.cleanup();
}

// ============================================================================
// BASIC-24 — final-report accuracy for a mixed pass (report implies reality)
// ============================================================================

// The public API used here exposes the FINAL `SyncReport` counts; the per-action progress-event
// stream is exercised via the observed-pass API in the `observability` category. What we assert
// black-box: the final report counts match the actual destination end-state exactly, and every
// item reported created truly exists and is byte-exact (reported success implies real success — no
// false success hiding data loss).
#[shared_test_runtime]
async fn basic_24_report_matches_destination_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// 4 files of varied sizes + 2 dirs.
	write_file(&sc.local, "tiny.txt", &prng_bytes(1, 1));
	write_file(&sc.local, "small.txt", &prng_bytes(2, 1000));
	write_file(&sc.local, "d1/med.bin", &prng_bytes(3, 50_000));
	write_file(&sc.local, "d2/big.bin", &prng_bytes(4, 2 << 20));

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 4, "{report:?}");
	assert_eq!(report.remote_dirs_created, 2, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	// Reported success implies actual success: each reported item truly exists, byte-exact size.
	let (dirs, files) = list_remote_root(&sc).await;
	assert_eq!(dirs.len(), 2, "exactly the 2 reported dirs");
	for n in ["tiny.txt", "small.txt"] {
		assert!(find_file(&files, n).is_some(), "{n} reported but missing");
	}
	let d1 = find_dir(&dirs, "d1").unwrap();
	let (_, d1_files) = list_remote_dir(&sc, d1).await;
	let med = find_file(&d1_files, "med.bin").expect("med.bin missing");
	assert_eq!(med.size, 50_000, "med.bin size mismatch");
	let d2 = find_dir(&dirs, "d2").unwrap();
	let (_, d2_files) = list_remote_dir(&sc, d2).await;
	let big = find_file(&d2_files, "big.bin").expect("big.bin missing");
	assert_eq!(big.size, 2u64 << 20, "big.bin size mismatch");
	sc.cleanup();
}

// ============================================================================
// BASIC-25 — engine's own writes in watch mode do not loop forever
// ============================================================================

#[shared_test_runtime]
async fn basic_25_watch_no_self_resync_loop() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;

	let local = fresh_local_dir("watchloop");
	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let handle = engine.clone().watch(pair).await.unwrap();

	// Create exactly one file under the watch.
	let content = b"single payload";
	write_file(&local, "watched.txt", content);

	// Wait until it reaches the remote (ground truth).
	let dirtype = DirType::<Normal>::Dir(Cow::Borrowed(&resources.dir));
	let mut pushed = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_d, files) = resources
			.client
			.list_dir(&dirtype, None::<&fn(u64, Option<u64>)>)
			.await
			.unwrap();
		if files
			.iter()
			.find(|f| f.name() == Some("watched.txt"))
			.map(|f| f.size)
			== Some(content.len() as u64)
		{
			pushed = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(pushed, "watch did not push the single file");

	// Quiet period past the debounce + one periodic safety-net interval, making NO further change.
	// If the engine reacted to its own write it would version watched.txt (size unchanged but a new
	// uuid) repeatedly — we assert the remote stays at exactly one watched.txt afterward.
	tokio::time::sleep(Duration::from_secs(20)).await;

	let (_d, files) = resources
		.client
		.list_dir(&dirtype, None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	let matching: Vec<_> = files
		.iter()
		.filter(|f| f.name() == Some("watched.txt"))
		.collect();
	assert_eq!(
		matching.len(),
		1,
		"self-resync loop produced extra copies: {}",
		matching.len()
	);
	assert_eq!(matching[0].size, content.len() as u64, "content drifted");

	// Stop watch, then a manual no-op pass must report 0 actions (steady state).
	drop(handle);
	let r = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r.uploaded, 0,
		"manual pass after quiesce should be no-op: {r:?}"
	);
	assert_eq!(r.remotely_trashed, 0, "{r:?}");
	assert_eq!(r.moved_remote, 0, "{r:?}");
	assert!(r.errors.is_empty(), "{r:?}");

	std::fs::remove_dir_all(&local).ok();
}

// ============================================================================
// (review-add) — delete a synced file -> deletion propagates in mirroring modes
// ============================================================================

#[shared_test_runtime]
async fn basic_add_delete_propagates_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "a.txt", b"aaa");
	write_file(&sc.local, "b.txt", b"bbb");
	write_file(&sc.local, "c.txt", b"ccc");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	std::fs::remove_file(sc.local.join("b.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.remotely_trashed, 1, "exactly one delete: {r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(
		r2.held_deletions, 0,
		"1 of 3 is below the mass-delete floor: {r2:?}"
	);
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "b.txt").is_none(),
		"b.txt not deleted on remote"
	);
	assert!(find_file(&files, "a.txt").is_some(), "a.txt lost");
	assert!(find_file(&files, "c.txt").is_some(), "c.txt lost");

	let r3 = sc.sync().await;
	assert_eq!(
		r3.remotely_trashed, 0,
		"delete absorbed into baseline: {r3:?}"
	);
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	sc.cleanup();
}

// ============================================================================
// (review-add) — delete a synced empty directory -> removal propagates
// ============================================================================

#[shared_test_runtime]
async fn basic_add_empty_dir_delete_propagates_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	std::fs::create_dir_all(sc.local.join("dir1")).unwrap();
	write_file(&sc.local, "sibling.txt", b"keep me");
	let r1 = sc.sync().await;
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	std::fs::remove_dir(sc.local.join("dir1")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.remotely_trashed, 1, "one dir removal: {r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_dir(&dirs, "dir1").is_none(),
		"dir1 not removed on remote"
	);
	assert!(
		find_file(&files, "sibling.txt").is_some(),
		"sibling affected"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	sc.cleanup();
}

// ============================================================================
// (review-add) — full round-trip integrity: push then independently pull back
// ============================================================================

#[shared_test_runtime]
async fn basic_add_full_roundtrip_integrity() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid().into();

	// Mixed tree: varied sizes + a binary blob.
	let local_a = fresh_local_dir("rt_a");
	write_file(&local_a, "doc.txt", b"a small document");
	write_file(&local_a, "sub/notes.md", &prng_bytes(5, 8192));
	let mut blob = Vec::new();
	while blob.len() < 40 * 1024 {
		blob.extend(0u8..=255);
	}
	blob.truncate(40 * 1024);
	write_file(&local_a, "sub/deep/blob.bin", &blob);

	let cache_a = TestCache::new(&resources.client, remote).await;
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let rpush = engine_a.sync_once(pair_a).await.unwrap();
	assert!(rpush.errors.is_empty(), "{rpush:?}");
	assert_eq!(rpush.uploaded, 3, "{rpush:?}");

	// Independent pull side, empty local + empty baseline.
	let cache_b = TestCache::new(&resources.client, remote).await;
	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		count_items(cache_b.db_path()) >= 1 + 2 + 3
	})
	.await;

	let local_b = fresh_local_dir("rt_b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let rpull = engine_b.sync_once(pair_b).await.unwrap();
	assert!(rpull.errors.is_empty(), "{rpull:?}");
	assert_eq!(rpull.downloaded, 3, "{rpull:?}");

	// The full export+re-import cycle must preserve everything byte-for-byte.
	assert_trees_identical(&local_a, &local_b, "pushed", "pulled");
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ============================================================================
// (review-add) — remote->local pull byte-exactness explicitly (pull path real)
// ============================================================================

#[shared_test_runtime]
async fn basic_add_remote_origin_pull_byte_exact() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let txt = b"hello world";
	let blob = (0u8..=255).collect::<Vec<u8>>();
	let f1 = upload_remote(&sc, "foo.txt", txt).await;
	let f2 = upload_remote(&sc, "blob.bin", &blob).await;
	wait_cache_sees(&sc, f1.uuid().into()).await;
	wait_cache_sees(&sc, f2.uuid().into()).await;

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.downloaded, 2, "{report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");

	// Local byte-exact (full content, not just length).
	assert_eq!(std::fs::read(sc.local.join("foo.txt")).unwrap(), txt);
	assert_eq!(std::fs::read(sc.local.join("blob.bin")).unwrap(), blob);
	// No partial/placeholder leftover beyond the two files.
	let tree = walk_tree(&sc.local);
	assert_eq!(
		tree.len(),
		2,
		"unexpected local entries: {:?}",
		tree.keys().collect::<Vec<_>>()
	);

	// Remote source unchanged.
	let (_d, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 2, "remote source changed");
	sc.cleanup();
}

// ============================================================================
// (review-add) — update propagates from remote -> local (single-sided, no conflict)
// ============================================================================

#[shared_test_runtime]
async fn basic_add_remote_update_pulls_down_two_way() {
	let sc = single_client(SyncMode::TwoWay).await;
	let v1 = b"version one";
	let rf = upload_remote(&sc, "foo.txt", v1).await;
	wait_cache_sees(&sc, rf.uuid().into()).await;
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "foo.txt", v1), "v1 not pulled");

	// Modify on the REMOTE only (re-upload same name -> server versions to a new uuid).
	let v2 = b"version two is noticeably longer";
	let new_rf = upload_remote(&sc, "foo.txt", v2).await;
	let new_uuid: Uuid = new_rf.uuid().into();
	wait_cache_sees(&sc, new_uuid).await;
	let db = sc.cache.db_path().to_path_buf();
	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		query_cached_file(&db, new_uuid).map(|t| t.1) == Some(v2.len() as i64)
	})
	.await;

	// Bounded retry around the eventual-consistency window.
	let mut r2 = sc.sync().await;
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while r2.downloaded == 0
		&& r2
			.errors
			.iter()
			.any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(Duration::from_millis(500)).await;
		r2 = sc.sync().await;
	}
	assert_eq!(r2.downloaded, 1, "remote update must pull down: {r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"single-sided change is not a conflict: {r2:?}"
	);
	assert!(read_eq(&sc.local, "foo.txt", v2), "local not updated to v2");

	// Only one foo.txt locally.
	let tree = walk_tree(&sc.local);
	assert_eq!(
		tree.len(),
		1,
		"duplicate/sidecar produced: {:?}",
		tree.keys().collect::<Vec<_>>()
	);
	sc.cleanup();
}

// ============================================================================
// (review-add) — re-create a file at a path previously synced then deleted
// ============================================================================

#[shared_test_runtime]
async fn basic_add_recreate_after_delete_push() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "foo.txt", b"original content");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// The engine resolves the trash target through its cache, so wait for the cache to observe the
	// upload: against a lagging cache the delete pass correctly defers instead of trashing.
	let (_d, files1) = list_remote_root(&sc).await;
	let uploaded = find_file(&files1, "foo.txt").expect("uploaded foo.txt missing");
	wait_cache_sees(&sc, uploaded.uuid()).await;

	// Delete and sync so the deletion is in baseline.
	std::fs::remove_file(sc.local.join("foo.txt")).unwrap();
	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 1, "{r2:?}");
	let (_d, files2) = list_remote_root(&sc).await;
	assert!(
		find_file(&files2, "foo.txt").is_none(),
		"foo.txt not deleted"
	);

	// Re-create a NEW foo.txt with different content at the same path.
	write_file(&sc.local, "foo.txt", b"brand new different content here");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.uploaded, 1,
		"re-created file must upload (1 create): {r3:?}"
	);
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");

	let (_d, files3) = list_remote_root(&sc).await;
	let f = find_file(&files3, "foo.txt").expect("re-created foo.txt missing");
	assert_eq!(
		f.size,
		b"brand new different content here".len() as u64,
		"old content resurrected"
	);

	// Subsequent pass is a no-op.
	let r4 = sc.sync().await;
	assert_eq!(r4.uploaded, 0, "{r4:?}");
	assert_eq!(r4.remotely_trashed, 0, "{r4:?}");
	sc.cleanup();
}
