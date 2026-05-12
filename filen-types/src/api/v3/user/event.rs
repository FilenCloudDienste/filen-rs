use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/user/event";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: UuidStr,
}
