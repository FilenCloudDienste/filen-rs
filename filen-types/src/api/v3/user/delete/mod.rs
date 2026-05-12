use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub mod all;
pub mod versions;

pub const ENDPOINT: &str = "v3/user/delete";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub two_factor_key: Cow<'a, str>,
}
