use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub name_hashed: String,
	pub parent: uuid::Uuid,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
	pub exists: bool,
	#[serde(with = "crate::serde::uuid::optional")]
	pub uuid: Option<uuid::Uuid>,
}
