use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Directory {
	pub uuid: uuid::Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString,
	#[serde(with = "crate::serde::uuid::base")]
	pub parent: Option<uuid::Uuid>,
	pub color: Option<String>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}
