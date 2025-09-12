use std::borrow::Cow;

use rsa::{RsaPublicKey, pkcs8::DecodePublicKey};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncodedString, error::ConversionError, impl_cow_helpers_for_newtype};

use super::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncodedPublicKey<'a>(pub EncodedString<'a>);
impl_cow_helpers_for_newtype!(EncodedPublicKey);

impl TryFrom<&EncodedPublicKey<'_>> for RsaPublicKey {
	type Error = ConversionError;
	fn try_from(value: &EncodedPublicKey) -> Result<Self, Self::Error> {
		let decoded: Vec<u8> = (&value.0).try_into()?;
		let key = rsa::RsaPublicKey::from_public_key_der(&decoded)?;
		Ok(key)
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct EncryptedPrivateKey<'a>(pub EncryptedString<'a>);
impl_cow_helpers_for_newtype!(EncryptedPrivateKey);

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(transparent)]
pub struct RSAEncryptedString<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(RSAEncryptedString);
