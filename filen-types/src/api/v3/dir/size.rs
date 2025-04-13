use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: uuid::Uuid,
	#[serde(with = "crate::serde::option::default")]
	pub sharer_id: Option<u64>,
	#[serde(with = "crate::serde::option::default")]
	pub receiver_id: Option<u64>,
	#[serde(with = "crate::serde::boolean::number")]
	pub trash: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Response {
	pub size: u64,
	pub files: u64,
	#[serde(rename = "folders")]
	pub dirs: u64,
}
