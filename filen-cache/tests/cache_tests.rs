use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	time::Duration,
};

use chrono::Utc;
use filen_cache::CacheHandle;
use filen_macros::shared_test_runtime;
use filen_sdk_rs::{
	auth::Client,
	crypto::file::FileKey,
	fs::{
		HasUUID,
		dir::meta::{DecryptedDirectoryMeta, DirectoryMeta, DirectoryMetaChanges},
		file::meta::{DecryptedFileMeta, FileMeta, FileMetaChanges},
	},
	io::{RemoteDirectory, RemoteFile},
	socket::DecryptedSocketEvent,
};
use filen_types::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	fs::{ParentUuid, UuidStr},
	traits::CowHelpersExt,
};
use rusqlite::{Connection, OpenFlags, params};
use uuid::Uuid;

/// Generate a unique temporary cache DB path.
fn temp_cache_path() -> PathBuf {
	let mut path = std::env::temp_dir();
	path.push(format!("filen_cache_test_{}.db", Uuid::new_v4()));
	path
}

/// Open a read-only SQLite connection to the cache DB.
fn open_read_db(path: &Path) -> rusqlite::Result<Connection> {
	Connection::open_with_flags(
		path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)
}

/// Poll until the given UUID appears in the items table, or timeout.
async fn poll_for_item(db_path: &Path, uuid: Uuid, timeout: Duration) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Ok(conn) = open_read_db(db_path) {
			let exists: Result<bool, _> = conn.query_row(
				"SELECT COUNT(*) > 0 FROM items WHERE uuid = ?",
				params![uuid],
				|row| row.get(0),
			);
			if let Ok(true) = exists {
				return true;
			}
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Poll until the given UUID is no longer in the items table, or timeout.
async fn poll_for_item_absent(db_path: &Path, uuid: Uuid, timeout: Duration) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Ok(conn) = open_read_db(db_path) {
			let absent: Result<bool, _> = conn.query_row(
				"SELECT COUNT(*) = 0 FROM items WHERE uuid = ?",
				params![uuid],
				|row| row.get(0),
			);
			if let Ok(true) = absent {
				return true;
			}
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Query the file metadata stored in the cache for the given UUID.
/// Returns (name, size, mime, parent_uuid) if found.
fn query_cached_file(db_path: &Path, uuid: Uuid) -> Option<(String, i64, String, Vec<u8>)> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT f.name, f.size, f.mime, i.parent
		 FROM items i JOIN files f ON f.id = i.id
		 WHERE i.uuid = ?",
		params![uuid],
		|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
	)
	.ok()
}

/// Query the file's favorite status in the cache for the given UUID.
fn query_cached_file_favorite(db_path: &Path, uuid: Uuid) -> Option<bool> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT f.favorite FROM items i JOIN files f ON f.id = i.id WHERE i.uuid = ?",
		params![uuid],
		|row| row.get(0),
	)
	.ok()
}

/// Query the directory metadata stored in the cache for the given UUID.
/// Returns (name, color, parent_uuid) if found.
fn query_cached_dir(db_path: &Path, uuid: Uuid) -> Option<(String, Option<String>, Vec<u8>)> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT d.name, d.color, i.parent
		 FROM items i JOIN dirs d ON d.id = i.id
		 WHERE i.uuid = ?",
		params![uuid],
		|row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
	)
	.ok()
}

/// Poll until the file's name in the cache matches the expected value, or timeout.
async fn poll_for_file_name(
	db_path: &Path,
	uuid: Uuid,
	expected_name: &str,
	timeout: Duration,
) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Some((name, _, _, _)) = query_cached_file(db_path, uuid)
			&& name == expected_name
		{
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Poll until the dir's name in the cache matches the expected value, or timeout.
async fn poll_for_dir_name(
	db_path: &Path,
	uuid: Uuid,
	expected_name: &str,
	timeout: Duration,
) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Some((name, _, _)) = query_cached_dir(db_path, uuid)
			&& name == expected_name
		{
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Poll until the dir's color in the cache matches the expected value, or timeout.
async fn poll_for_dir_color(
	db_path: &Path,
	uuid: Uuid,
	expected_color: &str,
	timeout: Duration,
) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Some((_, color, _)) = query_cached_dir(db_path, uuid)
			&& color.as_deref() == Some(expected_color)
		{
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Poll until the file's favorite status matches the expected value, or timeout.
async fn poll_for_file_favorite(
	db_path: &Path,
	uuid: Uuid,
	expected: bool,
	timeout: Duration,
) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if let Some(fav) = query_cached_file_favorite(db_path, uuid)
			&& fav == expected
		{
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Query the item type for the given UUID. Returns the type code (0=Root, 1=Dir, 2=File).
fn query_item_type(db_path: &Path, uuid: Uuid) -> Option<i8> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT type FROM items WHERE uuid = ?",
		params![uuid],
		|row| row.get(0),
	)
	.ok()
}

/// Count all items in the cache (including root).
fn count_items(db_path: &Path) -> usize {
	let conn = open_read_db(db_path).unwrap();
	conn.query_row("SELECT COUNT(*) FROM items", [], |row| {
		row.get::<_, usize>(0)
	})
	.unwrap()
}

/// Build a synthetic RemoteFile with decoded metadata for testing ListDirRecursive.
fn make_test_remote_file(name: &str, parent: &UuidStr) -> RemoteFile {
	let key_hex = "a".repeat(64);
	let file_key =
		FileKey::from_string_with_version(Cow::Owned(key_hex), FileEncryptionVersion::V3).unwrap();
	let now = Utc::now();

	RemoteFile::from_meta(
		UuidStr::new_v4(),
		ParentUuid::Uuid(*parent),
		1024,
		1,
		"us-east-1",
		"test-bucket",
		now,
		false,
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_string()),
			size: 1024,
			mime: Cow::Owned("text/plain".to_string()),
			key: Cow::Owned(file_key),
			last_modified: now,
			created: Some(now),
			hash: None,
		}),
	)
}

/// Build a synthetic RemoteDirectory with decoded metadata for testing ListDirRecursive.
fn make_test_remote_dir(name: &str, parent: &UuidStr) -> RemoteDirectory {
	let now = Utc::now();

	RemoteDirectory::from_meta(
		UuidStr::new_v4(),
		ParentUuid::Uuid(*parent),
		DirColor::Default,
		false,
		now,
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_string()),
			created: Some(now),
		}),
	)
}

/// Ensure the socket is authenticated by registering a temporary listener and waiting for authSuccess.
async fn ensure_socket_ready(client: &Client) {
	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
	let _handle = client
		.add_event_listener(
			Box::new(move |event| {
				let _ = tx.send(event.to_owned_cow());
			}),
			Some(vec![Cow::Borrowed("authSuccess")]),
		)
		.await
		.unwrap();

	test_utils::await_event(
		&mut rx,
		|event| *event == DecryptedSocketEvent::AuthSuccess,
		Duration::from_secs(20),
		"authSuccess (cache setup)",
	)
	.await;
}

/// A test cache wrapping CacheHandle with automatic temp DB cleanup.
struct TestCache {
	path: PathBuf,
	handle: CacheHandle,
}

impl TestCache {
	async fn new(client: &Client) -> Self {
		let path = temp_cache_path();
		let handle = CacheHandle::new(client, path.clone()).await.unwrap();
		ensure_socket_ready(client).await;
		Self { path, handle }
	}

	fn db_path(&self) -> &Path {
		&self.path
	}
}

// ─── Initialization Tests ───────────────────────────────────────────────────

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
		tables.contains(&"file_versions".to_string()),
		"file_versions table should exist"
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

// ─── File Event Tests (via Socket) ──────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_new_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_file_new.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client
		.upload_file(file.into(), b"cache test content")
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
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"to be trashed")
		.await
		.unwrap();
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
		let spec = client
			.make_file_builder(&name, *test_dir.uuid())
			.unwrap()
			.build();
		let file = client
			.upload_file(spec.into(), content.as_bytes())
			.await
			.unwrap();
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

// ─── Directory Event Tests (via Socket) ─────────────────────────────────────

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

// ─── File Move Event Tests ──────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_move_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client).await;

	let file = client
		.make_file_builder("cache_file_move.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"moveable content")
		.await
		.unwrap();
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

// ─── Directory Move Event Tests ─────────────────────────────────────────────

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

// ─── Manual Event Tests ─────────────────────────────────────────────────────

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

// ─── Lifecycle Tests ────────────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_shutdown_on_drop() {
	let client = test_utils::RESOURCES.client().await;
	let path = temp_cache_path();

	{
		let _cache = CacheHandle::new(&client, path.clone()).await.unwrap();
	}

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
	let client = &resources.client;
	let path = temp_cache_path();
	let test_dir_uuid = resources.dir.uuid();

	let dir = make_test_remote_dir("reopen_test_dir", test_dir_uuid);
	let dir_uuid: Uuid = dir.uuid().into();
	{
		let cache = CacheHandle::new(client, path.clone()).await.unwrap();
		ensure_socket_ready(client).await;
		cache
			.update_list_dir_recursive(vec![dir], vec![])
			.await
			.unwrap();
		assert!(poll_for_item(&path, dir_uuid, Duration::from_secs(10)).await);
	}

	{
		let _cache = CacheHandle::new(client, path.clone()).await.unwrap();

		assert!(
			poll_for_item(&path, dir_uuid, Duration::from_secs(5)).await,
			"data from previous session should persist"
		);

		let (name, _, _) = query_cached_dir(&path, dir_uuid).unwrap();
		assert_eq!(name, "reopen_test_dir");
	}
}

// ─── Irrelevant Event Tests ─────────────────────────────────────────────────

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

// ─── Combined Scenario Tests ────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_full_file_lifecycle() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(&resources.client).await;

	// 1. Create a directory
	let mut dir = client
		.create_dir(&test_dir.into(), "cache_lifecycle_dir")
		.await
		.unwrap();
	let dir_uuid: Uuid = dir.uuid().into();
	assert!(
		poll_for_item(cache.db_path(), dir_uuid, Duration::from_secs(30)).await,
		"lifecycle dir should appear"
	);

	// 2. Upload a file
	let file = client
		.make_file_builder("cache_lifecycle_file.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"lifecycle").await.unwrap();
	let file_uuid: Uuid = file.uuid().into();
	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"lifecycle file should appear"
	);

	// 3. Trash the file
	client.trash_file(&mut file).await.unwrap();
	assert!(
		poll_for_item_absent(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"lifecycle file should be removed after trash"
	);

	// 4. Trash the directory
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

	// 1. Upload a file via the server (triggers socket event)
	let file = client
		.make_file_builder("cache_mixed_socket.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let file = client.upload_file(file.into(), b"socket").await.unwrap();
	let socket_file_uuid: Uuid = file.uuid().into();

	// 2. Insert a file via manual ListDirRecursive
	let manual_file = make_test_remote_file("cache_mixed_manual.txt", test_dir.uuid());
	let manual_file_uuid: Uuid = manual_file.uuid().into();
	cache
		.handle
		.update_list_dir_recursive(vec![], vec![manual_file])
		.await
		.unwrap();

	// 3. Both should end up in the cache
	assert!(
		poll_for_item(cache.db_path(), socket_file_uuid, Duration::from_secs(30)).await,
		"socket-triggered file should appear in cache"
	);
	assert!(
		poll_for_item(cache.db_path(), manual_file_uuid, Duration::from_secs(10)).await,
		"manually-inserted file should appear in cache"
	);
}

// ─── File Restore Tests ────────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_restore_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_restore.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"restore me")
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

	client.restore_file(&mut file).await.unwrap();

	assert!(
		poll_for_item(cache.db_path(), file_uuid, Duration::from_secs(30)).await,
		"file should reappear in cache after FileRestore event"
	);

	let (name, _, _, _) = query_cached_file(cache.db_path(), file_uuid).unwrap();
	assert_eq!(name, "cache_file_restore.txt");
}

// ─── File Metadata Changed Tests ───────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_metadata_changed_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_rename_old.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client.upload_file(file.into(), b"rename me").await.unwrap();
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

// ─── File Deleted Permanently Tests ────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_deleted_permanently_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_perm_delete.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"delete me forever")
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

// ─── Dir Restore Tests ─────────────────────────────────────────────────────

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

// ─── Dir Metadata Changed Tests ────────────────────────────────────────────

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

// ─── Dir Color Changed Tests ───────────────────────────────────────────────

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

// ─── Dir Deleted Permanently Tests ─────────────────────────────────────────

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

	// Should remain absent
	tokio::time::sleep(Duration::from_secs(2)).await;
	assert!(
		query_cached_dir(cache.db_path(), dir_uuid).is_none(),
		"dir should remain absent after permanent delete"
	);
}

// ─── Item Favorite Tests ───────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_item_favorite_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client).await;

	let file = client
		.make_file_builder("cache_file_favorite.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let mut file = client
		.upload_file(file.into(), b"favorite me")
		.await
		.unwrap();
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

// ─── File Archived Tests ───────────────────────────────────────────────────

#[shared_test_runtime]
async fn test_cache_file_archived_via_socket() {
	let resources = test_utils::RESOURCES.get_resources().await;
	let client = &resources.client;
	let test_dir = &resources.dir;
	let cache = TestCache::new(client).await;

	// Upload original file
	let file = client
		.make_file_builder("cache_file_archive.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let _original = client
		.upload_file(file.into(), b"original content")
		.await
		.unwrap();
	let original_uuid: Uuid = _original.uuid().into();

	assert!(
		poll_for_item(cache.db_path(), original_uuid, Duration::from_secs(30)).await,
		"original file should appear in cache"
	);

	// Upload a new file with the same name — this archives the old one
	let replacement = client
		.make_file_builder("cache_file_archive.txt", *test_dir.uuid())
		.unwrap()
		.build();
	let replacement = client
		.upload_file(replacement.into(), b"replacement content")
		.await
		.unwrap();
	let replacement_uuid: Uuid = replacement.uuid().into();

	// Original should be removed (archived) from cache
	assert!(
		poll_for_item_absent(cache.db_path(), original_uuid, Duration::from_secs(30)).await,
		"original file should be removed from cache after FileArchived event"
	);

	// Replacement should appear in cache
	assert!(
		poll_for_item(cache.db_path(), replacement_uuid, Duration::from_secs(30)).await,
		"replacement file should appear in cache"
	);
}
