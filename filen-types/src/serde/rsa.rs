pub mod public_key_der {
	use std::borrow::Cow;

	use base64::{Engine, prelude::BASE64_STANDARD};
	use rsa::{
		RsaPublicKey,
		pkcs8::{DecodePublicKey, EncodePublicKey},
	};
	use serde::Serializer;

	pub fn serialize<S>(key: &RsaPublicKey, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let der = key
			.to_public_key_der()
			.expect("PKCS#1 DER serialization should not fail");
		let bytes = BASE64_STANDARD.encode(der.as_bytes());
		serializer.serialize_str(&bytes)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<RsaPublicKey, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let base_64_encoded: Cow<'_, str> = crate::serde::cow::deserialize(deserializer)?;
		let bytes = BASE64_STANDARD
			.decode(base_64_encoded.as_ref())
			.map_err(serde::de::Error::custom)?;

		RsaPublicKey::from_public_key_der(bytes.as_ref()).map_err(serde::de::Error::custom)
	}
}
