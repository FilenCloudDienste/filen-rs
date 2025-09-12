use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::rsa::RSAEncryptedString,
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/shared/in";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	#[serde(with = "crate::serde::uuid::shared_in")]
	pub uuid: Option<UuidStr>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "uploads")]
	pub files: Vec<SharedFileIn<'a>>,
	#[serde(rename = "folders")]
	pub dirs: Vec<SharedDirIn<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedFileIn<'a> {
	pub uuid: UuidStr,
	pub parent: Option<UuidStr>,
	pub metadata: RSAEncryptedString<'a>,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub chunks: u64,
	pub size: u64,
	pub version: FileEncryptionVersion,
	pub sharer_email: Cow<'a, str>,
	pub sharer_id: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedDirIn<'a> {
	pub uuid: UuidStr,
	pub parent: Option<UuidStr>,
	pub metadata: RSAEncryptedString<'a>,
	pub sharer_email: Cow<'a, str>,
	pub sharer_id: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub write_access: bool,
	pub color: DirColor<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number", rename = "is_sync")]
	pub is_sync: bool,
	#[serde(with = "crate::serde::boolean::number", rename = "is_default")]
	pub is_default: bool, // might be redundant with parent option
}
