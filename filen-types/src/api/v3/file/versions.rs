use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/file/versions";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct FileVersion<'a> {
	pub bucket: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	pub metadata: EncryptedString<'a>,
	pub region: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub uuid: Uuid,
	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub versions: Vec<FileVersion<'a>>,
}
