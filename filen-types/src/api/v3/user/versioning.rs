use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/versioning";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	#[serde(with = "crate::serde::boolean::number")]
	pub enabled: bool,
}
