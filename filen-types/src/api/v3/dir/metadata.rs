use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	pub name_hashed: String,
	#[serde(rename = "name")]
	pub metadata: EncryptedString,
}
