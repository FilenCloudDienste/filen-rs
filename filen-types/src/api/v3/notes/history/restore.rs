use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/notes/history/restore";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
	pub id: u64,
}
