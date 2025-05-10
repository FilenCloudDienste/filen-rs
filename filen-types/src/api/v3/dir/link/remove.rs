use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENDPOINT: &str = "v3/dir/link/remove";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}
