use serde::{Deserialize, Serialize};

use crate::{crypto::Sha256Hash, fs::ObjectType2};

pub const ENDPOINT: &str = "v3/search/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub items: Vec<SearchAddItem>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	added: u64,
	skipped: u64,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchAddItem {
	pub uuid: uuid::Uuid,
	pub hash: Sha256Hash,
	pub r#type: ObjectType2,
}
