use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub mod delete;
pub mod requests;

pub const ENDPOINT: &str = "v3/contacts";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response<'a>(pub Vec<Contact<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Contact<'a> {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub last_active: DateTime<Utc>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
}
