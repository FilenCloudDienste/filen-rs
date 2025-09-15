use std::borrow::Cow;

use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/publicKey";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
}
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::rsa::public_key_der")]
	pub public_key: RsaPublicKey,
}
