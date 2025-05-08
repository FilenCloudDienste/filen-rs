use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/dir/trash";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
}
