// Shared between the integration-test binaries (the cache and sync engine targets); each binary
// compiles its own copy and uses a subset.
#![allow(dead_code)]

use std::{
	borrow::Cow,
	collections::BTreeMap,
	panic::{AssertUnwindSafe, resume_unwind},
	path::{Path, PathBuf},
	sync::{Arc, Mutex},
	time::Duration,
};

use chrono::Utc;
use filen_sdk_rs::cache::{
	CacheError, CacheEvent, CacheMessage, ResyncProgress, SyncRootCallback, SyncRootHandle,
	open_sqlite_with_flags,
};
#[cfg(feature = "sync-engine")]
use filen_sdk_rs::sync_engine::{SyncEngine, SyncReport};
use filen_sdk_rs::{
	auth::Client,
	crypto::file::FileKey,
	fs::{
		HasName,
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
	fs::{ParentUuid, StableUuid},
	traits::CowHelpersExt,
};
use futures::FutureExt;
use rusqlite::{Connection, OpenFlags, params};
use uuid::Uuid;

/// Result-column names for the ad-hoc SQL these helpers run against the cache DB.
///
/// The crate's own `cache::sql::columns` is private, so the integration tests keep their own copy.
/// Same invariants: every value here is unique, every result set the tests query has unique column
/// names, and every aggregate/expression column is spelled out with `AS` (SQLite would otherwise
/// name it after the expression text).
pub mod columns {
	pub const ITEMS_PARENT: &str = "parent";
	pub const ITEMS_TYPE: &str = "type";

	pub const FILES_SIZE: &str = "size";
	pub const FILES_MIME: &str = "mime";

	pub const DIRS_COLOR: &str = "color";

	// `name`/`favorite` exist on both `files` and `dirs`, so both are always read through an alias.
	pub const FILE_NAME: &str = "file_name";
	pub const FILE_FAVORITE: &str = "file_favorite";
	pub const DIR_NAME: &str = "dir_name";

	pub const SQLITE_MASTER_NAME: &str = "name";

	/// `SELECT COUNT(*) AS count ...`
	pub const COUNT: &str = "count";
	/// `SELECT COUNT(*) > 0 AS item_exists ...` — `exists` itself is a SQL keyword.
	pub const ITEM_EXISTS: &str = "item_exists";
	/// `SELECT COUNT(*) = 0 AS item_absent ...`
	pub const ITEM_ABSENT: &str = "item_absent";
}

pub use columns::*;

/// The cache's worst-case convergence bound under heavy drive-lock contention — a self-heal
/// resync may need a full patient lock acquisition (~111s) plus a retry cycle before it commits.
/// NOT the expected duration: the happy path settles in well under a second and a state poll exits
/// the instant the condition holds; this only bounds the pathological contended case so a test
/// never gives up while the cache is still legitimately converging. Use it for ANY assertion
/// waiting on cache-converged state — both [`wait_for_converged_resync`] (convergence-driven) and
/// `poll_until`/`poll_for_item` on data that may be transiently lost to a membership gap under an
/// account-wide event shed and then restored only by the self-heal resync.
pub const CACHE_CONVERGE_TIMEOUT: Duration = Duration::from_secs(240);

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
	open_sqlite_with_flags(
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

/// Run `body` with the account's file-versioning setting at `enabled`, holding
/// [`test_utils::locks::USER_VERSIONING`], and put the setting back to what it was afterwards, also
/// when `body` panics: a setting left changed would change what every later test on the shared
/// account sees. For a test that lists file versions, [`with_versions_and_versioning`].
pub async fn with_versioning<T>(
	client: &Client,
	enabled: bool,
	body: impl Future<Output = T>,
) -> T {
	let _versioning_lock = client
		.acquire_lock_with_default(test_utils::locks::USER_VERSIONING)
		.await
		.unwrap();
	let original = client.get_user_info().await.unwrap().versioning_enabled;
	client.set_versioning_enabled(enabled).await.unwrap();
	// Nothing that can fail may sit between the call above and this guard.
	let outcome = AssertUnwindSafe(body).catch_unwind().await;
	client.set_versioning_enabled(original).await.unwrap();
	outcome.unwrap_or_else(|panic| resume_unwind(panic))
}

/// [`with_versioning`], also holding [`test_utils::locks::VERSIONS`] (taken first, the order every
/// test holding both uses) for a test that reads a file's version history.
pub async fn with_versions_and_versioning<T>(
	client: &Client,
	enabled: bool,
	body: impl Future<Output = T>,
) -> T {
	let _versions_lock = client
		.acquire_lock_with_default(test_utils::locks::VERSIONS)
		.await
		.unwrap();
	with_versioning(client, enabled, body).await
}

/// Poll until the given UUID appears in the items table, or timeout.
pub async fn poll_for_item(db_path: &Path, uuid: Uuid, timeout: Duration) -> bool {
	poll_until(timeout, || {
		open_read_db(db_path)
			.and_then(|conn| {
				conn.query_row(
					"SELECT COUNT(*) > 0 AS item_exists FROM items WHERE uuid = ?",
					params![uuid],
					|row| row.get(ITEM_EXISTS),
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
					"SELECT COUNT(*) = 0 AS item_absent FROM items WHERE uuid = ?",
					params![uuid],
					|row| row.get(ITEM_ABSENT),
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
		"SELECT f.name AS file_name, f.size, f.mime, i.parent
		 FROM items i JOIN files f ON f.id = i.id
		 WHERE i.uuid = ?",
		params![uuid],
		|row| {
			Ok((
				row.get(FILE_NAME)?,
				row.get(FILES_SIZE)?,
				row.get(FILES_MIME)?,
				row.get(ITEMS_PARENT)?,
			))
		},
	)
	.ok()
}

/// Query the file's favorite status in the cache for the given UUID.
pub fn query_cached_file_favorite(db_path: &Path, uuid: Uuid) -> Option<bool> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT f.favorite AS file_favorite FROM items i JOIN files f ON f.id = i.id \
		 WHERE i.uuid = ?",
		params![uuid],
		|row| row.get(FILE_FAVORITE),
	)
	.ok()
}

/// Query the directory metadata stored in the cache for the given UUID.
/// Returns (name, color, parent_uuid) if found.
pub fn query_cached_dir(db_path: &Path, uuid: Uuid) -> Option<(String, Option<String>, Vec<u8>)> {
	let conn = open_read_db(db_path).ok()?;
	conn.query_row(
		"SELECT d.name AS dir_name, d.color, i.parent
		 FROM items i JOIN dirs d ON d.id = i.id
		 WHERE i.uuid = ?",
		params![uuid],
		|row| {
			Ok((
				row.get(DIR_NAME)?,
				row.get(DIRS_COLOR)?,
				row.get(ITEMS_PARENT)?,
			))
		},
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
		|row| row.get(ITEMS_TYPE),
	)
	.ok()
}

/// Count all items in the cache (including root).
pub fn count_items(db_path: &Path) -> usize {
	let conn = open_read_db(db_path).unwrap();
	conn.query_row("SELECT COUNT(*) AS count FROM items", [], |row| {
		row.get::<_, usize>(COUNT)
	})
	.unwrap()
}

/// Build a synthetic RemoteFile with decoded metadata for testing ListDirRecursive.
pub fn make_test_remote_file(name: &str, parent: Uuid) -> RemoteFile {
	let key_hex = "a".repeat(64);
	let file_key = FileKey::from_str_with_version(&key_hex, FileEncryptionVersion::V3).unwrap();
	let now = Utc::now();

	let uuid = Uuid::new_v4();
	RemoteFile::from_meta(
		uuid,
		StableUuid::new_for_test(uuid),
		ParentUuid::Uuid(parent),
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
pub fn make_test_remote_file_encrypted_meta(parent: Uuid) -> RemoteFile {
	let now = Utc::now();
	let uuid = Uuid::new_v4();
	RemoteFile::from_meta(
		uuid,
		StableUuid::new_for_test(uuid),
		ParentUuid::Uuid(parent),
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
	let uuid = Uuid::new_v4();
	RemoteFile::from_meta(
		uuid,
		StableUuid::new_for_test(uuid),
		ParentUuid::Trash(Uuid::nil()),
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
pub fn make_test_remote_dir_encrypted_meta(parent: Uuid) -> RemoteDirectory {
	let now = Utc::now();
	RemoteDirectory::from_meta(
		Uuid::new_v4(),
		ParentUuid::Uuid(parent),
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
		Uuid::new_v4(),
		ParentUuid::Trash(Uuid::nil()),
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
pub fn make_test_remote_dir(name: &str, parent: Uuid) -> RemoteDirectory {
	let now = Utc::now();

	RemoteDirectory::from_meta(
		Uuid::new_v4(),
		ParentUuid::Uuid(parent),
		DirColor::Default,
		false,
		now,
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_string()),
			created: Some(now),
		}),
	)
}

/// The file named `name` in a listing, if any.
pub fn find_file<'a>(files: &'a [RemoteFile], name: &str) -> Option<&'a RemoteFile> {
	files.iter().find(|f| f.name() == Some(name))
}

/// The directory named `name` in a listing, if any.
pub fn find_dir<'a>(dirs: &'a [RemoteDirectory], name: &str) -> Option<&'a RemoteDirectory> {
	dirs.iter().find(|d| d.name() == Some(name))
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

/// A fresh, unique local sync-root temp directory.
pub fn fresh_local_dir(tag: &str) -> PathBuf {
	let dir = std::env::temp_dir().join(format!("sync_test_{tag}_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&dir).unwrap();
	dir
}

/// Deterministic content for a relative path (the path bytes), so a pull side can be checked to
/// have reproduced exactly the same bytes.
pub fn content_for(rel: &str) -> Vec<u8> {
	rel.as_bytes().to_vec()
}

/// Write a file (creating parent dirs) under `root`.
pub fn write_file(root: &Path, rel: &str, bytes: &[u8]) {
	let path = root.join(rel);
	std::fs::create_dir_all(path.parent().unwrap()).unwrap();
	std::fs::write(&path, bytes).unwrap();
}

/// True if the file at `rel` under `root` exists and its bytes equal `expected`.
pub fn read_eq(root: &Path, rel: &str, expected: &[u8]) -> bool {
	std::fs::read(root.join(rel)).is_ok_and(|b| b == expected)
}

/// Rename/move `from` -> `to` under `root` (creating the destination's parent dirs).
pub fn move_file(root: &Path, from: &str, to: &str) {
	let dst = root.join(to);
	std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
	std::fs::rename(root.join(from), dst).unwrap();
}

/// One entry in a walked tree: relative path -> (is_dir, file_len, content_bytes). For directories
/// `file_len` is 0 and `content` is empty.
pub type TreeMap = BTreeMap<String, (bool, u64, Vec<u8>)>;

/// Recursively walk `root` into a map keyed by the slash-joined relative path; the root itself is
/// not included. Excludes the engine's local quarantine bin (`.filen-sync-trash`, top-level): it is
/// per-side recovery state, not synced content, so two genuinely-converged trees must compare equal
/// modulo it. Panics when a directory cannot be listed.
pub fn walk_tree(root: &Path) -> TreeMap {
	let mut map = TreeMap::new();
	walk_into(root, root, &mut map);
	map
}

fn walk_into(root: &Path, dir: &Path, map: &mut TreeMap) {
	let mut entries: Vec<_> = std::fs::read_dir(dir)
		.unwrap_or_else(|e| panic!("read_dir {dir:?}: {e}"))
		.map(|e| e.unwrap())
		.collect();
	entries.sort_by_key(|e| e.file_name());
	for entry in entries {
		let path = entry.path();
		let rel = path
			.strip_prefix(root)
			.unwrap()
			.to_string_lossy()
			.replace('\\', "/");
		if rel == ".filen-sync-trash" {
			continue;
		}
		let ft = entry.file_type().unwrap();
		if ft.is_dir() {
			map.insert(rel, (true, 0, Vec::new()));
			walk_into(root, &path, map);
		} else if ft.is_file() {
			let content = std::fs::read(&path).unwrap();
			map.insert(rel, (false, content.len() as u64, content));
		}
		// Symlinks and other kinds are ignored: the engine does not produce them.
	}
}

/// True once the two local trees are byte-for-byte identical (quarantine bin ignored).
pub fn trees_equal(a: &Path, b: &Path) -> bool {
	walk_tree(a) == walk_tree(b)
}

/// Assert two walked trees are byte-for-byte identical, naming the first divergence.
pub fn assert_tree_maps_identical(a: &TreeMap, b: &TreeMap, label_a: &str, label_b: &str) {
	for key in a.keys() {
		assert!(
			b.contains_key(key),
			"path {key:?} present in {label_a} but missing in {label_b}"
		);
	}
	for key in b.keys() {
		assert!(
			a.contains_key(key),
			"path {key:?} present in {label_b} but missing in {label_a}"
		);
	}
	for (key, (a_dir, a_len, a_bytes)) in a {
		let (b_dir, b_len, b_bytes) = b.get(key).unwrap();
		assert_eq!(
			a_dir, b_dir,
			"path {key:?} is a dir in one tree but a file in the other ({label_a}={a_dir} {label_b}={b_dir})"
		);
		assert_eq!(
			a_len, b_len,
			"path {key:?} differs in length ({label_a}={a_len} {label_b}={b_len})"
		);
		assert_eq!(
			a_bytes, b_bytes,
			"path {key:?} differs in content bytes ({label_a} vs {label_b})"
		);
	}
	assert_eq!(
		a.len(),
		b.len(),
		"tree sizes differ: {label_a}={} {label_b}={}",
		a.len(),
		b.len()
	);
}

/// Recursively scan `root` — INCLUDING the engine's quarantine bin, which [`walk_tree`] hides — for
/// any regular file whose bytes equal `needle`. Use this (rather than a path-specific [`read_eq`])
/// to assert that data is recoverable SOMEWHERE in the tree: left in place, written under a
/// conflict-renamed name, or quarantined (e.g. when a remote delete races a local move, the
/// relocated copy is preserved under the quarantine bin rather than at its destination path).
pub fn bytes_recoverable_anywhere(root: &Path, needle: &[u8]) -> bool {
	let mut stack = vec![root.to_path_buf()];
	while let Some(dir) = stack.pop() {
		let Ok(rd) = std::fs::read_dir(&dir) else {
			continue;
		};
		for entry in rd.flatten() {
			let Ok(ft) = entry.file_type() else {
				continue;
			};
			if ft.is_dir() {
				stack.push(entry.path());
			} else if ft.is_file() && std::fs::read(entry.path()).is_ok_and(|b| b == needle) {
				return true;
			}
		}
	}
	false
}

/// The order in which two clients' passes run within a convergence round.
#[derive(Clone, Copy, Debug)]
pub enum Order {
	AFirst,
	BFirst,
	Concurrent,
}

/// Run one sync round (one pass per client) in the given order; returns both reports.
#[cfg(feature = "sync-engine")]
pub async fn sync_round(
	ea: &SyncEngine,
	pa: i64,
	eb: &SyncEngine,
	pb: i64,
	order: Order,
) -> (SyncReport, SyncReport) {
	match order {
		Order::AFirst => {
			let ra = ea.sync_once(pa).await.expect("engine A sync_once");
			let rb = eb.sync_once(pb).await.expect("engine B sync_once");
			(ra, rb)
		}
		Order::BFirst => {
			let rb = eb.sync_once(pb).await.expect("engine B sync_once");
			let ra = ea.sync_once(pa).await.expect("engine A sync_once");
			(ra, rb)
		}
		Order::Concurrent => {
			let (ra, rb) = tokio::join!(ea.sync_once(pa), eb.sync_once(pb));
			(
				ra.expect("engine A sync_once"),
				rb.expect("engine B sync_once"),
			)
		}
	}
}

/// One client of a two-client test: its engine and the pair it syncs.
#[cfg(feature = "sync-engine")]
#[derive(Clone, Copy)]
pub struct Peer<'a> {
	pub engine: &'a SyncEngine,
	pub pair: i64,
}

/// Run sync rounds (in `order`) on two clients until `done` holds, panicking after a bound. Each
/// pass must be error-free; surfaced conflicts are accumulated into `conflicts`. A short settle
/// between rounds lets each client's cache observe the other's just-committed remote writes.
#[cfg(feature = "sync-engine")]
pub async fn converge(
	a: Peer<'_>,
	b: Peer<'_>,
	order: Order,
	conflicts: &mut std::collections::BTreeSet<String>,
	label: &str,
	done: impl Fn() -> bool,
) {
	const MAX_ROUNDS: usize = 20;
	for _ in 0..MAX_ROUNDS {
		let (ra, rb) = sync_round(a.engine, a.pair, b.engine, b.pair, order).await;
		assert!(
			ra.errors.is_empty(),
			"{label}: engine A errors {:?}",
			ra.errors
		);
		assert!(
			rb.errors.is_empty(),
			"{label}: engine B errors {:?}",
			rb.errors
		);
		for c in ra.conflict_paths().chain(rb.conflict_paths()) {
			conflicts.insert(c.to_string());
		}
		if done() {
			return;
		}
		tokio::time::sleep(Duration::from_millis(1500)).await;
	}
	panic!("{label}: did not converge within {MAX_ROUNDS} rounds (order {order:?})");
}
