use core::panic;
use std::{
	hint::unreachable_unchecked,
	ops::Deref,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard, RwLock},
	time::Instant,
};

use chrono::{DateTime, Utc};
use filen_sdk_rs::{auth::StringifiedClient, fs::HasUUID};
use filen_types::{auth::FilenSDKConfig, crypto::Blake3Hash};
use log::{debug, info, trace};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::{
	io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
	sync::OwnedRwLockReadGuard,
};

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
	pub(crate) client: filen_sdk_rs::auth::Client,
	pub(crate) last_recents_update: RwLock<Option<Instant>>,
	pub(crate) last_trash_update: RwLock<Option<Instant>>,
	pub(crate) thumbnail_file_budget: u64,
	pub(crate) cache_file_budget: u64,
	pub(crate) last_cleanup: tokio::sync::RwLock<Option<DateTime<Utc>>>,
	pub(crate) last_cleanup_sem: tokio::sync::Semaphore,
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
	pub(crate) files_dir: PathBuf,
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

impl FilenMobileCacheState {
	pub async fn dump_db(&self, title: &str) {
		match &self.state.read().await.status {
			AuthStatus::Authenticated(auth_state) => {
				let conn = auth_state.conn();
				sql::dump_db(&conn, title).expect("Failed to dump database");
			}
			AuthStatus::Unauthenticated(_) => {
				log::info!("Cannot dump database, unauthenticated");
			}
		}
	}
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
	match std::fs::remove_file(db_path) {
		Ok(()) => {
			log::info!("Removed old database file: {}", db_path.display());
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			log::info!(
				"Database file not found, creating new one: {}",
				db_path.display()
			);
		}
		Err(e) => {
			log::error!("Failed to remove old database file: {e}");
			return Err(e.into());
		}
	}
	let db = Connection::open(db_path)?;
	db.execute_batch(sql::statements::INIT)?;
	let contents = serde_json::to_string(&SavedDBState::default())
		.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	std::fs::write(cache_state_file, contents)?;
	Ok(db)
}

fn db_from_files_dir(
	files_dir: &Path,
	cache_state_file: &Path,
) -> Result<(Connection, Option<SavedDBState>), CacheError> {
	let db_path = files_dir.join(DB_FILE_NAME);
	let state_file = match std::fs::read_to_string(cache_state_file) {
		Ok(contents) => contents,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return Ok((init_db(&db_path, cache_state_file)?, None));
		}
		Err(e) => {
			log::error!("Failed to read db_state.json, error: {e}");
			return Err(e.into());
		}
	};

	let parsed_saved_state = match serde_json::from_str::<SavedDBState>(&state_file) {
		Ok(state) => Some(state),
		Err(e) => {
			log::error!("Failed to parse db_state.json, error: {e}");
			None
		}
	};

	let Some(saved_state) = parsed_saved_state else {
		log::info!(
			"Failed to parse saved DB state, reinitializing database: {}",
			db_path.display()
		);
		return Ok((init_db(&db_path, cache_state_file)?, None));
	};

	if saved_state.db_hash != *sql::statements::DB_INIT_HASH {
		log::info!(
			"Database hash mismatch, reinitializing database. Expected: {:?}, Found: {:?}",
			*sql::statements::DB_INIT_HASH,
			saved_state.db_hash
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else if !db_path.exists() {
		log::info!(
			"Database file does not exist, creating new one: {}",
			db_path.display()
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else if saved_state.version.is_none_or(|v| v < CACHE_VERSION) {
		log::info!(
			"Database version is outdated or missing, reinitializing database: {}",
			db_path.display()
		);
		Ok((init_db(&db_path, cache_state_file)?, Some(saved_state)))
	} else {
		log::info!(
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
					log::error!("Failed to parse auth file, error: {e}");
					AuthFile::default()
				}
			}
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			info!("Auth file not found");
			AuthFile::default()
		}
		Err(e) => {
			log::error!("Failed to read auth file, error: {e}");
			AuthFile::default()
		}
	}
}

fn sync_get_auth_file(path: &Path) -> AuthFile {
	parse_auth_file(std::fs::read_to_string(path))
}

async fn async_get_auth_file(path: &Path) -> AuthFile {
	parse_auth_file(tokio::fs::read_to_string(path).await)
}

fn update_state(state: &mut CacheState, auth_file: AuthFile) {
	if auth_file.provider_enabled {
		match auth_file.sdk_config {
			Some(config) => {
				match AuthCacheState::from_sdk_config(
					config,
					&state.files_dir,
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
						log::error!("Failed to create AuthCacheState: {e}");
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
					let state_arc = self.state.clone();

					// run the update but do it async
					crate::env::get_runtime().spawn(async move {
						let auth_file = async_get_auth_file(&auth_file_path).await;
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
		let auth_file = async_get_auth_file(&write_state.auth_file).await;

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

		let file = sync_get_auth_file(&write_state.auth_file);
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
		let auth_file = async_get_auth_file(&write_state.auth_file).await;

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

		let file = sync_get_auth_file(&write_state.auth_file);
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
		max_thumbnail_files_budget: u64,
		max_cache_files_budget: u64,
	) -> Result<Self, CacheError> {
		let client = filen_sdk_rs::auth::Client::from_stringified(config.into())?;

		let cache_state_file = files_dir.join("db_state.json");

		let (db, state) = db_from_files_dir(files_dir, &cache_state_file)?;

		if state.as_ref().is_none_or(|state| state.version.is_none()) {
			log::info!(
				"Database version is missing, removing cache directory to ensure compatibility"
			);
			let (cache_dir, _, _) = crate::io::get_paths(files_dir);
			if let Err(e) = std::fs::remove_dir_all(cache_dir)
				&& e.kind() != std::io::ErrorKind::NotFound
			{
				log::error!("Failed to remove cache directory during DB version upgrade: {e}");
				return Err(e.into());
			}
		}

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir)?;

		let new = Self {
			conn: Mutex::new(db),
			cache_state_file,
			tmp_dir,
			cache_dir,
			thumbnail_dir,
			client,
			last_recents_update: RwLock::new(None),
			last_trash_update: RwLock::new(None),
			thumbnail_file_budget: max_thumbnail_files_budget,
			cache_file_budget: max_cache_files_budget,
			last_cleanup: tokio::sync::RwLock::new(
				state.as_ref().and_then(|s| s.last_cache_cleanup),
			),
			last_cleanup_sem: tokio::sync::Semaphore::new(1),
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
		let client = filen_sdk_rs::auth::Client::from_stringified(client)?;

		let cache_state_file = std::convert::AsRef::<Path>::as_ref(files_dir).join("db_state.json");

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir.as_ref())?;
		let db = Connection::open_in_memory()?;
		db.execute_batch(sql::statements::INIT)?;

		let new = Self {
			client,
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
		};
		new.add_root(new.client.root().uuid().as_ref())?;
		Ok(new)
	}

	pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
		match self.conn.lock() {
			Ok(conn) => conn,
			// continue if poisoned
			Err(poisoned) => {
				log::warn!(
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
	pub fn new(files_dir: String, auth_file: String) -> Self {
		crate::env::init_logger();
		debug!(
			"Initializing FilenMobileCacheState with files_dir: {files_dir} and auth_dir: {auth_file}"
		);
		let new = Self {
			state: Arc::new(tokio::sync::RwLock::new(CacheState {
				status: AuthStatus::Unauthenticated(UnauthCacheState {
					reason: UnauthReason::Disabled,
				}),
				auth_file: Arc::new(PathBuf::from(auth_file)),
				files_dir: PathBuf::from(files_dir),
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
				files_dir: PathBuf::from(files_dir),
				last_update: std::sync::RwLock::new(None),
			})),
			state_write_coordinator: tokio::sync::Mutex::new(()),
			allow_auth_disable: false,
		})
	}
}
