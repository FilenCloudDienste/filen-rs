use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "/v3/chat/conversations/name/edit";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: UuidStr,
	pub name: Cow<'a, str>,
}
