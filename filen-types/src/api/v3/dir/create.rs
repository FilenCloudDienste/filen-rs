use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::crypto::EncryptedString;

pub const ENDPOINT: &str = "v3/dir/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: uuid::Uuid,
	#[serde(rename = "name")]
	pub meta: Cow<'a, EncryptedString>,
	pub name_hashed: Cow<'a, str>,
	pub parent: uuid::Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: uuid::Uuid,
}
