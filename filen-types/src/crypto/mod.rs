pub mod rsa;
use std::{borrow::Cow, fmt::Formatter};

use base64::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::Sha512;

use crate::impl_cow_helpers_for_newtype;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct DerivedPassword<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(DerivedPassword);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncodedString<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(EncodedString);

impl TryFrom<&EncodedString<'_>> for Vec<u8> {
	type Error = base64::DecodeError;
	fn try_from(value: &EncodedString) -> Result<Self, Self::Error> {
		BASE64_STANDARD.decode(value.0.as_ref())
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedString<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(EncryptedString);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedMasterKeys<'a>(pub EncryptedString<'a>);
impl_cow_helpers_for_newtype!(EncryptedMasterKeys);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedDEK<'a>(pub EncryptedString<'a>);
impl_cow_helpers_for_newtype!(EncryptedDEK);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedMetaKey<'a>(pub EncryptedString<'a>);
impl_cow_helpers_for_newtype!(EncryptedMetaKey);

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
