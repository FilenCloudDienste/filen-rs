pub(crate) mod optional {
	use std::{fmt::Display, str::FromStr};

	use serde::{Deserialize, Serialize};

	use crate::serde::cow::CowStrWrapper;

	pub(crate) fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
	where
		T: FromStr,
		T::Err: Display,
		D: serde::Deserializer<'de>,
	{
		let value = Option::<CowStrWrapper>::deserialize(deserializer)?.map(|v| v.0);
		Ok(match value.as_deref() {
			Some("") | None => None,
			Some(string) => Some(T::from_str(string).map_err(serde::de::Error::custom)?),
		})
	}

	pub(crate) fn serialize<T, S>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
	where
		T: Serialize,
		S: serde::Serializer,
	{
		match value {
			Some(uuid) => uuid.serialize(serializer),
			None => serializer.serialize_none(),
		}
	}

	pub(crate) fn serialize_as_str<T, S>(
		value: &Option<T>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		T: Serialize,
		S: serde::Serializer,
	{
		match value {
			Some(uuid) => uuid.serialize(serializer),
			None => "".serialize(serializer),
		}
	}
}

macro_rules! uuid_option_module {
	($mod_name:ident, $none_value:expr) => {
		pub mod $mod_name {
			use std::{fmt::Display, str::FromStr};

			use serde::Serialize;

			pub fn deserialize<'de, T, D>(deserializer: D) -> Result<Option<T>, D::Error>
			where
				T: FromStr,
				T::Err: Display,
				D: serde::Deserializer<'de>,
			{
				let value = crate::serde::cow::deserialize(deserializer)?;
				Ok(match value.as_ref() {
					$none_value => None,
					string => Some(T::from_str(string).map_err(serde::de::Error::custom)?),
				})
			}

			pub fn serialize<T, S>(value: &Option<T>, serializer: S) -> Result<S::Ok, S::Error>
			where
				T: Serialize,
				S: serde::Serializer,
			{
				match value {
					Some(uuid) => uuid.serialize(serializer),
					None => serializer.serialize_str($none_value),
				}
			}
		}
	};
}

uuid_option_module!(base, "base");
uuid_option_module!(none, "none");
