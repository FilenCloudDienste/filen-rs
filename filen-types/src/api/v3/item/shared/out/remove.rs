use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/item/shared/out/remove";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	pub uuid: Uuid,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub receiver_id: u64,
}
