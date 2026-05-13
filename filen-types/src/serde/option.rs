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

/// `Option<Cow<'_, str>>` <-> JSON string, using the `"__NONE__"` sentinel
/// (per the Filen backend convention) to represent `None`. Used by the
/// `v3/user/personal/update` endpoint, where every field is mandatory on the
/// wire but the sentinel means "leave unchanged".
pub(crate) mod str_none_sentinel {
	use std::borrow::Cow;

	use serde::{Deserializer, Serializer};

	const NONE_SENTINEL: &str = "__NONE__";

	pub(crate) fn serialize<S>(
		value: &Option<Cow<'_, str>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(v) => serializer.serialize_str(v),
			None => serializer.serialize_str(NONE_SENTINEL),
		}
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let cow = crate::serde::cow::deserialize(deserializer)?;
		Ok(if cow == NONE_SENTINEL {
			None
		} else {
			Some(cow)
		})
	}
}

pub(crate) mod str_empty_is_none_owned {
	use std::borrow::Cow;

	use serde::{Deserializer, Serializer};

	pub(crate) fn serialize<V: AsRef<str>, S>(
		value: &Option<V>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let value = match value {
			Some(v) => Some(Cow::Borrowed(v.as_ref())),
			None => None,
		};
		super::str_empty_is_none_borrowed::serialize(&value, serializer)
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
	where
		D: Deserializer<'de>,
	{
		super::str_empty_is_none_borrowed::deserialize(deserializer)
			.map(|opt| opt.map(|cow| cow.into_owned()))
	}
}

pub(crate) mod str_empty_is_none_borrowed {
	use std::borrow::Cow;

	use serde::{Deserialize, Deserializer, Serializer};

	use crate::serde::cow::CowStrWrapper;

	pub(crate) fn serialize<S>(
		value: &Option<Cow<'_, str>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		match value {
			Some(v) => serializer.serialize_str(v),
			None => serializer.serialize_none(),
		}
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Cow<'de, str>>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let cow = Option::<CowStrWrapper<'de>>::deserialize(deserializer)?;
		match cow {
			None => Ok(None),
			Some(CowStrWrapper(cow)) if cow.is_empty() => Ok(None),
			Some(CowStrWrapper(cow)) => Ok(Some(cow)),
		}
	}
}
