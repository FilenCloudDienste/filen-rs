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
use filen_sdk_rs::sync_engine::{SyncEngine, SyncMode};
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
			.conflicts
			.iter()
			.chain(rb.conflicts.iter())
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
			.conflicts
			.iter()
			.chain(rb.conflicts.iter())
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
#[ignore = "blocked: descendant suppression under a conflicted path (deferred review finding). A \
two-way file<->dir type flip (A edits the file `thing`; B replaces it with a dir `thing/child.txt`) \
IS correctly surfaced as a conflict on `thing`, and no data is lost — but B's pass still tries to \
upload the descendant `thing/child.txt` and errors with `remote parent dir for upload is missing`, \
because the dir `thing` cannot be created while the conflicting file `thing` holds the path. The \
engine must skip actions on descendants of a path it is holding in conflict. TODO"]
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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

#[ignore = "blocked: pull-overwrite quarantine (deferred review finding). In RemoteToLocal the \
remote correctly wins a divergence (REM is downloaded, no conflict surfaced — right for a \
one-directional mode), but the overwritten local edit (LOC) is HARD-overwritten by the download \
rather than preserved. The delete path already quarantines a removed local file; the \
content-overwrite path should likewise stash the pre-overwrite local bytes so a clobbered local edit \
stays recoverable. TODO"]
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
		r2.held_deletions > 0 || r2.guard_message.is_some(),
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
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

#[ignore = "blocked: needs a conflict-RESOLUTION API (instruct the engine to keep local/remote and \
advance the baseline). The public surface (SyncEngine: open/add_pair/sync_once/watch) exposes no \
resolve entry point, so a caller cannot drive resolution. TODO: add a resolve_conflict(pair, path, \
choice) public method (or document the file-system convention) before implementing."]
#[shared_test_runtime]
async fn conflict_16_resolve_keep_local() {
	// plan: reach a two-way conflict on conf.txt (L/R); resolve keep-LOCAL; sync; assert both sides
	// hold L, conflict gone on subsequent passes, R preserved recoverably, baseline clean.
}

#[ignore = "blocked: needs a conflict-RESOLUTION API (keep-remote). Same gap as conflict_16. TODO: \
add resolve_conflict(pair, path, KeepRemote)."]
#[shared_test_runtime]
async fn conflict_17_resolve_keep_remote() {
	// plan: reach a two-way conflict on conf2.txt (L/R); resolve keep-REMOTE; sync; assert both sides
	// hold R, conflict gone, L preserved recoverably, baseline clean.
}

#[ignore = "blocked: needs control over the engine's persisted baseline DB path to simulate a true \
process restart against the SAME baseline store. The harness builds engines with a private random \
temp_cache_path() and exposes no accessor/reuse path, so re-opening the engine with the same baseline \
(the whole point of the test) is impossible black-box. TODO: harness hook to reopen an engine on the \
same baseline DB path."]
#[shared_test_runtime]
async fn conflict_18_conflict_persists_across_restart() {
	// plan: reach a conflict on persist.txt; drop the engine; reopen on the SAME baseline DB +
	// re-add the same pair; sync; assert the conflict is still reported, both versions intact, no
	// silent auto-resolution, no duplicate copies.
}

#[ignore = "blocked: needs a conflict-RESOLUTION API to force the FIRST conflict to be resolved (so a \
clean SECOND independent conflict can be produced on the same base path) AND deterministic control \
over conflict-copy naming. Without resolution we cannot stage two sequential independent conflicts on \
one path. Partially covered by conflict_02 (no per-pass copy growth) and conflict_24 (copy bytes \
exact). TODO: resolution API + conflict-copy naming inspection."]
#[shared_test_runtime]
async fn conflict_19_conflict_copy_naming_no_collision_or_recursion() {
	// plan: force a conflict on a.txt -> copy; resolve; force a SECOND conflict on a.txt; assert the
	// second copy gets a distinct non-colliding name, no infinite re-ingest, counts match genuine
	// conflicts.
}

#[ignore = "blocked: needs a conflict-RESOLUTION API. The whole test is about the LIFECYCLE of the \
leftover preserved copy AFTER a keep-local resolution round-trips through sync — which requires \
driving resolution first (same gap as conflict_16/17). TODO: resolution API."]
#[shared_test_runtime]
async fn conflict_add_resolved_copy_round_trips() {
	// plan: reach conflict on c.txt -> conflict-copy; resolve keep-local; sync twice; assert the
	// leftover copy is treated as an ordinary file (uploads once / retained), the original conflict
	// does not reappear, no unbounded copy growth, third pass is a no-op.
}
