use serde::{Deserialize, Serialize};

use crate::fs::UuidStr;

pub const ENDPOINT: &str = "v3/dir/size";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: UuidStr,
	#[serde(with = "crate::serde::option::default")]
	pub sharer_id: Option<u64>,
	#[serde(with = "crate::serde::option::default")]
	pub receiver_id: Option<u64>,
	#[serde(with = "crate::serde::boolean::number")]
	pub trash: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub size: u64,
	pub files: u64,
	#[serde(rename = "folders")]
	pub dirs: u64,
}
