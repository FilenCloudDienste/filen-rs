use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::EncryptedPrivateKey, serde::rsa::RsaDerPublicKey};

pub const ENDPOINT: &str = "v3/user/keyPair/info";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	/// `None` until a first regular login generates and stores the account's key pair.
	pub public_key: Option<RsaDerPublicKey<'a>>,
	pub private_key: Option<EncryptedPrivateKey<'a>>,
}
