//! The sealing key in Windows Credential Manager, through keyring's Windows store. Built on
//! Windows, and in tests on every host, where keyring's mock credential stands in for the entry.

use std::fmt;

use keyring::Entry;

use crate::{
	auth::{KeyAccount, KeyStore, KeyStoreError, SealingKey},
	sealed,
};

/// The sealing key as a generic credential in Windows Credential Manager, its secret the key's
/// 32 raw bytes. Its target name is `sealing-key.<account>.io.filen.desktop.cloud-files`, keyring's
/// `<user>.<service>`, persisted for the enterprise (keyring's fixed choice).
///
/// Never use it from an elevated process: in the measured Windows session, persistent credential
/// writes from an elevated process failed with ERROR_NO_SUCH_LOGON_SESSION (1312), which comes
/// back as [`CredentialError::NoStorageAccess`].
///
/// A missing credential loads as no key, so a new one is generated; a secret of the wrong length
/// loads as [`KeyStoreError::WrongLength`], so it is replaced. Every other failure is
/// [`KeyStoreError::Backend`] holding a [`CredentialError`].
pub struct CredentialManagerKeyStore {
	entry: Entry,
}

/// Why Credential Manager failed.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
	/// Credential Manager refused access, as it does an elevated process.
	#[error(
		"Windows Credential Manager refused access (an elevated process cannot use it; run the \
		 helper unelevated): {0}"
	)]
	NoStorageAccess(#[source] keyring::Error),
	/// Any other failure of Credential Manager or keyring.
	#[error("Windows Credential Manager failed: {0}")]
	Platform(#[source] keyring::Error),
}

impl CredentialManagerKeyStore {
	/// The keyring service every sealing key is stored under.
	pub const SERVICE: &str = "io.filen.desktop.cloud-files";

	/// The store of `account`'s sealing key. Creates no credential: the first store does.
	#[cfg(windows)]
	pub fn new(account: &KeyAccount) -> Result<Self, KeyStoreError> {
		Entry::new(Self::SERVICE, &Self::user(account))
			.map(Self::from_entry)
			.map_err(backend_error)
	}

	/// The keyring user `account`'s sealing key is stored under, with [`Self::SERVICE`].
	pub fn user(account: &KeyAccount) -> String {
		format!("sealing-key.{}", account.as_str())
	}

	/// The store over `entry`: Credential Manager's on Windows, a mock one in the tests.
	fn from_entry(entry: Entry) -> Self {
		Self { entry }
	}
}

/// keyring's `error` as [`KeyStoreError::Backend`], refused access kept apart from the rest.
fn backend_error(error: keyring::Error) -> KeyStoreError {
	let error = match error {
		keyring::Error::NoStorageAccess(_) => CredentialError::NoStorageAccess(error),
		// keyring::Error is non-exhaustive: whatever it adds later is a platform failure too.
		_ => CredentialError::Platform(error),
	};
	KeyStoreError::Backend(Box::new(error))
}

/// Prints nothing of the entry: a mock entry's `Debug` prints its secret.
impl fmt::Debug for CredentialManagerKeyStore {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("CredentialManagerKeyStore")
			.finish_non_exhaustive()
	}
}

impl sealed::Sealed for CredentialManagerKeyStore {}

impl KeyStore for CredentialManagerKeyStore {
	fn load(&self) -> Result<Option<SealingKey>, KeyStoreError> {
		match self.entry.get_secret() {
			Ok(secret) => Ok(Some(SealingKey::from_bytes(&secret)?)),
			Err(keyring::Error::NoEntry) => Ok(None),
			Err(e) => Err(backend_error(e)),
		}
	}

	fn store(&self, key: &SealingKey) -> Result<(), KeyStoreError> {
		self.entry.set_secret(key.as_bytes()).map_err(backend_error)
	}

	/// Deletes the credential; a missing one is already deleted.
	fn delete(&self) -> Result<(), KeyStoreError> {
		match self.entry.delete_credential() {
			Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
			Err(e) => Err(backend_error(e)),
		}
	}
}

#[cfg(test)]
mod tests {
	use keyring::mock::MockCredential;

	use super::*;
	use crate::auth::{DataDir, KeyLengthError, SessionStore};

	/// A store over a fresh mock credential, which holds nothing.
	fn mock_store() -> CredentialManagerKeyStore {
		CredentialManagerKeyStore::from_entry(Entry::new_with_credential(Box::new(
			MockCredential::default(),
		)))
	}

	/// The mock credential under `store`.
	fn mock(store: &CredentialManagerKeyStore) -> &MockCredential {
		store.entry.get_credential().downcast_ref().unwrap()
	}

	/// The raw secret the mock credential under `store` holds.
	fn raw(store: &CredentialManagerKeyStore) -> Option<Vec<u8>> {
		mock(store).inner.lock().unwrap().borrow().secret.clone()
	}

	/// `error`'s [`CredentialError`], which must be a backend error.
	fn credential_error(error: &KeyStoreError) -> &CredentialError {
		match error {
			KeyStoreError::Backend(source) => source.downcast_ref().unwrap(),
			KeyStoreError::WrongLength(e) => panic!("expected a backend error, got {e}"),
		}
	}

	#[test]
	fn the_credential_is_named_after_the_account() {
		let account = KeyAccount::new("42").unwrap();
		assert_eq!(CredentialManagerKeyStore::user(&account), "sealing-key.42");
		assert_eq!(
			CredentialManagerKeyStore::SERVICE,
			"io.filen.desktop.cloud-files"
		);
	}

	#[test]
	fn a_missing_credential_loads_as_no_key() {
		assert!(mock_store().load().unwrap().is_none());
	}

	#[test]
	fn a_stored_key_is_its_32_raw_bytes_and_loads_back() {
		let store = mock_store();
		let key = SealingKey::from_bytes(&[9; 32]).unwrap();
		store.store(&key).unwrap();
		assert_eq!(raw(&store), Some(vec![9; 32]));
		assert_eq!(store.load().unwrap().unwrap().as_bytes(), &[9; 32]);
	}

	#[test]
	fn a_secret_of_the_wrong_length_loads_as_wrong_length() {
		let store = mock_store();
		store.entry.set_secret(&[1; 5]).unwrap();
		assert!(matches!(
			store.load(),
			Err(KeyStoreError::WrongLength(KeyLengthError { len: 5 }))
		));
	}

	#[test]
	fn an_empty_secret_loads_as_wrong_length() {
		let store = mock_store();
		store.entry.set_secret(&[]).unwrap();
		assert!(matches!(
			store.load(),
			Err(KeyStoreError::WrongLength(KeyLengthError { len: 0 }))
		));
	}

	#[test]
	fn opening_a_session_store_replaces_a_secret_of_the_wrong_length() {
		let temp = tempfile::tempdir().unwrap();
		let dir = DataDir::new(temp.path().join("data")).unwrap();
		let store = mock_store();
		store.entry.set_secret(&[1; 5]).unwrap();
		SessionStore::open(dir, &store).unwrap();
		let replaced = raw(&store).unwrap();
		assert_eq!(replaced.len(), 32);
		assert_eq!(store.load().unwrap().unwrap().as_bytes()[..], replaced[..]);
	}

	#[test]
	fn refused_access_on_load_names_elevation() {
		let store = mock_store();
		mock(&store).set_error(keyring::Error::NoStorageAccess("refused".into()));
		let error = store.load().unwrap_err();
		assert!(matches!(
			credential_error(&error),
			CredentialError::NoStorageAccess(keyring::Error::NoStorageAccess(_))
		));
		assert_eq!(
			error.to_string(),
			"the key store failed: Windows Credential Manager refused access (an elevated \
			 process cannot use it; run the helper unelevated): Couldn't access platform secure \
			 storage: refused"
		);
	}

	#[test]
	fn a_platform_failure_on_store_is_a_backend_error() {
		let store = mock_store();
		mock(&store).set_error(keyring::Error::PlatformFailure("broken".into()));
		let error = store
			.store(&SealingKey::from_bytes(&[2; 32]).unwrap())
			.unwrap_err();
		assert!(matches!(
			credential_error(&error),
			CredentialError::Platform(keyring::Error::PlatformFailure(_))
		));
		assert_eq!(
			error.to_string(),
			"the key store failed: Windows Credential Manager failed: Platform secure storage \
			 failure: broken"
		);
		assert_eq!(raw(&store), None);
	}

	#[test]
	fn a_failed_load_other_than_a_missing_credential_is_no_new_key() {
		let temp = tempfile::tempdir().unwrap();
		let dir = DataDir::new(temp.path().join("data")).unwrap();
		let store = mock_store();
		store.entry.set_secret(&[4; 32]).unwrap();
		mock(&store).set_error(keyring::Error::PlatformFailure("broken".into()));
		assert!(SessionStore::open(dir, &store).is_err());
		assert_eq!(raw(&store), Some(vec![4; 32]));
	}

	#[test]
	fn delete_removes_the_key_and_a_missing_key_is_already_deleted() {
		let store = mock_store();
		store.delete().unwrap();
		store
			.store(&SealingKey::from_bytes(&[6; 32]).unwrap())
			.unwrap();
		store.delete().unwrap();
		assert!(store.load().unwrap().is_none());
	}

	#[test]
	fn a_failed_delete_is_returned() {
		let store = mock_store();
		store
			.store(&SealingKey::from_bytes(&[6; 32]).unwrap())
			.unwrap();
		mock(&store).set_error(keyring::Error::NoStorageAccess("refused".into()));
		assert!(matches!(
			credential_error(&store.delete().unwrap_err()),
			CredentialError::NoStorageAccess(_)
		));
		assert_eq!(raw(&store), Some(vec![6; 32]));
	}

	#[test]
	fn debug_shows_no_secret() {
		let store = mock_store();
		store.entry.set_secret(&[7; 32]).unwrap();
		assert_eq!(format!("{store:?}"), "CredentialManagerKeyStore { .. }");
	}
}
