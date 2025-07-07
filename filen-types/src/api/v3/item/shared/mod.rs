use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{crypto::rsa::EncodedPublicKey, fs::UuidStr};

pub mod r#in;
pub mod out;
pub mod rename;

pub const ENDPOINT: &str = "v3/item/shared";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub sharing: bool,
	pub users: Vec<SharedUser<'a>>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SharedUser<'a> {
	pub id: u64,
	pub email: Cow<'a, str>,
	pub public_key: Cow<'a, EncodedPublicKey>,
}
