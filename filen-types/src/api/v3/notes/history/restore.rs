use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/notes/history/restore";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub id: u64,
}
