use std::{
	borrow::Cow,
	sync::{Arc, RwLock},
};

use filen_types::{
	crypto::{EncryptedString, rsa::EncryptedPrivateKey},
	serde::str::SizedHexString,
};
use rsa::RsaPublicKey;
use typenum::U20;

use crate::{
	Error, api,
	auth::{http::AuthClient, unauth::UnauthClient},
	crypto::{
		self,
		error::ConversionError,
		shared::{CreateRandom, MetaCrypter},
		v2::{MasterKeys, hash},
	},
};

pub(crate) use crate::crypto::v2::MasterKey as MetaKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthInfo {
	pub(crate) master_keys: MasterKeys,
}

impl MetaCrypter for AuthInfo {
	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.master_keys.blocking_encrypt_meta_into(meta, out)
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		self.master_keys.blocking_decrypt_meta_into(meta, out)
	}
}

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
		Option<EncryptedPrivateKey<'static>>,
		Option<RsaPublicKey>,
	),
	Error,
> {
	let (master_key, pwd) =
		crypto::v2::derive_password_and_mk(pwd.as_bytes(), info.salt.as_bytes())?;

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

	let master_keys = if let Some(master_keys_str) = response.master_keys {
		crypto::v2::MasterKeys::new(master_keys_str, master_key).await?
		// no master key set, set one up
	} else {
		let master_keys = MasterKeys::new_from_key(master_key);
		let encrypted = master_keys.to_encrypted().await;
		api::v3::user::master_keys::post(
			&auth_client,
			&api::v3::user::master_keys::Request {
				master_keys: encrypted,
			},
		)
		.await?;
		master_keys
	};

	Ok((
		auth_client,
		super::AuthInfo::V2(AuthInfo { master_keys }),
		response.private_key,
		response.public_key.map(|k| k.0.into_owned()),
	))
}

/// Recover the v2 [`AuthInfo`] with an existing API key instead of a `/v3/login` call (so no
/// 2FA code is needed): `/v3/user/masterKeys` is an EXCHANGE — posting the password-derived key
/// (encrypted with itself) returns the account's full master-key chain.
pub(super) async fn auth_info_with_api_key(
	pwd: &str,
	info: &api::v3::auth::info::Response<'_>,
	auth_client: &super::http::AuthClient,
) -> Result<super::AuthInfo, Error> {
	let (master_key, _pwd) =
		crypto::v2::derive_password_and_mk(pwd.as_bytes(), info.salt.as_bytes())?;
	let encrypted = MasterKeys::new_from_key(master_key.clone())
		.to_encrypted()
		.await;
	let response = api::v3::user::master_keys::post(
		auth_client,
		&api::v3::user::master_keys::Request {
			master_keys: encrypted,
		},
	)
	.await?;
	let master_keys = crypto::v2::MasterKeys::new(response.keys, master_key).await?;
	Ok(super::AuthInfo::V2(AuthInfo { master_keys }))
}

pub(crate) fn hash_name(name: &str) -> SizedHexString<U20> {
	hash(name.to_lowercase().as_bytes()).into()
}

pub(super) fn generate_file_key() -> crypto::v2::FileKey {
	crypto::v2::FileKey::generate()
}
