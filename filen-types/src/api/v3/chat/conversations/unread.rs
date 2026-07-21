use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/chat/conversations/unread";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub unread: u64,
}
