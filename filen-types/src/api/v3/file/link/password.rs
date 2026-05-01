use serde::{Deserialize, Serialize};

use crate::{api::v3::dir::link::info::LinkPasswordSalt, fs::UuidStr};

pub const ENDPOINT: &str = "v3/file/link/password";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub has_password: bool,
	pub salt: LinkPasswordSalt<'a>,
}
