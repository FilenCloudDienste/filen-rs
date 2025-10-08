use std::borrow::Cow;

use base64::{Engine, prelude::BASE64_STANDARD};
use rsa::{
	RsaPublicKey,
	pkcs8::{DecodePublicKey, EncodePublicKey},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RsaDerPublicKey<'a>(pub Cow<'a, RsaPublicKey>);

impl Serialize for RsaDerPublicKey<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let der = self
			.0
			.to_public_key_der()
			.expect("PKCS#1 DER serialization should not fail");
		let bytes = BASE64_STANDARD.encode(der.as_bytes());
		serializer.serialize_str(&bytes)
	}
}

impl<'de, 'a> Deserialize<'de> for RsaDerPublicKey<'a> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let base_64_encoded: Cow<'_, str> = crate::serde::cow::deserialize(deserializer)?;
		let bytes = BASE64_STANDARD
			.decode(base_64_encoded.as_ref())
			.map_err(serde::de::Error::custom)?;

		let key =
			RsaPublicKey::from_public_key_der(bytes.as_ref()).map_err(serde::de::Error::custom)?;
		Ok(RsaDerPublicKey(Cow::Owned(key)))
	}
}

pub mod public_key_der {
	use super::*;

	pub fn serialize<S>(value: &RsaPublicKey, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		RsaDerPublicKey(Cow::Borrowed(value)).serialize(serializer)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<RsaPublicKey, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		RsaDerPublicKey::deserialize(deserializer).map(|k| k.0.into_owned())
	}
}
