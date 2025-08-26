pub mod rsa;
use std::fmt::Formatter;

use base64::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::Sha512;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct DerivedPassword(pub String);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncodedString(pub String);

impl TryFrom<&EncodedString> for Vec<u8> {
	type Error = base64::DecodeError;
	fn try_from(value: &EncodedString) -> Result<Self, Self::Error> {
		BASE64_STANDARD.decode(&value.0)
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedString(pub String);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedMasterKeys(pub EncryptedString);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedDEK(pub EncryptedString);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedMetaKey(pub EncryptedString);

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha512Hash(#[serde(with = "crate::serde::hex::const_size")] [u8; 64]);

impl std::fmt::Debug for Sha512Hash {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Sha512Hash({})", faster_hex::hex_string(&self.0))
	}
}

impl From<digest::Output<Sha512>> for Sha512Hash {
	fn from(hash: digest::Output<Sha512>) -> Self {
		Self(hash.into())
	}
}

impl From<Sha512Hash> for digest::Output<Sha512> {
	fn from(hash: Sha512Hash) -> Self {
		hash.0.into()
	}
}

impl From<Sha512Hash> for [u8; 64] {
	fn from(hash: Sha512Hash) -> Self {
		hash.0
	}
}

impl AsRef<[u8]> for Sha512Hash {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

impl From<[u8; 64]> for Sha512Hash {
	fn from(hash: [u8; 64]) -> Self {
		Self(hash)
	}
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha256Hash(#[serde(with = "crate::serde::hex::const_size")] [u8; 32]);

impl From<digest::Output<sha2::Sha256>> for Sha256Hash {
	fn from(hash: digest::Output<sha2::Sha256>) -> Self {
		Self(hash.into())
	}
}

impl std::fmt::Debug for Sha256Hash {
	fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
		write!(f, "Sha256Hash({})", faster_hex::hex_string(&self.0))
	}
}

impl From<Sha256Hash> for digest::Output<sha2::Sha256> {
	fn from(hash: Sha256Hash) -> Self {
		hash.0.into()
	}
}

impl From<Sha256Hash> for [u8; 32] {
	fn from(hash: Sha256Hash) -> Self {
		hash.0
	}
}

impl AsRef<[u8]> for Sha256Hash {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}
