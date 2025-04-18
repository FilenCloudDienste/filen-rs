pub mod optional {
	use chrono::Utc;
	use serde::Deserialize;

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<chrono::DateTime<Utc>>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let value = Option::<i64>::deserialize(deserializer)?;
		Ok(match value {
			None => None,
			Some(timestamp) => chrono::DateTime::<Utc>::from_timestamp_millis(timestamp),
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
