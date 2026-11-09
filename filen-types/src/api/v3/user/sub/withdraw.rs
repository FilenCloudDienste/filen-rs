use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::fs::Uuid;

pub const ENDPOINT: &str = "v3/user/sub/withdraw";

/// The language of the withdrawal confirmation the backend mails.
#[derive(Deserialize, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(from_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum WithdrawalLanguage {
	/// German.
	De,
	/// English.
	En,
}

/// Withdraws from a subscription's contract.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	/// The subscription's id, `UserAccountSubs::id`.
	pub uuid: Uuid,
	/// The name the withdrawal is made in, 1 to 255 characters.
	pub name: Cow<'a, str>,
	/// Where the confirmation is mailed.
	pub email: Cow<'a, str>,
	/// The language of the mailed confirmation.
	pub lang: WithdrawalLanguage,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	/// When the withdrawal was received. A repeated request answers with the first one's time.
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub withdraw_requested: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_request_writes_lang_in_lowercase() {
		for (lang, wire) in [
			(WithdrawalLanguage::De, "de"),
			(WithdrawalLanguage::En, "en"),
		] {
			let request = Request {
				uuid: Uuid::from_u128(1),
				name: Cow::Borrowed("Jane Doe"),
				email: Cow::Borrowed("jane@example.com"),
				lang,
			};
			assert_eq!(
				serde_json::to_string(&request).unwrap(),
				format!(
					r#"{{"uuid":"00000000-0000-0000-0000-000000000001","name":"Jane Doe","email":"jane@example.com","lang":"{wire}"}}"#
				)
			);
		}
	}

	#[test]
	fn the_response_reads_withdraw_requested_in_seconds() {
		let response: Response =
			serde_json::from_str(r#"{"withdrawRequested":1700000000}"#).unwrap();
		assert_eq!(
			response.withdraw_requested.timestamp_millis(),
			1_700_000_000_000
		);
	}
}
