use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::auth::AuthVersion;

pub const ENDPOINT: &str = "v3/auth/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub auth_version: AuthVersion,
	pub salt: Cow<'a, str>, // this is not base64 or hex encoded, so probably bad practice, we should take a look at this
}
