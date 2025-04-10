use crate::{
	auth::{APIKey, AuthVersion},
	crypto::{
		DerivedPassword, EncryptedDEK, EncryptedMasterKeys,
		rsa::{EncodedPublicKey, EncryptedPrivateKey},
	},
};

use reqwest::Body;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Request<'a> {
	pub email: &'a str,
	pub password: DerivedPassword,
	pub two_factor_code: &'a str,
	pub auth_version: AuthVersion,
}

impl From<Request<'_>> for Body {
	fn from(val: Request) -> Self {
		serde_json::to_string(&val).unwrap().into()
	}
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response {
	pub api_key: APIKey,
	pub master_keys: Option<EncryptedMasterKeys>,
	pub public_key: EncodedPublicKey,
	pub private_key: EncryptedPrivateKey,
	pub dek: Option<EncryptedDEK>,
}
