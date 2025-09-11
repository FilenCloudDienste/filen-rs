use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/notes/title/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub title: Cow<'a, str>,
}
