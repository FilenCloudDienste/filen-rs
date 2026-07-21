use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
	crypto::{EncryptedString, rsa::RSAEncryptedString},
	fs::Uuid,
};

pub const ENDPOINT: &str = "v3/chat/conversations/create";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub metadata: RSAEncryptedString<'a>,
	pub owner_metadata: EncryptedString<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub uuid: Uuid,
}
