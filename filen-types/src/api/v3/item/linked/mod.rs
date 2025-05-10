use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::EncryptedMetaKey;

pub mod rename;

pub const ENDPOINT: &str = "v3/item/linked";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub link: bool,
	pub links: Vec<ListedPublicLink<'a>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ListedPublicLink<'a> {
	#[serde(rename = "linkUUID")]
	pub link_uuid: Uuid,
	pub link_key: Cow<'a, EncryptedMetaKey>,
}
