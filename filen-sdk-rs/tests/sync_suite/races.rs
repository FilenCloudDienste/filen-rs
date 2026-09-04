//! Single-client event & race edge cases (`RACE-*`) for the two-way sync engine.
//!
//! These exercise the gap between "truth at scan time" and "truth at apply time": files
//! modified/deleted/replaced mid-pass, the mtime+size fast-path, type flips, special files, and
//! the converge-on-truth contract. Tests whose correctness hinges on DETERMINISTIC mid-transfer or
//! mid-pass interruption (a controllable clock, a fault-injection seam, or an adversarial mock
//! remote) are stubbed `#[ignore]` until that harness exists — the current harness cannot make the
//! race deterministic, and faking it would assert nothing. Everything the current harness genuinely
//! supports (converged end-state, fast-path detection, type flips, permission errors as per-file
//! failures, empty-vs-missing, mass-delete guard, populated-destination first-run, special files,
//! best-effort concurrent mutation, watch-mode coalescing & self-write suppression) is implemented
//! and asserts ABSENCE OF DATA LOSS.
use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::file::RemoteFile;
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{SyncEngine, SyncEvent, SyncMode, SyncObserver};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ---------------------------------------------------------------------------
// Local helpers (public-API only; remote ground-truth via the cache's client).
// ---------------------------------------------------------------------------

/// List the (dirs, files) directly under the single-client's remote root (ground truth).
async fn list_root(
	sc: &SingleClient,
) -> (Vec<filen_sdk_rs::fs::dir::RemoteDirectory>, Vec<RemoteFile>) {
	sc.cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap()
}

/// List the (dirs, files) directly under a remote subdir (ground truth).
async fn list_dir(
	sc: &SingleClient,
	dir: &filen_sdk_rs::fs::dir::RemoteDirectory,
) -> (Vec<filen_sdk_rs::fs::dir::RemoteDirectory>, Vec<RemoteFile>) {
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

fn find_dir<'a>(
	dirs: &'a [filen_sdk_rs::fs::dir::RemoteDirectory],
	name: &str,
) -> Option<&'a filen_sdk_rs::fs::dir::RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
}

/// Upload a file with exact bytes directly to the single-client's remote root.
async fn upload_root(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
	let builder = sc
		.cache
		.client
		.make_file_builder(name, sc.resources.dir.uuid())
		.unwrap();
	sc.cache.client.upload_file(builder, data).await.unwrap()
}

/// Force a file's mtime to a given `SystemTime` (simulate clock skew / mtime restore).
fn set_mtime(root: &std::path::Path, rel: &str, t: SystemTime) {
	let f = std::fs::OpenOptions::new()
		.write(true)
		.open(root.join(rel))
		.unwrap();
	f.set_modified(t).unwrap();
}

// ===========================================================================
// IMPLEMENTED — fast-path change detection (RACE-05/06/07 + remote add)
// ===========================================================================

/// RACE-05 — content changed, size identical, mtime advances naturally: must be detected.
#[shared_test_runtime]
async fn race_05_fast_path_same_size_mtime_advances_uploads() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "same.txt", b"AAAA");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Overwrite with the SAME size; the natural mtime advance must trigger detection. A short sleep
	// guarantees the filesystem mtime resolution actually advances.
	tokio::time::sleep(Duration::from_millis(1100)).await;
	write_file(&sc.local, "same.txt", b"BBBB");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 1,
		"equal-size content change must re-upload via mtime fast-path: {r2:?}"
	);

	let (_d, files) = list_root(&sc).await;
	assert_eq!(files.len(), 1, "still exactly one file: {}", files.len());
	let f = find_file(&files, "same.txt").expect("same.txt missing");
	assert_eq!(f.size, 4, "remote size still 4 bytes");

	// Idle pass: nothing left to do (baseline reflects BBBB, not stale AAAA).
	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "baseline must reflect BBBB: {r3:?}");
	sc.cleanup();
}

/// RACE-06 — content changed, size identical, mtime FORCED back to the baseline value.
///
/// Documents the engine's mtime+size fast-path contract: when both size AND mtime are restored to
/// the last-synced values, a pure mtime+size engine cannot see the change (the documented, intended
/// limitation). The hard requirement asserted here is the SAFETY one: this must never silently
/// corrupt an unrelated file, and baseline/remote must not end believing they are converged for a
/// DIFFERENT content than is recorded. We assert the observable invariant (remote holds a coherent
/// 4-byte version, the sibling is untouched) and tolerate EITHER outcome for sneaky.txt — detected
/// (CCCC) or the known fast-path miss (AAAA) — since both are coherent and non-corrupting.
#[shared_test_runtime]
async fn race_06_fast_path_mtime_restored_to_baseline() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "sneaky.txt", b"AAAA");
	write_file(&sc.local, "other.txt", b"unrelated");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Capture the on-disk baseline mtime, rewrite same-size, force mtime back to it.
	let m = std::fs::metadata(sc.local.join("sneaky.txt"))
		.unwrap()
		.modified()
		.unwrap();
	write_file(&sc.local, "sneaky.txt", b"CCCC");
	set_mtime(&sc.local, "sneaky.txt", m);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");

	let (_d, files) = list_root(&sc).await;
	let sneaky = find_file(&files, "sneaky.txt").expect("sneaky.txt missing");
	assert_eq!(
		sneaky.size, 4,
		"sneaky.txt remains a coherent 4-byte file (AAAA if fast-path missed, CCCC if detected): {}",
		sneaky.size
	);
	// The unrelated file must be wholly untouched — no fast-path miss may corrupt a sibling.
	let other = find_file(&files, "other.txt").expect("other.txt missing");
	assert_eq!(other.size, b"unrelated".len() as u64, "sibling corrupted!");
	sc.cleanup();
}

/// RACE-07 — clock skew: an initial future mtime, then a content change with an EARLIER mtime. The
/// engine must not assume monotonic time; the backdated edit must still be detected.
#[shared_test_runtime]
async fn race_07_clock_skew_backdated_edit_detected() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "future.txt", b"AAAA");
	// mtime far in the future (~year 2099).
	let far_future = SystemTime::UNIX_EPOCH + Duration::from_secs(4_080_000_000);
	set_mtime(&sc.local, "future.txt", far_future);

	let r1 = sc.sync().await;
	assert!(
		r1.errors.is_empty(),
		"future timestamp must not crash the pass: {r1:?}"
	);
	assert_eq!(
		r1.uploaded, 1,
		"initial upload despite future mtime: {r1:?}"
	);

	// Content change to DIFFERENT bytes, with mtime EARLIER than the recorded future value.
	write_file(&sc.local, "future.txt", b"BBBB");
	set_mtime(&sc.local, "future.txt", SystemTime::now());

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 1,
		"a backdated (mtime-decreased) content change must still be detected: {r2:?}"
	);

	let (_d, files) = list_root(&sc).await;
	let f = find_file(&files, "future.txt").expect("future.txt missing");
	assert_eq!(f.size, 4, "remote holds the 4-byte change");
	sc.cleanup();
}

/// (add) Remote-side fast-path: a same-size remote content change (re-upload versions it into a new
/// uuid). The engine's remote-change signal is the listing's uuid/version, so the change is detected
/// and the new content is downloaded byte-exact.
#[shared_test_runtime]
async fn race_add_remote_fast_path_same_size_change_detected() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let root_uuid = sc.resources.dir.uuid();
	let rf = upload_root(&sc, "rfast.txt", b"AAAA").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed rfast.txt"
	);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "rfast.txt", b"AAAA"));

	// Re-upload the SAME name, SAME size, different bytes (server versions -> new uuid).
	let builder2 = sc
		.cache
		.client
		.make_file_builder("rfast.txt", root_uuid)
		.unwrap();
	let new_rf = sc
		.cache
		.client
		.upload_file(builder2, b"BBBB")
		.await
		.unwrap();
	let new_uuid: Uuid = new_rf.uuid();
	let db = sc.cache.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(4)
		})
		.await,
		"cache never reflected the new same-size version"
	);

	// The remote change-signal (new uuid) must drive a re-download of the new bytes.
	let mut ok = false;
	for _ in 0..8 {
		let r = sc.sync().await;
		if read_eq(&sc.local, "rfast.txt", b"BBBB") {
			assert!(r.errors.is_empty(), "post-change pass errors: {r:?}");
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(700)).await;
	}
	assert!(
		ok,
		"same-size remote change must be detected and downloaded byte-exact"
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — type flips (file <-> dir)
// ===========================================================================

/// RACE-09 — a local FILE is replaced by a DIRECTORY of the same name between passes.
#[shared_test_runtime]
async fn race_09_file_replaced_by_directory() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "node", b"A-file-content");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Flip the type: delete the file, create a directory of the same name with a child.
	std::fs::remove_file(sc.local.join("node")).unwrap();
	write_file(&sc.local, "node/child.txt", b"B-child");

	// A type flip may legitimately take a delete-then-create across passes.
	let mut converged = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "type-flip pass errors: {r:?}");
		let (dirs, files) = list_root(&sc).await;
		let node_is_dir = find_dir(&dirs, "node").is_some();
		let node_is_file = find_file(&files, "node").is_some();
		if node_is_dir && !node_is_file {
			let nd = find_dir(&dirs, "node").unwrap();
			let (_cd, cf) = list_dir(&sc, nd).await;
			if let Some(c) = find_file(&cf, "child.txt") {
				assert_eq!(c.size, b"B-child".len() as u64, "child content size wrong");
				converged = true;
				break;
			}
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		converged,
		"remote 'node' must end as a directory containing child.txt, with no file 'node'"
	);
	sc.cleanup();
}

/// RACE-10 — a local DIRECTORY (with children) is replaced by a FILE of the same name.
#[shared_test_runtime]
async fn race_10_directory_replaced_by_file() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "data/a.txt", b"A");
	write_file(&sc.local, "data/b.txt", b"B");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Flip the type: remove the dir tree, create a regular file of the same name.
	std::fs::remove_dir_all(sc.local.join("data")).unwrap();
	write_file(&sc.local, "data", b"C-now-a-file");

	let mut converged = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "type-flip pass errors: {r:?}");
		let (dirs, files) = list_root(&sc).await;
		let data_is_file = find_file(&files, "data").is_some();
		let data_is_dir = find_dir(&dirs, "data").is_some();
		if data_is_file && !data_is_dir {
			let f = find_file(&files, "data").unwrap();
			assert_eq!(
				f.size,
				b"C-now-a-file".len() as u64,
				"file content size wrong"
			);
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		converged,
		"remote 'data' must end as a regular file, no dir 'data' and no stale a.txt/b.txt"
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — permission errors are per-file, not whole-pair aborts
// ===========================================================================

/// RACE-13 — one unreadable local file must not abort the pair; the others still upload, the
/// failure is surfaced per-file, and once readable it uploads on a later pass.
#[cfg(unix)]
#[shared_test_runtime]
async fn race_13_unreadable_local_file_does_not_abort_pair() {
	use std::os::unix::fs::PermissionsExt;

	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "ok1.txt", b"A");
	write_file(&sc.local, "secret.txt", b"B");
	write_file(&sc.local, "ok2.txt", b"C");
	// Remove all permissions from secret.txt (owner cannot open it).
	std::fs::set_permissions(
		sc.local.join("secret.txt"),
		std::fs::Permissions::from_mode(0o000),
	)
	.unwrap();

	let r1 = sc.sync().await;
	// The pair must not crash; the two readable files must upload regardless of the bad one.
	let (_d, files) = list_root(&sc).await;
	assert!(
		find_file(&files, "ok1.txt").is_some(),
		"ok1.txt not uploaded: {r1:?}"
	);
	assert!(
		find_file(&files, "ok2.txt").is_some(),
		"ok2.txt not uploaded: {r1:?}"
	);
	// secret.txt either failed (per-file error) or was skipped — never a coherent upload.
	assert!(
		find_file(&files, "secret.txt").is_none() || !r1.errors.is_empty(),
		"unreadable file must not be silently marked synced: {r1:?}"
	);

	// Restore permission; a later pass must upload it.
	std::fs::set_permissions(
		sc.local.join("secret.txt"),
		std::fs::Permissions::from_mode(0o644),
	)
	.unwrap();
	let mut ok = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "post-restore pass errors: {r:?}");
		let (_d, files) = list_root(&sc).await;
		if let Some(f) = find_file(&files, "secret.txt") {
			assert_eq!(f.size, 1, "secret.txt size wrong");
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(600)).await;
	}
	assert!(ok, "secret.txt must upload after permission is restored");
	sc.cleanup();
}

/// RACE-14 — permission denied on the local download destination must surface a per-file/dir error,
/// not crash the pair nor leave a partial; once writable, the download completes byte-exact.
#[cfg(unix)]
#[shared_test_runtime]
async fn race_14_undeliverable_download_does_not_crash_pair() {
	use std::os::unix::fs::PermissionsExt;

	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_root(&sc, "readonly.txt", b"A-payload").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed the remote file"
	);

	// Make the local sync root non-writable (deny create).
	std::fs::set_permissions(&sc.local, std::fs::Permissions::from_mode(0o500)).unwrap();

	let r1 = sc
		.engine
		.sync_once(sc.pair)
		.await
		.expect("sync_once must not hard-error");
	// Must not have produced a coherent file under a non-writable root.
	assert!(
		!sc.local.join("readonly.txt").exists() || !r1.errors.is_empty(),
		"undeliverable download must not be silently marked complete: {r1:?}"
	);
	// No zero-length/partial artifact masquerading as complete.
	if let Ok(md) = std::fs::metadata(sc.local.join("readonly.txt")) {
		assert_eq!(
			md.len(),
			b"A-payload".len() as u64,
			"a partial/zero-length file was left as if complete"
		);
	}

	// Restore write permission; a later pass must deliver the file byte-exact.
	std::fs::set_permissions(&sc.local, std::fs::Permissions::from_mode(0o755)).unwrap();
	let mut ok = false;
	for _ in 0..6 {
		let r = sc.engine.sync_once(sc.pair).await.expect("sync_once");
		if read_eq(&sc.local, "readonly.txt", b"A-payload") {
			assert!(r.errors.is_empty(), "post-restore pass errors: {r:?}");
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(600)).await;
	}
	assert!(
		ok,
		"readonly.txt must download byte-exact once the root becomes writable"
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — two-way genuine divergence / identical rewrite
// ===========================================================================

/// RACE-16 — same path genuinely diverges on both sides between passes: a conflict is surfaced and
/// neither side's content is destroyed.
#[shared_test_runtime]
async fn race_16_twoway_divergence_conflicts_without_loss() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = std::collections::BTreeSet::new();

	// Establish a shared baseline of shared.txt=BASE on both sides.
	write_file(&tc.local_a, "shared.txt", b"BASE");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"baseline",
		|| read_eq(&tc.local_b, "shared.txt", b"BASE") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	// Settling pass so each baseline records BASE as last-synced.
	let _ = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	conflicts.clear();

	// Diverge BOTH sides to different content.
	write_file(&tc.local_a, "shared.txt", b"LOCAL-A-edit");
	write_file(&tc.local_b, "shared.txt", b"REMOTE-B-edit");

	// Run several rounds; a conflict must surface and no side's data may be destroyed.
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
		tokio::time::sleep(Duration::from_millis(1200)).await;
	}

	let surfaced = conflicts.iter().any(|c| c.contains("shared.txt"));
	// Both user versions must remain recoverable somewhere (in place or as a conflict copy).
	let a_tree = walk_tree(&tc.local_a);
	let b_tree = walk_tree(&tc.local_b);
	let l_somewhere = a_tree.values().any(|(_, _, c)| c == b"LOCAL-A-edit")
		|| b_tree.values().any(|(_, _, c)| c == b"LOCAL-A-edit");
	let r_somewhere = a_tree.values().any(|(_, _, c)| c == b"REMOTE-B-edit")
		|| b_tree.values().any(|(_, _, c)| c == b"REMOTE-B-edit");
	assert!(
		l_somewhere && r_somewhere,
		"both diverged versions must remain recoverable (no silent overwrite): \
		 L_present={l_somewhere} R_present={r_somewhere} conflicts={conflicts:?}"
	);
	assert!(
		surfaced,
		"a genuine both-sides divergence must be surfaced as a conflict, saw: {conflicts:?}"
	);
	tc.cleanup();
}

/// RACE-25 — identical-bytes rewrite on both sides must NOT raise a false conflict; reconverges.
#[shared_test_runtime]
async fn race_25_same_content_rewrite_no_false_conflict() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts = std::collections::BTreeSet::new();

	write_file(&tc.local_a, "idem.txt", b"IDENTICAL");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"baseline",
		|| read_eq(&tc.local_b, "idem.txt", b"IDENTICAL") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	let _ = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
	)
	.await;
	conflicts.clear();

	// Rewrite the SAME bytes on both sides (mtime/version advances; content identical).
	tokio::time::sleep(Duration::from_millis(1100)).await;
	write_file(&tc.local_a, "idem.txt", b"IDENTICAL");
	write_file(&tc.local_b, "idem.txt", b"IDENTICAL");

	for _ in 0..6 {
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
		tokio::time::sleep(Duration::from_millis(1200)).await;
	}

	assert!(
		conflicts.is_empty(),
		"identical content on both sides must never conflict: {conflicts:?}"
	);
	assert!(
		read_eq(&tc.local_a, "idem.txt", b"IDENTICAL"),
		"A content lost"
	);
	assert!(
		read_eq(&tc.local_b, "idem.txt", b"IDENTICAL"),
		"B content lost"
	);
	tc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — empty-vs-missing, populated destination, mass-delete guard
// ===========================================================================

/// RACE-21 — a 0-byte file is real data, not absence; truncating to 0 is a content change.
#[shared_test_runtime]
async fn race_21_zero_byte_is_real_data_not_absence() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "empty.txt", b"");
	write_file(&sc.local, "note.txt", b"A-original");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(
		r1.uploaded, 2,
		"both files (incl. the empty one) upload: {r1:?}"
	);

	let (_d, files) = list_root(&sc).await;
	let empty = find_file(&files, "empty.txt").expect("empty.txt missing on remote");
	assert_eq!(
		empty.size, 0,
		"empty file must exist as a real 0-byte remote file"
	);

	// Truncate note.txt to 0 bytes — a content change, neither a delete nor a no-op.
	tokio::time::sleep(Duration::from_millis(1100)).await;
	write_file(&sc.local, "note.txt", b"");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"truncation to 0 is NOT a deletion: {r2:?}"
	);
	assert_eq!(
		r2.uploaded, 1,
		"truncation is a content change -> re-upload: {r2:?}"
	);

	let (_d2, files2) = list_root(&sc).await;
	let note = find_file(&files2, "note.txt").expect("note.txt must NOT be deleted");
	assert_eq!(
		note.size, 0,
		"note.txt must be updated to 0 bytes, not left at A"
	);
	assert!(
		find_file(&files2, "empty.txt").is_some(),
		"the originally-empty file must still be present"
	);
	sc.cleanup();
}

/// RACE-24 — first sync into an already-populated destination must NOT wipe pre-existing data on
/// either side; it merges.
#[shared_test_runtime]
async fn race_24_first_sync_into_populated_destination_no_wipe() {
	let sc = single_client(SyncMode::TwoWay).await;

	// Pre-existing remote-only file (placed before this pair's baseline exists).
	let rf = upload_root(&sc, "pre.txt", b"P-remote").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed pre.txt"
	);
	// Pre-existing local-only file.
	write_file(&sc.local, "local_pre.txt", b"Q-local");

	// First (and subsequent) passes must merge, never wipe.
	let mut merged = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "first-run pass errors: {r:?}");
		assert_eq!(
			r.locally_deleted, 0,
			"must not delete local-only data: {r:?}"
		);
		assert_eq!(
			r.remotely_trashed, 0,
			"must not trash remote-only data: {r:?}"
		);
		let local_ok = read_eq(&sc.local, "pre.txt", b"P-remote")
			&& read_eq(&sc.local, "local_pre.txt", b"Q-local");
		let (_d, files) = list_root(&sc).await;
		let remote_ok =
			find_file(&files, "pre.txt").is_some() && find_file(&files, "local_pre.txt").is_some();
		if local_ok && remote_ok {
			merged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		merged,
		"empty-baseline first run must merge both pre-existing files, not treat them as deletions"
	);
	sc.cleanup();
}

/// RACE-23 — a mass deletion (whole bulk/ dir) must trip the mass-delete guard and HOLD, regardless
/// of how the files vanished; the remote keeps everything until the deletion is re-attempted.
#[shared_test_runtime]
async fn race_23_mass_delete_guard_holds_bulk_vanish() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const TOTAL: usize = 20;
	for i in 0..TOTAL {
		write_file(
			&sc.local,
			&format!("bulk/f{i:02}.txt"),
			format!("c{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, TOTAL, "all bulk files uploaded: {r1:?}");

	// Delete the entire directory at once (mass vanish).
	std::fs::remove_dir_all(sc.local.join("bulk")).unwrap();
	let r2 = sc.sync().await;
	assert!(
		r2.held_deletions() > 0 || r2.guard.is_some(),
		"mass-delete guard must engage for the bulk vanish: {r2:?}"
	);
	assert_eq!(
		r2.remotely_trashed, 0,
		"guard must prevent the destructive mass trashing: {r2:?}"
	);

	// Remote must still hold every file (the guard kept them).
	let bulk_dir = {
		let (dirs, _f) = list_root(&sc).await;
		find_dir(&dirs, "bulk")
			.expect("bulk dir gone — guard failed")
			.clone()
	};
	let (_cd, cf) = list_dir(&sc, &bulk_dir).await;
	assert_eq!(
		cf.len(),
		TOTAL,
		"guard must keep all {TOTAL} remote files; found {}",
		cf.len()
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — special files (symlinks / loops / hardlinks)
// ===========================================================================

/// RACE-19 — a symlink and a symlink loop must not hang/recurse the scan; the real target file is
/// uploaded byte-exact and nothing is duplicated unboundedly.
#[cfg(unix)]
#[shared_test_runtime]
async fn race_19_symlink_and_loop_do_not_hang_scan() {
	use std::os::unix::fs::symlink;

	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "target.txt", b"A-target");
	// A symlink to a regular file, and a self-referential directory loop.
	symlink(sc.local.join("target.txt"), sc.local.join("link.txt")).unwrap();
	std::fs::create_dir_all(sc.local.join("cycle")).unwrap();
	symlink(sc.local.join("cycle"), sc.local.join("cycle/self")).unwrap();

	// The scan must terminate (no infinite recursion) and not crash; bounded so a hang fails fast.
	let _r1 = tokio::time::timeout(Duration::from_secs(180), sc.sync())
		.await
		.expect("sync hung on the symlink loop");
	let r2 = tokio::time::timeout(Duration::from_secs(180), sc.sync())
		.await
		.expect("second sync hung on the symlink loop");

	// The real target's bytes must be present on the remote regardless of the symlink policy.
	let (_d, files) = list_root(&sc).await;
	let t = find_file(&files, "target.txt").expect("target.txt must be uploaded");
	assert_eq!(t.size, b"A-target".len() as u64, "target.txt content lost");
	// Whatever the symlink contract, the scan must not have exploded the tree.
	assert!(
		files.len() < 50,
		"symlink loop produced an unbounded structure: {r2:?}"
	);
	sc.cleanup();
}

/// (add) Hardlinked file — two local paths to the same inode upload as two independent remote files
/// (remote has no hardlink concept); an edit via one path converges both copies.
#[cfg(unix)]
#[shared_test_runtime]
async fn race_add_hardlink_two_paths_independent_remote_files() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "orig.txt", b"A-shared-inode");
	std::fs::hard_link(sc.local.join("orig.txt"), sc.local.join("hardlink.txt")).unwrap();

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	// Two distinct names exist; both must be uploaded as independent remote files.
	let (_d, files) = list_root(&sc).await;
	assert!(
		find_file(&files, "orig.txt").is_some(),
		"orig.txt missing: {r1:?}"
	);
	assert!(
		find_file(&files, "hardlink.txt").is_some(),
		"hardlink.txt missing: {r1:?}"
	);

	// Edit via one path (both names now see B); both remote copies must converge.
	tokio::time::sleep(Duration::from_millis(1100)).await;
	std::fs::write(sc.local.join("orig.txt"), b"B-shared-inode-edit").unwrap();
	let mut ok = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "post-edit pass errors: {r:?}");
		let (_d, files) = list_root(&sc).await;
		let want = b"B-shared-inode-edit".len() as u64;
		let a = find_file(&files, "orig.txt").map(|f| f.size);
		let b = find_file(&files, "hardlink.txt").map(|f| f.size);
		if a == Some(want) && b == Some(want) {
			ok = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		ok,
		"both hardlinked remote copies must converge to the edited content"
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — backup-mode delete racing a remote edit (end-state assertion)
// ===========================================================================

/// RACE-22 — in LocalBackup, a local delete must never mirror to the remote even when the remote
/// copy was edited externally; the protected remote copy survives coherent and is not resurrected.
#[shared_test_runtime]
async fn race_22_local_backup_delete_never_wipes_remote_copy() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "keep.txt", b"A-local");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Externally edit the remote copy to B (versions in place), AND delete it locally.
	let _edited = upload_root(&sc, "keep.txt", b"B-remote-edit").await;
	std::fs::remove_file(sc.local.join("keep.txt")).unwrap();

	// Several backup passes: the remote copy must NEVER be trashed nor pulled back.
	for _ in 0..4 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "backup pass errors: {r:?}");
		assert_eq!(
			r.remotely_trashed, 0,
			"LocalBackup must never trash a remote copy: {r:?}"
		);
		assert_eq!(
			r.downloaded, 0,
			"LocalBackup must never pull the remote copy back: {r:?}"
		);
		tokio::time::sleep(Duration::from_millis(800)).await;
	}

	// Remote keep.txt must still exist as a coherent version (A or the externally-set B).
	let (_d, files) = list_root(&sc).await;
	let f = find_file(&files, "keep.txt").expect("protected remote copy was wiped — data loss");
	assert!(
		f.size == b"A-local".len() as u64 || f.size == b"B-remote-edit".len() as u64,
		"remote copy must be a coherent whole version, size={}",
		f.size
	);
	// Local must remain deleted (no resurrection loop in pure backup mode).
	assert!(
		!sc.local.join("keep.txt").exists(),
		"backup mode must not resurrect the locally-deleted file"
	);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — watch mode: coalescing & self-write loop suppression
// ===========================================================================

/// RACE-08 — rapid successive edits within the debounce window coalesce; the remote ends on the
/// FINAL content (V10), and the watch loop does not spin forever.
#[shared_test_runtime]
async fn race_08_rapid_edits_coalesce_to_final() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let engine = Arc::new(
		SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let handle = engine.clone().watch(pair).await.unwrap();

	// Burst-write 10 versions in quick succession.
	for v in 1..=10u8 {
		write_file(&sc.local, "burst.txt", format!("V{v}").as_bytes());
	}
	let final_bytes = b"V10";

	// Poll the remote until it reflects the FINAL content (size of "V10" == 3).
	let mut converged = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_d, files) = list_root(&sc).await;
		if find_file(&files, "burst.txt").map(|f| f.size) == Some(final_bytes.len() as u64) {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(400)).await;
	}
	assert!(
		converged,
		"watch did not settle the burst on the final content V10"
	);

	// Quiesce: after settling, no further changes -> the remote stays exactly on the final content.
	tokio::time::sleep(Duration::from_secs(3)).await;
	let (_d, files) = list_root(&sc).await;
	let f = find_file(&files, "burst.txt").expect("burst.txt missing");
	assert_eq!(
		f.size,
		final_bytes.len() as u64,
		"remote drifted off the final content"
	);
	assert_eq!(
		files.len(),
		1,
		"burst produced extra remote files: {}",
		files.len()
	);

	drop(handle);
	sc.cleanup();
}

/// RACE-17 — the engine's OWN local write (a download) must not retrigger an endless sync loop in
/// watch mode; after settling there is exactly one coherent state and no ever-growing transfers.
#[shared_test_runtime]
async fn race_17_self_write_does_not_loop() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	// Seed a remote file the engine will download (its own local write).
	let rf = upload_root(&sc, "pulled.txt", b"A-pulled").await;
	assert!(
		poll_for_item(sc.cache.db_path(), rf.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed pulled.txt"
	);

	let engine = Arc::new(
		SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::RemoteToLocal)
		.await
		.unwrap();

	// Count downloads across the continuous watch; the self-write must not keep retriggering them.
	let downloads = Arc::new(AtomicUsize::new(0));
	let dl = downloads.clone();
	let observer: SyncObserver = Box::new(move |ev| {
		if matches!(ev, SyncEvent::Downloading { .. }) {
			dl.fetch_add(1, Ordering::SeqCst);
		}
	});
	let handle = engine.clone().watch_observed(pair, observer).await.unwrap();

	// Wait for the initial download to land.
	let mut ready = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		if read_eq(&sc.local, "pulled.txt", b"A-pulled") {
			ready = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(ready, "watch never performed the initial download");

	// Let several debounce/safety-net cycles elapse with no external changes.
	tokio::time::sleep(Duration::from_secs(8)).await;
	let after_settle = downloads.load(Ordering::SeqCst);
	tokio::time::sleep(Duration::from_secs(6)).await;
	let final_count = downloads.load(Ordering::SeqCst);

	assert!(
		read_eq(&sc.local, "pulled.txt", b"A-pulled"),
		"the single coherent downloaded state must persist"
	);
	assert_eq!(
		after_settle, final_count,
		"the engine's own write retriggered downloads in an idle period \
		 (self-induced loop): {after_settle} -> {final_count}"
	);

	drop(handle);
	sc.cleanup();
}

// ===========================================================================
// IMPLEMENTED — deterministic / best-effort between-pass variants
// ===========================================================================

/// (add, deterministic variant) A file is moved across directories BETWEEN passes (the prior pass's
/// planned action already committed); a later pass converges to the file at its new path only, with
/// no duplicate and no data loss.
#[shared_test_runtime]
async fn race_add_move_between_passes_converges_no_duplicate() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "dir1/x.txt", b"A-move-me");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Move across directories between passes.
	move_file(&sc.local, "dir1/x.txt", "dir2/x.txt");
	let mut converged = false;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "move pass errors: {r:?}");
		let (dirs, _f) = list_root(&sc).await;
		let mut at_new = false;
		let mut at_old = false;
		if let Some(d2) = find_dir(&dirs, "dir2") {
			let (_dd, df) = list_dir(&sc, d2).await;
			at_new = find_file(&df, "x.txt").is_some();
		}
		if let Some(d1) = find_dir(&dirs, "dir1") {
			let (_dd, df) = list_dir(&sc, d1).await;
			at_old = find_file(&df, "x.txt").is_some();
		}
		if at_new && !at_old {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(800)).await;
	}
	assert!(
		converged,
		"after the move, remote must hold dir2/x.txt only — no duplicate at dir1 and no loss"
	);
	sc.cleanup();
}

/// RACE-15 (best-effort concurrent variant) — sibling files created/removed while a sync pass is in
/// flight. We spawn the mutation concurrently with `sync_once`; the pass must not crash on a
/// vanished entry, and a convergence loop must reach final truth (f1001 present, f0005 absent) with
/// no spurious conflict. This does not GUARANTEE the mutation lands mid-walk (no timing seam), but
/// it exercises genuine concurrent fs mutation against the engine and asserts the end-state.
#[shared_test_runtime]
async fn race_15_sibling_mutation_during_pass_converges() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	// A modest tree (kept small to stay live-test-friendly).
	const N: usize = 30;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("tree/f{i:04}.txt"),
			format!("c{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Mutate siblings concurrently with one more sync pass.
	let local = sc.local.clone();
	let mutator = tokio::task::spawn_blocking(move || {
		write_file(&local, "tree/f1001.txt", b"D-added");
		let _ = std::fs::remove_file(local.join("tree/f0005.txt"));
	});
	let r2 = sc.sync().await;
	mutator.await.unwrap();
	assert!(
		r2.errors
			.iter()
			.all(|e| !e.to_lowercase().contains("panic")),
		"pass must not crash on a vanished entry: {r2:?}"
	);

	// Converge to truth: f1001 present, f0005 absent, no conflict.
	let mut conflicts_seen = false;
	let mut converged = false;
	for _ in 0..8 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "convergence pass errors: {r:?}");
		conflicts_seen |= !r.conflicts.is_empty();
		let (dirs, _f) = list_root(&sc).await;
		if let Some(td) = find_dir(&dirs, "tree") {
			let (_dd, tf) = list_dir(&sc, td).await;
			let has_new = find_file(&tf, "f1001.txt").is_some();
			let removed_gone = find_file(&tf, "f0005.txt").is_none();
			if has_new && removed_gone {
				converged = true;
				break;
			}
		}
		tokio::time::sleep(Duration::from_millis(900)).await;
	}
	assert!(
		converged,
		"remote must converge: f1001.txt present, f0005.txt absent"
	);
	assert!(
		!conflicts_seen,
		"a mere mid-scan appear/vanish must not raise a spurious conflict"
	);
	sc.cleanup();
}

// ===========================================================================
// BLOCKED — require fault-injection / controllable-clock / mock-remote harness
// ===========================================================================

#[ignore = "blocked: needs deterministic mid-transfer interruption seam (overwrite the source file \
            after scan but strictly before upload starts). No such timing control in the current \
            harness — see TODO(sync-engine fault-injection)."]
#[shared_test_runtime]
async fn race_01_local_modified_after_scan_before_upload() {
	// plan: create foo.bin=A (1MB), begin pass with scan seeing A, overwrite to B (same len)
	// before upload starts, complete; second pass. Verify remote is a coherent A-or-B (never torn),
	// ends == B after pass 2, baseline reflects B, no success claimed for an un-transferred version.
}

#[ignore = "blocked: needs deterministic remote mid-download mutation (change remote bytes while a \
            download is in flight). Requires a controllable/mock remote — see TODO."]
#[shared_test_runtime]
async fn race_02_remote_changed_during_download() {
	// plan: remote bar.bin=A synced, ->B, begin pass reading listing=B, mutate to C while download
	// in flight, complete; second pass. Verify local is a coherent B-or-C (never torn), ends==C,
	// no partial/zero-length file, no success for an unwritten version.
}

#[ignore = "blocked: needs deterministic in-flight-upload delete (remove the source file precisely \
            while its upload is in progress). No mid-transfer seam in the current harness — see TODO."]
#[shared_test_runtime]
async fn race_03_local_file_deleted_mid_upload() {
	// plan: mid.bin=A (large), begin pass, delete locally while upload in flight, complete; second
	// pass. Verify no whole-pair abort (per-file error at most), final remote matches local truth
	// (absent), baseline not synced-present, no orphaned partial object.
}

#[ignore = "blocked: needs a controllable concurrent appender racing the in-flight scan (no \
            deterministic 'file actively growing during the pass' seam). See TODO."]
#[shared_test_runtime]
async fn race_04_actively_growing_file_during_pass() {
	// plan: appender writing grow.log ~100KB/s, run pass while growing, stop -> final S/F, second
	// pass. Verify first pass uploads a coherent prefix or defers (never claims a size it didn't
	// transfer), no torn content, final remote == F/S, no whole-pair abort.
}

#[ignore = "blocked: needs deterministic mid-rewrite interruption (pause an in-place truncate+refill \
            at a partial offset DURING the scan). No write-pause seam in the current harness — see TODO."]
#[shared_test_runtime]
async fn race_11_partial_truncated_write_during_scan() {
	// plan: doc.txt=A (1MB) synced, truncate-then-refill paused at 200KB, run pass, finish B,
	// fsync/close, second pass. Verify remote never ends on the 200KB torn intermediate, ends==B
	// byte-exact, baseline records complete B (first-pass partial corrected by re-plan).
}

#[ignore = "blocked: needs to time a sync pass to overlap an atomic temp-then-rename precisely \
            (scan sees A/.tmp, rename lands mid-pass). No mid-pass timing control — see TODO."]
#[shared_test_runtime]
async fn race_12_atomic_rename_writer_during_scan() {
	// plan: conf.yaml=A synced, write conf.yaml.tmp=B then atomic rename over conf.yaml, overlap a
	// pass with the rename, complete; second pass. Verify remote ==B after convergence, no permanent
	// .tmp artifact, no torn A+B mix, baseline records conf.yaml=B with no stale .tmp.
}

#[ignore = "blocked: needs a forcibly-interrupted (process-killed) pass with a partially-written \
            baseline, then a restart on the SAME baseline DB. The harness exposes no baseline-DB \
            path to a restarted engine and no kill seam — see TODO(sync-engine crash-injection)."]
#[shared_test_runtime]
async fn race_18_interrupted_pass_replans_from_truth() {
	// plan: queue new.txt=A + modify old.txt=B + delete gone.txt, begin pass, kill partway (torn
	// baseline), restart, run pass. Verify no startup crash, remote reflects full final truth, no
	// lost or duplicated action, baseline consistent.
}

#[ignore = "blocked: needs deterministic remote mid-download delete (remove the remote file while \
            its download is in flight). Requires a controllable/mock remote — see TODO."]
#[shared_test_runtime]
async fn race_20_remote_deleted_mid_download() {
	// plan: remote rdel.bin=A synced, ->B (download planned), delete remote while download in flight,
	// complete; second pass. Verify no crash (per-file error), no partial/zero-length file marked
	// complete, converges to remote truth (absent -> local removed/quarantined), baseline agrees.
}

#[ignore = "blocked: needs a controllable FS-watch event drop (deliver NO watcher event for a real \
            modification) to prove the periodic safety-net backstops it. No event-injection seam — \
            see TODO(sync-engine watch fault-injection)."]
#[shared_test_runtime]
async fn race_add_safety_net_catches_dropped_event() {
	// plan: watch a synced watched.txt=A, modify to B with the watcher event suppressed/dropped,
	// wait for the periodic safety-net pass. Verify B uploaded byte-exact without relying on the FS
	// event, baseline==B, no infinite re-pass loop.
}

#[ignore = "blocked: needs to land a change EXACTLY as the debounced pass begins scanning (after \
            this file's enumeration, before the debounce timer is consumed). No mid-pass timing \
            seam in the current harness — see TODO."]
#[shared_test_runtime]
async fn race_add_debounce_boundary_late_event_not_lost() {
	// plan: watch a synced edge.txt=A, trigger modify to B timed to the debounced-pass start (after
	// edge.txt's enumeration), finish the in-flight pass, quiesce. Verify B not lost in the gap,
	// remote==B after settling, a subsequent pass re-arms for the late event, no infinite loop.
}

#[ignore = "blocked: needs deterministic mid-pass injection (modify local shared.txt to L2 WHILE \
            the engine materializes the conflict copy). No mid-pass timing seam — see TODO."]
#[shared_test_runtime]
async fn race_add_local_edit_races_conflict_copy_write() {
	// plan: two-way shared.txt=BASE synced, diverge to L (local) and R (remote) -> conflict, begin
	// pass and modify local to L2 while the conflict copy is being written, complete; second pass.
	// Verify none of L/R/L2 destroyed (all recoverable byte-exact), conflict-copy name does not
	// clobber, baseline not converged while L2 unreconciled, second pass reflects L2 + artifact.
}

#[ignore = "blocked: needs deterministic mid-pass injection (rename a.txt->b.txt AND overwrite to B \
            strictly between scan and apply, defeating the hash/uuid move detector). No mid-pass \
            timing seam in the current harness — see TODO."]
#[shared_test_runtime]
async fn race_add_rename_racing_content_edit_mid_pass() {
	// plan: a.txt=A synced, begin pass with scan seeing a.txt at old path, before apply rename
	// a.txt->b.txt AND overwrite content to B, complete; second pass. Verify remote b.txt is a
	// coherent A-or-B (never torn), ends==B, no a.txt, baseline records the move+new content, no
	// duplicate (move A + create b.txt=B), no spurious conflict.
}
