pub mod optional {
	use chrono::Utc;
	use serde::Deserialize;

	use crate::serde::time::seconds_or_millis;

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<chrono::DateTime<Utc>>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let value = Option::<i64>::deserialize(deserializer)?;
		Ok(match value {
			None => None,
			Some(timestamp) => seconds_or_millis::from_seconds_or_millis(timestamp),
		})
	}

	pub fn serialize<S>(
		value: &Option<chrono::DateTime<Utc>>,
		serializer: S,
	) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match value {
			Some(time) => {
				let timestamp = time.timestamp_millis();
				if timestamp == 0 {
					serializer.serialize_none()
				} else {
					serializer.serialize_i64(timestamp)
				}
			}
			None => serializer.serialize_none(),
		}
	}
}

pub mod seconds_or_millis {
	use chrono::{DateTime, Utc};
	use serde::{Deserialize, Deserializer, Serializer};

	pub(crate) fn from_seconds_or_millis(value: i64) -> Option<DateTime<Utc>> {
		let now = Utc::now().timestamp_millis();
		DateTime::<Utc>::from_timestamp_millis(
			if (now - value).abs() < (now - value * 1000).abs() {
				value
			} else {
				value * 1000
			},
		)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let value = i64::deserialize(deserializer)?;
		from_seconds_or_millis(value).ok_or_else(|| {
			serde::de::Error::custom(format!("Failed to deserialize timestamp: {value}"))
		})
	}

	pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.serialize_i64(value.timestamp_millis())
	}
}
