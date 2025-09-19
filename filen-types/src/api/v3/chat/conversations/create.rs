use serde::{Deserialize, Serialize};

use crate::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::UuidStr,
};

pub const ENDPOINT: &str = "v3/chat/conversations/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub metadata: RSAEncryptedString<'a>,
	pub owner_metadata: EncryptedString<'a>,
}
