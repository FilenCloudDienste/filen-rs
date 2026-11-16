//! The key auth.json is sealed under, and where it is kept.

use std::fmt;

use filen_sdk_rs::crypto::v3::EncryptionKey;

use crate::sealed;

/// The length of a [`SealingKey`], in bytes.
pub const SEALING_KEY_LEN: usize = 32;

/// The AES-256-GCM key auth.json is sealed under. `Debug` prints nothing of it, not even a digest,
/// so logs cannot be correlated by key. Not wiped on drop: wiping keys is not done in this
/// workspace.
pub struct SealingKey([u8; SEALING_KEY_LEN]);

/// A stored key of the wrong length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("a sealing key is {SEALING_KEY_LEN} bytes, not {len}")]
pub struct KeyLengthError {
	/// The length found, in bytes.
	pub len: usize,
}

impl SealingKey {
	/// The key `bytes` hold, which must be exactly [`SEALING_KEY_LEN`] long.
	pub fn from_bytes(bytes: &[u8]) -> Result<Self, KeyLengthError> {
		<[u8; SEALING_KEY_LEN]>::try_from(bytes)
			.map(Self)
			.map_err(|_| KeyLengthError { len: bytes.len() })
	}

	/// The key's bytes, for the key store to keep.
	pub fn as_bytes(&self) -> &[u8; SEALING_KEY_LEN] {
		&self.0
	}

	/// A new key from the thread's cryptographically secure generator.
	pub(crate) fn generate() -> Self {
		Self(rand::random())
	}

	/// The key as the SDK's AES-256-GCM data crypter, the one the cache opens auth.json with.
	pub(crate) fn encryption_key(&self) -> EncryptionKey {
		EncryptionKey::new(self.0)
	}
}

impl fmt::Debug for SealingKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str("SealingKey(..)")
	}
}

/// Where the sealing key lives: Windows Credential Manager in the provider, memory in the tests.
/// Synchronous, as Credential Manager is; never called from a Cloud Files callback thread.
/// Nothing elevated may call it: an elevated process could not write such a credential in the
/// measured Windows session (error 1312), while the non-elevated helper can.
pub trait KeyStore: sealed::Sealed + Send + Sync {
	/// The stored key, or `Ok(None)` when none is stored. A stored secret of the wrong length is
	/// [`KeyStoreError::WrongLength`].
	fn load(&self) -> Result<Option<SealingKey>, KeyStoreError>;

	/// Stores `key`, replacing any key stored before.
	fn store(&self, key: &SealingKey) -> Result<(), KeyStoreError>;

	/// Deletes the stored key, as signing out does: an auth.json sealed under it can no longer be
	/// opened, and the next [`SessionStore::open`](crate::SessionStore::open) stores a new one. A
	/// store holding no key is already deleted.
	fn delete(&self) -> Result<(), KeyStoreError>;
}

/// The most characters a [`KeyAccount`] holds.
const MAX_KEY_ACCOUNT_LEN: usize = 64;

/// Whose sealing key a key store holds: 1 to 64 ASCII letters, digits, `-` and `_`. The provider
/// passes the Filen user id, as there is one data directory per user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAccount(String);

/// Why a string cannot be a [`KeyAccount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyAccountError {
	/// The string is empty.
	#[error("a key account is empty")]
	Empty,
	/// The string holds a character other than an ASCII letter, digit, `-` or `_`.
	#[error(
		"a key account holds only ASCII letters, digits, '-' and '_', not {found:?} at byte {at}"
	)]
	Disallowed {
		/// The character found.
		found: char,
		/// Its offset in the string, in bytes.
		at: usize,
	},
	/// The string is longer than 64 characters.
	#[error("a key account is at most {MAX_KEY_ACCOUNT_LEN} characters, not {len}")]
	TooLong {
		/// Its length, in characters.
		len: usize,
	},
}

impl KeyAccount {
	/// `account` as a key account, once it passes the rules on [`KeyAccount`].
	pub fn new(account: &str) -> Result<Self, KeyAccountError> {
		if account.is_empty() {
			return Err(KeyAccountError::Empty);
		}
		if let Some((at, found)) = account
			.char_indices()
			.find(|&(_, c)| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
		{
			return Err(KeyAccountError::Disallowed { found, at });
		}
		// Only ASCII is left, so the length in bytes is the length in characters.
		if account.len() > MAX_KEY_ACCOUNT_LEN {
			return Err(KeyAccountError::TooLong { len: account.len() });
		}
		Ok(Self(account.to_owned()))
	}

	/// The account, as it was given.
	pub fn as_str(&self) -> &str {
		&self.0
	}
}

/// Why the key store could not load or store the key.
#[derive(Debug, thiserror::Error)]
pub enum KeyStoreError {
	/// The store itself failed.
	#[error("the key store failed: {0}")]
	Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
	/// The stored secret is not a sealing key.
	#[error(transparent)]
	WrongLength(#[from] KeyLengthError),
}
