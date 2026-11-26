//! The session the desktop app pushes, sealed into the auth.json the cache reads.
//!
//! The app sends the same `AuthFile` JSON it pushes to the macOS extension. The helper checks it
//! as that extension does, seals it under a key held in the [`KeyStore`] and writes it to
//! auth.json in its data directory; the cache, opened once with its existing constructor, picks
//! each new session up on its own re-read of the file. Deleting auth.json is the cache's trusted
//! disable, as on macOS.

#[cfg(any(windows, test))]
mod credential_manager;
mod key_store;

use std::{
	fmt,
	fs::File,
	path::{Path, PathBuf},
	sync::{Mutex, PoisonError},
};

use filen_mobile_native_cache::auth::{AuthFile, FilenMobileCacheState};
use filen_sdk_rs::{
	crypto::{error::ConversionError, shared::DataCrypter},
	io::{remove_file_if_present, write_private_file_atomically},
};
use filen_types::fs::Uuid;
use tracing::warn;

use crate::local::RECORD_FILE_NAMES;

// Exported in tests too, where the store runs on keyring's mock credential on every host.
#[cfg(any(windows, test))]
pub use credential_manager::{CredentialError, CredentialManagerKeyStore};
pub use key_store::{
	KeyAccount, KeyAccountError, KeyLengthError, KeyStore, KeyStoreError, SEALING_KEY_LEN,
	SealingKey,
};

/// The largest payload accepted, in bytes, as the macOS extension's auth service: a real one is a
/// few KiB.
const MAX_AUTH_PAYLOAD_BYTES: usize = 1 << 20;
/// auth.json's format byte, which the cache's reader checks.
const AUTH_FILE_VERSION: u8 = 0x01;
/// The sealed session, in the data directory.
const AUTH_FILE_NAME: &str = "auth.json";
/// What a seal is written to before it is renamed onto auth.json.
const AUTH_TEMP_FILE_NAME: &str = "auth.json.tmp";
/// Locked while the sealing key is loaded or created, so two processes opening the store at once
/// agree on one key.
const KEY_LOCK_FILE_NAME: &str = "key.lock";
/// The cache's SQLite files, kept apart from the data directory's root so the cache's wipe of a
/// disabled session never reaches auth.json.
const DB_DIR_NAME: &str = "db";
/// The cache's file contents.
const FILES_DIR_NAME: &str = "files";
/// Written into a data directory the helper creates, or finds empty, before anything else goes in
/// it: nothing is wiped from a directory without it, nor kept in one that holds anything else.
pub(crate) const DATA_DIR_MARKER_NAME: &str = "filen-cloud-files-data";
/// What the marker says, for a person who finds it.
const DATA_DIR_MARKER_TEXT: &[u8] =
	b"The Filen desktop app's Cloud Files provider keeps a session and a cache here.\n";

/// A pushed `AuthFile` JSON that passed the checks the macOS extension makes: at most 1 MiB, an
/// `AuthFile` as the cache parses it (which also refuses an auth version the SDK does not know),
/// and a session whenever the provider is enabled. `Debug` prints only whether the provider is
/// enabled and whether a session is present: `AuthFile`'s own `Debug` prints the password and
/// keys. A session's account root (`baseFolderUUID`) must be a uuid, as the sync root's identity
/// carries it.
pub struct AuthPayload {
	file: AuthFile,
	owner: Option<SessionOwner>,
}

/// Whose session a payload carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionOwner {
	/// The Filen user id.
	pub user_id: u64,
	/// The uuid of the account's root directory.
	pub account_root: Uuid,
}

/// Why a pushed payload was refused. Nothing here quotes the payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthPayloadError {
	/// The payload is larger than 1 MiB.
	#[error("the payload is {len} bytes, more than {MAX_AUTH_PAYLOAD_BYTES}")]
	TooLarge {
		/// Its length, in bytes.
		len: usize,
	},
	/// The payload is not an `AuthFile`. serde_json's message can quote the payload, so only the
	/// kind of error and its position are kept.
	#[error("the payload is not an AuthFile ({category:?} at {line}:{column})")]
	Malformed {
		/// What kind of error serde_json found.
		category: serde_json::error::Category,
		/// Its line, from 1.
		line: usize,
		/// Its column, from 1.
		column: usize,
	},
	/// The provider is enabled but the payload carries no session.
	#[error("the provider is enabled without a session")]
	EnabledWithoutSession,
	/// The session's account root is not a uuid.
	#[error("the session's account root is not a uuid")]
	MalformedAccountRoot,
}

impl AuthPayload {
	/// Checks the pushed bytes `json`. A payload of exactly 1 MiB is accepted.
	pub fn parse(json: &[u8]) -> Result<Self, AuthPayloadError> {
		if json.len() > MAX_AUTH_PAYLOAD_BYTES {
			return Err(AuthPayloadError::TooLarge { len: json.len() });
		}
		let auth_file: AuthFile =
			serde_json::from_slice(json).map_err(|e| AuthPayloadError::Malformed {
				category: e.classify(),
				line: e.line(),
				column: e.column(),
			})?;
		if auth_file.provider_enabled && auth_file.sdk_config.is_none() {
			return Err(AuthPayloadError::EnabledWithoutSession);
		}
		let owner = auth_file
			.sdk_config
			.as_ref()
			.map(|config| {
				Uuid::parse_str(&config.base_folder_uuid)
					.map(|account_root| SessionOwner {
						user_id: config.user_id,
						account_root,
					})
					.map_err(|_| AuthPayloadError::MalformedAccountRoot)
			})
			.transpose()?;
		Ok(Self {
			file: auth_file,
			owner,
		})
	}

	/// Whether the app has the provider enabled.
	pub fn provider_enabled(&self) -> bool {
		self.file.provider_enabled
	}

	/// Whose session the payload carries; `None` when it carries none.
	pub fn owner(&self) -> Option<SessionOwner> {
		self.owner
	}
}

impl fmt::Debug for AuthPayload {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("AuthPayload")
			.field("provider_enabled", &self.file.provider_enabled)
			.field("has_session", &self.file.sdk_config.is_some())
			.finish()
	}
}

/// The helper's data directory, which holds auth.json, `db/` and `files/`. It belongs in local,
/// not roaming, app data: a roamed auth.json is sealed under a key the other machine does not
/// hold. Absolute and UTF-8, as the cache's constructor takes its paths as strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDir(PathBuf);

/// Why a path cannot be the data directory.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DataDirError {
	/// The path is relative.
	#[error("{0:?} is not absolute")]
	NotAbsolute(PathBuf),
	/// The path is not valid UTF-8.
	#[error("{0:?} is not UTF-8")]
	NotUtf8(PathBuf),
}

impl DataDir {
	/// `path` as the data directory, which need not exist yet.
	pub fn new(path: PathBuf) -> Result<Self, DataDirError> {
		if !path.is_absolute() {
			return Err(DataDirError::NotAbsolute(path));
		}
		if path.to_str().is_none() {
			return Err(DataDirError::NotUtf8(path));
		}
		Ok(Self(path))
	}

	/// The directory.
	pub fn path(&self) -> &Path {
		&self.0
	}

	/// The entry `name` (ASCII) of the directory.
	fn join(&self, name: &str) -> PathBuf {
		self.0.join(name)
	}

	/// The entry `name` (ASCII) of the directory, as the string the cache takes.
	fn join_utf8(&self, name: &str) -> String {
		self.join(name)
			.into_os_string()
			.into_string()
			.expect("a UTF-8 directory joined with an ASCII name is UTF-8 (should be impossible)")
	}
}

/// auth.json under a key the key store holds. Only [`SessionStore::open`] builds one, so nothing
/// is sealed under a key the store does not hold.
pub struct SessionStore {
	dir: DataDir,
	key: SealingKey,
	/// Serialises seals and clears, which write the same file: a clear must not land between a
	/// seal's write and its rename.
	write: Mutex<()>,
}

impl fmt::Debug for SessionStore {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SessionStore")
			.field("dir", &self.dir)
			.field("key", &self.key)
			.finish_non_exhaustive()
	}
}

/// Why the session could not be stored or cleared.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
	/// The key store failed.
	#[error(transparent)]
	Keys(#[from] KeyStoreError),
	/// Encrypting the session failed.
	#[error("sealing the session failed: {0}")]
	Seal(#[from] ConversionError),
	/// Serialising the checked session failed. It re-encodes a parsed `AuthFile`, so its message
	/// quotes no pushed input.
	#[error("serializing the session failed: {0}")]
	Encode(#[source] serde_json::Error),
	/// The data directory holds something and was not created by the helper, so the helper
	/// neither keeps anything in it nor deletes anything from it.
	#[error("{0:?} holds items and is not a data directory the provider created")]
	NotOurs(PathBuf),
	/// A file operation on the data directory failed.
	#[error("{context} {path:?}: {source}")]
	Io {
		/// What was being done.
		context: &'static str,
		/// The path it was done to.
		path: PathBuf,
		/// What the OS answered.
		#[source]
		source: std::io::Error,
	},
}

/// [`SessionError::Io`] for `context` on `path`.
fn io_error(context: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> SessionError {
	move |source| SessionError::Io {
		context,
		path: path.to_owned(),
		source,
	}
}

impl SessionStore {
	/// The session store in `dir`, under the key `keys` holds, or under a new one generated and
	/// stored before this returns. Claims the data directory ([`SessionStore::claim_dir`]), then
	/// creates `db/` and `files/`.
	///
	/// The key is loaded, and a new one stored, under an exclusive lock on `key.lock` in the data
	/// directory, so two processes opening the store at once (the helper and another instance)
	/// cannot each store a key of their own and seal under one the store no longer holds. Blocks
	/// while another process holds the lock.
	///
	/// A stored key of the wrong length is replaced, with a warning naming its length: an
	/// auth.json sealed under it cannot be opened either way, and the app pushes its session on
	/// every launch (recorded default for an open decision).
	pub fn open(dir: DataDir, keys: &impl KeyStore) -> Result<Self, SessionError> {
		Self::claim_dir(&dir)?;
		for name in [DB_DIR_NAME, FILES_DIR_NAME] {
			let path = dir.join(name);
			std::fs::create_dir_all(&path)
				.map_err(io_error("creating the data directory", &path))?;
		}
		let lock_path = dir.join(KEY_LOCK_FILE_NAME);
		let lock = File::options()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&lock_path)
			.map_err(io_error("opening", &lock_path))?;
		lock.lock().map_err(io_error("locking", &lock_path))?;
		let key = load_or_create(keys)?;
		// Unlocked as it closes.
		drop(lock);
		Ok(Self {
			dir,
			key,
			write: Mutex::new(()),
		})
	}

	/// Makes `dir` a data directory the helper created: creates it with its marker when it is
	/// missing, writes the marker into it when it is empty, and refuses ([`SessionError::NotOurs`])
	/// one that holds anything without the marker, such as a user's folder named by mistake.
	pub fn claim_dir(dir: &DataDir) -> Result<(), SessionError> {
		let marker = dir.join(DATA_DIR_MARKER_NAME);
		if marker.is_file() {
			return Ok(());
		}
		match std::fs::read_dir(&dir.0) {
			Ok(mut entries) => {
				if entries.next().is_some() {
					return Err(SessionError::NotOurs(dir.0.clone()));
				}
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				std::fs::create_dir_all(&dir.0)
					.map_err(io_error("creating the data directory", &dir.0))?;
			}
			Err(e) => return Err(io_error("reading", &dir.0)(e)),
		}
		std::fs::write(&marker, DATA_DIR_MARKER_TEXT).map_err(io_error("writing", &marker))
	}

	/// Seals `payload` into auth.json as `0x01 ‖ nonce ‖ AES-256-GCM ciphertext ‖ tag`, the format
	/// the cache reads, replacing the file atomically with owner-only permissions. The JSON
	/// written is the checked `AuthFile` re-encoded, so it holds exactly what was checked. The
	/// cache adopts the session on its own next re-read: within 1 s while it is unauthenticated,
	/// 10 s while it is signed in.
	pub fn seal(&self, payload: &AuthPayload) -> Result<(), SessionError> {
		let mut data = serde_json::to_vec(&payload.file).map_err(SessionError::Encode)?;
		self.key.encryption_key().blocking_encrypt_data(&mut data)?;
		let mut sealed = Vec::with_capacity(data.len() + 1);
		sealed.push(AUTH_FILE_VERSION);
		sealed.extend_from_slice(&data);
		let path = self.dir.join(AUTH_FILE_NAME);
		let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
		write_private_file_atomically(&self.dir.join(AUTH_TEMP_FILE_NAME), &path, &sealed)
			.map_err(io_error("writing", &path))
	}

	/// Deletes auth.json, the cache's trusted disable: it stops serving and, once the disable
	/// holds, wipes what it cached. A missing file is already cleared. The key stays: [`KeyStore::delete`]
	/// removes it.
	pub fn clear(&self) -> Result<(), SessionError> {
		let path = self.dir.join(AUTH_FILE_NAME);
		let _write = self.write.lock().unwrap_or_else(PoisonError::into_inner);
		remove_file_if_present(&path).map_err(io_error("removing", &path))
	}

	/// Whether the data directory `dir` holds a sealed session.
	pub fn has_session(dir: &DataDir) -> bool {
		dir.join(AUTH_FILE_NAME).is_file()
	}

	/// Deletes everything the session store, the cache and the local engine keep in the data
	/// directory `dir` (auth.json and its temporary file, the key lock, `db/`, `files/` and the
	/// engine's records), then the marker and
	/// the directory itself when nothing else is in it. Refuses ([`SessionError::NotOurs`]) a
	/// directory without the marker [`SessionStore::claim_dir`] writes, and does nothing to a
	/// missing one.
	pub fn wipe_dir(dir: &DataDir) -> Result<(), SessionError> {
		if !dir.join(DATA_DIR_MARKER_NAME).is_file() {
			return match std::fs::symlink_metadata(&dir.0) {
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
				Err(e) => Err(io_error("reading", &dir.0)(e)),
				Ok(_) => Err(SessionError::NotOurs(dir.0.clone())),
			};
		}
		let session = [AUTH_FILE_NAME, AUTH_TEMP_FILE_NAME, KEY_LOCK_FILE_NAME];
		for name in session.into_iter().chain(RECORD_FILE_NAMES) {
			let path = dir.join(name);
			remove_file_if_present(&path).map_err(io_error("removing", &path))?;
		}
		for name in [DB_DIR_NAME, FILES_DIR_NAME] {
			let path = dir.join(name);
			match std::fs::remove_dir_all(&path) {
				Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
					return Err(io_error("removing", &path)(e));
				}
				Ok(()) | Err(_) => {}
			}
		}
		// Last, so a wipe that failed above can be run again.
		let marker = dir.join(DATA_DIR_MARKER_NAME);
		remove_file_if_present(&marker).map_err(io_error("removing", &marker))?;
		// Left when anything else is in it.
		let _ = std::fs::remove_dir(&dir.0);
		Ok(())
	}

	/// Opens the cache on this data directory with its existing constructor,
	/// [`FilenMobileCacheState::new_with_db_dir`]. Blocking: it reads auth.json and opens SQLite,
	/// so call it from `spawn_blocking` or a thread outside the runtime.
	pub fn open_cache(&self) -> FilenMobileCacheState {
		FilenMobileCacheState::new_with_db_dir(
			self.dir.join_utf8(FILES_DIR_NAME),
			self.dir.join_utf8(DB_DIR_NAME),
			self.dir.join_utf8(AUTH_FILE_NAME),
			self.key.as_bytes().to_vec(),
		)
	}
}

/// The key `keys` holds, or a new one stored in it.
fn load_or_create(keys: &impl KeyStore) -> Result<SealingKey, KeyStoreError> {
	match keys.load() {
		Ok(Some(key)) => Ok(key),
		Ok(None) => generate_and_store(keys),
		Err(KeyStoreError::WrongLength(e)) => {
			warn!(
				len = e.len,
				"replacing a stored sealing key of the wrong length"
			);
			generate_and_store(keys)
		}
		Err(e @ KeyStoreError::Backend(_)) => Err(e),
	}
}

/// A new key, stored in `keys` before it is returned.
fn generate_and_store(keys: &impl KeyStore) -> Result<SealingKey, KeyStoreError> {
	let key = SealingKey::generate();
	keys.store(&key)?;
	Ok(key)
}

#[cfg(test)]
mod tests;
