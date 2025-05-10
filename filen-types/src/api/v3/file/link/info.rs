use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

pub const ENDPOINT: &str = "v3/file/link/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub password: Cow<'a, [u8]>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: Uuid,
	pub name: Cow<'a, EncryptedString>,
	pub mime: Cow<'a, EncryptedString>,
	#[serde(with = "crate::serde::hex::optional")]
	pub password: Option<Cow<'a, [u8]>>,

	pub size: Cow<'a, EncryptedString>,
	pub chunks: u64,

	pub region: Cow<'a, str>,
	pub bucket: Cow<'a, str>,

	pub version: FileEncryptionVersion,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}
