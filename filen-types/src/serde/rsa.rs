use std::{borrow::Cow, str::FromStr};

use base64::{Engine, prelude::BASE64_STANDARD};
use rsa::{
	RsaPublicKey,
	pkcs8::{DecodePublicKey, EncodePublicKey},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RsaDerPublicKey<'a>(pub Cow<'a, RsaPublicKey>);

impl std::fmt::Display for RsaDerPublicKey<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		let der = self
			.0
			.to_public_key_der()
			.expect("PKCS#1 DER serialization should not fail");
		write!(f, "{}", BASE64_STANDARD.encode(der.as_bytes()))
	}
}

impl std::str::FromStr for RsaDerPublicKey<'_> {
	type Err = crate::error::ConversionError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let bytes = BASE64_STANDARD.decode(s)?;
		let key = RsaPublicKey::from_public_key_der(bytes.as_ref())?;
		Ok(RsaDerPublicKey(Cow::Owned(key)))
	}
}

impl Serialize for RsaDerPublicKey<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		serializer.serialize_str(&self.to_string())
	}
}

impl<'de, 'a> Deserialize<'de> for RsaDerPublicKey<'a> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let base_64_encoded: Cow<'_, str> = crate::serde::cow::deserialize(deserializer)?;
		Self::from_str(&base_64_encoded).map_err(serde::de::Error::custom)
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
