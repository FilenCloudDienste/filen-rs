use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/chat/lastFocusUpdate";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	conversations: Vec<ChatLastFocusValues>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ChatLastFocusValues {
	uuid: UuidStr,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	last_focus: DateTime<Utc>,
}
