use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::EncryptedPrivateKey, serde::rsa::RsaDerPublicKey};

pub const ENDPOINT: &str = "v3/user/keyPair/update";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub public_key: RsaDerPublicKey<'a>,
	pub private_key: EncryptedPrivateKey<'a>,
}
