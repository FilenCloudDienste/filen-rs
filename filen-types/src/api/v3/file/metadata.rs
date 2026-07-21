use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/file/metadata";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub name: EncryptedString<'a>,
	pub name_hashed: Cow<'a, str>,
	pub metadata: EncryptedString<'a>,
}
