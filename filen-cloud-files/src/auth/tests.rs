use std::fs::File;
#[cfg(unix)]
use std::{
	ffi::OsStr,
	os::unix::{ffi::OsStrExt, fs::PermissionsExt},
};

use filen_mobile_native_cache::CacheError;
use filen_sdk_rs::crypto::{shared::DataCrypter, v3::EncryptionKey};
use tempfile::TempDir;

use super::*;
use crate::test_support::MemoryKeyStore;

/// An enabled payload with a whole session, every secret a recognisable marker.
const ENABLED: &str = r#"{
	"providerEnabled": true,
	"sdkConfig": {
		"email": "user@example.invalid",
		"password": "s3cr3t-password",
		"twoFactorCode": "",
		"masterKeys": ["s3cr3t-master-key"],
		"apiKey": "s3cr3t-api-key",
		"publicKey": "",
		"privateKey": "s3cr3t-private-key",
		"authVersion": 2,
		"baseFolderUUID": "00000000-0000-0000-0000-000000000100",
		"userId": "42",
		"metadataCache": false,
		"tmpPath": "",
		"connectToSocket": false
	}
}"#;

const DISABLED: &[u8] = br#"{"providerEnabled":false}"#;

/// A data directory in a fresh temporary directory, which the guard removes.
fn data_dir() -> (TempDir, DataDir) {
	let temp = tempfile::tempdir().unwrap();
	let dir = DataDir::new(temp.path().join("data")).unwrap();
	(temp, dir)
}

#[test]
fn opening_a_missing_or_empty_data_directory_claims_it() {
	let (_temp, dir) = data_dir();
	SessionStore::open(dir.clone(), &MemoryKeyStore::new()).unwrap();
	assert!(dir.path().join(DATA_DIR_MARKER_NAME).is_file());
	let (_temp, empty) = data_dir();
	std::fs::create_dir(empty.path()).unwrap();
	SessionStore::claim_dir(&empty).unwrap();
	assert!(empty.path().join(DATA_DIR_MARKER_NAME).is_file());
}

#[test]
fn a_data_directory_holding_items_without_the_marker_is_refused_and_left_alone() {
	let (_temp, dir) = data_dir();
	std::fs::create_dir_all(dir.path().join("files")).unwrap();
	std::fs::write(dir.path().join("files").join("thesis.docx"), b"mine").unwrap();
	let error = SessionStore::open(dir.clone(), &MemoryKeyStore::new()).unwrap_err();
	assert!(matches!(error, SessionError::NotOurs(_)), "{error:?}");
	assert!(!dir.path().join(DATA_DIR_MARKER_NAME).exists());
	assert!(!dir.path().join("db").exists());
	assert!(!dir.path().join(KEY_LOCK_FILE_NAME).exists());
}

/// The JSON auth.json under `temp` decrypts to with `key`, after checking its format byte.
fn unseal(temp: &TempDir, key: &[u8]) -> String {
	let sealed = std::fs::read(temp.path().join("data").join("auth.json")).unwrap();
	let (version, data) = sealed.split_first().unwrap();
	assert_eq!(*version, 0x01);
	let mut data = data.to_vec();
	EncryptionKey::new(key.try_into().unwrap())
		.blocking_decrypt_data(&mut data)
		.unwrap();
	String::from_utf8(data).unwrap()
}

#[test]
fn a_payload_of_one_mib_is_accepted_and_one_more_byte_refused() {
	let mut json = br#"{"providerEnabled":false}"#.to_vec();
	json.resize(1 << 20, b' ');
	assert!(!AuthPayload::parse(&json).unwrap().file.provider_enabled);
	json.push(b' ');
	assert_eq!(
		AuthPayload::parse(&json).unwrap_err(),
		AuthPayloadError::TooLarge { len: 1_048_577 }
	);
}

#[test]
fn a_malformed_payload_error_quotes_no_payload_text() {
	let json = br#"{"providerEnabled":true,"sdkConfig":{"password":"s3cr3t-marker"}}"#;
	let error = AuthPayload::parse(json).unwrap_err();
	assert_eq!(
		error,
		AuthPayloadError::Malformed {
			category: serde_json::error::Category::Data,
			line: 1,
			column: 64,
		}
	);
	assert_eq!(
		error.to_string(),
		"the payload is not an AuthFile (Data at 1:64)"
	);
}

#[test]
fn an_unknown_auth_version_is_malformed() {
	let json = ENABLED.replace(r#""authVersion": 2"#, r#""authVersion": 9"#);
	assert!(matches!(
		AuthPayload::parse(json.as_bytes()),
		Err(AuthPayloadError::Malformed {
			category: serde_json::error::Category::Data,
			..
		})
	));
}

#[test]
fn an_enabled_payload_without_a_session_is_refused() {
	assert_eq!(
		AuthPayload::parse(br#"{"providerEnabled":true,"sdkConfig":null}"#).unwrap_err(),
		AuthPayloadError::EnabledWithoutSession
	);
}

#[test]
fn a_disabled_payload_without_a_session_is_accepted() {
	let payload = AuthPayload::parse(DISABLED).unwrap();
	assert!(!payload.file.provider_enabled);
	assert!(payload.file.sdk_config.is_none());
}

#[test]
fn payload_debug_shows_no_secret() {
	let payload = AuthPayload::parse(ENABLED.as_bytes()).unwrap();
	assert_eq!(
		format!("{payload:?}"),
		"AuthPayload { provider_enabled: true, has_session: true }"
	);
}

#[test]
fn sealing_key_debug_shows_no_key_bytes() {
	let key = SealingKey::from_bytes(&[7; 32]).unwrap();
	assert_eq!(format!("{key:?}"), "SealingKey(..)");
}

#[test]
fn a_sealing_key_of_31_or_33_bytes_is_refused() {
	assert_eq!(
		SealingKey::from_bytes(&[0; 31]).unwrap_err(),
		KeyLengthError { len: 31 }
	);
	assert_eq!(
		SealingKey::from_bytes(&[0; 33]).unwrap_err(),
		KeyLengthError { len: 33 }
	);
}

#[test]
fn a_relative_or_non_utf8_data_dir_is_refused() {
	assert_eq!(
		DataDir::new(PathBuf::from("relative/data")).unwrap_err(),
		DataDirError::NotAbsolute(PathBuf::from("relative/data"))
	);
	#[cfg(unix)]
	{
		let path = PathBuf::from(OsStr::from_bytes(b"/data/\xff"));
		assert_eq!(
			DataDir::new(path.clone()).unwrap_err(),
			DataDirError::NotUtf8(path)
		);
	}
}

#[test]
fn open_generates_and_stores_a_key_once_then_reuses_it() {
	let (temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	let first = SessionStore::open(dir.clone(), &keys).unwrap();
	assert_eq!(keys.store_count(), 1);
	assert_eq!(
		keys.raw().unwrap(),
		first.key.as_bytes().to_vec(),
		"the key is stored before open returns"
	);
	let second = SessionStore::open(dir, &keys).unwrap();
	assert_eq!(keys.store_count(), 1);
	assert_eq!(second.key.as_bytes(), first.key.as_bytes());
	assert!(temp.path().join("data").join("db").is_dir());
	assert!(temp.path().join("data").join("files").is_dir());
}

/// A key store recording, on each load and store, whether another opener of the data directory's
/// key lock would have to wait.
struct LockProbe {
	keys: MemoryKeyStore,
	lock_path: PathBuf,
	/// Whether the key lock was held during each call, in order.
	held: Mutex<Vec<bool>>,
}

impl LockProbe {
	fn record(&self) {
		let file = File::options()
			.create(true)
			.write(true)
			.truncate(false)
			.open(&self.lock_path)
			.unwrap();
		let held = matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock));
		self.held.lock().unwrap().push(held);
	}
}

impl crate::sealed::Sealed for LockProbe {}

impl KeyStore for LockProbe {
	fn load(&self) -> Result<Option<SealingKey>, KeyStoreError> {
		self.record();
		self.keys.load()
	}

	fn store(&self, key: &SealingKey) -> Result<(), KeyStoreError> {
		self.record();
		self.keys.store(key)
	}

	fn delete(&self) -> Result<(), KeyStoreError> {
		self.record();
		self.keys.delete()
	}
}

#[test]
fn the_key_is_loaded_and_created_under_the_key_lock() {
	let (temp, dir) = data_dir();
	let probe = LockProbe {
		keys: MemoryKeyStore::new(),
		lock_path: temp.path().join("data").join("key.lock"),
		held: Mutex::new(Vec::new()),
	};
	SessionStore::open(dir.clone(), &probe).unwrap();
	assert_eq!(*probe.held.lock().unwrap(), [true, true]);
	// Released once open returns, so the next opener goes on.
	let file = File::open(&probe.lock_path).unwrap();
	file.try_lock().unwrap();
}

#[test]
fn a_wrong_length_stored_key_is_replaced() {
	let (_temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	keys.put_raw(vec![1; 16]);
	let store = SessionStore::open(dir, &keys).unwrap();
	assert_eq!(keys.store_count(), 1);
	assert_eq!(keys.raw().unwrap().len(), 32);
	assert_eq!(keys.raw().unwrap(), store.key.as_bytes().to_vec());
}

#[test]
fn a_failed_key_store_write_seals_nothing() {
	let (temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	keys.fail_store();
	let error = SessionStore::open(dir, &keys).unwrap_err();
	assert_eq!(
		error.to_string(),
		"the key store failed: the memory key store was told to fail"
	);
	assert_eq!(keys.raw(), None);
	assert!(!temp.path().join("data").join("auth.json").exists());
}

#[test]
fn a_failed_key_store_read_is_returned() {
	let (_temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	keys.put_raw(vec![3; 32]);
	keys.fail_load();
	assert!(matches!(
		SessionStore::open(dir, &keys),
		Err(SessionError::Keys(KeyStoreError::Backend(_)))
	));
	assert_eq!(keys.store_count(), 0);
	assert_eq!(keys.raw(), Some(vec![3; 32]));
}

#[test]
fn seal_writes_version_one_that_decrypts_to_the_canonical_json() {
	let (temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	keys.put_raw(vec![5; 32]);
	let store = SessionStore::open(dir, &keys).unwrap();
	let payload =
		AuthPayload::parse(br#"{ "providerEnabled": false, "unknownField": 1 }"#).unwrap();
	store.seal(&payload).unwrap();
	assert_eq!(
		unseal(&temp, &[5; 32]),
		concat!(
			r#"{"providerEnabled":false,"sdkConfig":null,"maxThumbnailFilesBudget":null,"#,
			r#""maxCacheFilesBudget":null,"thumbnailMemBudget":null,"#,
			r#""thumbnailMaxSourceBytes":null,"thumbnailDecodeConcurrency":null,"#,
			r#""thumbnailItemConcurrency":null}"#
		)
	);
	assert!(!temp.path().join("data").join("auth.json.tmp").exists());
}

#[cfg(unix)]
#[test]
fn auth_json_is_readable_by_its_owner_only() {
	let (temp, dir) = data_dir();
	let store = SessionStore::open(dir, &MemoryKeyStore::new()).unwrap();
	store.seal(&AuthPayload::parse(DISABLED).unwrap()).unwrap();
	let mode = std::fs::metadata(temp.path().join("data").join("auth.json"))
		.unwrap()
		.permissions()
		.mode();
	assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn clear_removes_auth_json_and_keeps_the_key() {
	let (temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	let store = SessionStore::open(dir, &keys).unwrap();
	store.seal(&AuthPayload::parse(DISABLED).unwrap()).unwrap();
	store.clear().unwrap();
	assert!(!temp.path().join("data").join("auth.json").exists());
	assert_eq!(keys.raw().unwrap(), store.key.as_bytes().to_vec());
}

#[test]
fn clear_on_a_missing_file_is_ok() {
	let (_temp, dir) = data_dir();
	let store = SessionStore::open(dir, &MemoryKeyStore::new()).unwrap();
	store.clear().unwrap();
}

/// Opens the cache on `store` from the blocking pool, as the provider must, and the error its
/// first authenticated call answers with.
async fn root_uuid_error(store: SessionStore) -> CacheError {
	let cache = tokio::task::spawn_blocking(move || store.open_cache())
		.await
		.unwrap();
	let error = cache.async_root_uuid().await.unwrap_err();
	cache.shutdown().await;
	error
}

#[tokio::test]
async fn a_disabled_session_opens_the_real_cache_as_disabled() {
	let (_temp, dir) = data_dir();
	let store = SessionStore::open(dir, &MemoryKeyStore::new()).unwrap();
	store.seal(&AuthPayload::parse(DISABLED).unwrap()).unwrap();
	let error = root_uuid_error(store).await;
	assert!(
		matches!(error, CacheError::Disabled(_)),
		"expected Disabled, got {error}"
	);
}

#[tokio::test]
async fn a_session_under_another_key_opens_the_cache_unauthenticated() {
	let (temp, dir) = data_dir();
	let store = SessionStore::open(dir.clone(), &MemoryKeyStore::new()).unwrap();
	store.seal(&AuthPayload::parse(DISABLED).unwrap()).unwrap();
	assert!(temp.path().join("data").join("auth.json").exists());
	// Another key store, so another key, on the same data directory.
	let other = SessionStore::open(dir, &MemoryKeyStore::new()).unwrap();
	let error = root_uuid_error(other).await;
	assert!(
		matches!(error, CacheError::Unauthenticated(_)),
		"expected Unauthenticated, got {error}"
	);
}

#[test]
fn a_key_account_of_letters_digits_dash_and_underscore_is_accepted() {
	assert_eq!(KeyAccount::new("Ab-9_z").unwrap().as_str(), "Ab-9_z");
}

#[test]
fn an_empty_key_account_is_refused() {
	assert_eq!(KeyAccount::new("").unwrap_err(), KeyAccountError::Empty);
}

#[test]
fn a_key_account_of_64_characters_is_accepted_and_65_refused() {
	let account = "a".repeat(64);
	assert_eq!(KeyAccount::new(&account).unwrap().as_str(), account);
	assert_eq!(
		KeyAccount::new(&"a".repeat(65)).unwrap_err(),
		KeyAccountError::TooLong { len: 65 }
	);
}

#[test]
fn a_key_account_with_a_dot_or_non_ascii_character_is_refused() {
	assert_eq!(
		KeyAccount::new("user.42").unwrap_err(),
		KeyAccountError::Disallowed { found: '.', at: 4 }
	);
	assert_eq!(
		KeyAccount::new("ab\u{e9}").unwrap_err(),
		KeyAccountError::Disallowed {
			found: '\u{e9}',
			at: 2
		}
	);
	assert_eq!(
		KeyAccount::new("a b").unwrap_err().to_string(),
		"a key account holds only ASCII letters, digits, '-' and '_', not ' ' at byte 1"
	);
}

#[test]
fn a_deleted_key_is_replaced_by_a_new_one_on_the_next_open() {
	let (_temp, dir) = data_dir();
	let keys = MemoryKeyStore::new();
	SessionStore::open(dir.clone(), &keys).unwrap();
	let first = keys.raw().unwrap();
	keys.delete().unwrap();
	assert_eq!(keys.raw(), None);
	assert_eq!(keys.delete_count(), 1);
	SessionStore::open(dir, &keys).unwrap();
	let second = keys.raw().unwrap();
	assert_eq!(second.len(), 32);
	assert_ne!(first, second);
}

#[test]
fn deleting_a_store_that_holds_no_key_succeeds() {
	let keys = MemoryKeyStore::new();
	keys.delete().unwrap();
	assert_eq!(keys.raw(), None);
}

#[test]
fn an_enabled_payload_names_its_owner() {
	let payload = AuthPayload::parse(ENABLED.as_bytes()).unwrap();
	assert!(payload.provider_enabled());
	assert_eq!(
		payload.owner(),
		Some(SessionOwner {
			user_id: 42,
			account_root: Uuid::from_u128(0x100),
		})
	);
}

#[test]
fn a_disabled_payload_has_no_owner() {
	let payload = AuthPayload::parse(DISABLED).unwrap();
	assert!(!payload.provider_enabled());
	assert_eq!(payload.owner(), None);
}

#[test]
fn a_session_whose_account_root_is_no_uuid_is_refused() {
	let json = ENABLED.replace("00000000-0000-0000-0000-000000000100", "not-a-uuid");
	assert_eq!(
		AuthPayload::parse(json.as_bytes()).unwrap_err(),
		AuthPayloadError::MalformedAccountRoot
	);
}

#[test]
fn a_data_directory_without_the_marker_is_never_wiped() {
	let (_temp, dir) = data_dir();
	std::fs::create_dir_all(dir.path().join("files")).unwrap();
	std::fs::write(dir.path().join("files").join("thesis.docx"), b"mine").unwrap();
	std::fs::write(dir.path().join(AUTH_FILE_NAME), b"not ours either").unwrap();
	let error = SessionStore::wipe_dir(&dir).unwrap_err();
	assert!(matches!(error, SessionError::NotOurs(_)), "{error:?}");
	assert_eq!(
		std::fs::read(dir.path().join("files").join("thesis.docx")).unwrap(),
		b"mine"
	);
	assert!(dir.path().join(AUTH_FILE_NAME).is_file());
}

#[test]
fn a_claimed_data_directory_is_wiped_whole_and_a_missing_one_is_already_wiped() {
	let (_temp, dir) = data_dir();
	let store = SessionStore::open(dir.clone(), &MemoryKeyStore::new()).unwrap();
	store
		.seal(&AuthPayload::parse(ENABLED.as_bytes()).unwrap())
		.unwrap();
	std::fs::write(dir.path().join("files").join("copy"), b"cached").unwrap();
	drop(store);
	SessionStore::wipe_dir(&dir).unwrap();
	assert!(!dir.path().exists());
	SessionStore::wipe_dir(&dir).unwrap();
}

#[test]
fn a_claimed_data_directory_is_wiped_with_the_local_engines_records() {
	let (_temp, dir) = data_dir();
	let store = SessionStore::open(dir.clone(), &MemoryKeyStore::new()).unwrap();
	drop(store);
	for name in RECORD_FILE_NAMES {
		std::fs::write(dir.path().join(name), b"{}").unwrap();
	}
	SessionStore::wipe_dir(&dir).unwrap();
	assert!(!dir.path().exists());
}
