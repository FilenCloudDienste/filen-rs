use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/conversations/participants/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(rename = "contactUUID")]
	pub contact_uuid: UuidStr,
	pub metadata: RSAEncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub last_active: DateTime<Utc>,
	pub appear_offline: bool,
}
