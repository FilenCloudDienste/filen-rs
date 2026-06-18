//! Moves & renames tests (`MOVE-*`) for the two-way sync engine.
//!
//! These verify that structural changes (renames, cross-directory moves, whole-subtree
//! relocations, swaps, move+edit) propagate as cheap metadata operations where possible and
//! NEVER lose data when detection is ambiguous. Live, black-box; each test scopes to its own
//! fresh remote dir via the shared harness.
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::time::Duration;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::fs::categories::{DirType, Normal};
use filen_sdk_rs::fs::dir::RemoteDirectory;
use filen_sdk_rs::fs::dir::meta::DirectoryMetaChanges;
use filen_sdk_rs::fs::file::RemoteFile;
use filen_sdk_rs::fs::file::meta::FileMetaChanges;
use filen_sdk_rs::fs::{HasName, HasUUID};
use filen_sdk_rs::sync_engine::{SyncEvent, SyncMode};
use uuid::Uuid;

use crate::harness::*;
use crate::helpers::*;

// ----------------------------------------------------------------------------
// Local remote-listing / verification helpers (ground truth via the client).
// ----------------------------------------------------------------------------

/// List the (dirs, files) directly under a remote directory via a client (ground truth).
async fn list_dir(
	client: &filen_sdk_rs::auth::Client,
	dir: &RemoteDirectory,
) -> (Vec<RemoteDirectory>, Vec<RemoteFile>) {
	client
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

/// A `DirType` borrowing `resources.dir` (the pair's remote root) for client scenario ops.
fn root_dirtype(sc: &SingleClient) -> DirType<'_, Normal> {
	DirType::<Normal>::Dir(Cow::Borrowed(&sc.resources.dir))
}

/// Upload bytes to the single-client remote root, returning the created file.
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

/// Every relative path in `root`'s tree, sorted (quarantine bin excluded, as `walk_tree` does) —
/// for asserting an exact end state rather than "the bytes are recoverable somewhere".
fn tree_paths(root: &std::path::Path) -> Vec<String> {
	walk_tree(root).into_keys().collect()
}

/// Wait until the engine's cache view observes `uuid`.
async fn wait_cache_has(sc: &SingleClient, uuid: Uuid) {
	assert!(
		poll_for_item(sc.cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

// ============================================================================
// MOVE-01 — In-place file rename propagates as a rename (no re-transfer).
// ============================================================================

#[shared_test_runtime]
async fn move01_inplace_rename_is_metadata_only() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let payload = vec![7u8; 256 * 1024];
	write_file(&sc.local, "a/notes.txt", &payload);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Capture the remote uuid so we can prove identity is preserved across the rename.
	let (dirs0, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let a0 = find_dir(&dirs0, "a").unwrap();
	let (_, a_files0) = list_dir(&sc.resources.client, a0).await;
	let orig_uuid = find_file(&a_files0, "notes.txt").unwrap().uuid();

	move_file(&sc.local, "a/notes.txt", "a/journal.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.moved_remote, 1, "rename must be a remote move: {r2:?}");
	assert_eq!(r2.uploaded, 0, "rename must not re-upload: {r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "rename must not trash: {r2:?}");

	let (dirs1, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let a1 = find_dir(&dirs1, "a").unwrap();
	let (_, a_files1) = list_dir(&sc.resources.client, a1).await;
	assert_eq!(a_files1.len(), 1, "exactly one file under a/: {a_files1:?}");
	let journal = find_file(&a_files1, "journal.txt").expect("journal.txt missing");
	assert!(
		find_file(&a_files1, "notes.txt").is_none(),
		"stale notes.txt"
	);
	assert_eq!(journal.uuid(), orig_uuid, "uuid preserved across rename");
	assert_eq!(journal.size, payload.len() as u64, "size intact");

	// Baseline now records journal.txt: an immediate pass is a no-op.
	let r3 = sc.sync().await;
	assert_eq!(r3.moved_remote, 0, "{r3:?}");
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.remotely_trashed, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-02 — Directory rename propagates as a single move; children preserved.
// ============================================================================

#[shared_test_runtime]
async fn move02_directory_rename_preserves_children() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "docs/f1.txt", b"C1-one");
	write_file(&sc.local, "docs/f2.txt", b"C2-two");
	write_file(&sc.local, "docs/sub/f3.txt", b"C3-three");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	move_file(&sc.local, "docs", "documents");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "children must not re-upload: {r2:?}");

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(find_dir(&dirs, "docs").is_none(), "stale docs/ lingers");
	let documents = find_dir(&dirs, "documents").expect("documents/ missing");
	let (d_dirs, d_files) = list_dir(&sc.resources.client, documents).await;
	assert!(find_file(&d_files, "f1.txt").is_some());
	assert!(find_file(&d_files, "f2.txt").is_some());
	let sub = find_dir(&d_dirs, "sub").expect("documents/sub missing");
	let (_, sub_files) = list_dir(&sc.resources.client, sub).await;
	let f3 = find_file(&sub_files, "f3.txt").expect("f3.txt missing");
	assert_eq!(f3.size, b"C3-three".len() as u64);

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-03 — Cross-directory file move (same name, new parent).
// ============================================================================

#[shared_test_runtime]
async fn move03_cross_directory_move() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "a/x.bin", b"cross-dir-payload");
	std::fs::create_dir_all(sc.local.join("b")).unwrap();
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	let (dirs0, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let a0 = find_dir(&dirs0, "a").unwrap();
	let (_, a_files0) = list_dir(&sc.resources.client, a0).await;
	let orig_uuid = find_file(&a_files0, "x.bin").unwrap().uuid();

	move_file(&sc.local, "a/x.bin", "b/x.bin");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.moved_remote, 1, "cross-dir move: {r2:?}");
	assert_eq!(r2.uploaded, 0, "{r2:?}");

	let (dirs1, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let a1 = find_dir(&dirs1, "a").expect("a/ should remain as empty dir");
	let (_, a_files1) = list_dir(&sc.resources.client, a1).await;
	assert!(find_file(&a_files1, "x.bin").is_none(), "x.bin still in a/");
	let b1 = find_dir(&dirs1, "b").unwrap();
	let (_, b_files1) = list_dir(&sc.resources.client, b1).await;
	let moved = find_file(&b_files1, "x.bin").expect("x.bin missing in b/");
	assert_eq!(moved.uuid(), orig_uuid, "uuid preserved across move");

	sc.cleanup();
}

// ============================================================================
// MOVE-04 — Remote-originated rename pulled down (remote->local).
// ============================================================================

#[shared_test_runtime]
async fn move04_remote_rename_pulled_down() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	let mut rf = upload_root(&sc, "report.pdf", b"PDF-bytes-C1").await;
	wait_cache_has(&sc, rf.uuid().into()).await;

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "report.pdf", b"PDF-bytes-C1"));

	sc.resources
		.client
		.update_file_metadata(
			&mut rf,
			FileMetaChanges::default().name("final.pdf").unwrap(),
		)
		.await
		.unwrap();
	assert!(
		poll_for_file_name(
			sc.cache.db_path(),
			rf.uuid().into(),
			"final.pdf",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the remote rename"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.moved_local, 1, "remote rename is a local move: {r2:?}");
	assert_eq!(r2.downloaded, 0, "must not re-download: {r2:?}");
	assert!(!sc.local.join("report.pdf").exists(), "old name remains");
	assert!(
		read_eq(&sc.local, "final.pdf", b"PDF-bytes-C1"),
		"content lost"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.moved_local, 0, "{r3:?}");
	assert_eq!(r3.downloaded, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-05 — Move + content edit in one pass.
// ============================================================================

#[shared_test_runtime]
async fn move05_move_and_edit_together() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "src/data.csv", b"a,b,c\n1,2,3\n");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Offline: move AND overwrite with new, differently-sized content.
	let c2 = b"x,y\n9,9\n7,7\n5,5\n".to_vec();
	move_file(&sc.local, "src/data.csv", "dest/data.csv");
	std::fs::write(sc.local.join("dest/data.csv"), &c2).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.conflicts.len(),
		0,
		"one-sided change: no conflict: {r2:?}"
	);

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	// src/ may remain as an empty dir, but must not still hold data.csv.
	if let Some(src) = find_dir(&dirs, "src") {
		let (_, src_files) = list_dir(&sc.resources.client, src).await;
		assert!(
			find_file(&src_files, "data.csv").is_none(),
			"old data.csv survives"
		);
	}
	let dest = find_dir(&dirs, "dest").expect("dest/ missing");
	let (_, dest_files) = list_dir(&sc.resources.client, dest).await;
	let f = find_file(&dest_files, "data.csv").expect("dest/data.csv missing");
	assert_eq!(f.size, c2.len() as u64, "final content must be the NEW C2");

	sc.cleanup();
}

// ============================================================================
// MOVE-06 — Move a whole subtree to a new parent.
// ============================================================================

#[shared_test_runtime]
async fn move06_whole_subtree_relocation() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "proj/a/1.txt", b"one");
	write_file(&sc.local, "proj/a/2.txt", b"two");
	write_file(&sc.local, "proj/b/c/3.txt", b"three");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	std::fs::create_dir_all(sc.local.join("archive")).unwrap();
	move_file(&sc.local, "proj", "archive/proj");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "subtree move must not re-upload: {r2:?}");

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(find_dir(&dirs, "proj").is_none(), "top-level proj/ lingers");
	let archive = find_dir(&dirs, "archive").expect("archive/ missing");
	let (arc_dirs, _) = list_dir(&sc.resources.client, archive).await;
	let proj = find_dir(&arc_dirs, "proj").expect("archive/proj missing");
	let (proj_dirs, _) = list_dir(&sc.resources.client, proj).await;
	let a = find_dir(&proj_dirs, "a").expect("archive/proj/a missing");
	let (_, a_files) = list_dir(&sc.resources.client, a).await;
	assert!(find_file(&a_files, "1.txt").is_some());
	assert!(find_file(&a_files, "2.txt").is_some());
	let b = find_dir(&proj_dirs, "b").expect("archive/proj/b missing");
	let (b_dirs, _) = list_dir(&sc.resources.client, b).await;
	let c = find_dir(&b_dirs, "c").expect("archive/proj/b/c missing");
	let (_, c_files) = list_dir(&sc.resources.client, c).await;
	assert!(find_file(&c_files, "3.txt").is_some());

	sc.cleanup();
}

// ============================================================================
// MOVE-07 — Swap two file names.
// ============================================================================

#[shared_test_runtime]
async fn move07_swap_two_file_names() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let c_a = b"AAAA-content-alpha".to_vec();
	let c_b = b"BBBB-content-beta-different-len".to_vec();
	write_file(&sc.local, "alpha.txt", &c_a);
	write_file(&sc.local, "beta.txt", &c_b);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Swap: alpha now holds C_B, beta now holds C_A.
	move_file(&sc.local, "alpha.txt", "tmp.swap");
	move_file(&sc.local, "beta.txt", "alpha.txt");
	move_file(&sc.local, "tmp.swap", "beta.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let alpha = find_file(&files, "alpha.txt").expect("alpha.txt missing");
	let beta = find_file(&files, "beta.txt").expect("beta.txt missing");
	assert!(find_file(&files, "tmp.swap").is_none(), "tmp.swap leaked");
	assert_eq!(alpha.size, c_b.len() as u64, "alpha must now hold C_B");
	assert_eq!(beta.size, c_a.len() as u64, "beta must now hold C_A");
	assert_eq!(
		files.len(),
		2,
		"exactly two files survive the swap: {files:?}"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-08 — Swap a file name with a directory name.
// ============================================================================

#[ignore = "blocked: a cyclic file<->dir NAME SWAP (item->box while box->item) needs temp-name \
staging to break the rename cycle — every target name is occupied by the other item, so the engine's \
create->move->delete phase ordering cannot resolve it and the server rejects the colliding move with \
`cannot_move_this_file`. Correct handling requires detecting the cycle and routing one leg through a \
temporary name; unimplemented. TODO: add cycle-breaking to order_actions/apply"]
#[shared_test_runtime]
async fn move08_swap_file_and_directory_names() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "item", b"FILE-content-C1");
	write_file(&sc.local, "box/inner.txt", b"DIR-child-C2");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Swap: item becomes the directory, box becomes the file.
	move_file(&sc.local, "item", "tmpfile");
	move_file(&sc.local, "box", "item");
	move_file(&sc.local, "tmpfile", "box");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (dirs, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let item_dir = find_dir(&dirs, "item").expect("item must now be a directory");
	assert!(find_file(&files, "item").is_none(), "item left as a file");
	let (_, item_files) = list_dir(&sc.resources.client, item_dir).await;
	let inner = find_file(&item_files, "inner.txt").expect("item/inner.txt missing");
	assert_eq!(inner.size, b"DIR-child-C2".len() as u64);
	let box_file = find_file(&files, "box").expect("box must now be a file");
	assert!(find_dir(&dirs, "box").is_none(), "box left as a dir");
	assert_eq!(box_file.size, b"FILE-content-C1".len() as u64);

	sc.cleanup();
}

// ============================================================================
// MOVE-09 — Move emits NO byte-transfer progress (genuine metadata op).
// ============================================================================

#[shared_test_runtime]
async fn move09_rename_emits_no_transfer_events() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let payload = vec![0xABu8; 512 * 1024];
	write_file(&sc.local, "big.iso", &payload);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	let (_, files0) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let orig_uuid = find_file(&files0, "big.iso").unwrap().uuid();

	move_file(&sc.local, "big.iso", "big-renamed.iso");

	let mut uploads: Vec<String> = Vec::new();
	let mut moves: Vec<(String, String)> = Vec::new();
	let r2 = sc
		.engine
		.sync_once_observed(sc.pair, &mut |ev: SyncEvent| match ev {
			SyncEvent::Uploading { rel_path } => uploads.push(rel_path),
			SyncEvent::MovingRemote { from, to } => moves.push((from, to)),
			_ => {}
		})
		.await
		.expect("sync_once_observed");

	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		uploads.is_empty(),
		"a metadata move must emit NO Uploading events, saw: {uploads:?}"
	);
	assert_eq!(r2.uploaded, 0, "no bytes re-transferred: {r2:?}");
	assert_eq!(
		r2.moved_remote, 1,
		"expected exactly one remote move: {r2:?}"
	);
	assert_eq!(moves.len(), 1, "expected one MovingRemote event: {moves:?}");

	let (_, files1) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let renamed = find_file(&files1, "big-renamed.iso").expect("renamed file missing");
	assert_eq!(
		renamed.uuid(),
		orig_uuid,
		"object identity preserved (no fresh upload)"
	);
	assert_eq!(renamed.size, payload.len() as u64, "content size intact");

	sc.cleanup();
}

// ============================================================================
// MOVE-10 — Ambiguous duplicate-content move falls back safely (no loss).
// ============================================================================

#[shared_test_runtime]
async fn move10_duplicate_content_move_converges() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	let dup = b"identical-bytes-everywhere".to_vec();
	write_file(&sc.local, "dup1.txt", &dup);
	write_file(&sc.local, "dup2.txt", &dup);
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Delete dup1, create dup3 with the same content: a content matcher cannot tell which.
	std::fs::remove_file(sc.local.join("dup1.txt")).unwrap();
	write_file(&sc.local, "dup3.txt", &dup);
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "dup1.txt").is_none(),
		"dup1.txt should be gone"
	);
	let f2 = find_file(&files, "dup2.txt").expect("dup2.txt missing");
	let f3 = find_file(&files, "dup3.txt").expect("dup3.txt missing");
	assert_eq!(f2.size, dup.len() as u64);
	assert_eq!(f3.size, dup.len() as u64);
	assert_eq!(
		files.len(),
		2,
		"exactly {{dup2,dup3}} must survive: {files:?}"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-11 — Move INTO a newly created directory.
// ============================================================================

#[shared_test_runtime]
async fn move11_move_into_new_directory() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "loose.txt", b"loose-payload-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Offline: create newdir/ AND move loose.txt into it.
	move_file(&sc.local, "loose.txt", "newdir/loose.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "no parent-missing failure: {r2:?}");

	let (dirs, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "loose.txt").is_none(),
		"root loose.txt remains"
	);
	let newdir = find_dir(&dirs, "newdir").expect("newdir/ missing");
	let (_, nd_files) = list_dir(&sc.resources.client, newdir).await;
	let f = find_file(&nd_files, "loose.txt").expect("newdir/loose.txt missing");
	assert_eq!(f.size, b"loose-payload-C1".len() as u64);

	sc.cleanup();
}

// ============================================================================
// MOVE-12 — Move OUT of a directory that is then deleted (rescue-before-delete).
// ============================================================================

#[shared_test_runtime]
async fn move12_rescue_before_directory_delete() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "cage/keep.txt", b"rescued-C1");
	write_file(&sc.local, "cage/junk.txt", b"doomed-C2");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Move keep.txt out, then delete the whole cage/ (junk.txt included).
	move_file(&sc.local, "cage/keep.txt", "keep.txt");
	std::fs::remove_dir_all(sc.local.join("cage")).unwrap();
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (dirs, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let keep = find_file(&files, "keep.txt").expect("rescued keep.txt missing");
	assert_eq!(
		keep.size,
		b"rescued-C1".len() as u64,
		"rescued content lost"
	);
	// cage/ and junk.txt must be gone (delete mirrored in LocalToRemote).
	if let Some(cage) = find_dir(&dirs, "cage") {
		let (_, cage_files) = list_dir(&sc.resources.client, cage).await;
		assert!(
			find_file(&cage_files, "junk.txt").is_none(),
			"junk.txt survives"
		);
	}

	sc.cleanup();
}

// ============================================================================
// MOVE-13 — Case-only rename is propagated.
// ============================================================================

#[shared_test_runtime]
async fn move13_case_only_rename() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "Readme.md", b"case-only-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Case-only rename. On case-insensitive FS this is a same-inode rename; still a rename.
	move_file(&sc.local, "Readme.md", "README.md");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let readme = find_file(&files, "README.md").expect("README.md (new casing) missing");
	assert_eq!(readme.size, b"case-only-C1".len() as u64);
	// No duplicate pair: exactly one file with this name (case-insensitive).
	let matching = files
		.iter()
		.filter(|f| {
			f.name()
				.is_some_and(|n| n.eq_ignore_ascii_case("readme.md"))
		})
		.count();
	assert_eq!(matching, 1, "exactly one readme file must exist: {files:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-14 — Two-way divergent move (both sides moved the same item differently).
// ============================================================================

#[shared_test_runtime]
async fn move14_twoway_divergent_move_conflicts() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = BTreeSet::new();

	// Seed the shared file and converge so both baselines record m.txt.
	write_file(&tc.local_a, "m.txt", b"shared-C1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"move14-baseline",
		|| read_eq(&tc.local_b, "m.txt", b"shared-C1") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline must be clean: {conflicts:?}"
	);

	// Diverge: A moves m.txt -> dirA/m.txt, B moves m.txt -> dirB/m.txt.
	move_file(&tc.local_a, "m.txt", "dirA/m.txt");
	move_file(&tc.local_b, "m.txt", "dirB/m.txt");

	// Run several rounds; a both-sides-diverged move must surface a conflict and not loop/crash.
	for _ in 0..12 {
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// Content C1 must survive somewhere (one of the two destinations, on either side).
	let survives = read_eq(&tc.local_a, "dirA/m.txt", b"shared-C1")
		|| read_eq(&tc.local_b, "dirB/m.txt", b"shared-C1")
		|| read_eq(&tc.local_a, "dirB/m.txt", b"shared-C1")
		|| read_eq(&tc.local_b, "dirA/m.txt", b"shared-C1");
	assert!(
		survives,
		"content C1 must be recoverable after the divergent move"
	);
	let surfaced = conflicts.iter().any(|c| c.contains("m.txt"));
	assert!(
		surfaced || trees_equal(&tc.local_a, &tc.local_b),
		"divergent move must surface a conflict or converge nondestructively: {conflicts:?}"
	);

	tc.cleanup();
}

// ============================================================================
// MOVE-15 — Two-way move-vs-edit (move on one side, in-place edit on the other).
// ============================================================================

#[shared_test_runtime]
async fn move15_twoway_move_vs_edit() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = BTreeSet::new();

	write_file(&tc.local_a, "e.txt", b"shared-C1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"move15-baseline",
		|| read_eq(&tc.local_b, "e.txt", b"shared-C1") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline must be clean: {conflicts:?}"
	);

	// A moves e.txt -> sub/e.txt; B edits e.txt in place to C2.
	move_file(&tc.local_a, "e.txt", "sub/e.txt");
	write_file(&tc.local_b, "e.txt", b"edited-C2-longer-bytes");

	for _ in 0..12 {
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The edited content C2 must NOT be lost: it must exist somewhere on some side.
	let c2 = b"edited-C2-longer-bytes";
	let c2_survives = read_eq(&tc.local_a, "e.txt", c2)
		|| read_eq(&tc.local_b, "e.txt", c2)
		|| read_eq(&tc.local_a, "sub/e.txt", c2)
		|| read_eq(&tc.local_b, "sub/e.txt", c2);
	assert!(
		c2_survives,
		"the edit C2 must survive reconciliation (no lost edit)"
	);

	tc.cleanup();
}

// ============================================================================
// MOVE-16 — Backup modes do NOT mirror the delete-half of a move.
// ============================================================================

#[shared_test_runtime]
async fn move16_local_backup_does_not_mirror_move_delete() {
	let sc = single_client(SyncMode::LocalBackup).await;
	write_file(&sc.local, "mv.txt", b"backup-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Rename locally; LocalBackup must push the new name but NOT trash the old.
	move_file(&sc.local, "mv.txt", "moved.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.remotely_trashed, 0,
		"backup must not trash on a move: {r2:?}"
	);

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "moved.txt").is_some(),
		"new name not pushed"
	);
	assert!(
		find_file(&files, "mv.txt").is_some(),
		"backup must RETAIN the original name (additive): {files:?}"
	);

	sc.cleanup();
}

#[shared_test_runtime]
async fn move16_remote_backup_does_not_mirror_move_delete() {
	let sc = single_client(SyncMode::RemoteBackup).await;
	let mut rf = upload_root(&sc, "mv.txt", b"backup-C1").await;
	wait_cache_has(&sc, rf.uuid().into()).await;
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(read_eq(&sc.local, "mv.txt", b"backup-C1"));

	// Rename on the remote; RemoteBackup pulls the new name but must keep the old local copy.
	sc.resources
		.client
		.update_file_metadata(
			&mut rf,
			FileMetaChanges::default().name("moved.txt").unwrap(),
		)
		.await
		.unwrap();
	assert!(
		poll_for_file_name(
			sc.cache.db_path(),
			rf.uuid().into(),
			"moved.txt",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the remote rename"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.locally_deleted, 0,
		"backup must not delete locally on a move: {r2:?}"
	);
	assert!(
		read_eq(&sc.local, "moved.txt", b"backup-C1"),
		"new name not pulled"
	);
	assert!(
		sc.local.join("mv.txt").exists(),
		"remote-backup must RETAIN the pre-move local name (additive)"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-17 — Crash mid-move, restart, idempotent re-plan. BLOCKED (fault injection).
// ============================================================================

#[ignore = "blocked: needs fault-injection harness (deterministic mid-pass crash between dest-create \
            and source-delete / baseline-advance) — see TODO"]
#[shared_test_runtime]
async fn move17_interrupted_move_is_idempotent() {
	// plan: sync k/file.dat (C1); move to k2/file.dat; kill the pass after the destination is
	// created but before the source delete/baseline advance; restart engine and sync; assert remote
	// has exactly k2/file.dat (C1), no permanent k/file.dat duplicate, baseline consistent, final
	// pass is a no-op. Requires a controllable crash seam the current harness does not expose.
}

// ============================================================================
// MOVE-18 — Move under watch mode does not loop.
// ============================================================================

#[shared_test_runtime]
async fn move18_move_under_watch_does_not_loop() {
	use std::sync::Arc;
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "w/a.txt", b"watched-C1");

	// Establish the baseline non-watched, then start watching.
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	let engine = Arc::new(
		filen_sdk_rs::sync_engine::SyncEngine::open(sc.cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(sc.local.clone(), sc.remote, SyncMode::LocalToRemote)
		.await
		.unwrap();
	// Prime this engine's baseline (it has its own baseline DB) so the move is detected, not a
	// fresh first-sync that re-uploads.
	let _ = engine.sync_once(pair).await.unwrap();

	let handle = engine.clone().watch(pair).await.unwrap();

	// Move under the active watch.
	move_file(&sc.local, "w/a.txt", "w/b.txt");

	// Wait until the remote reflects b.txt and a.txt is gone.
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	let mut ok = false;
	while std::time::Instant::now() < deadline {
		let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
		if let Some(w) = find_dir(&dirs, "w") {
			let (_, w_files) = list_dir(&sc.resources.client, w).await;
			if find_file(&w_files, "b.txt").is_some() && find_file(&w_files, "a.txt").is_none() {
				ok = true;
				break;
			}
		}
		tokio::time::sleep(Duration::from_millis(400)).await;
	}
	assert!(
		ok,
		"watch did not apply the move (b.txt present, a.txt gone)"
	);

	// Let several debounce/safety-net intervals pass; the engine's own write must not oscillate.
	tokio::time::sleep(Duration::from_secs(8)).await;
	drop(handle);

	// Steady state: exactly one file b.txt with C1, nothing else churned.
	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let w = find_dir(&dirs, "w").expect("w/ missing");
	let (_, w_files) = list_dir(&sc.resources.client, w).await;
	assert_eq!(
		w_files.len(),
		1,
		"exactly one file should remain: {w_files:?}"
	);
	let b = find_file(&w_files, "b.txt").expect("b.txt missing");
	assert_eq!(b.size, b"watched-C1".len() as u64);

	sc.cleanup();
}

// ============================================================================
// MOVE-19 — Move onto an occupied destination (overwrite or conflict, never loss).
// ============================================================================

#[shared_test_runtime]
async fn move19_move_onto_occupied_destination() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "from.txt", b"FROM-content-C1");
	write_file(&sc.local, "occupied.txt", b"OCCUPIED-content-OTHER");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Rename from.txt onto occupied.txt (std::fs::rename overwrites the destination).
	move_file(&sc.local, "from.txt", "occupied.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "from.txt").is_none(),
		"from.txt should be gone"
	);
	let occ = find_file(&files, "occupied.txt").expect("occupied.txt missing");
	// Local overwrite semantics: occupied.txt now holds C1's bytes (or a conflict is surfaced).
	let overwritten = occ.size == b"FROM-content-C1".len() as u64;
	assert!(
		overwritten || !r2.conflicts.is_empty(),
		"destination must reflect the overwrite (C1) or surface a conflict: {r2:?}, size={}",
		occ.size
	);
	// No orphaned duplicate: exactly one occupied.txt.
	assert_eq!(
		files
			.iter()
			.filter(|f| f.name() == Some("occupied.txt"))
			.count(),
		1,
		"no duplicate occupied.txt: {files:?}"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-20 — Large subtree relocation does not trip the mass-delete guard.
// ============================================================================

#[shared_test_runtime]
async fn move20_subtree_relocation_no_false_mass_delete_trip() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const N: usize = 60;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("bulk/d{}/f{i:03}.txt", i % 5),
			format!("c{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Relocate the whole subtree: naively this looks like N deletes + N creates.
	std::fs::create_dir_all(sc.local.join("relocated")).unwrap();
	move_file(&sc.local, "bulk", "relocated/bulk");

	// May take a couple of passes to fully apply; never error, never a storm of uploads.
	let mut total_uploaded = 0usize;
	let mut total_trashed = 0usize;
	let mut total_held = 0usize;
	for _ in 0..6 {
		let r = sc.sync().await;
		assert!(r.errors.is_empty(), "{r:?}");
		total_uploaded += r.uploaded;
		total_trashed += r.remotely_trashed;
		total_held += r.held_deletions;
		let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
		if find_dir(&dirs, "bulk").is_none() && find_dir(&dirs, "relocated").is_some() {
			break;
		}
	}
	assert!(
		total_uploaded < N,
		"a subtree move must not re-upload all {N} files (uploaded {total_uploaded})"
	);
	// The relocation re-parents every file (moves), then trashes the emptied source directories
	// (bulk/ plus bulk/d0..d4 = 6). No FILE data is trashed — the end-state check below proves all
	// N files survive under relocated/bulk. Only emptied container dirs are cleaned up.
	assert!(
		total_trashed <= 6,
		"only emptied source dirs may be cleaned up, never file data (trashed {total_trashed})"
	);
	assert_eq!(
		total_held, 0,
		"mass-delete guard must not trip on a relocation"
	);

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(find_dir(&dirs, "bulk").is_none(), "old bulk/ lingers");
	let relocated = find_dir(&dirs, "relocated").expect("relocated/ missing");
	let (rel_dirs, _) = list_dir(&sc.resources.client, relocated).await;
	let bulk = find_dir(&rel_dirs, "bulk").expect("relocated/bulk missing");
	let (bulk_dirs, _) = list_dir(&sc.resources.client, bulk).await;
	let mut found = 0usize;
	for d in &bulk_dirs {
		let (_, fs) = list_dir(&sc.resources.client, d).await;
		found += fs.len();
	}
	assert_eq!(
		found, N,
		"all {N} files must be under relocated/bulk; found {found}"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-21 — Reuse a just-vacated name in one pass (delete one.txt, rename two.txt->one.txt).
// ============================================================================

#[shared_test_runtime]
async fn move21_reuse_freed_name_in_one_pass() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "one.txt", b"ONE-C1");
	write_file(&sc.local, "two.txt", b"TWO-C2-different");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Offline: delete one.txt, then rename two.txt -> one.txt (one.txt now holds C2).
	std::fs::remove_file(sc.local.join("one.txt")).unwrap();
	move_file(&sc.local, "two.txt", "one.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "two.txt").is_none(),
		"two.txt should be gone"
	);
	let one = find_file(&files, "one.txt").expect("one.txt missing");
	assert_eq!(
		one.size,
		b"TWO-C2-different".len() as u64,
		"one.txt must now hold C2, not stale C1"
	);
	assert_eq!(
		files.iter().filter(|f| f.name() == Some("one.txt")).count(),
		1,
		"exactly one one.txt: {files:?}"
	);

	sc.cleanup();
}

// ============================================================================
// MOVE-22 — Move then move-back (net no-op).
// ============================================================================

#[shared_test_runtime]
async fn move22_move_then_move_back_is_noop() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "A/p.txt", b"net-noop-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Offline: move A/p.txt -> B/p.txt then back to A/p.txt.
	move_file(&sc.local, "A/p.txt", "B/p.txt");
	move_file(&sc.local, "B/p.txt", "A/p.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(r2.uploaded, 0, "net no-op must not re-upload: {r2:?}");
	assert_eq!(r2.moved_remote, 0, "net no-op must not move: {r2:?}");

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let a = find_dir(&dirs, "A").expect("A/ missing");
	let (_, a_files) = list_dir(&sc.resources.client, a).await;
	assert!(find_file(&a_files, "p.txt").is_some(), "A/p.txt missing");
	// B/ should hold no p.txt (the dir may exist if it was synced, but no phantom file).
	if let Some(b) = find_dir(&dirs, "B") {
		let (_, b_files) = list_dir(&sc.resources.client, b).await;
		assert!(find_file(&b_files, "p.txt").is_none(), "phantom B/p.txt");
	}

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-23 — Empty directory rename is propagated.
// ============================================================================

#[shared_test_runtime]
async fn move23_empty_directory_rename() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	std::fs::create_dir_all(sc.local.join("emptyA")).unwrap();
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.remote_dirs_created, 1, "{r1:?}");

	move_file(&sc.local, "emptyA", "emptyB");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 0,
		"no file ops for an empty-dir rename: {r2:?}"
	);

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(find_dir(&dirs, "emptyA").is_none(), "stale emptyA/ lingers");
	assert!(find_dir(&dirs, "emptyB").is_some(), "emptyB/ missing");

	let r3 = sc.sync().await;
	assert_eq!(r3.remote_dirs_created, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-24 — Cross-pair move: a file moved out of one sync root into another.
// ============================================================================

#[shared_test_runtime]
async fn move24_cross_pair_move() {
	// Build two sibling remote roots under the test's scoped dir, both covered by the same cache.
	let sc = single_client(SyncMode::LocalToRemote).await;
	let r1_dir = sc
		.resources
		.client
		.create_dir(&root_dirtype(&sc), "R1")
		.await
		.unwrap();
	let r2_dir = sc
		.resources
		.client
		.create_dir(&root_dirtype(&sc), "R2")
		.await
		.unwrap();
	wait_cache_has(&sc, r1_dir.uuid().into()).await;
	wait_cache_has(&sc, r2_dir.uuid().into()).await;

	let l1 = fresh_local_dir("p1");
	let l2 = fresh_local_dir("p2");
	let p1 = sc
		.engine
		.add_pair(l1.clone(), r1_dir.uuid().into(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let p2 = sc
		.engine
		.add_pair(l2.clone(), r2_dir.uuid().into(), SyncMode::LocalToRemote)
		.await
		.unwrap();

	// g.txt starts in L1; sync both pairs to baseline.
	write_file(&l1, "g.txt", b"cross-pair-C1");
	let a1 = sc.engine.sync_once(p1).await.unwrap();
	assert!(a1.errors.is_empty(), "{a1:?}");
	assert_eq!(a1.uploaded, 1, "{a1:?}");
	let b1 = sc.engine.sync_once(p2).await.unwrap();
	assert!(b1.errors.is_empty(), "{b1:?}");

	// Move g.txt OUT of L1 and INTO L2 (filesystem move across sync roots).
	std::fs::rename(l1.join("g.txt"), l2.join("g.txt")).unwrap();

	let a2 = sc.engine.sync_once(p1).await.unwrap();
	assert!(a2.errors.is_empty(), "{a2:?}");
	let b2 = sc.engine.sync_once(p2).await.unwrap();
	assert!(b2.errors.is_empty(), "{b2:?}");
	assert_eq!(b2.uploaded, 1, "P2 should upload g.txt: {b2:?}");

	// P1's remote (R1) must no longer have g.txt.
	let (_, r1_files) = list_dir(&sc.resources.client, &r1_dir).await;
	assert!(find_file(&r1_files, "g.txt").is_none(), "g.txt still in R1");
	// P2's remote (R2) must now have g.txt with C1.
	let (_, r2_files) = list_dir(&sc.resources.client, &r2_dir).await;
	let g = find_file(&r2_files, "g.txt").expect("g.txt missing in R2");
	assert_eq!(g.size, b"cross-pair-C1".len() as u64);

	std::fs::remove_dir_all(&l1).ok();
	std::fs::remove_dir_all(&l2).ok();
	sc.cleanup();
}

// ============================================================================
// MOVE-25 — Genuine mass move-out (delete within pair) trips the mass-delete guard.
// ============================================================================

#[shared_test_runtime]
async fn move25_mass_move_out_trips_guard() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	const N: usize = 30;
	for i in 0..N {
		write_file(
			&sc.local,
			&format!("watched/f{i:03}.txt",),
			format!("c{i}").as_bytes(),
		);
	}
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, N, "{r1:?}");

	// Move ALL files OUT of the sync root entirely (to an external temp dir). Within the pair this
	// is N genuine deletions, so the mass-delete guard must hold.
	let external = fresh_local_dir("external");
	for i in 0..N {
		let name = format!("f{i:03}.txt");
		std::fs::rename(sc.local.join("watched").join(&name), external.join(&name)).unwrap();
	}
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert!(
		r2.held_deletions > 0 || r2.guard_message.is_some(),
		"a genuine mass move-out (within-pair deletions) must trip the guard: {r2:?}"
	);
	assert_eq!(
		r2.remotely_trashed, 0,
		"guard must hold the destructive trashing: {r2:?}"
	);

	// Remote must NOT be wiped until confirmation.
	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let watched = find_dir(&dirs, "watched").expect("watched/ missing");
	let (_, w_files) = list_dir(&sc.resources.client, watched).await;
	assert_eq!(
		w_files.len(),
		N,
		"guard must retain all {N} remote files: {}",
		w_files.len()
	);

	std::fs::remove_dir_all(&external).ok();
	sc.cleanup();
}

// ============================================================================
// MOVE-A1 (review) — Remote-originated directory rename pulled down.
// ============================================================================

#[shared_test_runtime]
async fn move_a1_remote_directory_rename_pulled_down() {
	let sc = single_client(SyncMode::RemoteToLocal).await;
	// Build remote docs/ with children via the client.
	let mut docs = sc
		.resources
		.client
		.create_dir(&root_dirtype(&sc), "docs")
		.await
		.unwrap();
	let f1b = sc
		.resources
		.client
		.make_file_builder("f1.txt", docs.uuid())
		.unwrap();
	let f1 = sc.resources.client.upload_file(f1b, b"C1").await.unwrap();
	let f2b = sc
		.resources
		.client
		.make_file_builder("f2.txt", docs.uuid())
		.unwrap();
	let f2 = sc.resources.client.upload_file(f2b, b"C2").await.unwrap();
	let sub = sc
		.resources
		.client
		.create_dir(&DirType::<Normal>::Dir(Cow::Borrowed(&docs)), "sub")
		.await
		.unwrap();
	let f3b = sc
		.resources
		.client
		.make_file_builder("f3.txt", sub.uuid())
		.unwrap();
	let f3 = sc.resources.client.upload_file(f3b, b"C3").await.unwrap();
	wait_cache_has(&sc, f1.uuid().into()).await;
	wait_cache_has(&sc, f2.uuid().into()).await;
	wait_cache_has(&sc, f3.uuid().into()).await;

	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.downloaded, 3, "{r1:?}");
	assert!(read_eq(&sc.local, "docs/f1.txt", b"C1"));
	assert!(read_eq(&sc.local, "docs/sub/f3.txt", b"C3"));

	// Rename the remote dir docs -> documents.
	sc.resources
		.client
		.update_dir_metadata(
			&mut docs,
			DirectoryMetaChanges::default().name("documents").unwrap(),
		)
		.await
		.unwrap();
	assert!(
		poll_for_dir_name(
			sc.cache.db_path(),
			docs.uuid().into(),
			"documents",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the remote dir rename"
	);

	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.downloaded, 0,
		"children must NOT be re-downloaded: {r2:?}"
	);
	assert!(
		!sc.local.join("docs").exists(),
		"stale docs/ lingers locally"
	);
	assert!(read_eq(&sc.local, "documents/f1.txt", b"C1"));
	assert!(read_eq(&sc.local, "documents/f2.txt", b"C2"));
	assert!(read_eq(&sc.local, "documents/sub/f3.txt", b"C3"));

	let r3 = sc.sync().await;
	assert_eq!(r3.downloaded, 0, "{r3:?}");
	assert_eq!(r3.moved_local, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-A2 (review) — Two-way move-vs-delete divergence.
// ============================================================================

#[shared_test_runtime]
async fn move_a2_twoway_move_vs_delete() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = BTreeSet::new();

	write_file(&tc.local_a, "d.txt", b"shared-C1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"move_a2-baseline",
		|| read_eq(&tc.local_b, "d.txt", b"shared-C1") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline must be clean: {conflicts:?}"
	);

	// A moves d.txt -> sub/d.txt; B deletes d.txt.
	move_file(&tc.local_a, "d.txt", "sub/d.txt");
	std::fs::remove_file(tc.local_b.join("d.txt")).unwrap();

	// B's delete goes first in every round, so B decides it against the pre-move remote state (the
	// divergence under test) and A always meets an already-trashed source. Racing the two passes
	// instead would make the outcome a coin flip between "the move wins" and "the delete wins".
	// The one expected end state: the relocated copy at its destination on BOTH sides and nothing
	// else in either tree — source path gone, no conflict copy, no quarantined leftover.
	let settled = || {
		read_eq(&tc.local_a, "sub/d.txt", b"shared-C1")
			&& read_eq(&tc.local_b, "sub/d.txt", b"shared-C1")
			&& tree_paths(&tc.local_a) == ["sub", "sub/d.txt"]
			&& tree_paths(&tc.local_b) == ["sub", "sub/d.txt"]
	};
	let mut converged = false;
	for _ in 0..12 {
		let (ra, rb) = sync_round(
			&tc.engine_a,
			tc.pair_a,
			&tc.engine_b,
			tc.pair_b,
			Order::BFirst,
		)
		.await;
		// Only A moves anything, and its re-parent can lose the race to B's delete: the server
		// rejects a move of the just-trashed source with `cannot_move_this_file`. A recovers on a
		// later pass by re-uploading the relocated copy fresh (move detection no longer fires once
		// the remote source is gone). Tolerate ONLY that, only on A — B performs no move, so any
		// error B reports is real.
		assert!(
			ra.errors
				.iter()
				.all(|e| e.contains("cannot_move_this_file")),
			"A errors: {:?}",
			ra.errors
		);
		assert!(rb.errors.is_empty(), "B errors: {:?}", rb.errors);
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
		}
		if settled() {
			converged = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	assert!(
		converged,
		"move-vs-delete never settled — A {:?} / B {:?}",
		tree_paths(&tc.local_a),
		tree_paths(&tc.local_b)
	);

	// One more round must be completely clean on BOTH engines and leave that state untouched: the
	// move wins over the concurrent delete, the relocated copy lives at its DESTINATION on both
	// sides (not in the quarantine bin), and the source path is gone everywhere.
	let (ra, rb) = sync_round(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::BFirst,
	)
	.await;
	assert!(
		ra.errors.is_empty(),
		"settled pass, A errors: {:?}",
		ra.errors
	);
	assert!(
		rb.errors.is_empty(),
		"settled pass, B errors: {:?}",
		rb.errors
	);
	assert!(
		settled(),
		"the settled state did not hold — A {:?} / B {:?}",
		tree_paths(&tc.local_a),
		tree_paths(&tc.local_b)
	);
	assert!(
		!tc.local_a.join(".filen-sync-trash").exists(),
		"nothing should have been quarantined on A"
	);
	assert!(
		!tc.local_b.join(".filen-sync-trash").exists(),
		"nothing should have been quarantined on B"
	);
	let (_dirs, rfiles) = list_dir(&tc.resources.client, &tc.resources.dir).await;
	assert!(
		find_file(&rfiles, "d.txt").is_none(),
		"the moved-away source lingers on the remote: {rfiles:?}"
	);

	tc.cleanup();
}

// ============================================================================
// MOVE-A3 (review) — Two-way divergent DIRECTORY rename.
// ============================================================================

#[shared_test_runtime]
async fn move_a3_twoway_divergent_directory_rename() {
	let tc = two_clients(SyncMode::TwoWay).await;
	let mut conflicts: BTreeSet<String> = BTreeSet::new();

	write_file(&tc.local_a, "d/c.txt", b"child-C1");
	converge(
		&tc.engine_a,
		tc.pair_a,
		&tc.engine_b,
		tc.pair_b,
		Order::AFirst,
		&mut conflicts,
		"move_a3-baseline",
		|| read_eq(&tc.local_b, "d/c.txt", b"child-C1") && trees_equal(&tc.local_a, &tc.local_b),
	)
	.await;
	assert!(
		conflicts.is_empty(),
		"baseline must be clean: {conflicts:?}"
	);

	// A renames d/ -> dlocal/; B renames d/ -> dremote/.
	move_file(&tc.local_a, "d", "dlocal");
	move_file(&tc.local_b, "d", "dremote");

	for _ in 0..14 {
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
		for c in ra.conflicts.iter().chain(rb.conflicts.iter()) {
			conflicts.insert(c.clone());
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}

	// The child content must survive under SOME directory name on some side; no data loss.
	let child_survives = read_eq(&tc.local_a, "dlocal/c.txt", b"child-C1")
		|| read_eq(&tc.local_a, "dremote/c.txt", b"child-C1")
		|| read_eq(&tc.local_b, "dlocal/c.txt", b"child-C1")
		|| read_eq(&tc.local_b, "dremote/c.txt", b"child-C1")
		|| read_eq(&tc.local_a, "d/c.txt", b"child-C1")
		|| read_eq(&tc.local_b, "d/c.txt", b"child-C1");
	assert!(
		child_survives,
		"a divergent directory rename must not lose the child content"
	);

	tc.cleanup();
}

// ============================================================================
// MOVE-A4 (review) — Zero-byte rename detected as a move (not ambiguous delete+create).
// ============================================================================

#[shared_test_runtime]
async fn move_a4_zero_byte_rename() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "empty1.txt", b"");
	write_file(&sc.local, "nonempty.txt", b"content-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	move_file(&sc.local, "empty1.txt", "empty2.txt");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");

	let (_, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(
		find_file(&files, "empty1.txt").is_none(),
		"empty1.txt lingers"
	);
	let empty2 = find_file(&files, "empty2.txt").expect("empty2.txt missing");
	assert_eq!(empty2.size, 0, "empty2.txt must be 0 bytes");
	assert!(find_file(&files, "nonempty.txt").is_some());
	assert_eq!(
		files
			.iter()
			.filter(|f| f.name() == Some("empty2.txt"))
			.count(),
		1,
		"exactly one empty2.txt: {files:?}"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-A5 (review) — Partial subtree move (some children moved, others stay).
// ============================================================================

#[shared_test_runtime]
async fn move_a5_partial_subtree_move() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "proj/a/1.txt", b"C1");
	write_file(&sc.local, "proj/a/2.txt", b"C2");
	write_file(&sc.local, "proj/b/3.txt", b"C3");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 3, "{r1:?}");

	// Move only proj/a/ -> proj/moved-a/, leaving proj/b/ in place.
	move_file(&sc.local, "proj/a", "proj/moved-a");
	let r2 = sc.sync().await;
	assert!(r2.errors.is_empty(), "{r2:?}");
	assert_eq!(
		r2.uploaded, 0,
		"no child re-upload for a subtree move: {r2:?}"
	);

	let (dirs, _) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	let proj = find_dir(&dirs, "proj").expect("proj/ missing");
	let (proj_dirs, _) = list_dir(&sc.resources.client, proj).await;
	assert!(find_dir(&proj_dirs, "a").is_none(), "stale proj/a lingers");
	let moved_a = find_dir(&proj_dirs, "moved-a").expect("proj/moved-a missing");
	let (_, ma_files) = list_dir(&sc.resources.client, moved_a).await;
	assert!(find_file(&ma_files, "1.txt").is_some());
	assert!(find_file(&ma_files, "2.txt").is_some());
	let b = find_dir(&proj_dirs, "b").expect("proj/b untouched dir missing");
	let (_, b_files) = list_dir(&sc.resources.client, b).await;
	assert!(
		find_file(&b_files, "3.txt").is_some(),
		"proj/b/3.txt disturbed"
	);

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}

// ============================================================================
// MOVE-A6 (review) — Move into a destination dir that is itself simultaneously renamed.
// ============================================================================

#[shared_test_runtime]
async fn move_a6_move_into_simultaneously_renamed_dir() {
	let sc = single_client(SyncMode::LocalToRemote).await;
	write_file(&sc.local, "olddir/seed.txt", b"seed-content");
	write_file(&sc.local, "loose.txt", b"loose-C1");
	let r1 = sc.sync().await;
	assert!(r1.errors.is_empty(), "{r1:?}");
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Offline: rename olddir -> newdir AND move loose.txt into newdir/.
	move_file(&sc.local, "olddir", "newdir");
	move_file(&sc.local, "loose.txt", "newdir/loose.txt");
	let r2 = sc.sync().await;
	assert!(
		r2.errors.is_empty(),
		"no parent-missing / race failure: {r2:?}"
	);

	let (dirs, files) = list_dir(&sc.resources.client, &sc.resources.dir).await;
	assert!(find_dir(&dirs, "olddir").is_none(), "stale olddir/ lingers");
	assert!(
		find_file(&files, "loose.txt").is_none(),
		"root loose.txt remains"
	);
	let newdir = find_dir(&dirs, "newdir").expect("newdir/ missing");
	let (_, nd_files) = list_dir(&sc.resources.client, newdir).await;
	assert!(
		find_file(&nd_files, "seed.txt").is_some(),
		"newdir/seed.txt missing"
	);
	let loose = find_file(&nd_files, "loose.txt").expect("newdir/loose.txt missing");
	assert_eq!(loose.size, b"loose-C1".len() as u64);

	let r3 = sc.sync().await;
	assert_eq!(r3.uploaded, 0, "{r3:?}");
	assert_eq!(r3.moved_remote, 0, "{r3:?}");

	sc.cleanup();
}
