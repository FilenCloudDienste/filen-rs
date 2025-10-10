use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
	auth::AuthVersion,
	crypto::{DerivedPassword, EncryptedMasterKeys},
};

pub const ENDPOINT: &str = "v3/user/password/forgot/reset";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub token: Cow<'a, str>,
	pub password: DerivedPassword<'a>,
	pub auth_version: AuthVersion,
	pub salt: Cow<'a, str>,
	pub has_recovery_keys: bool,
	pub new_master_keys: EncryptedMasterKeys<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "newAPIKey")]
	pub new_api_key: Cow<'a, str>,
	#[serde(rename = "oldAPIKey")]
	pub old_api_key: Cow<'a, str>,
}
