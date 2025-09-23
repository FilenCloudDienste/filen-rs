pub(crate) mod default {
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	pub fn serialize<S: Serializer, T: Serialize + Default>(
		value: &Option<T>,
		serializer: S,
	) -> Result<S::Ok, S::Error> {
		match value {
			Some(v) => v.serialize(serializer),
			None => T::default().serialize(serializer),
		}
	}

	pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de> + Default>(
		deserializer: D,
	) -> Result<Option<T>, D::Error> {
		Option::<T>::deserialize(deserializer)
	}
}

pub(crate) mod result_to_option {
	use serde::Deserialize;

	pub(crate) fn deserialize<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(
		deserializer: D,
	) -> Result<Option<T>, D::Error> {
		let value = serde_json::Value::deserialize(deserializer)?;
		let deserialized: Result<T, _> = serde_json::from_value(value);
		Ok(deserialized.ok())
	}
}
