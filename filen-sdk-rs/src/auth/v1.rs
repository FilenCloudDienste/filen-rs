use std::borrow::Cow;

use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{ErrorKind, api, auth::http::UnauthClient, crypto, error::Error};

pub(super) async fn login(
	email: &str,
	pwd: &str,
	two_factor_code: &str,
	info: &api::v3::auth::info::Response<'_>,
	client: UnauthClient,
) -> Result<
	(
		super::AuthClient,
		super::AuthInfo,
		EncryptedPrivateKey<'static>,
		EncodedPublicKey<'static>,
	),
	Error,
> {
	let (master_key, pwd) = crypto::v1::derive_password_and_mk(pwd.as_bytes())?;

	let response = api::v3::login::post(
		&client,
		&api::v3::login::Request {
			email: Cow::Borrowed(email),
			password: pwd,
			two_factor_code: Cow::Borrowed(two_factor_code),
			auth_version: info.auth_version,
		},
	)
	.await?;

	let auth_client = super::AuthClient::new_from_client(response.api_key, client);

	let master_keys_str = response.master_keys.ok_or(Error::custom(
		ErrorKind::Response,
		"Missing master keys in login response",
	))?;

	let master_keys = crypto::v2::MasterKeys::new(master_keys_str, master_key)?;

	Ok((
		auth_client,
		super::AuthInfo::V1(super::v2::AuthInfo { master_keys }),
		response.private_key,
		response.public_key,
	))
}
