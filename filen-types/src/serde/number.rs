/// u64 deserialization for DECRYPTED metadata fields, where other clients
/// write floats or numeric strings. Floats are the only lossy case: they are
/// cast (truncating, saturating at the u64 bounds, so negative floats become
/// 0). Numeric strings convert to an integer or float first and then follow
/// the same rules. Every other type (null, bool, negative integer,
/// non-numeric string, object, array) fails like a plain u64 field. Wire API
/// types should use [`permissive_u64`] or stricter instead.
pub mod truncating_u64 {
	use serde::{
		Deserializer, Serialize, Serializer,
		de::{Error, Visitor},
	};

	pub fn serialize<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		value.serialize(serializer)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<u64, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct TruncatingU64Visitor;

		impl<'de> Visitor<'de> for TruncatingU64Visitor {
			type Value = u64;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a u64, a float, or a numeric string")
			}

			fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
				Ok(v)
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
			where
				E: Error,
			{
				v.try_into()
					.map_err(|_| E::custom("negative value cannot be converted to u64"))
			}

			// the only lossy case: cast, truncating and saturating at the
			// u64 bounds
			fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
				Ok(v as u64)
			}

			fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
			where
				E: Error,
			{
				let v = v.trim();
				if let Ok(u) = v.parse::<u64>() {
					return Ok(u);
				}
				if let Ok(i) = v.parse::<i64>() {
					return self.visit_i64(i);
				}
				if let Ok(f) = v.parse::<f64>() {
					return self.visit_f64(f);
				}
				Err(E::custom(format!("non-numeric u64 string: {v:?}")))
			}
		}

		deserializer.deserialize_any(TruncatingU64Visitor)
	}
}

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
