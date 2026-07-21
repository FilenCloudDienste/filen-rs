use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::RSAEncryptedString, fs::Uuid};

pub const ENDPOINT: &str = "v3/item/shared/rename";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub receiver_id: u64,
	pub metadata: RSAEncryptedString<'a>,
}
