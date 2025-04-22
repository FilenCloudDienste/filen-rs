pub mod delete;
pub mod exists;
pub mod metadata;
pub mod r#move;
pub mod restore;
pub mod trash;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: Uuid,
	pub region: String,
	pub bucket: String,
	pub name_encrypted: EncryptedString,
	pub name_hashed: String,
	pub size_encrypted: EncryptedString,
	pub mime_encrypted: EncryptedString,
	pub metadata: EncryptedString,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub timestamp: DateTime<Utc>,
	pub size: u64,
	pub parent: Uuid,
	pub versioned: bool,
	pub trash: bool,
	pub version: FileEncryptionVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: Uuid,
}
