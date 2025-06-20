use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

pub const ENDPOINT: &str = "v3/dir/link/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password: Cow<'a, [u8]>,
	pub parent: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "folders")]
	pub dirs: Vec<Directory<'a>>,
	pub files: Vec<File<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Directory<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "chrono::serde::ts_seconds")]
	pub timestamp: DateTime<Utc>,
	pub color: Option<Cow<'a, str>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "chrono::serde::ts_seconds")]
	pub timestamp: DateTime<Utc>,
	pub size: u64,
	pub chunks: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
}
