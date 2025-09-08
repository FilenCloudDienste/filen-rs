use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

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

pub(crate) fn deserialize_double_option_timestamp<'de, D>(
	deserializer: D,
) -> Result<Option<Option<DateTime<Utc>>>, D::Error>
where
	D: Deserializer<'de>,
{
	match Option::<i64>::deserialize(deserializer)? {
		Some(ts) => Ok(Some(Some(DateTime::<Utc>::from_timestamp_nanos(
			ts * 1_000_000,
		)))),
		None => Ok(Some(None)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Debug, Deserialize)]
	struct MyStruct {
		#[serde(default, deserialize_with = "deserialize_double_option_timestamp")]
		field: Option<Option<DateTime<Utc>>>,
	}

	#[test]
	fn test_name() {
		let json1 = r#"{}"#;
		let result1: MyStruct = serde_json::from_str(json1).unwrap();
		assert_eq!(result1.field, None);

		let json2 = r#"{"field": null}"#;
		let result2: MyStruct = serde_json::from_str(json2).unwrap();
		assert_eq!(result2.field, Some(None));

		let json3 = r#"{"field": 124124}"#;
		let result3: MyStruct = serde_json::from_str(json3).unwrap();
		assert_eq!(
			result3.field,
			Some(Some(DateTime::<Utc>::from_timestamp_nanos(
				124124 * 1_000_000
			)))
		);
	}
}
