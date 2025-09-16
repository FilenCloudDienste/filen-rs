use serde::{Deserialize, Serialize};

use crate::{crypto::EncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/chat/send";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	conversation: UuidStr,
	uuid: UuidStr,
	message: EncryptedString<'a>,
	reply_to: Option<UuidStr>,
}
