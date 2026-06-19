use filen_macros::js_type;
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/messageIds";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[js_type(
	no_default,
	no_ser,
	no_deser,
	export,
	wasm_target = not(feature = "service-worker")
)]
pub struct Response {
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub general: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub chat: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub contact: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub note: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub drive: u64,
}
