use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/confirmation/send";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
}
