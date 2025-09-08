use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub mod add;
pub mod delete;

pub const ENDPOINT: &str = "v3/contacts/blocked";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response<'a>(pub Vec<BlockedContact<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BlockedContact<'a> {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}
