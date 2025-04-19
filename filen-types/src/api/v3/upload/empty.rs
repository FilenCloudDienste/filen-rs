use serde::{Deserialize, Serialize};

use crate::{auth::FileEncryptionVersion, crypto::EncryptedString};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
	pub name: EncryptedString,
	pub name_hashed: String, // should this be a string or a stronger type?
	pub size: EncryptedString,
	pub parent: uuid::Uuid,
	pub mime: EncryptedString,
	pub metadata: EncryptedString,
	pub version: FileEncryptionVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub chunks: u64,
	pub size: u64,
}
