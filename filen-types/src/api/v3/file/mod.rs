pub mod delete;
pub mod exists;
pub mod link;
pub mod metadata;
pub mod r#move;
pub mod restore;
pub mod trash;

use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	auth::FileEncryptionVersion,
	crypto::EncryptedString,
	fs::{ParentUuid, UuidStr},
};

pub const ENDPOINT: &str = "v3/file";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: UuidStr,
	pub region: Cow<'a, str>,
	pub bucket: Cow<'a, str>,
	pub name_encrypted: Cow<'a, EncryptedString>,
	pub name_hashed: Cow<'a, str>,
	pub size_encrypted: Cow<'a, EncryptedString>,
	pub mime_encrypted: Cow<'a, EncryptedString>,
	pub metadata: Cow<'a, EncryptedString>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub size: u64,
	pub parent: ParentUuid,
	pub versioned: bool,
	pub trash: bool,
	pub version: FileEncryptionVersion,
}
