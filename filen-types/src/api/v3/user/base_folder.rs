use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
	pub uuid: uuid::Uuid,
}
