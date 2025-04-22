use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: Uuid,
	#[serde(rename = "to")]
	pub new_parent: Uuid,
}
