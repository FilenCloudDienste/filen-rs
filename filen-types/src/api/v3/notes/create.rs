use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/notes/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub title: EncryptedString<'a>,
	pub metadata: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub created_timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub edited_timestamp: DateTime<Utc>,
}
