use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request {
	pub uuid: uuid::Uuid,
	pub to: uuid::Uuid,
}
