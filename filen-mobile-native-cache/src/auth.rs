use core::panic;
use std::{
	hint::unreachable_unchecked,
	ops::Deref,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard, RwLock},
	time::Instant,
};

use filen_types::{auth::FilenSDKConfig, crypto::Sha256Hash};
use log::{debug, info, trace};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedRwLockReadGuard;

use crate::{CacheError, sql};

const UNAUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const AUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub const DB_FILE_NAME: &str = "native_cache.db";

pub struct AuthCacheState {
	conn: Mutex<Connection>,
	pub(crate) tmp_dir: PathBuf,
	pub(crate) cache_dir: PathBuf,
	pub(crate) thumbnail_dir: PathBuf,
	pub(crate) client: filen_sdk_rs::auth::Client,
	pub(crate) last_recents_update: RwLock<Option<Instant>>,
	pub(crate) last_trash_update: RwLock<Option<Instant>>,
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
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SavedDBState {
	pub(crate) db_hash: Sha256Hash,
}

fn init_db(db_path: &Path, state_file_path: &Path) -> Result<Connection, CacheError> {
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
	let contents = serde_json::to_string(&SavedDBState {
		db_hash: *sql::statements::DB_INIT_HASH,
	})
	.map_err(|e| CacheError::conversion(format!("Failed to serialize db_state.json: {e}")))?;
	std::fs::write(state_file_path, contents)?;
	Ok(db)
}

fn db_from_files_dir(files_dir: &Path) -> Result<Connection, CacheError> {
	let db_path = files_dir.join(DB_FILE_NAME);
	let state_file_path = files_dir.join("db_state.json");
	let state_file = match std::fs::read_to_string(&state_file_path) {
		Ok(contents) => contents,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			return init_db(&db_path, &state_file_path);
		}
		Err(e) => {
			log::error!("Failed to read db_state.json, error: {e}");
			return Err(e.into());
		}
	};

	let saved_state: SavedDBState = serde_json::from_str(&state_file)
		.map_err(|e| CacheError::conversion(format!("Failed to parse db_state.json: {e}")))?;
	if saved_state.db_hash != *sql::statements::DB_INIT_HASH {
		log::info!(
			"Database hash mismatch, reinitializing database. Expected: {:?}, Found: {:?}",
			*sql::statements::DB_INIT_HASH,
			saved_state.db_hash
		);
		init_db(&db_path, &state_file_path)
	} else if !db_path.exists() {
		log::info!(
			"Database file does not exist, creating new one: {}",
			db_path.display()
		);
		init_db(&db_path, &state_file_path)
	} else {
		log::info!(
			"Database hash matches, using existing database: {}",
			db_path.display()
		);
		Connection::open(db_path).map_err(Into::into)
	}
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct AuthFile {
	pub provider_enabled: bool,
	pub sdk_config: Option<FilenSDKConfig>,
}

fn parse_auth_file(result: Result<String, std::io::Error>) -> AuthFile {
	match result {
		Ok(content) => {
			let auth_file: serde_json::Result<AuthFile> = serde_json::from_str(&content);
			match auth_file {
				Ok(auth_file) => auth_file,
				Err(e) => {
					log::error!("Failed to parse auth file, error: {e}");
					AuthFile {
						provider_enabled: false,
						sdk_config: None,
					}
				}
			}
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			info!("Auth file not found");
			AuthFile {
				provider_enabled: false,
				sdk_config: None,
			}
		}
		Err(e) => {
			log::error!("Failed to read auth file, error: {e}");
			AuthFile {
				provider_enabled: false,
				sdk_config: None,
			}
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
				match AuthCacheState::from_sdk_config(config, &state.files_dir) {
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
	fn from_sdk_config(config: FilenSDKConfig, files_dir: &Path) -> Result<Self, CacheError> {
		let client = filen_sdk_rs::auth::Client::from_strings(
			config.email,
			&config.base_folder_uuid,
			&config.master_keys.join("|"), // hope this works
			&config.private_key,
			config.api_key,
			config.auth_version as u32,
		)?;

		let db = db_from_files_dir(files_dir)?;

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir)?;

		let new = Self {
			conn: Mutex::new(db),
			tmp_dir,
			cache_dir,
			thumbnail_dir,
			client,
			last_recents_update: RwLock::new(None),
			last_trash_update: RwLock::new(None),
		};
		new.add_root(&config.base_folder_uuid)?;
		Ok(new)
	}

	fn from_strings_in_memory(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
		files_dir: &str,
	) -> Result<Self, CacheError> {
		debug!("Creating FilenMobileCacheState from strings for email: {email}");
		let client = filen_sdk_rs::auth::Client::from_strings(
			email,
			root_uuid,
			auth_info,
			private_key,
			api_key,
			version,
		)?;

		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir.as_ref())?;
		let db = Connection::open_in_memory()?;
		db.execute_batch(sql::statements::INIT)?;

		let new = Self {
			client,
			conn: Mutex::new(db),
			cache_dir,
			tmp_dir,
			thumbnail_dir,
			last_recents_update: RwLock::new(None),
			last_trash_update: RwLock::new(None),
		};
		new.add_root(root_uuid)?;
		Ok(new)
	}

	pub(crate) fn conn(&self) -> MutexGuard<Connection> {
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
				f(new_guard).await
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
	pub fn from_strings_in_memory(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
		files_dir: &str,
	) -> Result<Self, CacheError> {
		crate::env::init_logger();
		Ok(Self {
			state: Arc::new(tokio::sync::RwLock::new(CacheState {
				status: AuthStatus::Authenticated(AuthCacheState::from_strings_in_memory(
					email,
					root_uuid,
					auth_info,
					private_key,
					api_key,
					version,
					files_dir,
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
