use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/upload/empty";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub name: EncryptedString<'a>,
	pub name_hashed: Cow<'a, str>, // should this be a string or a stronger type?
	pub size: EncryptedString<'a>,
	pub parent: Uuid,
	pub mime: EncryptedString<'a>,
	pub metadata: EncryptedString<'a>,
	pub version: FileEncryptionVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}
