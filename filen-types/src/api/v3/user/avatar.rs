use serde::{Deserialize, Serialize};
use url::Url;

use crate::{crypto::Sha512Hash, serde::str::Base64EncodedBytes};

pub const ENDPOINT: &str = "v3/user/avatar";

#[derive(Deserialize, Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub avatar: Base64EncodedBytes<'a>,
	pub hash: Sha512Hash,
}

#[derive(Deserialize, Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(rename = "avatarURL")]
	pub avatar_url: Url,
}
