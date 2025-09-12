use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/file/link/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password: Cow<'a, [u8]>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: UuidStr,
	pub name: EncryptedString<'a>,
	pub mime: EncryptedString<'a>,
	#[serde(with = "crate::serde::hex::optional")]
	pub password: Option<Cow<'a, [u8]>>,

	pub size: EncryptedString<'a>,
	pub chunks: u64,

	pub region: Cow<'a, str>,
	pub bucket: Cow<'a, str>,

	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}
