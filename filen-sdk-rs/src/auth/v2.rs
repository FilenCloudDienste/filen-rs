use std::{
	borrow::Cow,
	sync::{Arc, RwLock},
};

use filen_types::crypto::{EncryptedString, rsa::EncryptedPrivateKey};
use rsa::RsaPublicKey;

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

pub(crate) fn hash_name(name: &str) -> String {
	faster_hex::hex_string(&hash(name.to_lowercase().as_bytes()))
}

pub(super) fn generate_file_key() -> crypto::v2::FileKey {
	crypto::v2::FileKey::generate()
}
