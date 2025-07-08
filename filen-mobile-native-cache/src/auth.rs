use core::panic;
use std::{
	hint::unreachable_unchecked,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, MutexGuard},
	time::Instant,
};

use filen_types::auth::FilenSDKConfig;
use log::{debug, info};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::OwnedRwLockReadGuard;

use crate::CacheError;

const UNAUTH_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub struct AuthCacheState {
	conn: Mutex<Connection>,
	pub(crate) tmp_dir: PathBuf,
	pub(crate) cache_dir: PathBuf,
	pub(crate) thumbnail_dir: PathBuf,
	pub(crate) client: filen_sdk_rs::auth::Client,
}

enum UnauthReason {
	Disabled,
	Unauthenticated,
}

struct UnauthCacheState {
	reason: UnauthReason,
	last_update: Option<Instant>,
}

#[allow(clippy::large_enum_variant)]
enum AuthStatus {
	Authenticated(AuthCacheState),
	Unauthenticated(UnauthCacheState),
}

pub(crate) struct CacheState {
	status: AuthStatus,
	auth_file: PathBuf,
	files_dir: PathBuf,
}

#[derive(uniffi::Object)]
pub struct FilenMobileCacheState {
	state: Arc<tokio::sync::RwLock<CacheState>>,
	state_write_coordinator: tokio::sync::Mutex<()>,
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

fn match_state(state: &CacheState, now: Instant) -> bool {
	match &state.status {
		AuthStatus::Authenticated(_) => true,
		AuthStatus::Unauthenticated(unauth_state) => {
			if let Some(last_update) = unauth_state.last_update {
				(last_update - now) < UNAUTH_UPDATE_INTERVAL
			} else {
				false
			}
		}
	}
}

fn match_state_borrowed(
	state: tokio::sync::RwLockReadGuard<'_, CacheState>,
	now: Instant,
) -> Option<tokio::sync::RwLockReadGuard<'_, CacheState>> {
	match_state(&state, now).then_some(state)
}

fn match_state_owned(
	state: tokio::sync::OwnedRwLockReadGuard<CacheState>,
	now: Instant,
) -> Option<tokio::sync::OwnedRwLockReadGuard<CacheState>> {
	match_state(&state, now).then_some(state)
}

fn update_state(state: &mut CacheState, auth_file: AuthFile) {
	if auth_file.provider_enabled {
		match auth_file.sdk_config {
			Some(config) => {
				match AuthCacheState::from_sdk_config(config, &state.files_dir) {
					Ok(auth_state) => state.status = AuthStatus::Authenticated(auth_state),
					Err(e) => {
						log::error!("Failed to create AuthCacheState: {e}");
						state.status = AuthStatus::Unauthenticated(UnauthCacheState {
							reason: UnauthReason::Unauthenticated,
							last_update: Some(Instant::now()),
						});
					}
				};
			}
			None => {
				state.status = AuthStatus::Unauthenticated(UnauthCacheState {
					reason: UnauthReason::Unauthenticated,
					last_update: Some(Instant::now()),
				});
			}
		}
	} else {
		state.status = AuthStatus::Unauthenticated(UnauthCacheState {
			reason: UnauthReason::Disabled,
			last_update: Some(Instant::now()),
		});
	}
}

impl FilenMobileCacheState {
	async fn async_get_cache_state_borrowed(&self) -> tokio::sync::RwLockReadGuard<'_, CacheState> {
		let state = self.state.read().await;
		let now = Instant::now();

		// If the state is valid and up to date, return it
		if let Some(state) = match_state_borrowed(state, now) {
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
		if let Some(state) = match_state_borrowed(state, now) {
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
		if let Some(state) = match_state_borrowed(state, now) {
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
		if let Some(state) = match_state_owned(state, now) {
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
		if let Some(state) = match_state_owned(state, now) {
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
		if let Some(state) = match_state_owned(state, now) {
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

	fn sync_get_cache_state_owned(&self) -> tokio::sync::OwnedRwLockReadGuard<CacheState> {
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

		let db = Connection::open(files_dir.join("native_cache.db"))?;
		db.execute_batch(include_str!("../sql/init.sql"))?;
		let (cache_dir, tmp_dir, thumbnail_dir) = crate::io::init(files_dir)?;

		let new = Self {
			conn: Mutex::new(db),
			tmp_dir,
			cache_dir,
			thumbnail_dir,
			client,
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
		db.execute_batch(include_str!("../sql/init.sql"))?;
		let new = Self {
			client,
			conn: Mutex::new(db),
			cache_dir,
			tmp_dir,
			thumbnail_dir,
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
		let state = self.sync_get_cache_state_borrowed();
		match &state.status {
			AuthStatus::Authenticated(auth_state) => f(auth_state),
			AuthStatus::Unauthenticated(unauth_state) => match unauth_state.reason {
				UnauthReason::Disabled => {
					Err(CacheError::Disabled("Disabled: execute_authed".into()))
				}
				UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
					"Unauthenticated: execute_authed".into(),
				)),
			},
		}
	}

	pub(crate) async fn async_execute_authed_owned<T>(
		&self,
		f: impl AsyncFnOnce(OwnedRwLockReadGuard<CacheState, AuthCacheState>) -> Result<T, CacheError>
		+ Send,
	) -> Result<T, CacheError> {
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
			AuthStatus::Unauthenticated(unauth_state) => match unauth_state.reason {
				UnauthReason::Disabled => {
					Err(CacheError::Disabled("Disabled: execute_authed".into()))
				}
				UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
					"Unauthenticated: execute_authed".into(),
				)),
			},
		}
	}

	pub(crate) fn sync_execute_authed_owned<T>(
		&self,
		f: impl FnOnce(OwnedRwLockReadGuard<CacheState, AuthCacheState>) -> Result<T, CacheError>
		+ Send
		+ 'static,
	) -> Result<T, CacheError> {
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
			AuthStatus::Unauthenticated(unauth_state) => match unauth_state.reason {
				UnauthReason::Disabled => {
					Err(CacheError::Disabled("Disabled: execute_authed".into()))
				}
				UnauthReason::Unauthenticated => Err(CacheError::Unauthenticated(
					"Unauthenticated: execute_authed".into(),
				)),
			},
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
		Self {
			state: Arc::new(tokio::sync::RwLock::new(CacheState {
				status: AuthStatus::Unauthenticated(UnauthCacheState {
					reason: UnauthReason::Disabled,
					last_update: None,
				}),
				auth_file: PathBuf::from(auth_file),
				files_dir: PathBuf::from(files_dir),
			})),
			state_write_coordinator: tokio::sync::Mutex::new(()),
		}
	}

	#[uniffi::constructor(name = "new_from_strings_in_memory")]
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
				auth_file: PathBuf::from(files_dir).join("auth.json"),
				files_dir: PathBuf::from(files_dir),
			})),
			state_write_coordinator: tokio::sync::Mutex::new(()),
		})
	}
}
