use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::{
	auth::{APIKey, AuthVersion},
	crypto::{
		DerivedPassword, EncryptedDEK, EncryptedMasterKeys,
		rsa::{EncodedPublicKey, EncryptedPrivateKey},
	},
};

pub const ENDPOINT: &str = "v3/login";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
	pub password: Cow<'a, DerivedPassword>,
	pub two_factor_code: Cow<'a, str>,
	pub auth_version: AuthVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub api_key: Cow<'a, APIKey>,
	pub master_keys: Option<Cow<'a, EncryptedMasterKeys>>,
	pub public_key: Cow<'a, EncodedPublicKey>,
	pub private_key: Cow<'a, EncryptedPrivateKey>,
	pub dek: Option<Cow<'a, EncryptedDEK>>,
}
