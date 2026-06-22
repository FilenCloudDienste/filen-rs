#[cfg(any(feature = "wasm-full", test))]
use chrono::{DateTime, Utc};
#[cfg(any(feature = "wasm-full", test))]
use serde::{Deserialize, Deserializer};

// Only referenced by the `wasm-full`-gated `deserialize_with` attributes on
// `DirectoryMetaChanges`/`FileMetaChanges`; kept under `test` for its unit test.
#[cfg(any(feature = "wasm-full", test))]
pub(crate) fn deserialize_double_option_timestamp<'de, D>(
	deserializer: D,
) -> Result<Option<Option<DateTime<Utc>>>, D::Error>
where
	D: Deserializer<'de>,
{
	match Option::<i64>::deserialize(deserializer)? {
		Some(ts) => Ok(Some(Some(DateTime::<Utc>::from_timestamp_nanos(
			ts * 1_000_000,
		)))),
		None => Ok(Some(None)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Debug, Deserialize)]
	struct MyStruct {
		#[serde(default, deserialize_with = "deserialize_double_option_timestamp")]
		field: Option<Option<DateTime<Utc>>>,
	}

	#[test]
	fn test_name() {
		let json1 = r#"{}"#;
		let result1: MyStruct = serde_json::from_str(json1).unwrap();
		assert_eq!(result1.field, None);

		let json2 = r#"{"field": null}"#;
		let result2: MyStruct = serde_json::from_str(json2).unwrap();
		assert_eq!(result2.field, Some(None));

		let json3 = r#"{"field": 124124}"#;
		let result3: MyStruct = serde_json::from_str(json3).unwrap();
		assert_eq!(
			result3.field,
			Some(Some(DateTime::<Utc>::from_timestamp_nanos(
				124124 * 1_000_000
			)))
		);
	}
}
