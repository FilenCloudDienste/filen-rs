use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/dir/move";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
	pub to: uuid::Uuid,
}
