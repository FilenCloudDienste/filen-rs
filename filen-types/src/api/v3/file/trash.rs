use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: Uuid,
}
