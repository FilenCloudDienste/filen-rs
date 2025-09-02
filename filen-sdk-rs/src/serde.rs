pub(crate) mod rsa_public_key_pkcs1 {
	use rsa::{
		RsaPublicKey,
		pkcs1::{DecodeRsaPublicKey, EncodeRsaPublicKey},
	};
	use serde::Serializer;

	pub(crate) fn serialize<S>(key: &RsaPublicKey, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let der = key
			.to_pkcs1_der()
			.expect("PKCS#1 DER serialization should not fail");
		serde_bytes::serialize(der.as_bytes(), serializer)
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<RsaPublicKey, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let bytes: Vec<u8> = serde_bytes::deserialize(deserializer)?;
		RsaPublicKey::from_pkcs1_der(&bytes).map_err(serde::de::Error::custom)
	}
}
