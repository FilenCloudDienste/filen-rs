pub mod content;
pub mod create;
pub mod download;
pub mod exists;
pub mod link;
pub mod metadata;
pub mod r#move;
pub mod size;
pub mod trash;

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENDPOINT: &str = "v3/dir";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

use crate::crypto::EncryptedString;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub uuid: Uuid,
	#[serde(rename = "nameEncrypted")]
	pub metadata: Cow<'a, EncryptedString>,
	pub name_hashed: Cow<'a, str>,
	pub parent: Uuid,
	pub trash: bool,
	pub favorited: bool,
	pub color: Option<Cow<'a, str>>,
}
