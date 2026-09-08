//! Sync modes & directionality tests (`MODE-*`) for the two-way sync engine.
//!
//! Each test pins down the directional contract of one [`SyncMode`]: exactly what it propagates,
//! in which direction, and — critically — what it must REFUSE to do (backup modes never delete on
//! their destination; one-directional modes never write back to their source; two-way surfaces
//! genuine divergence as a conflict rather than silently losing a side).
use std::{borrow::Cow, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::sync_engine::{Backlog, PlannedActionKind};
use filen_sdk_rs::{
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		file::RemoteFile,
	},
	sync_engine::SyncMode,
};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ----------------------------------------------------------------------------
// Local helpers (remote-side setup/verify via the SingleClient's public Client).
//
// `sc.resources.client` is the shared base client and `sc.resources.dir` the remote sync-root
// folder (matching the black-box example tests). The engine's remote view comes from `sc.cache`,
// which converges asynchronously — so after any direct remote mutation we WAIT for the cache to
// observe it (poll_for_item / poll_for_file_name / poll_for_item_absent / a cached-size poll)
// before driving `sc.sync()`.
// ----------------------------------------------------------------------------

/// The remote sync-root as a `DirType` for client list/create calls.
fn root_dirtype(sc: &SingleClient) -> DirType<'_, Normal> {
	DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir))
}

/// Upload a file with exact bytes directly to the remote root; returns the created `RemoteFile`.
async fn upload_remote(sc: &SingleClient, name: &str, data: &[u8]) -> RemoteFile {
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

/// List (dirs, files) directly under the remote root via the client (ground truth).
async fn list_remote_root(
	sc: &SingleClient,
) -> (Vec<filen_sdk_rs::fs::dir::RemoteDirectory>, Vec<RemoteFile>) {
	sc.resources
		.client
		.list_dir(&root_dirtype(sc), None::<&fn(u64, Option<u64>)>)
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

/// Wait until the cache observes `uuid` under the remote root, asserting on timeout.
async fn wait_cache_has(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

/// Wait until the cache no longer holds `uuid`, asserting on timeout.
async fn wait_cache_absent(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item_absent(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped item {uuid}"
	);
}

/// Re-upload `name` with new bytes (the server versions it into a NEW uuid), then wait until the
/// cache reflects the new uuid AND its new size — so the engine reconciles against fully-committed
/// new content rather than a transient mid-versioning state. Returns the new uuid.
async fn modify_remote(sc: &SingleClient, name: &str, new_data: &[u8]) -> Uuid {
	let new_rf = upload_remote(sc, name, new_data).await;
	let new_uuid: Uuid = new_rf.uuid();
	wait_cache_has(sc, new_uuid).await;
	let db = sc.cache.db_path().to_path_buf();
	let want = new_data.len() as i64;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(want)
		})
		.await,
		"cache never reflected the modified remote size for {name}"
	);
	new_uuid
}

/// A bounded retry around a single download pass to absorb the cache's eventual-consistency window
/// after a remote re-version (mirrors the black-box `r2l_remote_modification_redownloads` guard).
async fn sync_expect_download(sc: &SingleClient) -> SyncReportLike {
	let mut r = sc.sync().await;
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while r.downloaded == 0
		&& r.errors.iter().any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(Duration::from_millis(500)).await;
		r = sc.sync().await;
	}
	r
}

// `SyncReport` is returned by `sc.sync()`; alias so the helper signature reads clearly without
// importing the type name twice.
type SyncReportLike = filen_sdk_rs::sync_engine::SyncReport;

// ============================================================================
// MODE-01..04 — LocalToRemote
// ============================================================================

#[shared_test_runtime]
async fn mode_01_l2r_local_create_pushes_up_byte_exact() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "foo.txt", b"AAA");

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "errors: {report:?}");
	assert_eq!(report.uploaded, 1, "{report:?}");
	assert_eq!(report.remotely_trashed, 0, "{report:?}");
	assert_eq!(report.locally_deleted, 0, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	// Local untouched.
	assert!(
		read_eq(&sc.local, "foo.txt", b"AAA"),
		"local foo.txt changed"
	);
	// Remote has exactly foo.txt with 3 bytes and nothing else.
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 1, "remote should hold exactly one file");
	assert_eq!(find_file(&files, "foo.txt").unwrap().size, 3, "{files:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_02_l2r_mirrors_local_deletion() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "foo.txt", b"X");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	std::fs::remove_file(sc.local.join("foo.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.remotely_trashed, 1, "local delete must mirror: {r2:?}");
	assert_eq!(r2.held_deletions(), 0, "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "foo.txt").is_none(),
		"remote still has it"
	);

	// Baseline advanced -> third pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_03_l2r_refuses_to_pull_remote_only_edit() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "bar.txt", b"X");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Modify on the REMOTE side only.
	modify_remote(&sc, "bar.txt", b"Y").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// One-directional push: never pull, never conflict; local source is authoritative.
	assert_eq!(r2.downloaded, 0, "must not pull remote edit down: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "push mode must not conflict: {r2:?}");
	// Local content is unchanged ('X') — no local data lost or modified.
	assert!(read_eq(&sc.local, "bar.txt", b"X"), "local content changed");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_04_l2r_remote_only_deletion_recreated_from_local() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "baz.txt", b"Z");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	let (_d, mut files0) = list_remote_root(&sc).await;
	let idx = files0
		.iter()
		.position(|f| f.name() == Some("baz.txt"))
		.unwrap();
	let orig = files0[idx].uuid();

	// Delete on the REMOTE side only.
	sc.resources
		.client
		.trash_file(&mut files0[idx])
		.await
		.unwrap();
	wait_cache_absent(&sc, orig).await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Source authoritative: re-upload, never mirror the remote delete back to local.
	assert_eq!(r2.uploaded, 1, "remote delete must be restored: {r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert!(read_eq(&sc.local, "baz.txt", b"Z"), "local untouched");

	let (_d, files1) = list_remote_root(&sc).await;
	assert_eq!(find_file(&files1, "baz.txt").unwrap().size, 1, "{files1:?}");

	sc.cleanup();
}

// ============================================================================
// MODE-05..06 + (add) — RemoteToLocal
// ============================================================================

#[shared_test_runtime]
async fn mode_05_r2l_remote_create_pulls_down_byte_exact() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_remote(&sc, "doc.txt", b"HELLO").await;
	wait_cache_has(&sc, rf.uuid()).await;

	let report = sc.sync().await;
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(report.downloaded, 1, "{report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert_eq!(report.locally_deleted, 0, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");

	assert!(read_eq(&sc.local, "doc.txt", b"HELLO"), "byte mismatch");
	// Remote still present unchanged.
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(find_file(&files, "doc.txt").unwrap().size, 5, "{files:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_06_r2l_mirrors_remote_deletion_locally() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let mut rf = upload_remote(&sc, "doc.txt", b"D").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "doc.txt", b"D"), "download failed");

	// Delete on the remote.
	sc.resources.client.trash_file(&mut rf).await.unwrap();
	wait_cache_absent(&sc, rf.uuid()).await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.locally_deleted, 1, "remote delete must mirror: {r2:?}");
	assert!(
		!sc.local.join("doc.txt").exists(),
		"local file still present"
	);

	// Baseline advanced -> third pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.locally_deleted, 0, "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_07_r2l_refuses_to_push_local_only_edit() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_remote(&sc, "note.txt", b"P").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");

	// Modify the LOCAL side only.
	write_file(&sc.local, "note.txt", b"Q");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Pull mode: never push, never conflict; remote source is authoritative.
	assert_eq!(r2.uploaded, 0, "must not push local edit up: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "pull mode must not conflict: {r2:?}");

	// Remote content unchanged ('P').
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(
		find_file(&files, "note.txt").unwrap().size,
		1,
		"remote changed: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_r2l_remote_edit_pulls_down() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let rf = upload_remote(&sc, "re.txt", b"P").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "re.txt", b"P"), "{r1:?}");

	// Edit the remote (re-version), then pull.
	modify_remote(&sc, "re.txt", b"P2").await;
	let r2 = sync_expect_download(&sc).await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.downloaded, 1, "remote edit must pull: {r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert!(read_eq(&sc.local, "re.txt", b"P2"), "local not updated");

	let r3 = sc.sync().await;
	assert_eq!(r3.downloaded, 0, "second pass should be a no-op: {r3:?}");

	sc.cleanup();
}

// ============================================================================
// MODE-08..12 + (add) — TwoWay
// ============================================================================

#[shared_test_runtime]
async fn mode_08_twoway_local_up_and_remote_down_one_pass() {
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "only.remote.txt", b"R").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "only.local.txt", b"L");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(
		r1.conflicts.len(),
		0,
		"disjoint paths must not conflict: {r1:?}"
	);

	// Both sides hold both files.
	assert!(
		read_eq(&sc.local, "only.remote.txt", b"R"),
		"remote->local failed"
	);
	assert!(read_eq(&sc.local, "only.local.txt", b"L"), "local intact");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "only.local.txt").is_some(),
		"local->remote failed"
	);
	assert!(find_file(&files, "only.remote.txt").is_some(), "{files:?}");

	// Second pass is a no-op.
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_09_twoway_divergent_edit_conflicts_nondestructively() {
	let sc = single_client(SyncMode::TwoWay).await;
	let base = b"BASE";
	let rf = upload_remote(&sc, "c.txt", base).await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "c.txt", base);
	// Establish a shared baseline (identical content -> no conflict).
	let r1 = sc.sync().await;
	assert_eq!(
		r1.conflicts.len(),
		0,
		"baseline pass should be clean: {r1:?}"
	);

	// Diverge BOTH sides.
	write_file(&sc.local, "c.txt", b"LOCAL-EDIT");
	modify_remote(&sc, "c.txt", b"REMOTE-EDIT").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.conflict_paths().any(|c| c.contains("c.txt")),
		"expected c.txt conflict: {r2:?}"
	);
	// No destructive action and the local side is left intact (recoverable).
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert!(
		read_eq(&sc.local, "c.txt", b"LOCAL-EDIT"),
		"local clobbered"
	);
	// The remote edit is still present (recoverable) — nothing destroyed without a copy.
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "c.txt").is_some(),
		"remote side destroyed: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_10_twoway_one_sided_edit_not_a_conflict() {
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "d.txt", b"BASE").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "d.txt", b"BASE");
	let r1 = sc.sync().await;
	assert_eq!(
		r1.conflicts.len(),
		0,
		"baseline pass should be clean: {r1:?}"
	);

	// Edit ONLY the local side.
	write_file(&sc.local, "d.txt", b"NEW");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"one-sided edit must not conflict: {r2:?}"
	);
	assert_eq!(r2.uploaded, 1, "local edit should push up: {r2:?}");
	assert!(read_eq(&sc.local, "d.txt", b"NEW"), "local changed");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(
		find_file(&files, "d.txt").unwrap().size,
		3,
		"remote not updated: {files:?}"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "second pass should be a no-op: {r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_11_twoway_one_sided_delete_propagates() {
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "e.txt", b"E").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "e.txt", b"E");
	let r1 = sc.sync().await;
	assert_eq!(r1.conflicts.len(), 0, "baseline pass clean: {r1:?}");

	// Delete ONLY locally; remote untouched.
	std::fs::remove_file(sc.local.join("e.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"clean delete must not conflict: {r2:?}"
	);
	assert_eq!(r2.remotely_trashed, 1, "delete must propagate: {r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "e.txt").is_none(),
		"remote still has it: {files:?}"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.remotely_trashed, 0, "second pass no-op: {r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_12_twoway_delete_vs_edit_conflicts_nondestructively() {
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "f.txt", b"BASE").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "f.txt", b"BASE");
	let r1 = sc.sync().await;
	assert_eq!(r1.conflicts.len(), 0, "baseline pass clean: {r1:?}");

	// Delete on local, edit on remote.
	std::fs::remove_file(sc.local.join("f.txt")).unwrap();
	modify_remote(&sc, "f.txt", b"REMOTE-EDIT").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.conflict_paths().any(|c| c.contains("f.txt")),
		"delete-vs-edit must conflict: {r2:?}"
	);
	// The edited remote file must NOT be silently destroyed by the local delete.
	assert_eq!(r2.remotely_trashed, 0, "remote edit destroyed: {r2:?}");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "f.txt").is_some(),
		"remote edit lost: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_twoway_convergent_identical_edit_not_a_conflict() {
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "cv.txt", b"BASE").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "cv.txt", b"BASE");
	let r1 = sc.sync().await;
	assert_eq!(r1.conflicts.len(), 0, "baseline pass clean: {r1:?}");

	// Both sides edit to the SAME new content.
	write_file(&sc.local, "cv.txt", b"SAME-NEW");
	modify_remote(&sc, "cv.txt", b"SAME-NEW").await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"identical convergent edits must not conflict: {r2:?}"
	);
	assert!(
		read_eq(&sc.local, "cv.txt", b"SAME-NEW"),
		"local content changed"
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(find_file(&files, "cv.txt").unwrap().size, 8, "{files:?}");

	// Baseline advanced to SAME-NEW -> second pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_twoway_create_create_different_content_conflicts() {
	let sc = single_client(SyncMode::TwoWay).await;
	// No baseline entry for new.txt: distinct creates on both sides at the same path.
	let rf = upload_remote(&sc, "new.txt", b"REMOTE-NEW").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "new.txt", b"LOCAL-NEW");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert!(
		r1.conflict_paths().any(|c| c.contains("new.txt")),
		"create/create divergence must conflict: {r1:?}"
	);
	// Neither create is destroyed.
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert!(
		read_eq(&sc.local, "new.txt", b"LOCAL-NEW"),
		"local create clobbered"
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "new.txt").is_some(),
		"remote create lost: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_twoway_convergent_delete_clean() {
	let sc = single_client(SyncMode::TwoWay).await;
	let mut rf = upload_remote(&sc, "dd.txt", b"DD").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "dd.txt", b"DD");
	let r1 = sc.sync().await;
	assert_eq!(r1.conflicts.len(), 0, "baseline pass clean: {r1:?}");

	// Delete on BOTH sides.
	std::fs::remove_file(sc.local.join("dd.txt")).unwrap();
	sc.resources.client.trash_file(&mut rf).await.unwrap();
	wait_cache_absent(&sc, rf.uuid()).await;

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"convergent delete must not conflict: {r2:?}"
	);
	// Nothing to re-create or re-download.
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert!(!sc.local.join("dd.txt").exists(), "local resurrected");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "dd.txt").is_none(),
		"remote resurrected: {files:?}"
	);

	// Second pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MODE-13..16 + (add) — Backup modes
// ============================================================================

#[shared_test_runtime]
async fn mode_13_local_backup_never_mirrors_local_deletion() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "k.txt", b"K");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Delete locally — backup must NOT remove the remote copy.
	std::fs::remove_file(sc.local.join("k.txt")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not trash remote: {r2:?}"
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "k.txt").is_some(),
		"remote copy was deleted: {files:?}"
	);

	// A subsequent create still propagates (additive backup is not read-only).
	write_file(&sc.local, "new.txt", b"N");
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.uploaded, 1, "create must still push: {r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	let (_dirs, files2) = list_remote_root(&sc).await;
	assert!(
		find_file(&files2, "k.txt").is_some(),
		"k.txt vanished: {files2:?}"
	);
	assert!(
		find_file(&files2, "new.txt").is_some(),
		"new.txt not pushed: {files2:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_14_local_backup_pushes_edit() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "m.txt", b"OLD");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	write_file(&sc.local, "m.txt", b"UPDATED");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 1, "edit must push: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert!(read_eq(&sc.local, "m.txt", b"UPDATED"), "local changed");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(
		find_file(&files, "m.txt").unwrap().size,
		7,
		"remote not updated: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_15_remote_backup_never_mirrors_remote_deletion() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let mut rf = upload_remote(&sc, "r.txt", b"R").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "r.txt", b"R"), "download failed");

	// Delete on remote — backup must NOT remove the local copy.
	sc.resources.client.trash_file(&mut rf).await.unwrap();
	wait_cache_absent(&sc, rf.uuid()).await;
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.locally_deleted, 0,
		"backup must not delete local: {r2:?}"
	);
	assert!(read_eq(&sc.local, "r.txt", b"R"), "local copy was deleted");

	// A subsequent remote create still propagates down.
	let rnew = upload_remote(&sc, "rnew.txt", b"RN").await;
	wait_cache_has(&sc, rnew.uuid()).await;
	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(r3.downloaded, 1, "create must still pull: {r3:?}");
	assert_eq!(r3.locally_deleted, 0, "{r3:?}");
	assert!(read_eq(&sc.local, "r.txt", b"R"), "r.txt vanished locally");
	assert!(read_eq(&sc.local, "rnew.txt", b"RN"), "rnew not pulled");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_16_remote_backup_refuses_to_push_local_only() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let rf = upload_remote(&sc, "s.txt", b"S").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");

	// Modify the LOCAL side only — remote-backup must never write to the remote.
	write_file(&sc.local, "s.txt", b"S2");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "remote-backup must never push: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(
		find_file(&files, "s.txt").unwrap().size,
		1,
		"remote mutated: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_local_backup_refuses_remote_edit_to_local() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "lb.txt", b"A");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Modify the REMOTE side only; local must not be mutated and no pull happens.
	modify_remote(&sc, "lb.txt", b"B").await;
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.downloaded, 0,
		"backup-out must never pull remote edit: {r2:?}"
	);
	assert_eq!(
		r2.conflicts.len(),
		0,
		"one-directional family must not conflict: {r2:?}"
	);
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert!(
		read_eq(&sc.local, "lb.txt", b"A"),
		"local source was mutated"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_remote_backup_pulls_remote_edit() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let rf = upload_remote(&sc, "rb.txt", b"OLD").await;
	wait_cache_has(&sc, rf.uuid()).await;
	let r1 = sc.sync().await;
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "rb.txt", b"OLD"), "{r1:?}");

	modify_remote(&sc, "rb.txt", b"UPDATED").await;
	let r2 = sync_expect_download(&sc).await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.downloaded, 1, "remote edit must pull: {r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert!(
		read_eq(&sc.local, "rb.txt", b"UPDATED"),
		"local not updated"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.downloaded, 0, "second pass should be a no-op: {r3:?}");

	sc.cleanup();
}

// ============================================================================
// MODE-17..19 — First sync (no prior baseline)
// ============================================================================

#[shared_test_runtime]
async fn mode_17_first_sync_full_directional_copy() {
	// local->remote: populated source (local) into an empty destination (remote).
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "dir/a.txt", b"1");
	write_file(&sc.local, "dir/b.txt", b"2");
	write_file(&sc.local, "top.txt", b"3");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "dir/ should be created: {r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");

	let (dirs, files) = list_remote_root(&sc).await;
	assert!(find_file(&files, "top.txt").is_some(), "top.txt missing");
	let dir = find_dir(&dirs, "dir").expect("dir/ missing");
	let (_dd, dir_files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert_eq!(
		find_file(&dir_files, "a.txt").unwrap().size,
		1,
		"{dir_files:?}"
	);
	assert_eq!(
		find_file(&dir_files, "b.txt").unwrap().size,
		1,
		"{dir_files:?}"
	);

	// Source unchanged; second pass is a no-op.
	assert!(read_eq(&sc.local, "dir/a.txt", b"1"), "source changed");
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_18_first_sync_does_not_wipe_populated_destination() {
	// local->remote: destination (remote) already holds keep-dest.txt; source has keep-src.txt.
	let sc = single_client(SyncMode::LocalToRemote).await;
	let dest = upload_remote(&sc, "keep-dest.txt", b"D").await;
	wait_cache_has(&sc, dest.uuid()).await;
	write_file(&sc.local, "keep-src.txt", b"S");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	// No baseline: the pre-existing destination file must not be treated as a propagated delete.
	assert_eq!(
		r1.remotely_trashed, 0,
		"first sync must not wipe destination: {r1:?}"
	);
	// The one pre-existing destination file is HELD (not trashed) by the first-sync guard — that
	// hold is the no-wipe mechanism, so held_deletions is 1, not 0.
	assert_eq!(r1.held_deletions(), 1, "{r1:?}");
	assert_eq!(r1.uploaded, 1, "source file should be copied: {r1:?}");

	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "keep-dest.txt").is_some(),
		"destination wiped: {files:?}"
	);
	assert!(
		find_file(&files, "keep-src.txt").is_some(),
		"source not copied: {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_19_identical_no_baseline_recognized_as_synced() {
	// two-way: same content on both sides with no baseline -> no churn.
	let sc = single_client(SyncMode::TwoWay).await;
	let rf = upload_remote(&sc, "same.txt", b"EQ").await;
	wait_cache_has(&sc, rf.uuid()).await;
	write_file(&sc.local, "same.txt", b"EQ");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 0, "{r1:?}");
	assert_eq!(r1.downloaded, 0, "{r1:?}");
	assert_eq!(r1.remotely_trashed, 0, "{r1:?}");
	assert_eq!(r1.locally_deleted, 0, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");

	assert!(read_eq(&sc.local, "same.txt", b"EQ"), "local changed");
	let (_dirs, files) = list_remote_root(&sc).await;
	assert_eq!(files.len(), 1, "remote churned: {files:?}");
	assert_eq!(find_file(&files, "same.txt").unwrap().size, 2, "{files:?}");

	// Baseline established -> second pass is also a no-op.
	let r2 = sc.sync().await;
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	sc.cleanup();
}

// ============================================================================
// MODE-20..22 — Mode switching (BLOCKED: no public mode-switch API)
// ============================================================================

// `SyncEngine::reconfigure_pair` changes an established pair's `SyncMode` prospectively: the
// baseline is kept, nothing is re-run, and the next pass reconciles the sides as they are now under
// the new rules. `add_pair` with a different mode is an error rather than a silent switch, so these
// tests go through `reconfigure_pair`.

/// MODE-20 — switching two-way -> local->remote makes the local side authoritative: a local delete
/// is mirrored, and a remote-only edit is overwritten instead of pulled.
#[shared_test_runtime]
async fn mode_20_switch_twoway_to_l2r_changes_deletion_directionality() {
	let sc = single_client(SyncMode::TwoWay).await;
	write_file(&sc.local, "t.txt", b"T");
	write_file(&sc.local, "u.txt", b"U-local");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	sc.engine
		.reconfigure_pair(sc.pair, SyncMode::LocalToRemote, Backlog::Propagate)
		.await
		.unwrap();
	assert_eq!(
		sc.engine.list_pairs().await.unwrap()[0].mode,
		SyncMode::LocalToRemote,
		"the pair reports its new mode"
	);

	// A remote-only edit of u.txt that two-way would have PULLED, plus a local delete of t.txt.
	modify_remote(&sc, "u.txt", b"U-remote-edit").await;
	std::fs::remove_file(sc.local.join("t.txt")).unwrap();

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 1,
		"the local delete is mirrored: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "the remote edit is NOT pulled: {r2:?}");
	assert!(
		read_eq(&sc.local, "u.txt", b"U-local"),
		"the local copy must survive under local->remote"
	);
	assert_eq!(
		r2.uploaded, 1,
		"the local copy is re-pushed instead: {r2:?}"
	);

	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "t.txt").is_none(),
		"t.txt still on remote"
	);
	assert_eq!(
		find_file(&files, "u.txt").unwrap().size,
		b"U-local".len() as u64
	);

	sc.cleanup();
}

/// MODE-21 — switching local->remote -> local-backup stops mirroring deletions from the next pass
/// on (contrast MODE-02, where the same delete is propagated).
#[shared_test_runtime]
async fn mode_21_switch_l2r_to_local_backup_stops_mirroring_deletes() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "w.txt", b"W");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	sc.engine
		.reconfigure_pair(sc.pair, SyncMode::LocalBackup, Backlog::Propagate)
		.await
		.unwrap();
	std::fs::remove_file(sc.local.join("w.txt")).unwrap();

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"a backup mode never deletes: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions(),
		0,
		"not held either — never planned: {r2:?}"
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "w.txt").is_some(),
		"the backup copy must survive the local delete"
	);

	sc.cleanup();
}

/// MODE-22 — a mode switch is prospective in the PASSES, not in the divergence: switching a backup
/// pair to local->remote makes the deletions the backup mode had left standing pending under the
/// new rules, and the very next pass propagates them.
///
/// (The stub this replaces expected the opposite — that a pre-switch local delete would never be
/// mirrored. That would need the switch to rewrite the baseline to forget the deleted path, i.e. to
/// act retroactively on history, which is exactly what `reconfigure_pair` promises NOT to do.
/// `plan_pair` after a switch is how a caller sees the backlog before it is applied.)
#[shared_test_runtime]
async fn mode_22_switch_local_backup_to_l2r_applies_the_standing_divergence() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "x.txt", b"X");
	write_file(&sc.local, "y.txt", b"Y");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Under local-backup the delete is not propagated: x.txt survives remotely.
	std::fs::remove_file(sc.local.join("x.txt")).unwrap();
	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");

	sc.engine
		.reconfigure_pair(sc.pair, SyncMode::LocalToRemote, Backlog::Propagate)
		.await
		.unwrap();

	// The dry run shows the standing divergence BEFORE any pass acts on it.
	let plan = sc.engine.plan_pair(sc.pair).await.unwrap();
	assert!(
		plan.actions
			.iter()
			.any(|a| a.rel_path == "x.txt" && a.kind == PlannedActionKind::TrashRemote),
		"the switch makes the standing local delete a pending remote trash: {plan:?}"
	);

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.remotely_trashed, 1,
		"the standing deletion is propagated under the new mode: {r3:?}"
	);
	assert_eq!(
		r3.uploaded, 0,
		"no re-transfer from the mode change: {r3:?}"
	);
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	// A later local delete is mirrored the same way, and y.txt was never re-transferred.
	std::fs::remove_file(sc.local.join("y.txt")).unwrap();
	let r4 = sc.sync().await;
	assert_eq!(r4.remotely_trashed, 1, "{r4:?}");
	let (_d, files) = list_remote_root(&sc).await;
	assert!(files.is_empty(), "both deletions landed: {files:?}");

	sc.cleanup();
}

/// MODE-22b — the same switch with `Backlog::AdoptDestination`: the standing backlog is re-seeded
/// from the DESTINATION before the new mode takes effect, so the backup copies of files the source
/// deleted count as intended. The next pass plans nothing at all, and the copies survive — while a
/// deletion made AFTER the switch still propagates, since the adoption is a one-off re-seed and not
/// a permanent exemption.
#[shared_test_runtime]
async fn mode_22b_switch_local_backup_to_l2r_can_adopt_the_destination_instead() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "x.txt", b"X");
	write_file(&sc.local, "y.txt", b"Y");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Under local-backup the delete is not propagated: x.txt survives remotely (MODE-22's setup).
	std::fs::remove_file(sc.local.join("x.txt")).unwrap();
	let r2 = sc.sync().await;
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");

	sc.engine
		.reconfigure_pair(sc.pair, SyncMode::LocalToRemote, Backlog::AdoptDestination)
		.await
		.unwrap();
	assert_eq!(
		sc.engine.list_pairs().await.unwrap()[0].mode,
		SyncMode::LocalToRemote,
		"the pair reports its new mode"
	);

	// Contrast MODE-22, where the very same dry run shows a TrashRemote for x.txt.
	let plan = sc.engine.plan_pair(sc.pair).await.unwrap();
	assert!(
		plan.actions.is_empty() && plan.held.is_empty(),
		"an adopted backlog leaves the first pass with nothing to do: {plan:?}"
	);

	let r3 = sc.sync().await;
	assert!(r3.errors.is_empty(), "{r3:?}");
	assert_eq!(
		r3.remotely_trashed, 0,
		"the adopted copy must not be trashed: {r3:?}"
	);
	assert_eq!(r3.uploaded, 0, "nor pushed back to the source: {r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");
	assert!(
		!sc.local.join("x.txt").exists(),
		"a one-way mirror never writes to its source"
	);
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "x.txt").is_some(),
		"the adopted backup copy must survive the switch"
	);

	// A deletion made after the switch is mirrored the ordinary way.
	std::fs::remove_file(sc.local.join("y.txt")).unwrap();
	let r4 = sc.sync().await;
	assert!(r4.errors.is_empty(), "{r4:?}");
	assert_eq!(r4.remotely_trashed, 1, "{r4:?}");
	let (_d, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "x.txt").is_some(),
		"the adopted copy must still be there"
	);
	assert!(
		find_file(&files, "y.txt").is_none(),
		"the later deletion did not propagate"
	);

	sc.cleanup();
}

/// Re-registering an established pair with a different mode is refused rather than silently
/// re-pointing it; the same mode is idempotent.
#[shared_test_runtime]
async fn mode_add_pair_refuses_a_silent_mode_change() {
	let sc = single_client(SyncMode::TwoWay).await;

	let same = sc
		.engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::TwoWay)
		.await
		.expect("re-registering the same pair is idempotent");
	assert_eq!(same, sc.pair, "the same roots return the same id");

	let error = sc
		.engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::LocalBackup)
		.await
		.expect_err("a different mode must be refused")
		.to_string();
	assert!(
		error.contains("reconfigure_pair"),
		"the refusal must point at the explicit call: {error}"
	);
	assert_eq!(
		sc.engine.list_pairs().await.unwrap()[0].mode,
		SyncMode::TwoWay,
		"the refused call left the pair alone"
	);

	sc.cleanup();
}

// ============================================================================
// MODE-23..25 + (add) — Moves, empty-source safety, idempotency, dirs
// ============================================================================

#[shared_test_runtime]
async fn mode_23_local_rename_propagates_as_move_not_delete_create() {
	// local->remote (a one-directional move path; black-box already covers two-way separately).
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "old/name.txt", b"CONTENT");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	let (dirs0, _f0) = list_remote_root(&sc).await;
	let old0 = find_dir(&dirs0, "old").unwrap();
	let (_od, old_files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(old0)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let orig_uuid = find_file(&old_files, "name.txt").unwrap().uuid();

	// Move/rename locally (same content).
	move_file(&sc.local, "old/name.txt", "new/name.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.moved_remote, 1, "rename should be a remote move: {r2:?}");
	assert_eq!(r2.uploaded, 0, "rename must NOT re-upload bytes: {r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");

	let (dirs1, _f1) = list_remote_root(&sc).await;
	let new1 = find_dir(&dirs1, "new").expect("new/ dir missing");
	let (_nd, new_files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(new1)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let moved = find_file(&new_files, "name.txt").expect("moved file missing");
	assert_eq!(
		moved.uuid(),
		orig_uuid,
		"uuid preserved across move (content intact)"
	);
	assert_eq!(moved.size, 7, "content intact: {new_files:?}");

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_24_r2l_empty_source_leaves_populated_destination_intact() {
	// remote->local: empty source (remote) with a populated destination (local), no baseline.
	let sc = single_client(SyncMode::RemoteToLocal).await;
	write_file(&sc.local, "local-only.txt", b"L");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	// Empty source must NOT be read as "delete everything on the destination".
	assert_eq!(r1.locally_deleted, 0, "empty source wiped local: {r1:?}");
	// The single local-only file is HELD by the first-sync guard rather than deleted — holding is
	// the no-wipe mechanism, so held_deletions is 1.
	assert_eq!(r1.held_deletions(), 1, "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	assert!(
		read_eq(&sc.local, "local-only.txt", b"L"),
		"local data lost"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_25_twoway_idempotency_stable_across_repeated_passes() {
	let sc = single_client(SyncMode::TwoWay).await;
	// r.txt synced on both sides first (so a later local edit is a one-sided edit, not a create).
	let rbase = upload_remote(&sc, "r.txt", b"R0").await;
	wait_cache_has(&sc, rbase.uuid()).await;
	write_file(&sc.local, "r.txt", b"R0");
	let r0 = sc.sync().await;
	assert_eq!(r0.conflicts.len(), 0, "baseline pass clean: {r0:?}");

	// A mix: create p.txt locally, create q.txt remotely, edit r.txt locally.
	write_file(&sc.local, "p.txt", b"P");
	let q = upload_remote(&sc, "q.txt", b"Q").await;
	wait_cache_has(&sc, q.uuid()).await;
	write_file(&sc.local, "r.txt", b"R1-edited");

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	// p and q on both sides, r updated on remote.
	assert!(read_eq(&sc.local, "q.txt", b"Q"), "q not pulled");
	assert!(
		read_eq(&sc.local, "r.txt", b"R1-edited"),
		"local r changed unexpectedly"
	);
	let (_dirs, files) = list_remote_root(&sc).await;
	assert!(
		find_file(&files, "p.txt").is_some(),
		"p not pushed: {files:?}"
	);
	assert_eq!(
		find_file(&files, "r.txt").unwrap().size,
		9,
		"remote r not updated: {files:?}"
	);

	// Second and third passes must report ZERO actions and zero conflicts.
	for label in ["second", "third"] {
		let rn = sc.sync().await;
		assert!(rn.errors.is_empty(), "{label}: {rn:?}");
		assert_eq!(rn.uploaded, 0, "{label} pass uploaded: {rn:?}");
		assert_eq!(rn.downloaded, 0, "{label} pass downloaded: {rn:?}");
		assert_eq!(rn.local_dirs_created, 0, "{label}: {rn:?}");
		assert_eq!(rn.remote_dirs_created, 0, "{label}: {rn:?}");
		assert_eq!(rn.locally_deleted, 0, "{label}: {rn:?}");
		assert_eq!(rn.remotely_trashed, 0, "{label}: {rn:?}");
		assert_eq!(rn.moved_remote, 0, "{label}: {rn:?}");
		assert_eq!(rn.moved_local, 0, "{label}: {rn:?}");
		assert_eq!(rn.conflicts.len(), 0, "{label} spurious conflict: {rn:?}");
	}

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_local_backup_rename_does_not_delete_old_remote_copy() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "old/n.txt", b"C");
	let r1 = sc.sync().await;
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	// Move/rename locally. In backup mode the delete-half of a move is a destination deletion,
	// which backup must suppress — so the new path must appear and NO hard delete may occur.
	move_file(&sc.local, "old/n.txt", "new/n.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	// Backup never trashes the destination, regardless of whether the move is applied as a true
	// rename (moved_remote) or as an additive create of the new path.
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not trash on a move: {r2:?}"
	);

	let (dirs, _files) = list_remote_root(&sc).await;
	let new_dir = find_dir(&dirs, "new").expect("new/ dir missing on remote");
	let (_nd, new_files) = sc
		.resources
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(new_dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert_eq!(
		find_file(&new_files, "n.txt").map(|f| f.size),
		Some(1),
		"new/n.txt missing on remote: {new_files:?}"
	);
	// No data loss either way: the content is recoverable at the new path (asserted above), and the
	// backup contract forbids a destructive trash (asserted via remotely_trashed == 0).

	sc.cleanup();
}

#[shared_test_runtime]
async fn mode_add_twoway_directory_create_and_delete_propagate() {
	// Directory create AND delete directionality in two-way, exercised one-sided via single_client.
	let sc = single_client(SyncMode::TwoWay).await;

	// Local creates ldir/ (with a file so it is unambiguously present); remote creates rdir/.
	write_file(&sc.local, "ldir/inside.txt", b"L");
	let rdir = sc
		.resources
		.client
		.create_dir(&root_dirtype(&sc), "rdir")
		.await
		.unwrap();
	wait_cache_has(&sc, rdir.uuid()).await;

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.conflicts.len(), 0, "{r1:?}");
	// ldir/ propagated up; rdir/ propagated down.
	let (dirs, _files) = list_remote_root(&sc).await;
	assert!(
		find_dir(&dirs, "ldir").is_some(),
		"ldir not pushed: {dirs:?}"
	);
	assert!(sc.local.join("rdir").is_dir(), "rdir not pulled locally");

	// Settle one pass so both sides record the converged dirs in the baseline.
	let r_settle = sc.sync().await;
	assert!(r_settle.errors.is_empty(), "{r_settle:?}");

	// Delete ldir/ locally -> deletion must mirror to the remote in two-way.
	std::fs::remove_dir_all(sc.local.join("ldir")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert!(
		r2.remotely_trashed >= 1,
		"local dir deletion must mirror to remote: {r2:?}"
	);
	let (dirs2, _f2) = list_remote_root(&sc).await;
	assert!(
		find_dir(&dirs2, "ldir").is_none(),
		"remote still has ldir: {dirs2:?}"
	);

	// Second pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.conflicts.len(), 0, "{r3:?}");

	sc.cleanup();
}
