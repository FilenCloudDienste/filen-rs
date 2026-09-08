//! Two-way conflict resolution & error tests (`CONFLICT-*`) for the sync engine.
//!
//! These are LIVE black-box tests. The hard invariant under test across the whole category is
//! NO SILENT DATA LOSS: a genuine both-sides divergence must be surfaced (held + reported) and
//! every diverged byte-stream must remain recoverable somewhere on disk or remote — never
//! clobbered. We verify recoverability by scanning the ENTIRE local tree (including the engine's
//! `.filen-sync-trash` quarantine bin, which `walk_tree` deliberately hides) for the exact bytes.
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::file::RemoteFile;
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{ConflictResolution, SyncEngine, SyncMode};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ---------------------------------------------------------------------------
// Local helpers (recoverability scanning + remote setup), built only on the
// documented harness + public Client API.
// ---------------------------------------------------------------------------

/// Recursively scan EVERY regular file under `root` — INCLUDING the `.filen-sync-trash`
/// quarantine bin that `walk_tree` hides — and return true if any file's exact bytes equal
/// `needle`. This is how we prove a diverged version was preserved recoverably (either left in
/// place, written under a conflict-renamed name, or quarantined) rather than silently destroyed.
fn bytes_recoverable_anywhere(root: &Path, needle: &[u8]) -> bool {
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
			} else if ft.is_file() && std::fs::read(entry.path()).is_ok_and(|b| b == needle) {
				return true;
			}
		}
	}
	false
}

/// Count the regular files sitting in the engine's `.filen-sync-trash` quarantine bin under
/// `root`. Zero proves a pass stashed nothing.
fn quarantined_file_count(root: &Path) -> usize {
	let mut count = 0usize;
	let mut stack = vec![root.join(".filen-sync-trash")];
	while let Some(dir) = stack.pop() {
		let rd = match std::fs::read_dir(&dir) {
			Ok(rd) => rd,
			Err(_) => continue,
		};
		for entry in rd.flatten() {
			match entry.file_type() {
				Ok(ft) if ft.is_dir() => stack.push(entry.path()),
				Ok(ft) if ft.is_file() => count += 1,
				_ => {}
			}
		}
	}
	count
}

/// Count regular files anywhere under `root` whose base name contains `frag` (case-sensitive),
/// INCLUDING the quarantine bin. Used to bound conflict-copy proliferation across passes.
fn count_files_named_containing(root: &Path, frag: &str) -> usize {
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
			} else if ft.is_file() && entry.file_name().to_string_lossy().contains(frag) {
				count += 1;
			}
		}
	}
	count
}

/// Total regular files anywhere under `root`, INCLUDING the quarantine bin.
fn total_files(root: &Path) -> usize {
	count_files_named_containing(root, "")
}

/// A `DirType` view of the shared remote root for `list_dir` / `create_dir` / file builders.
fn root_dirtype(tc: &TwoClients) -> DirType<'_, Normal> {
	DirType::<Normal>::Dir(Cow::Borrowed(&tc.resources.dir))
}

/// List the (dirs, files) directly under the shared remote root (ground truth).
async fn list_remote_root(
	tc: &TwoClients,
) -> (Vec<filen_sdk_rs::fs::dir::RemoteDirectory>, Vec<RemoteFile>) {
	tc.resources
		.client
		.list_dir(&root_dirtype(tc), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap()
}

/// Upload `data` to the remote root under `name` (versioning any same-name file in place),
/// returning the created RemoteFile. Uses the SHARED resources client (ground-truth writer).
#[allow(dead_code)]
async fn upload_remote(tc: &TwoClients, name: &str, data: &[u8]) -> RemoteFile {
	let builder = tc
		.resources
		.client
		.make_file_builder(name, tc.resources.dir.uuid())
		.unwrap();
	tc.resources
		.client
		.upload_file(builder, data)
		.await
		.unwrap()
}

fn find_file<'a>(files: &'a [RemoteFile], name: &str) -> Option<&'a RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

// ===========================================================================
// CONFLICT-01 — both sides modify same file differently -> conflict, both kept
// ===========================================================================
#[shared_test_runtime]
async fn conflict_01_both_modify_surfaces_conflict_both_versions_preserved() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// Converge a shared "BASE" baseline on both sides.
	write_file(&tc.local_a, "notes.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c01-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "notes.txt", b"BASE"),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline should be conflict-free: {conflicts:?}"
	);

	// Diverge BOTH sides.
	write_file(&tc.local_a, "notes.txt", b"LOCAL-EDIT");
	write_file(&tc.local_b, "notes.txt", b"REMOTE-EDIT");

	// Run several rounds; we do NOT require tree-equality (a held conflict need not converge).
	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
		assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		conflicts.iter().any(|c| c.contains("notes.txt")),
		"divergent edit must surface as a conflict: {conflicts:?}"
	);
	// Both versions' bytes survive across the two local trees (in place or quarantined).
	let local_present = bytes_recoverable_anywhere(&tc.local_a, b"LOCAL-EDIT")
		|| bytes_recoverable_anywhere(&tc.local_b, b"LOCAL-EDIT");
	let remote_present = bytes_recoverable_anywhere(&tc.local_a, b"REMOTE-EDIT")
		|| bytes_recoverable_anywhere(&tc.local_b, b"REMOTE-EDIT");
	assert!(local_present, "LOCAL-EDIT bytes were destroyed — data loss");
	assert!(
		remote_present,
		"REMOTE-EDIT bytes were destroyed — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-02 — conflict persists across re-run; copies do not accumulate
// ===========================================================================
#[shared_test_runtime]
async fn conflict_02_persists_across_reruns_without_copy_growth() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "notes.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c02-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "notes.txt", b"BASE"),
	)
	.await;

	write_file(&tc.local_a, "notes.txt", b"LOCAL-EDIT");
	write_file(&tc.local_b, "notes.txt", b"REMOTE-EDIT");

	// Pass 1 — surface the conflict and settle.
	let mut saw_conflict = false;
	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		saw_conflict |= ra
			.conflict_paths()
			.chain(rb.conflict_paths())
			.any(|c| c.contains("notes.txt"));
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	assert!(saw_conflict, "conflict never surfaced in the settle phase");

	// Snapshot the total file footprint of both trees (in-place + quarantine).
	let footprint = |tc: &TwoClients| total_files(&tc.local_a) + total_files(&tc.local_b);
	let after_settle = footprint(&tc);

	// Two more no-change passes: the conflict must keep being reported and the footprint must NOT
	// grow (no one-conflict-copy-per-pass runaway), and both versions stay intact.
	for pass in 0..2 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		let still = ra
			.conflict_paths()
			.chain(rb.conflict_paths())
			.any(|c| c.contains("notes.txt"));
		assert!(still, "conflict was silently cleared on rerun pass {pass}");
		assert_eq!(
			footprint(&tc),
			after_settle,
			"conflict-copy footprint grew on rerun pass {pass} (runaway duplication)"
		);
		assert!(
			bytes_recoverable_anywhere(&tc.local_a, b"LOCAL-EDIT")
				|| bytes_recoverable_anywhere(&tc.local_b, b"LOCAL-EDIT")
		);
		assert!(
			bytes_recoverable_anywhere(&tc.local_a, b"REMOTE-EDIT")
				|| bytes_recoverable_anywhere(&tc.local_b, b"REMOTE-EDIT")
		);
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-03 — local modify vs remote delete -> no data loss
// ===========================================================================
#[shared_test_runtime]
async fn conflict_03_local_modify_vs_remote_delete_preserves_local_edit() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "report.doc", b"V1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c03-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "report.doc", b"V1"),
	)
	.await;
	let (_d, rfiles) = list_remote_root(&tc).await;
	let baseline_uuid = find_file(&rfiles, "report.doc")
		.expect("the baseline never reached the remote")
		.uuid();

	// Let A observe its OWN upload before anything diverges: a remote write this engine made is
	// held over the cache snapshot until the cache accounts for it, and its path is skipped
	// meanwhile. B's delete below leaves nothing at that path to account for it with, so the hold
	// would stand for its whole grace window and swallow the divergence staged after it. One settle
	// pass retires it — SCALE-D stages its mass divergence the same way.
	assert!(
		poll_for_item(tc.cache_a.db_path(), baseline_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"A's cache never observed its own upload"
	);
	let _ = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;

	// A modifies; B deletes locally (mirrors to remote as a trash).
	write_file(&tc.local_a, "report.doc", b"V2");
	std::fs::remove_file(tc.local_b.join("report.doc")).unwrap();

	// Stage the divergence sequentially (as CONFLICT-05 does): B's delete commits first and A's
	// cache — the engine's whole remote view — observes the tombstone BEFORE A's pass, so A
	// reconciles local=Modified against remote=Deleted. Racing the two passes instead lets A push
	// V2 while its cache still shows the file, collapsing the divergence into a one-sided update
	// that no later round can surface.
	let rb0 = tc
		.engine_b
		.sync_once(tc.pair_b)
		.await
		.expect("engine B sync_once");
	assert!(rb0.errors.is_empty(), "{rb0:?}");
	let (_d, rfiles) = list_remote_root(&tc).await;
	assert!(
		find_file(&rfiles, "report.doc").is_none(),
		"B's delete never reached the remote: {rb0:?}"
	);
	assert!(
		poll_for_item_absent(tc.cache_a.db_path(), baseline_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"A's cache never observed B's delete"
	);

	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The locally-edited V2 bytes must not be destroyed by the remote tombstone.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"V2"),
		"V2 local edit was wiped by the remote delete — data loss"
	);
	// Surfaced as divergence (conflict) rather than silently honored as a clean delete on A.
	assert!(
		conflicts.iter().any(|c| c.contains("report.doc")),
		"modify-vs-delete must be surfaced: {conflicts:?}"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-04 — remote modify vs local delete -> no data loss
// ===========================================================================
#[shared_test_runtime]
async fn conflict_04_remote_modify_vs_local_delete_preserves_remote_edit() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "budget.csv", b"V1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c04-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "budget.csv", b"V1"),
	)
	.await;
	let (_d, rfiles) = list_remote_root(&tc).await;
	let baseline_uuid = find_file(&rfiles, "budget.csv")
		.expect("the baseline never reached the remote")
		.uuid();

	// Let A observe its OWN upload before anything diverges, as CONFLICT-03 must: a remote write
	// this engine made is held over the cache snapshot until the cache accounts for it. Here B's
	// re-upload leaves a foreign uuid at the path, which accounts for it on its own — the settle
	// pass makes the precondition explicit rather than incidental.
	assert!(
		poll_for_item(tc.cache_a.db_path(), baseline_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"A's cache never observed its own upload"
	);
	let _ = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;

	// B modifies (becomes the "remote" edit once pushed); A deletes locally.
	write_file(&tc.local_b, "budget.csv", b"V2");
	std::fs::remove_file(tc.local_a.join("budget.csv")).unwrap();

	// Stage the divergence sequentially (as CONFLICT-05 does): B's edit commits first and A's
	// cache observes that version BEFORE A's pass, so A reconciles local=Deleted against
	// remote=Modified. Racing the passes instead lets A mirror its delete against a stale cache,
	// collapsing the divergence into a plain one-sided delete.
	let rb0 = tc
		.engine_b
		.sync_once(tc.pair_b)
		.await
		.expect("engine B sync_once");
	assert!(rb0.errors.is_empty(), "{rb0:?}");
	let (_d, rfiles) = list_remote_root(&tc).await;
	let edited = find_file(&rfiles, "budget.csv").expect("B's edit never reached the remote");
	assert_ne!(
		edited.uuid(),
		baseline_uuid,
		"B's pass did not push the edit: {rb0:?}"
	);
	assert!(
		poll_for_item(tc.cache_a.db_path(), edited.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"A's cache never observed B's edit"
	);

	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The remotely-edited V2 bytes must survive (on B, restored to A, or remote ground truth).
	let v2_local = bytes_recoverable_anywhere(&tc.local_a, b"V2")
		|| bytes_recoverable_anywhere(&tc.local_b, b"V2");
	let (_d, rfiles) = list_remote_root(&tc).await;
	let v2_remote = find_file(&rfiles, "budget.csv").map(|f| f.size) == Some(2);
	assert!(
		v2_local || v2_remote,
		"V2 remote edit was destroyed by the local delete — data loss"
	);
	assert!(
		conflicts.iter().any(|c| c.contains("budget.csv")),
		"delete-vs-modify must be surfaced: {conflicts:?}"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-05 — create-vs-create, different content (no baseline) -> conflict
// ===========================================================================
#[shared_test_runtime]
async fn conflict_05_create_vs_create_different_content() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "fresh.txt", b"AAA");
	write_file(&tc.local_b, "fresh.txt", b"BBB");

	// Stage the divergence sequentially: A's create commits first and B's cache observes it BEFORE
	// B's first pass, so B reconciles local=Created against remote=Created with no common baseline
	// — the create-vs-create the test is about. Racing the two passes instead would have both
	// engines upload the same name, which the server linearizes into versions of one file.
	let r0 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	assert!(r0.errors.is_empty(), "{r0:?}");
	let (_d, rfiles) = list_remote_root(&tc).await;
	let uploaded = find_file(&rfiles, "fresh.txt").expect("A's create never reached the remote");
	assert!(
		poll_for_item(
			tc.cache_b.db_path(),
			uploaded.uuid(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"B's cache never observed A's create"
	);

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		conflicts.iter().any(|c| c.contains("fresh.txt")),
		"create-vs-create divergence must surface: {conflicts:?}"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"AAA")
			|| bytes_recoverable_anywhere(&tc.local_b, b"AAA"),
		"AAA content destroyed — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"BBB")
			|| bytes_recoverable_anywhere(&tc.local_b, b"BBB"),
		"BBB content destroyed — data loss"
	);

	// A further no-change pass must not duplicate conflict copies.
	let footprint = total_files(&tc.local_a) + total_files(&tc.local_b);
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	assert!(
		ra.errors.is_empty() && rb.errors.is_empty(),
		"{ra:?} {rb:?}"
	);
	assert_eq!(
		total_files(&tc.local_a) + total_files(&tc.local_b),
		footprint,
		"a no-change pass duplicated conflict copies"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-06 — create-vs-create IDENTICAL content -> no conflict, converge
// ===========================================================================
#[shared_test_runtime]
async fn conflict_06_create_vs_create_identical_content_no_conflict() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "same.txt", b"SAME");
	write_file(&tc.local_b, "same.txt", b"SAME");

	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
		&mut conflicts,
		"c06-converge",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& read_eq(&tc.local_a, "same.txt", b"SAME")
				&& read_eq(&tc.local_b, "same.txt", b"SAME")
		},
	)
	.await;

	assert!(
		conflicts.is_empty(),
		"byte-identical independent creates must NOT conflict: {conflicts:?}"
	);
	// Baseline advanced: a follow-up round is a no-op (zero counters).
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	assert_eq!(ra.uploaded + rb.uploaded, 0, "{ra:?} {rb:?}");
	assert_eq!(ra.downloaded + rb.downloaded, 0, "{ra:?} {rb:?}");
	assert_eq!(ra.conflicts.len() + rb.conflicts.len(), 0, "{ra:?} {rb:?}");
	// Exactly one file each — no spurious conflict copy.
	assert_eq!(total_files(&tc.local_a), 1, "spurious copy on A");
	assert_eq!(total_files(&tc.local_b), 1, "spurious copy on B");

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-07 — rename-vs-rename of same file to two different names
// ===========================================================================
#[shared_test_runtime]
async fn conflict_07_rename_vs_rename_file() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "doc.txt", b"X");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c07-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "doc.txt", b"X"),
	)
	.await;

	move_file(&tc.local_a, "doc.txt", "doc-local.txt");
	move_file(&tc.local_b, "doc.txt", "doc-remote.txt");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The single content "X" must survive — no phantom delete may destroy the only copy.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"X")
			|| bytes_recoverable_anywhere(&tc.local_b, b"X"),
		"the file content X was destroyed by divergent renames — data loss"
	);
	// At least one of the two trees still holds the content under SOME name.
	assert!(
		total_files(&tc.local_a) >= 1 && total_files(&tc.local_b) >= 1,
		"a tree ended up empty after rename-vs-rename"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-08 — file edited locally, replaced by directory on the remote
// ===========================================================================
#[shared_test_runtime]
async fn conflict_08_file_edit_vs_remote_type_flip_to_dir() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "thing", b"FILE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c08-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "thing", b"FILE"),
	)
	.await;

	// A: edit the file. B: delete the file, create a directory `thing/child.txt`.
	write_file(&tc.local_a, "thing", b"FILE2");
	std::fs::remove_file(tc.local_b.join("thing")).unwrap();
	write_file(&tc.local_b, "thing/child.txt", b"CHILD");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		conflicts.iter().any(|c| c.contains("thing")),
		"file<->dir type flip must surface: {conflicts:?}"
	);
	// The edited file bytes and the new directory child must both survive somewhere.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"FILE2")
			|| bytes_recoverable_anywhere(&tc.local_b, b"FILE2"),
		"local FILE2 bytes destroyed — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"CHILD")
			|| bytes_recoverable_anywhere(&tc.local_b, b"CHILD"),
		"remote dir child destroyed — data loss"
	);
	// No tree may claim `thing` as BOTH a regular file and a directory.
	for root in [&tc.local_a, &tc.local_b] {
		let p = root.join("thing");
		if p.exists() {
			let md = std::fs::symlink_metadata(&p).unwrap();
			assert!(
				md.is_file() != md.is_dir(),
				"path `thing` is ambiguous (both file and dir) in {root:?}"
			);
		}
	}

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-09 — file->dir type flip on BOTH sides simultaneously
// ===========================================================================
#[shared_test_runtime]
async fn conflict_09_double_type_flip_file_to_dir_both_sides() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "item", b"C");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c09-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "item", b"C"),
	)
	.await;

	std::fs::remove_file(tc.local_a.join("item")).unwrap();
	write_file(&tc.local_a, "item/a.txt", b"AAA");
	std::fs::remove_file(tc.local_b.join("item")).unwrap();
	write_file(&tc.local_b, "item/b.txt", b"BBB");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::Concurrent,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"AAA")
			|| bytes_recoverable_anywhere(&tc.local_b, b"AAA"),
		"a.txt content destroyed — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"BBB")
			|| bytes_recoverable_anywhere(&tc.local_b, b"BBB"),
		"b.txt content destroyed — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-13 — local-backup: local delete is NOT mirrored to remote
// ===========================================================================
#[shared_test_runtime]
async fn conflict_13_local_backup_delete_not_mirrored() {
	let sc = single_client(SyncMode::LocalBackup).await;

	write_file(&sc.local, "keep.txt", b"K");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Delete locally; remote unchanged.
	std::fs::remove_file(sc.local.join("keep.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must NOT mirror the delete: {r2:?}"
	);
	assert_eq!(
		r2.conflicts.len(),
		0,
		"delete-not-mirrored is not a conflict: {r2:?}"
	);
	assert_eq!(
		r2.downloaded, 0,
		"backup must not resurrect the deleted file: {r2:?}"
	);

	// Remote still has it; local stays deleted.
	let (_d, files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert!(
		find_file(&files, "keep.txt").is_some(),
		"remote lost the backup copy"
	);
	assert!(
		!sc.local.join("keep.txt").exists(),
		"deleted local file was resurrected"
	);

	// A third pass remains a no-op (deletion not reprocessed).
	let r3 = sc.sync().await;
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

// ===========================================================================
// CONFLICT-14 — remote-backup: remote delete is NOT mirrored locally
// ===========================================================================
#[shared_test_runtime]
async fn conflict_14_remote_backup_delete_not_mirrored() {
	let sc = single_client(SyncMode::RemoteBackup).await;

	// Seed a remote file and pull it down.
	let mut rf = {
		let b = sc
			.resources
			.client
			.make_file_builder("archive.bin", sc.resources.dir.uuid())
			.unwrap();
		sc.resources.client.upload_file(b, b"A").await.unwrap()
	};
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never saw the seeded remote file"
	);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "archive.bin", b"A"));

	// Delete on remote; wait for the cache to drop it.
	sc.resources.client.trash_file(&mut rf).await.unwrap();
	assert!(
		poll_for_item_absent(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped the trashed file"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.locally_deleted, 0,
		"backup must NOT mirror remote delete: {r2:?}"
	);
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert_eq!(
		r2.uploaded, 0,
		"backup must not re-push the kept local file: {r2:?}"
	);
	assert!(
		read_eq(&sc.local, "archive.bin", b"A"),
		"local backup copy lost"
	);

	// Remote stays empty (not re-uploaded).
	let (_d, files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert!(
		find_file(&files, "archive.bin").is_none(),
		"file resurrected on remote"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.locally_deleted, 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");

	sc.cleanup();
}

// ===========================================================================
// CONFLICT-15 — one-directional modes resolve divergence deterministically
// ===========================================================================
#[shared_test_runtime]
async fn conflict_15a_local_to_remote_local_wins_no_conflict() {
	// LocalToRemote: local "LOC" wins, no conflict surfaced. One client suffices.
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "pin.txt", b"BASE");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Diverge: local edits to LOCALWINS (9 bytes); remote independently edited to REM (3 bytes).
	// Distinct lengths so the remote `size` uniquely identifies the winner.
	write_file(&sc.local, "pin.txt", b"LOCALWINS");
	let _ = upload_remote_single(&sc, "pin.txt", b"REM").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"one-directional mode must not surface a conflict: {r2:?}"
	);

	// Remote ends with the local-wins content (size 9). Re-run is stable.
	assert_eq!(
		remote_file_size(&sc, "pin.txt").await,
		Some(9),
		"remote should hold the local-wins content (LOCALWINS)"
	);
	let r3 = sc.sync().await;
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "stable re-run must not re-upload: {r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn conflict_15b_remote_to_local_remote_wins_no_conflict() {
	// RemoteToLocal: remote "REM" wins, no conflict surfaced.
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let _base = upload_remote_single(&sc, "pin.txt", b"BASE").await;
	assert!(
		poll_for_item(sc.cache.db_path(), _base.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never saw base"
	);
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");

	// Diverge: local edited to LOC; remote re-uploaded to REM.
	write_file(&sc.local, "pin.txt", b"LOC");
	let rem = upload_remote_single(&sc, "pin.txt", b"REM").await;
	let new_uuid: Uuid = rem.uuid();
	let db = sc.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(3)
		})
		.await,
		"cache never reflected the new remote content"
	);

	// Allow a bounded eventual-consistency window for the redownload.
	let mut r2 = sc.sync().await;
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while !read_eq(&sc.local, "pin.txt", b"REM") && std::time::Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(500)).await;
		r2 = sc.sync().await;
	}
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"one-directional mode must not surface a conflict: {r2:?}"
	);
	assert!(
		read_eq(&sc.local, "pin.txt", b"REM"),
		"local should hold the remote-wins content"
	);
	// The overwritten local "LOC" should be preserved recoverably (quarantine), not hard-destroyed.
	assert!(
		bytes_recoverable_anywhere(&sc.local, b"LOC"),
		"overwritten local LOC content was hard-destroyed with no recovery copy"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn conflict_15c_unmodified_local_refresh_leaves_no_quarantine_copy() {
	// The counterpart to 15b: the pull-overwrite stash must be surgical. A plain refresh of a local
	// file that still matches its baseline destroys nothing, so it must NOT leave a recovery copy
	// behind — otherwise every remote edit doubles the local disk usage.
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let v1 = upload_remote_single(&sc, "fresh.txt", b"V1").await;
	assert!(
		poll_for_item(sc.cache.db_path(), v1.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never saw V1"
	);
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "fresh.txt", b"V1"), "V1 did not land");

	// Remote-only edit; the local copy is untouched since that sync.
	let v2 = upload_remote_single(&sc, "fresh.txt", b"V222").await;
	let new_uuid: Uuid = v2.uuid();
	let db = sc.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(4)
		})
		.await,
		"cache never reflected the new remote content"
	);

	let mut r2 = sc.sync().await;
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while !read_eq(&sc.local, "fresh.txt", b"V222") && std::time::Instant::now() < deadline {
		tokio::time::sleep(Duration::from_millis(500)).await;
		r2 = sc.sync().await;
	}
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		read_eq(&sc.local, "fresh.txt", b"V222"),
		"the refresh did not land"
	);
	assert_eq!(
		quarantined_file_count(&sc.local),
		0,
		"an unmodified refresh must not stash a recovery copy"
	);

	sc.cleanup();
}

// ===========================================================================
// CONFLICT-20 — convergent edit (same new bytes both sides) is NOT a conflict
// ===========================================================================
#[shared_test_runtime]
async fn conflict_20_convergent_edit_not_a_conflict() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "agree.txt", b"OLD");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c20-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "agree.txt", b"OLD"),
	)
	.await;

	// Both sides make the IDENTICAL edit.
	write_file(&tc.local_a, "agree.txt", b"NEW");
	write_file(&tc.local_b, "agree.txt", b"NEW");

	let mut conflicts2 = BTreeSet::new();
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
		&mut conflicts2,
		"c20-converge",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& read_eq(&tc.local_a, "agree.txt", b"NEW")
				&& read_eq(&tc.local_b, "agree.txt", b"NEW")
		},
	)
	.await;

	assert!(
		conflicts2.is_empty(),
		"identical edit on both sides must NOT conflict: {conflicts2:?}"
	);
	assert_eq!(total_files(&tc.local_a), 1, "spurious conflict copy on A");
	assert_eq!(total_files(&tc.local_b), 1, "spurious conflict copy on B");

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-21 — modify-vs-delete crossing the mass-delete confirmation gate
// ===========================================================================
#[shared_test_runtime]
async fn conflict_21_mass_delete_gate_with_modify_vs_delete() {
	// One engine, RemoteToLocal: remote mass-deletes; local edits 3 files. The mass-delete guard
	// must hold the deletions AND the 3 local edits must survive.
	let sc = single_client(SyncMode::RemoteToLocal).await;

	const TOTAL: usize = 50;
	let mut uuids = Vec::new();
	for i in 0..TOTAL {
		let b = sc
			.resources
			.client
			.make_file_builder(&format!("f{i:02}.txt"), sc.resources.dir.uuid())
			.unwrap();
		let rf = sc
			.resources
			.client
			.upload_file(b, format!("base {i}").as_bytes())
			.await
			.unwrap();
		uuids.push(rf);
	}
	// Wait for the cache to hold the full set.
	let last = uuids.last().unwrap().uuid();
	assert!(
		poll_for_item(sc.cache.db_path(), last, CACHE_CONVERGE_TIMEOUT).await,
		"cache never converged the 50 files"
	);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, TOTAL, "{r1:?}");

	// Locally modify 3 of them.
	write_file(&sc.local, "f00.txt", b"LOCAL-EDIT-0");
	write_file(&sc.local, "f01.txt", b"LOCAL-EDIT-1");
	write_file(&sc.local, "f02.txt", b"LOCAL-EDIT-2");

	// Remotely trash ALL 50 (mass deletion).
	for rf in uuids.iter_mut() {
		sc.resources.client.trash_file(rf).await.unwrap();
	}
	assert!(
		poll_for_item_absent(sc.cache.db_path(), last, CACHE_CONVERGE_TIMEOUT).await,
		"cache never reflected the mass trash"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.held_deletions() > 0 || r2.guard.is_some(),
		"mass-delete guard must engage: {r2:?}"
	);
	// The 3 edited files keep their new local content; nothing was destroyed under the held batch.
	assert!(
		read_eq(&sc.local, "f00.txt", b"LOCAL-EDIT-0"),
		"edit 0 lost"
	);
	assert!(
		read_eq(&sc.local, "f01.txt", b"LOCAL-EDIT-1"),
		"edit 1 lost"
	);
	assert!(
		read_eq(&sc.local, "f02.txt", b"LOCAL-EDIT-2"),
		"edit 2 lost"
	);
	assert_eq!(
		r2.locally_deleted, 0,
		"guard should have prevented destructive deletion: {r2:?}"
	);

	sc.cleanup();
}

// ===========================================================================
// CONFLICT-22 — local rename vs remote content edit of the same item
// ===========================================================================
#[shared_test_runtime]
async fn conflict_22_local_rename_vs_remote_modify() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "m.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c22-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "m.txt", b"BASE"),
	)
	.await;

	// A renames m.txt -> renamed.txt; B edits m.txt content to EDIT.
	move_file(&tc.local_a, "m.txt", "renamed.txt");
	write_file(&tc.local_b, "m.txt", b"EDIT");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The remote EDIT bytes must survive somewhere (not lost to the rename).
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"EDIT")
			|| bytes_recoverable_anywhere(&tc.local_b, b"EDIT"),
		"remote EDIT content destroyed by the local rename — data loss"
	);
	// No stale BASE duplicate may linger at both m.txt and renamed.txt with old bytes on one side.
	// (We assert no data loss + at least the EDIT survives; the precise placement is engine policy.)

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-23 — directory deleted on one side vs new child added on the other
// ===========================================================================
#[shared_test_runtime]
async fn conflict_23_dir_delete_vs_new_child() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "d/old.txt", b"OLD");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c23-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "d/old.txt", b"OLD"),
	)
	.await;

	// B deletes the whole directory; A adds a new child inside it.
	std::fs::remove_dir_all(tc.local_b.join("d")).unwrap();
	write_file(&tc.local_a, "d/new.txt", b"NEW");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The freshly-added child must survive the recursive remote deletion.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"NEW")
			|| bytes_recoverable_anywhere(&tc.local_b, b"NEW"),
		"d/new.txt content destroyed by the remote directory deletion — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-24 — conflict-copy bytes are exact (1 MiB, no truncation)
// ===========================================================================
#[shared_test_runtime]
async fn conflict_24_conflict_copy_bytes_exact_binary() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// 1 MiB deterministic pseudo-random base payload.
	let base: Vec<u8> = (0..(1024 * 1024))
		.map(|i| ((i * 2654435761usize) >> 13) as u8)
		.collect();
	std::fs::write(tc.local_a.join("blob.bin"), &base).unwrap();
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c24-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && tc.local_b.join("blob.bin").is_file(),
	)
	.await;

	// Diverge: distinct trailing bytes on each side.
	let mut local_v = base.clone();
	local_v.extend_from_slice(b"-LOCAL-TAIL");
	let mut remote_v = base.clone();
	remote_v.extend_from_slice(b"-REMOTE-TAIL-DIFFERENT");
	std::fs::write(tc.local_a.join("blob.bin"), &local_v).unwrap();
	std::fs::write(tc.local_b.join("blob.bin"), &remote_v).unwrap();

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		conflicts.iter().any(|c| c.contains("blob.bin")),
		"binary divergence must surface as a conflict: {conflicts:?}"
	);
	// Both exact byte streams (correct length, byte-for-byte) must be retrievable.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, &local_v)
			|| bytes_recoverable_anywhere(&tc.local_b, &local_v),
		"local 1 MiB+tail payload truncated/corrupted in preservation"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, &remote_v)
			|| bytes_recoverable_anywhere(&tc.local_b, &remote_v),
		"remote 1 MiB+tail payload truncated/corrupted in preservation"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-A1 (review add) — move-vs-modify (cross-directory relocate vs edit)
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_move_vs_modify() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "proj/spec.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"cA1-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "proj/spec.txt", b"BASE"),
	)
	.await;

	// A moves proj/spec.txt -> archive/spec.txt; B edits proj/spec.txt to EDIT.
	move_file(&tc.local_a, "proj/spec.txt", "archive/spec.txt");
	write_file(&tc.local_b, "proj/spec.txt", b"EDIT");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The remote EDIT bytes must survive the relocation.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"EDIT")
			|| bytes_recoverable_anywhere(&tc.local_b, b"EDIT"),
		"remote EDIT bytes lost to the cross-dir move — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-A2 (review add) — directory rename-vs-rename with children
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_dir_rename_vs_rename() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "docs/a.txt", b"AA");
	write_file(&tc.local_a, "docs/b.txt", b"BB");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"cA2-baseline",
		|| {
			trees_equal(&tc.local_a, &tc.local_b)
				&& read_eq(&tc.local_b, "docs/a.txt", b"AA")
				&& read_eq(&tc.local_b, "docs/b.txt", b"BB")
		},
	)
	.await;

	// Divergent directory renames.
	move_file(&tc.local_a, "docs/a.txt", "docs-local/a.txt");
	move_file(&tc.local_a, "docs/b.txt", "docs-local/b.txt");
	std::fs::remove_dir_all(tc.local_a.join("docs")).ok();
	move_file(&tc.local_b, "docs/a.txt", "docs-remote/a.txt");
	move_file(&tc.local_b, "docs/b.txt", "docs-remote/b.txt");
	std::fs::remove_dir_all(tc.local_b.join("docs")).ok();

	for _ in 0..10 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// No child content may be lost — a phantom recursive delete must not destroy the only copy.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"AA")
			|| bytes_recoverable_anywhere(&tc.local_b, b"AA"),
		"a.txt destroyed by divergent dir rename — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"BB")
			|| bytes_recoverable_anywhere(&tc.local_b, b"BB"),
		"b.txt destroyed by divergent dir rename — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-A3 (review add) — remote rename vs local modify (mirror of -22)
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_remote_rename_vs_local_modify() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "r.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"cA3-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "r.txt", b"BASE"),
	)
	.await;

	// A edits content (LOCAL-EDIT); B renames r.txt -> r-renamed.txt.
	write_file(&tc.local_a, "r.txt", b"LOCAL-EDIT");
	move_file(&tc.local_b, "r.txt", "r-renamed.txt");

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"LOCAL-EDIT")
			|| bytes_recoverable_anywhere(&tc.local_b, b"LOCAL-EDIT"),
		"local edit lost to the remote rename — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-A4 (review add) — backup modes do NOT surface a conflict on content
// divergence (additive, deterministic resolution; no data loss)
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_local_backup_content_divergence_no_conflict() {
	// LocalBackup: local LOC wins, no conflict; prior remote REM preserved recoverably (remote keeps
	// its version chain). One client suffices.
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "b.txt", b"BASE");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// local LOCALWINS (9 bytes) vs remote REM (3 bytes) — distinct lengths for size discrimination.
	write_file(&sc.local, "b.txt", b"LOCALWINS");
	let _ = upload_remote_single(&sc, "b.txt", b"REM").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"backup mode must not surface a conflict: {r2:?}"
	);

	// Local stays LOCALWINS; remote ends pushed to it (local is source of truth).
	assert!(read_eq(&sc.local, "b.txt", b"LOCALWINS"));
	assert_eq!(
		remote_file_size(&sc, "b.txt").await,
		Some(9),
		"remote should hold the pushed LOCALWINS content"
	);

	// Re-run stable.
	let r3 = sc.sync().await;
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");

	sc.cleanup();
}

// ===========================================================================
// CONFLICT-A5 (review add) — conflict isolated to one pair does not leak into a
// second pair on the same relative filename (multi-pair on ONE engine each)
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_pair_isolation() {
	// Build two two-way pairs sharing the SAME remote root but DIFFERENT local roots, one pair per
	// engine. Pair A's both-sides divergence must not raise a conflict or alter pair B. Because both
	// pairs map the same remote dir, we keep the filenames disjoint per pair to make "no leakage"
	// observable on the remote (pair A touches a-shared.txt, pair B touches b-shared.txt).
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "a-shared.txt", b"BASE-A");
	write_file(&tc.local_b, "b-shared.txt", b"BASE-B");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"cA5-baseline",
		|| {
			read_eq(&tc.local_a, "a-shared.txt", b"BASE-A")
				&& read_eq(&tc.local_a, "b-shared.txt", b"BASE-B")
				&& read_eq(&tc.local_b, "a-shared.txt", b"BASE-A")
				&& read_eq(&tc.local_b, "b-shared.txt", b"BASE-B")
		},
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline should be clean: {conflicts:?}"
	);

	// Diverge a-shared.txt on BOTH local roots (pair A's file): A->Ala, B->Ara.
	write_file(&tc.local_a, "a-shared.txt", b"Ala");
	write_file(&tc.local_b, "a-shared.txt", b"Ara");
	// b-shared.txt left untouched on both.
	let b_a_before = std::fs::read(tc.local_a.join("b-shared.txt")).unwrap();
	let b_b_before = std::fs::read(tc.local_b.join("b-shared.txt")).unwrap();

	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The conflict must be confined to a-shared.txt; b-shared.txt must not be implicated.
	assert!(
		conflicts.iter().any(|c| c.contains("a-shared.txt")),
		"a-shared.txt divergence must surface: {conflicts:?}"
	);
	assert!(
		!conflicts.iter().any(|c| c.contains("b-shared.txt")),
		"untouched b-shared.txt must NOT be dragged into a conflict: {conflicts:?}"
	);
	// b-shared.txt bytes unchanged on both sides (no cross-contamination).
	assert_eq!(
		std::fs::read(tc.local_a.join("b-shared.txt")).unwrap(),
		b_a_before
	);
	assert_eq!(
		std::fs::read(tc.local_b.join("b-shared.txt")).unwrap(),
		b_b_before
	);
	// Both diverged copies of a-shared.txt survive.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"Ala")
			|| bytes_recoverable_anywhere(&tc.local_b, b"Ala")
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"Ara")
			|| bytes_recoverable_anywhere(&tc.local_b, b"Ara")
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-A6 (review add) — sub-threshold directory delete vs edited child
// ===========================================================================
#[shared_test_runtime]
async fn conflict_add_subthreshold_dir_delete_vs_edited_child() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	write_file(&tc.local_a, "d/keep.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"cA6-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "d/keep.txt", b"BASE"),
	)
	.await;

	// A edits the single child; B deletes the whole (single-file, sub-threshold) directory.
	write_file(&tc.local_a, "d/keep.txt", b"EDIT");
	std::fs::remove_dir_all(tc.local_b.join("d")).unwrap();

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The edited child must survive the sub-threshold directory deletion.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"EDIT")
			|| bytes_recoverable_anywhere(&tc.local_b, b"EDIT"),
		"d/keep.txt EDIT bytes steamrolled by the sub-threshold dir delete — data loss"
	);

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-25 — two-way conflict during watch mode does not loop infinitely
// ===========================================================================
#[ignore = "blocked: same watch-mode pass-timing nondeterminism as watch_18 (a debounced pass may \
push/pull one side before the other is observed, dissolving the conflict), compounded by two \
independent caches sharing one remote dir plus server same-name versioning on the concurrent pushes \
— a genuine both-sides conflict cannot be reliably staged live. Needs the mock-server + single-step \
control-plane seam. TODO"]
#[shared_test_runtime]
async fn conflict_25_watch_mode_conflict_does_not_loop() {
	// Built inline (not via `two_clients`) because `watch` consumes `Arc<SyncEngine>` and the engines
	// must therefore be owned as `Arc`s — the harness fixture owns them by value.
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache_a = TestCache::new(&resources.client, remote).await;
	let cache_b = TestCache::new(&resources.client, remote).await;
	let local_a = fresh_local_dir("c25a");
	let local_b = fresh_local_dir("c25b");
	let engine_a = Arc::new(
		SyncEngine::open(cache_a.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let engine_b = Arc::new(
		SyncEngine::open(cache_b.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair_a = engine_a
		.add_pair(local_a.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	let pair_b = engine_b
		.add_pair(local_b.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	// Converge a baseline first (so the divergence is a genuine both-sides-changed conflict).
	let mut conflicts = BTreeSet::new();
	write_file(&local_a, "watched.txt", b"BASE");
	converge(
		&engine_a,
		pair_a,
		&engine_b,
		pair_b,
		Order::AFirst,
		&mut conflicts,
		"c25-baseline",
		|| trees_equal(&local_a, &local_b) && read_eq(&local_b, "watched.txt", b"BASE"),
	)
	.await;

	// Start watch on BOTH pairs.
	let handle_a = engine_a.clone().watch(pair_a).await.unwrap();
	let handle_b = engine_b.clone().watch(pair_b).await.unwrap();

	// Diverge under the active watch.
	write_file(&local_a, "watched.txt", b"L");
	write_file(&local_b, "watched.txt", b"R");

	// Let several debounced + safety-net cycles run.
	tokio::time::sleep(Duration::from_secs(25)).await;

	// Snapshot the conflict-copy footprint, wait more, and confirm it has quiesced (bounded growth).
	let footprint1 = total_files(&local_a) + total_files(&local_b);
	tokio::time::sleep(Duration::from_secs(20)).await;
	let footprint2 = total_files(&local_a) + total_files(&local_b);
	assert!(
		footprint2 <= footprint1 + 1,
		"watch-mode conflict handling kept spawning copies (footprint {footprint1} -> {footprint2}) — loop"
	);

	// Both diverged versions survive.
	assert!(
		bytes_recoverable_anywhere(&local_a, b"L") || bytes_recoverable_anywhere(&local_b, b"L"),
		"L bytes lost under watch"
	);
	assert!(
		bytes_recoverable_anywhere(&local_a, b"R") || bytes_recoverable_anywhere(&local_b, b"R"),
		"R bytes lost under watch"
	);

	drop(handle_a);
	drop(handle_b);
	std::fs::remove_dir_all(&local_a).ok();
	std::fs::remove_dir_all(&local_b).ok();
}

// ---------------------------------------------------------------------------
// Single-client remote-setup helpers (used by the one-directional / backup
// content-divergence tests above).
// ---------------------------------------------------------------------------

/// Upload `data` under `name` to the SingleClient's remote root (versioning in place).
async fn upload_remote_single(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
	let b = sc
		.resources
		.client
		.make_file_builder(name, sc.resources.dir.uuid())
		.unwrap();
	sc.resources.client.upload_file(b, data).await.unwrap()
}

/// Wait until the engine's cache has observed `uuid` (its remote view is fed by the cache, not by
/// the server directly), so the next pass reconciles against fresh truth.
async fn wait_cache_has(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// Wait until the cache has observed the CURRENT remote version of `name` — i.e. the version the
/// engine itself just pushed — so a follow-up pass is not reconciled against a stale snapshot.
async fn wait_cache_current(sc: &SingleClient, name: &str) {
	let uuid = remote_file_uuid(sc, name)
		.await
		.unwrap_or_else(|| panic!("remote file {name} does not exist"));
	wait_cache_has(sc, uuid).await;
}

/// The uuid of the CURRENT remote version of `name` under the SingleClient's root (ground truth).
async fn remote_file_uuid(sc: &SingleClient, name: &str) -> Option<Uuid> {
	let (_d, files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	files
		.into_iter()
		.find(|f| f.name() == Some(name))
		.map(|f| f.uuid())
}

/// The CURRENT remote version of file `name` directly under a test resources root (ground truth).
async fn remote_file_in(resources: &test_utils::TestResources, name: &str) -> Option<RemoteFile> {
	let (_d, files) = resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	files.into_iter().find(|f| f.name() == Some(name))
}

/// The current `size` of remote file `name` directly under a test resources root (ground truth).
async fn remote_file_size_in(resources: &test_utils::TestResources, name: &str) -> Option<u64> {
	remote_file_in(resources, name).await.map(|f| f.size)
}

/// The current `size` of remote file `name` under the SingleClient's root (ground truth via a
/// fresh listing). Returns None if absent. We assert on size — not raw bytes — because the public
/// native download path is not exposed as a simple `Client` method here; pick distinct-length
/// sentinel contents so size uniquely identifies which version won.
async fn remote_file_size(sc: &SingleClient, name: &str) -> Option<u64> {
	let (_d, files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	files
		.into_iter()
		.find(|f| f.name() == Some(name))
		.map(|f| f.size)
}

// ===========================================================================
// BLOCKED tests — require infrastructure the current harness does not expose.
// ===========================================================================

#[ignore = "blocked: needs a case-insensitive-FS guarantee + a mismatched-name-hash seam to make two \
distinct remote items collide on one local path (the blackbox suite's `remote_case_collision_refused` \
uses the `malformed` feature's create_*_with_name_hash, which the harness does not surface). TODO: \
expose a colliding-remote-setup helper / run under -F malformed."]
#[shared_test_runtime]
async fn conflict_10_case_only_collision() {
	// plan: create Readme.txt("lower") and README.TXT("UPPER") on remote (distinct items); sync to a
	// case-insensitive local FS; assert neither silently merged, collision held/reported, both bytes
	// retrievable, re-run stable.
}

#[ignore = "blocked: needs a name-normalizing local FS guarantee (NFC<->NFD equivalence on \
macOS/HFS) AND a way to create two normalization-variant names on the remote that the server keeps \
distinct — the public client API normalizes/dedups names, so the precondition cannot be staged. \
TODO: name-hash-bypass seam + normalizing-FS fixture."]
#[shared_test_runtime]
async fn conflict_11_unicode_normalization_collision() {
	// plan: create NFC "café.txt"("NFC") and NFD "café.txt"("NFD") on remote; sync to a normalizing
	// local FS; assert not merged, collision surfaced/both forms preserved, re-run stable.
}

#[ignore = "blocked: needs to create remote items whose names contain locally-illegal chars (e.g. \
\"a:b.txt\") that sanitize to a shared local path. make_file_builder rejects/normalizes such names \
via the public API, so the colliding precondition cannot be staged without a name-validation bypass \
seam. TODO: malformed-name remote-setup helper."]
#[shared_test_runtime]
async fn conflict_12_sanitization_collision_onto_one_local_path() {
	// plan: create two remote items sanitizing to the same local path with different content; sync;
	// assert both preserved (one disambiguated) or collision held; deterministic re-run.
}

/// CONFLICT-16 — resolving a held conflict KEEP-LOCAL pushes the local copy on the next pass.
///
/// (The original plan also wanted the losing remote version "preserved recoverably". Under the
/// engine's resolution model that is what `KeepBoth` is for — see `conflict_19` — while `KeepLocal`
/// deliberately makes the local copy win outright.)
#[shared_test_runtime]
async fn conflict_16_resolve_keep_local() {
	let sc = single_client(SyncMode::TwoWay).await;

	// Converge a shared baseline, then diverge BOTH sides (distinct lengths name the winner).
	write_file(&sc.local, "conf.txt", b"BASE");
	assert_eq!(sc.sync().await.uploaded, 1);
	write_file(&sc.local, "conf.txt", b"LLLLLLLLLL");
	let remote_version = upload_remote_single(&sc, "conf.txt", b"RRR").await;
	wait_cache_has(&sc, remote_version.uuid()).await;

	// The divergence is surfaced and HELD: no transfer, and it does not silently resolve itself.
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.conflict_paths().any(|c| c == "conf.txt"),
		"divergence must surface: {r2:?}"
	);
	assert_eq!(
		r2.uploaded + r2.downloaded,
		0,
		"a held conflict moves nothing: {r2:?}"
	);
	let r3 = sc.sync().await;
	assert!(
		r3.conflict_paths().any(|c| c == "conf.txt"),
		"the conflict must stay held until resolved: {r3:?}"
	);
	assert_eq!(r3.uploaded + r3.downloaded, 0, "{r3:?}");

	// Resolve keep-LOCAL; the next pass pushes the local copy and clears the hold.
	sc.engine
		.resolve_conflict(sc.pair, "conf.txt", ConflictResolution::KeepLocal)
		.await
		.expect("resolve keep-local");
	let r4 = sc.sync().await;
	assert!(r4.errors.is_empty(), "{r4:?}");
	assert!(r4.conflicts.is_empty(), "conflict not cleared: {r4:?}");
	assert_eq!(
		r4.uploaded, 1,
		"keep-local must push the local copy: {r4:?}"
	);
	assert_eq!(r4.downloaded, 0, "keep-local must not pull: {r4:?}");

	// Both sides now hold the local-wins bytes.
	assert!(read_eq(&sc.local, "conf.txt", b"LLLLLLLLLL"));
	assert_eq!(
		remote_file_size(&sc, "conf.txt").await,
		Some(10),
		"remote must hold the local-wins content"
	);

	// Settled: the resolved conflict never returns and the winner is never clobbered.
	wait_cache_current(&sc, "conf.txt").await;
	let r5 = sc.sync().await;
	assert!(r5.conflicts.is_empty(), "the conflict came back: {r5:?}");
	assert_eq!(
		r5.uploaded + r5.downloaded,
		0,
		"re-run must be a no-op: {r5:?}"
	);
	assert!(
		read_eq(&sc.local, "conf.txt", b"LLLLLLLLLL"),
		"the keep-local winner was clobbered"
	);

	sc.cleanup();
}

/// CONFLICT-17 — resolving a held conflict KEEP-REMOTE pulls the remote copy on the next pass.
///
/// (As with `conflict_16`, keeping the LOSING side recoverable is `KeepBoth`'s job — `KeepRemote`
/// deliberately lets the remote copy win outright.)
#[shared_test_runtime]
async fn conflict_17_resolve_keep_remote() {
	let sc = single_client(SyncMode::TwoWay).await;

	write_file(&sc.local, "conf2.txt", b"BASE");
	assert_eq!(sc.sync().await.uploaded, 1);
	write_file(&sc.local, "conf2.txt", b"LLLLLLLLLL");
	let remote_version = upload_remote_single(&sc, "conf2.txt", b"RRR").await;
	wait_cache_has(&sc, remote_version.uuid()).await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.conflict_paths().any(|c| c == "conf2.txt"),
		"divergence must surface: {r2:?}"
	);

	sc.engine
		.resolve_conflict(sc.pair, "conf2.txt", ConflictResolution::KeepRemote)
		.await
		.expect("resolve keep-remote");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert!(r3.conflicts.is_empty(), "conflict not cleared: {r3:?}");
	assert_eq!(
		r3.downloaded, 1,
		"keep-remote must pull the remote copy: {r3:?}"
	);
	assert_eq!(r3.uploaded, 0, "keep-remote must not push: {r3:?}");

	// Both sides now hold the remote-wins bytes.
	assert!(read_eq(&sc.local, "conf2.txt", b"RRR"));
	assert_eq!(remote_file_size(&sc, "conf2.txt").await, Some(3));

	let r4 = sc.sync().await;
	assert!(r4.conflicts.is_empty(), "the conflict came back: {r4:?}");
	assert_eq!(
		r4.uploaded + r4.downloaded,
		0,
		"re-run must be a no-op: {r4:?}"
	);
	assert!(read_eq(&sc.local, "conf2.txt", b"RRR"));

	sc.cleanup();
}

/// CONFLICT-19 — two sequential conflicts on ONE path, each resolved KEEP-BOTH, produce distinctly
/// named preserved copies: `a.old.txt` then `a.old.1.txt`, never a collision or a re-ingest loop.
#[shared_test_runtime]
async fn conflict_19_conflict_copy_naming_no_collision_or_recursion() {
	let sc = single_client(SyncMode::TwoWay).await;

	write_file(&sc.local, "a.txt", b"BASE");
	assert_eq!(sc.sync().await.uploaded, 1);

	// --- first conflict, resolved keep-both -> a.old.txt ---
	write_file(&sc.local, "a.txt", b"LOCAL-1");
	let v1 = upload_remote_single(&sc, "a.txt", b"REMOTE-1").await;
	wait_cache_has(&sc, v1.uuid()).await;
	assert!(
		sc.sync().await.conflict_paths().any(|c| c == "a.txt"),
		"first conflict must surface"
	);
	sc.engine
		.resolve_conflict(sc.pair, "a.txt", ConflictResolution::KeepBoth)
		.await
		.expect("first keep-both");
	assert!(
		read_eq(&sc.local, "a.old.txt", b"LOCAL-1"),
		"keep-both must preserve the local copy as a.old.txt"
	);
	let r = sc.sync().await;
	assert!(r.errors.is_empty(), "{r:?}");
	assert!(r.conflicts.is_empty(), "first conflict not cleared: {r:?}");
	assert!(
		read_eq(&sc.local, "a.txt", b"REMOTE-1"),
		"remote copy not pulled"
	);
	assert_eq!(
		r.uploaded, 1,
		"the preserved copy uploads as a new file: {r:?}"
	);
	wait_cache_current(&sc, "a.txt").await;
	wait_cache_current(&sc, "a.old.txt").await;

	// --- second, independent conflict on the SAME path, resolved keep-both -> a.old.1.txt ---
	write_file(&sc.local, "a.txt", b"LOCAL-2");
	let v2 = upload_remote_single(&sc, "a.txt", b"REMOTE-2").await;
	wait_cache_has(&sc, v2.uuid()).await;
	assert!(
		sc.sync().await.conflict_paths().any(|c| c == "a.txt"),
		"second conflict must surface"
	);
	sc.engine
		.resolve_conflict(sc.pair, "a.txt", ConflictResolution::KeepBoth)
		.await
		.expect("second keep-both");

	// A DISTINCT name: the first preserved copy is untouched, the second gets its own.
	assert!(
		read_eq(&sc.local, "a.old.txt", b"LOCAL-1"),
		"the first preserved copy was overwritten — data loss"
	);
	assert!(
		read_eq(&sc.local, "a.old.1.txt", b"LOCAL-2"),
		"the second preserved copy must get a distinct name"
	);

	let r = sc.sync().await;
	assert!(r.errors.is_empty(), "{r:?}");
	assert!(r.conflicts.is_empty(), "second conflict not cleared: {r:?}");
	assert!(read_eq(&sc.local, "a.txt", b"REMOTE-2"));

	// Exactly the three files exist — no runaway copies, no `.old.old.` recursion.
	wait_cache_current(&sc, "a.txt").await;
	wait_cache_current(&sc, "a.old.1.txt").await;
	let r = sc.sync().await;
	assert!(r.conflicts.is_empty(), "{r:?}");
	assert_eq!(
		count_files_named_containing(&sc.local, ".old"),
		2,
		"exactly two preserved copies: {:?}",
		walk_tree(&sc.local).keys().collect::<Vec<_>>()
	);
	assert_eq!(total_files(&sc.local), 3, "unexpected extra files");

	sc.cleanup();
}

/// (review-add) — the copy a KEEP-BOTH resolution leaves behind round-trips as an ORDINARY file:
/// it uploads exactly once, is retained across further passes, does not re-surface the original
/// conflict, and does not multiply.
#[shared_test_runtime]
async fn conflict_add_resolved_copy_round_trips() {
	let sc = single_client(SyncMode::TwoWay).await;

	write_file(&sc.local, "c.txt", b"BASE");
	assert_eq!(sc.sync().await.uploaded, 1);
	write_file(&sc.local, "c.txt", b"LOCAL-EDIT");
	let version = upload_remote_single(&sc, "c.txt", b"REMOTE-EDIT").await;
	wait_cache_has(&sc, version.uuid()).await;
	assert!(
		sc.sync().await.conflict_paths().any(|c| c == "c.txt"),
		"conflict must surface"
	);

	sc.engine
		.resolve_conflict(sc.pair, "c.txt", ConflictResolution::KeepBoth)
		.await
		.expect("keep-both");

	// Pass 1 after the resolution: the copy uploads once, the remote copy is pulled.
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert!(r1.conflicts.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "the leftover copy uploads once: {r1:?}");
	assert_eq!(r1.downloaded, 1, "the remote winner is pulled: {r1:?}");
	assert!(read_eq(&sc.local, "c.old.txt", b"LOCAL-EDIT"));
	assert!(read_eq(&sc.local, "c.txt", b"REMOTE-EDIT"));

	// Pass 2 and 3: no re-upload, no returning conflict, no copy growth.
	wait_cache_current(&sc, "c.txt").await;
	wait_cache_current(&sc, "c.old.txt").await;
	let footprint = total_files(&sc.local);
	for pass in 0..2 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "pass {pass}: {r:?}");
		assert!(
			r.conflicts.is_empty(),
			"conflict returned on pass {pass}: {r:?}"
		);
		assert_eq!(
			r.uploaded + r.downloaded,
			0,
			"pass {pass} must be a no-op: {r:?}"
		);
		assert_eq!(
			total_files(&sc.local),
			footprint,
			"copies multiplied on pass {pass}"
		);
	}
	assert!(
		read_eq(&sc.local, "c.old.txt", b"LOCAL-EDIT"),
		"the copy was lost"
	);
	assert!(
		remote_file_size(&sc, "c.old.txt").await == Some(b"LOCAL-EDIT".len() as u64),
		"the copy never reached the remote"
	);

	sc.cleanup();
}

/// CONFLICT-18 — a held conflict lives in the PERSISTED baseline, not in engine memory: a fresh
/// engine opened on the same baseline DB (the process-restart analogue) still reports it, still
/// moves nothing and duplicates nothing, and resolving it there propagates normally.
#[shared_test_runtime]
async fn conflict_18_conflict_persists_across_restart() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let remote: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, remote).await;
	wait_for_converged_resync(&cache.messages, remote, 0, CACHE_CONVERGE_TIMEOUT).await;
	let local = fresh_local_dir("c18");
	// A STABLE baseline path (unlike the harness's private random one) is what makes the restart
	// real: the second engine must load exactly the store the first one wrote.
	let db_path = temp_cache_path();

	let engine1 = SyncEngine::open(cache.client.clone(), db_path.clone())
		.await
		.unwrap();
	let pair1 = engine1
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();

	// Converge a baseline, then diverge both sides (distinct lengths name the winner).
	write_file(&local, "persist.txt", b"BASE");
	let r1 = engine1.sync_once(pair1).await.unwrap();
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	// Let the cache announce the engine's own upload before the sideband edit replaces its uuid —
	// the staging CONFLICT-03/04 use. The foreign uuid that edit leaves at the path would retire
	// the held write on its own here; the settle pass makes the precondition explicit.
	let base_uuid = remote_file_in(&resources, "persist.txt")
		.await
		.expect("the baseline never reached the remote")
		.uuid();
	assert!(
		poll_for_item(cache.db_path(), base_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"the cache never observed the engine's own upload"
	);
	let settle = engine1.sync_once(pair1).await.unwrap();
	assert!(settle.errors.is_empty(), "{settle:?}");

	write_file(&local, "persist.txt", b"LLLLLLLLLL");
	let builder = resources
		.client
		.make_file_builder("persist.txt", remote)
		.unwrap();
	let remote_version = resources.client.upload_file(builder, b"RRR").await.unwrap();
	assert!(
		poll_for_item(
			cache.db_path(),
			remote_version.uuid(),
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"the cache never observed the remote edit"
	);

	let r2 = engine1.sync_once(pair1).await.unwrap();
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.conflict_paths().any(|c| c == "persist.txt"),
		"divergence must surface: {r2:?}"
	);
	assert_eq!(
		r2.uploaded + r2.downloaded,
		0,
		"a held conflict moves nothing: {r2:?}"
	);
	drop(engine1);

	// Restart: a brand-new engine on the SAME baseline DB, same pair.
	let engine2 = SyncEngine::open(cache.client.clone(), db_path)
		.await
		.unwrap();
	let pair2 = engine2
		.add_pair(local.clone(), remote, SyncMode::TwoWay)
		.await
		.unwrap();
	assert_eq!(pair2, pair1, "re-registering the pair must reuse its id");

	let r3 = engine2.sync_once(pair2).await.unwrap();
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert!(
		r3.conflict_paths().any(|c| c == "persist.txt"),
		"the held conflict must survive the restart: {r3:?}"
	);
	assert_eq!(
		r3.uploaded + r3.downloaded,
		0,
		"the restarted engine must not silently auto-resolve: {r3:?}"
	);
	assert!(
		read_eq(&local, "persist.txt", b"LLLLLLLLLL"),
		"the local version was clobbered across the restart"
	);
	assert_eq!(
		remote_file_size_in(&resources, "persist.txt").await,
		Some(3),
		"the remote version was clobbered across the restart"
	);
	assert_eq!(
		total_files(&local),
		1,
		"the restart duplicated copies: {:?}",
		walk_tree(&local).keys().collect::<Vec<_>>()
	);

	// Resolving on the RESTARTED engine propagates exactly as it does before a restart.
	engine2
		.resolve_conflict(pair2, "persist.txt", ConflictResolution::KeepLocal)
		.await
		.expect("resolve keep-local after the restart");
	let r4 = engine2.sync_once(pair2).await.unwrap();
	assert!(r4.errors.is_empty(), "{r4:?}");
	assert!(r4.conflicts.is_empty(), "conflict not cleared: {r4:?}");
	assert_eq!(
		r4.uploaded, 1,
		"keep-local must push the local copy: {r4:?}"
	);
	assert_eq!(r4.downloaded, 0, "keep-local must not pull: {r4:?}");
	assert!(read_eq(&local, "persist.txt", b"LLLLLLLLLL"));
	assert_eq!(
		remote_file_size_in(&resources, "persist.txt").await,
		Some(10),
		"the remote must hold the local-wins content"
	);

	std::fs::remove_dir_all(&local).ok();
}

// ===========================================================================
// CONFLICT-26 — two clients edit the same file in the SAME round: the client
// whose push the server superseded surfaces a conflict instead of silently
// pulling the winner over its own copy.
// ===========================================================================
#[shared_test_runtime]
async fn conflict_26_same_round_edits_surface_on_the_superseded_client() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// Converge a shared "BASE" on both sides.
	write_file(&tc.local_a, "race.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c26-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "race.txt", b"BASE"),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"the baseline round must be conflict-free: {conflicts:?}"
	);

	// Give BOTH clients a pass that lists the shared version as the remote head. That observation
	// is what records BASE as the content both sides hold; without it neither side has an agreed
	// content to measure the divergence below against, and the loser would just pull.
	let (_d, files) = list_remote_root(&tc).await;
	let base_uuid: Uuid = find_file(&files, "race.txt")
		.expect("race.txt missing")
		.uuid();
	for cache in [&tc.cache_a, &tc.cache_b] {
		assert!(
			poll_for_item(cache.db_path(), base_uuid, CACHE_CONVERGE_TIMEOUT).await,
			"a cache never listed the shared version"
		);
	}
	let (rc_a, rc_b) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	assert!(rc_a.errors.is_empty(), "{rc_a:?}");
	assert!(rc_b.errors.is_empty(), "{rc_b:?}");

	// Let the clock leave the second the shared version was stamped in. The server records a
	// version's time to the second, and the winner's side of this race is recognised by the buried
	// version being LATER than the one its pass saw — inside one second the version chain cannot
	// order them at all (see `plan::interleaved_version`). The race below is still one round.
	tokio::time::sleep(Duration::from_millis(1500)).await;

	// Both clients edit and push in the SAME round: the server linearises the two uploads, so one
	// version ends up on top of the other and its author never saw its own push as the head.
	write_file(&tc.local_a, "race.txt", b"A-EDIT");
	write_file(&tc.local_b, "race.txt", b"B-EDIT-LONGER");
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
	)
	.await;
	assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
	assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
	assert_eq!(
		ra.uploaded + rb.uploaded,
		2,
		"both clients must have pushed their edit: {ra:?} / {rb:?}"
	);

	// One of the two uploads is now the remote head; the other is a version underneath it. Both
	// caches must be reading THAT state before the rounds below, never the intermediate one where
	// a client still lists the version its own upload replaced: sampled there, the loser confirms
	// its own push against the snapshot, and the round after it pulls the winner like an ordinary
	// remote edit — the test would pass over an engine that never surfaced anything.
	let (_d, files) = list_remote_root(&tc).await;
	let head = find_file(&files, "race.txt").expect("race.txt missing after the concurrent round");
	let head_uuid = head.uuid();
	// Distinct-length sentinels, so the surviving size names the winner (see `remote_file_size_in`).
	let b_won = head.size == b"B-EDIT-LONGER".len() as u64;
	assert!(
		b_won || head.size == b"A-EDIT".len() as u64,
		"the head is neither client's edit: {} bytes",
		head.size
	);
	for cache in [&tc.cache_a, &tc.cache_b] {
		assert!(
			poll_for_item(cache.db_path(), head_uuid, CACHE_CONVERGE_TIMEOUT).await,
			"a cache never listed the version that won the race"
		);
	}

	// The superseded client sees a foreign version of the same file on top of a push it never saw
	// land. That is the conflict this test exists for; a held conflict need not converge, so the
	// rounds below only collect what is reported — per client, because the two sides of one race
	// hold DIFFERENT conflicts: the loser's is the foreign version sitting on its push, the
	// winner's is the edit its own upload buried, which only the server's version chain shows.
	let (mut from_a, mut from_b) = (BTreeSet::new(), BTreeSet::new());
	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
		assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
		from_a.extend(ra.conflict_paths().map(str::to_string));
		from_b.extend(rb.conflict_paths().map(str::to_string));
		conflicts.extend(from_a.iter().chain(from_b.iter()).cloned());
		if conflicts.iter().any(|c| c.contains("race.txt")) {
			break;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	let (loser, winner) = if b_won {
		(&from_a, &from_b)
	} else {
		(&from_b, &from_a)
	};
	assert!(
		loser.iter().any(|c| c.contains("race.txt")),
		"a same-round edit by both clients must surface on the superseded client, not resolve \
		 itself silently: {conflicts:?}"
	);
	// And the WINNER's side of the same race. Its upload was versioned on top of an edit it never
	// saw: nothing it can read afterwards shows that — the remote head is its own copy and its
	// baseline agrees — so the pass asks the server's version chain as the upload lands and holds
	// what it buried. Without that, the overwritten edit is visible to nobody but its author.
	assert!(
		winner.iter().any(|c| c.contains("race.txt")),
		"the client whose upload buried the other's edit must surface it too: {winner:?}"
	);

	// Neither edit was destroyed: both are still readable somewhere across the two trees.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"A-EDIT")
			|| bytes_recoverable_anywhere(&tc.local_b, b"A-EDIT"),
		"A's edit was destroyed — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"B-EDIT-LONGER")
			|| bytes_recoverable_anywhere(&tc.local_b, b"B-EDIT-LONGER"),
		"B's edit was destroyed — data loss"
	);

	// Resolve both halves and converge. The winner keeps BOTH — the version it buried comes back
	// down beside its own copy under a name of its own — and the loser takes the head, whose bytes
	// it is about to be handed anyway. Both clients must end up with both files, one per name.
	let (win_engine, win_pair) = if b_won {
		(&tc.engine_b, tc.pair_b)
	} else {
		(&tc.engine_a, tc.pair_a)
	};
	let (lose_engine, lose_pair) = if b_won {
		(&tc.engine_a, tc.pair_a)
	} else {
		(&tc.engine_b, tc.pair_b)
	};
	let (won_bytes, lost_bytes): (&[u8], &[u8]) = if b_won {
		(b"B-EDIT-LONGER", b"A-EDIT")
	} else {
		(b"A-EDIT", b"B-EDIT-LONGER")
	};
	win_engine
		.resolve_conflict(win_pair, "race.txt", ConflictResolution::KeepBoth)
		.await
		.unwrap();
	lose_engine
		.resolve_conflict(lose_pair, "race.txt", ConflictResolution::KeepRemote)
		.await
		.unwrap();

	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c26-resolved",
		|| {
			[&tc.local_a, &tc.local_b].iter().all(|root| {
				read_eq(root, "race.txt", won_bytes) && read_eq(root, "race.old.txt", lost_bytes)
			})
		},
	)
	.await;

	// One file per name on the remote: keeping both added a file, it did not duplicate one.
	let (_d, files) = list_remote_root(&tc).await;
	for name in ["race.txt", "race.old.txt"] {
		assert_eq!(
			files.iter().filter(|f| f.name() == Some(name)).count(),
			1,
			"{name} exists more than once on the remote after resolving"
		);
	}

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-27 — two clients create the SAME name at once: the loser conflicts
// ===========================================================================
/// A brand-new file's baseline row carries no agreed content, and a same-name upload is versioned
/// by the server rather than refused — so when two clients create one name at the same moment, the
/// loser's row is a push nothing ever confirmed sitting under the winner's bytes. That is
/// indistinguishable from an ordinary "someone edited it after me" only if the missing marker is
/// read as consent; it is not, so the loser surfaces a conflict and both byte-streams survive.
///
/// Contrast CONFLICT-05, which stages the same divergence SEQUENTIALLY (the second client sees the
/// first's file before it ever uploads, so there is no baseline row on either side). Here both
/// clients really do upload, which is the race the agreed-content marker exists for.
#[shared_test_runtime]
async fn conflict_27_concurrent_same_name_create_surfaces_a_conflict() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// Distinct lengths, so the surviving size names which upload won the race.
	write_file(&tc.local_a, "race.txt", b"AAA-from-a");
	write_file(&tc.local_b, "race.txt", b"BBB-from-b-and-longer");

	// Both passes at once: the server linearizes the two uploads into two versions of ONE file.
	let (r0a, r0b) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
	)
	.await;
	assert!(
		r0a.errors.is_empty() && r0b.errors.is_empty(),
		"{r0a:?} {r0b:?}"
	);
	assert_eq!(
		r0a.uploaded + r0b.uploaded,
		2,
		"both clients must have pushed their own copy: {r0a:?} {r0b:?}"
	);

	// One upload is now the remote head, the other a version underneath it. Both caches must be
	// reading THAT state before the rounds below, never the intermediate one where the loser still
	// lists its own upload as the head: sampled there, the loser confirms its own push against the
	// snapshot and then pulls the winner like an ordinary later edit — and the test would pass over
	// an engine that never surfaced anything (see CONFLICT-26, which has the same hazard).
	let (_d, files) = list_remote_root(&tc).await;
	let head = find_file(&files, "race.txt").expect("race.txt missing after the concurrent round");
	let head_uuid = head.uuid();
	assert!(
		head.size == b"AAA-from-a".len() as u64
			|| head.size == b"BBB-from-b-and-longer".len() as u64,
		"the head is neither client's copy: {} bytes",
		head.size
	);
	for cache in [&tc.cache_a, &tc.cache_b] {
		assert!(
			poll_for_item(cache.db_path(), head_uuid, CACHE_CONVERGE_TIMEOUT).await,
			"a cache never listed the version that won the race"
		);
	}

	for _ in 0..8 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(
			ra.errors.is_empty() && rb.errors.is_empty(),
			"{ra:?} {rb:?}"
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	assert!(
		conflicts.iter().any(|c| c.contains("race.txt")),
		"the loser of a concurrent same-name create must surface a conflict: {conflicts:?}"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"AAA-from-a")
			|| bytes_recoverable_anywhere(&tc.local_b, b"AAA-from-a"),
		"A's content destroyed — data loss"
	);
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, b"BBB-from-b-and-longer")
			|| bytes_recoverable_anywhere(&tc.local_b, b"BBB-from-b-and-longer"),
		"B's content destroyed — data loss"
	);

	// The negative, on the same fixture and a path of its own: create, let ONE pass list our own
	// version as the remote head (which records it as the agreed content), then a foreign edit.
	// That edit is strictly later than our push, and must pull rather than surface — otherwise the
	// rule above would turn every ordinary sequential edit into a conflict.
	write_file(&tc.local_a, "solo.txt", b"S1");
	let s0 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	assert!(s0.errors.is_empty(), "{s0:?}");
	assert_eq!(s0.uploaded, 1, "{s0:?}");

	let (_d, files) = list_remote_root(&tc).await;
	let ours: Uuid = find_file(&files, "solo.txt")
		.expect("solo.txt never reached the remote")
		.uuid();
	assert!(
		poll_for_item(tc.cache_a.db_path(), ours, CACHE_CONVERGE_TIMEOUT).await,
		"A's cache never observed A's own push"
	);
	let s1 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	assert!(s1.errors.is_empty(), "{s1:?}");
	assert_eq!(
		s1.uploaded + s1.downloaded,
		0,
		"the confirming pass must be a no-op: {s1:?}"
	);

	let edited = upload_remote(&tc, "solo.txt", b"S2-remote").await;
	let edited_uuid: Uuid = edited.uuid();
	let db_a = tc.cache_a.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db_a, edited_uuid).map(|t| t.1) == Some(b"S2-remote".len() as i64)
		})
		.await,
		"A's cache never reflected the foreign edit"
	);
	let mut s2 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while s2.downloaded == 0
		&& s2
			.errors
			.iter()
			.any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(Duration::from_millis(500)).await;
		s2 = tc.engine_a.sync_once(tc.pair_a).await.unwrap();
	}
	assert!(s2.errors.is_empty(), "{s2:?}");
	assert_eq!(
		s2.downloaded, 1,
		"an edit made after a CONFIRMED push must pull: {s2:?}"
	);
	assert!(
		!s2.conflict_paths().any(|c| c.contains("solo.txt")),
		"a confirmed push must not conflict with a later foreign edit: {s2:?}"
	);
	assert!(read_eq(&tc.local_a, "solo.txt", b"S2-remote"));

	tc.cleanup();
}

// ===========================================================================
// CONFLICT-28 — the winner of a same-round race takes the edit it buried:
// KEEP-REMOTE on the buried-edit conflict must restore that version as the
// head, not quietly leave the winner's own bytes standing.
// ===========================================================================
/// CONFLICT-26 resolves the winner's side with `KeepBoth`. This is the other half of the same
/// shape: the winner asks for the copy it was shown — the edit its upload went on top of — which
/// only exists in the file's version history.
///
/// The trap this pins is the one the version chain sets. Its order is by original upload time
/// stamped to the SECOND, so the two uploads of a race tie and the listing may put either first:
/// reading the head off the chain instead of asking for it by lineage makes the restore look like a
/// no-op about half the time, and the resolution then quarantines the local copy and pulls the
/// winner's own bytes straight back — the exact opposite of what was asked for, with the other
/// client's edit left buried.
#[shared_test_runtime]
async fn conflict_28_keep_remote_restores_the_buried_edit() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = BTreeSet::new();

	// Converge a shared "BASE", then give both clients a pass that lists it as the remote head —
	// that observation is what records BASE as the content both sides hold (see CONFLICT-26).
	write_file(&tc.local_a, "race.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"c28-baseline",
		|| trees_equal(&tc.local_a, &tc.local_b) && read_eq(&tc.local_b, "race.txt", b"BASE"),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"the baseline round must be conflict-free: {conflicts:?}"
	);
	let (_d, files) = list_remote_root(&tc).await;
	let base_uuid: Uuid = find_file(&files, "race.txt")
		.expect("race.txt missing")
		.uuid();
	for cache in [&tc.cache_a, &tc.cache_b] {
		assert!(
			poll_for_item(cache.db_path(), base_uuid, CACHE_CONVERGE_TIMEOUT).await,
			"a cache never listed the shared version"
		);
	}
	let (rc_a, rc_b) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	assert!(rc_a.errors.is_empty(), "{rc_a:?}");
	assert!(rc_b.errors.is_empty(), "{rc_b:?}");

	// Let the clock leave the second the shared version was stamped in. The server records a
	// version's time to the second, and the winner's side of this race is recognised by the buried
	// version being LATER than the one its pass saw — inside one second the version chain cannot
	// order them at all (see `plan::interleaved_version`). The race below is still one round.
	tokio::time::sleep(Duration::from_millis(1500)).await;

	// Both edit and push in the SAME round: one upload lands on top of the other.
	write_file(&tc.local_a, "race.txt", b"A-EDIT");
	write_file(&tc.local_b, "race.txt", b"B-EDIT-LONGER");
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::Concurrent,
	)
	.await;
	assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
	assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
	assert_eq!(
		ra.uploaded + rb.uploaded,
		2,
		"both clients must have pushed their edit: {ra:?} / {rb:?}"
	);

	// Distinct-length sentinels, so the surviving size names the winner.
	let (_d, files) = list_remote_root(&tc).await;
	let head = find_file(&files, "race.txt").expect("race.txt missing after the concurrent round");
	let head_uuid = head.uuid();
	let b_won = head.size == b"B-EDIT-LONGER".len() as u64;
	assert!(
		b_won || head.size == b"A-EDIT".len() as u64,
		"the head is neither client's edit: {} bytes",
		head.size
	);
	for cache in [&tc.cache_a, &tc.cache_b] {
		assert!(
			poll_for_item(cache.db_path(), head_uuid, CACHE_CONVERGE_TIMEOUT).await,
			"a cache never listed the version that won the race"
		);
	}

	let (mut from_a, mut from_b) = (BTreeSet::new(), BTreeSet::new());
	for _ in 0..6 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::AFirst,
		)
		.await;
		assert!(ra.errors.is_empty(), "A errors: {:?}", ra.errors);
		assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
		from_a.extend(ra.conflict_paths().map(str::to_string));
		from_b.extend(rb.conflict_paths().map(str::to_string));
		conflicts.extend(from_a.iter().chain(from_b.iter()).cloned());
		if from_a
			.iter()
			.chain(from_b.iter())
			.any(|c| c.contains("race.txt"))
		{
			break;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	let (winner, loser) = if b_won {
		(&from_b, &from_a)
	} else {
		(&from_a, &from_b)
	};
	assert!(
		winner.iter().any(|c| c.contains("race.txt")),
		"the client whose upload buried the other's edit must surface it: {winner:?}"
	);
	assert!(
		loser.iter().any(|c| c.contains("race.txt")),
		"the superseded client must surface its own side of the race: {loser:?}"
	);

	// The winner asks for the copy it was shown — the edit it buried.
	let (win_engine, win_pair) = if b_won {
		(&tc.engine_b, tc.pair_b)
	} else {
		(&tc.engine_a, tc.pair_a)
	};
	let (won_bytes, lost_bytes): (&[u8], &[u8]) = if b_won {
		(b"B-EDIT-LONGER", b"A-EDIT")
	} else {
		(b"A-EDIT", b"B-EDIT-LONGER")
	};
	win_engine
		.resolve_conflict(win_pair, "race.txt", ConflictResolution::KeepRemote)
		.await
		.unwrap();

	// Ground truth, read from the server rather than from either client's cache: the buried
	// version is the head again, under one name.
	let (_d, files) = list_remote_root(&tc).await;
	let remote: Vec<_> = files
		.iter()
		.filter(|f| f.name() == Some("race.txt"))
		.collect();
	assert_eq!(
		remote.len(),
		1,
		"race.txt exists more than once on the remote"
	);
	assert_eq!(
		remote[0].size,
		lost_bytes.len() as u64,
		"keep-remote left the resolver's own bytes as the head instead of restoring the version \
		 it buried"
	);
	// And the bytes it displaced are still recoverable, as every resolution must leave them.
	assert!(
		bytes_recoverable_anywhere(&tc.local_a, won_bytes)
			|| bytes_recoverable_anywhere(&tc.local_b, won_bytes),
		"the winner's own edit was destroyed — data loss"
	);
	// What the two trees do NEXT is deliberately not asserted here. A restore puts a version the
	// cache had already seen archived back at the head, and nothing announces that, so both
	// clients read the path as absent until their next resync — a gap in the cache's event
	// handling, not in the resolution this test is about (CONFLICT-26 covers converging after a
	// resolution that leaves the head alone).

	tc.cleanup();
}
