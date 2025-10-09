use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
	auth::{APIKey, AuthVersion},
	crypto::{DerivedPassword, EncryptedMasterKeys},
};

pub const ENDPOINT: &str = "v3/user/settings/password/change";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub password: DerivedPassword<'a>,
	pub current_password: DerivedPassword<'a>,
	pub auth_version: AuthVersion,
	pub salt: Cow<'a, str>,
	pub master_keys: EncryptedMasterKeys<'a>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	#[serde(rename = "newAPIKey")]
	pub new_api_key: APIKey<'a>,
}
