use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/upload/empty";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub name: EncryptedString<'a>,
	pub name_hashed: Cow<'a, str>, // should this be a string or a stronger type?
	pub size: EncryptedString<'a>,
	pub parent: UuidStr,
	pub mime: EncryptedString<'a>,
	pub metadata: EncryptedString<'a>,
	pub version: FileEncryptionVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub chunks: u64,
	pub size: u64,
}
