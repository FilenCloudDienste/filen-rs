//! Black-box end-to-end tests for the sync engine, authored against the documented PUBLIC API
//! only (`SyncEngine`, `SyncMode`, `SyncReport`, `WatchHandle`) — no knowledge of the engine's
//! internals. Lives in the separate integration-test crate, which can only reach `pub` items.
//!
//! Run: `cargo test -p filen-sdk-rs --features sync-engine --test sync_engine_blackbox_tests`
//! (needs `.env` with TEST_EMAIL / TEST_PASSWORD). One test (`remote_case_collision_refused`)
//! additionally needs the `malformed` feature to construct its precondition: run the suite with
//! `-F sync-engine,malformed` to include it.

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

/// Find a file by name in a listing.
fn find_file<'a>(
	files: &'a [filen_sdk_rs::fs::file::RemoteFile],
	name: &str,
) -> Option<&'a filen_sdk_rs::fs::file::RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

fn find_dir<'a>(dirs: &'a [RemoteDirectory], name: &str) -> Option<&'a RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
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
// Case-insensitivity collision refusal
// ============================================================================

// Two same-parent items differing ONLY in case can coexist on a real remote: the server dedups
// case-insensitively via a client-supplied name hash, so a non-conforming client that sends a
// MISMATCHED hash (here, `create_dir_with_name_hash`) bypasses the dedup. Pulling both onto a
// case-insensitive local FS would clobber/lose data, so the engine must DETECT the collision and
// refuse the whole pass (surfacing an error) rather than do partial destructive work.
//
// Requires the `malformed` feature for the mismatched-hash seam:
//   cargo test -p filen-sdk-rs -F sync-engine,malformed --test sync_engine_blackbox_tests
#[cfg(feature = "malformed")]
#[shared_test_runtime]
async fn remote_case_collision_refused() {
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
	let result = engine.sync_once(pair).await;

	// Refusal can be a hard Err, or a report carrying an error and NO partial destructive work.
	match result {
		Err(_) => { /* hard refusal is acceptable */ }
		Ok(report) => {
			assert!(
				!report.errors.is_empty() || !report.conflicts.is_empty(),
				"engine must surface the case collision as an error/conflict: {report:?}"
			);
			// The engine must not materialize BOTH colliding names onto a case-insensitive FS.
			assert!(
				report.local_dirs_created < 2,
				"engine must not create both colliding names locally: {report:?}"
			);
		}
	}

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
