use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const ENDPOINT: &str = "v3/contacts/requests/deny";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
}
