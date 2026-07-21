use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	api::v3::dir::color::DirColor,
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, LinkHashedPassword},
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/dir/link/content";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub password: LinkHashedPassword<'a>,
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
	pub metadata: EncryptedString<'a>,
	#[serde(with = "chrono::serde::ts_seconds")]
	pub timestamp: DateTime<Utc>,
	pub color: DirColor<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct File<'a> {
	pub uuid: Uuid,
	pub parent: Uuid,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "chrono::serde::ts_seconds")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	pub version: FileEncryptionVersion,
}
