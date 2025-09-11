use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/notes/tags/rename";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub name: Cow<'a, str>,
}
