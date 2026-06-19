use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/chat/unread";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub unread: u64,
}
