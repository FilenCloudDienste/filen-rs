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
	/// The lock announcement counter. Servers that predate lock announcements leave it out,
	/// which reads as 0.
	#[serde(default, with = "crate::serde::number::permissive_u64")]
	pub lock: u64,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_lock_counter_is_read_when_the_server_sends_it() {
		let response: Response = serde_json::from_str(
			r#"{"general":1,"chat":2,"contact":3,"note":4,"drive":5,"lock":"6"}"#,
		)
		.unwrap();
		assert_eq!(
			(
				response.general,
				response.chat,
				response.contact,
				response.note,
				response.drive,
				response.lock
			),
			(1, 2, 3, 4, 5, 6)
		);
	}

	#[test]
	fn a_server_without_lock_announcements_reads_as_lock_counter_zero() {
		let response: Response =
			serde_json::from_str(r#"{"general":1,"chat":2,"contact":3,"note":4,"drive":5}"#)
				.unwrap();
		assert_eq!(response.lock, 0);
	}
}
