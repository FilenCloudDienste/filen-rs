macro_rules! parent_uuid_option_module {
	($mod_name:ident, $none_value:expr) => {
		pub mod $mod_name {
			use std::str::FromStr;

			use serde::Serialize;

			use crate::fs::UuidStr;

			pub fn deserialize<'de, D>(
				deserializer: D,
			) -> Result<Option<crate::fs::ParentUuid>, D::Error>
			where
				D: serde::Deserializer<'de>,
			{
				let value = crate::serde::cow::deserialize(deserializer)?;
				Ok(match value.as_ref() {
					$none_value => None,
					string => Some(
						UuidStr::from_str(string)
							.map_err(serde::de::Error::custom)?
							.into(),
					),
				})
			}

			pub fn serialize<S>(
				value: &Option<crate::fs::ParentUuid>,
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

parent_uuid_option_module!(base, "base");
