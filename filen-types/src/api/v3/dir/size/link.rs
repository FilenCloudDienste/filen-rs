use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/dir/size/link";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	#[serde(rename = "linkUUID")]
	pub link_uuid: Uuid,
}

pub use super::Response;
