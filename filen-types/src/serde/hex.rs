use serde::{Deserialize, Deserializer, Serializer, de::IntoDeserializer};
use std::borrow::Cow;

pub(crate) mod optional {
	use super::*;
	pub(crate) fn serialize<S>(
		value: &Option<Cow<'_, [u8]>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(value) => faster_hex::nopfx_lowercase::serialize(value, serializer),
			None => serializer.serialize_none(),
		}
	}

	pub(crate) fn deserialize<'de, D>(
		deserializer: D,
	) -> Result<Option<Cow<'static, [u8]>>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = <Option<&str>>::deserialize(deserializer)?;
		match value {
			Some(v) => {
				let bytes = faster_hex::nopfx_ignorecase::deserialize(v.into_deserializer())?;
				Ok(Some(Cow::Owned(bytes)))
			}
			None => Ok(None),
		}
	}
}

pub(crate) mod const_size {
	use super::*;

	pub(crate) fn serialize<S>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		faster_hex::nopfx_ignorecase::serialize(value, serializer)
	}

	pub(crate) fn deserialize<'de, D, const N: usize>(deserializer: D) -> Result<[u8; N], D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let cow = Cow::<str>::deserialize(deserializer)?;
		let mut bytes = [0u8; N];
		if cow.len() != N * 2 {
			return Err(serde::de::Error::custom(format!(
				"Invalid length for [u8; {}]: {}",
				N,
				cow.len()
			)));
		}
		faster_hex::hex_decode(cow.as_bytes(), &mut bytes).map_err(serde::de::Error::custom)?;
		Ok(bytes)
	}
}
