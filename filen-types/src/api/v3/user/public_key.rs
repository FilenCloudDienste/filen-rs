use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::crypto::rsa::EncodedPublicKey;

pub const ENDPOINT: &str = "v3/user/publicKey";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
}
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub public_key: EncodedPublicKey<'a>,
}
