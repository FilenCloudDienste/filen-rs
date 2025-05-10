pub(crate) mod optional {
	use std::borrow::Cow;

	use serde::{Deserialize, Deserializer, Serializer, de::IntoDeserializer};

	pub(crate) fn serialize<S>(
		value: &Option<Cow<'_, [u8]>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(v) => serializer.serialize_bytes(v),
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
