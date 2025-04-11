use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::content::serde_u8_bool;
use crate::crypto::EncryptedString;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
	pub skip_cache: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub files: Vec<super::content::File>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory>,
}

mod serde_base_uuid {
	use serde::Deserialize;

	pub(super) fn deserialize<'de, D>(deserializer: D) -> Result<Option<uuid::Uuid>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		if s == "base" {
			return Ok(None);
		}
		Ok(Some(
			uuid::Uuid::parse_str(&s).map_err(serde::de::Error::custom)?,
		))
	}

	pub(super) fn serialize<S>(value: &Option<uuid::Uuid>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match value {
			Some(uuid) => serializer.serialize_str(&uuid.to_string()),
			None => serializer.serialize_str("base"),
		}
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Directory {
	pub uuid: uuid::Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString,
	#[serde(with = "serde_base_uuid")]
	pub parent: Option<uuid::Uuid>,
	pub color: Option<String>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "serde_u8_bool")]
	pub favorited: bool,
}
