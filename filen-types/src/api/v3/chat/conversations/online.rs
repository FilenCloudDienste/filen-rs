use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/chat/conversations/online";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub conversation: Uuid,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(transparent)]
pub struct Response(pub Vec<OnlineStatus>);

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OnlineStatus {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub user_id: u64,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub last_active: DateTime<Utc>,
	pub appear_offline: bool,
}
