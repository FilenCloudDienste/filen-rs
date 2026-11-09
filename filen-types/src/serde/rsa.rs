use std::{borrow::Cow, str::FromStr};

use base64::{Engine, prelude::BASE64_STANDARD};
use rsa::{
	PublicKey,
	pkcs8::{DecodePublicKey, EncodePublicKey},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RsaDerPublicKey<'a>(pub Cow<'a, PublicKey>);

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
		let key = PublicKey::from_public_key_der(bytes.as_ref())?;
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

	pub fn serialize<S>(value: &PublicKey, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		RsaDerPublicKey(Cow::Borrowed(value)).serialize(serializer)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<PublicKey, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		RsaDerPublicKey::deserialize(deserializer).map(|k| k.0.into_owned())
	}
}

#[cfg(test)]
mod tests {
	use std::str::FromStr;

	use base64::{Engine, prelude::BASE64_STANDARD};
	use rsa::{pkcs8::EncodePublicKey, traits::PublicKeyParts};

	use super::RsaDerPublicKey;

	// The SubjectPublicKeyInfo DER, base64, of a 2048-bit key generated with rsa 0.9.8.
	const SPKI_BASE64: &str = "MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAvg+AOhimAL4YHrPu9rqEZC2MUcT7vKWXWlGGyd2dtXhF6reeJvendCiPixh/pDXM+gFWHiwfcyF8DuKl954hvX2TZCA3+ipgrU02BV7yndT/yjwzy7SdfN1qQt2P6fLeRQ4FKSnENouFrDAuxG05Z02i151MZWArbTNFSfDwlicvq1+kqYv5D28M7tH7lwB0qh6SXCLloE5m1480U3elVkDwwlqaxyFiDg1uvaWnIfB3J3dHXuckq2W34Nmyl3HRvvY9ymXFFmi+D/LF0PyAR+Y8srkvWc2KgAR8brQuzzaXwwjy0PJ3U8fGOemIhOYzHYZl+p5QKmKOxvoWIVzWKwIDAQAB";

	// The same key's modulus, read from the DER with `openssl rsa -pubin -modulus`.
	const SPKI_MODULUS_HEX: &str = "be0f803a18a600be181eb3eef6ba84642d8c51c4fbbca5975a5186c9dd9db57845eab79e26f7a774288f8b187fa435ccfa01561e2c1f73217c0ee2a5f79e21bd7d93642037fa2a60ad4d36055ef29dd4ffca3c33cbb49d7cdd6a42dd8fe9f2de450e052929c4368b85ac302ec46d39674da2d79d4c65602b6d334549f0f096272fab5fa4a98bf90f6f0ceed1fb970074aa1e925c22e5a04e66d78f345377a55640f0c25a9ac721620e0d6ebda5a721f0772777475ee724ab65b7e0d9b29771d1bef63dca65c51668be0ff2c5d0fc8047e63cb2b92f59cd8a80047c6eb42ecf3697c308f2d0f27753c7c639e98884e6331d8665fa9e502a628ec6fa16215cd62b";

	#[test]
	fn rsa_der_public_key_parses_the_spki_modulus_and_exponent() {
		let key = RsaDerPublicKey::from_str(SPKI_BASE64).unwrap();
		assert_eq!(hex::encode(key.0.n_bytes()), SPKI_MODULUS_HEX);
		assert_eq!(*key.0.e_bytes(), [0x01, 0x00, 0x01]);
	}

	#[test]
	fn rsa_der_public_key_re_encodes_the_spki_it_was_parsed_from() {
		let key = RsaDerPublicKey::from_str(SPKI_BASE64).unwrap();
		assert_eq!(key.to_string(), SPKI_BASE64);
		assert_eq!(
			serde_json::to_string(&key).unwrap(),
			format!("\"{SPKI_BASE64}\"")
		);
	}

	#[test]
	fn rsa_der_public_key_to_public_key_der_matches_the_decoded_spki() {
		let key = RsaDerPublicKey::from_str(SPKI_BASE64).unwrap();
		assert_eq!(
			key.0.to_public_key_der().unwrap().as_bytes(),
			BASE64_STANDARD.decode(SPKI_BASE64).unwrap()
		);
	}

	#[test]
	fn rsa_der_public_key_deserializes_from_a_json_string() {
		let key: RsaDerPublicKey = serde_json::from_str(&format!("\"{SPKI_BASE64}\"")).unwrap();
		assert_eq!(key.to_string(), SPKI_BASE64);
	}
}
