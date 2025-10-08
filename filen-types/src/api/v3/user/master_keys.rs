use serde::{Deserialize, Serialize};

use crate::crypto::EncryptedMasterKeys;

pub const ENDPOINT: &str = "v3/user/masterKeys";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub master_keys: EncryptedMasterKeys<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub keys: EncryptedMasterKeys<'a>,
}
