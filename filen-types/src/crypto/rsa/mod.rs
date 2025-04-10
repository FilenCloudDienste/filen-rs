use rsa::{RsaPublicKey, pkcs8::DecodePublicKey};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncodedString, error::ConversionError};

use super::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncodedPublicKey(EncodedString);

impl TryFrom<&EncodedPublicKey> for RsaPublicKey {
	type Error = ConversionError;
	fn try_from(value: &EncodedPublicKey) -> Result<Self, Self::Error> {
		let decoded: Vec<u8> = (&value.0).try_into()?;
		let key = rsa::RsaPublicKey::from_public_key_der(&decoded)?;
		Ok(key)
	}
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct EncryptedPrivateKey(pub EncryptedString);
