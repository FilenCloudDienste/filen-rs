use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, UuidStr},
};

pub const ENDPOINT: &str = "v3/dir/download";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: ParentUuid,
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
	pub uuid: UuidStr,
	#[serde(rename = "name")]
	pub meta: Cow<'a, EncryptedString>,
	#[serde(with = "crate::serde::parent_uuid::base")]
	pub parent: Option<ParentUuid>,
	pub color: DirColor<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub chunks: u64,
	pub size: Cow<'a, EncryptedString>,
	pub chunks_size: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub parent: ParentUuid,
	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}
