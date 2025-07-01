use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, Sha256Hash},
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/search/find";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub hashes: Vec<Sha256Hash>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response<'a> {
	pub items: Vec<SearchFindItem<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
pub enum SearchFindItem<'a> {
	#[serde(rename = "directory")]
	Dir(SearchFindDirectory<'a>),
	#[serde(rename = "file")]
	File(SearchFindFile<'a>),
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchFindFile<'a> {
	pub uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: UuidStr,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
	pub trash: bool,
	pub versioned: bool,
	pub uuid_path: Vec<UuidStr>,
	pub metadata_path: Vec<Cow<'a, EncryptedString>>,
	pub name_hashed: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchFindDirectory<'a> {
	pub uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	pub parent: UuidStr,
	pub color: Option<Cow<'a, str>>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
	pub trash: bool,
	pub uuid_path: Vec<UuidStr>,
	pub metadata_path: Vec<Cow<'a, EncryptedString>>,
	pub name_hashed: Cow<'a, str>,
}
