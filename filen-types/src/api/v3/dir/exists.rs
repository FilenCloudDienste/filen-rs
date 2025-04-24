use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub name_hashed: String,
	pub parent: uuid::Uuid,
}

pub use crate::api::v3::file::exists::Response;
