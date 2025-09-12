use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedMetaKey, fs::UuidStr};

pub mod rename;

pub const ENDPOINT: &str = "v3/item/linked";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
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
	pub link_uuid: UuidStr,
	pub link_key: EncryptedMetaKey<'a>,
}
