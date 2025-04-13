use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: uuid::Uuid,
}
