pub(crate) mod permissive_u64 {
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
				formatter.write_str("a u64, float or string representing a u64")
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				if v < 0 {
					Err(serde::de::Error::custom(
						"negative value cannot be converted to u64",
					))
				} else {
					Ok(v as u64)
				}
			}

			fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				Ok(value)
			}

			fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				crate::conversions::str_to_u64(v).map_err(serde::de::Error::custom)
			}

			fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
			where
				E: serde::de::Error,
			{
				crate::conversions::f64_to_u64(value).map_err(serde::de::Error::custom)
			}
		}

		deserializer.deserialize_any(MaybeFloatu64Visitor)
	}
}

pub(crate) mod permissive_u64_opt {
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub(crate) fn serialize<S>(value: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		value.serialize(serializer)
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
	where
		D: Deserializer<'de>,
	{
		#[derive(Deserialize)]
		struct Wrapper(#[serde(with = "super::permissive_u64")] u64);

		Ok(Option::<Wrapper>::deserialize(deserializer)?.map(|Wrapper(v)| v))
	}
}
