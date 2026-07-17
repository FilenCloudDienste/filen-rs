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
};
use filen_types::{auth::FilenSDKConfig, crypto::Blake3Hash};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
	sync::OwnedRwLockReadGuard,
};
use tracing::{debug, info, trace};

use crate::{CacheError, sql};

const UNAUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const AUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub(crate) const AUTH_CLEANUP_INTERVAL: chrono::TimeDelta = chrono::TimeDelta::minutes(10); // 10 minutes

const DEFAULT_MAX_THUMBNAIL_FILES_BUDGET: u64 = 256 * 1024 * 1024; // 256 MiB
const DEFAULT_MAX_CACHE_FILES_BUDGET: u64 = 768 * 1024 * 1024; // 768 MiB

pub const DB_FILE_NAME: &str = "native_cache.db";

// 1 - initial version, changed how files as stored in cache from flat to per-file directories
const CACHE_VERSION: u64 = 1;

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
	pub(crate) last_cleanup: tokio::sync::RwLock<Option<DateTime<Utc>>>,
	pub(crate) last_cleanup_sem: tokio::sync::Semaphore,
	/// Path of the SDK cache DB backing live search (see [`crate::search`]). Separate from the
	/// hand-rolled `native_cache.db`; opened lazily on the first search.
	pub(crate) sdk_cache_path: PathBuf,
	/// The one live `cache::search` on the drive root, reused across queries via `set_config`.
	pub(crate) search: tokio::sync::Mutex<Option<crate::search::ActiveSearch>>,
}

enum UnauthReason {
	Disabled,
	Unauthenticated,
}

struct UnauthCacheState {
	reason: UnauthReason,
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
}

#[derive(uniffi::Object)]
pub struct FilenMobileCacheState {
	pub(crate) state: Arc<tokio::sync::RwLock<CacheState>>,
	state_write_coordinator: tokio::sync::Mutex<()>,
	// allows spawning async tasks to check if the auth file has been updated
	// to disable the provider, will always check if currently disabled
	allow_auth_disable: bool,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedDBState {
	pub(crate) db_hash: Blake3Hash,
	#[serde(default)]
	pub(crate) version: Option<u64>,
	#[serde(default)]
	pub(crate) last_cache_cleanup: Option<DateTime<Utc>>,
}

impl Default for SavedDBState {
	fn default() -> Self {
		SavedDBState {
			db_hash: *sql::statements::DB_INIT_HASH,
			version: Some(CACHE_VERSION),
			last_cache_cleanup: None,
		}
	}
}

pub(crate) async fn update_saved_db_state_cache_cleanup_time(
	state_file_path: &Path,
	timestamp: DateTime<Utc>,
) -> Result<(), CacheError> {
	let mut file = tokio::fs::OpenOptions::new()
		.create(true)
		.truncate(false)
		.read(true)
		.write(true)
		.open(state_file_path)
		.await?;
	let mut contents = String::new();
	file.read_to_string(&mut contents).await?;
	let mut saved_state = serde_json::from_str::<SavedDBState>(&contents).unwrap_or_default();
	saved_state.last_cache_cleanup = Some(timestamp);
	contents.clear();
	// SAFETY: serde_json::to_writer always writes valid UTF-8
	serde_json::to_writer(unsafe { contents.as_mut_vec() }, &saved_state)
		.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	file.set_len(0).await?;
	file.seek(std::io::SeekFrom::Start(0)).await?;
	file.write_all(contents.as_bytes()).await?;
	Ok(())
}

fn init_db(db_path: &Path, cache_state_file: &Path) -> Result<Connection, CacheError> {
	// Remove the DB together with its WAL sidecars: a stale -wal surviving next to a recreated
	// DB can be replayed into it on open (sqlite.org/howtocorrupt.html §4.4). Covers every
	// reinit path (hash mismatch, version bump, wipe interrupted between unlinks).
	for suffix in ["", "-wal", "-shm"] {
		let mut os = db_path.as_os_str().to_os_string();
		os.push(suffix);
		let path = std::path::PathBuf::from(os);
		match std::fs::remove_file(&path) {
			Ok(()) => {
				tracing::info!("Removed old database file: {}", path.display());
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
			Err(e) => {
				tracing::error!("Failed to remove old database file {}: {e}", path.display());
				return Err(e.into());
			}
		}
	}
	let db = Connection::open(db_path)?;
	db.execute_batch(sql::statements::INIT)?;
	let contents = serde_json::to_string(&SavedDBState::default())
		.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	std::fs::write(cache_state_file, contents)?;
	Ok(db)
}

fn db_from_dir(
	db_dir: &Path,
	cache_state_file: &Path,
) -> Result<(Connection, Option<SavedDBState>), CacheError> {
	// Unlike files_dir (system-provided document storage), a relocated db_dir is OURS to
	// create — don't rely on the platform caller having done it (the Swift side's
	// createDirectory failure is deliberately non-fatal there).
	std::fs::create_dir_all(db_dir)?;
	let db_path = db_dir.join(DB_FILE_NAME);
	let state_file = match std::fs::read_to_string(cache_state_file) {
		Ok(contents) => contents,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return Ok((init_db(&db_path, cache_state_file)?, None));
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
		return Ok((init_db(&db_path, cache_state_file)?, None));
	};

	if saved_state.db_hash != *sql::statements::DB_INIT_HASH {
		tracing::info!(
			"Database hash mismatch, reinitializing database. Expected: {:?}, Found: {:?}",
			*sql::statements::DB_INIT_HASH,
			saved_state.db_hash
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else if !db_path.exists() {
		tracing::info!(
			"Database file does not exist, creating new one: {}",
			db_path.display()
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else if saved_state.version.is_none_or(|v| v < CACHE_VERSION) {
		tracing::info!(
			"Database version is outdated or missing, reinitializing database: {}",
			db_path.display()
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else {
		tracing::info!(
			"Database hash matches, using existing database: {}",
			db_path.display()
		);
		Ok((Connection::open(db_path)?, Some(saved_state)))
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
}

fn parse_auth_file(result: Result<String, std::io::Error>) -> AuthFile {
	match result {
		Ok(content) => {
			let auth_file: serde_json::Result<AuthFile> = serde_json::from_str(&content);
			match auth_file {
				Ok(auth_file) => auth_file,
				Err(e) => {
					tracing::error!("Failed to parse auth file, error: {e}");
					AuthFile::default()
				}
			}
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			info!("Auth file not found");
			AuthFile::default()
		}
		Err(e) => {
			tracing::error!("Failed to read auth file, error: {e}");
			AuthFile::default()
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

fn sync_get_auth_file(path: &Path, dek: Option<&EncryptionKey>) -> AuthFile {
	parse_auth_file(std::fs::read(path).and_then(|bytes| decrypt_auth_bytes(&bytes, dek)))
}

async fn async_get_auth_file(path: &Path, dek: Option<&EncryptionKey>) -> AuthFile {
	parse_auth_file(
		tokio::fs::read(path)
			.await
			.and_then(|bytes| decrypt_auth_bytes(&bytes, dek)),
	)
}

fn update_state(state: &mut CacheState, auth_file: AuthFile) {
	if auth_file.provider_enabled {
		match auth_file.sdk_config {
			Some(config) => {
				match AuthCacheState::from_sdk_config(
					config,
					&state.files_dir,
					&state.db_dir,
					auth_file
						.max_thumbnail_files_budget
						.unwrap_or(DEFAULT_MAX_THUMBNAIL_FILES_BUDGET),
					auth_file
						.max_cache_files_budget
						.unwrap_or(DEFAULT_MAX_CACHE_FILES_BUDGET),
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

					// run the update but do it async
					crate::env::get_runtime().spawn(async move {
						let auth_file = async_get_auth_file(&auth_file_path, dek.as_ref()).await;
						if !auth_file.provider_enabled || auth_file.sdk_config.is_none() {
							update_state(&mut *state_arc.write().await, auth_file);
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

		Some(write_state.downgrade())
	}

	fn sync_get_cache_state_borrowed(&self) -> tokio::sync::RwLockReadGuard<'_, CacheState> {
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
	fn from_sdk_config(
		config: FilenSDKConfig,
		files_dir: &Path,
		db_dir: &Path,
		max_thumbnail_files_budget: u64,
		max_cache_files_budget: u64,
	) -> Result<Self, CacheError> {
		let unauth_client = UnauthClient::from_config(ClientConfig::default())?;
		let client = unauth_client.from_stringified(config.into())?;

		let cache_state_file = db_dir.join("db_state.json");

		let (db, state) = db_from_dir(db_dir, &cache_state_file)?;

		if state.as_ref().is_none_or(|state| state.version.is_none()) {
			tracing::info!(
				"Database version is missing, removing cache directory to ensure compatibility"
			);
			let (cache_dir, _, _) = crate::io::get_paths(files_dir);
			if let Err(e) = std::fs::remove_dir_all(cache_dir)
				&& e.kind() != std::io::ErrorKind::NotFound
			{
				tracing::error!("Failed to remove cache directory during DB version upgrade: {e}");
				return Err(e.into());
			}
		}

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
			thumbnail_file_budget: max_thumbnail_files_budget,
			cache_file_budget: max_cache_files_budget,
			last_cleanup: tokio::sync::RwLock::new(
				state.as_ref().and_then(|s| s.last_cache_cleanup),
			),
			last_cleanup_sem: tokio::sync::Semaphore::new(1),
			sdk_cache_path,
			search: tokio::sync::Mutex::new(None),
		};
		new.add_root(new.client.root().uuid().as_ref())?;
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

		let unauth_client = UnauthClient::from_config(ClientConfig::default())?;
		let client = unauth_client.from_stringified(client)?;

		let cache_state_file = std::convert::AsRef::<Path>::as_ref(files_dir).join("db_state.json");

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir.as_ref())?;
		let db = Connection::open_in_memory()?;
		db.execute_batch(sql::statements::INIT)?;

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
			last_cleanup: tokio::sync::RwLock::new(None),
			last_cleanup_sem: tokio::sync::Semaphore::new(1),
			sdk_cache_path,
			search: tokio::sync::Mutex::new(None),
		};
		new.add_root(new.client.root().uuid().as_ref())?;
		Ok(new)
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
	pub(crate) fn sync_execute_authed<T>(
		&self,
		f: impl FnOnce(&AuthCacheState) -> Result<T, CacheError> + Send,
	) -> Result<T, CacheError> {
		trace!("sync_execute_authed");
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
		let state = self.async_get_cache_state_owned().await;
		match &state.status {
			AuthStatus::Authenticated(_) => {
				let new_guard = OwnedRwLockReadGuard::map(state, |state| match state.status {
					AuthStatus::Authenticated(ref auth_cache_state) => auth_cache_state,
					// SAFETY: We just checked that the status is Authenticated, so this is safe
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
		let state = self.sync_get_cache_state_owned();
		match &state.status {
			AuthStatus::Authenticated(_) => {
				let new_guard = OwnedRwLockReadGuard::map(state, |state| match state.status {
					AuthStatus::Authenticated(ref auth_cache_state) => auth_cache_state,
					// SAFETY: We just checked that the status is Authenticated, so this is safe
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
			})),
			state_write_coordinator: tokio::sync::Mutex::new(()),
			allow_auth_disable: true,
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
			})),
			state_write_coordinator: tokio::sync::Mutex::new(()),
			allow_auth_disable: false,
		})
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
