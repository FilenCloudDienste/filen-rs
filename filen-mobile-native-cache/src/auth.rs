use core::panic;
use std::{
	hint::unreachable_unchecked,
	ops::Deref,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard, RwLock},
	time::Instant,
};

use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	auth::{StringifiedClient, http::ClientConfig, unauth::UnauthClient},
	crypto::{shared::DataCrypter, v3::EncryptionKey},
	fs::HasUUID,
	thumbnail::{DEFAULT_THUMBNAIL_MEM_BUDGET, REMOTE_SOURCE_RESIDENT_BYTES},
};
use filen_types::{
	auth::FilenSDKConfig,
	crypto::Blake3Hash,
	fs::{Uuid, UuidStr},
};
use rusqlite::{Connection, ToSql, types::ToSqlOutput};
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedRwLockReadGuard;
use tracing::{debug, info, trace};

use crate::{CacheError, env::BackgroundTasks, sql};

const UNAUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const AUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) const AUTH_CLEANUP_INTERVAL: chrono::TimeDelta = chrono::TimeDelta::minutes(10); // 10 minutes

const DEFAULT_MAX_THUMBNAIL_FILES_BUDGET: u64 = 256 * 1024 * 1024; // 256 MiB
const DEFAULT_MAX_CACHE_FILES_BUDGET: u64 = 768 * 1024 * 1024; // 768 MiB

pub const DB_FILE_NAME: &str = "native_cache.db";

// 1 - initial version, changed how files as stored in cache from flat to per-file directories
// 2 - store uuid/parent as BLOB, add `trashed` flag (trashed items keep their original parent);
//     dropped the synthetic 'trash' row
// 3 - add `items.stable_uuid` (server-minted whole-life file id; FILES ONLY, NULL for dirs/roots,
//     whose own uuid is already their whole-life id). The wipe-and-resync reinit repopulates it
//     from directory listings.
// 4 - add the change-tracking substrate: `items.change_seq` + `items.materialised_at`, the
//     `tombstones` and `change_meta` tables, and the triggers that maintain them. A replica
//     syncing against a pre-4 database would see an empty history, so start everyone fresh.
// 5 - pending-upload guards on the stale sweep and both delete cascades: rows holding an unsent
//     edit (and the dirs sheltering them) survive listing-driven deletion as phantoms instead of
//     losing the marker the launch drain relies on. (The init.sql hash change alone would force
//     the reinit; the bump records why.)
const CACHE_VERSION: u64 = 5;

pub struct AuthCacheState {
	conn: Mutex<Connection>,
	pub(crate) cache_state_file: PathBuf,
	pub(crate) tmp_dir: PathBuf,
	pub(crate) cache_dir: PathBuf,
	pub(crate) thumbnail_dir: PathBuf,
	pub(crate) client: Arc<filen_sdk_rs::auth::Client>,
	pub(crate) last_recents_update: RwLock<Option<Instant>>,
	pub(crate) last_trash_update: RwLock<Option<Instant>>,
	pub(crate) thumbnail_file_budget: u64,
	pub(crate) cache_file_budget: u64,
	/// See [`crate::thumbnail::DEFAULT_THUMBNAIL_ITEM_CONCURRENCY`].
	pub(crate) thumbnail_item_concurrency: usize,
	pub(crate) last_cleanup: tokio::sync::RwLock<Option<DateTime<Utc>>>,
	pub(crate) last_cleanup_sem: tokio::sync::Semaphore,
	/// Path of the SDK cache DB backing live search (see [`crate::search`]). Separate from the
	/// hand-rolled `native_cache.db`; opened lazily on the first search.
	pub(crate) sdk_cache_path: PathBuf,
	/// The one live `cache::search` on the drive root, reused across queries via `set_config`.
	pub(crate) search: tokio::sync::Mutex<Option<crate::search::ActiveSearch>>,
	/// The live socket path (subscription handle + drainer, see [`crate::live`]). Dropping this
	/// state — which is what deauth does — unsubscribes and stops the drainer.
	pub(crate) live: crate::live::LiveState,
	/// The MATERIALIZED CONTAINERS the platform reported (see
	/// [`crate::live`]'s `set_materialized_containers`): the directories the system has synced
	/// to disk and will never re-enumerate on its own. Held unconditionally — losing an id here
	/// silently reproduces the stale-forever bug — and mirrored to `db_state.json` so an offline
	/// launch before the first drain still has the last-known set. The account root is NOT
	/// stored; it is injected at query time, always.
	pub(crate) materialized_containers: RwLock<std::collections::HashSet<Uuid>>,
	/// See [`SavedDBState::drive_watermark`]; this is the live copy, persisted on advance.
	pub(crate) drive_watermark: Mutex<Option<u64>>,
	/// See [`SavedDBState::events_cursor`]; this is the live copy, persisted on advance.
	pub(crate) events_cursor: Mutex<Option<i64>>,
	/// See [`SavedDBState::pending_reconcile`]; this is the live copy, persisted on change.
	/// A COUNTER rather than a flag: zero means nothing is owed, and every mark bumps it, so a
	/// pass can only clear the debt it actually saw (compare-and-clear). A report landing while
	/// a pass is already listing would otherwise be cleared by that pass without ever having
	/// been part of it.
	pub(crate) pending_reconcile: std::sync::atomic::AtomicU64,
	/// Orders server listings against the live applier (the F3 snapshot race): a `dir/content`
	/// listing fetched BEFORE a socket event but applied AFTER it would silently revert the
	/// event — a rename undone, a deleted row resurrected — until the item's next event. Listing
	/// paths hold this as READERS across fetch+apply; the applier takes it as a WRITER per
	/// event, so an event can only apply once every listing already in flight has landed, and a
	/// listing started later carries the event's effect in its own snapshot.
	pub(crate) listing_barrier: tokio::sync::RwLock<()>,
	/// Serialises mutations of a single item's local cache file; see [`crate::file_locks`].
	pub(crate) file_locks: crate::file_locks::FileLocks,
}

pub(crate) enum UnauthReason {
	/// A TRUSTED read said the provider is off: auth.json decrypted fine with
	/// `providerEnabled: false`, or the file is genuinely absent (`disable()` deletes it, and a
	/// never-enabled install never wrote one). The only reason the destructive cleanup may act on.
	Disabled,
	Unauthenticated,
	/// auth.json exists but could not be read or decrypted — a transient IO failure, a
	/// pre-first-unlock launch whose DEK the keychain would not release, a torn write. Fails
	/// closed like `Unauthenticated`, but is NEVER grounds for the destructive cleanup: the
	/// file's actual contents are unknown, and unknown must not read as disabled — that wiped
	/// the whole cache (DB, anchors, pending-upload markers) over a blip.
	Unavailable,
}

pub(crate) struct UnauthCacheState {
	pub(crate) reason: UnauthReason,
}

#[allow(clippy::large_enum_variant)]
// we never actually need to read UnauthCacheState
// we only need to know if we are authenticated
#[allow(private_interfaces)]
pub(crate) enum AuthStatus {
	Authenticated(AuthCacheState),
	Unauthenticated(UnauthCacheState),
}

pub(crate) struct CacheState {
	pub(crate) status: AuthStatus,
	auth_file: Arc<PathBuf>, // to allow async access without cloning
	// AES-256-GCM key used to decrypt auth_file on each read; supplied at construction from the
	// platform Keychain/Keystore. None when the extension couldn't obtain it (or it had the wrong
	// length), which makes decryption fail -> AuthFile::default() -> unauthenticated (fail-closed).
	dek: Option<EncryptionKey>,
	pub(crate) files_dir: PathBuf,
	// Where the SQLite files live (native_cache.db, db_state.json, the SDK search DB). Defaults
	// to files_dir; iOS passes the extension's private container instead — both DBs are WAL (a
	// connection holds a shared lock even while idle) and iOS kills a process suspended while
	// holding a lock on a shared-container file (0xdead10cc), which files_dir (the provider's
	// document storage inside the app group) is.
	pub(crate) db_dir: PathBuf,
	last_update: std::sync::RwLock<Option<Instant>>,
	/// Single-flight gate for the delayed disabled-state wipe confirmation: every
	/// unauthenticated FFI call launches a cleanup task, and each Disabled confirmation sleeps
	/// before re-reading auth.json — without the gate those pile up, each redundantly
	/// re-decrypting the file. Arc so the task can hold it after dropping the state guard.
	pub(crate) disabled_wipe_gate: Arc<tokio::sync::Semaphore>,
	/// Who to tell when working-set tracking changed something. Lives out here rather than on the
	/// authenticated state so it survives a re-auth, which replaces `status` wholesale.
	pub(crate) working_set_listener:
		Mutex<Option<Arc<dyn crate::traits::WorkingSetUpdateListener>>>,
}

#[derive(uniffi::Object)]
pub struct FilenMobileCacheState {
	pub(crate) state: Arc<tokio::sync::RwLock<CacheState>>,
	// Arc so the spawned disable-check can hold it too — every writer must go through it.
	state_write_coordinator: Arc<tokio::sync::Mutex<()>>,
	// allows spawning async tasks to check if the auth file has been updated
	// to disable the provider, will always check if currently disabled
	allow_auth_disable: bool,
	/// Every background task spawned for this state; dropping the state aborts them and
	/// [`shutdown`](Self::shutdown) waits for them. See [`BackgroundTasks`].
	pub(crate) tasks: BackgroundTasks,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedDBState {
	pub(crate) db_hash: Blake3Hash,
	#[serde(default)]
	pub(crate) version: Option<u64>,
	#[serde(default)]
	pub(crate) last_cache_cleanup: Option<DateTime<Utc>>,
	/// Root uuid of the account the on-disk cache belongs to. The db_dir is keyed by the
	/// DOMAIN identifier, which is constant across accounts, so without this stamp an account
	/// swap silently inherits the previous account's whole tree (and its decrypted names).
	/// Checked at open in `db_from_dir`; a mismatch — including the `None` a pre-stamp
	/// db_state.json deserializes to — re-initializes the cache.
	#[serde(default)]
	pub(crate) owner: Option<String>,
	/// Last-known materialized containers (see [`AuthCacheState::materialized_containers`]),
	/// so a launch that never gets a report — offline, or killed early — still serves the set
	/// it served last time.
	#[serde(default)]
	pub(crate) materialized_containers: Option<Vec<UuidStr>>,
	/// The last drive socket event this cache applied (`driveMessageId`), advanced as applies
	/// commit — never before. Compared against `v3/messageIds` on (re)connect: a higher remote
	/// counter means events were missed, and the materialized containers are re-listed to close
	/// the gap. `None` (a fresh or wiped cache, or a pre-watermark file) reads as "everything
	/// may have been missed", which runs the same pass.
	#[serde(default)]
	pub(crate) drive_watermark: Option<u64>,
	/// Set while a reconcile pass is OWED — a container that failed to converge, an event that
	/// failed to apply, or a container reported materialized for the first time — and cleared
	/// only by a pass that converged completely. Durable because the watermark cannot express
	/// it: the very next event advances the watermark past the gap, and a watermark-only gate
	/// would then never run the pass the failure asked for. Same shape as the SDK engine's
	/// `needs_resync`, and for the same reason.
	#[serde(default)]
	pub(crate) pending_reconcile: Option<bool>,
	/// The newest `v3/user/events` second this cache has replayed, in whole seconds — the feed
	/// carries no finer resolution. Resumed from INCLUSIVELY: the newest second may still have
	/// been gaining events when it was read, and re-listing a container twice costs one
	/// listing, where missing one costs freshness until the container is next browsed.
	/// `None` means replay has never run, and the gap can only be closed by a full pass.
	#[serde(default)]
	pub(crate) events_cursor: Option<i64>,
}

impl Default for SavedDBState {
	fn default() -> Self {
		SavedDBState {
			db_hash: *sql::statements::DB_INIT_HASH,
			version: Some(CACHE_VERSION),
			last_cache_cleanup: None,
			owner: None,
			materialized_containers: None,
			drive_watermark: None,
			pending_reconcile: None,
			events_cursor: None,
		}
	}
}

pub(crate) async fn update_saved_db_state_cache_cleanup_time(
	state_file_path: &Path,
	timestamp: DateTime<Utc>,
) -> Result<(), CacheError> {
	update_saved_db_state(state_file_path, |saved_state| {
		saved_state.last_cache_cleanup = Some(timestamp);
	})
	.await
}

/// Read-modify-writes `db_state.json`. The durable home of everything that must survive the
/// process without being worth a schema change (an `init.sql` edit wipes the account's cache).
///
/// Written via a pid-suffixed temp file + rename, never in place, and serialized on one
/// process-wide lock: the live path calls this per applied socket event, and an unserialized
/// read-modify-write pair loses one side's update — while a torn or defaulted file is far
/// worse, because `db_from_dir` answers an unparseable or owner-less state file with a full
/// cache wipe, pending-upload markers included. The rename keeps every crash presenting either
/// the old state or the new one; a READ failure (other than the file being absent) aborts the
/// update rather than clobbering a good file with defaults.
pub(crate) async fn update_saved_db_state(
	state_file_path: &Path,
	update: impl FnOnce(&mut SavedDBState),
) -> Result<(), CacheError> {
	static WRITER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
	let _writer = WRITER.lock().await;
	let contents = match tokio::fs::read_to_string(state_file_path).await {
		Ok(contents) => contents,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
		// A transient read failure over a GOOD file must not become a default state: the
		// default's `owner: None` reads as an account mismatch and wipes the cache next open.
		Err(e) => return Err(e.into()),
	};
	// An UNPARSEABLE file is already a wipe sentence at the next open; rewriting it as a
	// parseable default is no worse, and lets the fields written below survive.
	let mut saved_state = serde_json::from_str::<SavedDBState>(&contents).unwrap_or_default();
	update(&mut saved_state);
	let serialized = serde_json::to_string(&saved_state)
		.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	let mut tmp = state_file_path.as_os_str().to_owned();
	tmp.push(format!(".{}.tmp", std::process::id()));
	let tmp = PathBuf::from(tmp);
	tokio::fs::write(&tmp, serialized.as_bytes()).await?;
	tokio::fs::rename(&tmp, state_file_path).await?;
	Ok(())
}

/// Puts a connection into the configuration the schema is written against. Must be called on
/// every connection before anything else runs — the pragmas are per-connection state (only
/// `journal_mode` persists in the DB file), so setting them at `init_db` time alone would leave
/// every later open running with SQLite's defaults: `recursive_triggers` OFF caps
/// `cascade_on_delete_delete_children` at one level and orphans grandchildren, and
/// `foreign_keys` ON is otherwise only a compile-time default of the bundled SQLite.
///
/// Also registers the connection-local `uuid_text(blob)` SQL function, which renders a 16-byte
/// BLOB uuid as its canonical lowercase-hyphenated text. Used by queries that need a uuid as a
/// string (path-component fallback when metadata isn't decoded, and uuid-based path navigation)
/// now that `items.uuid` is stored as a BLOB.
pub(crate) fn configure_conn(conn: &Connection) -> Result<(), rusqlite::Error> {
	use rusqlite::functions::FunctionFlags;
	conn.execute_batch(
		// busy_timeout goes first so it also covers the statements under it — journal_mode
		// especially, since converting or checkpointing a WAL takes locks and is the likeliest
		// here to meet a busy one. The contention it answers is entirely intra-process: only the
		// iOS file provider and the Android documents provider ever open this database, never the
		// app. Two connections still overlap when a displaced cache state closes (a checkpoint on
		// close) while a fresh one is already writing, and when the system runs two provider
		// instances at once. Failing those outright with SQLITE_BUSY surfaces as a spurious
		// operation error; waiting the moment out is what the caller wanted.
		"PRAGMA busy_timeout = 5000;
		PRAGMA recursive_triggers = TRUE;
		PRAGMA journal_mode = WAL;
		PRAGMA temp_store = MEMORY;
		PRAGMA foreign_keys = ON;",
	)?;
	// The slot-remint TABLE (not the trigger — that needs `items` to exist and is installed by
	// `sql::install_slot_remint_log` once the schema is up). Created here because the identity
	// sweep's query joins against it on every connection, schema or no schema. Deliberately
	// OUTSIDE init.sql: an init.sql edit flips DB_INIT_HASH and wipes the account's cache, which
	// is precisely the data the log protects.
	conn.execute_batch(sql::statements::CREATE_SLOT_REMINTS_TABLE)?;
	conn.create_scalar_function(
		"uuid_text",
		1,
		FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
		|ctx| {
			let blob = ctx.get_raw(0).as_blob()?;
			let uuid = filen_types::fs::Uuid::from_slice(blob)
				.map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
			Ok(UuidText(uuid.into()))
		},
	)
}

struct UuidText(UuidStr);
impl ToSql for UuidText {
	fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
		Ok(ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Text(
			self.0.as_ref().as_bytes(),
		)))
	}
}

fn init_db(
	files_dir: &Path,
	db_dir: &Path,
	cache_state_file: &Path,
	owner: &str,
) -> Result<Connection, CacheError> {
	// A re-init discards the WHOLE on-disk state, not just the DB: the cache/tmp/thumbnail
	// dirs and the SDK search cache all hold the previous state's decrypted names or bytes
	// (another account's, on an owner mismatch), and every uuid keying them dies with the DB.
	// One shared definition of that set — including the DBs' -wal/-shm sidecars, whose stale
	// replay hazard (sqlite.org/howtocorrupt.html §4.4) this also covers — lives in
	// `io::wipe_account_data`. Covers every reinit path (hash mismatch, version bump, owner
	// swap, wipe interrupted between unlinks); `from_sdk_config` recreates the dirs afterwards.
	crate::io::wipe_account_data(files_dir, db_dir)?;
	let db = Connection::open(db_dir.join(DB_FILE_NAME))?;
	configure_conn(&db)?;
	db.execute_batch(sql::statements::INIT)?;
	sql::install_slot_remint_log(&db)?;
	let contents = serde_json::to_string(&SavedDBState {
		owner: Some(owner.to_string()),
		..SavedDBState::default()
	})
	.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	std::fs::write(cache_state_file, contents)?;
	Ok(db)
}

/// Opens the account's cache DB, re-initializing it whenever the state file says the one on
/// disk cannot be trusted. The state comes back only when the DB is the one it describes: a
/// re-init has just written a default `db_state.json`, and `from_sdk_config` seeds the live gap
/// bookkeeping — watermark, events cursor, reconcile debt, materialized set — from what is
/// returned here. Carried over a wiped DB, a watermark reads as "caught up": no pass runs,
/// nothing fills the cache, and a replay over the empty DB closes the gap having listed nothing.
fn db_from_dir(
	files_dir: &Path,
	db_dir: &Path,
	cache_state_file: &Path,
	owner: &str,
) -> Result<(Connection, Option<SavedDBState>), CacheError> {
	// Unlike files_dir (system-provided document storage), a relocated db_dir is OURS to
	// create — don't rely on the platform caller having done it (the Swift side's
	// createDirectory failure is deliberately non-fatal there).
	std::fs::create_dir_all(db_dir)?;
	let db_path = db_dir.join(DB_FILE_NAME);
	let state_file = match std::fs::read_to_string(cache_state_file) {
		Ok(contents) => contents,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None));
		}
		Err(e) => {
			tracing::error!("Failed to read db_state.json, error: {e}");
			return Err(e.into());
		}
	};

	let parsed_saved_state = match serde_json::from_str::<SavedDBState>(&state_file) {
		Ok(state) => Some(state),
		Err(e) => {
			tracing::error!("Failed to parse db_state.json, error: {e}");
			None
		}
	};

	let Some(saved_state) = parsed_saved_state else {
		tracing::info!(
			"Failed to parse saved DB state, reinitializing database: {}",
			db_path.display()
		);
		return Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None));
	};

	if saved_state.db_hash != *sql::statements::DB_INIT_HASH {
		tracing::info!(
			"Database hash mismatch, reinitializing database. Expected: {:?}, Found: {:?}",
			*sql::statements::DB_INIT_HASH,
			saved_state.db_hash
		);
		Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None))
	} else if !db_path.exists() {
		tracing::info!(
			"Database file does not exist, creating new one: {}",
			db_path.display()
		);
		Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None))
	} else if saved_state.version.is_none_or(|v| v < CACHE_VERSION) {
		tracing::info!(
			"Database version is outdated or missing, reinitializing database: {}",
			db_path.display()
		);
		Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None))
	} else if saved_state.owner.as_deref() != Some(owner) {
		// The cache belongs to a different account (or predates the owner stamp, which reads
		// the same way): its tree — and its decrypted names — must not leak into this session.
		tracing::info!(
			"Database owner mismatch (saved: {:?}, current: {owner}), reinitializing database: {}",
			saved_state.owner,
			db_path.display()
		);
		Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None))
	} else {
		tracing::info!(
			"Database hash matches, using existing database: {}",
			db_path.display()
		);
		match Connection::open(&db_path).and_then(|conn| {
			configure_conn(&conn)?;
			sql::install_slot_remint_log(&conn)?;
			Ok(conn)
		}) {
			Ok(conn) => Ok((conn, Some(saved_state))),
			// A corrupt or not-a-database file would otherwise fail every query forever: this
			// reuse branch never re-inits on its own, and (on iOS) the disabled wipe is
			// unreachable once the domain is gone — effectively reinstall-only. The cache is
			// authoritative for nothing, so rebuild it.
			Err(rusqlite::Error::SqliteFailure(e, msg))
				if matches!(
					e.code,
					rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
				) =>
			{
				tracing::error!("cached database unusable ({e:?}: {msg:?}); reinitializing");
				Ok((init_db(files_dir, db_dir, cache_state_file, owner)?, None))
			}
			Err(e) => Err(e.into()),
		}
	}
}

#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthFile {
	pub provider_enabled: bool,
	pub sdk_config: Option<FilenSDKConfig>,
	#[serde(default)]
	pub max_thumbnail_files_budget: Option<u64>,
	#[serde(default)]
	pub max_cache_files_budget: Option<u64>,
	/// Thumbnail policy. Absent means the file-provider extension's tuned values (see
	/// [`thumbnail_client_config`]); the SDK's own defaults are sized for a whole app process
	/// and would not survive this extension's jetsam ceiling.
	#[serde(default)]
	pub thumbnail_mem_budget: Option<u64>,
	#[serde(default)]
	pub thumbnail_max_source_bytes: Option<u64>,
	#[serde(default)]
	pub thumbnail_decode_concurrency: Option<u32>,
	/// Items in flight in the bulk batch loop, NOT decodes — the SDK has no notion of a
	/// thumbnail batch, so this one is applied here rather than through `ClientConfig`.
	#[serde(default)]
	pub thumbnail_item_concurrency: Option<u32>,
}

/// The extension decodes under the SAFE preset, not the SDK's app-process default: it has ~20 MB
/// before jetsam, so ONE 12 MiB decode at a time is all that fits — and this gate also covers the
/// single-item `get_thumbnail` path (Android's only route). auth.json may raise either.
fn thumbnail_client_config(settings: &AuthFile) -> ClientConfig {
	let mut config = ClientConfig::default()
		.with_thumbnail_mem_budget(settings.thumbnail_mem_budget.map_or(
			DEFAULT_THUMBNAIL_MEM_BUDGET,
			|budget| {
				// Same floor `JsClientConfig` applies to the same knob, for the same reason:
				// the remote path subtracts the chunk source's two resident slots, so a budget
				// at or below that saturates to 0 and refuses every REMOTE thumbnail while
				// local ones keep working — a silent, half-broken state a host cannot see.
				usize::try_from(budget)
					.unwrap_or(usize::MAX)
					.max(2 * REMOTE_SOURCE_RESIDENT_BYTES)
			},
		))
		// 0 is floored to 1 by the SDK, so no clamp here.
		.with_thumbnail_decode_concurrency(
			settings.thumbnail_decode_concurrency.unwrap_or(1) as usize
		);
	if let Some(max_source_bytes) = settings.thumbnail_max_source_bytes {
		config = config.with_thumbnail_max_source_bytes(max_source_bytes);
	}
	config
}

/// `None` means the file's contents are UNKNOWN (unreadable, undecryptable, or unparseable) —
/// distinct from the trusted default an absent file yields, because callers must fail closed on
/// unknown without ever treating it as an affirmative disable.
fn parse_auth_file(result: Result<String, std::io::Error>) -> Option<AuthFile> {
	match result {
		Ok(content) => {
			let auth_file: serde_json::Result<AuthFile> = serde_json::from_str(&content);
			match auth_file {
				Ok(auth_file) => Some(auth_file),
				Err(e) => {
					tracing::error!("Failed to parse auth file, error: {e}");
					None
				}
			}
		}
		// A genuinely absent file is a TRUSTED disabled state: `disable()` deletes auth.json,
		// and a never-enabled install never wrote one.
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			info!("Auth file not found");
			Some(AuthFile::default())
		}
		Err(e) => {
			tracing::error!("Failed to read auth file, error: {e}");
			None
		}
	}
}

/// Decrypts the on-disk auth.json blob written by the app (fileProvider.ts).
/// Format: version(1 byte, 0x01) ++ nonce(12) ++ ciphertext ++ tag(16), AES-256-GCM, no AAD —
/// after the version byte this is exactly the SDK's [`DataCrypter`] data format.
/// A missing key or any format/version/decrypt failure returns an InvalidData error so
/// parse_auth_file falls through to AuthFile::default() (unauthenticated / fail-closed).
fn decrypt_auth_bytes(bytes: &[u8], dek: Option<&EncryptionKey>) -> Result<String, std::io::Error> {
	const AUTH_FILE_VERSION: u8 = 0x01;
	let dek = dek.ok_or_else(|| {
		std::io::Error::new(std::io::ErrorKind::InvalidData, "missing auth file key")
	})?;
	if bytes.first() != Some(&AUTH_FILE_VERSION) {
		return Err(std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"unrecognized auth file format",
		));
	}
	let mut data = bytes[1..].to_vec();
	dek.blocking_decrypt_data(&mut data).map_err(|_| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"auth file decryption failed",
		)
	})?;
	String::from_utf8(data).map_err(|_| {
		std::io::Error::new(
			std::io::ErrorKind::InvalidData,
			"auth file plaintext not utf-8",
		)
	})
}

fn sync_get_auth_file(path: &Path, dek: Option<&EncryptionKey>) -> Option<AuthFile> {
	parse_auth_file(std::fs::read(path).and_then(|bytes| decrypt_auth_bytes(&bytes, dek)))
}

pub(crate) async fn async_get_auth_file(
	path: &Path,
	dek: Option<&EncryptionKey>,
) -> Option<AuthFile> {
	parse_auth_file(
		tokio::fs::read(path)
			.await
			.and_then(|bytes| decrypt_auth_bytes(&bytes, dek)),
	)
}

impl CacheState {
	/// Clones of the handles the delayed disable confirmation needs, so [`confirm_disabled`]
	/// can run after the state read guard has been dropped.
	pub(crate) fn wipe_confirmation_handles(&self) -> (Arc<PathBuf>, Option<EncryptionKey>) {
		(self.auth_file.clone(), self.dek)
	}
}

/// Re-reads auth.json and answers whether a TRUSTED read (still) says the provider is off.
/// The destructive cleanup calls this — after [`crate::io`]'s confirm delay — before wiping
/// anything: an unreadable file, or one that came back enabled, aborts the wipe.
pub(crate) async fn confirm_disabled(auth_file: &Path, dek: Option<&EncryptionKey>) -> bool {
	matches!(
		async_get_auth_file(auth_file, dek).await,
		Some(auth_file) if !auth_file.provider_enabled
	)
}

fn update_state(state: &mut CacheState, auth_file: Option<AuthFile>) {
	let Some(mut auth_file) = auth_file else {
		debug!("Auth file unavailable; failing closed without treating it as a disable");
		state.status = AuthStatus::Unauthenticated(UnauthCacheState {
			reason: UnauthReason::Unavailable,
		});
		let mut last_update = state.last_update.write().unwrap();
		last_update.replace(Instant::now());
		return;
	};
	if auth_file.provider_enabled {
		match auth_file.sdk_config.take() {
			Some(config) => {
				match AuthCacheState::from_sdk_config(
					config,
					&auth_file,
					&state.files_dir,
					&state.db_dir,
				) {
					Ok(auth_state) => {
						info!("Authenticated with Filen SDK");
						state.status = AuthStatus::Authenticated(auth_state);
					}
					Err(e) => {
						tracing::error!("Failed to create AuthCacheState: {e}");
						state.status = AuthStatus::Unauthenticated(UnauthCacheState {
							reason: UnauthReason::Unauthenticated,
						});
					}
				};
			}
			None => {
				debug!("Auth file does not contain SDK config, setting to unauthenticated");
				state.status = AuthStatus::Unauthenticated(UnauthCacheState {
					reason: UnauthReason::Unauthenticated,
				});
			}
		}
	} else {
		debug!("Provider is disabled, setting to disabled");
		state.status = AuthStatus::Unauthenticated(UnauthCacheState {
			reason: UnauthReason::Disabled,
		});
	}
	let mut last_update = state.last_update.write().unwrap();
	last_update.replace(Instant::now());
}

impl FilenMobileCacheState {
	fn match_state<T>(&self, state: T, now: Instant) -> Option<T>
	where
		T: Deref<Target = CacheState>,
	{
		// read and immediately drop lock
		let lock = state.last_update.read().unwrap();
		let last_update = *lock;
		std::mem::drop(lock);

		match (&state.status, last_update, self.allow_auth_disable) {
			(AuthStatus::Authenticated(_), last_update, true) => {
				if last_update.is_none_or(|last_update| now - last_update > AUTH_UPDATE_INTERVAL) {
					let mut last_update = state.last_update.write().unwrap();
					*last_update = Some(Instant::now());
					std::mem::drop(last_update);

					let auth_file_path = state.auth_file.clone();
					let dek = state.dek;
					let state_arc = self.state.clone();
					let coordinator = self.state_write_coordinator.clone();

					// run the update but do it async
					self.tasks.spawn(async move {
						let auth_file = async_get_auth_file(&auth_file_path, dek.as_ref()).await;
						// Only a TRUSTED read demotes an authenticated session: an unavailable
						// file is no evidence of a logout, and demoting on it tore down a
						// session whose credentials were already validated over a read blip.
						if let Some(auth_file) = auth_file
							&& (!auth_file.provider_enabled || auth_file.sdk_config.is_none())
						{
							// Behind the same coordinator every other writer takes, so the
							// coordinated try_read/try_write fast paths stay truthful (their
							// expects assume no uncoordinated writer exists).
							let _coordinator_guard = coordinator.lock().await;
							update_state(&mut *state_arc.write().await, Some(auth_file));
						}
					});
				}
			}
			(AuthStatus::Unauthenticated(_), last_update, _) => {
				if last_update.is_none_or(|last_update| now - last_update > UNAUTH_UPDATE_INTERVAL)
				{
					return None;
				}
			}
			_ => {}
		}
		Some(state)
	}

	async fn async_get_cache_state_borrowed(&self) -> tokio::sync::RwLockReadGuard<'_, CacheState> {
		let state = self.state.read().await;
		let now = Instant::now();

		// If the state is valid and up to date, return it
		if let Some(state) = self.match_state(state, now) {
			return state;
		}

		// otherwise we need to update the state, but we only need one thread to do this
		// so we use a coordinator
		let _coordinator_guard = self.state_write_coordinator.lock().await;
		let state = self
			.state
			.try_read()
			.expect("Coordinated read access should always succeed");

		// check again after acquiring the coordinator lock
		if let Some(state) = self.match_state(state, now) {
			return state;
		}

		let mut write_state = self.state.write().await;

		// actually perform the update
		let auth_file = async_get_auth_file(&write_state.auth_file, write_state.dek.as_ref()).await;

		update_state(&mut write_state, auth_file);
		// A (re)established auth is what the live socket path keys off; idempotent otherwise.
		crate::live::ensure_started(&self.state, &self.tasks);

		write_state.downgrade()
	}

	fn sync_get_cache_state_borrowed_inner(
		&self,
	) -> Option<tokio::sync::RwLockReadGuard<'_, CacheState>> {
		let state = self.state.try_read().ok()?;
		let now = Instant::now();

		// If the state is valid and up to date, return it
		if let Some(state) = self.match_state(state, now) {
			return Some(state);
		}

		// otherwise we need to update the state, but we only need one thread to do this
		// so we use a coordinator
		let _coordinator_guard = self.state_write_coordinator.try_lock().ok()?;
		let mut write_state = self.state.try_write().ok()?;

		let file = sync_get_auth_file(&write_state.auth_file, write_state.dek.as_ref());
		update_state(&mut write_state, file);
		crate::live::ensure_started(&self.state, &self.tasks);

		Some(write_state.downgrade())
	}

	pub(crate) fn sync_get_cache_state_borrowed(
		&self,
	) -> tokio::sync::RwLockReadGuard<'_, CacheState> {
		if let Some(state) = self.sync_get_cache_state_borrowed_inner() {
			return state;
		}
		match tokio::runtime::Handle::try_current() {
			Ok(_) => {
				// there doesn't seem to be a way to resolve this without panicking
				panic!(
					"Synchronous access to async state is not allowed, use async_get_cache_state instead"
				);
			}
			Err(_) => crate::env::get_runtime()
				.block_on(async { self.async_get_cache_state_borrowed().await }),
		}
	}

	pub(crate) async fn async_get_cache_state_owned(
		&self,
	) -> tokio::sync::OwnedRwLockReadGuard<CacheState> {
		let state = self.state.clone().read_owned().await;
		let now = Instant::now();

		// If the state is valid and up to date, return it
		if let Some(state) = self.match_state(state, now) {
			return state;
		}

		// otherwise we need to update the state, but we only need one thread to do this
		// so we use a coordinator
		let _coordinator_guard = self.state_write_coordinator.lock().await;
		let state = self
			.state
			.clone()
			.try_read_owned()
			.expect("Coordinated read access should always succeed");

		// check again after acquiring the coordinator lock
		if let Some(state) = self.match_state(state, now) {
			return state;
		}

		let mut write_state = self.state.clone().write_owned().await;

		// actually perform the update
		let auth_file = async_get_auth_file(&write_state.auth_file, write_state.dek.as_ref()).await;

		update_state(&mut write_state, auth_file);
		crate::live::ensure_started(&self.state, &self.tasks);

		write_state.downgrade()
	}

	fn sync_get_cache_state_owned_inner(
		&self,
	) -> Option<tokio::sync::OwnedRwLockReadGuard<CacheState>> {
		let state = self.state.clone().try_read_owned().ok()?;
		let now = Instant::now();

		// If the state is valid and up to date, return it
		if let Some(state) = self.match_state(state, now) {
			return Some(state);
		}

		// otherwise we need to update the state, but we only need one thread to do this
		// so we use a coordinator
		let _coordinator_guard = self.state_write_coordinator.try_lock().ok()?;
		let mut write_state = self.state.clone().try_write_owned().ok()?;

		let file = sync_get_auth_file(&write_state.auth_file, write_state.dek.as_ref());
		update_state(&mut write_state, file);
		crate::live::ensure_started(&self.state, &self.tasks);

		Some(write_state.downgrade())
	}

	pub(crate) fn sync_get_cache_state_owned(
		&self,
	) -> tokio::sync::OwnedRwLockReadGuard<CacheState> {
		if let Some(state) = self.sync_get_cache_state_owned_inner() {
			return state;
		}
		match tokio::runtime::Handle::try_current() {
			Ok(_) => {
				// there doesn't seem to be a way to resolve this without panicking
				panic!(
					"Synchronous access to async state is not allowed, use async_get_cache_state instead"
				);
			}
			Err(_) => crate::env::get_runtime()
				.block_on(async { self.async_get_cache_state_owned().await }),
		}
	}
}

impl AuthCacheState {
	/// Takes the lock serialising local-cache mutations for a single item, held across the
	/// download or the delete so the two cannot interleave.
	pub(crate) async fn lock_local_file(&self, uuid: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
		self.file_locks.lock(uuid).await
	}

	fn from_sdk_config(
		config: FilenSDKConfig,
		settings: &AuthFile,
		files_dir: &Path,
		db_dir: &Path,
	) -> Result<Self, CacheError> {
		let unauth_client = UnauthClient::from_config(thumbnail_client_config(settings))?;
		let client = unauth_client.from_stringified(config.into())?;

		let cache_state_file = db_dir.join("db_state.json");

		// The account's root uuid is the cache's owner stamp: db_dir is keyed by the domain
		// identifier, shared across accounts, and a mismatch makes db_from_dir re-initialize
		// rather than hand this session another account's tree.
		let owner = client.root().uuid().to_string();
		let (db, state) = db_from_dir(files_dir, db_dir, &cache_state_file, &owner)?;

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir)?;

		// Keep the SDK search DB next to native_cache.db, NOT under cache_dir (which the cache
		// cleanup scans/wipes expecting only per-file uuid subdirectories).
		let sdk_cache_path = db_dir.join(crate::search::SDK_CACHE_DB_NAME);
		let new = Self {
			conn: Mutex::new(db),
			cache_state_file,
			tmp_dir,
			cache_dir,
			thumbnail_dir,
			client: Arc::new(client),
			last_recents_update: RwLock::new(None),
			last_trash_update: RwLock::new(None),
			thumbnail_file_budget: settings
				.max_thumbnail_files_budget
				.unwrap_or(DEFAULT_MAX_THUMBNAIL_FILES_BUDGET),
			cache_file_budget: settings
				.max_cache_files_budget
				.unwrap_or(DEFAULT_MAX_CACHE_FILES_BUDGET),
			thumbnail_item_concurrency: settings.thumbnail_item_concurrency.map_or(
				crate::thumbnail::DEFAULT_THUMBNAIL_ITEM_CONCURRENCY,
				// `for_each_concurrent` panics on a limit of zero.
				|items| (items as usize).max(1),
			),
			last_cleanup: tokio::sync::RwLock::new(
				state.as_ref().and_then(|s| s.last_cache_cleanup),
			),
			last_cleanup_sem: tokio::sync::Semaphore::new(1),
			sdk_cache_path,
			search: tokio::sync::Mutex::new(None),
			live: Default::default(),
			materialized_containers: RwLock::new(
				state
					.as_ref()
					.and_then(|s| s.materialized_containers.as_ref())
					.map(|uuids| uuids.iter().map(|uuid| Uuid::from(*uuid)).collect())
					.unwrap_or_default(),
			),
			drive_watermark: Mutex::new(state.as_ref().and_then(|s| s.drive_watermark)),
			events_cursor: Mutex::new(state.as_ref().and_then(|s| s.events_cursor)),
			pending_reconcile: std::sync::atomic::AtomicU64::new(
				state
					.as_ref()
					.and_then(|s| s.pending_reconcile)
					.unwrap_or(false)
					.into(),
			),
			listing_barrier: tokio::sync::RwLock::new(()),
			file_locks: crate::file_locks::FileLocks::default(),
		};
		new.add_root(&new.client.root().uuid().to_string())?;
		Ok(new)
	}

	fn from_stringified_in_memory(
		client: StringifiedClient,
		files_dir: &str,
	) -> Result<Self, CacheError> {
		debug!(
			"Creating FilenMobileCacheState from strings for email: {}",
			client.email
		);

		let unauth_client =
			UnauthClient::from_config(thumbnail_client_config(&AuthFile::default()))?;
		let client = unauth_client.from_stringified(client)?;

		let cache_state_file = std::convert::AsRef::<Path>::as_ref(files_dir).join("db_state.json");

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir.as_ref())?;
		// No owner check here: the DB is in-memory and db_state.json is never read, so there is
		// no previous account's tree to inherit.
		let db = Connection::open_in_memory()?;
		configure_conn(&db)?;
		db.execute_batch(sql::statements::INIT)?;
		sql::install_slot_remint_log(&db)?;

		let sdk_cache_path =
			std::convert::AsRef::<Path>::as_ref(files_dir).join(crate::search::SDK_CACHE_DB_NAME);
		let new = Self {
			client: Arc::new(client),
			conn: Mutex::new(db),
			cache_state_file,
			cache_dir,
			tmp_dir,
			thumbnail_dir,
			last_recents_update: RwLock::new(None),
			last_trash_update: RwLock::new(None),
			thumbnail_file_budget: DEFAULT_MAX_THUMBNAIL_FILES_BUDGET,
			cache_file_budget: DEFAULT_MAX_CACHE_FILES_BUDGET,
			thumbnail_item_concurrency: crate::thumbnail::DEFAULT_THUMBNAIL_ITEM_CONCURRENCY,
			last_cleanup: tokio::sync::RwLock::new(None),
			last_cleanup_sem: tokio::sync::Semaphore::new(1),
			sdk_cache_path,
			search: tokio::sync::Mutex::new(None),
			live: Default::default(),
			materialized_containers: RwLock::new(Default::default()),
			drive_watermark: Mutex::new(None),
			events_cursor: Mutex::new(None),
			pending_reconcile: std::sync::atomic::AtomicU64::new(0),
			listing_barrier: tokio::sync::RwLock::new(()),
			file_locks: crate::file_locks::FileLocks::default(),
		};
		new.add_root(&new.client.root().uuid().to_string())?;
		Ok(new)
	}

	/// The slot-remint rescuer over this state's connection, cache directory and per-item locks
	/// (see [`crate::io::SlotRescue`]).
	pub(crate) fn slot_rescue(&self) -> crate::io::SlotRescue<'_> {
		crate::io::SlotRescue {
			conn: &self.conn,
			cache_dir: &self.cache_dir,
			locks: &self.file_locks,
		}
	}

	/// The raw connection mutex, for the helpers ([`crate::io::SlotRescue`], the live applier)
	/// that need per-statement access without borrowing the whole state.
	pub(crate) fn conn_mutex(&self) -> &Mutex<Connection> {
		&self.conn
	}

	pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
		match self.conn.lock() {
			Ok(conn) => conn,
			// continue if poisoned
			Err(poisoned) => {
				tracing::warn!(
					"Cache connection is poisoned, continuing with poisoned state: {poisoned:?}"
				);
				poisoned.into_inner()
			}
		}
	}
}

impl FilenMobileCacheState {
	/// Keeps the live socket path up across its own failures: `ensure_started` is one atomic
	/// load when it already is, and the auth-transition call alone would leave a failed
	/// subscription down for the whole process (`update_state` never re-runs for a state that
	/// stays authenticated). Production (auth-file) states only — the in-memory test
	/// constructor is deliberately manual, so the shared test state never grows a socket by
	/// side effect.
	fn ensure_live_updates(&self) {
		if self.allow_auth_disable {
			crate::live::ensure_started(&self.state, &self.tasks);
		}
	}

	pub(crate) fn sync_execute_authed<T>(
		&self,
		f: impl FnOnce(&AuthCacheState) -> Result<T, CacheError> + Send,
	) -> Result<T, CacheError> {
		trace!("sync_execute_authed");
		self.ensure_live_updates();
		let state = self.sync_get_cache_state_borrowed();
		match &state.status {
			AuthStatus::Authenticated(auth_state) => f(auth_state),
			AuthStatus::Unauthenticated(unauth_state) => {
				self.sync_launch_cleanup_task();
				match unauth_state.reason {
					UnauthReason::Disabled => {
						Err(CacheError::Disabled("Disabled: sync_execute_authed".into()))
					}
					UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
						"Unauthenticated: sync_execute_authed".into(),
					)),
					UnauthReason::Unavailable => Err(CacheError::Unauthenticated(
						"Auth state unavailable: sync_execute_authed".into(),
					)),
				}
			}
		}
	}

	pub(crate) async fn async_execute_authed_owned<T>(
		&self,
		f: impl AsyncFnOnce(OwnedRwLockReadGuard<CacheState, AuthCacheState>) -> Result<T, CacheError>
		+ Send,
	) -> Result<T, CacheError> {
		trace!("async_execute_authed_owned");
		self.ensure_live_updates();
		let state = self.async_get_cache_state_owned().await;
		match &state.status {
			AuthStatus::Authenticated(_) => {
				let new_guard = OwnedRwLockReadGuard::map(state, |state| match state.status {
					AuthStatus::Authenticated(ref auth_cache_state) => auth_cache_state,
					// SAFETY: We just checked that the status is Authenticated, so this is safe
					#[allow(unsafe_code)]
					AuthStatus::Unauthenticated(_) => unsafe { unreachable_unchecked() },
				});
				// we check for cleanup separately so we don't spawn an unnecessary task and try to reacquire the lock for no reason
				let should_cleanup = new_guard.should_cleanup().await;
				let res = f(new_guard).await;
				if should_cleanup {
					self.async_launch_cleanup_task().await;
				}
				res
			}
			AuthStatus::Unauthenticated(unauth_state) => {
				self.async_launch_cleanup_task().await;
				match unauth_state.reason {
					UnauthReason::Disabled => Err(CacheError::Disabled(
						"Disabled: async_execute_authed_owned".into(),
					)),
					UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
						"Unauthenticated: async_execute_authed_owned".into(),
					)),
					UnauthReason::Unavailable => Err(CacheError::Unauthenticated(
						"Auth state unavailable: async_execute_authed_owned".into(),
					)),
				}
			}
		}
	}

	pub(crate) fn sync_execute_authed_owned<T>(
		&self,
		f: impl FnOnce(OwnedRwLockReadGuard<CacheState, AuthCacheState>) -> Result<T, CacheError>
		+ Send
		+ 'static,
	) -> Result<T, CacheError> {
		trace!("sync_execute_authed_owned");
		self.ensure_live_updates();
		let state = self.sync_get_cache_state_owned();
		match &state.status {
			AuthStatus::Authenticated(_) => {
				let new_guard = OwnedRwLockReadGuard::map(state, |state| match state.status {
					AuthStatus::Authenticated(ref auth_cache_state) => auth_cache_state,
					// SAFETY: We just checked that the status is Authenticated, so this is safe
					#[allow(unsafe_code)]
					AuthStatus::Unauthenticated(_) => unsafe { unreachable_unchecked() },
				});
				f(new_guard)
			}
			AuthStatus::Unauthenticated(unauth_state) => {
				self.sync_launch_cleanup_task();
				match unauth_state.reason {
					UnauthReason::Disabled => Err(CacheError::Disabled(
						"Disabled: sync_execute_authed_owned".into(),
					)),
					UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
						"Unauthenticated: sync_execute_authed_owned".into(),
					)),
					UnauthReason::Unavailable => Err(CacheError::Unauthenticated(
						"Auth state unavailable: sync_execute_authed_owned".into(),
					)),
				}
			}
		}
	}
}

#[uniffi::export]
impl FilenMobileCacheState {
	#[uniffi::constructor(name = "new")]
	pub fn new(files_dir: String, auth_file: String, dek: Vec<u8>) -> Self {
		let db_dir = files_dir.clone();
		Self::new_internal(files_dir, db_dir, auth_file, dek)
	}

	/// Like [`new`](Self::new), but with the SQLite files (`native_cache.db`, `db_state.json`,
	/// the SDK search DB) rooted at `db_dir` instead of `files_dir`. iOS passes the extension's
	/// private container here: both DBs are WAL (a connection holds a shared lock even while
	/// idle) and iOS kills a process that is suspended while holding a lock on a file in the
	/// shared app-group container (0xdead10cc) — which is where `files_dir` (the provider's
	/// document storage) lives. Deliberately NO migration or cleanup of DB files an earlier
	/// build left at `files_dir` — nothing reads those paths again, and a fresh `db_dir` simply
	/// reinitializes and re-syncs the cache (one-time re-download of materialized content).
	#[uniffi::constructor(name = "new_with_db_dir")]
	pub fn new_with_db_dir(
		files_dir: String,
		db_dir: String,
		auth_file: String,
		dek: Vec<u8>,
	) -> Self {
		Self::new_internal(files_dir, db_dir, auth_file, dek)
	}
}

impl FilenMobileCacheState {
	fn new_internal(files_dir: String, db_dir: String, auth_file: String, dek: Vec<u8>) -> Self {
		crate::env::init_logger();
		debug!(
			"Initializing FilenMobileCacheState with files_dir: {files_dir}, db_dir: {db_dir} and auth_dir: {auth_file}"
		);
		// A key of the wrong length (including the empty "couldn't obtain it" marker) becomes
		// None, which fails auth file decryption -> unauthenticated (fail-closed).
		let dek = <[u8; 32]>::try_from(dek).ok().map(EncryptionKey::new);
		let new = Self {
			state: Arc::new(tokio::sync::RwLock::new(CacheState {
				status: AuthStatus::Unauthenticated(UnauthCacheState {
					reason: UnauthReason::Disabled,
				}),
				auth_file: Arc::new(PathBuf::from(auth_file)),
				dek,
				files_dir: PathBuf::from(files_dir),
				db_dir: PathBuf::from(db_dir),
				last_update: std::sync::RwLock::new(None),
				disabled_wipe_gate: Arc::new(tokio::sync::Semaphore::new(1)),
				working_set_listener: Mutex::new(None),
			})),
			state_write_coordinator: Arc::new(tokio::sync::Mutex::new(())),
			allow_auth_disable: true,
			tasks: BackgroundTasks::default(),
		};
		new.sync_launch_cleanup_task();
		new
	}
}

impl FilenMobileCacheState {
	pub fn from_stringified_in_memory(
		client: StringifiedClient,
		files_dir: &str,
	) -> Result<Self, CacheError> {
		crate::env::init_logger();
		Ok(Self {
			state: Arc::new(tokio::sync::RwLock::new(CacheState {
				status: AuthStatus::Authenticated(AuthCacheState::from_stringified_in_memory(
					client, files_dir,
				)?),
				auth_file: Arc::new(PathBuf::from(files_dir).join("auth.json")),
				// In-memory auth never reads the file (allow_auth_disable = false), so no key needed.
				dek: None,
				files_dir: PathBuf::from(files_dir),
				db_dir: PathBuf::from(files_dir),
				last_update: std::sync::RwLock::new(None),
				disabled_wipe_gate: Arc::new(tokio::sync::Semaphore::new(1)),
				working_set_listener: Mutex::new(None),
			})),
			state_write_coordinator: Arc::new(tokio::sync::Mutex::new(())),
			allow_auth_disable: false,
			tasks: BackgroundTasks::default(),
		})
	}
}

#[filen_macros::create_uniffi_wrapper]
impl FilenMobileCacheState {
	/// Stops every background task this state has spawned — the cleanup sweeps, the auth-file
	/// re-check, the live path — and returns once they are gone, so nothing of the state
	/// outlives the call. The tasks are what keep the authenticated state, and the SQLite
	/// connection inside it, alive past a drop: a drop aborts them too, but an abort only lands
	/// at the task's next yield on a runtime thread, and a caller that reopens the same
	/// directory right after — a fresh instance, a wipe — found the DB still open (a sharing
	/// violation on Windows; elsewhere a task working on a file it no longer owns). Terminal:
	/// [`Self::stop_live_updates`] is the call for pausing the live path on a state that stays
	/// in use.
	pub async fn shutdown(&self) {
		self.tasks.shutdown().await;
	}
}

#[cfg(test)]
mod auth_file_crypto_tests {
	use super::decrypt_auth_bytes;
	use filen_sdk_rs::crypto::{shared::DataCrypter, v3::EncryptionKey};

	// Mirrors the app-side seal (fileProvider.ts): version(0x01) ++ nonce(12) ++ ciphertext ++ tag(16).
	fn seal(plaintext: &[u8], dek: &EncryptionKey) -> Vec<u8> {
		let mut data = plaintext.to_vec();
		dek.blocking_encrypt_data(&mut data).unwrap();
		let mut out = vec![0x01];
		out.extend_from_slice(&data);
		out
	}

	#[test]
	fn roundtrips_a_valid_blob() {
		let dek = EncryptionKey::new([7u8; 32]);
		let plaintext = br#"{"providerEnabled":true,"sdkConfig":null}"#;
		let blob = seal(plaintext, &dek);
		let decrypted = decrypt_auth_bytes(&blob, Some(&dek)).expect("valid blob should decrypt");
		assert_eq!(decrypted.as_bytes(), plaintext);
	}

	#[test]
	fn rejects_unknown_version_byte() {
		let dek = EncryptionKey::new([7u8; 32]);
		let mut blob = seal(b"hello", &dek);
		blob[0] = 0x02;
		assert!(decrypt_auth_bytes(&blob, Some(&dek)).is_err());
	}

	#[test]
	fn rejects_wrong_key() {
		let blob = seal(b"hello", &EncryptionKey::new([7u8; 32]));
		assert!(decrypt_auth_bytes(&blob, Some(&EncryptionKey::new([8u8; 32]))).is_err());
	}

	#[test]
	fn rejects_missing_key() {
		let blob = seal(b"hello", &EncryptionKey::new([7u8; 32]));
		assert!(decrypt_auth_bytes(&blob, None).is_err());
	}

	#[test]
	fn rejects_truncated_or_empty_blob() {
		let dek = EncryptionKey::new([7u8; 32]);
		assert!(decrypt_auth_bytes(&[0x01, 0x00], Some(&dek)).is_err());
		assert!(decrypt_auth_bytes(&[], Some(&dek)).is_err());
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	/// Removes the DB directory when dropped so a failing assertion doesn't leak temp files.
	pub(crate) struct TempDbDir(pub(crate) std::path::PathBuf);

	impl TempDbDir {
		pub(crate) fn create(prefix: &str) -> Self {
			let dir =
				TempDbDir(std::env::temp_dir().join(format!("{prefix}-{}", rand::random::<u64>())));
			std::fs::create_dir_all(&dir.0).unwrap();
			dir
		}
	}

	impl Drop for TempDbDir {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}
}

#[cfg(test)]
mod connection_pragma_tests {
	use rusqlite::Connection;

	use super::{configure_conn, test_support::TempDbDir};
	use crate::sql;

	// The pragmas are per-connection state, so they must arrive via `configure_conn` (which every
	// open path calls), not via `init.sql` (which only the creation path executes). Pin that on a
	// REOPENED connection — the shape of `db_from_dir`'s reuse branch — a delete still cascades to
	// grandchildren: with `recursive_triggers` at SQLite's default OFF,
	// `cascade_on_delete_delete_children` cannot re-enter and the grandchild survives.
	#[test]
	fn reopened_connection_cascades_past_the_first_generation() {
		let dir = TempDbDir::create("filen-cache-pragma-test");
		let db_path = dir.0.join("native_cache.db");

		{
			let conn = Connection::open(&db_path).unwrap();
			configure_conn(&conn).unwrap();
			conn.execute_batch(sql::statements::INIT).unwrap();
		}

		let conn = Connection::open(&db_path).unwrap();
		configure_conn(&conn).unwrap();

		// root dir (A) -> child dir (B) -> grandchild file (C)
		conn.execute_batch(
			"INSERT INTO items (uuid, stable_uuid, parent, type)
			VALUES
			(x'AA000000000000000000000000000000', NULL, NULL, 1),
			(x'BB000000000000000000000000000000', NULL, x'AA000000000000000000000000000000', 1),
			(
				x'CC000000000000000000000000000000',
				x'DD000000000000000000000000000000',
				x'BB000000000000000000000000000000',
				2
			);",
		)
		.unwrap();

		conn.execute(
			"DELETE FROM items WHERE uuid = x'AA000000000000000000000000000000'",
			[],
		)
		.unwrap();

		let remaining: i64 = conn
			.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap();
		assert_eq!(
			remaining, 0,
			"cascade stopped early: a reopened connection is missing PRAGMA recursive_triggers"
		);
	}
}

#[cfg(test)]
mod owner_stamp_tests {
	use super::{db_from_dir, test_support::TempDbDir};

	const OWNER_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
	const OWNER_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

	fn open(dir: &TempDbDir, owner: &str) -> rusqlite::Connection {
		let state_file = dir.0.join("db_state.json");
		db_from_dir(&dir.0, &dir.0, &state_file, owner).unwrap().0
	}

	fn insert_marker(conn: &rusqlite::Connection) {
		conn.execute_batch(
			"INSERT INTO items (uuid, stable_uuid, parent, type)
			VALUES (x'AA000000000000000000000000000000', NULL, NULL, 1);",
		)
		.unwrap();
	}

	fn marker_count(conn: &rusqlite::Connection) -> i64 {
		conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
			.unwrap()
	}

	/// Everything account A's session left on disk, beyond the DB itself: the decrypted bytes
	/// and names that would leak into account B's session if only the DB were recreated.
	fn litter(dir: &TempDbDir) -> Vec<std::path::PathBuf> {
		let mut paths = Vec::new();
		for sub in ["cache", "tmp", "thumbnails"] {
			let slot = dir.0.join(sub).join("11111111-1111-1111-1111-111111111111");
			std::fs::create_dir_all(&slot).unwrap();
			let file = slot.join("decrypted-name.txt");
			std::fs::write(&file, b"account A's bytes").unwrap();
			paths.push(file);
		}
		// The SDK search cache and its WAL, which carry decrypted names. Never opened by
		// db_from_dir, so fake contents are safe here.
		for name in ["sdk_search_cache.db", "sdk_search_cache.db-wal"] {
			let file = dir.0.join(name);
			std::fs::write(&file, b"account A's names").unwrap();
			paths.push(file);
		}
		paths
	}

	/// The point of the stamp: one physical db_dir, keyed by the constant domain identifier, must
	/// not hand account B the tree account A left behind — neither the DB rows nor the decrypted
	/// bytes and names in the cache/tmp/thumbnail dirs and the SDK search cache. A swap that only
	/// recreated the DB would leave the actual leak in place.
	#[test]
	fn a_different_owner_wipes_everything_the_previous_account_left() {
		let dir = TempDbDir::create("filen-cache-owner-swap");
		insert_marker(&open(&dir, OWNER_A));
		let leftovers = litter(&dir);

		// Same owner: the cache is theirs, so ALL of it is kept.
		let conn = open(&dir, OWNER_A);
		assert_eq!(marker_count(&conn), 1, "same owner must keep the cache");
		for path in &leftovers {
			assert!(path.exists(), "same owner must keep {}", path.display());
		}
		drop(conn);

		let conn = open(&dir, OWNER_B);
		assert_eq!(
			marker_count(&conn),
			0,
			"another account's tree survived the swap"
		);
		for sub in ["cache", "tmp", "thumbnails"] {
			assert!(
				!dir.0.join(sub).exists(),
				"account A's {sub} dir survived the swap"
			);
		}
		for path in &leftovers {
			assert!(!path.exists(), "{} survived the swap", path.display());
		}
	}

	/// A db_state.json written before the stamp existed has no `owner` field. It must parse (not
	/// error) and read as a mismatch — exactly one re-init, after which the stamped file holds.
	#[test]
	fn a_pre_stamp_db_state_reinitializes_exactly_once() {
		let dir = TempDbDir::create("filen-cache-owner-upgrade");
		let state_file = dir.0.join("db_state.json");
		insert_marker(&open(&dir, OWNER_A));

		// Strip the stamp to recreate the pre-upgrade format, leaving hash/version as written.
		let mut state: serde_json::Value =
			serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
		assert!(state.as_object_mut().unwrap().remove("owner").is_some());
		std::fs::write(&state_file, serde_json::to_string(&state).unwrap()).unwrap();

		let conn = open(&dir, OWNER_A);
		assert_eq!(marker_count(&conn), 0, "the upgrade re-init must happen");
		insert_marker(&conn);
		drop(conn);

		let conn = open(&dir, OWNER_A);
		assert_eq!(marker_count(&conn), 1, "one re-init on upgrade, not two");
	}
}

#[cfg(test)]
mod reinit_state_tests {
	use filen_types::fs::{Uuid, UuidStr};

	use super::{DB_FILE_NAME, SavedDBState, db_from_dir, test_support::TempDbDir};
	use crate::auth::update_saved_db_state;

	const OWNER_A: &str = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
	const OWNER_B: &str = "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb";

	/// The gap bookkeeping the constructor seeds from the state `db_from_dir` hands back.
	fn gap_state(state: Option<SavedDBState>) -> (Option<u64>, Option<i64>, Option<bool>, bool) {
		state
			.map(|s| {
				(
					s.drive_watermark,
					s.events_cursor,
					s.pending_reconcile,
					s.materialized_containers.is_some(),
				)
			})
			.unwrap_or_default()
	}

	async fn record_a_caught_up_session(state_file: &std::path::Path) {
		update_saved_db_state(state_file, |state| {
			state.drive_watermark = Some(42);
			state.events_cursor = Some(7);
			state.pending_reconcile = Some(true);
			state.materialized_containers = Some(vec![UuidStr::from(Uuid::from_bytes([1; 16]))]);
		})
		.await
		.unwrap();
	}

	/// A re-init wipes the DB and writes a default db_state.json, and what it hands back must
	/// describe THAT file, not the one the wipe discarded. The constructor seeds the live gap
	/// bookkeeping from it — watermark, events cursor, reconcile debt, materialized set — and a
	/// watermark carried over an emptied DB reads as "caught up": no pass runs, nothing fills the
	/// cache, and a replay over the empty DB folds every event as irrelevant and closes the gap
	/// having listed nothing.
	#[tokio::test]
	async fn a_reinit_hands_back_no_state_from_before_the_wipe() {
		let dir = TempDbDir::create("filen-cache-reinit-state");
		let state_file = dir.0.join("db_state.json");
		drop(db_from_dir(&dir.0, &dir.0, &state_file, OWNER_A).unwrap());
		record_a_caught_up_session(&state_file).await;

		// An account swap: the re-init every install can hit.
		let (conn, state) = db_from_dir(&dir.0, &dir.0, &state_file, OWNER_B).unwrap();
		assert_eq!(
			gap_state(state),
			(None, None, None, false),
			"an owner swap carried the previous account's gap state into the wiped cache"
		);
		drop(conn);

		// The DB gone under a valid state file: what a disable-wipe leaves behind, since it
		// deletes the DB but keeps db_state.json.
		record_a_caught_up_session(&state_file).await;
		std::fs::remove_file(dir.0.join(DB_FILE_NAME)).unwrap();
		let (_conn, state) = db_from_dir(&dir.0, &dir.0, &state_file, OWNER_B).unwrap();
		assert_eq!(
			gap_state(state),
			(None, None, None, false),
			"a re-created DB carried the gap state of the DB it replaced"
		);
	}
}

#[cfg(test)]
mod pending_reconcile_state_tests {
	use super::{SavedDBState, test_support::TempDbDir, update_saved_db_state};

	async fn read(path: &std::path::Path) -> SavedDBState {
		serde_json::from_str(&tokio::fs::read_to_string(path).await.unwrap()).unwrap()
	}

	/// The debt and the watermark are independent halves of the same file: a pass that failed
	/// records the debt, and every event that follows advances the watermark right past the gap.
	/// If an advance could clear (or overwrite) the debt, the failed pass would never be retried
	/// — which is exactly the bug the flag exists to close.
	#[tokio::test]
	async fn a_watermark_advance_leaves_the_reconcile_debt_standing() {
		let dir = TempDbDir::create("filen-cache-pending-reconcile");
		let state_file = dir.0.join("db_state.json");

		update_saved_db_state(&state_file, |state| state.pending_reconcile = Some(true))
			.await
			.unwrap();
		for id in [7u64, 99, 1000] {
			update_saved_db_state(&state_file, |state| state.drive_watermark = Some(id))
				.await
				.unwrap();
		}
		let state = read(&state_file).await;
		assert_eq!(state.drive_watermark, Some(1000));
		assert_eq!(
			state.pending_reconcile,
			Some(true),
			"the watermark ran away with the events; the owed pass must still be owed"
		);

		update_saved_db_state(&state_file, |state| state.pending_reconcile = Some(false))
			.await
			.unwrap();
		let state = read(&state_file).await;
		assert_eq!(
			state.pending_reconcile,
			Some(false),
			"a converged pass clears the debt"
		);
		assert_eq!(state.drive_watermark, Some(1000), "and keeps the watermark");
	}

	/// The upgrade path: a db_state.json written by a build without the field must PARSE, not
	/// error — an unparseable state file is a full cache wipe, pending-upload markers included.
	#[tokio::test]
	async fn a_state_file_without_the_field_parses_as_nothing_owed() {
		let dir = TempDbDir::create("filen-cache-pending-reconcile-upgrade");
		let state_file = dir.0.join("db_state.json");
		update_saved_db_state(&state_file, |state| state.drive_watermark = Some(4))
			.await
			.unwrap();

		let mut json: serde_json::Value =
			serde_json::from_str(&std::fs::read_to_string(&state_file).unwrap()).unwrap();
		assert!(
			json.as_object_mut()
				.unwrap()
				.remove("pendingReconcile")
				.is_some(),
			"the field must be written under its camelCase name"
		);
		std::fs::write(&state_file, serde_json::to_string(&json).unwrap()).unwrap();

		let state = read(&state_file).await;
		assert_eq!(state.pending_reconcile, None);
		assert_eq!(
			state.drive_watermark,
			Some(4),
			"the rest survives the parse"
		);
	}
}
