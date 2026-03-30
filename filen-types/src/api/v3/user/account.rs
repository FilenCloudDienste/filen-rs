use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/account";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub aff_balance: f64,
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
	pub max_storage: u64,
	pub personal: Personal,
	pub plans: Vec<UserAccountPlan>,
	pub ref_id: String,
	pub ref_limit: u64,
	pub ref_storage: u64,
	pub refer_count: u64,
	pub refer_storage: u64,
	pub storage: u64,
	pub nick_name: String,
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
	#[serde(with = "chrono::serde::ts_milliseconds")]
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
	#[serde(with = "chrono::serde::ts_milliseconds")]
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
	pub storage: u64,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub activated: DateTime<Utc>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub cancelled: DateTime<Utc>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub start_timestamp: DateTime<Utc>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	pub cancel_timestamp: DateTime<Utc>,
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
