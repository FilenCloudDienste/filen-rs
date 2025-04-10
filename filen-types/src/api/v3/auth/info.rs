use reqwest::Body;
use serde::{Deserialize, Serialize};

use crate::auth::AuthVersion;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub email: &'a str,
}

impl From<Request<'_>> for Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub auth_version: AuthVersion,
	pub salt: String, // this is not base64 or hex encoded, so probably bad practice, we should take a look at this
}
