use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/chat/unread";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Response {
	unread: u64,
}
