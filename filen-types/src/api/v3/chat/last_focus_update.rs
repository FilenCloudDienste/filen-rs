use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "/v3/chat/lastFocusUpdate";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	conversations: Vec<ChatLastFocusValues>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatLastFocusValues {
	uuid: UuidStr,
	last_focus: u64,
}
