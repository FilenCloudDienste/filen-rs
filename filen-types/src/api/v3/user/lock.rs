use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENDPOINT: &str = "v3/user/lock";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub r#type: LockType,
	pub resource: Cow<'a, str>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub acquired: bool,
	pub released: bool,
	pub refreshed: bool,
	pub resource: Cow<'a, str>,
	pub status: Option<LockStatus>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum LockType {
	Acquire,
	Refresh,
	Status,
	Release,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub enum LockStatus {
	Locked,
}
