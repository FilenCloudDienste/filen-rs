use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/file/metadata";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub name: Cow<'a, EncryptedString>,
	pub name_hashed: Cow<'a, str>,
	pub metadata: Cow<'a, EncryptedString>,
}
