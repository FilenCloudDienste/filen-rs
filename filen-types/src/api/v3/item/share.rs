use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
	crypto::rsa::RSAEncryptedString,
	fs::{ObjectType, UuidStr},
};

pub const ENDPOINT: &str = "v3/item/share";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: UuidStr,
	#[serde(with = "crate::serde::uuid::none")]
	pub parent: Option<UuidStr>,
	pub email: Cow<'a, str>,
	pub r#type: ObjectType,
	pub metadata: RSAEncryptedString<'a>,
}
