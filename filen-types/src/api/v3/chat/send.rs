use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/send";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub conversation: UuidStr,
	pub uuid: UuidStr,
	pub message: EncryptedString<'a>,
	#[serde(
		serialize_with = "crate::serde::uuid::optional::serialize_as_str",
		deserialize_with = "crate::serde::uuid::optional::deserialize"
	)]
	pub reply_to: Option<UuidStr>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}
