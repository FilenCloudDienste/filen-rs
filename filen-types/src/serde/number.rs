pub(crate) mod maybe_string_u64 {
	use serde::{Deserializer, Serialize, Serializer, de::Visitor};

	pub(crate) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		value.serialize(serializer)
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct MaybeStringu64Visitor;

		impl<'de> Visitor<'de> for MaybeStringu64Visitor {
			type Value = u64;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a u64 or a string representing a u64")
			}

			fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(value)
			}

			fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				value.parse::<u64>().map_err(serde::de::Error::custom)
			}
		}

		deserializer.deserialize_any(MaybeStringu64Visitor)
	}
}

pub(crate) mod maybe_float_u64 {
	use serde::{Deserializer, Serialize, Serializer, de::Visitor};

	pub(crate) fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		value.serialize(serializer)
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct MaybeFloatu64Visitor;

		impl<'de> Visitor<'de> for MaybeFloatu64Visitor {
			type Value = u64;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a u64 or a float representing a u64")
			}

			fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(value)
			}

			fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				// if we lose precision here, it's a JS problem anyway
				Ok(value as u64)
			}
		}

		deserializer.deserialize_any(MaybeFloatu64Visitor)
	}
}
