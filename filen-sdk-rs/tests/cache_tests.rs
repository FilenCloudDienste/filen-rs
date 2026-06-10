use std::time::Duration;

use filen_macros::shared_test_runtime;
use filen_sdk_rs::cache::CacheError;
use filen_sdk_rs::{
	ErrorKind,
	fs::{HasUUID, dir::meta::DirectoryMetaChanges, file::meta::FileMetaChanges},
	io::{RemoteDirectory, RemoteFile},
};
use filen_types::api::v3::dir::color::DirColor;
use rusqlite::params;
use uuid::Uuid;

mod helpers;
use helpers::*;

#[shared_test_runtime]
async fn test_cache_init_creates_schema() {
	let client = test_utils::RESOURCES.client().await;
	let cache = TestCache::new(&client).await;

	let conn = open_read_db(cache.db_path()).unwrap();

	let tables: Vec<String> = conn
		.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
		.unwrap()
		.query_map([], |row| row.get(0))
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
	let client = test_utils::RESOURCES.client().await;
	let cache = TestCache::new(&client).await;

	let root_uuid: Uuid = client.root().uuid().into();

	let item_type = query_item_type(cache.db_path(), root_uuid);
	assert_eq!(item_type, Some(0), "root should be type 0 (Root)");

	let conn = open_read_db(cache.db_path()).unwrap();
	let root_exists: bool = conn
		.query_row(
			"SELECT COUNT(*) > 0 FROM roots r JOIN items i ON i.id = r.id WHERE i.uuid = ?",
			params![root_uuid],
			|row| row.get(0),
		)
		.unwrap();
	assert!(root_exists, "root should exist in roots table");
}

#[shared_test_runtime]
async fn test_cache_file_new_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_file_new.txt", *test_dir.uuid())
		.unwrap();
	let file = client
		.upload_file(file, b"cache test content")
		.await
		.unwrap();
	let file_uuid: Uuid = file.uuid().into();

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
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_file_trash.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"to be trashed").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();

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
	let cache = TestCache::new(&resources.client).await;

	let mut file_uuids = Vec::new();

	for i in 0..3 {
		let name = format!("cache_multi_{i}.txt");
		let content = format!("content {i}");
		let spec = client.make_file_builder(&name, *test_dir.uuid()).unwrap();
		let file = client.upload_file(spec, content.as_bytes()).await.unwrap();
		file_uuids.push(Uuid::from(file.uuid()));
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
	let cache = TestCache::new(&resources.client).await;

	let dir = client
		.create_dir(&test_dir.into(), "cache_dir_new")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

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
	let cache = TestCache::new(&resources.client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_trash")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

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
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_file_move.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"moveable content").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	let target_dir = client
		.create_dir(&test_dir.into(), "cache_move_target")
		.await
		.unwrap();
	let target_dir_uuid: Uuid = target_dir.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), target_dir_uuid, Duration::from_secs(30)).await,
		"target dir should appear in cache"
	);

	client
		.move_file(&mut file, &(&target_dir).into())
		.await
		.unwrap();

	// Give time for the move event to be processed
	tokio::time::sleep(Duration::from_secs(3)).await;

	// File should still exist in the cache (updated with new parent)
	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(5)).await,
		"file should still exist in cache after move event"
	);
}

#[shared_test_runtime]
async fn test_cache_dir_move_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client).await;

	let mut move_dir = client
		.create_dir(&test_dir.into(), "cache_dir_to_move")
		.await
		.unwrap();
	let move_dir_uuid: Uuid = move_dir.uuid().into();

	let target_dir = client
		.create_dir(&test_dir.into(), "cache_dir_move_target")
		.await
		.unwrap();
	let target_dir_uuid: Uuid = target_dir.uuid().into();

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

	assert!(
		poll_for_item(cache.db_path(), move_dir_uuid, Duration::from_secs(30)).await,
		"moved dir should still exist in cache"
	);
}

#[shared_test_runtime]
async fn test_cache_list_dir_recursive() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	let dirs: Vec<RemoteDirectory> = (0..3)
		.map(|i| make_test_remote_dir(&format!("ldr_dir_{i}"), test_dir_uuid))
		.collect();
	let files: Vec<RemoteFile> = (0..5)
		.map(|i| make_test_remote_file(&format!("ldr_file_{i}.txt"), test_dir_uuid))
		.collect();

	let dir_uuids: Vec<Uuid> = dirs.iter().map(|d| d.uuid().into()).collect();
	let file_uuids: Vec<Uuid> = files.iter().map(|f| f.uuid().into()).collect();

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
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	let dirs: Vec<RemoteDirectory> = (0..50)
		.map(|i| make_test_remote_dir(&format!("ldr_batch_dir_{i}"), test_dir_uuid))
		.collect();
	let files: Vec<RemoteFile> = (0..100)
		.map(|i| make_test_remote_file(&format!("ldr_batch_file_{i}.txt"), test_dir_uuid))
		.collect();

	let last_dir_uuid: Uuid = dirs.last().unwrap().uuid().into();
	let last_file_uuid: Uuid = files.last().unwrap().uuid().into();

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
	let client = test_utils::RESOURCES.client().await;
	let client = derive_client(client.as_ref());
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	{
		let _handle = client
			.clone()
			.add_sync_root(client.root().uuid().into(), noop_sync_root_callback())
			.await
			.unwrap();
	}
	// Dropping the last handle shuts the worker down on its own; `flush_cache` joins it so the DB
	// is deterministically closed before we read it.
	client.flush_cache().await;

	assert!(path.exists(), "DB file should persist after cache drop");

	let conn = open_read_db(&path).unwrap();
	let root_uuid: Uuid = client.root().uuid().into();
	let root_exists: bool = conn
		.query_row(
			"SELECT COUNT(*) > 0 FROM items WHERE uuid = ? AND type = 0",
			params![root_uuid],
			|row| row.get(0),
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
	let account_root: Uuid = client.root().uuid().into();
	// Configured ONCE — the config (and DB path) survives the worker stopping and respawning.
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	let dir = make_test_remote_dir("reopen_test_dir", test_dir_uuid);
	let dir_uuid: Uuid = dir.uuid().into();
	{
		let handle = client
			.clone()
			.add_sync_root(account_root, noop_sync_root_callback())
			.await
			.unwrap();
		ensure_socket_ready(&client).await;
		handle
			.update_list_dir_recursive(vec![dir], vec![])
			.await
			.unwrap();
		assert!(poll_for_item(&path, dir_uuid, Duration::from_secs(10)).await);
	}
	client.flush_cache().await;

	{
		let _handle = client
			.clone()
			.add_sync_root(account_root, noop_sync_root_callback())
			.await
			.unwrap();

		assert!(
			poll_for_item(&path, dir_uuid, Duration::from_secs(5)).await,
			"data from previous session should persist"
		);

		let (name, _, _) = query_cached_dir(&path, dir_uuid).unwrap();
		assert_eq!(name, "reopen_test_dir");
	}
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
	let test_dir_uuid: Uuid = test_dir.uuid().into();
	let account_root: Uuid = client.root().uuid().into();
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	// Session 1: a fresh cache. The startup gap-check (watermark None, remote drive id > 0) resyncs and
	// populates the cache from the account listing, so the existing test dir shows up.
	{
		let _handle = client
			.clone()
			.add_sync_root(account_root, noop_sync_root_callback())
			.await
			.unwrap();
		assert!(
			poll_for_item(&path, test_dir_uuid, Duration::from_secs(60)).await,
			"startup resync should populate the cache from the account listing"
		);
	}
	client.flush_cache().await; // clean flush + join before reopening the same DB file

	// Offline change: create a dir while NO cache is running, advancing the remote drive id.
	let mut offline_dir = client
		.create_dir(&test_dir.into(), "cache_restart_resync")
		.await
		.unwrap();
	let offline_uuid: Uuid = offline_dir.uuid().into();

	// Session 2: re-adding a sync root respawns the worker on the same DB. The startup gap-check sees
	// the advanced drive id and resyncs, so the dir created while we were offline appears — with no
	// socket delivery involved.
	{
		let _handle = client
			.clone()
			.add_sync_root(account_root, noop_sync_root_callback())
			.await
			.unwrap();
		assert!(
			poll_for_item(&path, offline_uuid, Duration::from_secs(60)).await,
			"restart resync should catch up the dir created while the cache was offline"
		);
	}
	client.flush_cache().await;

	let _ = client.trash_dir(&mut offline_dir).await;
}

/// A sync root PERMANENTLY DELETED server-side while the cache is closed: registrations do not
/// survive a worker restart, so re-adding the root on reopen runs the `add_sync_root` validation —
/// the server answers not-found and the add is REJECTED with `CacheError::InvalidSyncRoot` (the bad
/// key never re-enters the active set) AND the stale subtree the prior session cached under the
/// root is wiped. The stale handle from the previous session stays inert and its drop is a harmless
/// no-op. (The LIVE not-found classification inside a running resync — drop + wipe +
/// `SyncRootsDeleted` — is covered deterministically by the `finalize_resync` unit tests.)
#[shared_test_runtime]
async fn test_cache_re_add_of_permanently_deleted_sync_root_is_rejected() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = derive_client(resources.client.as_ref());
	let test_dir = &resources.dir;
	let path = temp_cache_path();
	client.configure_cache(path.clone(), |_| {}).await.unwrap();

	// The subdir that will be the sole sync root, plus a child so there is a populated subtree.
	let mut root_dir = client
		.create_dir(&test_dir.into(), "cache_resync_deleted_root")
		.await
		.unwrap();
	let root_uuid: Uuid = root_dir.uuid().into();
	let child = client
		.create_dir(&(&root_dir).into(), "child")
		.await
		.unwrap();
	let child_uuid: Uuid = child.uuid().into();

	// Session 1: selective sync of ONLY `root_dir`; populate via the convergence resync, then flush.
	let stale_handle = client
		.clone()
		.add_sync_root(root_uuid, noop_sync_root_callback())
		.await
		.unwrap();
	assert!(
		poll_for_item(&path, root_uuid, Duration::from_secs(60)).await,
		"the sync root should be cached after the convergence resync"
	);
	assert!(
		poll_for_item(&path, child_uuid, Duration::from_secs(60)).await,
		"the child should be cached after the convergence resync"
	);
	client.flush_cache().await;

	// Permanently delete the root (and its subtree) while the cache is offline.
	client.trash_dir(&mut root_dir).await.unwrap();
	client.delete_dir_permanently(root_dir).await.unwrap();

	// The `/v3/dir` metadata lookup is eventually-consistent: a permanently-deleted dir keeps resolving
	// for a few seconds before the server reports it gone. Wait for that to settle BEFORE re-adding, so
	// the validating `get_dir` deterministically sees the not-found.
	let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
	loop {
		match client.get_dir((&root_uuid).into()).await {
			Err(e) if e.kind() == ErrorKind::FolderNotFound => break,
			_ => {
				assert!(
					tokio::time::Instant::now() < deadline,
					"the server never reported the permanently-deleted root as gone"
				);
				tokio::time::sleep(Duration::from_millis(500)).await;
			}
		}
	}

	// Session 2: the app re-adds the root it has not yet learned is gone — validation rejects it.
	let err = client
		.clone()
		.add_sync_root(root_uuid, noop_sync_root_callback())
		.await
		.expect_err("re-adding a permanently-deleted sync root must be rejected");
	assert!(
		matches!(
			err.downcast_ref::<CacheError>(),
			Some(CacheError::InvalidSyncRoot { uuid, .. }) if *uuid == root_uuid
		),
		"the rejection should carry CacheError::InvalidSyncRoot for the deleted root, got {err:?}"
	);

	// The definitive not-found also wiped the stale subtree session 1 cached under the root —
	// without it those rows would be stranded forever (membership-gated out of live events,
	// anchored by no resync diff), serving deleted content to any DB reader.
	assert!(
		poll_for_item_absent(&path, root_uuid, Duration::from_secs(30)).await,
		"the deleted root's stale row must be wiped by the rejected re-add"
	);
	assert!(
		poll_for_item_absent(&path, child_uuid, Duration::from_secs(30)).await,
		"the deleted root's stale subtree must be cascade-wiped by the rejected re-add"
	);

	// The stale session-1 handle is inert (its worker is gone); dropping it must be a no-op.
	drop(stale_handle);
	client.flush_cache().await;
}

#[shared_test_runtime]
async fn test_cache_ignores_irrelevant_events() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	let dir = make_test_remote_dir("irrelevant_test_dir", test_dir_uuid);
	let dir_uuid: Uuid = dir.uuid().into();
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
	let cache = TestCache::new(&resources.client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_lifecycle_dir")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();
	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"lifecycle dir should appear"
	);

	let file = client
		.make_file_builder("cache_lifecycle_file.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"lifecycle").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();
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
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_mixed_socket.txt", *test_dir.uuid())
		.unwrap();
	let file = client.upload_file(file, b"socket").await.unwrap();
	let socket_file_uuid: Uuid = file.uuid().into();

	let manual_file = make_test_remote_file("cache_mixed_manual.txt", test_dir.uuid());
	let manual_file_uuid: Uuid = manual_file.uuid().into();
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
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_restore.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"restore me").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

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
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_rename_old.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"rename me").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();

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
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_perm_delete.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client
		.upload_file(file, b"delete me forever")
		.await
		.unwrap();
	let file_uuid: Uuid = file.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

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
	let cache = TestCache::new(client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_restore")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

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
	let cache = TestCache::new(client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_rename_old")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

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
	let cache = TestCache::new(client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_color")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

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
	let cache = TestCache::new(client).await;

	let mut dir = client
		.create_dir(&test_dir.into(), "cache_dir_perm_delete")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"dir should appear in cache"
	);

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
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_favorite.txt", *test_dir.uuid())
		.unwrap();
	let mut file = client.upload_file(file, b"favorite me").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should appear in cache"
	);

	client.set_file_favorite(&mut file, true).await.unwrap();

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
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_archive.txt", *test_dir.uuid())
		.unwrap();
	let _original = client.upload_file(file, b"original content").await.unwrap();
	let original_uuid: Uuid = _original.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), original_uuid, Duration::from_secs(30)).await,
		"original file should appear in cache"
	);

	// Upload a new file with the same name — this archives the old one
	let replacement = client
		.make_file_builder("cache_file_archive.txt", *test_dir.uuid())
		.unwrap();
	let replacement = client
		.upload_file(replacement, b"replacement content")
		.await
		.unwrap();
	let replacement_uuid: Uuid = replacement.uuid().into();

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
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	let bad_file = make_test_remote_file_encrypted_meta(test_dir_uuid);
	let bad_file_uuid: Uuid = bad_file.uuid().into();

	cache
		.handle
		.update_list_dir_recursive(vec![], vec![bad_file])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_and_inspect_messages(1, Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::FileCacheableConversion(failed)
						if Uuid::from(failed.file.uuid()) == bad_file_uuid)
				})
			})
		})
		.await
		.unwrap_or(false);

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
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	let bad_dir = make_test_remote_dir_encrypted_meta(test_dir_uuid);
	let bad_dir_uuid: Uuid = bad_dir.uuid().into();

	cache
		.handle
		.update_list_dir_recursive(vec![bad_dir], vec![])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_and_inspect_messages(1, Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::DirCacheableConversion(failed)
						if Uuid::from(failed.dir.uuid()) == bad_dir_uuid)
				})
			})
		})
		.await
		.unwrap_or(false);

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
	let cache = TestCache::new(&resources.client).await;

	let bad_file = make_test_remote_file_bad_parent("trashed_file.txt");
	let bad_file_uuid: Uuid = bad_file.uuid().into();

	cache
		.handle
		.update_list_dir_recursive(vec![], vec![bad_file])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_and_inspect_messages(1, Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::FileCacheableConversion(failed)
						if Uuid::from(failed.file.uuid()) == bad_file_uuid)
				})
			})
		})
		.await
		.unwrap_or(false);

	assert!(
		saw_expected_error,
		"expected a FileCacheableConversion error for the non-UUID parent file"
	);
}

#[shared_test_runtime]
async fn test_cache_error_on_dir_with_non_uuid_parent() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client).await;

	let bad_dir = make_test_remote_dir_bad_parent("trashed_dir");
	let bad_dir_uuid: Uuid = bad_dir.uuid().into();

	cache
		.handle
		.update_list_dir_recursive(vec![bad_dir], vec![])
		.await
		.unwrap();

	let saw_expected_error = cache
		.wait_and_inspect_messages(1, Duration::from_secs(10), |msgs| {
			msgs.iter().any(|msg| {
				message_errors(msg).iter().any(|e| {
					matches!(e, CacheError::DirCacheableConversion(failed)
						if Uuid::from(failed.dir.uuid()) == bad_dir_uuid)
				})
			})
		})
		.await
		.unwrap_or(false);

	assert!(
		saw_expected_error,
		"expected a DirCacheableConversion error for the non-UUID parent dir"
	);
}

#[shared_test_runtime]
async fn test_cache_partial_success_with_mixed_good_and_bad_items() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let cache = TestCache::new(&resources.client).await;
	let test_dir_uuid = resources.dir.uuid();

	// Two good dirs, two bad dirs (one encrypted, one bad parent)
	let good_dirs: Vec<RemoteDirectory> = (0..2)
		.map(|i| make_test_remote_dir(&format!("partial_dir_{i}"), test_dir_uuid))
		.collect();
	let good_dir_uuids: Vec<Uuid> = good_dirs.iter().map(|d| d.uuid().into()).collect();
	let bad_dir_encrypted = make_test_remote_dir_encrypted_meta(test_dir_uuid);
	let bad_dir_parent = make_test_remote_dir_bad_parent("partial_bad_parent_dir");

	// Three good files, two bad files (one encrypted, one bad parent)
	let good_files: Vec<RemoteFile> = (0..3)
		.map(|i| make_test_remote_file(&format!("partial_file_{i}.txt"), test_dir_uuid))
		.collect();
	let good_file_uuids: Vec<Uuid> = good_files.iter().map(|f| f.uuid().into()).collect();
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

	// Exactly four conversion errors should have been reported in a single message batch.
	let (file_errs, dir_errs) = cache
		.wait_and_inspect_messages(1, Duration::from_secs(10), |msgs| {
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
			(file_errs, dir_errs)
		})
		.await
		.unwrap_or((0, 0));

	assert_eq!(file_errs, 2, "expected 2 file conversion errors");
	assert_eq!(dir_errs, 2, "expected 2 dir conversion errors");
}

/// `add_sync_root` with a uuid that is not a reachable directory is REJECTED — the future resolves
/// to `Err` carrying `CacheError::InvalidSyncRoot` — so the bad key never enters the active set
/// (which would otherwise make every subsequent resync's `get_dir` fail and re-trigger a resync on
/// each event: a tight loop).
#[shared_test_runtime]
async fn test_add_sync_root_rejects_invalid_uuid() {
	let client = test_utils::RESOURCES.client().await;
	let cache = TestCache::new(&client).await;

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
