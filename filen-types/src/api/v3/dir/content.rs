use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: uuid::Uuid,
}

impl From<Request> for reqwest::Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	#[serde(rename = "uploads")]
	pub files: Vec<File>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory>,
}

pub(crate) mod serde_u8_bool {
	use serde::{Deserialize, de::Unexpected};

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<bool, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		match serde_json::Value::deserialize(deserializer) {
			Ok(serde_json::Value::Bool(value)) => Ok(value),
			Ok(serde_json::Value::Number(num)) => {
				if let Some(value) = num.as_i64() {
					Ok(value != 0)
				} else if let Some(value) = num.as_u64() {
					Ok(value != 0)
				} else {
					Err(serde::de::Error::invalid_value(
						Unexpected::Other("not a boolean or number"),
						&"boolean or number",
					))
				}
			}
			Ok(other) => Err(serde::de::Error::invalid_value(
				Unexpected::Other(&format!("{:?}", other)),
				&"boolean or number",
			)),
			Err(e) => Err(e),
		}
	}

	pub(crate) fn serialize<S>(value: &bool, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		let value = if *value { 1 } else { 0 };
		serializer.serialize_u64(value)
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct File {
	pub uuid: uuid::Uuid,
	pub metadata: EncryptedString,
	pub rm: String,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
	pub bucket: String,
	pub region: String,
	pub parent: uuid::Uuid,
	pub version: FileEncryptionVersion,
	#[serde(with = "serde_u8_bool")]
	pub favorited: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Directory {
	pub uuid: uuid::Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString,
	pub parent: uuid::Uuid,
	pub color: Option<String>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "serde_u8_bool")]
	pub favorited: bool,
	#[serde(with = "serde_u8_bool")]
	pub is_sync: bool,
	#[serde(with = "serde_u8_bool")]
	pub is_default: bool,
}
