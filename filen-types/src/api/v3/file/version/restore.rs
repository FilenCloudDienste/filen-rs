use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, StableUuid, Uuid},
};

pub const ENDPOINT: &str = "v3/file/version/restore";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	pub current: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: Uuid,
	#[serde(rename = "currentUUID")]
	pub current_uuid: Uuid,
	#[serde(rename = "stableUUID")]
	pub stable_uuid: StableUuid,
	pub metadata: EncryptedString<'a>,
	pub bucket: Cow<'a, str>,
	pub region: Cow<'a, str>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chunks: u64,
	pub parent: ParentUuid,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub version: FileEncryptionVersion,
	#[serde(with = "crate::serde::boolean::number")]
	pub favorited: bool,
}
