use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/gdpr";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub user: User<'a>,
	pub events: Events<'a>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct User<'a> {
	pub email: Cow<'a, str>,
	#[serde(rename = "lastActiveUnixTimestamp", with = "chrono::serde::ts_seconds")]
	pub last_active: DateTime<Utc>,
	#[serde(
		rename = "lastActiveChatUnixTimestamp",
		with = "chrono::serde::ts_seconds"
	)]
	pub last_active_chat: DateTime<Utc>,
	#[serde(rename = "lastIPAddress")]
	pub last_ip_address: Cow<'a, str>,
	pub nick_name: Option<Cow<'a, str>>,
	pub personal: Personal<'a>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Personal<'a> {
	pub first_name: Option<Cow<'a, str>>,
	pub last_name: Option<Cow<'a, str>>,
	pub company_name: Option<Cow<'a, str>>,
	pub vat_id: Option<Cow<'a, str>>,
	pub street: Option<Cow<'a, str>>,
	pub street_number: Option<Cow<'a, str>>,
	pub city: Option<Cow<'a, str>>,
	pub postal_code: Option<Cow<'a, str>>,
	pub country: Option<Cow<'a, str>>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Events<'a> {
	pub ip_addresses: Vec<Cow<'a, str>>,
	pub user_agents: Vec<Cow<'a, str>>,
}
