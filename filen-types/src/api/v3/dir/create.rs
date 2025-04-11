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

impl From<Request> for reqwest::Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	pub uuid: uuid::Uuid,
}
