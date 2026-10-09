use std::{
	borrow::Cow,
	path::Path,
	sync::{Arc, Mutex},
	time::Duration,
};

use filen_macros::shared_test_runtime;
use filen_sdk_rs::cache::{
	CacheError, CacheEvent, CacheMessage, ResyncProgress, SyncRootCallback, SyncRootHandle,
};
use filen_sdk_rs::{
	auth::Client,
	fs::{HasParent, HasUUID, dir::meta::DirectoryMetaChanges, file::meta::FileMetaChanges},
	io::{RemoteDirectory, RemoteFile},
	socket::{DecryptedDriveEvent, DecryptedSocketEvent, FileNew, FolderSubCreated},
};
use filen_types::{api::v3::dir::color::DirColor, fs::ParentUuid, traits::CowHelpersExt};
use rusqlite::params;
use uuid::Uuid;

mod helpers;
use helpers::*;

#[shared_test_runtime]
async fn test_cache_init_creates_schema() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let conn = open_read_db(cache.db_path()).unwrap();

	let tables: Vec<String> = conn
		.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
		.unwrap()
		.query_map([], |row| row.get(SQLITE_MASTER_NAME))
		.unwrap()
		.collect::<Result<_, _>>()
		.unwrap();

	assert!(
		tables.contains(&"items".to_string()),
		"items table should exist"
	);
	assert!(
		tables.contains(&"roots".to_string()),
		"roots table should exist"
	);
	assert!(
		tables.contains(&"files".to_string()),
		"files table should exist"
	);
	assert!(
		tables.contains(&"dirs".to_string()),
		"dirs table should exist"
	);
	assert!(
		tables.contains(&"events".to_string()),
		"events table should exist"
	);
	assert!(
		tables.contains(&"cache_meta".to_string()),
		"cache_meta table should exist"
	);
}

#[shared_test_runtime]
async fn test_cache_init_inserts_root() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	// The ACCOUNT root's row is written by init_db itself, independent of the sync scope.
	let root_uuid: Uuid = resources.client.root().uuid();

	let item_type = query_item_type(cache.db_path(), root_uuid);
	assert_eq!(item_type, Some(0), "root should be type 0 (Root)");

	let conn = open_read_db(cache.db_path()).unwrap();
	let root_exists: bool = conn
		.query_row(
			"SELECT COUNT(*) > 0 AS item_exists FROM roots r JOIN items i ON i.id = r.id \
			 WHERE i.uuid = ?",
			params![root_uuid],
			|row| row.get(ITEM_EXISTS),
		)
		.unwrap();
	assert!(root_exists, "root should exist in roots table");
}

#[shared_test_runtime]
async fn test_cache_file_new_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_new.txt", test_dir.uuid())
		.unwrap();
	let file = client
		.upload_file(file, b"cache test content")
		.await
		.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache after FileNew event"
	);

	let cached = query_cached_file(cache.db_path(), file_uuid);
	assert!(cached.is_some(), "file metadata should be queryable");
	let (name, size, mime, _parent) = cached.unwrap();
	assert_eq!(name, "cache_file_new.txt");
	assert_eq!(size, 18); // b"cache test content".len()
	assert_eq!(mime, "text/plain");

	assert_eq!(query_item_type(cache.db_path(), file_uuid), Some(2));
}

#[shared_test_runtime]
async fn test_cache_file_trash_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_trash.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"to be trashed").await.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache before trashing"
	);

	client.trash_file(&mut file).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should be removed from cache after FileTrash event"
	);
}

#[shared_test_runtime]
async fn test_cache_multiple_file_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let mut file_uuids = Vec::new();

	for i in 0..3 {
		let name = format!("cache_multi_{i}.txt");
		let content = format!("content {i}");
		let spec = client.make_file_builder(&name, test_dir.uuid()).unwrap();
		let file = client.upload_file(spec, content.as_bytes()).await.unwrap();
		file_uuids.push(file.uuid());
	}

	for (i, uuid) in file_uuids.iter().enumerate() {
		assert!(
			poll_for_item(cache.db_path(), *uuid, Duration::from_secs(30)).await,
			"file {i} should appear in cache"
		);
	}

	for (i, uuid) in file_uuids.iter().enumerate() {
		let (name, _, _, _) = query_cached_file(cache.db_path(), *uuid).unwrap();
		assert_eq!(name, format!("cache_multi_{i}.txt"));
	}
}

#[shared_test_runtime]
async fn test_cache_dir_new_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let dir = client
		.create_dir(&test_dir.into(), "cache_dir_new")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"directory should appear in cache after FolderSubCreated event"
	);

	let cached = query_cached_dir(cache.db_path(), dir_uuid);
	assert!(cached.is_some(), "dir metadata should be queryable");
	let (name, _color, _parent) = cached.unwrap();
	assert_eq!(name, "cache_dir_new");

	assert_eq!(query_item_type(cache.db_path(), dir_uuid), Some(1));
}

#[shared_test_runtime]
async fn test_cache_dir_trash_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_trash")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache before trashing"
	);

	client.trash_dir(&mut dir).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should be removed from cache after FolderTrash event"
	);
}

#[shared_test_runtime]
async fn test_cache_file_move_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_move.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"moveable content").await.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	let target_dir = client
		.create_dir(&test_dir.into(), "cache_move_target")
		.await
		.unwrap();
	let target_dir_uuid: Uuid = target_dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), target_dir_uuid, Duration::from_secs(30)).await,
		"target dir should appear in cache"
	);

	client
		.move_file(&mut file, &(&target_dir).into())
		.await
		.unwrap();

	// The file must still exist after the move (re-parented, not removed). The new parent is a
	// NESTED dir, so under an account-wide event shed the move can hit the membership gap
	// (target_dir momentarily uncached) and the file is transiently deleted, then restored by the
	// self-heal resync — so wait up to the convergence bound, not a fixed few seconds.
	assert!(
		poll_for_item(cache.db_path(), file_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"file should still exist in cache after move event"
	);
}

#[shared_test_runtime]
async fn test_cache_dir_move_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let mut move_dir = client
		.create_dir(&test_dir.into(), "cache_dir_to_move")
		.await
		.unwrap();
	let move_dir_uuid: Uuid = move_dir.uuid();

	let target_dir = client
		.create_dir(&test_dir.into(), "cache_dir_move_target")
		.await
		.unwrap();
	let target_dir_uuid: Uuid = target_dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), move_dir_uuid, Duration::from_secs(30)).await,
		"source dir should appear in cache"
	);
	assert!(
		poll_for_item(cache.db_path(), target_dir_uuid, Duration::from_secs(30)).await,
		"target dir should appear in cache"
	);

	client
		.move_dir(&mut move_dir, &(&target_dir).into())
		.await
		.unwrap();

	// As for the file move: the new parent is a NESTED dir, so under an account-wide event shed
	// the move can hit the membership gap (target_dir momentarily uncached) and the moved dir is
	// transiently deleted, then restored by the self-heal resync — wait up to the convergence
	// bound rather than a fixed window.
	assert!(
		poll_for_item(cache.db_path(), move_dir_uuid, CACHE_CONVERGE_TIMEOUT).await,
		"moved dir should still exist in cache"
	);
}

#[shared_test_runtime]
async fn test_cache_list_dir_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	// Injected synthetic items are absent-from-server fiction: wait for the initial populate
	// resync to converge FIRST, or its convergence diff correctly wipes them mid-test.
	assert!(
		wait_for_converged_resync(&cache.messages, test_dir_uuid, 0, CACHE_CONVERGE_TIMEOUT).await,
		"the initial populate resync should converge before synthetic injection"
	);

	let dirs: Vec<RemoteDirectory> = (0..3)
		.map(|i| make_test_remote_dir(&format!("ldr_dir_{i}"), test_dir_uuid))
		.collect();
	let files: Vec<RemoteFile> = (0..5)
		.map(|i| make_test_remote_file(&format!("ldr_file_{i}.txt"), test_dir_uuid))
		.collect();

	let dir_uuids: Vec<Uuid> = dirs.iter().map(|d| d.uuid()).collect();
	let file_uuids: Vec<Uuid> = files.iter().map(|f| f.uuid()).collect();

	cache
		.handle
		.update_list_dir_recursive(dirs, files)
		.await
		.unwrap();

	for (i, uuid) in dir_uuids.iter().enumerate() {
		assert!(
			poll_for_item(cache.db_path(), *uuid, Duration::from_secs(10)).await,
			"synthetic dir {i} should appear in cache via ListDirRecursive"
		);
	}
	for (i, uuid) in file_uuids.iter().enumerate() {
		assert!(
			poll_for_item(cache.db_path(), *uuid, Duration::from_secs(10)).await,
			"synthetic file {i} should appear in cache via ListDirRecursive"
		);
	}

	let (name, _, _) = query_cached_dir(cache.db_path(), dir_uuids[0]).unwrap();
	assert_eq!(name, "ldr_dir_0");

	let (name, size, mime, _) = query_cached_file(cache.db_path(), file_uuids[0]).unwrap();
	assert_eq!(name, "ldr_file_0.txt");
	assert_eq!(size, 1024);
	assert_eq!(mime, "text/plain");
}

#[shared_test_runtime]
async fn test_cache_list_dir_recursive_large_batch() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	// See test_cache_list_dir_recursive: converge the populate resync before injecting fiction.
	assert!(
		wait_for_converged_resync(&cache.messages, test_dir_uuid, 0, CACHE_CONVERGE_TIMEOUT).await,
		"the initial populate resync should converge before synthetic injection"
	);

	let dirs: Vec<RemoteDirectory> = (0..50)
		.map(|i| make_test_remote_dir(&format!("ldr_batch_dir_{i}"), test_dir_uuid))
		.collect();
	let files: Vec<RemoteFile> = (0..100)
		.map(|i| make_test_remote_file(&format!("ldr_batch_file_{i}.txt"), test_dir_uuid))
		.collect();

	let last_dir_uuid: Uuid = dirs.last().unwrap().uuid();
	let last_file_uuid: Uuid = files.last().unwrap().uuid();

	cache
		.handle
		.update_list_dir_recursive(dirs, files)
		.await
		.unwrap();

	assert!(
		poll_for_item(cache.db_path(), last_dir_uuid, Duration::from_secs(15)).await,
		"last dir in batch should appear in cache"
	);
	assert!(
		poll_for_item(cache.db_path(), last_file_uuid, Duration::from_secs(15)).await,
		"last file in batch should appear in cache"
	);

	// root + 50 dirs + 100 files = 151
	let total = count_items(cache.db_path());
	assert!(total >= 151, "expected at least 151 items, got {total}");
}

#[shared_test_runtime]
async fn test_cache_shutdown_on_drop() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	{
		let _handle = client
			.clone()
			.add_sync_root(resources.dir.uuid(), noop_sync_root_callback())
			.await
			.unwrap();
	}
	// Dropping the last handle shuts the worker down on its own; `flush_cache` joins it so the DB
	// is deterministically closed before we read it.
	client.flush_cache().await;

	assert!(path.exists(), "DB file should persist after cache drop");

	let conn = open_read_db(&path).unwrap();
	let root_uuid: Uuid = client.root().uuid();
	let root_exists: bool = conn
		.query_row(
			"SELECT COUNT(*) > 0 AS item_exists FROM items WHERE uuid = ? AND type = 0",
			params![root_uuid],
			|row| row.get(ITEM_EXISTS),
		)
		.unwrap();
	assert!(
		root_exists,
		"root should still be in DB after cache shutdown"
	);
}

#[shared_test_runtime]
async fn test_cache_reopen_preserves_data() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let path = temp_cache_path();
	let test_dir_uuid = resources.dir.uuid();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	let dir_uuid: Uuid;
	{
		let _handle = client
			.clone()
			.add_sync_root(test_dir_uuid, noop_sync_root_callback())
			.await
			.unwrap();
		ensure_socket_ready(&client).await;
		// A REAL child created AFTER the listener is live: the FolderSubCreated socket event
		// populates the cache (no drive lock needed — robust under suite-wide lock contention,
		// unlike the convergence resync), and real data survives every later resync, unlike a
		// synthetic `update_list_dir_recursive` injection (which `diff_subtree_absent` correctly
		// wipes as absent-from-server on the next resync of this scoped root).
		let dir = client
			.create_dir(&(&resources.dir).into(), "reopen_test_dir")
			.await
			.unwrap();
		dir_uuid = dir.uuid();
		assert!(
			poll_for_item(&path, dir_uuid, Duration::from_secs(60)).await,
			"the FolderSubCreated event should cache the child"
		);
	}
	client.flush_cache().await;

	// Reopen the DB FILE directly — no worker, so no resync runs to wipe or re-fetch. The row
	// being present proves the flush persisted it to the file.
	assert!(
		poll_for_item(&path, dir_uuid, Duration::from_secs(5)).await,
		"data from the previous session should persist in the reopened DB file"
	);
	let (name, _, _) = query_cached_dir(&path, dir_uuid).unwrap();
	assert_eq!(name, "reopen_test_dir");
}

/// App close/resume: the cache catches up on changes that happened while it was offline. After a clean
/// `shutdown()`, reopening the SAME DB runs the startup gap-check; because the remote drive id advanced
/// (a dir was created while no cache was running), it resyncs and the offline-created dir appears —
/// even though no socket event for it is ever delivered to the second session.
///
/// The negative case (drive id unchanged → no resync) is covered deterministically by the unit test
/// `startup_should_resync_gates_on_drive_id_advance`.
#[shared_test_runtime]
async fn test_cache_resyncs_on_restart_after_offline_change() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let test_dir = &resources.dir;
	let test_dir_uuid: Uuid = test_dir.uuid();
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();

	// Session 1: a fresh cache. The startup gap-check (watermark None, remote drive id > 0) resyncs and
	// populates the cache from the account listing, so the existing test dir shows up.
	{
		let since = messages_len(&messages);
		let _handle = client
			.clone()
			.add_sync_root(test_dir_uuid, noop_sync_root_callback())
			.await
			.unwrap();
		// Wait on the convergence SIGNAL, not a fixed item-poll window: under drive-lock contention
		// the resync can legitimately take longer than any fixed window, and giving up early is the
		// flakiness we are removing. Once it reports converged, the row is already committed.
		assert!(
			wait_for_converged_resync(&messages, test_dir_uuid, since, CACHE_CONVERGE_TIMEOUT)
				.await,
			"the populate resync should converge"
		);
		assert!(
			poll_for_item(&path, test_dir_uuid, Duration::from_secs(5)).await,
			"the populate resync should materialize the scope root"
		);
	}
	client.flush_cache().await; // clean flush + join before reopening the same DB file

	// Offline change: create a dir while NO cache is running, advancing the remote drive id.
	let mut offline_dir = client
		.create_dir(&test_dir.into(), "cache_restart_resync")
		.await
		.unwrap();
	let offline_uuid: Uuid = offline_dir.uuid();

	// Session 2: re-adding a sync root respawns the worker on the same DB. The startup gap-check sees
	// the advanced drive id and resyncs, so the dir created while we were offline appears — with no
	// socket delivery involved.
	{
		let since = messages_len(&messages);
		let _handle = client
			.clone()
			.add_sync_root(test_dir_uuid, noop_sync_root_callback())
			.await
			.unwrap();
		// `since` skips session 1's convergence (the log persists across the restart), so this waits
		// for session 2's gap-check resync of the same root, then reads the offline-created child.
		assert!(
			wait_for_converged_resync(&messages, test_dir_uuid, since, CACHE_CONVERGE_TIMEOUT)
				.await,
			"the restart resync should converge"
		);
		assert!(
			poll_for_item(&path, offline_uuid, Duration::from_secs(5)).await,
			"restart resync should catch up the dir created while the cache was offline"
		);
	}
	client.flush_cache().await;

	let _ = client.trash_dir(&mut offline_dir).await;
}

/// Whether a progress message from index `since` on shows `root`'s subtree being listed: a
/// `Listing` tick for it (native always sends a final one per listing) or an `Applying`.
fn listed_since(log: &MessageLog, root: Uuid, since: usize) -> bool {
	log.lock().unwrap().iter().skip(since).any(|msg| match msg {
		CacheMessage::ResyncProgress(ResyncProgress::Applying) => true,
		CacheMessage::ResyncProgress(ResyncProgress::Listing { root: listed, .. }) => {
			*listed == root
		}
		_ => false,
	})
}

/// The drive id the cache DB at `path` holds the drive at (its watermark).
fn cached_watermark(path: &Path) -> u64 {
	let watermark: i64 = open_read_db(path)
		.unwrap()
		.query_row(
			"SELECT value FROM cache_meta WHERE meta_key = 'last_drive_message_id'",
			[],
			|row| row.get(0),
		)
		.unwrap();
	u64::try_from(watermark).unwrap()
}

/// One cache session on `root`: add it to the cache on `client` and wait until the add converges.
/// Returns the handle, whose drop ends the session, and the index of the session's first message.
async fn open_session(
	client: &Arc<Client>,
	root: Uuid,
	messages: &MessageLog,
) -> (SyncRootHandle, usize) {
	let since = messages_len(messages);
	let handle = client
		.clone()
		.add_sync_root(root, noop_sync_root_callback())
		.await
		.unwrap();
	assert!(
		wait_for_converged_resync(messages, root, since, CACHE_CONVERGE_TIMEOUT).await,
		"the add converges"
	);
	(handle, since)
}

/// End a session from [`open_session`]: drop its handle and join the worker, so the DB is closed
/// before the next session opens it.
async fn close_session(client: &Client, session: SyncRootHandle) {
	drop(session);
	client.flush_cache().await;
}

/// Wait until the cache at `path` holds the drive at least at the remote drive id as read now, which
/// the events of every write made so far bring it to. Returns that id.
async fn wait_until_caught_up(client: &Client, path: &Path) -> u64 {
	let remote = client.get_last_event_ids().await.unwrap().drive;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || cached_watermark(path) >= remote).await,
		"the cache catches up with drive id {remote}, still at {}",
		cached_watermark(path)
	);
	remote
}

/// Whether every resync whose start is logged from index `since` on has finished.
fn no_resync_in_flight(log: &MessageLog, since: usize) -> bool {
	let mut in_flight = false;
	for msg in log.lock().unwrap().iter().skip(since) {
		match msg {
			CacheMessage::ResyncProgress(ResyncProgress::Started { .. }) => in_flight = true,
			CacheMessage::ResyncProgress(ResyncProgress::Finished { .. }) => in_flight = false,
			_ => {}
		}
	}
	!in_flight
}

/// Let a session's startup check settle before the writes a test is about. Once its socket
/// subscribes, the worker reads the remote drive id and lists every root if the drive is ahead of
/// what it holds, so a write landing before that read turns into a listing. This makes one write
/// under `parent`, waits until the cache has it and no listing runs, and returns the index of the
/// next message: the check runs once per session, so nothing after it lists for that reason.
async fn settle_session_start(
	client: &Client,
	parent: &RemoteDirectory,
	path: &Path,
	messages: &MessageLog,
) -> usize {
	ensure_socket_ready(client).await;
	let since = messages_len(messages);
	client
		.create_dir(&parent.into(), &format!("settle-{}", Uuid::new_v4()))
		.await
		.unwrap();
	wait_until_caught_up(client, path).await;
	assert!(
		poll_until(CACHE_CONVERGE_TIMEOUT, || no_resync_in_flight(
			messages, since
		))
		.await,
		"a listing the settling write set off finishes"
	);
	messages_len(messages)
}

/// Make the dir `name` under `parent` with a file in it, the way clients do: the dir, then an
/// upload into it.
async fn create_dir_with_file(
	client: &Client,
	parent: &RemoteDirectory,
	name: &str,
) -> (RemoteDirectory, RemoteFile) {
	let dir = client.create_dir(&parent.into(), name).await.unwrap();
	let file = client.make_file_builder("inside.txt", dir.uuid()).unwrap();
	let file = client.upload_file(file, b"inside").await.unwrap();
	(dir, file)
}

/// Reopening a cache whose sync root an earlier session listed whole confirms the root with one
/// drive-id read instead of listing it again, and a change made while no cache ran still makes the
/// next add list it and pick the change up.
///
/// The account's drive id moves with every write anyone makes, so the test holds the drive-write
/// lock throughout, taken by the very client the cache runs on: the cache's own resyncs and this
/// test's write share it in-process, while every other test's drive write waits for it. A favorite
/// toggle does not take the lock; the test checks the drive id did not move rather than assume it.
#[shared_test_runtime]
async fn test_cache_reopen_confirms_a_listed_root_without_listing_it() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let root: Uuid = resources.dir.uuid();
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();
	let lock = client.lock_drive().await.unwrap();

	// Session 1: the first add lists the root.
	let (session, since) = open_session(&client, root, &messages).await;
	close_session(&client, session).await;
	assert!(
		listed_since(&messages, root, since),
		"the first add lists the root"
	);

	// Session 2, nothing changed remotely: the add converges without a listing.
	let held_at = cached_watermark(&path);
	let (session, since) = open_session(&client, root, &messages).await;
	close_session(&client, session).await;
	assert_eq!(
		client.get_last_event_ids().await.unwrap().drive,
		held_at,
		"the drive id moved during the re-add (a write that skips the drive lock, such as a \
		 concurrent test's favorite toggle), so whether it listed proves nothing"
	);
	assert!(
		!listed_since(&messages, root, since),
		"a root held whole at an unmoved drive id is not listed again: {:?}",
		&messages.lock().unwrap()[since..]
	);

	// A change while no cache runs moves the drive id past the cache's watermark.
	let made_offline = client
		.create_dir(&(&resources.dir).into(), "made_while_closed")
		.await
		.unwrap();

	// Session 3: the add lists the root and picks the change up.
	let (session, since) = open_session(&client, root, &messages).await;
	assert!(
		poll_for_item(&path, made_offline.uuid(), Duration::from_secs(5)).await,
		"the dir made while no cache ran is cached"
	);
	close_session(&client, session).await;
	assert!(
		listed_since(&messages, root, since),
		"the drive moved, so the add lists the root"
	);
	drop(lock);
}

/// A dir made, with a file in it, under a root the cache holds whole while the cache runs leaves the
/// root held whole: the socket announces the dir alone, and the file in an event of its own after
/// it, so the next session confirms the root without listing it and holds both.
///
/// The drive lock keeps the drive id still between sessions, as in the test above.
#[shared_test_runtime]
async fn test_cache_reopen_confirms_a_listed_root_a_dir_was_made_in() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let root: Uuid = resources.dir.uuid();
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();
	let lock = client.lock_drive().await.unwrap();

	let (session, _) = open_session(&client, root, &messages).await;
	close_session(&client, session).await;

	// Session 2: the dir and its file arrive from the socket while the cache runs.
	let (session, _) = open_session(&client, root, &messages).await;
	let since = settle_session_start(&client, &resources.dir, &path, &messages).await;
	let (sender, mut announced) = tokio::sync::mpsc::unbounded_channel();
	let _listener = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = sender.send(event.to_owned_cow());
			}),
			Some(vec![
				Cow::Borrowed("folderSubCreated"),
				Cow::Borrowed("fileNew"),
			]),
		)
		.await
		.unwrap();
	let (made, file) = create_dir_with_file(&client, &resources.dir, "made_while_open").await;
	let held_at = wait_until_caught_up(&client, &path).await;
	close_session(&client, session).await;
	assert!(
		!listed_since(&messages, root, since),
		"the dir and its file came from the socket, not a listing: {:?}",
		&messages.lock().unwrap()[since..]
	);

	let mut order = Vec::new();
	while order.len() < 2 {
		let event = tokio::time::timeout(Duration::from_secs(30), announced.recv())
			.await
			.expect("both events are announced")
			.expect("the listener is registered");
		match event {
			DecryptedSocketEvent::Drive {
				inner: DecryptedDriveEvent::FolderSubCreated(FolderSubCreated(dir)),
				..
			} if dir.uuid() == made.uuid() => order.push("folderSubCreated"),
			DecryptedSocketEvent::Drive {
				inner: DecryptedDriveEvent::FileNew(FileNew(new)),
				..
			} if new.uuid() == file.uuid() => {
				assert_eq!(new.parent(), &ParentUuid::Uuid(made.uuid()));
				order.push("fileNew");
			}
			_ => {}
		}
	}
	assert_eq!(
		order,
		["folderSubCreated", "fileNew"],
		"the dir is announced alone, before the file in it"
	);

	// Session 3: the add confirms the root without listing it.
	let (session, since) = open_session(&client, root, &messages).await;
	close_session(&client, session).await;
	assert_eq!(
		client.get_last_event_ids().await.unwrap().drive,
		held_at,
		"the drive id moved during the re-add, so whether it listed proves nothing"
	);
	assert!(
		!listed_since(&messages, root, since),
		"a dir made under a root held whole leaves it held whole: {:?}",
		&messages.lock().unwrap()[since..]
	);
	assert!(query_cached_dir(&path, made.uuid()).is_some());
	assert!(query_cached_file(&path, file.uuid()).is_some());
	drop(lock);
}

/// A dir restored from the trash under a root the cache holds whole comes back without the file
/// the trash took out of the cache, and no event brings it back, so the next session lists the
/// root, which does.
///
/// The drive lock keeps the drive id still between sessions, so the listing is down to the
/// restore and not to a moved drive.
#[shared_test_runtime]
async fn test_cache_reopen_lists_a_listed_root_a_dir_was_restored_in() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let root: Uuid = resources.dir.uuid();
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();
	// The trash lock (see the file restore test) before the drive lock, which a trash takes too:
	// a test holding the trash lock waits for the drive lock, never the other way round.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let lock = client.lock_drive().await.unwrap();

	let (session, _) = open_session(&client, root, &messages).await;
	close_session(&client, session).await;

	// Session 2: a dir with a file in it goes to the trash and comes back.
	let (session, _) = open_session(&client, root, &messages).await;
	settle_session_start(&client, &resources.dir, &path, &messages).await;
	let (mut restored, file) =
		create_dir_with_file(&client, &resources.dir, "restored_while_open").await;
	wait_until_caught_up(&client, &path).await;
	assert!(query_cached_file(&path, file.uuid()).is_some());
	client.trash_dir(&mut restored).await.unwrap();
	client.restore_dir(&mut restored).await.unwrap();
	// For a moment after it answers the restore, the server lists the dir without what was in it.
	// Wait until a listing of the root, read the way session 3's add reads it but by a client with
	// no cache, has the file back in the dir.
	let deadline = tokio::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	loop {
		let (_, files) = resources
			.client
			.list_dir_recursive::<_, fn(u64, Option<u64>)>(&(&resources.dir).into(), None, ())
			.await
			.unwrap();
		if files.iter().any(|listed| {
			listed.uuid() == file.uuid() && *listed.parent() == ParentUuid::Uuid(restored.uuid())
		}) {
			break;
		}
		assert!(
			tokio::time::Instant::now() < deadline,
			"the server lists {} in the restored dir",
			file.uuid()
		);
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	let held_at = wait_until_caught_up(&client, &path).await;
	assert!(
		query_cached_dir(&path, restored.uuid()).is_some(),
		"the restored dir is back"
	);
	assert!(
		query_cached_file(&path, file.uuid()).is_none(),
		"the file in it is not"
	);
	close_session(&client, session).await;

	// Session 3: the add lists the root, which brings the file back.
	let (session, since) = open_session(&client, root, &messages).await;
	assert!(
		query_cached_file(&path, file.uuid()).is_some(),
		"the listing brings the file back"
	);
	close_session(&client, session).await;
	assert_eq!(
		client.get_last_event_ids().await.unwrap().drive,
		held_at,
		"the drive id moved during the re-add, so whether it listed proves nothing"
	);
	assert!(
		listed_since(&messages, root, since),
		"a dir restored under a root held whole makes its next add list it"
	);
	drop(lock);
}

/// A sync root the server no longer has, gone while the cache is closed: registrations do not
/// survive a worker restart, so re-adding the root on reopen runs the `add_sync_root` validation —
/// the server answers not-found and the add is REJECTED with `CacheError::InvalidSyncRoot` (the bad
/// key never re-enters the active set) AND the stale subtree the prior session cached under the
/// root is wiped. The stale handle from the previous session stays inert and its drop is a harmless
/// no-op. (The LIVE not-found classification inside a running resync — drop + wipe +
/// `SyncRootsDeleted` — is covered deterministically by the `finalize_resync` unit tests.)
///
/// The gone root is a uuid the server never had, injected into the cache: `/v3/dir` answers a
/// permanently deleted dir as trashed until the backend purges it, which has no bound (it once took
/// over 300 s), and answers a uuid it never had with the same `folder_not_found` as a purged one.
#[shared_test_runtime]
async fn test_cache_re_add_of_permanently_deleted_sync_root_is_rejected() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();

	// Session 1: a real dir is the sole sync root, and the cache holds the gone root under it, with
	// a child, and a sibling of the gone root.
	let live = client
		.create_dir(&(&resources.dir).into(), "cache_readd_live")
		.await
		.unwrap();
	let (stale_handle, _) = open_session(&client, live.uuid(), &messages).await;
	// A listing of `live` wipes the injected dirs, so the session's startup check goes first.
	settle_session_start(&client, &live, &path, &messages).await;
	let gone = make_test_remote_dir("cache_readd_gone_root", live.uuid());
	let child = make_test_remote_dir("child", gone.uuid());
	let sibling = make_test_remote_dir("sibling", live.uuid());
	let (gone_uuid, child_uuid, sibling_uuid) = (gone.uuid(), child.uuid(), sibling.uuid());
	stale_handle
		.update_list_dir_recursive(vec![gone, child, sibling], vec![])
		.await
		.unwrap();
	for uuid in [gone_uuid, child_uuid, sibling_uuid] {
		assert!(
			poll_for_item(&path, uuid, Duration::from_secs(10)).await,
			"the injected dir {uuid} should be cached"
		);
	}
	client.flush_cache().await;

	// Session 2: the app re-adds the root it has not yet learned is gone — validation rejects it.
	let err = client
		.clone()
		.add_sync_root(gone_uuid, noop_sync_root_callback())
		.await
		.expect_err("re-adding a sync root the server no longer has must be rejected");
	assert!(
		matches!(
			err.downcast_ref::<CacheError>(),
			Some(CacheError::InvalidSyncRoot { uuid, .. }) if *uuid == gone_uuid
		),
		"the rejection should carry CacheError::InvalidSyncRoot for the gone root, got {err:?}"
	);

	// The definitive not-found also wiped the stale subtree session 1 cached under the root —
	// without it those rows would be stranded forever (membership-gated out of live events,
	// anchored by no resync diff), serving deleted content to any DB reader.
	assert!(
		poll_for_item_absent(&path, gone_uuid, Duration::from_secs(30)).await,
		"the gone root's stale row must be wiped by the rejected re-add"
	);
	assert!(
		poll_for_item_absent(&path, child_uuid, Duration::from_secs(30)).await,
		"the gone root's stale subtree must be cascade-wiped by the rejected re-add"
	);
	// A listing of `live` would have taken the sibling too, so it staying shows the rows went with
	// the rejected re-add.
	assert!(
		query_item_type(&path, sibling_uuid).is_some(),
		"the gone root's sibling must survive the rejected re-add"
	);

	// The stale session-1 handle is inert (its worker is gone); dropping it must be a no-op.
	drop(stale_handle);
	client.flush_cache().await;
}

/// A sync root trashed while the cache is closed. `/v3/dir` still answers for a trashed dir, with
/// the trash as its parent, so re-adding the root on reopen must read that answer the way the live
/// `folderTrash` is read: the add is REJECTED with `CacheError::InvalidSyncRoot` and the subtree the
/// prior session cached under the root is wiped. Accepted, the root would sit in the active set with
/// a node no resync can store (its parent is the trash) and its stale rows would never go.
#[shared_test_runtime]
async fn test_cache_re_add_of_trashed_sync_root_is_rejected() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();

	// Session 1: a real dir with a child dir is the sole sync root, and the add caches both.
	let mut trashed = client
		.create_dir(&(&resources.dir).into(), "cache_readd_trashed_root")
		.await
		.unwrap();
	let child = client
		.create_dir(&(&trashed).into(), "child")
		.await
		.unwrap();
	let (session, _) = open_session(&client, trashed.uuid(), &messages).await;
	for uuid in [trashed.uuid(), child.uuid()] {
		assert!(
			poll_for_item(&path, uuid, CACHE_CONVERGE_TIMEOUT).await,
			"the add caches {uuid}"
		);
	}
	close_session(&client, session).await;

	// Another client trashes the root while no session is listening. The trash lock keeps another
	// leg's account-wide empty-trash from purging it before session 2 reads it as trashed.
	let _trash_lock = resources
		.client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	resources.client.trash_dir(&mut trashed).await.unwrap();

	// Session 2: the app re-adds the root it has not yet learned is trashed, and validation rejects it.
	let err = client
		.clone()
		.add_sync_root(trashed.uuid(), noop_sync_root_callback())
		.await
		.expect_err("re-adding a sync root that is in the trash must be rejected");
	assert!(
		matches!(
			err.downcast_ref::<CacheError>(),
			Some(CacheError::InvalidSyncRoot { uuid, .. }) if *uuid == trashed.uuid()
		),
		"the rejection should carry CacheError::InvalidSyncRoot for the trashed root, got {err:?}"
	);
	assert!(
		poll_for_item_absent(&path, trashed.uuid(), Duration::from_secs(30)).await,
		"the trashed root's stale row must be wiped by the rejected re-add"
	);
	assert!(
		poll_for_item_absent(&path, child.uuid(), Duration::from_secs(30)).await,
		"the trashed root's stale subtree must be cascade-wiped by the rejected re-add"
	);
	client.flush_cache().await;

	resources
		.client
		.delete_dir_permanently(trashed)
		.await
		.unwrap();
}

/// Whether a resync that lists `root` has run to its end from index `since` on, and if so whether it
/// converged: a `Started` naming it, the `Applying` its listing ends with, then its `Finished`.
fn listing_resync_outcome(log: &MessageLog, root: Uuid, since: usize) -> Option<bool> {
	let mut covers_root = false;
	let mut applied = false;
	for msg in log.lock().unwrap().iter().skip(since) {
		match msg {
			CacheMessage::ResyncProgress(ResyncProgress::Started { roots }) => {
				covers_root = roots.contains(&root);
				applied = false;
			}
			CacheMessage::ResyncProgress(ResyncProgress::Applying) => applied = covers_root,
			CacheMessage::ResyncProgress(ResyncProgress::Finished { converged }) if applied => {
				return Some(*converged);
			}
			_ => {}
		}
	}
	None
}

/// The dirs [`check_resync_drops_roots_in_an_unheard_trash`] can register: `trashed` goes to the
/// trash, and `nested` is in it.
struct TrashedTree {
	trashed: Uuid,
	nested: Uuid,
}

/// Sync roots in a dir trashed while the cache does not hear it. `/v3/dir` still answers for a dir in
/// the trash, and for one anywhere under a trashed dir, with the trash as its parent, so the resync
/// must read that answer the way the live `folderTrash` is read: it drops each root, wipes the subtree
/// cached under it and reports it in `SyncRootsDeleted`, and it converges. Read as live, a root's
/// listing fails (`folder_inside_trash`), so every resync skips it as unreachable and retries it, and
/// its rows stay. A root directly inside another one the resync drops goes with it, even when its own
/// reads still answer as live.
///
/// The tree is `name/trashed/nested/inner`, and `pick` chooses the roots among `trashed` and
/// `nested`, outermost first. `trashed` goes to the trash while no cache runs, and the roots are
/// registered again through the covered fast path, which does not validate: the previous session's
/// rows still link them to `name`, added as a root just before them. Evicting it then leaves them
/// the only roots, so no listing of an ancestor takes them out. The drive lock, held by another
/// client than the cache's, keeps every resync from listing until then, so the first one to list
/// reads the picked roots alone.
async fn check_resync_drops_roots_in_an_unheard_trash(
	name: &str,
	pick: impl FnOnce(&TrashedTree) -> Vec<Uuid>,
) {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let path = temp_cache_path();
	let (messages, status_cb) = capturing_status_callback();
	client
		.configure_cache(path.clone(), status_cb)
		.await
		.unwrap();

	// Session 1: the parent is the sync root, and its add caches the dir to trash and the dirs in it.
	let parent = client
		.create_dir(&(&resources.dir).into(), name)
		.await
		.unwrap();
	let mut trashed = client
		.create_dir(&(&parent).into(), "trashed")
		.await
		.unwrap();
	let nested = client
		.create_dir(&(&trashed).into(), "nested")
		.await
		.unwrap();
	let inner = client.create_dir(&(&nested).into(), "inner").await.unwrap();
	let (session, _) = open_session(&client, parent.uuid(), &messages).await;
	for uuid in [trashed.uuid(), nested.uuid(), inner.uuid()] {
		assert!(
			poll_for_item(&path, uuid, CACHE_CONVERGE_TIMEOUT).await,
			"the add caches {uuid}"
		);
	}
	close_session(&client, session).await;
	let mut roots = pick(&TrashedTree {
		trashed: trashed.uuid(),
		nested: nested.uuid(),
	});
	let outermost = roots[0];
	roots.sort_unstable();

	// The dir goes to the trash while no session is listening. The trash lock keeps another leg's
	// account-wide empty-trash from purging it before the resync reads it; taken first, as the
	// drive lock the trash also takes is shared in-process with the one held here.
	let _trash_lock = resources
		.client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	let drive_lock = resources.client.lock_drive().await.unwrap();
	resources.client.trash_dir(&mut trashed).await.unwrap();
	// For a moment after a trash the server can still answer for a dir under the trashed one as
	// live, and a root reached only through its own reads cannot be told apart from a live one then.
	// Wait until the cache's client reads the outermost root in the trash; what lies inside it is
	// left to the resync.
	let deadline = tokio::time::Instant::now() + CACHE_CONVERGE_TIMEOUT;
	while !client.get_dir(outermost).await.unwrap().parent().is_trash() {
		assert!(
			tokio::time::Instant::now() < deadline,
			"the server answers for {outermost} in the trash"
		);
		tokio::time::sleep(Duration::from_millis(50)).await;
	}

	// Session 2: the parent's add acks before its resync, which waits for the drive lock. The picked
	// dirs' cached rows hang under the now active parent, so their adds register them unchecked.
	let since = messages_len(&messages);
	let parent_session = client
		.clone()
		.add_sync_root(parent.uuid(), noop_sync_root_callback())
		.await
		.unwrap();
	let mut sessions = Vec::with_capacity(roots.len());
	for root in &roots {
		sessions.push(
			client
				.clone()
				.add_sync_root(*root, noop_sync_root_callback())
				.await
				.expect("the covered add registers a dir in the trash without validating it"),
		);
	}
	parent_session.evict().await.unwrap();
	drop(drive_lock);

	poll_until(CACHE_CONVERGE_TIMEOUT, || {
		listing_resync_outcome(&messages, outermost, since).is_some()
	})
	.await;
	let outcome = listing_resync_outcome(&messages, outermost, since);
	let (reported, log) = {
		let guard = messages.lock().unwrap();
		let log = &guard[since..];
		let mut reported: Vec<Uuid> = log
			.iter()
			.filter_map(|msg| match msg {
				CacheMessage::SyncRootsDeleted(dropped) => Some(dropped.iter().copied()),
				_ => None,
			})
			.flatten()
			.collect();
		reported.sort_unstable();
		(reported, format!("{log:?}"))
	};
	// Everything cached at or under a dropped root goes, which is `nested` and `inner` either way.
	let left: Vec<Uuid> = roots
		.iter()
		.copied()
		.chain([nested.uuid(), inner.uuid()])
		.filter(|uuid| query_item_type(&path, *uuid).is_some())
		.collect();
	// Clean up before asserting, so a failure leaves nothing in the trash either.
	drop(sessions);
	client.flush_cache().await;
	resources
		.client
		.delete_dir_permanently(trashed)
		.await
		.unwrap();

	assert_eq!(
		outcome,
		Some(true),
		"a resync lists the roots once the drive lock is free, and converges: {log:?}"
	);
	assert_eq!(
		reported, roots,
		"the resync reports exactly the roots in the trash deleted: {log:?}"
	);
	assert!(
		left.is_empty(),
		"the rows at and under the dropped roots are wiped, left: {left:?}"
	);
}

/// A registered sync root trashed while the cache does not hear it.
#[shared_test_runtime]
async fn test_cache_resync_drops_a_registered_sync_root_in_the_trash() {
	check_resync_drops_roots_in_an_unheard_trash("cache_resync_trashed_parent", |tree| {
		vec![tree.trashed]
	})
	.await;
}

/// A registered sync root inside a dir, not itself a sync root, that is trashed while the cache does
/// not hear it.
#[shared_test_runtime]
async fn test_cache_resync_drops_a_sync_root_under_a_trashed_dir() {
	check_resync_drops_roots_in_an_unheard_trash("cache_resync_under_trashed_parent", |tree| {
		vec![tree.nested]
	})
	.await;
}

/// A registered sync root trashed while the cache does not hear it, with another registered root
/// nested in it: both are dropped and reported, the nested one not left registered without rows.
#[shared_test_runtime]
async fn test_cache_resync_drops_a_trashed_sync_root_and_one_nested_in_it() {
	check_resync_drops_roots_in_an_unheard_trash("cache_resync_trashed_nested_parent", |tree| {
		vec![tree.trashed, tree.nested]
	})
	.await;
}

#[shared_test_runtime]
async fn test_cache_ignores_irrelevant_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	// See test_cache_list_dir_recursive: converge the populate resync before injecting fiction.
	assert!(
		wait_for_converged_resync(&cache.messages, test_dir_uuid, 0, CACHE_CONVERGE_TIMEOUT).await,
		"the initial populate resync should converge before synthetic injection"
	);

	let dir = make_test_remote_dir("irrelevant_test_dir", test_dir_uuid);
	let dir_uuid: Uuid = dir.uuid();
	cache
		.handle
		.update_list_dir_recursive(vec![dir], vec![])
		.await
		.unwrap();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(10)).await,
		"baseline dir should be in cache"
	);

	// The cache has been receiving all socket events (authSuccess, etc.) as "irrelevant"
	// and should have processed them without crashing.
	let item_type = query_item_type(cache.db_path(), dir_uuid);
	assert_eq!(
		item_type,
		Some(1),
		"cache should still be functional after irrelevant events"
	);
}

#[shared_test_runtime]
async fn test_cache_full_file_lifecycle() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_lifecycle_dir")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();
	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"lifecycle dir should appear"
	);

	let file = client
		.make_file_builder("cache_lifecycle_file.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"lifecycle").await.unwrap();
	let file_uuid: Uuid = file.uuid();
	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"lifecycle file should appear"
	);

	client.trash_file(&mut file).await.unwrap();
	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"lifecycle file should be removed after trash"
	);

	client.trash_dir(&mut dir).await.unwrap();
	assert!(
		poll_for_item_absent(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"lifecycle dir should be removed after trash"
	);
}

#[shared_test_runtime]
async fn test_cache_mixed_socket_and_manual_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	// See test_cache_list_dir_recursive: converge the populate resync before injecting fiction.
	assert!(
		wait_for_converged_resync(&cache.messages, test_dir.uuid(), 0, CACHE_CONVERGE_TIMEOUT)
			.await,
		"the initial populate resync should converge before synthetic injection"
	);

	let file = client
		.make_file_builder("cache_mixed_socket.txt", test_dir.uuid())
		.unwrap();
	let file = client.upload_file(file, b"socket").await.unwrap();
	let socket_file_uuid: Uuid = file.uuid();

	let manual_file = make_test_remote_file("cache_mixed_manual.txt", test_dir.uuid());
	let manual_file_uuid: Uuid = manual_file.uuid();
	cache
		.handle
		.update_list_dir_recursive(vec![], vec![manual_file])
		.await
		.unwrap();

	assert!(
		poll_for_item(cache.db_path(), socket_file_uuid, Duration::from_secs(30)).await,
		"socket-triggered file should appear in cache"
	);
	assert!(
		poll_for_item(cache.db_path(), manual_file_uuid, Duration::from_secs(10)).await,
		"manually-inserted file should appear in cache"
	);
}

#[shared_test_runtime]
async fn test_cache_file_restore_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_restore.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"restore me").await.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	// Hold the trash lock across the trash -> restore/permanent-delete window: another leg's
	// account-global empty-trash (serialized on this lock) would permanently delete the file
	// out from under the later call.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should be removed after trash"
	);

	client.restore_file(&mut file).await.unwrap();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should reappear in cache after FileRestore event"
	);

	let (name, _, _, _) = query_cached_file(cache.db_path(), file_uuid).unwrap();
	assert_eq!(name, "cache_file_restore.txt");
}

#[shared_test_runtime]
async fn test_cache_file_metadata_changed_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_rename_old.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"rename me").await.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	let changes = FileMetaChanges::default()
		.name("cache_file_rename_new.txt")
		.unwrap();
	client
		.update_file_metadata(&mut file, changes)
		.await
		.unwrap();

	assert!(
		poll_for_file_name(
			cache.db_path(),
			file_uuid,
			"cache_file_rename_new.txt",
			Duration::from_secs(30)
		)
		.await,
		"file name should be updated in cache after FileMetadataChanged event"
	);
}

#[shared_test_runtime]
async fn test_cache_file_deleted_permanently_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_perm_delete.txt", test_dir.uuid())
		.unwrap();
	let mut file = client
		.upload_file(file, b"delete me forever")
		.await
		.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	// Hold the trash lock across the trash -> restore/permanent-delete window: another leg's
	// account-global empty-trash (serialized on this lock) would permanently delete the file
	// out from under the later call.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should be removed after trash"
	);

	client.delete_file_permanently(file).await.unwrap();

	// Should remain absent (permanent delete event should not re-add it)
	tokio::time::sleep(Duration::from_secs(2)).await;
	assert!(
		query_cached_file(cache.db_path(), file_uuid).is_none(),
		"file should remain absent after permanent delete"
	);
}

#[shared_test_runtime]
async fn test_cache_dir_restore_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_restore")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

	// Trash lock: see the file restore test above.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	client.trash_dir(&mut dir).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should be removed after trash"
	);

	client.restore_dir(&mut dir).await.unwrap();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should reappear in cache after FolderRestore event"
	);

	let (name, _, _) = query_cached_dir(cache.db_path(), dir_uuid).unwrap();
	assert_eq!(name, "cache_dir_restore");
}

#[shared_test_runtime]
async fn test_cache_dir_metadata_changed_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_rename_old")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

	let changes = DirectoryMetaChanges::default()
		.name("cache_dir_rename_new")
		.unwrap();
	client.update_dir_metadata(&mut dir, changes).await.unwrap();

	assert!(
		poll_for_dir_name(
			cache.db_path(),
			dir_uuid,
			"cache_dir_rename_new",
			Duration::from_secs(30)
		)
		.await,
		"dir name should be updated in cache after FolderMetadataChanged event"
	);
}

#[shared_test_runtime]
async fn test_cache_dir_color_changed_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_color")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

	client
		.set_dir_color(&mut dir, DirColor::Blue)
		.await
		.unwrap();

	assert!(
		poll_for_dir_color(cache.db_path(), dir_uuid, "blue", Duration::from_secs(30)).await,
		"dir color should be updated in cache after FolderColorChanged event"
	);
}

#[shared_test_runtime]
async fn test_cache_dir_deleted_permanently_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_perm_delete")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

	// Trash lock: see the file restore test above.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	client.trash_dir(&mut dir).await.unwrap();

	assert!(
		poll_for_item_absent(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should be removed after trash"
	);

	client.delete_dir_permanently(dir).await.unwrap();

	tokio::time::sleep(Duration::from_secs(2)).await;
	assert!(
		query_cached_dir(cache.db_path(), dir_uuid).is_none(),
		"dir should remain absent after permanent delete"
	);
}

#[shared_test_runtime]
async fn test_cache_item_favorite_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_favorite.txt", test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"favorite me").await.unwrap();
	let file_uuid: Uuid = file.uuid();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	// A favorite toggle moves the drive id like any write but does not take the drive lock, so take
	// it here: a test that holds the lock to keep the drive id still is not moved under its feet.
	{
		let _lock = client.lock_drive().await.unwrap();
		client.set_file_favorite(&mut file, true).await.unwrap();
	}

	assert!(
		poll_for_file_favorite(cache.db_path(), file_uuid, true, Duration::from_secs(30)).await,
		"file should be favorited in cache after ItemFavorite event"
	);
}

#[shared_test_runtime]
async fn test_cache_file_archived_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client, resources.dir.uuid()).await;

	let file = client
		.make_file_builder("cache_file_archive.txt", test_dir.uuid())
		.unwrap();
	let _original = client.upload_file(file, b"original content").await.unwrap();
	let original_uuid: Uuid = _original.uuid();

	assert!(
		poll_for_item(cache.db_path(), original_uuid, Duration::from_secs(30)).await,
		"original file should appear in cache"
	);

	// Upload a new file with the same name — this archives the old one
	let replacement = client
		.make_file_builder("cache_file_archive.txt", test_dir.uuid())
		.unwrap();
	let replacement = client
		.upload_file(replacement, b"replacement content")
		.await
		.unwrap();
	let replacement_uuid: Uuid = replacement.uuid();

	// Original should be removed (archived) from cache
	assert!(
		poll_for_item_absent(cache.db_path(), original_uuid, Duration::from_secs(30)).await,
		"original file should be removed from cache after FileArchived event"
	);

	assert!(
		poll_for_item(cache.db_path(), replacement_uuid, Duration::from_secs(30)).await,
		"replacement file should appear in cache"
	);
}

#[shared_test_runtime]
async fn test_cache_error_on_file_with_encrypted_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	let bad_file = make_test_remote_file_encrypted_meta(test_dir_uuid);
	let bad_file_uuid: Uuid = bad_file.uuid();

	cache
		.handle
		.update_list_dir_recursive(vec![], vec![bad_file])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_for_messages(Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::FileCacheableConversion(failed)
						if failed.file.uuid() == bad_file_uuid)
				})
			})
		})
		.await;

	assert!(
		saw_expected_error,
		"expected a FileCacheableConversion error for the encrypted-meta file"
	);

	assert!(
		query_cached_file(cache.db_path(), bad_file_uuid).is_none(),
		"file with encrypted meta should not be inserted into the cache"
	);
}

#[shared_test_runtime]
async fn test_cache_error_on_dir_with_encrypted_meta() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	let bad_dir = make_test_remote_dir_encrypted_meta(test_dir_uuid);
	let bad_dir_uuid: Uuid = bad_dir.uuid();

	cache
		.handle
		.update_list_dir_recursive(vec![bad_dir], vec![])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_for_messages(Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::DirCacheableConversion(failed)
						if failed.dir.uuid() == bad_dir_uuid)
				})
			})
		})
		.await;

	assert!(
		saw_expected_error,
		"expected a DirCacheableConversion error for the encrypted-meta dir"
	);

	assert!(
		query_cached_dir(cache.db_path(), bad_dir_uuid).is_none(),
		"dir with encrypted meta should not be inserted into the cache"
	);
}

#[shared_test_runtime]
async fn test_cache_error_on_file_with_non_uuid_parent() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let bad_file = make_test_remote_file_bad_parent("trashed_file.txt");
	let bad_file_uuid: Uuid = bad_file.uuid();

	cache
		.handle
		.update_list_dir_recursive(vec![], vec![bad_file])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_for_messages(Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::FileCacheableConversion(failed)
						if failed.file.uuid() == bad_file_uuid)
				})
			})
		})
		.await;

	assert!(
		saw_expected_error,
		"expected a FileCacheableConversion error for the non-UUID parent file"
	);
}

#[shared_test_runtime]
async fn test_cache_error_on_dir_with_non_uuid_parent() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	let bad_dir = make_test_remote_dir_bad_parent("trashed_dir");
	let bad_dir_uuid: Uuid = bad_dir.uuid();

	cache
		.handle
		.update_list_dir_recursive(vec![bad_dir], vec![])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_for_messages(Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::DirCacheableConversion(failed)
						if failed.dir.uuid() == bad_dir_uuid)
				})
			})
		})
		.await;

	assert!(
		saw_expected_error,
		"expected a DirCacheableConversion error for the non-UUID parent dir"
	);
}

#[shared_test_runtime]
async fn test_cache_partial_success_with_mixed_good_and_bad_items() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;
	let test_dir_uuid = resources.dir.uuid();

	// Injected synthetic items are absent-from-server fiction: wait for the initial populate
	// resync to converge FIRST, or its convergence diff correctly wipes them mid-test.
	assert!(
		wait_for_converged_resync(&cache.messages, test_dir_uuid, 0, CACHE_CONVERGE_TIMEOUT).await,
		"the initial populate resync should converge before synthetic injection"
	);

	// Two good dirs, two bad dirs (one encrypted, one bad parent)
	let good_dirs: Vec<RemoteDirectory> = (0..2)
		.map(|i| make_test_remote_dir(&format!("partial_dir_{i}"), test_dir_uuid))
		.collect();
	let good_dir_uuids: Vec<Uuid> = good_dirs.iter().map(|d| d.uuid()).collect();
	let bad_dir_encrypted = make_test_remote_dir_encrypted_meta(test_dir_uuid);
	let bad_dir_parent = make_test_remote_dir_bad_parent("partial_bad_parent_dir");

	// Three good files, two bad files (one encrypted, one bad parent)
	let good_files: Vec<RemoteFile> = (0..3)
		.map(|i| make_test_remote_file(&format!("partial_file_{i}.txt"), test_dir_uuid))
		.collect();
	let good_file_uuids: Vec<Uuid> = good_files.iter().map(|f| f.uuid()).collect();
	let bad_file_encrypted = make_test_remote_file_encrypted_meta(test_dir_uuid);
	let bad_file_parent = make_test_remote_file_bad_parent("partial_bad_parent_file.txt");

	let mut all_dirs = good_dirs;
	all_dirs.push(bad_dir_encrypted);
	all_dirs.push(bad_dir_parent);
	let mut all_files = good_files;
	all_files.push(bad_file_encrypted);
	all_files.push(bad_file_parent);

	cache
		.handle
		.update_list_dir_recursive(all_dirs, all_files)
		.await
		.unwrap();

	// Good items should be inserted despite the bad ones in the same batch.
	for (i, uuid) in good_dir_uuids.iter().enumerate() {
		assert!(
			poll_for_item(cache.db_path(), *uuid, Duration::from_secs(10)).await,
			"good dir {i} should appear in cache despite bad items in same batch"
		);
	}
	for (i, uuid) in good_file_uuids.iter().enumerate() {
		assert!(
			poll_for_item(cache.db_path(), *uuid, Duration::from_secs(10)).await,
			"good file {i} should appear in cache despite bad items in same batch"
		);
	}

	// Exactly four conversion errors should be reported (more would mean the good items also
	// failed; the count only grows, so an overshoot surfaces as a timeout here).
	let saw_all_errors = cache
		.wait_for_messages(Duration::from_secs(10), |msgs| {
			let mut file_errs = 0usize;
			let mut dir_errs = 0usize;
			for msg in msgs {
				for err in message_errors(msg) {
					match err {
						CacheError::FileCacheableConversion(_) => file_errs += 1,
						CacheError::DirCacheableConversion(_) => dir_errs += 1,
						_ => {}
					}
				}
			}
			file_errs == 2 && dir_errs == 2
		})
		.await;

	assert!(
		saw_all_errors,
		"expected exactly 2 file + 2 dir conversion errors to be reported"
	);
}

/// `add_sync_root` with a uuid that is not a reachable directory is REJECTED — the future resolves
/// to `Err` carrying `CacheError::InvalidSyncRoot` — so the bad key never enters the active set
/// (which would otherwise make every subsequent resync's `get_dir` fail and re-trigger a resync on
/// each event: a tight loop).
#[shared_test_runtime]
async fn test_add_sync_root_rejects_invalid_uuid() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client, resources.dir.uuid()).await;

	// A random uuid that does not correspond to any directory on the account.
	let bogus = Uuid::new_v4();
	let err = cache
		.client
		.clone()
		.add_sync_root(bogus, noop_sync_root_callback())
		.await
		.expect_err("add_sync_root with a bogus uuid must be rejected");

	assert!(
		matches!(
			err.downcast_ref::<CacheError>(),
			Some(CacheError::InvalidSyncRoot { uuid, .. }) if *uuid == bogus
		),
		"the rejection should carry CacheError::InvalidSyncRoot, got {err:?}"
	);
}

/// The bounded-acquisition liveness property: while ANOTHER holder owns the account's drive
/// lock, a worker whose resync cannot acquire it must still apply socket events between its
/// bounded attempts (under the old unbounded acquisition the worker parked inside the island
/// until the lock was won, freezing event application for the whole contention window), and the
/// resync itself must converge via the retry timer once the lock frees up. An event applied in that
/// window must reach the root's registration, and a second registration of the root interrupting
/// the wait must not leave the resync without a retry.
#[shared_test_runtime]
async fn test_cache_applies_events_while_drive_lock_is_contended() {
	// ISOLATION: this is the ONLY cache test that holds the account-wide drive lock for an
	// extended window (across the upload + the 45s liveness poll below). The lock is per-account
	// and admits one holder, so on the shared main account this hold STARVES every other test's
	// convergence resync until release — the dominant source of suite flakiness. Run it on the
	// SHARE account instead, which no other cache test touches, so its monopoly contends only
	// with itself. (`cargo test --test cache_tests` runs as its own process, so the chat/socket
	// tests that use the share account are not running concurrently.)
	let resources = test_utils::SHARE_RESOURCES.get_resources().await;
	let client = &resources.client;

	// A populated dir that will become the (UNCOVERED) sync root of a derived cache — created
	// before the lock is taken so server-side propagation isn't racing the lock window.
	let sub = client
		.create_dir(&(&resources.dir).into(), "lock_contended_root")
		.await
		.unwrap();
	let sub_uuid: Uuid = sub.uuid();

	// A derived cache scoped to `sub` ONLY (no whole-account root), so the add below cannot
	// take the covered fast path and must run a lock-needing convergence resync.
	let client2 = derive_client(client.as_ref());
	let path2 = temp_cache_path();
	let (messages2, status_cb2) = capturing_status_callback();
	client2
		.configure_cache(path2.clone(), status_cb2)
		.await
		.unwrap();

	// The TEST monopolizes the drive lock (auto-refreshed while held; released on drop).
	let lock = client.lock_drive().await.unwrap();

	// Snapshot the log BEFORE the add triggers the convergence resync: that resync emits its
	// `Started` (covering `sub`) up front, before the patient lock wait, so `since` must precede
	// the add for `wait_for_converged_resync` to pair that `Started` with the post-release
	// `Finished`. (The contended attempts in between report `converged: false` and never match.)
	let since = messages_len(&messages2);

	// What the root's registration is told. The events below land while the root's first listing
	// is still waiting for the lock, so its own node is not cached yet — and an event applied then
	// must still reach the root that admitted it: the listing that commits later finds the item
	// already cached and announces nothing.
	let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
	let recording = || -> SyncRootCallback {
		let seen = seen.clone();
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			seen.lock()
				.unwrap()
				.extend(events.map(|event| format!("{:?}", event.event)));
		})
	};

	// Validation (`get_dir`, no lock involved) can hit fresh-dir propagation lag — retry.
	let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
	let _handle2 = loop {
		match client2.clone().add_sync_root(sub_uuid, recording()).await {
			Ok(handle) => break handle,
			Err(e) if tokio::time::Instant::now() < deadline => {
				eprintln!("add_sync_root({sub_uuid}) not accepted yet ({e}); retrying");
				tokio::time::sleep(Duration::from_millis(1000)).await;
			}
			Err(e) => panic!("add_sync_root kept failing: {e:?}"),
		}
	};

	// Wait for the WORKER's socket to authenticate before emitting the event it must catch.
	// `configure_cache` only stores config — the worker and its socket listener spawn on the
	// FIRST `add_sync_root`, so the socket's connect+auth window opens here. Calling this any
	// earlier both guarantees nothing about that socket AND spins up a throwaway socket task
	// (the helper's temp listener is the only strong request-channel sender, so its drop tears
	// the task down again). An upload racing the connect window loses its FileNew for good —
	// the socket never redelivers, and the only recovery is the resync this test deliberately
	// blocks. Here the helper's listener joins the worker's live task (connected-manager adds
	// replay authSuccess immediately if auth already happened).
	ensure_socket_ready(&client2).await;

	// LIVENESS: the worker's resync attempts keep failing on the contended lock, but the
	// FileNew socket event must apply in a drain window between attempts. (`sub` is a sync-root
	// key, so its direct children pass the membership gate without any cached ancestry.)
	let upload_deadline = tokio::time::Instant::now() + Duration::from_secs(60);
	let file = loop {
		let builder = client.make_file_builder("alive.txt", sub.uuid()).unwrap();
		match client.upload_file(builder, b"x").await {
			Ok(file) => break file,
			Err(e) if tokio::time::Instant::now() < upload_deadline => {
				eprintln!("upload into fresh dir not accepted yet ({e}); retrying");
				tokio::time::sleep(Duration::from_millis(1000)).await;
			}
			Err(e) => panic!("upload kept failing: {e:?}"),
		}
	};
	let file_uuid: Uuid = file.uuid();
	assert!(
		poll_for_item(&path2, file_uuid, Duration::from_secs(45)).await,
		"socket events must keep applying while the drive lock is contended"
	);
	let file_key = file_uuid.to_string();
	assert!(
		poll_until(Duration::from_secs(5), || {
			seen.lock()
				.unwrap()
				.iter()
				.any(|event| event.contains(&file_key))
		})
		.await,
		"an event applied before the root's first listing commits is announced to the root"
	);

	// A second registration of the same root while its resync is still waiting for the lock: the
	// join interrupts that wait but re-runs nothing itself (the root is already active), and on a
	// quiet account no event will either — the resync has to re-arm its own retry, or the
	// convergence below never comes.
	let _join = client2
		.clone()
		.add_sync_root(sub_uuid, noop_sync_root_callback())
		.await
		.unwrap();

	// CONVERGENCE: once the lock frees up, the patient acquisition (or the next retry) wins it and
	// the resync materializes the root's anchor row (which only the resync writes — no event
	// carries it). Wait on the converged signal rather than a fixed window.
	drop(lock);
	assert!(
		wait_for_converged_resync(&messages2, sub_uuid, since, CACHE_CONVERGE_TIMEOUT).await,
		"the resync converges after the lock is released"
	);
	assert!(
		poll_for_item(&path2, sub_uuid, Duration::from_secs(5)).await,
		"the resync materializes the root anchor row once the lock is released"
	);
}

#[shared_test_runtime]
async fn test_cache_resync_reports_progress() {
	let resources = test_utils::RESOURCES.get_resources().await;
	// Seed the scope dir BEFORE the cache exists so the populate listing is non-empty (the
	// Listing assertion below wants bytes_downloaded > 0).
	resources
		.client
		.create_dir(&(&resources.dir).into(), "progress_seed")
		.await
		.unwrap();
	let root: Uuid = resources.dir.uuid();
	let cache = TestCache::new(&resources.client, root).await;

	// `TestCache::new` registered the test dir as a NEW sync root, which triggers exactly
	// the convergence resync whose progress we expect: `Started` carrying the root, byte
	// `Listing` tick(s) for it (the download layer guarantees a final tick per listing on
	// native), `Applying`, and `Finished { converged: true }` (transient failures retry and
	// converge eventually). The worker's startup gap-check can emit an EARLIER empty-roots
	// Started/Finished pair (it runs before the add is processed) and the status callback
	// appends batches via spawned tasks (order not guaranteed), so assert PRESENCE of each
	// step rather than strict ordering. The window is the suite's convergence timeout, not a
	// shorter number of its own: one contended lock acquisition is the worker's full patient
	// budget (60 polls x 2 s ~= 120 s) and ends in `Finished { converged: false }` plus a
	// retry, so a 120 s window was exactly one such acquisition (windows V1, 2026-09-03).
	let saw_full_sequence = cache
		.wait_for_messages(CACHE_CONVERGE_TIMEOUT, |msgs| {
			let progress: Vec<&ResyncProgress> = msgs
				.iter()
				.filter_map(|msg| match msg {
					CacheMessage::ResyncProgress(progress) => Some(progress),
					_ => None,
				})
				.collect();
			let started = progress.iter().any(
				|step| matches!(step, ResyncProgress::Started { roots } if roots.contains(&root)),
			);
			let listing = progress.iter().any(|step| {
				matches!(
					step,
					ResyncProgress::Listing {
						root: listed,
						root_index: 0,
						root_count: 1,
						bytes_downloaded,
						..
					} if *listed == root && *bytes_downloaded > 0
				)
			});
			let applying = progress
				.iter()
				.any(|step| matches!(step, ResyncProgress::Applying));
			let finished = progress
				.iter()
				.any(|step| matches!(step, ResyncProgress::Finished { converged: true }));
			started && listing && applying && finished
		})
		.await;
	if !saw_full_sequence {
		panic!(
			"timed out waiting for the full resync progress sequence; got: {:?}",
			*cache.messages.lock().unwrap()
		);
	}
}

/// A FILE sync root follows its lineage across a content edit. Registered ALONE — no dir root
/// covers the file — so its row can only be populated by the by-stable fetch, and after an edit
/// the cached row must be the SUCCESSOR uuid (the `fileTrash`+`fileNew` pair is an identity
/// update, not a delete + create).
#[shared_test_runtime]
async fn test_cache_file_sync_root_follows_an_edit() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let test_dir = &resources.dir;
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	let file = client
		.make_file_builder("file_root_edit.txt", test_dir.uuid())
		.unwrap();
	let file = client.upload_file(file, b"v1").await.unwrap();

	let _handle = client
		.clone()
		.add_file_sync_root(file.stable_uuid(), noop_sync_root_callback())
		.await
		.unwrap();
	ensure_socket_ready(&client).await;

	assert!(
		poll_for_item(&path, file.uuid(), Duration::from_secs(30)).await,
		"the convergence fetch populates the tracked file (nothing else can)"
	);

	// backend timestamps have a resolution of one second
	tokio::time::sleep(Duration::from_secs(2)).await;
	let edited = client
		.upload_file(
			client
				.make_file_builder("file_root_edit.txt", test_dir.uuid())
				.unwrap(),
			b"v2 contents",
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), file.uuid(), "an edit re-mints the uuid");

	assert!(
		poll_for_item(&path, edited.uuid(), Duration::from_secs(30)).await,
		"the tracked row is re-filed under the successor uuid"
	);
	assert!(
		poll_for_item_absent(&path, file.uuid(), Duration::from_secs(30)).await,
		"the superseded uuid is retired"
	);
}
/// The same edit on a versioning-DISABLED account. There the server retires the old uuid with a
/// `fileTrash` whose stable id is the retired row's own freshly minted one — never the lineage's,
/// or a stable-keyed consumer would tombstone the live file — so the engine has to find the
/// lineage through the row it holds, not through the event, for the identity update to happen.
#[shared_test_runtime]
async fn test_cache_file_sync_root_follows_a_versioning_disabled_edit() {
	let resources = test_utils::RESOURCES.get_resources().await;
	// Same order as the socket and user tests: version-chain lock, then the account-wide
	// versioning-flag lock.
	let _version_lock = resources
		.client
		.acquire_lock_with_default(test_utils::locks::VERSIONS)
		.await
		.unwrap();
	let _versioning_lock = resources
		.client
		.acquire_lock_with_default(test_utils::locks::USER_VERSIONING)
		.await
		.unwrap();
	resources
		.client
		.set_versioning_enabled(false)
		.await
		.unwrap();

	let client = derive_client(resources.client.as_ref());
	let test_dir = &resources.dir;
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	let file = client
		.make_file_builder("file_root_vd_edit.txt", test_dir.uuid())
		.unwrap();
	let file = client.upload_file(file, b"v1").await.unwrap();

	// What the file root is told is the difference between re-keying the row and deleting it:
	// the retirement must reach it as the identity update it is, never as a removal.
	let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
	let seen_cb = seen.clone();
	let recording: SyncRootCallback =
		Box::new(move |events: &mut dyn Iterator<Item = &CacheEvent<'_>>| {
			seen_cb
				.lock()
				.unwrap()
				.extend(events.map(|event| format!("{:?}", event.event)));
		});
	let _handle = client
		.clone()
		.add_file_sync_root(file.stable_uuid(), recording)
		.await
		.unwrap();
	ensure_socket_ready(&client).await;

	assert!(
		poll_for_item(&path, file.uuid(), Duration::from_secs(30)).await,
		"the convergence fetch populates the tracked file (nothing else can)"
	);

	// backend timestamps have a resolution of one second
	tokio::time::sleep(Duration::from_secs(2)).await;
	let edited = client
		.upload_file(
			client
				.make_file_builder("file_root_vd_edit.txt", test_dir.uuid())
				.unwrap(),
			b"v2 contents",
		)
		.await
		.unwrap();
	assert_ne!(edited.uuid(), file.uuid(), "an edit re-mints the uuid");
	assert_eq!(
		edited.stable_uuid(),
		file.stable_uuid(),
		"the lineage's stable id moves to the successor"
	);

	assert!(
		poll_for_item(&path, edited.uuid(), Duration::from_secs(30)).await,
		"the tracked row is re-filed under the successor uuid"
	);
	assert!(
		poll_for_item_absent(&path, file.uuid(), Duration::from_secs(30)).await,
		"the superseded uuid is retired"
	);
	// The dispatch lands after the commit the polls observed, so give it a moment of its own.
	let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
	loop {
		let events = seen.lock().unwrap().clone();
		if events.iter().any(|event| event.contains("Trashed")) {
			assert!(
				events.iter().all(|event| !event.contains("Removed")),
				"no removal is ever dispatched for the tracked lineage: {events:?}"
			);
			break;
		}
		assert!(
			tokio::time::Instant::now() < deadline,
			"the retirement never reached the file root as an identity update: {events:?}"
		);
		tokio::time::sleep(Duration::from_millis(200)).await;
	}

	resources.client.set_versioning_enabled(true).await.unwrap();
}

/// A tracked file TRASHED while the cache was closed. No socket event for it will ever be
/// delivered, so the reopen's by-stable head fetch is the only thing that can notice — and what it
/// gets back is a head parented to the TRASH. That head is the removal a `fileTrash` would have
/// been had we been listening: the cached row has to go, or the engine (and search, which reads
/// those same rows) keeps handing out a file that is in the trash until somebody browses its
/// parent. Registered ALONE — no dir root covers the file — so nothing but the head can populate
/// the row in session 1, and nothing but the head can remove it in session 2.
#[shared_test_runtime]
async fn test_cache_file_root_trashed_while_offline_is_removed_on_reopen() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let test_dir = &resources.dir;
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	let mut file = client
		.upload_file(
			client
				.make_file_builder("file_root_offline_trash.txt", test_dir.uuid())
				.unwrap(),
			b"trash me while nobody is listening",
		)
		.await
		.unwrap();

	// Session 1: the convergence fetch populates the tracked lineage.
	{
		let _handle = client
			.clone()
			.add_file_sync_root(file.stable_uuid(), noop_sync_root_callback())
			.await
			.unwrap();
		assert!(
			poll_for_item(&path, file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"the convergence fetch populates the tracked file (nothing else can)"
		);
	}
	client.flush_cache().await; // clean flush + join before reopening the same DB file

	// Trashed with nobody listening. The trash lock keeps another leg's account-global
	// empty-trash from permanently deleting it before session 2 observes it and the final
	// delete_file_permanently below runs.
	let _trash_lock = client
		.acquire_lock_with_default(test_utils::locks::TRASH)
		.await
		.unwrap();
	client.trash_file(&mut file).await.unwrap();

	// Session 2: same DB, same lineage. Re-adding the file root respawns the worker and converges.
	{
		let _handle = client
			.clone()
			.add_file_sync_root(file.stable_uuid(), noop_sync_root_callback())
			.await
			.unwrap();
		assert!(
			poll_for_item_absent(&path, file.uuid(), CACHE_CONVERGE_TIMEOUT).await,
			"a trashed head must resync as the removal it is, not be dropped as a bad parent"
		);
	}
	client.flush_cache().await;

	client.delete_file_permanently(file).await.unwrap();
}
