use std::borrow::Cow;

use serde::{Deserialize, Serialize};

pub const ENDPOINT: &str = "v3/user/settings";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub email: Cow<'a, str>,
	pub storage_used: u64,
	#[serde(with = "crate::serde::boolean::number")]
	pub two_factor_enabled: bool,
	pub two_factor_key: Cow<'a, str>,
	pub unfinished_files: u64,
	pub unfinished_storage: Cow<'a, str>,
	pub versioned_files: u64,
	pub versioned_storage: Cow<'a, str>,
	pub versioning_enabled: bool,
	pub login_alerts_enabled: bool,
}
