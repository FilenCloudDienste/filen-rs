use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/notes/participants/permissions";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub user_id: u64,
	pub permissions_write: bool,
}
