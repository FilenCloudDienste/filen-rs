//! Black-box end-to-end tests for the sync engine, authored against the documented PUBLIC API
//! only (`SyncEngine`, `SyncMode`, `SyncReport`, `WatchHandle`) — no knowledge of the engine's
//! internals. Lives in the separate integration-test crate, which can only reach `pub` items.
//!
//! Run: `cargo test -p filen-sdk-rs --features sync-engine-live-tests --test sync_engine_blackbox_tests`
//! (needs `.env` with TEST_EMAIL / TEST_PASSWORD). `remote_case_collision_held_back` and the tests
//! of a remote file not matching its hash additionally need the `malformed` feature to construct
//! their preconditions: run the suite with `-F sync-engine-live-tests,malformed` to include them.

use std::{borrow::Cow, path::PathBuf, sync::Arc, time::Duration};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		categories::{DirType, Normal},
		dir::RemoteDirectory,
		file::meta::FileMetaChanges,
	},
	sync_engine::{SyncEngine, SyncMode},
};
// Only the hash-mismatch tests, which need the `malformed` seam, read these.
#[cfg(feature = "malformed")]
use filen_sdk_rs::fs::file::traits::HasRemoteFileInfo;
#[cfg(feature = "malformed")]
use filen_sdk_rs::sync_engine::{
	ConflictResolution, PlannedAction, PlannedActionKind, SyncEvent, SyncReport, UnsyncablePath,
	UnsyncableReason,
};
use uuid::Uuid;

mod helpers;
use helpers::*;

// ----------------------------------------------------------------------------
// Shared scaffolding
// ----------------------------------------------------------------------------

/// A scoped fixture: a fresh remote subfolder (auto-cleaned via `TestResources`' Drop), a derived
/// private cache pinned to that subfolder, and a fresh local temp dir. `cleanup()` removes the
/// local dir; the remote dir is reclaimed when `resources` drops.
struct Fixture {
	client: Arc<Client>,
	cache: TestCache,
	resources: test_utils::TestResources,
	remote_uuid: Uuid,
	local: PathBuf,
}

impl Fixture {
	async fn new() -> Self {
		let resources = test_utils::RESOURCES.get_resources().await;
		let remote_uuid: Uuid = resources.dir.uuid();
		let cache = TestCache::new(&resources.client, remote_uuid).await;
		let local = std::env::temp_dir().join(format!("e2e_{}", Uuid::new_v4()));
		std::fs::create_dir_all(&local).unwrap();
		Fixture {
			client: resources.client.clone(),
			cache,
			remote_uuid,
			local,
			resources,
		}
	}

	fn db_path(&self) -> &std::path::Path {
		self.cache.db_path()
	}

	/// Wait until the cache has a converged listing of the remote root. Uses `since = 0` so the
	/// initial convergence that `TestCache::new` itself triggers (which may already have committed
	/// before we get here) still counts — there is no later resync to wait for on an idle root.
	async fn wait_initial_converged(&self) {
		assert!(
			wait_for_converged_resync(
				&self.cache.messages,
				self.remote_uuid,
				0,
				CACHE_CONVERGE_TIMEOUT
			)
			.await,
			"initial resync of remote root never converged"
		);
	}

	/// List the (dirs, files) directly under the remote root, via the client (ground truth).
	async fn list_remote_root(
		&self,
	) -> (
		Vec<RemoteDirectory>,
		Vec<filen_sdk_rs::fs::file::RemoteFile>,
	) {
		self.client
			.list_dir(
				&DirType::<Normal>::Dir(Cow::Borrowed(&self.resources.dir)),
				None::<&fn(u64, Option<u64>)>,
			)
			.await
			.unwrap()
	}

	async fn list_remote_dir(
		&self,
		dir: &RemoteDirectory,
	) -> (
		Vec<RemoteDirectory>,
		Vec<filen_sdk_rs::fs::file::RemoteFile>,
	) {
		self.client
			.list_dir(
				&DirType::<Normal>::Dir(Cow::Borrowed(dir)),
				None::<&fn(u64, Option<u64>)>,
			)
			.await
			.unwrap()
	}

	fn root_dirtype(&self) -> DirType<'_, Normal> {
		DirType::<Normal>::Dir(Cow::Borrowed(&self.resources.dir))
	}

	async fn open_engine(&self) -> SyncEngine {
		// The engine's remote view comes from the cache, so it MUST be opened against the
		// derived client whose cache is configured (`cache.client`), NOT the shared base client.
		SyncEngine::open(self.cache.client.clone(), temp_cache_path())
			.await
			.unwrap()
	}

	fn cleanup(&self) {
		std::fs::remove_dir_all(&self.local).ok();
	}
}

/// Upload a file with exact bytes to the remote root, return the created RemoteFile.
async fn upload_remote(
	fx: &Fixture,
	name: &str,
	data: &[u8],
) -> filen_sdk_rs::fs::file::RemoteFile {
	let builder = fx
		.client
		.make_file_builder(name, fx.resources.dir.uuid())
		.unwrap();
	fx.client.upload_file(builder, data).await.unwrap()
}

/// Wait until the cache reports a file with `name` directly under the remote root, returning its
/// uuid. Asserts on timeout.
async fn wait_cache_file_in_root(fx: &Fixture, uuid: Uuid) {
	assert!(
		poll_for_item(fx.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache never observed item {uuid}"
	);
}

fn write_local(local: &std::path::Path, rel: &str, data: &[u8]) {
	let path = local.join(rel);
	if let Some(parent) = path.parent() {
		std::fs::create_dir_all(parent).unwrap();
	}
	std::fs::write(path, data).unwrap();
}

fn read_local(local: &std::path::Path, rel: &str) -> Vec<u8> {
	std::fs::read(local.join(rel)).unwrap()
}

fn local_exists(local: &std::path::Path, rel: &str) -> bool {
	local.join(rel).exists()
}

/// The byte size of a remote file (public `size` field).
fn rsize(f: &filen_sdk_rs::fs::file::RemoteFile) -> u64 {
	f.size
}

/// Synchronous "is this uuid in the cache?" check, usable inside a `poll_until` predicate.
#[cfg(feature = "malformed")]
fn poll_item_sync(db: &std::path::Path, uuid: Uuid) -> bool {
	query_item_type(db, uuid).is_some()
}

// ============================================================================
// LocalToRemote
// ============================================================================

#[shared_test_runtime]
async fn l2r_new_file_uploads() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "hello.txt", b"hello world");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(
		report.uploaded, 1,
		"expected exactly one upload: {report:?}"
	);
	assert_eq!(report.downloaded, 0, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");
	assert!(report.errors.is_empty(), "{report:?}");

	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(files.len(), 1, "remote should have exactly 1 file");
	let f = find_file(&files, "hello.txt").expect("hello.txt missing on remote");
	assert_eq!(rsize(f), b"hello world".len() as u64);

	fx.cleanup();
}

#[shared_test_runtime]
async fn l2r_nested_dirs_and_files() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "a/b/c/deep.txt", b"deep content");
	write_local(&fx.local, "a/top.txt", b"top");
	write_local(&fx.local, "root.txt", b"r");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.uploaded, 3, "expected 3 file uploads: {report:?}");
	assert_eq!(
		report.remote_dirs_created, 3,
		"expected dirs a, a/b, a/b/c: {report:?}"
	);
	assert!(report.errors.is_empty(), "{report:?}");

	// Verify the remote tree shape.
	let (dirs, files) = fx.list_remote_root().await;
	assert!(find_file(&files, "root.txt").is_some());
	let a = find_dir(&dirs, "a").expect("dir a missing");
	let (a_dirs, a_files) = fx.list_remote_dir(a).await;
	assert!(find_file(&a_files, "top.txt").is_some());
	let b = find_dir(&a_dirs, "b").expect("dir a/b missing");
	let (b_dirs, _) = fx.list_remote_dir(b).await;
	let c = find_dir(&b_dirs, "c").expect("dir a/b/c missing");
	let (_, c_files) = fx.list_remote_dir(c).await;
	let deep = find_file(&c_files, "deep.txt").expect("deep.txt missing");
	assert_eq!(rsize(deep), b"deep content".len() as u64);

	fx.cleanup();
}

#[shared_test_runtime]
async fn l2r_empty_file() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "empty.bin", b"");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.uploaded, 1, "{report:?}");
	assert!(report.errors.is_empty(), "{report:?}");

	let (_dirs, files) = fx.list_remote_root().await;
	let f = find_file(&files, "empty.bin").expect("empty.bin missing");
	assert_eq!(rsize(f), 0, "empty file should be 0 bytes on remote");

	fx.cleanup();
}

#[shared_test_runtime]
async fn l2r_modify_then_resync_reuploads() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "doc.txt", b"version one");
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Modify locally, sync again.
	write_local(&fx.local, "doc.txt", b"version two is longer");
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r2.uploaded, 1, "modified file should re-upload: {r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");

	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(
		files.len(),
		1,
		"still exactly one file (versioned in place)"
	);
	let f = find_file(&files, "doc.txt").expect("doc.txt missing");
	assert_eq!(rsize(f), b"version two is longer".len() as u64);

	fx.cleanup();
}

#[shared_test_runtime]
async fn l2r_local_delete_propagates_to_remote() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "keep.txt", b"keep");
	write_local(&fx.local, "gone.txt", b"gone");
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 2, "{r1:?}");

	// Delete one local file; LocalToRemote should mirror the deletion.
	std::fs::remove_file(fx.local.join("gone.txt")).unwrap();
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.remotely_trashed, 1,
		"local delete should trash on remote: {r2:?}"
	);
	assert_eq!(
		r2.held_deletions(),
		0,
		"1 of 2 should not trip the guard: {r2:?}"
	);

	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(files.len(), 1, "remote should have only the kept file");
	assert!(find_file(&files, "keep.txt").is_some());
	assert!(find_file(&files, "gone.txt").is_none());

	fx.cleanup();
}

#[shared_test_runtime]
async fn l2r_empty_directory() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	std::fs::create_dir_all(fx.local.join("emptydir")).unwrap();

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.uploaded, 0, "{report:?}");
	assert_eq!(
		report.remote_dirs_created, 1,
		"empty dir should be created on remote: {report:?}"
	);
	assert!(report.errors.is_empty(), "{report:?}");

	let (dirs, files) = fx.list_remote_root().await;
	assert!(files.is_empty());
	assert!(find_dir(&dirs, "emptydir").is_some(), "emptydir missing");

	fx.cleanup();
}

// ============================================================================
// RemoteToLocal
// ============================================================================

#[shared_test_runtime]
async fn r2l_download_byte_exact() {
	let fx = Fixture::new().await;
	let payload: Vec<u8> = (0u8..=255).collect();
	let rf = upload_remote(&fx, "blob.bin", &payload).await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.downloaded, 1, "{report:?}");
	assert_eq!(report.uploaded, 0, "{report:?}");
	assert!(report.errors.is_empty(), "{report:?}");

	assert!(local_exists(&fx.local, "blob.bin"), "local blob missing");
	assert_eq!(read_local(&fx.local, "blob.bin"), payload, "byte mismatch");

	fx.cleanup();
}

#[shared_test_runtime]
async fn r2l_nested_and_empty_file() {
	let fx = Fixture::new().await;
	// Build remote tree: sub/inner.txt and sub/zero.bin (empty).
	let sub = fx
		.client
		.create_dir(&fx.root_dirtype(), "sub")
		.await
		.unwrap();
	let inner_builder = fx
		.client
		.make_file_builder("inner.txt", sub.uuid())
		.unwrap();
	let inner = fx
		.client
		.upload_file(inner_builder, b"inner bytes")
		.await
		.unwrap();
	let zero_builder = fx.client.make_file_builder("zero.bin", sub.uuid()).unwrap();
	let zero = fx.client.upload_file(zero_builder, b"").await.unwrap();

	wait_cache_file_in_root(&fx, inner.uuid()).await;
	wait_cache_file_in_root(&fx, zero.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.downloaded, 2, "{report:?}");
	assert_eq!(
		report.local_dirs_created, 1,
		"sub dir should be created locally: {report:?}"
	);
	assert!(report.errors.is_empty(), "{report:?}");

	assert_eq!(read_local(&fx.local, "sub/inner.txt"), b"inner bytes");
	assert!(local_exists(&fx.local, "sub/zero.bin"));
	assert_eq!(
		read_local(&fx.local, "sub/zero.bin"),
		b"",
		"empty remote file should be empty locally"
	);

	fx.cleanup();
}

// A remote content change is published by re-uploading the same name, which the server versions
// into a NEW uuid. The engine reconciles the uuid swap and re-downloads the new content over the
// existing local file. (This was once broken: the download's "file changed during download" guard
// re-stat'd the temp file instead of the destination and failed with FileChangedDuringSync; fixed
// in the io download path.)
#[shared_test_runtime]
async fn r2l_remote_modification_redownloads() {
	let fx = Fixture::new().await;
	let rf = upload_remote(&fx, "m.txt", b"first").await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(read_local(&fx.local, "m.txt"), b"first");

	// Re-upload the same name with new content (server versions it in place).
	let new_content = b"second and longer";
	let new_rf = upload_remote(&fx, "m.txt", new_content).await;
	// Wait until the cache reflects the NEW uuid AND its new size, so the engine reconciles
	// against the fully-committed new version (not a transient mid-versioning state).
	wait_cache_file_in_root(&fx, new_rf.uuid()).await;
	let new_uuid: Uuid = new_rf.uuid();
	let db = fx.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			query_cached_file(&db, new_uuid).map(|t| t.1) == Some(new_content.len() as i64)
		})
		.await,
		"cache never reflected the new file size"
	);

	// Give a bounded eventual-consistency window in case the cache is mid-versioning (the public
	// ops are eventually consistent); with the download-guard fix this resolves on the first pass.
	let mut r2 = engine.sync_once(pair).await.unwrap();
	let deadline = std::time::Instant::now() + Duration::from_secs(30);
	while r2.downloaded == 0
		&& r2
			.errors
			.iter()
			.any(|e| e.contains("FileChangedDuringSync"))
		&& std::time::Instant::now() < deadline
	{
		tokio::time::sleep(Duration::from_millis(500)).await;
		r2 = engine.sync_once(pair).await.unwrap();
	}
	assert_eq!(
		r2.downloaded, 1,
		"modified remote should re-download: {r2:?}"
	);
	assert_eq!(read_local(&fx.local, "m.txt"), new_content);

	fx.cleanup();
}

#[shared_test_runtime]
async fn r2l_remote_delete_removes_local() {
	let fx = Fixture::new().await;
	let keep = upload_remote(&fx, "keep.txt", b"keep").await;
	let mut gone = upload_remote(&fx, "gone.txt", b"gone").await;
	wait_cache_file_in_root(&fx, keep.uuid()).await;
	wait_cache_file_in_root(&fx, gone.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.downloaded, 2, "{r1:?}");
	assert!(local_exists(&fx.local, "gone.txt"));

	// Trash on remote; wait for the cache to drop it.
	fx.client.trash_file(&mut gone).await.unwrap();
	assert!(
		poll_for_item_absent(fx.db_path(), gone.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"cache never dropped trashed file"
	);

	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.locally_deleted, 1,
		"remote delete should remove the local file: {r2:?}"
	);
	assert!(
		!local_exists(&fx.local, "gone.txt"),
		"local file still present"
	);
	assert!(local_exists(&fx.local, "keep.txt"));

	fx.cleanup();
}

#[shared_test_runtime]
async fn r2l_deep_tree() {
	let fx = Fixture::new().await;
	// Build d1/d2/d3 with a file at the bottom and one mid-level.
	let d1 = fx
		.client
		.create_dir(&fx.root_dirtype(), "d1")
		.await
		.unwrap();
	let d2 = fx
		.client
		.create_dir(&DirType::<Normal>::Dir(Cow::Borrowed(&d1)), "d2")
		.await
		.unwrap();
	let d3 = fx
		.client
		.create_dir(&DirType::<Normal>::Dir(Cow::Borrowed(&d2)), "d3")
		.await
		.unwrap();
	let mid_b = fx.client.make_file_builder("mid.txt", d2.uuid()).unwrap();
	let mid = fx.client.upload_file(mid_b, b"mid").await.unwrap();
	let bot_b = fx
		.client
		.make_file_builder("bottom.txt", d3.uuid())
		.unwrap();
	let bottom = fx.client.upload_file(bot_b, b"bottom value").await.unwrap();
	wait_cache_file_in_root(&fx, mid.uuid()).await;
	wait_cache_file_in_root(&fx, bottom.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.downloaded, 2, "{report:?}");
	assert_eq!(report.local_dirs_created, 3, "{report:?}");
	assert!(report.errors.is_empty(), "{report:?}");
	assert_eq!(read_local(&fx.local, "d1/d2/mid.txt"), b"mid");
	assert_eq!(
		read_local(&fx.local, "d1/d2/d3/bottom.txt"),
		b"bottom value"
	);

	fx.cleanup();
}

// ============================================================================
// TwoWay
// ============================================================================

#[shared_test_runtime]
async fn twoway_merge_both_sides_in_one_pass() {
	let fx = Fixture::new().await;
	let rf = upload_remote(&fx, "remote_only.txt", b"from remote").await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;

	write_local(&fx.local, "local_only.txt", b"from local");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.uploaded, 1, "{report:?}");
	assert_eq!(report.downloaded, 1, "{report:?}");
	assert_eq!(report.conflicts.len(), 0, "{report:?}");
	assert!(report.errors.is_empty(), "{report:?}");

	// Both sides end with both files.
	assert!(local_exists(&fx.local, "local_only.txt"));
	assert_eq!(read_local(&fx.local, "remote_only.txt"), b"from remote");
	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(files.len(), 2, "remote should have both files");
	assert!(find_file(&files, "remote_only.txt").is_some());
	assert!(find_file(&files, "local_only.txt").is_some());

	fx.cleanup();
}

#[shared_test_runtime]
async fn twoway_same_content_both_sides_no_conflict() {
	let fx = Fixture::new().await;
	let same = b"identical bytes on both";
	let rf = upload_remote(&fx, "same.txt", same).await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;
	write_local(&fx.local, "same.txt", same);

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(
		report.conflicts.len(),
		0,
		"identical content must not conflict: {report:?}"
	);
	assert!(report.errors.is_empty(), "{report:?}");
	// Content unchanged on both sides.
	assert_eq!(read_local(&fx.local, "same.txt"), same);
	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(files.len(), 1);
	assert_eq!(
		rsize(find_file(&files, "same.txt").unwrap()),
		same.len() as u64
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn twoway_divergent_modification_conflicts_nondestructively() {
	let fx = Fixture::new().await;
	let base = b"base content";
	let rf = upload_remote(&fx, "fight.txt", base).await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;
	write_local(&fx.local, "fight.txt", base);

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	// First pass establishes baseline (same content -> no conflict).
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r1.conflicts.len(),
		0,
		"baseline pass should be clean: {r1:?}"
	);

	// Now diverge BOTH sides.
	write_local(&fx.local, "fight.txt", b"LOCAL EDIT wins?");
	let new_rf = upload_remote(&fx, "fight.txt", b"REMOTE EDIT different bytes").await;
	wait_cache_file_in_root(&fx, new_rf.uuid()).await;

	let r2 = engine.sync_once(pair).await.unwrap();
	assert!(
		r2.conflict_paths().any(|c| c.contains("fight.txt")),
		"expected fight.txt in conflicts: {r2:?}"
	);
	// No destructive action: nothing trashed/deleted.
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");

	// Local content must be left untouched (conflict = left alone).
	assert_eq!(
		read_local(&fx.local, "fight.txt"),
		b"LOCAL EDIT wins?",
		"local side must be untouched on conflict"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn twoway_local_delete_propagates() {
	let fx = Fixture::new().await;
	let rf = upload_remote(&fx, "doomed.txt", b"x").await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert!(local_exists(&fx.local, "doomed.txt"));

	// Delete locally; TwoWay should mirror to remote (delete propagates).
	std::fs::remove_file(fx.local.join("doomed.txt")).unwrap();
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.remotely_trashed, 1,
		"a local delete should trash on remote in TwoWay: {r2:?}"
	);
	assert_eq!(r2.held_deletions(), 0, "{r2:?}");

	let (_dirs, files) = fx.list_remote_root().await;
	assert!(
		find_file(&files, "doomed.txt").is_none(),
		"remote still has it"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn twoway_idempotent_second_pass() {
	let fx = Fixture::new().await;
	let rf = upload_remote(&fx, "r.txt", b"remote").await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;
	write_local(&fx.local, "l.txt", b"local");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 1, "{r1:?}");
	assert_eq!(r1.downloaded, 1, "{r1:?}");

	// Second pass: nothing changed anywhere -> every counter zero.
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r2.uploaded, 0, "{r2:?}");
	assert_eq!(r2.downloaded, 0, "{r2:?}");
	assert_eq!(r2.local_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.remote_dirs_created, 0, "{r2:?}");
	assert_eq!(r2.locally_deleted, 0, "{r2:?}");
	assert_eq!(r2.remotely_trashed, 0, "{r2:?}");
	assert_eq!(r2.moved_remote, 0, "{r2:?}");
	assert_eq!(r2.moved_local, 0, "{r2:?}");
	assert_eq!(r2.conflicts.len(), 0, "{r2:?}");
	assert_eq!(r2.held_deletions(), 0, "{r2:?}");
	assert!(r2.errors.is_empty(), "{r2:?}");

	fx.cleanup();
}

// ============================================================================
// Backup modes
// ============================================================================

#[shared_test_runtime]
async fn local_backup_pushes_but_never_trashes_remote_only() {
	let fx = Fixture::new().await;
	// A remote-only file that must survive.
	let survivor = upload_remote(&fx, "survivor.txt", b"keep me").await;
	wait_cache_file_in_root(&fx, survivor.uuid()).await;
	// A local-only file that should be pushed up.
	write_local(&fx.local, "pushed.txt", b"new local");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalBackup)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.uploaded, 1, "local file pushed: {report:?}");
	assert_eq!(report.downloaded, 0, "backup never pulls: {report:?}");
	assert_eq!(
		report.remotely_trashed, 0,
		"LocalBackup must never trash a remote-only file: {report:?}"
	);
	assert_eq!(report.locally_deleted, 0, "{report:?}");

	let (_dirs, files) = fx.list_remote_root().await;
	assert!(
		find_file(&files, "survivor.txt").is_some(),
		"survivor trashed!"
	);
	assert!(find_file(&files, "pushed.txt").is_some(), "push failed");
	// The remote-only file was NOT pulled down locally.
	assert!(
		!local_exists(&fx.local, "survivor.txt"),
		"backup pulled remote down"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn remote_backup_pulls_but_never_deletes_local_only() {
	let fx = Fixture::new().await;
	let pulled = upload_remote(&fx, "pulled.txt", b"from remote").await;
	wait_cache_file_in_root(&fx, pulled.uuid()).await;
	// A local-only file that must survive.
	write_local(&fx.local, "local_survivor.txt", b"do not delete");

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteBackup)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	assert_eq!(report.downloaded, 1, "remote file pulled: {report:?}");
	assert_eq!(report.uploaded, 0, "remote backup never pushes: {report:?}");
	assert_eq!(
		report.locally_deleted, 0,
		"RemoteBackup must never delete a local-only file: {report:?}"
	);
	assert_eq!(report.remotely_trashed, 0, "{report:?}");

	assert_eq!(read_local(&fx.local, "pulled.txt"), b"from remote");
	assert!(
		local_exists(&fx.local, "local_survivor.txt"),
		"local-only file was deleted by RemoteBackup!"
	);
	// The local-only file was NOT pushed up.
	let (_dirs, files) = fx.list_remote_root().await;
	assert!(
		find_file(&files, "local_survivor.txt").is_none(),
		"backup pushed local up"
	);

	fx.cleanup();
}

// ============================================================================
// Moves / renames
// ============================================================================

#[shared_test_runtime]
async fn local_rename_moves_remote_in_place() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "before.txt", b"stable content payload");
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	// Capture the uuid the engine created remotely.
	let (_dirs, files0) = fx.list_remote_root().await;
	let orig_uuid = find_file(&files0, "before.txt").unwrap().uuid();

	// Rename locally (same bytes).
	std::fs::rename(fx.local.join("before.txt"), fx.local.join("after.txt")).unwrap();
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r2.moved_remote, 1, "rename should be a remote move: {r2:?}");
	assert_eq!(r2.uploaded, 0, "rename must NOT re-upload bytes: {r2:?}");

	let (_dirs, files1) = fx.list_remote_root().await;
	assert_eq!(files1.len(), 1, "remote should keep exactly one file");
	let after = find_file(&files1, "after.txt").expect("renamed file missing");
	assert_eq!(
		after.uuid(),
		orig_uuid,
		"uuid must be preserved across rename"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn remote_rename_moves_local_in_place() {
	let fx = Fixture::new().await;
	let mut rf = upload_remote(&fx, "old_name.txt", b"intact content here").await;
	wait_cache_file_in_root(&fx, rf.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(
		read_local(&fx.local, "old_name.txt"),
		b"intact content here"
	);

	// Rename on remote; wait for the cache to reflect the new name.
	fx.client
		.update_file_metadata(
			&mut rf,
			FileMetaChanges::default().name("new_name.txt").unwrap(),
		)
		.await
		.unwrap();
	assert!(
		poll_for_file_name(
			fx.db_path(),
			rf.uuid(),
			"new_name.txt",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache never saw the rename"
	);

	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.moved_local, 1,
		"remote rename should be a local move: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "rename must NOT re-download: {r2:?}");

	assert!(
		!local_exists(&fx.local, "old_name.txt"),
		"old local name remains"
	);
	assert_eq!(
		read_local(&fx.local, "new_name.txt"),
		b"intact content here",
		"content must survive the local rename"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn local_move_across_directories() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "src/file.txt", b"moving payload bytes");
	std::fs::create_dir_all(fx.local.join("dst")).unwrap();
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, 1, "{r1:?}");

	let (dirs0, _f0) = fx.list_remote_root().await;
	let src0 = find_dir(&dirs0, "src").unwrap();
	let (_sd, src_files) = fx.list_remote_dir(src0).await;
	let orig_uuid = find_file(&src_files, "file.txt").unwrap().uuid();

	// Move locally across directories.
	std::fs::rename(fx.local.join("src/file.txt"), fx.local.join("dst/file.txt")).unwrap();
	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.moved_remote, 1,
		"cross-dir move should be a remote move: {r2:?}"
	);
	assert_eq!(r2.uploaded, 0, "move must not re-upload bytes: {r2:?}");

	let (dirs1, _f1) = fx.list_remote_root().await;
	let dst = find_dir(&dirs1, "dst").expect("dst dir missing");
	let (_dd, dst_files) = fx.list_remote_dir(dst).await;
	let moved = find_file(&dst_files, "file.txt").expect("moved file missing in dst");
	assert_eq!(moved.uuid(), orig_uuid, "uuid preserved across move");
	// src should no longer contain the file.
	let src1 = find_dir(&dirs1, "src").unwrap();
	let (_s2, src_files2) = fx.list_remote_dir(src1).await;
	assert!(
		find_file(&src_files2, "file.txt").is_none(),
		"file still in src"
	);

	fx.cleanup();
}

#[shared_test_runtime]
async fn remote_move_across_directories() {
	let fx = Fixture::new().await;
	let from = fx
		.client
		.create_dir(&fx.root_dirtype(), "from")
		.await
		.unwrap();
	let to = fx
		.client
		.create_dir(&fx.root_dirtype(), "to")
		.await
		.unwrap();
	let fb = fx
		.client
		.make_file_builder("mover.txt", from.uuid())
		.unwrap();
	let mut mover = fx
		.client
		.upload_file(fb, b"cross dir remote move")
		.await
		.unwrap();
	wait_cache_file_in_root(&fx, mover.uuid()).await;

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.downloaded, 1, "{r1:?}");
	assert_eq!(
		read_local(&fx.local, "from/mover.txt"),
		b"cross dir remote move"
	);

	// Move on remote from `from` to `to`.
	fx.client
		.move_file(&mut mover, &DirType::<Normal>::Dir(Cow::Borrowed(&to)))
		.await
		.unwrap();
	// Wait for the cache to reflect the new parent: poll the cached parent_uuid.
	let to_uuid: Uuid = to.uuid();
	let mover_uuid: Uuid = mover.uuid();
	let db = fx.db_path().to_path_buf();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			match query_cached_file(&db, mover_uuid) {
				Some((_, _, _, parent)) => parent == to_uuid.as_bytes(),
				None => false,
			}
		})
		.await,
		"cache never saw the remote move"
	);

	let r2 = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		r2.moved_local, 1,
		"remote move should be a local move: {r2:?}"
	);
	assert_eq!(r2.downloaded, 0, "move must not re-download: {r2:?}");

	assert!(
		!local_exists(&fx.local, "from/mover.txt"),
		"old local path remains"
	);
	assert_eq!(
		read_local(&fx.local, "to/mover.txt"),
		b"cross dir remote move"
	);

	fx.cleanup();
}

// ============================================================================
// Mass-delete guard
// ============================================================================

// Create 20 files, sync up, then delete ALL 20 (100%) at once. The deletion volume
// (20 > max(floor=10, 0.5*20=10)) trips the mass-delete guard, which holds the trashing back
// (held_deletions > 0, remotely_trashed == 0) rather than nuking the whole remote in one pass.
#[shared_test_runtime]
async fn mass_delete_guard_holds_large_deletion() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	const TOTAL: usize = 20;
	const DELETE: usize = 20;
	for i in 0..TOTAL {
		write_local(
			&fx.local,
			&format!("f{i:02}.txt"),
			format!("content {i}").as_bytes(),
		);
	}
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let r1 = engine.sync_once(pair).await.unwrap();
	assert_eq!(r1.uploaded, TOTAL, "{r1:?}");

	for i in 0..DELETE {
		std::fs::remove_file(fx.local.join(format!("f{i:02}.txt"))).unwrap();
	}
	let r2 = engine.sync_once(pair).await.unwrap();

	assert!(
		r2.held_deletions() > 0 || r2.guard.is_some(),
		"mass-delete guard should have engaged for {DELETE}/{TOTAL} deletions: {r2:?}"
	);
	assert_eq!(
		r2.remotely_trashed, 0,
		"guard should have prevented the destructive trashing: {r2:?}"
	);

	// Remote should still have all files (nothing nuked).
	let (_dirs, files) = fx.list_remote_root().await;
	assert_eq!(
		files.len(),
		TOTAL,
		"guard must keep all remote files; found {}",
		files.len()
	);

	fx.cleanup();
}

// ============================================================================
// Case-insensitivity collision
// ============================================================================

// Two same-parent items differing ONLY in case can coexist on a real remote: the server dedups
// case-insensitively via a client-supplied name hash, so a non-conforming client that sends a
// MISMATCHED hash (here, `create_dir_with_name_hash`) bypasses the dedup. Pulling both onto a
// case-insensitive local FS would clobber/lose data, so the engine must DETECT the collision and
// hold both back, reporting each as unsyncable, rather than do partial destructive work.
//
// Requires the `malformed` feature for the mismatched-hash seam:
//   cargo test -p filen-sdk-rs -F sync-engine-live-tests,malformed --test sync_engine_blackbox_tests
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn remote_case_collision_held_back() {
	let fx = Fixture::new().await;
	let parent = fx.root_dirtype();

	// "Note" with its correct (lowercased) hash; then "note" with a DELIBERATELY different hash so
	// the server does not version it away against "Note".
	let upper = fx.client.create_dir(&parent, "Note").await.unwrap();
	let mismatched = fx.client.hash_name("note-collision-bypass");
	let lower = fx
		.client
		.create_dir_with_name_hash(&parent, "note", &mismatched)
		.await
		.unwrap();
	let upper_uuid: Uuid = upper.uuid();
	let lower_uuid: Uuid = lower.uuid();

	// Wait (bounded) for the cache (the engine's remote view) to hold BOTH.
	let db = fx.db_path().to_path_buf();
	poll_until(Duration::from_secs(60), || {
		poll_item_sync(&db, upper_uuid) && poll_item_sync(&db, lower_uuid)
	})
	.await;

	// Ground-truth precondition: the remote actually holds BOTH case-colliding dirs. (If the server
	// rejected the mismatched-hash bypass this would fail here, before the engine is exercised.)
	let (remote_dirs, _files) = fx.list_remote_root().await;
	let colliding: Vec<_> = remote_dirs
		.iter()
		.filter(|d| d.name().is_some_and(|n| n.eq_ignore_ascii_case("note")))
		.collect();
	assert_eq!(
		colliding.len(),
		2,
		"precondition: the remote must hold BOTH case-colliding dirs (mismatched-hash bypass), but \
		 held {} (names: {:?})",
		colliding.len(),
		remote_dirs
			.iter()
			.filter_map(|d| d.name())
			.collect::<Vec<_>>()
	);

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let report = engine.sync_once(pair).await.unwrap();

	let held: Vec<&str> = report
		.unsyncable
		.iter()
		.filter(|path| {
			path.reason == filen_sdk_rs::sync_engine::UnsyncableReason::RemoteNameCollision
		})
		.map(|path| path.rel_path.as_str())
		.collect();
	assert_eq!(
		held,
		["Note", "note"],
		"both colliding names must be reported: {report:?}"
	);
	assert_eq!(
		report.local_dirs_created, 0,
		"neither colliding name may be created locally: {report:?}"
	);

	fx.cleanup();
}

/// The BLAKE3 the cache lists the file `uuid` with, as the lowercase hex it stores, or `None` when
/// it lists none or no such file.
#[cfg(feature = "malformed")]
fn cached_hash(db: &std::path::Path, uuid: Uuid) -> Option<String> {
	open_read_db(db)
		.ok()?
		.query_row(
			"SELECT f.hash FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
			rusqlite::params![uuid],
			|row| row.get(0),
		)
		.ok()
		.flatten()
}

/// A remote file whose content does not match the hash its metadata declares is not downloaded:
/// it is reported on its own on every pass, the local copy at its path stays as it was (put back
/// after the download moved it aside, then never touched again), and once the file is uploaded
/// afresh the path syncs as usual.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_remote_file_not_matching_its_hash_is_skipped_until_it_changes() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;
	let mut bad = upload_remote(&fx, "bad.txt", b"the bytes the uploader wrote").await;
	let wrong = blake3::hash(b"other bytes");
	fx.client
		.replace_file_hash(&mut bad, wrong.into())
		.await
		.unwrap();
	let db = fx.db_path().to_path_buf();
	let bad_uuid: Uuid = bad.uuid();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			cached_hash(&db, bad_uuid).as_deref() == Some(wrong.to_hex().as_str())
		})
		.await,
		"the cache never listed the file with the hash it was given"
	);
	// A copy no pass ever synced, so the download moves it aside first.
	let local_copy = b"the local copy";
	write_local(&fx.local, "bad.txt", local_copy);

	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let skipped = |report: &SyncReport| {
		report
			.unsyncable
			.iter()
			.filter(|path| {
				matches!(
					path.reason,
					UnsyncableReason::RemoteContentHashMismatch { .. }
				)
			})
			.map(|path| path.rel_path.clone())
			.collect::<Vec<_>>()
	};

	let first = engine.sync_once(pair).await.unwrap();
	assert_eq!(first.downloaded, 0, "{first:?}");
	assert_eq!(skipped(&first), ["bad.txt"], "{first:?}");
	assert_eq!(first.errors.len(), 1, "{first:?}");
	assert!(first.errors[0].starts_with("bad.txt: "), "{first:?}");
	assert!(first.quarantined.is_empty(), "{first:?}");
	assert_eq!(read_local(&fx.local, "bad.txt"), local_copy);

	let second = engine.sync_once(pair).await.unwrap();
	assert_eq!(second.downloaded, 0, "{second:?}");
	assert_eq!(skipped(&second), ["bad.txt"], "{second:?}");
	assert!(second.errors.is_empty(), "nothing is attempted: {second:?}");
	assert!(second.quarantined.is_empty(), "{second:?}");
	assert_eq!(read_local(&fx.local, "bad.txt"), local_copy);

	// A fresh upload versions the file: a new uuid, whose content matches its hash.
	let fixed = b"the bytes uploaded afresh";
	let fixed_uuid: Uuid = upload_remote(&fx, "bad.txt", fixed).await.uuid();
	let fixed_hash = blake3::hash(fixed);
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			cached_hash(&db, fixed_uuid).as_deref() == Some(fixed_hash.to_hex().as_str())
				&& query_cached_file(&db, fixed_uuid).map(|file| file.1)
					== Some(i64::try_from(fixed.len()).unwrap())
		})
		.await,
		"the cache never listed the new version"
	);

	let third = engine.sync_once(pair).await.unwrap();
	assert_eq!(third.downloaded, 1, "{third:?}");
	assert!(skipped(&third).is_empty(), "{third:?}");
	assert_eq!(read_local(&fx.local, "bad.txt"), fixed);
	assert_eq!(
		third.quarantined,
		["bad.txt"],
		"the copy no pass synced is moved aside for the download: {third:?}"
	);

	fx.cleanup();
}

/// In two-way, a synced file's new remote version that does not match its hash is skipped while the
/// local copy stays what the last sync left, touched or not, and a local edit then goes up over it:
/// a dry run shows the upload and changes nothing, the real pass uploads it, keeps the damaged
/// version in the file's history and ends the skip, and the pass after it has nothing to do.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_local_edit_uploads_over_a_remote_file_not_matching_its_hash() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;
	with_versions_and_versioning(&fx.client, true, async {
		let synced = b"the content both sides synced";
		let first = upload_remote(&fx, "bad.txt", synced).await;
		let db = fx.db_path().to_path_buf();
		let (first_uuid, synced_hash): (Uuid, _) = (first.uuid(), blake3::hash(synced));
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || {
				cached_hash(&db, first_uuid).as_deref() == Some(synced_hash.to_hex().as_str())
			})
			.await,
			"the cache never listed the synced file"
		);
		let engine = fx.open_engine().await;
		let pair = engine
			.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
			.await
			.unwrap();
		let initial = engine.sync_once(pair).await.unwrap();
		assert_eq!(initial.downloaded, 1, "{initial:?}");
		assert_eq!(read_local(&fx.local, "bad.txt"), synced);

		let mut bad = upload_remote(&fx, "bad.txt", b"the new version").await;
		let wrong = blake3::hash(b"other bytes");
		fx.client
			.replace_file_hash(&mut bad, wrong.into())
			.await
			.unwrap();
		let bad_uuid: Uuid = bad.uuid();
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || {
				cached_hash(&db, bad_uuid).as_deref() == Some(wrong.to_hex().as_str())
			})
			.await,
			"the cache never listed the new version with the hash it was given"
		);
		let skipped = |unsyncable: &[UnsyncablePath]| {
			unsyncable
				.iter()
				.filter(|path| {
					matches!(
						path.reason,
						UnsyncableReason::RemoteContentHashMismatch { .. }
					)
				})
				.map(|path| path.rel_path.clone())
				.collect::<Vec<_>>()
		};

		let found = engine.sync_once(pair).await.unwrap();
		assert_eq!(found.downloaded, 0, "{found:?}");
		assert_eq!(skipped(&found.unsyncable), ["bad.txt"], "{found:?}");
		assert_eq!(read_local(&fx.local, "bad.txt"), synced);

		// What the failed download left, and a touch of it, are no local edits.
		let left = engine.sync_once(pair).await.unwrap();
		assert_eq!((left.uploaded, left.downloaded), (0, 0), "{left:?}");
		assert_eq!(skipped(&left.unsyncable), ["bad.txt"], "{left:?}");
		assert!(left.errors.is_empty(), "{left:?}");
		std::fs::File::options()
			.write(true)
			.open(fx.local.join("bad.txt"))
			.unwrap()
			.set_modified(std::time::SystemTime::now() + Duration::from_secs(3_600))
			.unwrap();
		let touched = engine.sync_once(pair).await.unwrap();
		assert_eq!(
			(touched.uploaded, touched.downloaded),
			(0, 0),
			"{touched:?}"
		);
		assert_eq!(skipped(&touched.unsyncable), ["bad.txt"], "{touched:?}");
		assert!(touched.conflicts.is_empty(), "{touched:?}");

		let edit = b"the edit made since the last sync";
		write_local(&fx.local, "bad.txt", edit);

		for _ in 0..2 {
			let dry = engine.plan_pair(pair).await.unwrap();
			assert!(
				matches!(
					dry.actions.as_slice(),
					[PlannedAction {
						kind: PlannedActionKind::UploadFile,
						rel_path,
						..
					}] if rel_path == "bad.txt"
				),
				"{dry:?}"
			);
			assert!(dry.conflicts.is_empty(), "{dry:?}");
			assert!(skipped(&dry.unsyncable).is_empty(), "{dry:?}");
		}

		let replaced = engine.sync_once(pair).await.unwrap();
		assert_eq!(replaced.uploaded, 1, "{replaced:?}");
		assert_eq!(replaced.downloaded, 0, "{replaced:?}");
		assert!(replaced.conflicts.is_empty(), "{replaced:?}");
		assert!(replaced.errors.is_empty(), "{replaced:?}");
		assert!(skipped(&replaced.unsyncable).is_empty(), "{replaced:?}");
		assert_eq!(read_local(&fx.local, "bad.txt"), edit);

		let (_dirs, files) = fx.list_remote_root().await;
		let head = find_file(&files, "bad.txt").expect("the file stays at its path");
		let head_uuid: Uuid = head.uuid();
		assert_ne!(head_uuid, bad_uuid);
		assert_eq!(head.hash(), Some(blake3::hash(edit).into()));
		let versions = fx.client.list_file_versions(head).await.unwrap();
		assert!(
			versions.iter().any(|version| version.uuid() == bad_uuid),
			"the damaged version stays in the file's history"
		);

		let settled = engine.sync_once(pair).await.unwrap();
		assert_eq!(
			(settled.uploaded, settled.downloaded),
			(0, 0),
			"{settled:?}"
		);
		assert!(settled.conflicts.is_empty(), "{settled:?}");
		assert!(settled.errors.is_empty(), "{settled:?}");
		assert!(skipped(&settled.unsyncable).is_empty(), "{settled:?}");
	})
	.await;
	fx.cleanup();
}

/// What [`local_edit_held_over_a_damaged_version`] leaves in the local `bad.txt`.
#[cfg(feature = "malformed")]
const EDIT_OVER_A_DAMAGED_VERSION: &[u8] = b"the edit made since the last sync";

/// With versioning off, take a synced file's local edit over a new remote version not matching its
/// hash to the pass that holds it as a conflict, instead of uploading it: the upload would lose
/// that version for good. Nothing has run since that pass. The caller runs it under
/// [`with_versioning`] with the setting off.
#[cfg(feature = "malformed")]
async fn local_edit_held_over_a_damaged_version(fx: &Fixture) -> (SyncEngine, i64) {
	fx.wait_initial_converged().await;
	let synced = b"the content both sides synced";
	let first = upload_remote(fx, "bad.txt", synced).await;
	let db = fx.db_path().to_path_buf();
	let (first_uuid, synced_hash): (Uuid, _) = (first.uuid(), blake3::hash(synced));
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			cached_hash(&db, first_uuid).as_deref() == Some(synced_hash.to_hex().as_str())
		})
		.await,
		"the cache never listed the synced file"
	);
	let engine = fx.open_engine().await;
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	let initial = engine.sync_once(pair).await.unwrap();
	assert_eq!(initial.downloaded, 1, "{initial:?}");

	let mut bad = upload_remote(fx, "bad.txt", b"the new version").await;
	let wrong = blake3::hash(b"other bytes");
	fx.client
		.replace_file_hash(&mut bad, wrong.into())
		.await
		.unwrap();
	let bad_uuid: Uuid = bad.uuid();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			cached_hash(&db, bad_uuid).as_deref() == Some(wrong.to_hex().as_str())
		})
		.await,
		"the cache never listed the new version with the hash it was given"
	);
	let found = engine.sync_once(pair).await.unwrap();
	assert_eq!(
		skipped_for_hash(&found.unsyncable),
		["bad.txt"],
		"{found:?}"
	);

	write_local(&fx.local, "bad.txt", EDIT_OVER_A_DAMAGED_VERSION);
	let dry = engine.plan_pair(pair).await.unwrap();
	assert!(dry.actions.is_empty(), "{dry:?}");
	assert_eq!(dry.conflicts.len(), 1, "{dry:?}");

	let held = engine.sync_once(pair).await.unwrap();
	assert_eq!((held.uploaded, held.downloaded), (0, 0), "{held:?}");
	assert_eq!(
		held.conflicts
			.iter()
			.map(|conflict| conflict.rel_path.as_str())
			.collect::<Vec<_>>(),
		["bad.txt"],
		"{held:?}"
	);
	assert_eq!(
		read_local(&fx.local, "bad.txt"),
		EDIT_OVER_A_DAMAGED_VERSION
	);
	let (_dirs, files) = fx.list_remote_root().await;
	let head = find_file(&files, "bad.txt").expect("the file stays at its path");
	let head_uuid: Uuid = head.uuid();
	assert_eq!(head_uuid, bad_uuid, "nothing went up over it");
	(engine, pair)
}

/// The conflict is what the pair reports from then on, the version stays the file's head until the
/// user decides, and keeping the local side uploads the edit.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_local_edit_over_a_damaged_version_is_a_conflict_with_versioning_off() {
	let fx = Fixture::new().await;
	with_versioning(&fx.client, false, async {
		let (engine, pair) = local_edit_held_over_a_damaged_version(&fx).await;

		let still = engine.sync_once(pair).await.unwrap();
		assert_eq!((still.uploaded, still.downloaded), (0, 0), "{still:?}");
		assert_eq!(still.conflicts.len(), 1, "{still:?}");
		assert!(skipped_for_hash(&still.unsyncable).is_empty(), "{still:?}");

		engine
			.resolve_conflict(pair, "bad.txt", ConflictResolution::KeepLocal)
			.await
			.unwrap();
		let resolved = engine.sync_once(pair).await.unwrap();
		assert_eq!(resolved.uploaded, 1, "{resolved:?}");
		assert!(resolved.conflicts.is_empty(), "{resolved:?}");
		let (_dirs, files) = fx.list_remote_root().await;
		let head = find_file(&files, "bad.txt").expect("the file stays at its path");
		assert_eq!(
			head.hash(),
			Some(blake3::hash(EDIT_OVER_A_DAMAGED_VERSION).into())
		);
	})
	.await;
	fx.cleanup();
}

/// Keeping the local side with no pass since the one that held the conflict uploads the edit: the
/// hash mismatch's mark does not outlive the resolution.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn keeping_local_right_after_the_conflict_over_a_damaged_version_uploads_the_edit() {
	let fx = Fixture::new().await;
	with_versioning(&fx.client, false, async {
		let (engine, pair) = local_edit_held_over_a_damaged_version(&fx).await;

		engine
			.resolve_conflict(pair, "bad.txt", ConflictResolution::KeepLocal)
			.await
			.unwrap();
		let resolved = engine.sync_once(pair).await.unwrap();
		assert_eq!(resolved.uploaded, 1, "{resolved:?}");
		assert!(resolved.conflicts.is_empty(), "{resolved:?}");
		assert!(
			skipped_for_hash(&resolved.unsyncable).is_empty(),
			"{resolved:?}"
		);
		let (_dirs, files) = fx.list_remote_root().await;
		let head = find_file(&files, "bad.txt").expect("the file stays at its path");
		assert_eq!(
			head.hash(),
			Some(blake3::hash(EDIT_OVER_A_DAMAGED_VERSION).into())
		);
	})
	.await;
	fx.cleanup();
}

/// Keeping the remote side with no pass since the one that held the conflict moves the edit to the
/// bin and downloads the version, which finds the mismatch again and skips the path from then on.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn keeping_remote_right_after_the_conflict_over_a_damaged_version_marks_the_version_again() {
	let fx = Fixture::new().await;
	with_versioning(&fx.client, false, async {
		let (engine, pair) = local_edit_held_over_a_damaged_version(&fx).await;

		let quarantined = engine
			.resolve_conflict(pair, "bad.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap()
			.quarantined
			.expect("the edit goes to the bin");
		assert_eq!(
			std::fs::read(quarantined).unwrap(),
			EDIT_OVER_A_DAMAGED_VERSION
		);
		let pulled = engine.sync_once(pair).await.unwrap();
		assert_eq!((pulled.uploaded, pulled.downloaded), (0, 0), "{pulled:?}");
		assert!(!local_exists(&fx.local, "bad.txt"));
		let skipped = engine.sync_once(pair).await.unwrap();
		assert_eq!(
			(skipped.uploaded, skipped.downloaded),
			(0, 0),
			"{skipped:?}"
		);
		assert_eq!(
			skipped_for_hash(&skipped.unsyncable),
			["bad.txt"],
			"{skipped:?}"
		);
	})
	.await;
	fx.cleanup();
}

/// Keeping both sides with no pass since the one that held the conflict uploads the copy renamed
/// aside and downloads the version, which finds the mismatch again and skips the path from then on.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn keeping_both_right_after_the_conflict_over_a_damaged_version_uploads_the_aside_copy() {
	let fx = Fixture::new().await;
	with_versioning(&fx.client, false, async {
		let (engine, pair) = local_edit_held_over_a_damaged_version(&fx).await;

		engine
			.resolve_conflict(pair, "bad.txt", ConflictResolution::KeepBoth)
			.await
			.unwrap();
		let pulled = engine.sync_once(pair).await.unwrap();
		assert_eq!((pulled.uploaded, pulled.downloaded), (1, 0), "{pulled:?}");
		assert!(!local_exists(&fx.local, "bad.txt"));
		let (_dirs, files) = fx.list_remote_root().await;
		let aside = find_file(&files, "bad.old.txt").expect("the copy renamed aside goes up");
		assert_eq!(
			aside.hash(),
			Some(blake3::hash(EDIT_OVER_A_DAMAGED_VERSION).into())
		);
		let skipped = engine.sync_once(pair).await.unwrap();
		assert_eq!(
			skipped_for_hash(&skipped.unsyncable),
			["bad.txt"],
			"{skipped:?}"
		);
	})
	.await;
	fx.cleanup();
}

/// The paths a pass reports as skipped for a remote file that does not match its hash.
#[cfg(feature = "malformed")]
fn skipped_for_hash(unsyncable: &[UnsyncablePath]) -> Vec<String> {
	unsyncable
		.iter()
		.filter(|path| {
			matches!(
				path.reason,
				UnsyncableReason::RemoteContentHashMismatch { .. }
			)
		})
		.map(|path| path.rel_path.clone())
		.collect()
}

/// A running watch on `pair`, and every report it completes a pass with, in order.
#[cfg(feature = "malformed")]
async fn watch_reporting(
	engine: &Arc<SyncEngine>,
	pair: i64,
) -> (
	filen_sdk_rs::sync_engine::WatchHandle,
	Arc<std::sync::Mutex<Vec<SyncReport>>>,
) {
	let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
	let handle = engine
		.clone()
		.watch_observed(pair, {
			let reports = Arc::clone(&reports);
			Box::new(move |event| {
				if let SyncEvent::PassCompleted { report } = event {
					reports.lock().unwrap().push(report);
				}
			})
		})
		.await
		.unwrap();
	(handle, reports)
}

/// Upload `name` to the remote root declaring a hash of other content, until the cache lists it so,
/// and wait for a pass of the watch `reports` come from to skip it.
#[cfg(feature = "malformed")]
async fn damaged_upload_is_skipped(
	fx: &Fixture,
	name: &str,
	reports: &std::sync::Mutex<Vec<SyncReport>>,
) -> filen_sdk_rs::fs::file::RemoteFile {
	let mut bad = upload_remote(fx, name, b"the bytes the uploader wrote").await;
	let wrong = blake3::hash(b"other bytes");
	fx.client
		.replace_file_hash(&mut bad, wrong.into())
		.await
		.unwrap();
	let skipped = poll_until(CACHE_CONVERGE_TIMEOUT, || {
		reports
			.lock()
			.unwrap()
			.last()
			.is_some_and(|report| skipped_for_hash(&report.unsyncable) == [name])
	})
	.await;
	assert!(skipped, "{:?}", reports.lock().unwrap());
	bad
}

/// Upload `name` to the remote root to wake the watch `reports` come from, and return the report of
/// the first change-scoped pass to complete after that. The pass the upload wakes reads whole in a
/// tree smaller than a changelist's cap, and the local change it makes wakes a narrowed one.
#[cfg(feature = "malformed")]
async fn pass_woken_by_a_remote_file(
	fx: &Fixture,
	name: &str,
	reports: &std::sync::Mutex<Vec<SyncReport>>,
) -> SyncReport {
	let seen = reports.lock().unwrap().len();
	upload_remote(fx, name, b"a file that wakes the watch").await;
	let scoped = |reports: &[SyncReport]| {
		reports
			.get(seen..)
			.into_iter()
			.flatten()
			.find(|report| report.full_pass.is_none())
			.cloned()
	};
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			scoped(&reports.lock().unwrap()).is_some()
		})
		.await,
		"no change-scoped pass followed {name}: {:?}",
		reports.lock().unwrap()
	);
	scoped(&reports.lock().unwrap()).expect("just found")
}

/// On a watched pair a change-scoped pass reads the remote file's mark like any other: the unsynced
/// local copy at the path stays where it is and the path stays reported, and a new version of the
/// file ends the skip at the pass it wakes.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_watched_pairs_change_scoped_pass_keeps_the_skip_until_the_file_changes() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;
	// A file that downloads, so the pair has a baseline for the watch's passes to narrow from.
	let kept = upload_remote(&fx, "keep.txt", b"a file that syncs").await;
	wait_cache_file_in_root(&fx, kept.uuid()).await;
	let mut bad = upload_remote(&fx, "bad.txt", b"the bytes the uploader wrote").await;
	let wrong = blake3::hash(b"other bytes");
	fx.client
		.replace_file_hash(&mut bad, wrong.into())
		.await
		.unwrap();
	let (db, bad_uuid): (_, Uuid) = (fx.db_path().to_path_buf(), bad.uuid());
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			cached_hash(&db, bad_uuid).as_deref() == Some(wrong.to_hex().as_str())
		})
		.await,
		"the cache never listed the file with the hash it was given"
	);
	let local_copy = b"the local copy";
	write_local(&fx.local, "bad.txt", local_copy);
	let engine = Arc::new(fx.open_engine().await);
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::RemoteToLocal)
		.await
		.unwrap();
	let (handle, reports) = watch_reporting(&engine, pair).await;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			reports
				.lock()
				.unwrap()
				.last()
				.is_some_and(|report| skipped_for_hash(&report.unsyncable) == ["bad.txt"])
		})
		.await,
		"{:?}",
		reports.lock().unwrap()
	);

	let scoped = pass_woken_by_a_remote_file(&fx, "wake.txt", &reports).await;
	assert_eq!(
		skipped_for_hash(&scoped.unsyncable),
		["bad.txt"],
		"{scoped:?}"
	);
	assert!(scoped.quarantined.is_empty(), "{scoped:?}");
	assert!(scoped.errors.is_empty(), "nothing is attempted: {scoped:?}");
	assert_eq!(read_local(&fx.local, "bad.txt"), local_copy);

	let fixed = b"the bytes uploaded afresh";
	upload_remote(&fx, "bad.txt", fixed).await;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			std::fs::read(fx.local.join("bad.txt")).is_ok_and(|bytes| bytes == fixed)
		})
		.await,
		"the new version never came down: {:?}",
		reports.lock().unwrap()
	);

	drop(handle);
	fx.cleanup();
}

/// On a watched pair a marked file's deletion on the remote ends the skip at the pass it wakes: the
/// removal names a version no row records, and the cache no longer listing it sends that pass to
/// read both sides whole. The local copy then goes the way of any deleted file.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_watched_pairs_marked_file_deleted_on_the_remote_ends_the_skip() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;
	let engine = Arc::new(fx.open_engine().await);
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
		.await
		.unwrap();
	upload_remote(&fx, "bad.txt", b"the synced content").await;
	let (handle, reports) = watch_reporting(&engine, pair).await;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			std::fs::read(fx.local.join("bad.txt"))
				.is_ok_and(|bytes| bytes == b"the synced content")
		})
		.await,
		"the first version never came down"
	);
	let mut bad = damaged_upload_is_skipped(&fx, "bad.txt", &reports).await;
	// A change-scoped pass in between keeps it, so that what follows is a change-scoped wake.
	pass_woken_by_a_remote_file(&fx, "wake.txt", &reports).await;

	fx.client.trash_file(&mut bad).await.unwrap();
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || !fx
			.local
			.join("bad.txt")
			.exists())
		.await,
		"the local copy of the deleted file stayed: {:?}",
		reports.lock().unwrap()
	);
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || {
			reports
				.lock()
				.unwrap()
				.last()
				.is_some_and(|report| skipped_for_hash(&report.unsyncable).is_empty())
		})
		.await,
		"{:?}",
		reports.lock().unwrap()
	);

	drop(handle);
	fx.cleanup();
}

/// On a watched pair a local edit over a marked version reaches its exit from a change-scoped pass:
/// it goes up over that version, which stays in the file's history, with no conflict.
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn a_watched_pairs_local_edit_uploads_over_a_marked_version() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;
	with_versions_and_versioning(&fx.client, true, async {
		let engine = Arc::new(fx.open_engine().await);
		let pair = engine
			.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::TwoWay)
			.await
			.unwrap();
		upload_remote(&fx, "bad.txt", b"the synced content").await;
		let (handle, reports) = watch_reporting(&engine, pair).await;
		assert!(
			poll_until(CACHE_CONVERGE_TIMEOUT, || {
				std::fs::read(fx.local.join("bad.txt"))
					.is_ok_and(|bytes| bytes == b"the synced content")
			})
			.await,
			"the first version never came down"
		);
		let bad = damaged_upload_is_skipped(&fx, "bad.txt", &reports).await;
		let bad_uuid: Uuid = bad.uuid();
		pass_woken_by_a_remote_file(&fx, "wake.txt", &reports).await;

		let edit = b"the edit made since the last sync";
		let edit_hash: filen_types::crypto::Blake3Hash = blake3::hash(edit).into();
		write_local(&fx.local, "bad.txt", edit);
		let mut head = None;
		let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
		while head.is_none() && std::time::Instant::now() < deadline {
			let (_dirs, files) = fx.list_remote_root().await;
			head = find_file(&files, "bad.txt")
				.filter(|file| file.hash() == Some(edit_hash))
				.cloned();
			tokio::time::sleep(Duration::from_millis(300)).await;
		}
		let head =
			head.unwrap_or_else(|| panic!("the edit never went up: {:?}", reports.lock().unwrap()));
		let versions = fx.client.list_file_versions(&head).await.unwrap();
		assert!(
			versions.iter().any(|version| version.uuid() == bad_uuid),
			"the damaged version stays in the file's history"
		);
		assert!(
			reports
				.lock()
				.unwrap()
				.iter()
				.all(|report| report.conflicts.is_empty()),
			"{:?}",
			reports.lock().unwrap()
		);

		drop(handle);
	})
	.await;
	fx.cleanup();
}

// ============================================================================
// Continuous engine (watch)
// ============================================================================

#[shared_test_runtime]
async fn watch_auto_pushes_local_create() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	let engine = Arc::new(fx.open_engine().await);
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let handle = engine.clone().watch(pair).await.unwrap();

	// Create a file locally AFTER watch started.
	write_local(&fx.local, "watched.txt", b"auto-pushed payload");

	// Poll the REMOTE listing (ground truth) until the file appears.
	let mut found = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_dirs, files) = fx.list_remote_root().await;
		if find_file(&files, "watched.txt").is_some() {
			found = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(
		found,
		"watch did not auto-push the new local file to remote"
	);

	drop(handle);
	fx.cleanup();
}

#[shared_test_runtime]
async fn watch_propagates_modification() {
	let fx = Fixture::new().await;
	fx.wait_initial_converged().await;

	write_local(&fx.local, "live.txt", b"first version");

	let engine = Arc::new(fx.open_engine().await);
	let pair = engine
		.add_pair(fx.local.clone(), fx.remote_uuid, SyncMode::LocalToRemote)
		.await
		.unwrap();
	let handle = engine.clone().watch(pair).await.unwrap();

	// Wait until the initial create is pushed.
	let initial_size = b"first version".len() as u64;
	let mut ready = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_dirs, files) = fx.list_remote_root().await;
		if find_file(&files, "live.txt").map(rsize) == Some(initial_size) {
			ready = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(ready, "watch never pushed the initial version");

	// Modify under the active watch.
	let new_content = b"second version which is noticeably longer than the first";
	write_local(&fx.local, "live.txt", new_content);

	let mut updated = false;
	let deadline = std::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while std::time::Instant::now() < deadline {
		let (_dirs, files) = fx.list_remote_root().await;
		if find_file(&files, "live.txt").map(rsize) == Some(new_content.len() as u64) {
			updated = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(300)).await;
	}
	assert!(updated, "watch did not propagate the modification");

	drop(handle);
	fx.cleanup();
}
