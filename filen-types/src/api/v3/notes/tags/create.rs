use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/notes/tags/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub name: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
}
