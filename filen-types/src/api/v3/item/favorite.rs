use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fs::ObjectType;

pub const ENDPOINT: &str = "v3/item/favorite";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Request {
	pub uuid: Uuid,
	pub r#type: ObjectType,
	#[serde(with = "crate::serde::boolean::number")]
	pub value: bool,
}

pub type Response = Request;
