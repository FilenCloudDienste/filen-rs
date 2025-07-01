use std::borrow::Cow;

// use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/contacts/requests/in";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response<'a>(pub Vec<ContactRequestIn<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestIn<'a> {
	pub uuid: UuidStr,
	pub user_id: u64,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	// #[serde(with = "chrono::serde::ts_milliseconds")]
	// pub timestamp: DateTime<Utc>,
}
