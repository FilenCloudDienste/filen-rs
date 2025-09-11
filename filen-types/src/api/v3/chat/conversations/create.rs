use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "/v3/chat/conversations/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub metadata: Cow<'a, EncryptedString>,
	pub owner_metadata: Cow<'a, EncryptedString>,
}
