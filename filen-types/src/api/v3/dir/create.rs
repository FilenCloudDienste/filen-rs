use serde::{Deserialize, Serialize};

use crate::crypto::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
	#[serde(rename = "name")]
	pub meta: EncryptedString,
	pub name_hashed: String,
	pub parent: uuid::Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub uuid: uuid::Uuid,
}
