use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request<'a> {
	pub uuid: Uuid,
	pub r#type: LockType,
	pub resource: Cow<'a, str>,
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

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
	pub acquired: bool,
	pub released: bool,
	pub refreshed: bool,
	pub resource: String,
	pub status: Option<LockStatus>,
}
