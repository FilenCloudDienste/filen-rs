pub const ENDPOINT: &str = "v3/dir/download/shared";

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::rsa::RSAEncryptedString, fs::Uuid};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	pub skip_cache: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub files: Vec<File<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Directory<'a> {
	pub uuid: Uuid,
	#[serde(rename = "name")]
	pub meta: RSAEncryptedString<'a>,
	#[serde(with = "crate::serde::uuid::base")]
	pub parent: Option<Uuid>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub metadata: RSAEncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks_size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: Uuid,
	pub version: FileEncryptionVersion,
}
