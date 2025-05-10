use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENDPOINT: &str = "v3/file/link/password";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub has_password: bool,
	#[serde(with = "faster_hex::nopfx_ignorecase")]
	pub salt: Cow<'a, [u8]>,
}
