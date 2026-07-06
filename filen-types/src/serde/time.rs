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

/// `serde(default)` helper for timestamps in decrypted metadata.
pub fn unix_epoch() -> chrono::DateTime<chrono::Utc> {
	chrono::DateTime::UNIX_EPOCH
}

/// Timestamp deserialization for DECRYPTED metadata, where other clients
/// write floats (e.g. a raw JS `mtimeMs`) or numeric strings. Floats are the
/// only lossy case: they are cast to millis (truncating, saturating at the
/// i64 bounds). Numeric strings convert to an integer or float first and then
/// follow the same rules. Every other type (null, bool, non-numeric string,
/// object, array) fails like the strict [`seconds_or_millis`], which wire API
/// types must keep using. Serialization stays strict integer millis.
pub mod truncating_seconds_or_millis {
	use chrono::{DateTime, Utc};
	use serde::{Deserializer, Serializer, de::Error};

	pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
	where
		D: Deserializer<'de>,
	{
		super::truncating_visitor::deserialize_opt(deserializer)?
			.ok_or_else(|| D::Error::custom("timestamp out of representable range"))
	}

	pub fn serialize<S>(value: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		super::seconds_or_millis::serialize(value, serializer)
	}
}

/// Optional twin of [`truncating_seconds_or_millis`]. Like the strict
/// [`optional`], null means `None` and an unrepresentable numeric timestamp
/// degrades to `None`; wrong types still fail.
pub mod truncating_seconds_or_millis_opt {
	use chrono::{DateTime, Utc};
	use serde::{Deserializer, de::Visitor};

	pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<DateTime<Utc>>, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct OptionalVisitor;

		impl<'de> Visitor<'de> for OptionalVisitor {
			type Value = Option<DateTime<Utc>>;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter
					.write_str("a unix timestamp as an integer, float or numeric string, or null")
			}

			fn visit_none<E>(self) -> Result<Self::Value, E> {
				Ok(None)
			}

			fn visit_unit<E>(self) -> Result<Self::Value, E> {
				Ok(None)
			}

			fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
			where
				D: Deserializer<'de>,
			{
				super::truncating_visitor::deserialize_opt(deserializer)
			}
		}

		deserializer.deserialize_option(OptionalVisitor)
	}

	pub fn serialize<S>(value: &Option<DateTime<Utc>>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		super::optional::serialize(value, serializer)
	}
}

mod truncating_visitor {
	use chrono::{DateTime, Utc};
	use serde::{
		Deserializer,
		de::{Error, Visitor},
	};

	use super::seconds_or_millis::from_seconds_or_millis;

	/// `Ok(None)` means a valid numeric input whose timestamp is not
	/// representable (mirroring how the strict [`super::optional`] flattens
	/// those to `None`); wrong types are errors.
	pub(super) fn deserialize_opt<'de, D>(
		deserializer: D,
	) -> Result<Option<DateTime<Utc>>, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct TruncatingTimestampVisitor;

		impl<'de> Visitor<'de> for TruncatingTimestampVisitor {
			type Value = Option<DateTime<Utc>>;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str(
					"a unix timestamp in seconds or milliseconds as an integer, float or numeric string",
				)
			}

			fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
				Ok(from_seconds_or_millis(v))
			}

			fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
			where
				E: Error,
			{
				i64::try_from(v)
					.map_err(|_| E::custom(format!("timestamp {v} does not fit in an i64")))
					.map(from_seconds_or_millis)
			}

			// the only lossy case: cast, truncating and saturating at the
			// i64 bounds
			fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
				Ok(from_seconds_or_millis(v as i64))
			}

			fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
			where
				E: Error,
			{
				let v = v.trim();
				if let Ok(i) = v.parse::<i64>() {
					return self.visit_i64(i);
				}
				if let Ok(f) = v.parse::<f64>() {
					return self.visit_f64(f);
				}
				Err(E::custom(format!("non-numeric timestamp string: {v:?}")))
			}
		}

		deserializer.deserialize_any(TruncatingTimestampVisitor)
	}
}

pub mod seconds_or_millis {
	use chrono::{DateTime, Utc};
	use serde::{Deserialize, Deserializer, Serializer};

	pub(crate) fn from_seconds_or_millis(value: i64) -> Option<DateTime<Utc>> {
		// i128 arithmetic: `value * 1000` and `now - value` overflow i64 for
		// values near its bounds
		let now = i128::from(Utc::now().timestamp_millis());
		let value = i128::from(value);
		let millis = if (now - value).abs() < (now - value * 1000).abs() {
			value
		} else {
			value * 1000
		};
		DateTime::<Utc>::from_timestamp_millis(i64::try_from(millis).ok()?)
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

#[cfg(test)]
mod tests {
	use chrono::{DateTime, Utc};
	use serde::Deserialize;

	use super::seconds_or_millis::from_seconds_or_millis;

	#[derive(Deserialize)]
	struct Strict {
		#[serde(with = "super::seconds_or_millis")]
		t: DateTime<Utc>,
	}

	#[derive(Deserialize)]
	struct Optional {
		#[serde(with = "super::optional")]
		t: Option<DateTime<Utc>>,
	}

	#[derive(Deserialize)]
	struct TruncatingOpt {
		#[serde(with = "super::truncating_seconds_or_millis_opt")]
		t: Option<DateTime<Utc>>,
	}

	#[test]
	fn seconds_scale_is_interpreted_as_seconds() {
		let secs = Utc::now().timestamp();
		let parsed = from_seconds_or_millis(secs).unwrap();
		assert_eq!(parsed.timestamp_millis(), secs * 1000);
	}

	#[test]
	fn millis_scale_is_interpreted_as_millis() {
		let millis = Utc::now().timestamp_millis();
		let parsed = from_seconds_or_millis(millis).unwrap();
		assert_eq!(parsed.timestamp_millis(), millis);
	}

	#[test]
	fn extreme_timestamps_yield_none_without_panicking() {
		for value in [
			9_200_000_000_000_000_000,
			i64::MAX,
			i64::MIN,
			i64::MIN + 1,
			i64::MAX / 1000 + 1,
		] {
			assert_eq!(from_seconds_or_millis(value), None, "value: {value}");
		}
	}

	#[test]
	fn strict_deserializer_rejects_out_of_range_timestamps() {
		assert!(serde_json::from_str::<Strict>(r#"{"t":9200000000000000000}"#).is_err());
		assert!(serde_json::from_str::<Strict>(r#"{"t":-9223372036854775808}"#).is_err());
	}

	#[test]
	fn strict_deserializer_accepts_current_millis() {
		let now = Utc::now().timestamp_millis();
		let parsed: Strict = serde_json::from_str(&format!(r#"{{"t":{now}}}"#)).unwrap();
		assert_eq!(parsed.t.timestamp_millis(), now);
	}

	#[test]
	fn optional_deserializer_flattens_out_of_range_to_none() {
		let parsed: Optional = serde_json::from_str(r#"{"t":9200000000000000000}"#).unwrap();
		assert_eq!(parsed.t, None);
	}

	#[test]
	fn truncating_optional_degrades_out_of_range_float_to_none() {
		let parsed: TruncatingOpt = serde_json::from_str(r#"{"t":"9.2e21"}"#).unwrap();
		assert_eq!(parsed.t, None);
	}
}
