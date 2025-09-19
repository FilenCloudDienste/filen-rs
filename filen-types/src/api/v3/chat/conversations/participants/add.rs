use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/conversations/participants/add";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub contact_uuid: UuidStr,
	pub metadata: RSAEncryptedString<'a>,
}
