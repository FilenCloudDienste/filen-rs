pub mod content;
pub mod create;
pub mod download;
pub mod exists;
pub mod metadata;
pub mod r#move;
pub mod size;
pub mod trash;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: Uuid,
}

use crate::crypto::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: Uuid,
	#[serde(rename = "nameEncrypted")]
	pub metadata: EncryptedString,
	pub name_hashed: String,
	pub parent: Uuid,
	pub trash: bool,
	pub favorited: bool,
	pub color: Option<String>,
}
