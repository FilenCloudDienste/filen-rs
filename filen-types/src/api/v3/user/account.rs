use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/account";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub aff_balance: f64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub aff_count: u64,
	pub aff_earnings: f64,
	pub aff_id: String,
	pub aff_rate: f64,
	#[serde(rename = "avatarURL")]
	pub avatar_url: String,
	pub email: String,
	// TODO: Figure out what the invoice type is
	// pub invoices: Vec<()>,
	#[serde(with = "crate::serde::boolean::number")]
	pub is_premium: bool,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub max_storage: u64,
	pub personal: Personal,
	pub plans: Vec<UserAccountPlan>,
	pub ref_id: String,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub ref_limit: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub ref_storage: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub refer_count: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub refer_storage: u64,
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub storage: u64,
	#[serde(with = "crate::serde::option::str_empty_is_none_owned")]
	pub nick_name: Option<String>,
	pub display_name: String,
	pub appear_offline: bool,
	pub subs: Vec<UserAccountSubs>,
	pub subs_invoices: Vec<UserAccountSubsInvoices>,
	pub did_export_master_keys: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UserAccountPlan {
	pub cost: f64,
	/// When the plan ends; read in seconds or millis, written to JS as millis.
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub end_timestamp: DateTime<Utc>,
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub id: u64,
	pub length_type: String,
	pub name: String,
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub storage: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UserAccountSubsInvoices {
	pub gateway: String,
	pub id: String,
	pub plan_cost: f64,
	pub plan_name: String,
	pub sub_id: String,
	/// When the invoice was issued; read in seconds or millis, written to JS as millis.
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	pub timestamp: DateTime<Utc>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct UserAccountSubs {
	pub id: String,
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub plan_id: u64,
	pub plan_name: String,
	pub plan_cost: f64,
	pub gateway: String,
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint")
	)]
	#[serde(with = "crate::serde::number::permissive_u64")]
	pub storage: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub activated: bool,
	#[serde(with = "crate::serde::boolean::number")]
	pub cancelled: bool,
	/// When a withdrawal from the contract was received, `None` if none was. Read in seconds
	/// or millis (0 or null is `None`), written to JS as millis.
	#[serde(default, with = "crate::serde::time::optional")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint", optional)
	)]
	pub withdraw_requested: Option<DateTime<Utc>>,
	/// When the subscription started, `None` while the backend has not recorded it. Read in
	/// seconds or millis (0 or null is `None`), written to JS as millis.
	#[serde(default, with = "crate::serde::time::optional")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint", optional)
	)]
	pub start_timestamp: Option<DateTime<Utc>>,
	/// When the subscription was cancelled, `None` if it was not; it can lie in the future.
	/// Read in seconds or millis (0 or null is `None`), written to JS as millis.
	#[serde(default, with = "crate::serde::time::optional")]
	#[cfg_attr(
		all(
			target_family = "wasm",
			target_os = "unknown",
			not(feature = "service-worker")
		),
		tsify(type = "bigint", optional)
	)]
	pub cancel_timestamp: Option<DateTime<Utc>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
	all(
		target_family = "wasm",
		target_os = "unknown",
		not(feature = "service-worker")
	),
	derive(tsify::Tsify),
	tsify(large_number_types_as_bigints, hashmap_as_object)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct Personal {
	city: Option<String>,
	company_name: Option<String>,
	country: Option<String>,
	first_name: Option<String>,
	last_name: Option<String>,
	postal_code: Option<String>,
	street: Option<String>,
	street_number: Option<String>,
	vat_id: Option<String>,
}

#[cfg(test)]
mod tests {
	use serde_json::{Value, json};

	use super::*;

	/// 2023-11-14T22:13:20Z, in seconds, as the backend stores subscription times.
	const SECONDS: i64 = 1_700_000_000;
	const MILLIS: i64 = SECONDS * 1000;

	fn sub(start: Value, cancel: Value, withdraw: Option<Value>) -> UserAccountSubs {
		let mut sub = json!({
			"id": "00000000-0000-0000-0000-000000000001",
			"planId": 42,
			"planName": "Pro",
			"planCost": 9.99,
			"gateway": "stripe",
			"storage": 1_000_000_000_000_u64,
			"activated": 1,
			"cancelled": 0,
			"startTimestamp": start,
			"cancelTimestamp": cancel,
		});
		if let Some(withdraw) = withdraw {
			sub["withdrawRequested"] = withdraw;
		}
		serde_json::from_value(sub).unwrap()
	}

	#[test]
	fn subscription_times_in_seconds_read_as_their_millis() {
		let sub = sub(json!(SECONDS), json!(SECONDS), Some(json!(SECONDS)));
		assert_eq!(
			sub.start_timestamp.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
		assert_eq!(
			sub.cancel_timestamp.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
		assert_eq!(
			sub.withdraw_requested.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
	}

	#[test]
	fn subscription_times_in_millis_stay_millis() {
		let sub = sub(json!(MILLIS), json!(MILLIS), Some(json!(MILLIS)));
		assert_eq!(
			sub.start_timestamp.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
		assert_eq!(
			sub.cancel_timestamp.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
		assert_eq!(
			sub.withdraw_requested.map(|t| t.timestamp_millis()),
			Some(MILLIS)
		);
	}

	#[test]
	fn a_zero_subscription_time_is_none_not_the_epoch() {
		let sub = sub(json!(0), json!(0), Some(json!(0)));
		assert_eq!(sub.start_timestamp, None);
		assert_eq!(sub.cancel_timestamp, None);
		assert_eq!(sub.withdraw_requested, None);
	}

	#[test]
	fn a_null_subscription_time_is_none() {
		let sub = sub(Value::Null, Value::Null, Some(Value::Null));
		assert_eq!(sub.start_timestamp, None);
		assert_eq!(sub.cancel_timestamp, None);
		assert_eq!(sub.withdraw_requested, None);
	}

	#[test]
	fn a_missing_withdraw_requested_is_none() {
		let sub = sub(json!(SECONDS), json!(0), None);
		assert_eq!(sub.withdraw_requested, None);
	}

	#[test]
	fn subscription_times_are_written_as_millis_and_none_as_null() {
		let sub = sub(json!(SECONDS), json!(0), None);
		let written = serde_json::to_value(&sub).unwrap();
		assert_eq!(written["startTimestamp"], json!(MILLIS));
		assert_eq!(written["cancelTimestamp"], Value::Null);
		assert_eq!(written["withdrawRequested"], Value::Null);
	}

	#[test]
	fn an_invoice_time_in_seconds_reads_as_its_millis() {
		let invoice: UserAccountSubsInvoices = serde_json::from_value(json!({
			"gateway": "stripe",
			"id": "00000000-0000-0000-0000-000000000002",
			"planCost": 9.99,
			"planName": "Pro",
			"subId": "00000000-0000-0000-0000-000000000001",
			"timestamp": SECONDS,
		}))
		.unwrap();
		assert_eq!(invoice.timestamp.timestamp_millis(), MILLIS);
	}

	#[test]
	fn a_plan_end_in_seconds_reads_as_its_millis() {
		let plan: UserAccountPlan = serde_json::from_value(json!({
			"cost": 9.99,
			"endTimestamp": SECONDS,
			"id": 42,
			"lengthType": "monthly",
			"name": "Pro",
			"storage": 1_000_000_000_000_u64,
		}))
		.unwrap();
		assert_eq!(plan.end_timestamp.timestamp_millis(), MILLIS);
	}
}
