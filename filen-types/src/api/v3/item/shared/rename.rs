use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::UuidStr};

pub const ENDPOINT: &str = "v3/item/shared/rename";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub receiver_id: u64,
	pub metadata: Cow<'a, RSAEncryptedString>,
}
