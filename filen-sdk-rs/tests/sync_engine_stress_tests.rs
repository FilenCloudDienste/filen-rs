//! Black-box stress / contention tests for the sync engine.
//!
//! These are heavy, live, network-bound tests. They are `#[ignore]`d by default and
//! are scaled via env vars so the orchestrator can crank them up:
//!   - `SYNC_STRESS_N`             (Test 1, default 10_000): item count of the round-trip tree.
//!   - `SYNC_STRESS_CONTENTION_N`  (Test 2, default 200):    total disjoint-file count.

use std::{collections::BTreeSet, path::Path, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	fs::HasUUID,
	sync_engine::{ConflictResolution, PlannedConflict, SyncEngine, SyncMode},
};
use uuid::Uuid;

mod helpers;
use helpers::*;

/// Build a nested tree of ~`n` small files under `root`, returning (num_files, num_dirs).
///
/// Layout: ~sqrt(n) top-level subdirs, each with a nested sub-subdir, files spread across both
/// levels, plus a handful of empty dirs. File contents are deterministic from the relative path.
fn build_tree(root: &Path, n: usize) -> (usize, usize) {
	// ⌈√n⌉, so `buckets²` covers every file.
	let sqrt = n.isqrt();
	let buckets = if sqrt * sqrt < n { sqrt + 1 } else { sqrt }.max(1);
	let mut files = 0usize;
	let mut dirs = 0usize;

	// A few empty dirs at the top level (must round-trip as empty dirs).
	for e in 0..3usize {
		let d = root.join(format!("empty_{e:03}"));
		std::fs::create_dir_all(&d).unwrap();
		dirs += 1;
	}

	let mut placed = 0usize;
	'outer: for b in 0..buckets {
		let bucket = root.join(format!("dir_{b:04}"));
		std::fs::create_dir_all(&bucket).unwrap();
		dirs += 1;
		// A nested level under each bucket.
		let nested = bucket.join("nested");
		std::fs::create_dir_all(&nested).unwrap();
		dirs += 1;

		for i in 0..buckets {
			// Alternate placement between the bucket and its nested child.
			let (parent, level) = if i % 2 == 0 {
				(&bucket, "")
			} else {
				(&nested, "nested/")
			};
			let rel = format!("dir_{b:04}/{level}f_{i:04}.txt");
			let abs = parent.join(format!("f_{i:04}.txt"));
			std::fs::write(&abs, content_for(&rel)).unwrap();
			files += 1;
			placed += 1;
			if placed >= n {
				break 'outer;
			}
		}
	}

	(files, dirs)
}

// ---------------------------------------------------------------------------
// Test 1: large round-trip local -> remote -> (separate client) -> local.
// ---------------------------------------------------------------------------

#[ignore = "reason: heavy/live — large tree round-trip over the network"]
#[shared_test_runtime]
async fn roundtrip_large_tree_local_to_remote_then_remote_to_local() {
	let n: usize = std::env::var("SYNC_STRESS_N")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(10_000);

	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();

	// --- build the source tree ---
	let local_a = fresh_local_dir("a");
	let (num_files, num_dirs) = build_tree(&local_a, n);

	// --- push side (engine A, its own derived client/cache on the remote dir) ---
	let cache_a = TestCache::new(&resources.client, remote).await;
	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::LocalToRemote)
		.await
		.unwrap();

	let report = engine_a.sync_once(pair_a).await.unwrap();
	assert!(
		report.errors.is_empty(),
		"push pass reported errors: {:?}",
		report.errors
	);
	// At this scale a single pass should upload everything in one go.
	assert_eq!(
		report.uploaded, num_files,
		"expected to upload all {num_files} files in one pass, got {} (errors: {:?})",
		report.uploaded, report.errors
	);
	assert_eq!(
		report.remote_dirs_created, num_dirs,
		"expected to create all {num_dirs} remote dirs, got {}",
		report.remote_dirs_created
	);

	// --- pull side: a SEPARATE client ("different device") ---
	let cache_b = TestCache::new(&resources.client, remote).await;

	// Total cache items once converged: root + dirs + files.
	let expected_items = 1 + num_dirs + num_files;
	// Generous, n-scaled timeout (the converge bound is contention-driven, not throughput).
	let converge_timeout = Duration::from_secs(60 + n as u64 / 50);

	// From 0, not the log's length now: `cache_b` is new, so everything in its log is its own, and
	// the populate resync `TestCache::new` started may already have finished — a `since` read
	// here would skip it and wait out the whole timeout.
	wait_for_converged_resync(&cache_b.messages, remote, 0, converge_timeout).await;
	// Be robust: also explicitly poll the item count until the full tree is present.
	let converged = poll_until(converge_timeout, || {
		count_items(cache_b.db_path()) >= expected_items
	})
	.await;
	assert!(
		converged,
		"cache_b did not converge on the full remote tree: have {} items, expected {expected_items}",
		count_items(cache_b.db_path())
	);

	let local_b = fresh_local_dir("b");
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();

	// Try a single pass first; assert it suffices, but allow a bounded multi-pass fallback for
	// a very large tree where something is transiently held.
	let mut total_downloaded = 0usize;
	let mut last_report = engine_b.sync_once(pair_b).await.unwrap();
	assert!(
		last_report.errors.is_empty(),
		"pull pass reported errors: {:?}",
		last_report.errors
	);
	total_downloaded += last_report.downloaded;

	if total_downloaded != num_files {
		// Fallback: loop until everything is on disk, bounded.
		const MAX_PASSES: usize = 8;
		let mut passes = 1usize;
		while count_files(&local_b) < num_files && passes < MAX_PASSES {
			last_report = engine_b.sync_once(pair_b).await.unwrap();
			assert!(
				last_report.errors.is_empty(),
				"pull pass {passes} reported errors: {:?}",
				last_report.errors
			);
			total_downloaded += last_report.downloaded;
			passes += 1;
		}
	}

	let files_on_disk = count_files(&local_b);
	assert_eq!(
		files_on_disk, num_files,
		"pull side did not reproduce all files: {files_on_disk} on disk, expected {num_files} \
		 (total downloaded across passes: {total_downloaded})"
	);

	// --- the two local trees must be byte-for-byte identical ---
	let tree_a = walk_tree(&local_a);
	let tree_b = walk_tree(&local_b);
	assert_tree_maps_identical(&tree_a, &tree_b, "local_a", "local_b");

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

/// Count regular files recursively under `root`.
fn count_files(root: &Path) -> usize {
	let mut count = 0usize;
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
			} else if ft.is_file() {
				count += 1;
			}
		}
	}
	count
}

// ---------------------------------------------------------------------------
// Test 2: two clients two-way-syncing the SAME remote dir, with contention.
// ---------------------------------------------------------------------------

#[ignore = "reason: heavy/live — two clients contend on a shared remote dir"]
#[shared_test_runtime]
async fn twoway_two_clients_contend_and_converge() {
	let m: usize = std::env::var("SYNC_STRESS_CONTENTION_N")
		.ok()
		.and_then(|s| s.parse().ok())
		.unwrap_or(200);

	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();

	// Two independent client+cache+engine stacks on the SAME remote dir.
	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;

	let local_a = fresh_local_dir("twa");
	let local_b = fresh_local_dir("twb");

	// Seed DISJOINT changes: half the files only in A, half only in B.
	let half = (m / 2).max(1);
	std::fs::create_dir_all(local_a.join("a")).unwrap();
	std::fs::create_dir_all(local_b.join("b")).unwrap();
	for i in 0..half {
		let name = format!("a/a{i:04}.txt");
		std::fs::write(local_a.join(&name), content_for(&name)).unwrap();
	}
	for i in 0..half {
		let name = format!("b/b{i:04}.txt");
		std::fs::write(local_b.join(&name), content_for(&name)).unwrap();
	}

	// One conflicting path. Seed it IDENTICALLY on both sides first, then converge it into a
	// shared 3-way baseline below, then diverge it — that is what makes the eventual edit a
	// genuine BOTH-SIDES-CHANGED conflict (rather than two independent first-time adds, which a
	// two-way reconcile can legitimately just unify).
	std::fs::write(local_a.join("conflict.txt"), b"shared-baseline").unwrap();
	std::fs::write(local_b.join("conflict.txt"), b"shared-baseline").unwrap();

	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	// Collect conflicts observed across all rounds.
	let mut all_conflicts: std::collections::BTreeSet<String> = Default::default();

	// Generous per-round wait for each cache to observe the other's remote writes.
	let observe_timeout = Duration::from_secs(60 + m as u64 / 10);

	// --- Phase 0: converge the disjoint files AND the shared conflict.txt baseline ---
	// Run rounds until both local trees contain every disjoint file and the same conflict.txt.
	// This establishes conflict.txt in each engine's 3-way baseline (identical bytes everywhere).
	const BASELINE_ROUNDS: usize = 12;
	let mut baseline_converged = false;
	for _round in 0..BASELINE_ROUNDS {
		let (ra, rb) = tokio::join!(engine_a.sync_once(pair_a), engine_b.sync_once(pair_b));
		let ra = ra.expect("engine_a.sync_once (baseline) must not Err");
		let rb = rb.expect("engine_b.sync_once (baseline) must not Err");
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			all_conflicts.insert(c.to_string());
		}
		let _ = poll_until(observe_timeout, || {
			disjoint_present(&local_a, &local_b, half)
				&& local_a.join("conflict.txt").is_file()
				&& local_b.join("conflict.txt").is_file()
		})
		.await;
		if disjoint_present(&local_a, &local_b, half)
			&& std::fs::read(local_a.join("conflict.txt")).ok().as_deref()
				== Some(b"shared-baseline".as_ref())
			&& std::fs::read(local_b.join("conflict.txt")).ok().as_deref()
				== Some(b"shared-baseline".as_ref())
		{
			baseline_converged = true;
			break;
		}
	}
	assert!(
		baseline_converged,
		"failed to converge the shared baseline (disjoint files + identical conflict.txt) \
		 within {BASELINE_ROUNDS} rounds"
	);

	// One more pass each so the baseline store records the converged conflict.txt as last-synced.
	let (ra, rb) = tokio::join!(engine_a.sync_once(pair_a), engine_b.sync_once(pair_b));
	for c in ra
		.expect("settle a")
		.conflict_paths()
		.chain(rb.expect("settle b").conflict_paths())
	{
		all_conflicts.insert(c.to_string());
	}

	// --- Phase 1: DIVERGE conflict.txt on both sides, then contend ---
	std::fs::write(local_a.join("conflict.txt"), b"from-A-side-diverged").unwrap();
	std::fs::write(local_b.join("conflict.txt"), b"from-B-side-diverged").unwrap();

	// Run several concurrent rounds. We do NOT break early on the disjoint condition (it is
	// already satisfied from phase 0); the point of these rounds is to let the diverged
	// conflict.txt reconcile under contention and surface as a conflict, and to confirm the
	// disjoint files STAY converged (no corruption / spurious deletion).
	const MAX_ROUNDS: usize = 12;
	for _round in 0..MAX_ROUNDS {
		// Both engines sync concurrently — genuine contention on the shared remote.
		let (ra, rb) = tokio::join!(engine_a.sync_once(pair_a), engine_b.sync_once(pair_b));
		let ra = ra.expect("engine_a.sync_once must not Err");
		let rb = rb.expect("engine_b.sync_once must not Err");

		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			all_conflicts.insert(c.to_string());
		}

		// Sanity: nothing should indicate corruption. We tolerate per-pass guard/held messages,
		// but a sync_once must never error out, which is already asserted above.
		// (errors here are per-action failures the pass continued past; we just record them.)

		// Let each cache observe the other side's writes before the next round.
		let _ = poll_until(Duration::from_secs(3), || false).await;
	}

	// The disjoint files must STILL be present in both trees after the contention rounds.
	let converged = disjoint_present(&local_a, &local_b, half);
	assert!(
		converged,
		"disjoint files did not stay converged across both local trees after {MAX_ROUNDS} rounds \
		 (local_a a-files={}, b-files={}; local_b a-files={}, b-files={})",
		count_prefixed(&local_a, "a", half),
		count_prefixed(&local_a, "b", half),
		count_prefixed(&local_b, "a", half),
		count_prefixed(&local_b, "b", half),
	);

	// --- convergence of disjoint files: every aNNN and bNNN present in BOTH local trees ---
	for i in 0..half {
		let a_name = format!("a/a{i:04}.txt");
		let b_name = format!("b/b{i:04}.txt");
		assert!(
			local_a.join(&a_name).is_file(),
			"{a_name} missing in local_a (its own file)"
		);
		assert!(
			local_b.join(&a_name).is_file(),
			"{a_name} did not converge into local_b"
		);
		assert!(
			local_b.join(&b_name).is_file(),
			"{b_name} missing in local_b (its own file)"
		);
		assert!(
			local_a.join(&b_name).is_file(),
			"{b_name} did not converge into local_a"
		);
	}

	// One last concurrent pass each, so whatever the rounds left in flight has settled.
	let (ra, rb) = tokio::join!(engine_a.sync_once(pair_a), engine_b.sync_once(pair_b));
	for c in ra
		.expect("final settle a")
		.conflict_paths()
		.chain(rb.expect("final settle b").conflict_paths())
	{
		all_conflicts.insert(c.to_string());
	}

	// --- conflict handling for conflict.txt ---
	// The engine holds a two-way conflict rather than picking a side, so it must have reported it.
	assert!(
		all_conflicts.contains("conflict.txt"),
		"conflict.txt was never surfaced via report.conflicts (saw {all_conflicts:?})"
	);

	// Whichever upload lost the race, its engine still holds the conflict (the winner may hold one
	// too, for the edit its upload buried), and a held conflict leaves the local copy alone. The
	// winner's copy is its own push. So each side's edit is still in place in its own tree.
	let held =
		|conflicts: Vec<PlannedConflict>| conflicts.iter().any(|c| c.rel_path == "conflict.txt");
	let held_a = held(engine_a.list_conflicts(pair_a).await.unwrap());
	let held_b = held(engine_b.list_conflicts(pair_b).await.unwrap());
	assert!(
		held_a || held_b,
		"neither engine holds the conflict on conflict.txt after the contention rounds"
	);
	for (root, label, edit) in [
		(&local_a, "A", b"from-A-side-diverged".as_slice()),
		(&local_b, "B", b"from-B-side-diverged"),
	] {
		assert!(
			read_eq(root, "conflict.txt", edit),
			"{label}'s edit is no longer at conflict.txt in {label}'s own tree (it holds {:?})",
			std::fs::read(root.join("conflict.txt"))
				.map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
		);
	}

	// Resolve it by keeping both on one side. When both engines hold it the other takes the
	// remote: a second keep-both would upload a conflict.old.txt of its own at the same time, a
	// same-name create race of its own.
	let (keep_both, take_remote) = if held_a {
		((&engine_a, pair_a), held_b.then_some((&engine_b, pair_b)))
	} else {
		((&engine_b, pair_b), None)
	};
	let kept_as = keep_both
		.0
		.resolve_conflict(keep_both.1, "conflict.txt", ConflictResolution::KeepBoth)
		.await
		.unwrap()
		.kept_as;
	assert_eq!(kept_as.as_deref(), Some("conflict.old.txt"));
	if let Some((engine, pair)) = take_remote {
		engine
			.resolve_conflict(pair, "conflict.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap();
	}

	// Then the two trees converge to the same contents, both edits among them.
	converge(
		Peer {
			engine: &engine_a,
			pair: pair_a,
		},
		Peer {
			engine: &engine_b,
			pair: pair_b,
		},
		Order::AFirst,
		&mut all_conflicts,
		"contention-resolved",
		|| trees_equal(&local_a, &local_b),
	)
	.await;
	let tree_a = walk_tree(&local_a);
	assert_tree_maps_identical(&tree_a, &walk_tree(&local_b), "local_a", "local_b");
	for edit in [b"from-A-side-diverged".as_slice(), b"from-B-side-diverged"] {
		assert!(
			["conflict.txt", "conflict.old.txt"]
				.iter()
				.any(|name| tree_a.get(*name).is_some_and(|(_, _, bytes)| bytes == edit)),
			"the edit {:?} is not in the converged trees",
			String::from_utf8_lossy(edit)
		);
	}
	assert!(
		!held(engine_a.list_conflicts(pair_a).await.unwrap())
			&& !held(engine_b.list_conflicts(pair_b).await.unwrap()),
		"a resolved conflict is still held after the trees converged"
	);

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

/// True once both local trees contain all `half` aNNN and all `half` bNNN files.
fn disjoint_present(local_a: &Path, local_b: &Path, half: usize) -> bool {
	count_prefixed(local_a, "a", half) == half
		&& count_prefixed(local_a, "b", half) == half
		&& count_prefixed(local_b, "a", half) == half
		&& count_prefixed(local_b, "b", half) == half
}

/// Count how many of the `half` expected `<prefix>/<prefix>NNNN.txt` files exist under `root`.
fn count_prefixed(root: &Path, prefix: &str, half: usize) -> usize {
	(0..half)
		.filter(|i| root.join(format!("{prefix}/{prefix}{i:04}.txt")).is_file())
		.count()
}

// ===========================================================================
// Two-client NON-COLLIDING convergence — changes propagate both ways, in any sync order.
// ===========================================================================

/// A bunch of NON-COLLIDING changes (at most one client touches any given path between syncs)
/// applied across two clients two-way-syncing the same remote dir. Each change must propagate to
/// the other client and leave both local trees byte-for-byte identical — and the sync order is
/// rotated (A-first / B-first / concurrent) per scenario to show it does not matter. No scenario
/// here is a genuine conflict, so the engine must surface ZERO conflicts across the whole run.
#[ignore = "reason: heavy/live — two clients, many non-colliding edits, must converge both ways"]
#[shared_test_runtime]
async fn twoway_noncolliding_changes_propagate_both_ways_in_any_order() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;
	let local_a = fresh_local_dir("nca");
	let local_b = fresh_local_dir("ncb");

	// Seed the shared baseline on A only; converging makes B pull it so BOTH baselines record it
	// (so later one-sided deletes/edits are recognized, not held by the first-sync guard).
	for rel in [
		"base/s0.txt",
		"base/s1.txt",
		"base/s2.txt",
		"top.txt",
		"keep/k0.txt",
	] {
		write_file(&local_a, rel, content_for(rel).as_slice());
	}

	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pa = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pb = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let peer_a = Peer {
		engine: &engine_a,
		pair: pa,
	};
	let peer_b = Peer {
		engine: &engine_b,
		pair: pb,
	};

	let mut conflicts = BTreeSet::new();

	// Establish the shared baseline.
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"baseline",
		|| {
			trees_equal(&local_a, &local_b)
				&& local_b.join("top.txt").is_file()
				&& local_b.join("base/s2.txt").is_file()
		},
	)
	.await;

	// S1 — A creates a new file. [A-first]
	write_file(&local_a, "a/new1.txt", b"new-one");
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"S1 A-creates",
		|| trees_equal(&local_a, &local_b) && read_eq(&local_b, "a/new1.txt", b"new-one"),
	)
	.await;

	// S2 — B creates a new nested file (+ dirs). [B-first]
	write_file(&local_b, "b/deep/new2.txt", b"new-two");
	converge(
		peer_a,
		peer_b,
		Order::BFirst,
		&mut conflicts,
		"S2 B-creates-nested",
		|| trees_equal(&local_a, &local_b) && read_eq(&local_a, "b/deep/new2.txt", b"new-two"),
	)
	.await;

	// S3 — A modifies an existing file. [concurrent]
	write_file(&local_a, "top.txt", b"top-modified");
	converge(
		peer_a,
		peer_b,
		Order::Concurrent,
		&mut conflicts,
		"S3 A-modifies",
		|| trees_equal(&local_a, &local_b) && read_eq(&local_b, "top.txt", b"top-modified"),
	)
	.await;

	// S4 — B renames a file in place (unique content -> uuid move). [A-first]
	move_file(&local_b, "base/s0.txt", "base/renamed0.txt");
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"S4 B-renames",
		|| {
			trees_equal(&local_a, &local_b)
				&& local_a.join("base/renamed0.txt").is_file()
				&& !local_a.join("base/s0.txt").exists()
		},
	)
	.await;

	// S5 — A moves a file across directories. [B-first]
	move_file(&local_a, "base/s1.txt", "moved/s1.txt");
	converge(
		peer_a,
		peer_b,
		Order::BFirst,
		&mut conflicts,
		"S5 A-moves",
		|| {
			trees_equal(&local_a, &local_b)
				&& local_b.join("moved/s1.txt").is_file()
				&& !local_b.join("base/s1.txt").exists()
		},
	)
	.await;

	// S6 — A deletes a file (one-sided delete propagates). [concurrent]
	std::fs::remove_file(local_a.join("base/s2.txt")).unwrap();
	converge(
		peer_a,
		peer_b,
		Order::Concurrent,
		&mut conflicts,
		"S6 A-deletes",
		|| trees_equal(&local_a, &local_b) && !local_b.join("base/s2.txt").exists(),
	)
	.await;

	// S7 — B creates an empty directory. [A-first]
	std::fs::create_dir_all(local_b.join("emptydir")).unwrap();
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"S7 B-empty-dir",
		|| trees_equal(&local_a, &local_b) && local_a.join("emptydir").is_dir(),
	)
	.await;

	// S8 — B deletes the (now-converged) empty directory. [B-first]
	std::fs::remove_dir(local_b.join("emptydir")).unwrap();
	converge(
		peer_a,
		peer_b,
		Order::BFirst,
		&mut conflicts,
		"S8 B-deletes-empty-dir",
		|| trees_equal(&local_a, &local_b) && !local_a.join("emptydir").exists(),
	)
	.await;

	// S9 — both sides create DIFFERENT files in the same round (disjoint). [concurrent]
	write_file(&local_a, "a/x.txt", b"x");
	write_file(&local_b, "b/y.txt", b"y");
	converge(
		peer_a,
		peer_b,
		Order::Concurrent,
		&mut conflicts,
		"S9 disjoint-creates",
		|| {
			trees_equal(&local_a, &local_b)
				&& local_b.join("a/x.txt").is_file()
				&& local_a.join("b/y.txt").is_file()
		},
	)
	.await;

	// S10 — both sides edit DIFFERENT existing files in the same round (disjoint). [concurrent]
	write_file(&local_a, "a/new1.txt", b"new-one-edited");
	write_file(&local_b, "top.txt", b"top-modified-again");
	converge(
		peer_a,
		peer_b,
		Order::Concurrent,
		&mut conflicts,
		"S10 disjoint-edits",
		|| {
			trees_equal(&local_a, &local_b)
				&& read_eq(&local_b, "a/new1.txt", b"new-one-edited")
				&& read_eq(&local_a, "top.txt", b"top-modified-again")
		},
	)
	.await;

	// S11 — both sides create the SAME new path with IDENTICAL content (the adopt/no-op branch:
	// both changed but agree, so it is NOT a conflict). [concurrent]
	write_file(&local_a, "shared/same.txt", b"identical");
	write_file(&local_b, "shared/same.txt", b"identical");
	converge(
		peer_a,
		peer_b,
		Order::Concurrent,
		&mut conflicts,
		"S11 identical-both-create",
		|| {
			trees_equal(&local_a, &local_b)
				&& read_eq(&local_a, "shared/same.txt", b"identical")
				&& read_eq(&local_b, "shared/same.txt", b"identical")
		},
	)
	.await;

	// S12 — A creates a zero-byte file. [A-first]
	write_file(&local_a, "a/empty.txt", b"");
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"S12 zero-byte",
		|| trees_equal(&local_a, &local_b) && read_eq(&local_b, "a/empty.txt", b""),
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"non-colliding scenarios must never surface a conflict, but saw: {conflicts:?}"
	);

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

/// The SAME disjoint change-set must converge to the SAME final tree no matter the sync order.
/// Runs it under each of the three orders in independent fixtures and asserts identical results.
#[ignore = "reason: heavy/live — same change-set converges identically under every sync order"]
#[shared_test_runtime]
async fn twoway_disjoint_changes_converge_identically_regardless_of_order() {
	let a = converge_disjoint_under(Order::AFirst).await;
	let b = converge_disjoint_under(Order::BFirst).await;
	let c = converge_disjoint_under(Order::Concurrent).await;

	assert!(
		a == b && b == c,
		"the same change-set converged to DIFFERENT trees by sync order \
		 (A-first: {} entries, B-first: {}, concurrent: {})",
		a.len(),
		b.len(),
		c.len()
	);
	// Sanity: the expected union really is present.
	assert!(
		a.contains_key("seed.txt") && a.contains_key("from_a.txt") && a.contains_key("from_b.txt"),
		"converged tree missing an expected path: {:?}",
		a.keys().collect::<Vec<_>>()
	);
}

/// A fresh two-client fixture: seed + converge a baseline, then stage the SAME disjoint changes
/// (A creates `from_a.txt` and edits `seed.txt`; B creates `from_b.txt`), converge under `order`,
/// assert both local trees agree, and return that converged tree.
async fn converge_disjoint_under(order: Order) -> TreeMap {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;
	let local_a = fresh_local_dir("oia");
	let local_b = fresh_local_dir("oib");

	write_file(&local_a, "seed.txt", b"seed-v1");

	let engine_a = SyncEngine::open(cache_a.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let engine_b = SyncEngine::open(cache_b.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pa = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pb = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let peer_a = Peer {
		engine: &engine_a,
		pair: pa,
	};
	let peer_b = Peer {
		engine: &engine_b,
		pair: pb,
	};

	let mut conflicts = BTreeSet::new();
	converge(
		peer_a,
		peer_b,
		Order::AFirst,
		&mut conflicts,
		"oi-baseline",
		|| trees_equal(&local_a, &local_b) && local_b.join("seed.txt").is_file(),
	)
	.await;

	// Stage the disjoint changes: A adds a file and edits the seed; B adds a different file.
	write_file(&local_a, "from_a.txt", b"a");
	write_file(&local_a, "seed.txt", b"seed-v2");
	write_file(&local_b, "from_b.txt", b"b");
	converge(peer_a, peer_b, order, &mut conflicts, "oi-converge", || {
		trees_equal(&local_a, &local_b)
			&& local_a.join("from_b.txt").is_file()
			&& local_b.join("from_a.txt").is_file()
			&& read_eq(&local_b, "seed.txt", b"seed-v2")
	})
	.await;

	assert!(
		conflicts.is_empty(),
		"order-independence run ({order:?}) surfaced conflicts: {conflicts:?}"
	);
	let tree = walk_tree(&local_a);
	assert_tree_maps_identical(&tree, &walk_tree(&local_b), "local_a", "local_b");

	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
	tree
}
