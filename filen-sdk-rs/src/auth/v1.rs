use std::{
	borrow::Cow,
	sync::{Arc, RwLock},
};

use filen_types::crypto::rsa::EncryptedPrivateKey;
use rsa::RsaPublicKey;

use crate::{
	ErrorKind, api,
	auth::{http::AuthClient, unauth::UnauthClient},
	crypto,
	error::Error,
};

pub(super) async fn login(
	email: &str,
	pwd: &str,
	two_factor_code: &str,
	info: &api::v3::auth::info::Response<'_>,
	client: &UnauthClient,
) -> Result<
	(
		AuthClient,
		super::AuthInfo,
		EncryptedPrivateKey<'static>,
		RsaPublicKey,
	),
	Error,
> {
	let (master_key, pwd) = crypto::v1::derive_password_and_mk(pwd.as_bytes())?;

	let response = api::v3::login::post(
		client,
		&api::v3::login::Request {
			email: Cow::Borrowed(email),
			password: pwd,
			two_factor_code: Cow::Borrowed(two_factor_code),
			auth_version: info.auth_version,
		},
	)
	.await?;

	let auth_client =
		AuthClient::from_unauthed(client.clone(), Arc::new(RwLock::new(response.api_key)));

	let master_keys_str = response.master_keys.ok_or(Error::custom(
		ErrorKind::Response,
		"Missing master keys in v1 login response",
	))?;

	let master_keys = crypto::v2::MasterKeys::new(master_keys_str, master_key).await?;

	Ok((
		auth_client,
		super::AuthInfo::V1(super::v2::AuthInfo { master_keys }),
		response.private_key.ok_or(Error::custom(
			ErrorKind::Response,
			"Missing private key in v1 login response",
		))?,
		response
			.public_key
			.ok_or(Error::custom(
				ErrorKind::Response,
				"Missing public key in v1 login response",
			))?
			.0
			.into_owned(),
	))
}

/// Recover the v1 [`AuthInfo`] with an existing API key instead of a `/v3/login` call, so no 2FA
/// code is needed.
///
/// The v1 master key derives from the password alone — no salt, no server data — so the same
/// [`super::v2::fetch_master_keys`] call as v2 rebuilds the chain. The blob it posts is in v2
/// metadata format, since this SDK cannot encrypt v1 metadata at all; that matches how it already
/// treats v1 accounts, whose item metadata, name hashes and file bodies it writes with the v2
/// scheme. Reading is unaffected either way — the v2 decrypter falls back to the v1 layout on the
/// `U2FsdGVk` marker.
pub(super) async fn auth_info_with_api_key(
	pwd: &str,
	auth_client: &AuthClient,
) -> Result<super::AuthInfo, Error> {
	let (master_key, _pwd) = crypto::v1::derive_password_and_mk(pwd.as_bytes())?;
	Ok(super::AuthInfo::V1(super::v2::AuthInfo {
		master_keys: super::v2::fetch_master_keys(master_key, auth_client).await?,
	}))
}
