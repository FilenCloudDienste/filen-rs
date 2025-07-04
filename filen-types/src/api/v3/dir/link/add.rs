use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
	crypto::{EncryptedMetaKey, EncryptedString},
	fs::{ObjectType, UuidStr},
};

pub const ENDPOINT: &str = "v3/dir/link/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(with = "crate::serde::uuid::base")]
	pub parent: Option<UuidStr>,
	#[serde(rename = "linkUUID")]
	pub link_uuid: UuidStr,
	pub r#type: ObjectType,
	pub metadata: Cow<'a, EncryptedString>,
	pub key: Cow<'a, EncryptedMetaKey>,
	pub expiration: super::PublicLinkExpiration,
}
