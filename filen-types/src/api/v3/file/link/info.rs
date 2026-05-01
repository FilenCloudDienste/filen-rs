use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, LinkHashedPassword},
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/file/link/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub password: LinkHashedPassword<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: UuidStr,
	pub name: EncryptedString<'a>,
	pub mime: EncryptedString<'a>,
	#[serde(default)]
	pub password: Option<LinkHashedPassword<'a>>,

	pub size: EncryptedString<'a>,
	pub chunks: u64,

	pub region: Cow<'a, str>,
	pub bucket: Cow<'a, str>,

	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}
