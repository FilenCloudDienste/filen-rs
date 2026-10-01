//! Scale & performance tests (`SCALE-*`) for the two-way sync engine.
//!
//! These are LIVE tests against the real shared account, so the item COUNTS here are deliberately
//! scaled DOWN from the design doc's 50k/100k/multi-GB targets to a small-but-representative size
//! (a few dozen items, a few KB each). What is preserved is the STRUCTURE of each guarantee:
//! exact report counts (no double-counting), byte-exactness across the whole tree, the fast-path
//! no-op second pass after a baseline, move-not-reupload detection on large batches, the
//! mass-delete hold, quarantine recoverability, backup-mode additivity, baseline reload across an
//! engine "restart", and bounded/ordered progress events. Tests whose load-bearing claim is a true
//! resource bound (multi-GB memory ceiling, request-rate/429 instrumentation, deterministic
//! crash/interrupt or transient-failure injection, debounce-window timing) require fault-injection
//! / instrumentation harnesses that do not exist yet and are `#[ignore]`d with a plan stub.
use std::{
	borrow::Cow,
	collections::BTreeSet,
	path::Path,
	sync::{
		Arc,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::{
	HasName, HasUUID,
	categories::{DirType, Normal},
	dir::RemoteDirectory,
	file::RemoteFile,
};
use filen_sdk_rs::sync_engine::{SyncEngine, SyncEvent, SyncMode, WatchConfig};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

/// A generous settle window for a watch to react to one burst (debounce + a pass + cache observe).
const WATCH_SETTLE: Duration = Duration::from_secs(180);

// ----------------------------------------------------------------------------
// Local remote-state helpers (public-API only; mirror the example test files).
// All remote setup/verify goes through `sc.resources.client` (the base client whose writes the
// derived `sc.cache` observes); cache-convergence polling uses `sc.cache.db_path()` / messages.
// ----------------------------------------------------------------------------

fn root_dirtype(sc: &SingleClient) -> DirType<'_, Normal> {
	DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir))
}

async fn upload_root(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
	let builder = sc
		.resources
		.client
		.make_file_builder(name, sc.resources.dir.uuid())
		.unwrap();
	sc.resources
		.client
		.upload_file(builder, data)
		.await
		.unwrap()
}

async fn create_root_dir(sc: &SingleClient, name: &str) -> RemoteDirectory {
	sc.resources
		.client
		.create_dir(&root_dirtype(sc), name)
		.await
		.unwrap()
}

async fn upload_into(
	sc: &SingleClient,
	parent: &RemoteDirectory,
	name: &str,
	data: &[u8],
) -> RemoteFile {
	let builder = sc
		.resources
		.client
		.make_file_builder(name, parent.uuid())
		.unwrap();
	sc.resources
		.client
		.upload_file(builder, data)
		.await
		.unwrap()
}

async fn list_root(sc: &SingleClient) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	sc.resources
		.client
		.list_dir(&root_dirtype(sc), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
}

async fn list_dir(
	sc: &SingleClient,
	dir: &RemoteDirectory,
) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	sc.resources
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

/// Count regular files recursively under `root` (excluding the quarantine bin).
fn count_files(root: &Path) -> usize {
	walk_tree(root)
		.values()
		.filter(|(is_dir, ..)| !is_dir)
		.count()
}

/// Files in the quarantine bin (`.filen-sync-trash`), keyed by quarantine-relative path -> bytes.
fn quarantine_files(root: &Path) -> std::collections::BTreeMap<String, Vec<u8>> {
	let mut out = std::collections::BTreeMap::new();
	let bin = root.join(".filen-sync-trash");
	let mut stack = vec![bin.clone()];
	while let Some(dir) = stack.pop() {
		let rd = match std::fs::read_dir(&dir) {
			Ok(rd) => rd,
			Err(_) => continue,
		};
		for entry in rd.flatten() {
			let path = entry.path();
			let ft = match entry.file_type() {
				Ok(ft) => ft,
				Err(_) => continue,
			};
			if ft.is_dir() {
				stack.push(path);
			} else if ft.is_file() {
				let rel = path
					.strip_prefix(&bin)
					.unwrap()
					.to_string_lossy()
					.replace('\\', "/");
				out.insert(rel, std::fs::read(&path).unwrap());
			}
		}
	}
	out
}

// ============================================================================
// First sync / incremental fast-path
// ============================================================================

/// SCALE-01 — first sync of a (scaled-down) multi-dir tree to an empty remote: exact create/upload
/// counts, byte-exact remote, zero deletions/conflicts/errors, and an immediate second pass that
/// reports zero changes (baseline captured everything).
#[shared_test_runtime]
async fn scale_01_large_first_sync_to_empty_remote() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	// 12 dirs (4 top + 8 nested) holding 48 files of varied size (incl. zero-byte).
	const TOP: usize = 4;
	const PER: usize = 12;
	let mut files = 0usize;
	let mut dirs = 0usize;
	for d in 0..TOP {
		for s in 0..2 {
			let sub = if s == 0 {
				format!("dir{d:02}")
			} else {
				format!("dir{d:02}/nested")
			};
			dirs += 1;
			for i in 0..(PER / 2) {
				let rel = format!("{sub}/f{i:02}.txt");
				// Vary size: every 3rd file zero-byte, otherwise the path bytes repeated.
				let bytes = if i % 3 == 0 {
					Vec::new()
				} else {
					rel.repeat(i + 1).into_bytes()
				};
				write_file(&sc.local, &rel, &bytes);
				files += 1;
			}
		}
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "errors: {:?}", r1.errors);
	assert_eq!(r1.uploaded, files, "all files uploaded once: {r1:?}");
	assert_eq!(
		r1.remote_dirs_created, dirs,
		"all dirs created once: {r1:?}"
	);
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	// Second immediate pass: baseline captured everything -> zero changes.
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	sc.cleanup();
}

/// SCALE-02 — a tiny incremental local change after a baseline applies exactly 2 actions (1 upload
/// for the modified file + 1 create for the new file) and re-touches nothing else.
#[shared_test_runtime]
async fn scale_02_tiny_local_incremental_after_baseline() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 30;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("base/f{i:02}.txt"),
			format!("v0-{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Modify exactly one file; add exactly one new file.
	write_file(&sc.local, "base/f00.txt", b"modified-content-longer");
	write_file(&sc.local, "base/new.txt", b"brand new");

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 2, "exactly the modified + new file: {r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");

	// The two changed files are byte-exact on the remote.
	let (dirs, _) = list_root(&sc).await;
	let base = find_dir(&dirs, "base").expect("base dir");
	let (_, bfiles) = list_dir(&sc, base).await;
	assert_eq!(
		bfiles.len(),
		N + 1,
		"remote has N+1 files, no dup: {}",
		bfiles.len()
	);
	assert_eq!(
		find_file(&bfiles, "f00.txt").unwrap().size,
		b"modified-content-longer".len() as u64
	);
	assert_eq!(
		find_file(&bfiles, "new.txt").unwrap().size,
		b"brand new".len() as u64
	);

	// Third pass: zero changes (incremental converged).
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");

	sc.cleanup();
}

/// SCALE-A (review-add) — a tiny REMOTE-side incremental change after a baseline applies exactly 2
/// actions (1 download for the modified file + 1 create for the new one) without re-pulling the
/// rest. Exercises the remote-listing fast-path the design doc notes SCALE-02 never hits.
#[shared_test_runtime]
async fn scale_a_tiny_remote_incremental_after_baseline() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	// Seed the remote root with a small baseline, wait for the cache to see it, pull it down.
	const N: usize = 12;
	let mut seeded = Vec::new();
	for i in 0..N {
		let f = upload_root(&sc, &format!("r{i:02}.txt"), format!("rv0-{i}").as_bytes()).await;
		seeded.push(f);
	}
	for f in &seeded {
		assert!(
			poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"cache never saw seeded file"
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, N, "{r1:?}");

	// Remote: modify one (re-upload same name -> server versions to a new uuid) + add one new.
	let new_modified = upload_root(&sc, "r00.txt", b"remote-modified-and-longer").await;
	let brand_new = upload_root(&sc, "rnew.txt", b"brand new remote").await;
	assert!(
		poll_for_item(
			sc.cache.db_path(),
			new_modified.uuid(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the re-versioned file"
	);
	assert!(
		poll_for_item(sc.cache.db_path(), brand_new.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never saw the new remote file"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.downloaded, 2, "exactly the modified + new file: {r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert!(
		read_eq(&sc.local, "r00.txt", b"remote-modified-and-longer"),
		"byte-exact modified"
	);
	assert!(
		read_eq(&sc.local, "rnew.txt", b"brand new remote"),
		"byte-exact new"
	);

	sc.cleanup();
}

/// SCALE-03 — many tiny zero-byte and 1-byte files in a single wide directory: all present, exact
/// sizes, first pass creates all, second pass zero changes.
#[shared_test_runtime]
async fn scale_03_many_tiny_files_one_wide_dir() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const ZEROS: usize = 25;
	const ONES: usize = 25;
	for i in 0..ZEROS {
		write_file(&sc.local, &format!("wide/z{i:03}.dat"), b"");
	}
	for i in 0..ONES {
		// Distinct single byte value derived from index.
		let byte = u8::try_from(i % 256).expect("a remainder of 256 fits a byte");
		write_file(&sc.local, &format!("wide/o{i:03}.dat"), &[byte]);
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, ZEROS + ONES, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "single wide dir: {r1:?}");

	let (dirs, _) = list_root(&sc).await;
	let wide = find_dir(&dirs, "wide").expect("wide dir");
	let (_, wfiles) = list_dir(&sc, wide).await;
	assert_eq!(
		wfiles.len(),
		ZEROS + ONES,
		"remote count exact, no collisions/drops"
	);
	for i in 0..ZEROS {
		assert_eq!(find_file(&wfiles, &format!("z{i:03}.dat")).unwrap().size, 0);
	}
	for i in 0..ONES {
		assert_eq!(find_file(&wfiles, &format!("o{i:03}.dat")).unwrap().size, 1);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");

	sc.cleanup();
}

/// SCALE-04 — a wide remote directory pulled down byte-exact, then a small incremental remote add:
/// exactly the new files download and none of the originals are re-touched.
#[shared_test_runtime]
async fn scale_04_wide_remote_dir_download_then_incremental() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	let dir = create_root_dir(&sc, "wide").await;
	const N: usize = 40;
	let mut seeded = Vec::new();
	for i in 0..N {
		let f = upload_into(
			&sc,
			&dir,
			&format!("w{i:03}.txt"),
			format!("wide-{i}").as_bytes(),
		)
		.await;
		seeded.push(f);
	}
	// Wait until the cache holds the dir + all files.
	assert!(poll_for_item(sc.cache.db_path(), dir.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	for f in &seeded {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, N, "{r1:?}");
	assert_eq!(r1.local_dirs_created, 1, "{r1:?}");
	assert_eq!(
		count_files(&sc.local),
		N,
		"all files on disk, none skipped/duplicated"
	);
	for i in 0..N {
		assert!(read_eq(
			&sc.local,
			&format!("wide/w{i:03}.txt"),
			format!("wide-{i}").as_bytes()
		));
	}

	// Add 5 more remote files into the same dir.
	let mut more = Vec::new();
	for i in 0..5 {
		let f = upload_into(
			&sc,
			&dir,
			&format!("extra{i}.txt"),
			format!("extra-{i}").as_bytes(),
		)
		.await;
		more.push(f);
	}
	for f in &more {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.downloaded, 5,
		"only the 5 new files, originals untouched: {r2:?}"
	);
	assert_eq!(count_files(&sc.local), N + 5, "now N+5 on disk");

	sc.cleanup();
}

/// SCALE-05 — deep directory nesting round-trips intact (push up, pull down on a fresh local root),
/// with the deepest file byte-exact and no truncation/flattening. Depth scaled to 40 levels (deep
/// enough to exercise recursion without hitting server/path-length limits on a live account).
#[shared_test_runtime]
async fn scale_05_deep_nesting_round_trips() {
	const DEPTH: usize = 40;
	let deep_rel = {
		let mut p = String::new();
		for _ in 0..DEPTH {
			p.push_str("a/");
		}
		p.push_str("deep.txt");
		p
	};
	let payload = b"bottom-of-the-well";

	// --- push side ---
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, &deep_rel, payload);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(
		r1.remote_dirs_created, DEPTH,
		"all {DEPTH} levels created: {r1:?}"
	);

	// --- pull side: a SEPARATE client on the same remote dir ---
	let remote = sc.remote;
	let cache_b = TestCache::new(&sc.resources.client, remote).await;
	let expected_items = 1 + DEPTH + 1;
	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let converged = poll_until(CACHE_CONVERGE_TIMEOUT, || {
		count_items(cache_b.db_path()) >= expected_items
	})
	.await;
	assert!(converged, "pull cache did not converge on the deep tree");

	let local_b = fresh_local_dir("scale05b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r2 = engine_b.sync_once(pair_b).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.downloaded, 1, "{r2:?}");
	assert_eq!(
		r2.local_dirs_created, DEPTH,
		"all {DEPTH} levels reproduced: {r2:?}"
	);

	assert!(
		read_eq(&local_b, &deep_rel, payload),
		"deepest file byte-exact after round trip"
	);
	assert_trees_identical(&sc.local, &local_b, "push", "pull");

	std::fs::remove_dir_all(&local_b).ok();
	sc.cleanup();
}

// ============================================================================
// Move / delete batches
// ============================================================================

/// SCALE-10 — a large LOCAL move batch (rename of a whole directory) is ONE move of the directory,
/// not delete+re-upload nor a move per file: zero uploads, zero trashes, the directory keeps its
/// uuid and every file is still under it, byte-exact.
#[shared_test_runtime]
async fn scale_10_local_move_batch_is_moves_not_reupload() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 25;
	for i in 0..N {
		// Unique content per file so the engine can match by hash/uuid across the move.
		write_file(
			&sc.local,
			&format!("A/f{i:03}.txt"),
			format!("payload-unique-{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");
	let a_uuid = find_dir(&list_root(&sc).await.0, "A")
		.expect("A dir present")
		.uuid();

	// Rename the whole directory A -> B in one local operation.
	std::fs::rename(sc.local.join("A"), sc.local.join("B")).unwrap();

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "a rename must NOT re-upload bytes: {r2:?}");
	// The directory itself is renamed on the remote, its files riding along: nothing is created
	// and nothing — not even an emptied source directory — is trashed.
	assert_eq!(
		r2.remotely_trashed, 0,
		"a directory rename trashes nothing: {r2:?}"
	);
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(
		r2.held_deletions(),
		0,
		"a move must not trip the mass-delete hold: {r2:?}"
	);
	assert_eq!(
		r2.moved_remote, 1,
		"the whole directory is one move, whatever it holds: {r2:?}"
	);

	// Remote reflects the move: the same directory, now named B, holding every file.
	let (dirs, _) = list_root(&sc).await;
	assert!(find_dir(&dirs, "A").is_none(), "A should be gone");
	let b = find_dir(&dirs, "B").expect("B dir present");
	assert_eq!(b.uuid(), a_uuid, "the directory keeps its uuid");
	let (_, bfiles) = list_dir(&sc, b).await;
	assert_eq!(bfiles.len(), N, "all files re-parented under B");
	for i in 0..N {
		assert_eq!(
			find_file(&bfiles, &format!("f{i:03}.txt")).unwrap().size,
			format!("payload-unique-{i}").len() as u64
		);
	}

	// Re-run: zero changes.
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

/// SCALE-B (review-add) — a large REMOTE move batch (a whole remote dir renamed) is detected as
/// local moves, not delete+re-download: zero downloads, zero local deletions, files re-parent
/// locally byte-exact, no quarantine/hold tripped.
#[shared_test_runtime]
async fn scale_b_remote_move_batch_is_local_moves_not_redownload() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	let mut dir_a = create_root_dir(&sc, "A").await;
	const N: usize = 20;
	let mut seeded = Vec::new();
	for i in 0..N {
		let f = upload_into(
			&sc,
			&dir_a,
			&format!("f{i:03}.txt"),
			format!("remote-unique-{i}").as_bytes(),
		)
		.await;
		seeded.push(f);
	}
	assert!(poll_for_item(sc.cache.db_path(), dir_a.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	for f in &seeded {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, N, "{r1:?}");

	// Rename the remote dir A -> B (re-parents all N files in one operation). update_file_metadata
	// only renames files; for a dir rename we move it under a fresh "B"-named parent is awkward, so
	// rename the directory's own metadata via... the public API exposes only move_dir/trash_dir for
	// dirs. Instead: create dir B at root and move every file into it (a batch re-parent), which is
	// the remote analogue exercising the same local-move detection path.
	let dir_b = create_root_dir(&sc, "B").await;
	for f in &mut seeded {
		sc.resources
			.client
			.move_file(f, &DirType::<Normal>::Dir(Cow::Borrowed(&dir_b)))
			.await
			.unwrap();
	}
	let _ = &mut dir_a;
	// Wait for the cache to reflect the new parent for every moved file.
	let b_uuid: Uuid = dir_b.uuid();
	for f in &seeded {
		let fu: Uuid = f.uuid();
		let db = sc.cache.db_path().to_path_buf();
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || {
				matches!(query_cached_file(&db, fu), Some((_, _, _, parent)) if parent == b_uuid.as_bytes())
			})
			.await,
			"cache never saw the remote re-parent of a file"
		);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.downloaded, 0,
		"a remote move must NOT re-download: {r2:?}"
	);
	assert_eq!(
		r2.locally_deleted, 0,
		"a remote move must NOT delete locally: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions(),
		0,
		"a move must not trip the quarantine hold: {r2:?}"
	);
	assert!(r2.moved_local >= 1, "expected local moves: {r2:?}");

	// Local reflects the move: files under B, none under A.
	assert_eq!(
		count_files(&sc.local),
		N,
		"still N files locally, byte-exact"
	);
	for i in 0..N {
		assert!(read_eq(
			&sc.local,
			&format!("B/f{i:03}.txt"),
			format!("remote-unique-{i}").as_bytes()
		));
		assert!(
			!sc.local.join(format!("A/f{i:03}.txt")).exists(),
			"file still under A locally"
		);
	}

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");
	assert_eq!(r3.moved_local, 0, "{r3:?}");

	sc.cleanup();
}

/// SCALE-11 — a large local delete batch (100% of the tree) trips the mass-deletion hold: nothing
/// is trashed remotely, the hold is surfaced, and after a confirming re-run (here the guard relaxes
/// once the deletion is no longer "sudden") the deletes apply. We assert the all-held, not-partial
/// behaviour; full re-run-to-zero is asserted loosely since the suite has no confirmation API.
#[shared_test_runtime]
async fn scale_11_mass_local_delete_is_held() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 24;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("f{i:02}.txt"),
			format!("content-{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Delete ALL N locally at once.
	for i in 0..N {
		std::fs::remove_file(sc.local.join(format!("f{i:02}.txt"))).unwrap();
	}
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.held_deletions() > 0 || r2.guard.is_some(),
		"mass-delete guard should engage for {N}/{N}: {r2:?}"
	);
	assert_eq!(
		r2.remotely_trashed, 0,
		"no partial application — all held: {r2:?}"
	);

	// Remote still has every file (nothing nuked, no partial delete).
	let (_, files) = list_root(&sc).await;
	assert_eq!(
		files.len(),
		N,
		"guard kept all remote files: found {}",
		files.len()
	);

	sc.cleanup();
}

/// SCALE-12 — a large remote delete batch removes the local copies but routes them into the
/// recoverable quarantine bin byte-exact, with no same-name collisions dropping data. Scaled to a
/// modest batch; deletes a fraction below the mass-delete floor so the pass applies without a
/// confirmation API.
#[shared_test_runtime]
async fn scale_12_remote_delete_batch_quarantined_recoverable() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	const KEEP: usize = 20;
	const DROP: usize = 6; // below the mass-delete floor so it applies in one pass
	let mut keep = Vec::new();
	let mut drop = Vec::new();
	for i in 0..KEEP {
		keep.push(upload_root(&sc, &format!("k{i:02}.txt"), format!("keep-{i}").as_bytes()).await);
	}
	for i in 0..DROP {
		drop.push(
			upload_root(
				&sc,
				&format!("d{i:02}.txt"),
				format!("drop-payload-{i}").as_bytes(),
			)
			.await,
		);
	}
	for f in keep.iter().chain(drop.iter()) {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, KEEP + DROP, "{r1:?}");

	// Trash the DROP set on the remote; wait for the cache to drop them.
	for f in &mut drop {
		sc.resources.client.trash_file(f).await.unwrap();
	}
	for f in &drop {
		assert!(poll_for_item_absent(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.locally_deleted, DROP,
		"all dropped files removed from active tree: {r2:?}"
	);

	// Active tree: only the KEEP set remains.
	assert_eq!(
		count_files(&sc.local),
		KEEP,
		"only kept files in the active tree"
	);
	for i in 0..DROP {
		assert!(
			!sc.local.join(format!("d{i:02}.txt")).exists(),
			"dropped file still active"
		);
	}
	// Quarantine: every dropped file is recoverable byte-exact (no collision dropped one).
	let q = quarantine_files(&sc.local);
	for i in 0..DROP {
		let want = format!("drop-payload-{i}").into_bytes();
		let found = q.values().any(|v| *v == want);
		assert!(
			found,
			"dropped file d{i:02}.txt content not recoverable in quarantine; bin={:?}",
			q.keys().collect::<Vec<_>>()
		);
	}

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");
	assert_eq!(r3.locally_deleted, 0, "{r3:?}");

	sc.cleanup();
}

/// SCALE-22 — deleting an entire remote subdirectory removes its whole local subtree (quarantined
/// recoverably) while files outside it are untouched. Scaled; the design's "efficient subtree, not
/// per-file thrash" perf claim is not asserted (no perf instrumentation), only the correctness.
#[shared_test_runtime]
async fn scale_22_remote_subtree_delete_removes_local_subtree() {
	let sc = single_client(SyncMode::RemoteToLocal).await;

	let mut doomed = create_root_dir(&sc, "doomed").await;
	let mut inside = Vec::new();
	const N: usize = 6; // below the mass-delete floor so the pass applies without a confirm API
	for i in 0..N {
		inside.push(
			upload_into(
				&sc,
				&doomed,
				&format!("g{i:02}.txt"),
				format!("inside-{i}").as_bytes(),
			)
			.await,
		);
	}
	let outside = upload_root(&sc, "outside.txt", b"survivor").await;
	assert!(poll_for_item(sc.cache.db_path(), doomed.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	for f in inside.iter().chain(std::iter::once(&outside)) {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, N + 1, "{r1:?}");

	// Trash the whole subtree on the remote.
	sc.resources.client.trash_dir(&mut doomed).await.unwrap();
	for f in &inside {
		assert!(poll_for_item_absent(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Files outside the subtree are untouched.
	assert!(
		read_eq(&sc.local, "outside.txt", b"survivor"),
		"outside file survived"
	);
	// The subtree's files are gone from the active tree and recoverable in quarantine.
	for i in 0..N {
		assert!(
			!sc.local.join(format!("doomed/g{i:02}.txt")).exists(),
			"subtree file still active"
		);
	}
	let q = quarantine_files(&sc.local);
	for i in 0..N {
		let want = format!("inside-{i}").into_bytes();
		assert!(
			q.values().any(|v| *v == want),
			"subtree file g{i:02}.txt not recoverable in quarantine"
		);
	}

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// Mixed batches / repeated scans / dedup
// ============================================================================

/// SCALE-15 — one pass with simultaneous many creates, modifies, moves, and deletes (deletes below
/// the mass-delete floor) lands the exact remote end-state, categorizing moves as moves (not
/// delete+create), and a re-run reports zero changes.
#[shared_test_runtime]
async fn scale_15_mixed_large_batch_one_pass() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	// Baseline: 24 files. m00..m05 will be modified, v00..v05 moved, d00..d03 deleted, rest untouched.
	const BASE: usize = 24;
	for i in 0..BASE {
		let rel = format!("base/b{i:02}.txt");
		write_file(&sc.local, &rel, format!("orig-unique-{i}").as_bytes());
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, BASE, "{r1:?}");

	// In ONE batch:
	// - add 6 new files
	for i in 0..6 {
		write_file(
			&sc.local,
			&format!("base/new{i:02}.txt"),
			format!("created-{i}").as_bytes(),
		);
	}
	// - modify 6 existing files
	for i in 0..6 {
		write_file(
			&sc.local,
			&format!("base/b{i:02}.txt"),
			format!("MODIFIED-{i}-longer").as_bytes(),
		);
	}
	// - move 4 files into a new subtree (unique content -> detected as moves)
	for i in 6..10 {
		move_file(
			&sc.local,
			&format!("base/b{i:02}.txt"),
			&format!("moved/b{i:02}.txt"),
		);
	}
	// - delete 3 files (below the mass-delete floor)
	for i in 10..13 {
		std::fs::remove_file(sc.local.join(format!("base/b{i:02}.txt"))).unwrap();
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 12,
		"6 creates + 6 modifies re-uploaded: {r2:?}"
	);
	assert!(r2.moved_remote >= 1, "moves categorized as moves: {r2:?}");
	assert_eq!(r2.remotely_trashed, 3, "exactly the 3 deletes: {r2:?}");
	assert_eq!(
		r2.held_deletions(),
		0,
		"3 deletes below the floor — not held: {r2:?}"
	);

	// Net remote file count: 24 + 6 new - 3 deleted = 27 (moves don't change the count).
	let (dirs, _) = list_root(&sc).await;
	let base = find_dir(&dirs, "base").expect("base dir");
	let moved = find_dir(&dirs, "moved").expect("moved dir");
	let (_, bfiles) = list_dir(&sc, base).await;
	let (_, mfiles) = list_dir(&sc, moved).await;
	assert_eq!(
		bfiles.len() + mfiles.len(),
		BASE + 6 - 3,
		"net file count exact"
	);
	assert_eq!(mfiles.len(), 4, "4 files under the new moved/ subtree");

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

/// SCALE-13 — repeated no-op passes over an unchanged baseline all report zero changes (the
/// fast-path holds; no churn / spurious writes), and the tree is byte-identical before and after.
#[shared_test_runtime]
async fn scale_13_repeated_noop_passes_stay_zero() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 30;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("d{}/f{i:02}.txt", i % 5),
			format!("v-{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	let before = walk_tree(&sc.local);
	for pass in 0..10 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {pass}: {r:?}");
		assert_eq!(r.uploaded, 0, "pass {pass} re-uploaded: {r:?}");
		assert_eq!(r.downloaded, 0, "pass {pass}: {r:?}");
		assert_eq!(r.moved_remote, 0, "pass {pass}: {r:?}");
		assert_eq!(r.remotely_trashed, 0, "pass {pass}: {r:?}");
		assert_eq!(r.remote_dirs_created, 0, "pass {pass}: {r:?}");
		assert_eq!(r.conflicts.len(), 0, "pass {pass}: {r:?}");
	}
	let after = walk_tree(&sc.local);
	assert_eq!(
		before, after,
		"baseline tree must be byte-identical across the no-op passes"
	);

	sc.cleanup();
}

/// SCALE-24 — many duplicate-content files at distinct paths all sync correctly; modifying exactly
/// one changes only that file remotely (no shared-content aliasing collateral), no file dropped as
/// a "duplicate".
#[shared_test_runtime]
async fn scale_24_duplicate_content_files_no_aliasing() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 25;
	let dup = b"identical content shared by every file";
	for i in 0..N {
		write_file(&sc.local, &format!("dup/f{i:03}.txt"), dup);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.uploaded, N,
		"every duplicate uploaded, none dropped: {r1:?}"
	);

	let (dirs, _) = list_root(&sc).await;
	let d = find_dir(&dirs, "dup").expect("dup dir");
	let (_, files) = list_dir(&sc, d).await;
	assert_eq!(
		files.len(),
		N,
		"all distinct paths present despite identical content"
	);
	for f in &files {
		assert_eq!(
			f.size,
			dup.len() as u64,
			"byte-exact size for every duplicate"
		);
	}

	// Modify exactly one.
	write_file(
		&sc.local,
		"dup/f000.txt",
		b"now this one is different and longer",
	);
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 1,
		"only the one changed file re-uploads: {r2:?}"
	);
	assert_eq!(
		r2.moved_remote, 0,
		"no spurious move from dedup aliasing: {r2:?}"
	);
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");

	let (dirs, _) = list_root(&sc).await;
	let d = find_dir(&dirs, "dup").expect("dup dir");
	let (_, files) = list_dir(&sc, d).await;
	assert_eq!(
		find_file(&files, "f000.txt").unwrap().size,
		b"now this one is different and longer".len() as u64
	);
	for i in 1..N {
		assert_eq!(
			find_file(&files, &format!("f{i:03}.txt")).unwrap().size,
			dup.len() as u64,
			"sibling {i} collaterally changed"
		);
	}

	sc.cleanup();
}

// ============================================================================
// First-sync-into-populated / backup additivity / restart
// ============================================================================

/// SCALE-19 — a first two-way sync against an already-populated remote does NOT wipe it: the union
/// is preserved (remote-only and local-only both survive), same-path differing-content files
/// surface as conflicts rather than being blindly overwritten, and nothing is mass-trashed.
#[shared_test_runtime]
async fn scale_19_first_sync_into_populated_remote_no_wipe() {
	let sc = single_client(SyncMode::TwoWay).await;

	// Pre-populate the remote (no baseline for this pair yet): 10 remote files, 4 of which share a
	// path with a local file (2 same content, 2 differing content), plus 6 remote-only.
	const SHARED_SAME: usize = 2;
	const SHARED_DIFF: usize = 2;
	const REMOTE_ONLY: usize = 6;
	const LOCAL_ONLY: usize = 4;
	let mut remote_files = Vec::new();
	for i in 0..SHARED_SAME {
		remote_files.push(
			upload_root(
				&sc,
				&format!("same{i}.txt"),
				format!("agree-{i}").as_bytes(),
			)
			.await,
		);
		write_file(
			&sc.local,
			&format!("same{i}.txt"),
			format!("agree-{i}").as_bytes(),
		);
	}
	for i in 0..SHARED_DIFF {
		remote_files.push(
			upload_root(
				&sc,
				&format!("diff{i}.txt"),
				format!("REMOTE-{i}").as_bytes(),
			)
			.await,
		);
		write_file(
			&sc.local,
			&format!("diff{i}.txt"),
			format!("LOCAL-{i}").as_bytes(),
		);
	}
	for i in 0..REMOTE_ONLY {
		remote_files.push(
			upload_root(
				&sc,
				&format!("ronly{i}.txt"),
				format!("remote-only-{i}").as_bytes(),
			)
			.await,
		);
	}
	for i in 0..LOCAL_ONLY {
		write_file(
			&sc.local,
			&format!("lonly{i}.txt"),
			format!("local-only-{i}").as_bytes(),
		);
	}
	for f in &remote_files {
		assert!(poll_for_item(sc.cache.db_path(), f.uuid(), CACHE_CONVERGE_TIMEOUT).await);
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");

	// No mass-wipe: zero remote trashes, zero local deletions.
	assert_eq!(
		r1.remotely_trashed, 0,
		"first sync must not trash a populated remote: {r1:?}"
	);
	assert_eq!(
		r1.locally_deleted, 0,
		"first sync must not delete local-only files: {r1:?}"
	);

	// Differing-content same-path files surface as conflicts (not blindly overwritten).
	for i in 0..SHARED_DIFF {
		assert!(
			r1.conflict_paths()
				.any(|c| c.contains(&format!("diff{i}.txt"))),
			"diff{i}.txt should be a conflict: {r1:?}"
		);
		// Local side left untouched on conflict.
		assert!(read_eq(
			&sc.local,
			&format!("diff{i}.txt"),
			format!("LOCAL-{i}").as_bytes()
		));
	}

	// Union preserved: remote-only files arrive locally; local-only files survive (and push up).
	let mut converged = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "{r:?}");
		assert_eq!(r.remotely_trashed, 0, "still no wipe: {r:?}");
		assert_eq!(r.locally_deleted, 0, "still no local delete: {r:?}");
		let have_remote = (0..REMOTE_ONLY).all(|i| {
			read_eq(
				&sc.local,
				&format!("ronly{i}.txt"),
				format!("remote-only-{i}").as_bytes(),
			)
		});
		let (_, files) = list_root(&sc).await;
		let have_local =
			(0..LOCAL_ONLY).all(|i| find_file(&files, &format!("lonly{i}.txt")).is_some());
		if have_remote && have_local {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(1000)).await;
	}
	assert!(
		converged,
		"union of local-only and remote-only files was not preserved on both sides"
	);

	sc.cleanup();
}

/// SCALE-20 — in backup mode (LocalBackup) a full local delete is NOT mirrored: all remote files
/// remain, zero remote trashes, no mass-delete prompt, and a subsequent local re-create syncs as
/// creates (not conflicts).
#[shared_test_runtime]
async fn scale_20_local_backup_delete_not_mirrored() {
	let sc = single_client(SyncMode::LocalBackup).await;

	const N: usize = 18;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("f{i:02}.txt"),
			format!("backup-{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Delete ALL locally.
	for i in 0..N {
		std::fs::remove_file(sc.local.join(format!("f{i:02}.txt"))).unwrap();
	}
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not mirror local deletes: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions(),
		0,
		"no deletions to hold in backup mode: {r2:?}"
	);
	assert!(
		r2.guard.is_none(),
		"no mass-delete prompt in backup mode: {r2:?}"
	);

	// All remote files remain.
	let (_, files) = list_root(&sc).await;
	assert_eq!(
		files.len(),
		N,
		"backup retained all remote files: {}",
		files.len()
	);

	// Re-create one locally; it must sync up as a create (baseline advanced), not a conflict.
	write_file(&sc.local, "f00.txt", b"re-created locally");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 1, "re-create pushes as an upload: {r3:?}");
	assert_eq!(
		r3.conflicts.len(),
		0,
		"re-create must not be a conflict: {r3:?}"
	);

	sc.cleanup();
}

/// SCALE-21 — baseline persists across an engine "restart": re-opening the engine against the SAME
/// baseline DB path and re-adding the same pair reports zero changes on a no-op pass (baseline
/// reloaded, not rebuilt as a first sync), with no spurious re-uploads.
#[shared_test_runtime]
async fn scale_21_baseline_persists_across_restart() {
	// Build the fixture manually so we can reuse one fixed baseline DB path across two `open`s.
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("scale21");
	let db_path = temp_cache_path();

	const N: usize = 30;
	for i in 0..N {
		write_file(
			&local,
			&format!("d{}/f{i:02}.txt", i % 4),
			format!("persist-{i}").as_bytes(),
		);
	}

	// --- session 1: first sync establishes the baseline ---
	{
		let engine = SyncEngine::open(cache.client.clone(), db_path.clone())
			.await
			.unwrap();
		let pair = engine
			.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
			.await
			.unwrap();
		let r1 = engine.sync_once(pair).await.unwrap();
		assert!(r1.errors.is_empty(), "{r1:?}");
		assert_eq!(r1.uploaded, N, "{r1:?}");
		// engine dropped here -> "process stop"
	}

	// --- session 2: re-open against the SAME db path, re-add the same pair, no-op pass ---
	let engine2 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r2 = engine2.sync_once(pair2).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 0,
		"restart must NOT re-upload an unchanged baseline: {r2:?}"
	);
	assert_eq!(
		r2.remote_dirs_created, 0,
		"restart must NOT re-create dirs: {r2:?}"
	);
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	std::fs::remove_dir_all(&local).ok();
}

// ============================================================================
// Two-way & conflict scale (review additions)
// ============================================================================

/// SCALE-C (review-add) — a two-way pass with many simultaneous DISJOINT changes on BOTH sides
/// converges to the union both ways, with no spurious deletes, and a re-run reports zero changes.
#[shared_test_runtime]
async fn scale_c_two_way_both_sides_disjoint_changes() {
	let tc = two_clients(SyncMode::TwoWay).await;

	const SEED: usize = 8;
	// Seed a shared baseline on A; converge so both baselines record it.
	for i in 0..SEED {
		write_file(
			&tc.local_a,
			&format!("seed/s{i:02}.txt"),
			format!("seed-{i}").as_bytes(),
		);
	}
	let mut conflicts = BTreeSet::new();
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"scale_c baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& tc
					.local_b
					.join(format!("seed/s{:02}.txt", SEED - 1))
					.is_file()
		},
	)
	.await;

	// Disjoint changes on both sides: A adds 5 + edits 3 seed files; B adds 5 (different) + edits a
	// DIFFERENT 3 seed files.
	for i in 0..5 {
		write_file(
			&tc.local_a,
			&format!("fromA/a{i:02}.txt"),
			format!("a-new-{i}").as_bytes(),
		);
	}
	for i in 0..3 {
		write_file(
			&tc.local_a,
			&format!("seed/s{i:02}.txt"),
			format!("a-edit-{i}").as_bytes(),
		);
	}
	for i in 0..5 {
		write_file(
			&tc.local_b,
			&format!("fromB/b{i:02}.txt"),
			format!("b-new-{i}").as_bytes(),
		);
	}
	for i in 4..7 {
		write_file(
			&tc.local_b,
			&format!("seed/s{i:02}.txt"),
			format!("b-edit-{i}").as_bytes(),
		);
	}

	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
		&mut conflicts,
		"scale_c converge",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& (0..5).all(|i| tc.local_b.join(format!("fromA/a{i:02}.txt")).is_file())
				&& (0..5).all(|i| tc.local_a.join(format!("fromB/b{i:02}.txt")).is_file())
				&& read_eq(&tc.local_b, "seed/s00.txt", b"a-edit-0")
				&& read_eq(&tc.local_a, "seed/s06.txt", b"b-edit-6")
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"disjoint changes must not conflict: {conflicts:?}"
	);
	assert_trees_identical(&tc.local_a, &tc.local_b, "local_a", "local_b");

	// Re-run: zero changes both ways.
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
	)
	.await;
	assert_eq!(ra.uploaded + ra.downloaded, 0, "A re-run not zero: {ra:?}");
	assert_eq!(rb.uploaded + rb.downloaded, 0, "B re-run not zero: {rb:?}");

	tc.cleanup();
}

/// The uuids of the CURRENT remote versions of the `c/c{i:02}.txt` files under the shared root
/// (ground truth), asserting all `n` of them exist.
async fn remote_conflict_uuids(tc: &TwoClients, n: usize) -> Vec<Uuid> {
	let client = &tc.resources.client;
	let (dirs, _f) = client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&tc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let c_dir = dirs
		.iter()
		.find(|d| d.name() == Some("c"))
		.expect("the remote c/ dir is missing");
	let (_d, files) = client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(c_dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	(0..n)
		.map(|i| {
			let name = format!("c{i:02}.txt");
			files
				.iter()
				.find(|f| f.name() == Some(name.as_str()))
				.unwrap_or_else(|| panic!("remote c/{name} is missing"))
				.uuid()
		})
		.collect()
}

/// SCALE-D (review-add) — many simultaneous both-sides-diverged conflicts are ALL surfaced, none
/// dropped/coalesced, non-conflicting files stay correct, and the surviving content is recoverable
/// (never destroyed on both sides). Scaled to a modest conflict count.
#[shared_test_runtime]
async fn scale_d_many_simultaneous_conflicts_all_surfaced() {
	let tc = two_clients(SyncMode::TwoWay).await;

	const CONFLICTING: usize = 8;
	const CLEAN: usize = 6;
	// Seed identical baseline for both the soon-to-conflict and the clean files.
	for i in 0..CONFLICTING {
		let rel = format!("c/c{i:02}.txt");
		write_file(&tc.local_a, &rel, b"shared-baseline");
		write_file(&tc.local_b, &rel, b"shared-baseline");
	}
	for i in 0..CLEAN {
		let rel = format!("k/k{i:02}.txt");
		write_file(&tc.local_a, &rel, format!("clean-{i}").as_bytes());
	}

	let mut conflicts = BTreeSet::new();
	// Converge the baseline so both engines record identical bytes for every c-file + the clean set.
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"scale_d baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& (0..CONFLICTING)
					.all(|i| read_eq(&tc.local_b, &format!("c/c{i:02}.txt"), b"shared-baseline"))
				&& (0..CLEAN).all(|i| tc.local_b.join(format!("k/k{i:02}.txt")).is_file())
		},
	)
	.await;
	// One more settle pass each so the converged baseline is recorded as last-synced.
	let _ = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	conflicts.clear();

	// Diverge the SAME CONFLICTING files on both sides to differing content.
	for i in 0..CONFLICTING {
		let rel = format!("c/c{i:02}.txt");
		write_file(&tc.local_a, &rel, format!("A-side-{i}").as_bytes());
		write_file(&tc.local_b, &rel, format!("B-side-{i}").as_bytes());
	}

	// Stage the divergence sequentially (as CONFLICT-05 does): A's edits commit first and B's
	// cache observes every new version BEFORE B's pass, so B reconciles each local edit against a
	// changed remote. Pushing both sides at once instead lets the server linearize the same-name
	// uploads (last write wins, the loser kept only in history), after which the loser pulls
	// silently and there is no divergence left for any round to surface.
	let ra0 = tc
		.engine_a
		.sync_once(tc.pair_a)
		.await
		.expect("engine A sync_once");
	assert!(ra0.errors.is_empty(), "A errors: {:?}", ra0.errors);
	assert_eq!(
		ra0.uploaded, CONFLICTING,
		"A must push every diverged file first: {ra0:?}"
	);
	for uuid in remote_conflict_uuids(&tc, CONFLICTING).await {
		assert!(
			poll_for_item(tc.cache_b.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
			"B's cache never observed A's edit {uuid}"
		);
	}

	// Run several rounds to let every divergence reconcile and surface.
	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::BFirst,
		)
		.await;
		assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
		assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1200)).await;
	}

	// Every divergent file must be surfaced as a conflict, none dropped.
	for i in 0..CONFLICTING {
		assert!(
			conflicts
				.iter()
				.any(|c| c.contains(&format!("c{i:02}.txt"))),
			"conflict c{i:02}.txt not surfaced (saw {} conflicts)",
			conflicts.len()
		);
	}

	// Each conflicting file's content must survive SOMEWHERE (never destroyed on both sides).
	for i in 0..CONFLICTING {
		let rel = format!("c/c{i:02}.txt");
		let a_ok = read_eq(&tc.local_a, &rel, format!("A-side-{i}").as_bytes());
		let b_ok = read_eq(&tc.local_b, &rel, format!("B-side-{i}").as_bytes());
		assert!(a_ok || b_ok, "c{i:02}.txt lost on both sides — data loss");
	}

	// Non-conflicting clean files unaffected on both sides.
	for i in 0..CLEAN {
		assert!(
			read_eq(
				&tc.local_a,
				&format!("k/k{i:02}.txt"),
				format!("clean-{i}").as_bytes()
			),
			"clean A {i}"
		);
		assert!(
			read_eq(
				&tc.local_b,
				&format!("k/k{i:02}.txt"),
				format!("clean-{i}").as_bytes()
			),
			"clean B {i}"
		);
	}

	tc.cleanup();
}

/// SCALE-E (review-add) — many max-length / multi-byte-unicode filenames sync byte-exact with names
/// preserved (no truncation/collision), first pass creates all, second pass zero changes. Scaled
/// count; names are long but within a safe per-name budget for a live account.
#[shared_test_runtime]
async fn scale_e_long_and_unicode_filenames() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 20;
	let mut names = Vec::new();
	for i in 0..N {
		// A long, distinct name (~160 chars). Half use multi-byte unicode.
		let name = if i % 2 == 0 {
			format!("{}_{i:03}.txt", "long_ascii_name_segment".repeat(6))
		} else {
			format!("{}_{i:03}.txt", "ファイル名_длинное_名前".repeat(4))
		};
		write_file(&sc.local, &name, format!("content-{i}").as_bytes());
		names.push(name);
	}

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "all long-named files uploaded: {r1:?}");

	let (_, files) = list_root(&sc).await;
	assert_eq!(
		files.len(),
		N,
		"remote count exact — no truncation collisions"
	);
	for name in &names {
		let f = find_file(&files, name)
			.unwrap_or_else(|| panic!("name not preserved exactly: {name:?}"));
		// Every name ends `_{i:03}.txt`.
		let (_, index) = name.strip_suffix(".txt").unwrap().rsplit_once('_').unwrap();
		let i: usize = index.parse().unwrap();
		assert_eq!(
			f.size,
			format!("content-{i}").len() as u64,
			"byte-exact size for {name:?}"
		);
	}

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");

	sc.cleanup();
}

// ============================================================================
// Progress events
// ============================================================================

/// SCALE-23 — a pass reports progress events via `sync_once_observed`: a `PassStarted`, a single
/// `Planned` whose count equals the applied actions, one in-progress event per action (no phantom
/// progress, bounded — not one-per-byte), and a final `PassCompleted` whose report reconciles with
/// the in-progress event counts.
#[shared_test_runtime]
async fn scale_23_progress_events_emitted_and_bounded() {
	let sc = single_client(SyncMode::LocalToRemote).await;

	const N: usize = 20;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("p/f{i:02}.txt"),
			format!("prog-{i}").as_bytes(),
		);
	}

	let mut events: Vec<SyncEvent> = Vec::new();
	let mut observer = |e: SyncEvent| events.push(e);
	let report = sc
		.engine
		.sync_once_observed(sc.pair, &mut observer)
		.await
		.expect("sync_once_observed");

	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(report.uploaded, N, "{report:?}");
	assert_eq!(report.remote_dirs_created, 1, "{report:?}");

	// Lifecycle: PassStarted first, PassCompleted last.
	assert!(
		matches!(events.first(), Some(SyncEvent::PassStarted { .. })),
		"first event must be PassStarted: {:?}",
		events.first()
	);
	assert!(
		matches!(events.last(), Some(SyncEvent::PassCompleted { .. })),
		"last event must be PassCompleted: {:?}",
		events.last()
	);

	// Exactly one Planned event; its count == applied actions == in-progress events.
	let planned: Vec<usize> = events
		.iter()
		.filter_map(|e| {
			if let SyncEvent::Planned { actions } = e {
				Some(*actions)
			} else {
				None
			}
		})
		.collect();
	assert_eq!(planned.len(), 1, "exactly one Planned event: {planned:?}");
	let planned_actions = planned[0];

	// In-progress (per-action) events: uploads + dir creates here.
	let uploading = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::Uploading { .. }))
		.count();
	let creating = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::CreatingRemoteDir { .. }))
		.count();
	assert_eq!(
		uploading, N,
		"one Uploading per file, no phantom progress: {uploading}"
	);
	assert_eq!(
		creating, 1,
		"one CreatingRemoteDir for the single dir: {creating}"
	);
	assert_eq!(
		uploading + creating,
		planned_actions,
		"in-progress events must equal Planned count"
	);

	// Bounded: total event volume is O(actions), not one-per-byte. Byte progress is throttled per
	// transfer, and each of these files is far smaller than one upload chunk, so each upload reports
	// its bytes exactly once, when they land.
	let progress = events
		.iter()
		.filter(|e| matches!(e, SyncEvent::Progress { .. }))
		.count();
	assert_eq!(
		progress, N,
		"one Progress per small upload, not one per byte: {events:?}"
	);
	assert!(
		events.len() - progress <= planned_actions + 4,
		"event volume must be bounded (~actions + lifecycle): {} events for {planned_actions} actions",
		events.len() - progress
	);

	// The final PassCompleted carries a report reconciling with the in-progress counts.
	if let Some(SyncEvent::PassCompleted { report: rep }) = events.last() {
		assert_eq!(
			rep.uploaded, uploading,
			"PassCompleted upload count reconciles"
		);
		assert_eq!(
			rep.remote_dirs_created, creating,
			"PassCompleted dir-create count reconciles"
		);
	}

	sc.cleanup();
}

/// SCALE-16 — watch mode debounces a burst of LOCAL events into exactly ONE coalesced pass.
///
/// Scaled down like the rest of this module (a few dozen files, not 5k); what is preserved is the
/// claim — a burst written entirely inside one debounce window costs ONE pass, not one per file.
/// The window is pinned with an explicit [`WatchConfig`] so that is a fact about the loop rather
/// than a race against the production 800 ms default.
#[shared_test_runtime]
async fn scale_16_watch_debounces_local_event_burst() {
	const BURST_DEBOUNCE: Duration = Duration::from_secs(15);
	const N: usize = 60;

	let sc = single_client(SyncMode::LocalToRemote).await;
	// watch() needs an `Arc<SyncEngine>`; the harness's own engine is left unused here.
	let engine = Arc::new(
		SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::LocalToRemote)
		.await
		.unwrap();

	let passes = Arc::new(AtomicUsize::new(0));
	let uploaded = Arc::new(AtomicUsize::new(0));
	let (pass_count, upload_count) = (passes.clone(), uploaded.clone());
	let handle = engine
		.clone()
		.watch_with(
			pair,
			WatchConfig {
				debounce: BURST_DEBOUNCE,
				// Far enough out that no pass observed here can be a safety-net pass.
				safety_net: Duration::from_secs(3600),
			},
			Box::new(move |event| {
				if let SyncEvent::PassCompleted { report } = event {
					pass_count.fetch_add(1, Ordering::SeqCst);
					upload_count.fetch_add(report.uploaded, Ordering::SeqCst);
				}
			}),
		)
		.await
		.unwrap();

	// Settle the loop's immediate initial pass (and any trigger the registrations produced) before
	// the burst, so the pass count it is measured against is stable.
	assert!(
		poll_until(WATCH_SETTLE, || passes.load(Ordering::SeqCst) >= 1).await,
		"the watch never ran its initial pass"
	);
	tokio::time::sleep(BURST_DEBOUNCE + Duration::from_secs(5)).await;
	let before = passes.load(Ordering::SeqCst);

	// The whole burst lands well inside one debounce window.
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("burst{i:03}.txt"),
			format!("b{i}").as_bytes(),
		);
	}

	assert!(
		poll_until(WATCH_SETTLE, || uploaded.load(Ordering::SeqCst) >= N).await,
		"the burst never uploaded (up={}, passes={})",
		uploaded.load(Ordering::SeqCst),
		passes.load(Ordering::SeqCst)
	);
	assert_eq!(
		uploaded.load(Ordering::SeqCst),
		N,
		"files were uploaded more than once"
	);
	assert_eq!(
		passes.load(Ordering::SeqCst),
		before + 1,
		"the burst was not coalesced into a single pass"
	);

	// Byte-exact on the remote...
	let (_dirs, files) = list_root(&sc).await;
	assert_eq!(files.len(), N, "remote file count mismatch");
	for i in 0..N {
		let name = format!("burst{i:03}.txt");
		let f = files
			.iter()
			.find(|f| f.name() == Some(name.as_str()))
			.unwrap_or_else(|| panic!("{name} missing on the remote"));
		assert_eq!(f.size, format!("b{i}").len() as u64, "{name} size mismatch");
	}

	// ...and settled: the loop is not chasing its own writes, so a pass after it has nothing to do.
	handle.stop().await;
	let after = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		(after.uploaded, after.downloaded),
		(0, 0),
		"a settled burst re-transferred: {after:?}"
	);

	sc.cleanup();
}

// ============================================================================
// Blocked: need fault-injection / instrumentation harnesses that do not exist yet
// ============================================================================

/// SCALE-06 — multi-GB single file upload + bounded-memory + byte-exact hash.
/// plan: create a multi-GB local file with deterministic content, sync, hash-compare the remote,
/// assert size+hash match and memory stays bounded (streamed/chunked), re-run reports 0 changes.
#[ignore = "blocked: needs fault-injection / instrumentation harness (multi-GB fixture + bounded-memory sampling) — see TODO"]
#[shared_test_runtime]
async fn scale_06_multi_gb_upload_byte_exact_bounded_memory() {}

/// SCALE-07 — multi-GB single file download + bounded-memory + atomic finalize (no partial file
/// exposed as complete).
/// plan: place a multi-GB known-content remote file, remote->local sync, hash the local copy,
/// assert size+hash match, memory bounded, no partial-download artifact visible as complete.
#[ignore = "blocked: needs fault-injection / instrumentation harness (multi-GB fixture + bounded-memory + interrupt) — see TODO"]
#[shared_test_runtime]
async fn scale_07_multi_gb_download_byte_exact_atomic() {}

/// SCALE-08 — high-concurrency throughput stays within rate limits (no 429 storm).
/// plan: sync 10k small files while monitoring request rate / 429s; assert in-flight transfers
/// stay under the engine's concurrency cap, throttling is backed-off not surfaced as errors, all
/// upload byte-exact, report shows 10k uploads / 0 errors.
#[ignore = "blocked: needs request-rate / 429 instrumentation harness (in-flight + throttle observability) — see TODO"]
#[shared_test_runtime]
async fn scale_08_high_concurrency_within_rate_limits() {}

/// SCALE-09 — bounded memory across a full-tree scan of a huge (200k) baseline.
/// plan: establish a 200k-item baseline, make no changes, run a safety-net re-scan while sampling
/// peak memory; assert 0 changes, peak memory does not scale prohibitively, no OOM, baseline intact.
#[ignore = "blocked: needs peak-memory sampling harness + 200k live baseline — see TODO"]
#[shared_test_runtime]
async fn scale_09_bounded_memory_full_tree_scan() {}

/// SCALE-14 — interrupted large first sync resumes without re-doing completed work or losing data.
/// plan: begin a first sync of a 40k tree, forcibly interrupt mid-transfer (~50%), restart and run
/// to completion; assert all files byte-exact, completed work not re-uploaded, no half-uploaded
/// items, final re-run reports 0 changes, no data loss either side.
#[ignore = "blocked: needs deterministic mid-transfer interruption / crash-injection harness — see TODO"]
#[shared_test_runtime]
async fn scale_14_interrupted_first_sync_resumes() {}

/// SCALE-A2 (review-add) — watch mode coalesces a burst of thousands of REMOTE-change events into
/// bounded passes.
/// The debounce window is pinnable now (`WatchConfig`, see SCALE-16), but the remote half still is
/// not: the cache delivers a bulk remote create as however many batches it happens to commit,
/// spread over however long convergence takes, so "one coalesced pass" is a race against cache
/// batching rather than a statement about the loop.
/// plan: start watch on a baselined two-way/remote->local pair, burst-create/modify 5k remote
/// files, wait to quiesce; assert all reflected locally byte-exact, the remote-event burst
/// coalesces into few passes (not thousands), self-writes don't loop, manual pass then 0 changes.
#[ignore = "blocked: needs deterministic in-window delivery of the remote change notifications — see TODO"]
#[shared_test_runtime]
async fn scale_a2_watch_debounces_remote_event_burst() {}

/// SCALE-17 — many sync pairs run together without resource exhaustion or rate-limit breach.
/// plan: configure 50 pairs (distinct local/remote roots) each with a few thousand files, run all,
/// monitor aggregate concurrency/memory/request-rate; assert every pair converges byte-exact,
/// aggregate in-flight + global request rate stay within caps (shared global budget), no starvation,
/// memory bounded, no OOM.
#[ignore = "blocked: needs aggregate concurrency / request-rate / memory instrumentation harness across 50 live pairs — see TODO"]
#[shared_test_runtime]
async fn scale_17_many_pairs_within_global_budget() {}

/// SCALE-18 — wide directory of many large files: combined width + size stress with bounded memory.
/// plan: 200 files of ~100MB each of distinct known content, sync, re-run; assert each byte-exact
/// (size+hash), concurrent large transfers bounded so memory stays flat (not all 200 buffered), no
/// truncation/cross-file content mixing, first pass 200 uploads, second pass 0 changes.
#[ignore = "blocked: needs bounded-memory sampling harness + 200x100MB live fixture — see TODO"]
#[shared_test_runtime]
async fn scale_18_wide_dir_of_large_files_bounded_memory() {}

/// SCALE-25 — transient failures during a large pass are retried, and the pass is re-runnable to
/// convergence.
/// plan: first sync of a 20k tree with injected intermittent transient transfer failures; allow
/// retry/back-off, run additional passes if needed; assert transient failures are retried (not
/// whole-pass abort), all byte-exact after convergence, failed-then-succeeded items not
/// duplicated/partial, final pass 0 changes, intermediate error counts accurate.
#[ignore = "blocked: needs transient-failure injection harness (intermittent transfer faults) — see TODO"]
#[shared_test_runtime]
async fn scale_25_transient_failures_retried_to_convergence() {}
