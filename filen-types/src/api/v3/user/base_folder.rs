use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/baseFolder";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub uuid: uuid::Uuid,
}
