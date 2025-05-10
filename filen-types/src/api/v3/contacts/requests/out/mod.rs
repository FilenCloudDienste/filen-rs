use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod delete;

pub const ENDPOINT: &str = "v3/contacts/requests/out";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response<'a>(pub Vec<ContactRequestOut<'a>>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ContactRequestOut<'a> {
	pub uuid: Uuid,
	pub email: Cow<'a, str>,
	pub avatar: Option<Cow<'a, str>>,
	pub nick_name: Cow<'a, str>,
	// #[serde(with = "chrono::serde::ts_milliseconds")]
	// pub timestamp: DateTime<Utc>,
}
