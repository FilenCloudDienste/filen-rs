use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub hashes: Vec<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum SearchFindItem {
	#[serde(rename = "directory")]
	Dir(SearchFindDirectory),
	#[serde(rename = "file")]
	File(SearchFindFile),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchFindFile {
	pub uuid: Uuid,
	pub metadata: EncryptedString,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
	pub bucket: String,
	pub region: String,
	pub parent: Uuid,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
	pub trash: bool,
	pub versioned: bool,
	pub uuid_path: Vec<Uuid>,
	pub metadata_path: Vec<EncryptedString>,
	pub name_hashed: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchFindDirectory {
	pub uuid: Uuid,
	pub metadata: EncryptedString,
	pub parent: Uuid,
	pub color: Option<String>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
	pub trash: bool,
	pub uuid_path: Vec<Uuid>,
	pub metadata_path: Vec<EncryptedString>,
	pub name_hashed: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub items: Vec<SearchFindItem>,
}
