use std::borrow::Cow;

use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{api, auth::http::UnauthClient, crypto, error::Error};

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
		EncryptedPrivateKey,
		EncodedPublicKey,
	),
	Error,
> {
	let (master_key, pwd) = crypto::v1::derive_password_and_mk(pwd.as_bytes())?;

	let response = api::v3::login::post(
		&client,
		&api::v3::login::Request {
			email: Cow::Borrowed(email),
			password: Cow::Borrowed(&pwd),
			two_factor_code: Cow::Borrowed(two_factor_code),
			auth_version: info.auth_version,
		},
	)
	.await?;

	let auth_client = super::AuthClient::new_from_client(response.api_key.into_owned(), client);

	let master_keys_str = response.master_keys.ok_or(Error::Custom(
		"Missing master keys in login response".to_string(),
	))?;

	let master_keys = crypto::v2::MasterKeys::new(master_keys_str.into_owned(), master_key)?;

	Ok((
		auth_client,
		super::AuthInfo::V1(super::v2::AuthInfo { master_keys }),
		response.private_key.into_owned(),
		response.public_key.into_owned(),
	))
}
