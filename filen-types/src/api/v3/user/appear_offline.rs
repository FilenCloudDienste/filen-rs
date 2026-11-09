use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/appearOffline";

/// Whether the user appears offline to their chat contacts. The backend reads a JSON boolean
/// here, not the `0`/`1` other user toggles take.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request {
	/// `true` to appear offline, `false` to appear as usual.
	pub appear_offline: bool,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn appear_offline_is_sent_as_a_json_boolean() {
		assert_eq!(
			serde_json::to_string(&Request {
				appear_offline: true
			})
			.unwrap(),
			r#"{"appearOffline":true}"#
		);
		assert_eq!(
			serde_json::to_string(&Request {
				appear_offline: false
			})
			.unwrap(),
			r#"{"appearOffline":false}"#
		);
	}
}
