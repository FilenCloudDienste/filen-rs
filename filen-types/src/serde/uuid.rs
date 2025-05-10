pub(crate) mod optional {
	use serde::Deserialize;

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<uuid::Uuid>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let value = Option::<&str>::deserialize(deserializer)?;
		Ok(match value {
			Some("") | None => None,
			Some(string) => Some(uuid::Uuid::parse_str(string).map_err(serde::de::Error::custom)?),
		})
	}

	pub(crate) fn serialize<S>(value: &Option<uuid::Uuid>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match value {
			Some(uuid) => uuid::serde::simple::serialize(uuid, serializer),
			None => serializer.serialize_none(),
		}
	}
}

macro_rules! uuid_option_module {
	($mod_name:ident, $none_value:expr) => {
		pub mod $mod_name {
			use serde::{Deserialize, Serialize};

			pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<uuid::Uuid>, D::Error>
			where
				D: serde::Deserializer<'de>,
			{
				let value = <&str>::deserialize(deserializer)?;
				Ok(match value {
					$none_value => None,
					string => {
						Some(uuid::Uuid::parse_str(string).map_err(serde::de::Error::custom)?)
					}
				})
			}

			pub fn serialize<S>(
				value: &Option<uuid::Uuid>,
				serializer: S,
			) -> Result<S::Ok, S::Error>
			where
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
uuid_option_module!(shared_out, "shared-out");
uuid_option_module!(shared_in, "shared-in");
uuid_option_module!(none, "none");
