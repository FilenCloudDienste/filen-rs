// Shared between the integration-test binaries (cache_tests, cache_search_tests); each binary
// compiles its own copy and uses a subset.
#![allow(dead_code)]

use std::{
	borrow::Cow,
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	time::Duration,
};

use chrono::Utc;
use filen_sdk_rs::cache::{
	CacheError, CacheEvent, CacheMessage, ResyncProgress, SyncRootCallback, SyncRootHandle,
};
use filen_sdk_rs::{
	auth::Client,
	crypto::file::FileKey,
	fs::{
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
use uuid::Uuid;

/// Generate a unique temporary cache DB path.
pub fn temp_cache_path() -> PathBuf {
	let mut path = std::env::temp_dir();
	path.push(format!("filen_cache_test_{}.db", Uuid::new_v4()));
	path
}

/// A no-op sync-root notification callback.
pub fn noop_sync_root_callback() -> SyncRootCallback {
	Box::new(|_: &mut dyn Iterator<Item = &CacheEvent<'_>>| {})
}

/// Derive a private `Client` for the same account (via the stringified round-trip, like
/// `socket_tests` does) so each test owns its own cache slot — a `Client` runs ONE cache worker,
/// and the integration tests run concurrently against the single shared `RESOURCES` client.
pub fn derive_client(base: &Client) -> Arc<Client> {
	Arc::new(
		base.get_unauthed()
			.from_stringified(base.to_stringified())
			.unwrap(),
	)
}

/// The `CacheError`s carried by a status message, or empty for non-error messages (e.g.
/// `SyncRootsDeleted`). Lets the error-assertion helpers below ignore the message variant.
pub fn message_errors(msg: &CacheMessage) -> &[CacheError] {
	match msg {
		CacheMessage::Error(errors) => errors,
		CacheMessage::SyncRootsDeleted(_) | CacheMessage::ResyncProgress(_) => &[],
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

/// Shared queue of `CacheMessage`s captured from the cache's status callback, in ARRIVAL ORDER.
/// Wrapping in an `Arc<Mutex<_>>` shares it with the `'static` status callback while letting the
/// test thread inspect the contents. The `Mutex` is a `std::sync::Mutex` (blocking) ON PURPOSE:
/// the status callback appends synchronously (no task spawn), so messages — and the `Started`/
/// `Finished` resync brackets in particular — never reorder, which [`wait_for_converged_resync`]
/// relies on. Hold the guard only briefly and never across an `.await`.
pub type MessageLog = std::sync::Arc<Mutex<Vec<CacheMessage>>>;

/// A test cache: a DERIVED private client (its own cache slot, configured onto a unique temp DB)
/// syncing `scope_root` — pass the test's own directory, NEVER the account root: the shared test
/// account accumulates data (multi-100k-item perf trees included), and a whole-account populate
/// listing serializes every parallel test behind the drive lock for its multi-second duration.
/// Scoping keeps the resync account-size-independent.
pub struct TestCache {
	path: PathBuf,
	pub client: Arc<Client>,
	pub handle: SyncRootHandle,
	pub messages: MessageLog,
}

impl TestCache {
	pub async fn new(base: &Arc<Client>, scope_root: Uuid) -> Self {
		let client = derive_client(base.as_ref());
		let path = temp_cache_path();
		let messages: MessageLog = Arc::new(Mutex::new(Vec::new()));
		let messages_cb = messages.clone();
		client
			.configure_cache(path.clone(), move |msgs| {
				// Synchronous append (see `MessageLog`): keeps messages in arrival order.
				messages_cb.lock().unwrap().extend(msgs);
			})
			.await
			.unwrap();
		// A freshly-created scope dir can hit server propagation lag on the validating
		// `get_dir` — retry rather than flaking (the rejection is otherwise definitive).
		let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
		let handle = loop {
			match client
				.clone()
				.add_sync_root(scope_root, noop_sync_root_callback())
				.await
			{
				Ok(handle) => break handle,
				Err(e) if tokio::time::Instant::now() < deadline => {
					eprintln!("add_sync_root({scope_root}) not accepted yet ({e}); retrying");
					tokio::time::sleep(Duration::from_millis(1000)).await;
				}
				Err(e) => panic!("add_sync_root({scope_root}) kept failing: {e:?}"),
			}
		};
		ensure_socket_ready(&client).await;
		Self {
			path,
			client,
			handle,
			messages,
		}
	}

	pub fn db_path(&self) -> &Path {
		&self.path
	}

	/// Poll the captured status messages until `predicate` over the WHOLE log returns true, or
	/// `timeout` elapses (returning false). Never assume which message arrives first or how
	/// many arrive together — `ResyncProgress` ticks land continuously alongside whatever a
	/// test is actually waiting for.
	pub async fn wait_for_messages(
		&self,
		timeout: Duration,
		predicate: impl Fn(&[CacheMessage]) -> bool,
	) -> bool {
		let deadline = tokio::time::Instant::now() + timeout;
		loop {
			{
				let guard = self.messages.lock().unwrap();
				if predicate(&guard) {
					return true;
				}
			}
			if tokio::time::Instant::now() >= deadline {
				return false;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	}
}

/// A `configure_cache` status callback that appends every [`CacheMessage`] into the returned
/// [`MessageLog`] in ARRIVAL ORDER (synchronous append — no task spawn, so the `Started`/
/// `Finished` resync brackets never reorder across batches). For tests that build the cache
/// inline rather than via [`TestCache`]; pair with [`wait_for_converged_resync`].
pub fn capturing_status_callback() -> (
	MessageLog,
	impl Fn(Vec<CacheMessage>) + Send + Sync + 'static,
) {
	let log: MessageLog = Arc::new(Mutex::new(Vec::new()));
	let cb_log = log.clone();
	(log, move |msgs: Vec<CacheMessage>| {
		cb_log.lock().unwrap().extend(msgs);
	})
}

/// The current number of captured messages. Snapshot this BEFORE an operation, then pass it as
/// `since` to [`wait_for_converged_resync`] so a PRIOR session's convergence (the log persists
/// across worker restarts) is not mistaken for this one's.
pub fn messages_len(log: &MessageLog) -> usize {
	log.lock().unwrap().len()
}

/// Wait until a convergence resync that COVERS `root` commits successfully: a
/// [`ResyncProgress::Finished`] with `converged: true` following a [`ResyncProgress::Started`]
/// whose `roots` include `root`, among messages at index `>= since`.
///
/// This is the DETERMINISTIC signal that the cache now holds a complete listing of `root`'s
/// subtree, so waiting on it lets a test wait EXACTLY as long as the resync actually takes. The
/// drive lock is a fairness-free race with no latency bound (see `run_resync`), so a fixed
/// item-poll window can expire while the resync is still legitimately polling for the lock —
/// `timeout` here is only a generous safety net against a true hang, NOT the expected duration.
pub async fn wait_for_converged_resync(
	log: &MessageLog,
	root: Uuid,
	since: usize,
	timeout: Duration,
) -> bool {
	let deadline = tokio::time::Instant::now() + timeout;
	loop {
		{
			let guard = log.lock().unwrap();
			// Resyncs run sequentially on the worker, so between a `Started` and its `Finished`
			// there is never another `Started`: tracking the most recent `Started`'s coverage and
			// returning on the next successful `Finished` correctly pairs the two.
			let mut covers_root = false;
			for msg in guard.iter().skip(since) {
				match msg {
					CacheMessage::ResyncProgress(ResyncProgress::Started { roots }) => {
						covers_root = roots.contains(&root);
					}
					CacheMessage::ResyncProgress(ResyncProgress::Finished { converged: true })
						if covers_root =>
					{
						return true;
					}
					_ => {}
				}
			}
		}
		if tokio::time::Instant::now() >= deadline {
			return false;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}
