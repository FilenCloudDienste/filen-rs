use std::borrow::Cow;

use rsa::RsaPublicKey;
use serde::{Deserialize, Serialize};

use crate::{
	auth::{APIKey, AuthVersion},
	crypto::{DerivedPassword, EncryptedDEK, EncryptedMasterKeys, rsa::EncryptedPrivateKey},
};

pub const ENDPOINT: &str = "v3/login";

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: Cow<'a, str>,
	pub password: DerivedPassword<'a>,
	pub two_factor_code: Cow<'a, str>,
	pub auth_version: AuthVersion,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub api_key: APIKey<'a>,
	pub master_keys: Option<EncryptedMasterKeys<'a>>,
	#[serde(with = "crate::serde::rsa::public_key_der")]
	pub public_key: RsaPublicKey,
	pub private_key: EncryptedPrivateKey<'a>,
	pub dek: Option<EncryptedDEK<'a>>,
}
