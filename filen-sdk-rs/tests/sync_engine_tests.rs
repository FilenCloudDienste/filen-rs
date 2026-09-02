//! Live end-to-end tests for the sync engine: real account, real cache, real local filesystem.
//!
//! Each test scopes its cache + sync pair to a fresh per-test remote dir (auto-cleaned), so they
//! stay account-size independent. Run with: `cargo test -p filen-sdk-rs --features sync-engine
//! --test sync_engine_tests` (needs `.env` with TEST_EMAIL / TEST_PASSWORD).

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
	sync_engine::{SyncEngine, SyncEvent, SyncMode},
};
use uuid::Uuid;

mod helpers;
use helpers::*;

/// A fresh local temp directory for a test's local sync root.
fn temp_local_dir() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("filen_sync_e2e_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Direct (non-cache) listing of a remote dir's child dir-names and file-names.
async fn remote_listing(client: &Client, dir: &RemoteDirectory) -> (Vec<String>, Vec<String>) {
	let (dirs, files) = client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let dir_names = dirs.iter().map(|d| d.name().unwrap().to_string()).collect();
	let file_names = files
		.iter()
		.map(|f| f.name().unwrap().to_string())
		.collect();
	(dir_names, file_names)
}

/// `sync_once_observed` reports live per-action progress events to the caller's observer, in
/// happen-before order (PassStarted → Planned → per-action → PassCompleted).
#[shared_test_runtime]
async fn sync_once_observed_reports_per_action_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	assert!(
		wait_for_converged_resync(
			&cache.messages,
			resources.dir.uuid(),
			0,
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"initial resync of the empty root should converge"
	);

	let local = temp_local_dir();
	std::fs::write(local.join("a.txt"), b"alpha").unwrap();
	std::fs::create_dir(local.join("sub")).unwrap();
	std::fs::write(local.join("sub").join("b.txt"), b"bravo").unwrap();

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();

	let events = Arc::new(std::sync::Mutex::new(Vec::<SyncEvent>::new()));
	let sink = Arc::clone(&events);
	let report = engine
		.sync_once_observed(pair, &mut move |event| sink.lock().unwrap().push(event))
		.await
		.unwrap();
	assert!(
		report.errors.is_empty(),
		"apply errors: {:?}",
		report.errors
	);

	// The observer closure is dropped at the end of the call above, so we hold the only ref now.
	let events = Arc::try_unwrap(events).unwrap().into_inner().unwrap();

	assert!(
		matches!(
			events.first(),
			Some(SyncEvent::PassStarted {
				mode: SyncMode::LocalToRemote
			})
		),
		"first event must be PassStarted: {events:?}"
	);
	assert!(
		matches!(events.last(), Some(SyncEvent::PassCompleted { report }) if report.uploaded == 2),
		"last event must be PassCompleted with the final report: {events:?}"
	);
	// 2 uploads + 1 dir create = 3 planned actions.
	assert!(
		events
			.iter()
			.any(|e| matches!(e, SyncEvent::Planned { actions: 3 })),
		"a Planned event must announce the 3 actions: {events:?}"
	);

	let uploaded: Vec<&str> = events
		.iter()
		.filter_map(|e| match e {
			SyncEvent::Uploading { rel_path } => Some(rel_path.as_str()),
			_ => None,
		})
		.collect();
	assert!(
		uploaded.contains(&"a.txt") && uploaded.contains(&"sub/b.txt"),
		"both files must emit an Uploading event: {uploaded:?}"
	);
	assert!(
		events
			.iter()
			.any(|e| matches!(e, SyncEvent::CreatingRemoteDir { rel_path } if rel_path == "sub")),
		"the dir must emit a CreatingRemoteDir event: {events:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// LocalToRemote: a local tree (a file + a nested dir/file) is created on an empty remote root.
#[shared_test_runtime]
async fn local_to_remote_uploads_the_local_tree() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	// Converge the (empty) remote into the cache so the engine reconciles against truth.
	assert!(
		wait_for_converged_resync(
			&cache.messages,
			resources.dir.uuid(),
			0,
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"initial resync of the empty root should converge"
	);

	let local = temp_local_dir();
	std::fs::write(local.join("top.txt"), b"hello top").unwrap();
	std::fs::create_dir(local.join("sub")).unwrap();
	std::fs::write(local.join("sub").join("nested.txt"), b"hello nested").unwrap();

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();

	let report = engine.sync_once(pair).await.unwrap();
	assert!(
		report.errors.is_empty(),
		"apply errors: {:?}",
		report.errors
	);
	assert_eq!(report.uploaded, 2, "top.txt + sub/nested.txt");
	assert_eq!(report.remote_dirs_created, 1, "sub/");

	let (dirs, files) = remote_listing(&cache.client, &resources.dir).await;
	assert!(files.contains(&"top.txt".to_string()), "files: {files:?}");
	assert!(dirs.contains(&"sub".to_string()), "dirs: {dirs:?}");

	// Verify the nested file landed under sub/.
	let (subdirs, _) = cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let sub = subdirs
		.into_iter()
		.find(|d| d.name().unwrap() == "sub")
		.expect("sub dir exists on the remote");
	let (_, sub_files) = remote_listing(&cache.client, &sub).await;
	assert!(
		sub_files.contains(&"nested.txt".to_string()),
		"nested files: {sub_files:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// RemoteToLocal: a remote file is mirrored down to the empty local root with its content intact.
#[shared_test_runtime]
async fn remote_to_local_downloads_the_remote_file() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	// Create a remote file under the sync root and wait for the cache to see it.
	let builder = cache
		.client
		.make_file_builder("remote_only.txt", resources.dir.uuid())
		.unwrap();
	let remote_file = cache
		.client
		.upload_file(builder, b"remote content here")
		.await
		.unwrap();
	assert!(
		poll_for_item(cache.db_path(), remote_file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
		"the cache should observe the uploaded remote file"
	);

	let local = temp_local_dir();
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::RemoteToLocal)
		.await
		.unwrap();

	let report = engine.sync_once(pair).await.unwrap();
	assert!(
		report.errors.is_empty(),
		"apply errors: {:?}",
		report.errors
	);
	assert_eq!(report.downloaded, 1, "the one remote file");

	let downloaded = std::fs::read(local.join("remote_only.txt")).expect("file downloaded");
	assert_eq!(downloaded, b"remote content here", "content round-trips");

	std::fs::remove_dir_all(&local).ok();
}

/// Regression guard for the engine's core assumption: a same-name upload VERSIONS the existing
/// remote file (one current file), it does NOT create a duplicate — so the apply layer never needs
/// to delete a stale uuid after re-uploading a changed file. See
/// reference-filen-same-name-versioning.
#[shared_test_runtime]
async fn same_name_upload_versions_rather_than_duplicates() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;

	let first = client
		.make_file_builder("versioned.txt", resources.dir.uuid())
		.unwrap();
	let first = client.upload_file(first, b"v1").await.unwrap();

	let second = client
		.make_file_builder("versioned.txt", resources.dir.uuid())
		.unwrap();
	let second = client
		.upload_file(second, b"v2 longer content")
		.await
		.unwrap();

	// A fresh uuid is minted each upload (the client never reuses one).
	assert_ne!(first.uuid(), second.uuid(), "each upload mints a new uuid");

	let (_, files) = remote_listing(client, &resources.dir).await;
	let versioned_count = files.iter().filter(|n| *n == "versioned.txt").count();
	assert_eq!(
		versioned_count, 1,
		"a same-name upload versions the file; the remote shows exactly one 'versioned.txt', not a duplicate (got {files:?})"
	);
}

/// A second pass with nothing changed is a no-op — the baseline + mtime/size fast-path recognize
/// the already-synced state and transfer nothing.
#[shared_test_runtime]
async fn second_pass_with_no_changes_is_a_noop() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let builder = cache
		.client
		.make_file_builder("stable.txt", resources.dir.uuid())
		.unwrap();
	let remote_file = cache.client.upload_file(builder, b"stable").await.unwrap();
	assert!(poll_for_item(cache.db_path(), remote_file.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let local = temp_local_dir();
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::RemoteToLocal)
		.await
		.unwrap();

	let first = engine.sync_once(pair).await.unwrap();
	assert_eq!(first.downloaded, 1, "first pass downloads the file");

	let second = engine.sync_once(pair).await.unwrap();
	assert_eq!(second.downloaded, 0, "nothing to download the second time");
	assert_eq!(second.uploaded, 0);
	assert_eq!(second.locally_deleted, 0);
	assert_eq!(second.remotely_trashed, 0);
	assert!(second.errors.is_empty(), "no errors: {:?}", second.errors);

	std::fs::remove_dir_all(&local).ok();
}

/// RemoteToLocal mirror: a remote deletion propagates to the local tree, with the local copy moved
/// to the recoverable quarantine dir rather than destroyed.
#[shared_test_runtime]
async fn remote_deletion_quarantines_the_local_copy() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let builder = cache
		.client
		.make_file_builder("doomed.txt", resources.dir.uuid())
		.unwrap();
	let mut remote_file = cache.client.upload_file(builder, b"doomed").await.unwrap();
	let file_uuid: Uuid = remote_file.uuid();
	assert!(poll_for_item(cache.db_path(), file_uuid, CACHE_CONVERGE_TIMEOUT).await);

	let local = temp_local_dir();
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::RemoteToLocal)
		.await
		.unwrap();
	assert_eq!(engine.sync_once(pair).await.unwrap().downloaded, 1);
	assert!(local.join("doomed.txt").exists(), "downloaded first");

	// Trash it remotely and wait for the cache to drop it.
	cache.client.trash_file(&mut remote_file).await.unwrap();
	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"cache should observe the remote deletion"
	);

	let report = engine.sync_once(pair).await.unwrap();
	assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
	assert_eq!(report.locally_deleted, 1, "the deletion propagated locally");
	assert!(!local.join("doomed.txt").exists(), "removed from the tree");
	assert!(
		local.join(".filen-sync-trash").join("doomed.txt").exists(),
		"moved to the recoverable quarantine, not destroyed"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// TwoWay: a local-only file is pushed up and a remote-only file is pulled down in a single pass;
/// both sides end up holding both files.
#[shared_test_runtime]
async fn two_way_merges_both_sides_in_one_pass() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let builder = cache
		.client
		.make_file_builder("from_remote.txt", resources.dir.uuid())
		.unwrap();
	let remote_file = cache
		.client
		.upload_file(builder, b"from remote")
		.await
		.unwrap();
	assert!(poll_for_item(cache.db_path(), remote_file.uuid(), CACHE_CONVERGE_TIMEOUT).await);

	let local = temp_local_dir();
	std::fs::write(local.join("from_local.txt"), b"from local").unwrap();

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::TwoWay)
		.await
		.unwrap();

	let report = engine.sync_once(pair).await.unwrap();
	assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
	assert_eq!(report.uploaded, 1, "the local-only file went up");
	assert_eq!(report.downloaded, 1, "the remote-only file came down");
	assert!(report.conflicts.is_empty(), "distinct paths never conflict");

	// Local now has both.
	assert!(local.join("from_local.txt").exists());
	assert_eq!(
		std::fs::read(local.join("from_remote.txt")).unwrap(),
		b"from remote"
	);
	// Remote now has both.
	let (_, files) = remote_listing(&cache.client, &resources.dir).await;
	assert!(
		files.contains(&"from_local.txt".to_string()),
		"files: {files:?}"
	);
	assert!(
		files.contains(&"from_remote.txt".to_string()),
		"files: {files:?}"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// A local rename is propagated as a remote MOVE (same uuid, no content re-upload), not a
/// trash + re-upload.
#[shared_test_runtime]
async fn local_rename_moves_the_remote_file_in_place() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	assert!(
		wait_for_converged_resync(
			&cache.messages,
			resources.dir.uuid(),
			0,
			CACHE_CONVERGE_TIMEOUT
		)
		.await
	);

	let local = temp_local_dir();
	std::fs::write(local.join("orig.txt"), b"move me without re-uploading").unwrap();

	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	assert_eq!(engine.sync_once(pair).await.unwrap().uploaded, 1);

	// Identify the uploaded file's uuid and wait for the cache to observe it.
	let (_, original_files) = cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	let original = original_files
		.into_iter()
		.find(|f| f.name().unwrap() == "orig.txt")
		.expect("orig.txt uploaded");
	let original_uuid: Uuid = original.uuid();
	assert!(poll_for_item(cache.db_path(), original_uuid, CACHE_CONVERGE_TIMEOUT).await);

	// Rename it locally, then sync again.
	std::fs::rename(local.join("orig.txt"), local.join("renamed.txt")).unwrap();
	let report = engine.sync_once(pair).await.unwrap();
	assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
	assert_eq!(report.moved_remote, 1, "a local rename is a remote move");
	assert_eq!(report.uploaded, 0, "content is NOT re-uploaded");

	// The remote shows exactly one file, now renamed.txt, with the SAME uuid (moved in place).
	let (_, renamed_files) = cache
		.client
		.list_dir(
			&DirType::<Normal>::Dir(Cow::Borrowed(&resources.dir)),
			None::<&fn(u64, Option<u64>)>,
		)
		.await
		.unwrap();
	assert_eq!(renamed_files.len(), 1, "still one file, not a duplicate");
	assert_eq!(renamed_files[0].name().unwrap(), "renamed.txt");
	assert_eq!(
		renamed_files[0].uuid(),
		original_uuid,
		"same uuid — the file was moved, not re-created"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// A remote rename is propagated as a local MOVE (rename on disk), not a delete + re-download.
#[shared_test_runtime]
async fn remote_rename_moves_the_local_file_in_place() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let builder = cache
		.client
		.make_file_builder("r_orig.txt", resources.dir.uuid())
		.unwrap();
	let mut remote_file = cache
		.client
		.upload_file(builder, b"remote move me")
		.await
		.unwrap();
	let uuid: Uuid = remote_file.uuid();
	assert!(poll_for_item(cache.db_path(), uuid, CACHE_CONVERGE_TIMEOUT).await);

	let local = temp_local_dir();
	let engine = SyncEngine::open(cache.client.clone(), temp_cache_path())
		.await
		.unwrap();
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::RemoteToLocal)
		.await
		.unwrap();
	assert_eq!(engine.sync_once(pair).await.unwrap().downloaded, 1);
	assert!(local.join("r_orig.txt").exists());

	// Rename the file on the remote and wait for the cache to reflect the new name.
	cache
		.client
		.update_file_metadata(
			&mut remote_file,
			FileMetaChanges::default().name("r_renamed.txt").unwrap(),
		)
		.await
		.unwrap();
	assert!(
		poll_for_file_name(
			cache.db_path(),
			uuid,
			"r_renamed.txt",
			CACHE_CONVERGE_TIMEOUT
		)
		.await,
		"cache should observe the remote rename"
	);

	let report = engine.sync_once(pair).await.unwrap();
	assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
	assert_eq!(report.moved_local, 1, "a remote rename is a local move");
	assert_eq!(report.downloaded, 0, "content is NOT re-downloaded");
	assert!(!local.join("r_orig.txt").exists(), "old name gone");
	assert_eq!(
		std::fs::read(local.join("r_renamed.txt")).unwrap(),
		b"remote move me",
		"renamed on disk, content intact"
	);

	std::fs::remove_dir_all(&local).ok();
}

/// The continuous engine: with a watch active, a file created locally is pushed to the remote on
/// its own (FS watcher -> debounce -> sync pass), no manual sync_once call.
#[shared_test_runtime]
async fn watch_pushes_a_new_local_file_automatically() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	assert!(
		wait_for_converged_resync(
			&cache.messages,
			resources.dir.uuid(),
			0,
			CACHE_CONVERGE_TIMEOUT
		)
		.await
	);

	let local = temp_local_dir();
	let engine = Arc::new(
		SyncEngine::open(cache.client.clone(), temp_cache_path())
			.await
			.unwrap(),
	);
	let pair = engine
		.add_pair(local.clone(), resources.dir.uuid(), SyncMode::LocalToRemote)
		.await
		.unwrap();
	let watch = engine.watch(pair).await.unwrap();

	// Create a file AFTER the watch is live; the engine should push it on its own.
	std::fs::write(local.join("watched.txt"), b"pushed by the watcher").unwrap();

	let deadline = tokio::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	let mut appeared = false;
	while tokio::time::Instant::now() < deadline {
		let (_, files) = remote_listing(&cache.client, &resources.dir).await;
		if files.contains(&"watched.txt".to_string()) {
			appeared = true;
			break;
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
	assert!(
		appeared,
		"the watcher should have pushed the new local file to the remote on its own"
	);

	drop(watch);
	std::fs::remove_dir_all(&local).ok();
}
