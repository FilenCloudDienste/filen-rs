use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/dir/link/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub parent: UuidStr,
	pub metadata: EncryptedString<'a>,
	pub has_password: bool,
	#[serde(with = "crate::serde::hex::optional")]
	pub salt: Option<Cow<'a, [u8]>>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}
