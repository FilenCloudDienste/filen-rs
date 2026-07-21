pub mod delete;
pub mod exists;
pub mod link;
pub mod metadata;
pub mod r#move;
pub mod restore;
pub mod trash;
pub mod version;
pub mod versions;

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, Uuid},
};

pub const ENDPOINT: &str = "v3/file";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: Uuid,
	pub region: Cow<'a, str>,
	pub bucket: Cow<'a, str>,
	pub name_encrypted: EncryptedString<'a>,
	pub name_hashed: Cow<'a, str>,
	pub size_encrypted: EncryptedString<'a>,
	pub mime_encrypted: EncryptedString<'a>,
	pub metadata: EncryptedString<'a>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub size: u64,
	pub parent: ParentUuid,
	pub versioned: bool,
	pub trash: bool,
	pub version: FileEncryptionVersion,
	pub favorited: bool,
}
