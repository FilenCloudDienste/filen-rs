use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{
	api,
	crypto::{self, shared::MetaCrypter},
	error::Error,
};

use super::http::UnauthClient;

pub(crate) struct AuthInfo {
	master_keys: crate::crypto::v2::MasterKeys,
}

impl MetaCrypter for AuthInfo {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crypto::error::ConversionError> {
		self.master_keys.encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString,
		out: &mut String,
	) -> Result<(), crypto::error::ConversionError> {
		self.master_keys.decrypt_meta_into(meta, out)
	}
}

pub(super) async fn login(
	email: &str,
	pwd: &str,
	two_factor_code: &str,
	info: &api::v3::auth::info::Response,
	client: UnauthClient,
) -> Result<
	(
		super::AuthClient,
		super::AuthInfo,
		EncryptedPrivateKey,
		EncodedPublicKey,
	),
	Error,
> {
	let (master_key, pwd) = crypto::v2::derive_password_and_mk(pwd, &info.salt)?;

	let response = api::v3::login::post(
		&client,
		api::v3::login::Request {
			email,
			password: pwd,
			two_factor_code,
			auth_version: info.auth_version,
		},
	)
	.await?;

	let auth_client = super::AuthClient::new_from_client(response.api_key, client);

	let master_keys_str = response.master_keys.ok_or(Error::Custom(
		"Missing master keys in login response".to_string(),
	))?;

	let master_keys = crypto::v2::MasterKeys::new(master_keys_str, master_key)?;

	Ok((
		auth_client,
		super::AuthInfo::V2(AuthInfo { master_keys }),
		response.private_key,
		response.public_key,
	))
}
