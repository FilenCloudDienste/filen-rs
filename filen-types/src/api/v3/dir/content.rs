use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, UuidStr},
};

pub const ENDPOINT: &str = "v3/dir/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: ParentUuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "uploads")]
	pub files: Vec<File<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	pub rm: Cow<'a, str>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: ParentUuid,
	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Directory<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "name")]
	pub meta: Cow<'a, EncryptedString>,
	pub parent: ParentUuid,
	pub color: Option<Cow<'a, str>>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
	#[serde(with = "crate::serde::boolean::number")]
	#[serde(rename = "is_sync")]
	pub is_sync: bool,
	#[serde(with = "crate::serde::boolean::number")]
	#[serde(rename = "is_default")]
	pub is_default: bool,
}
