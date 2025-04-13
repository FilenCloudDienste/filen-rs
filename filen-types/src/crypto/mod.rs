pub mod rsa;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sha512Hash(#[serde(with = "faster_hex::nopfx_ignorecase")] digest::Output<Sha512>);

impl From<digest::Output<Sha512>> for Sha512Hash {
	fn from(hash: digest::Output<Sha512>) -> Self {
		Self(hash)
	}
}

impl From<Sha512Hash> for digest::Output<Sha512> {
	fn from(hash: Sha512Hash) -> Self {
		hash.0
	}
}

impl From<Sha512Hash> for [u8; 64] {
	fn from(hash: Sha512Hash) -> Self {
		hash.0.into()
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sha256Hash(#[serde(with = "faster_hex::nopfx_ignorecase")] digest::Output<sha2::Sha256>);
impl From<digest::Output<sha2::Sha256>> for Sha256Hash {
	fn from(hash: digest::Output<sha2::Sha256>) -> Self {
		Self(hash)
	}
}

impl From<Sha256Hash> for digest::Output<sha2::Sha256> {
	fn from(hash: Sha256Hash) -> Self {
		hash.0
	}
}

impl From<Sha256Hash> for [u8; 32] {
	fn from(hash: Sha256Hash) -> Self {
		hash.0.into()
	}
}
