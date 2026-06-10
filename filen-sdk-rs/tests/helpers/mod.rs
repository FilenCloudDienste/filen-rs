use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use chrono::Utc;
use filen_sdk_rs::cache::{CacheError, CacheEvent, CacheHandle, CacheMessage, SyncRootCallback};
use filen_sdk_rs::{
	auth::Client,
	crypto::file::FileKey,
	fs::{
		HasUUID,
		dir::meta::{DecryptedDirectoryMeta, DirectoryMeta},
		file::meta::{DecryptedFileMeta, FileMeta},
	},
	io::{RemoteDirectory, RemoteFile},
	socket::DecryptedSocketEvent,
};
use filen_types::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, UuidStr},
	traits::CowHelpersExt,
};
use rusqlite::{Connection, OpenFlags, params};
use tokio::sync::Mutex;
use uuid::Uuid;

/// Generate a unique temporary cache DB path.
pub fn temp_cache_path() -> PathBuf {
	let mut path = std::env::temp_dir();
	path.push(format!("filen_cache_test_{}.db", Uuid::new_v4()));
	path
}

/// Configure the cache to sync the WHOLE account (the account root as the sole sync root, with a no-op
/// callback) — the behavior these tests were written against, now that the production default is
/// explicit sync roots.
pub fn whole_account_sync_root(client: &Client) -> Vec<(Uuid, SyncRootCallback)> {
	let root: Uuid = client.root().uuid().into();
	let noop: SyncRootCallback = Box::new(|_: &mut dyn Iterator<Item = &CacheEvent<'_>>| {});
	vec![(root, noop)]
}

/// The `CacheError`s carried by a status message, or empty for non-error messages (e.g.
/// `SyncRootsDeleted`). Lets the error-assertion helpers below ignore the message variant.
pub fn message_errors(msg: &CacheMessage) -> &[CacheError] {
	match msg {
		CacheMessage::Error(errors) => errors,
		CacheMessage::SyncRootsDeleted(_) => &[],
	}
}

/// Open a read-only SQLite connection to the cache DB.
pub fn open_read_db(path: &Path) -> rusqlite::Result<Connection> {
	Connection::open_with_flags(
		path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)
}

/// Poll `predicate` every 200 ms until it returns true or `timeout` elapses (returning false).
/// The predicate is checked once before the first sleep.
pub async fn poll_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		if predicate() {
			return true;
		}
		tokio::time::sleep(Duration::from_millis(200)).await;
	}
}

/// Poll until the given UUID appears in the items table, or timeout.
pub async fn poll_for_item(db_path: &Path, uuid: Uuid, timeout: Duration) -> bool {
	poll_until(timeout, || {
		open_read_db(db_path)
			.and_then(|conn| {
				conn.query_row(
					"SELECT COUNT(*) > 0 FROM items WHERE uuid = ?",
					params![uuid],
					|row| row.get(0),
				)
			})
			.unwrap_or(false)
	})
	.await
}

/// Poll until the given UUID is no longer in the items table, or timeout.
pub async fn poll_for_item_absent(db_path: &Path, uuid: Uuid, timeout: Duration) -> bool {
	poll_until(timeout, || {
		open_read_db(db_path)
			.and_then(|conn| {
				conn.query_row(
					"SELECT COUNT(*) = 0 FROM items WHERE uuid = ?",
					params![uuid],
					|row| row.get(0),
				)
			})
			.unwrap_or(false)
	})
	.await
}

/// Query the file metadata stored in the cache for the given UUID.
/// Returns (name, size, mime, parent_uuid) if found.
pub fn query_cached_file(db_path: &Path, uuid: Uuid) -> Option<(String, i64, String, Vec<u8>)> {
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
pub fn query_cached_file_favorite(db_path: &Path, uuid: Uuid) -> Option<bool> {
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
pub fn query_cached_dir(db_path: &Path, uuid: Uuid) -> Option<(String, Option<String>, Vec<u8>)> {
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
pub async fn poll_for_file_name(
	db_path: &Path,
	uuid: Uuid,
	expected_name: &str,
	timeout: Duration,
) -> bool {
	poll_until(timeout, || {
		query_cached_file(db_path, uuid).is_some_and(|(name, ..)| name == expected_name)
	})
	.await
}

/// Poll until the dir's name in the cache matches the expected value, or timeout.
pub async fn poll_for_dir_name(
	db_path: &Path,
	uuid: Uuid,
	expected_name: &str,
	timeout: Duration,
) -> bool {
	poll_until(timeout, || {
		query_cached_dir(db_path, uuid).is_some_and(|(name, ..)| name == expected_name)
	})
	.await
}

/// Poll until the dir's color in the cache matches the expected value, or timeout.
pub async fn poll_for_dir_color(
	db_path: &Path,
	uuid: Uuid,
	expected_color: &str,
	timeout: Duration,
) -> bool {
	poll_until(timeout, || {
		query_cached_dir(db_path, uuid)
			.is_some_and(|(_, color, _)| color.as_deref() == Some(expected_color))
	})
	.await
}

/// Poll until the file's favorite status matches the expected value, or timeout.
pub async fn poll_for_file_favorite(
	db_path: &Path,
	uuid: Uuid,
	expected: bool,
	timeout: Duration,
) -> bool {
	poll_until(timeout, || {
		query_cached_file_favorite(db_path, uuid).is_some_and(|fav| fav == expected)
	})
	.await
}

/// Query the item type for the given UUID. Returns the type code (0=Root, 1=Dir, 2=File).
pub fn query_item_type(db_path: &Path, uuid: Uuid) -> Option<i8> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT type FROM items WHERE uuid = ?",
		params![uuid],
		|row| row.get(0),
	)
	.ok()
}

/// Count all items in the cache (including root).
pub fn count_items(db_path: &Path) -> usize {
	let conn = open_read_db(db_path).unwrap();
	conn.query_row("SELECT COUNT(*) FROM items", [], |row| {
		row.get::<_, usize>(0)
	})
	.unwrap()
}

/// Build a synthetic RemoteFile with decoded metadata for testing ListDirRecursive.
pub fn make_test_remote_file(name: &str, parent: &UuidStr) -> RemoteFile {
	let key_hex = "a".repeat(64);
	let file_key = FileKey::from_str_with_version(&key_hex, FileEncryptionVersion::V3).unwrap();
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
			key: file_key,
			last_modified: now,
			created: Some(now),
			hash: None,
		}),
	)
}

/// Build a synthetic RemoteFile with encrypted (un-decryptable) metadata. The conversion
/// to `CacheableFile` should fail because the meta is not in the `Decoded` variant.
pub fn make_test_remote_file_encrypted_meta(parent: &UuidStr) -> RemoteFile {
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
		FileMeta::Encrypted(EncryptedString(Cow::Owned("encrypted_blob".to_string()))),
	)
}

/// Build a synthetic RemoteFile parented to a non-UUID slot (Trash). Conversion should fail
/// because `CacheableFile` only accepts a real `Uuid` parent.
pub fn make_test_remote_file_bad_parent(name: &str) -> RemoteFile {
	let key_hex = "a".repeat(64);
	let file_key = FileKey::from_str_with_version(&key_hex, FileEncryptionVersion::V3).unwrap();
	let now = Utc::now();
	RemoteFile::from_meta(
		UuidStr::new_v4(),
		ParentUuid::Trash,
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
			key: file_key,
			last_modified: now,
			created: Some(now),
			hash: None,
		}),
	)
}

/// Build a synthetic RemoteDirectory with encrypted (un-decryptable) metadata.
pub fn make_test_remote_dir_encrypted_meta(parent: &UuidStr) -> RemoteDirectory {
	let now = Utc::now();
	RemoteDirectory::from_meta(
		UuidStr::new_v4(),
		ParentUuid::Uuid(*parent),
		DirColor::Default,
		false,
		now,
		DirectoryMeta::Encrypted(EncryptedString(Cow::Owned("encrypted_blob".to_string()))),
	)
}

/// Build a synthetic RemoteDirectory parented to a non-UUID slot (Trash).
pub fn make_test_remote_dir_bad_parent(name: &str) -> RemoteDirectory {
	let now = Utc::now();
	RemoteDirectory::from_meta(
		UuidStr::new_v4(),
		ParentUuid::Trash,
		DirColor::Default,
		false,
		now,
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_string()),
			created: Some(now),
		}),
	)
}

/// Build a synthetic RemoteDirectory with decoded metadata for testing ListDirRecursive.
pub fn make_test_remote_dir(name: &str, parent: &UuidStr) -> RemoteDirectory {
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
pub async fn ensure_socket_ready(client: &Client) {
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

/// Shared queue of `CacheMessage`s captured from a `CacheHandle`'s status callback.
/// Wrapping in an `Arc<Mutex<_>>` is the simplest way to share with the `'static`
/// callback while letting the test thread inspect the contents. Note `Mutex` here is
/// `tokio::sync::Mutex` (async) — lock it with `.lock().await`, not a blocking `.lock()`.
pub type MessageLog = std::sync::Arc<Mutex<Vec<CacheMessage>>>;

/// A test cache wrapping CacheHandle with automatic temp DB cleanup.
pub struct TestCache {
	path: PathBuf,
	pub handle: CacheHandle,
	pub messages: MessageLog,
}

impl TestCache {
	pub async fn new(client: &Arc<Client>) -> Self {
		let path = temp_cache_path();
		let messages: MessageLog = Arc::new(Mutex::new(Vec::new()));
		let messages_cb = messages.clone();
		let handle = CacheHandle::new(
			client.clone(),
			path.clone(),
			whole_account_sync_root(client.as_ref()),
			move |msgs| {
				let messages_cb = messages_cb.clone();
				tokio::task::spawn(async move {
					messages_cb.lock().await.extend(msgs);
				});
			},
		)
		.await
		.unwrap();
		ensure_socket_ready(client).await;
		Self {
			path,
			handle,
			messages,
		}
	}

	pub fn db_path(&self) -> &Path {
		&self.path
	}

	/// Wait until at least `count` messages have been received, then run `inspect` while
	/// holding the lock. Returns whatever the inspection returned, or `None` on timeout.
	pub async fn wait_and_inspect_messages<R>(
		&self,
		count: usize,
		timeout: Duration,
		inspect: impl FnOnce(&[CacheMessage]) -> R,
	) -> Option<R> {
		let deadline = tokio::time::Instant::now() + timeout;
		loop {
			{
				let guard = self.messages.lock().await;
				if guard.len() >= count {
					return Some(inspect(&guard));
				}
			}
			if tokio::time::Instant::now() >= deadline {
				return None;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}
}
