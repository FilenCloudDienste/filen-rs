use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::rsa::EncodedPublicKey;

pub mod rename;

pub const ENDPOINT: &str = "v3/item/shared";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
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
