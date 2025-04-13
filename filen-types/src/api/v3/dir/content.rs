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
	#[serde(with = "crate::serde::boolean::number")]
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
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
	#[serde(with = "crate::serde::boolean::number")]
	pub is_sync: bool,
	#[serde(with = "crate::serde::boolean::number")]
	pub is_default: bool,
}
