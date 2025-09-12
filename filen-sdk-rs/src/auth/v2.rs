use std::borrow::Cow;

use filen_types::crypto::{
	EncryptedString,
	rsa::{EncodedPublicKey, EncryptedPrivateKey},
};

use crate::{
	Error, ErrorKind, api,
	crypto::{
		self,
		error::ConversionError,
		shared::{CreateRandom, MetaCrypter},
		v2::{MasterKeys, hash},
	},
};

use super::http::UnauthClient;

pub(crate) use crate::crypto::v2::MasterKey as MetaKey;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthInfo {
	pub(crate) master_keys: MasterKeys,
}

impl MetaCrypter for AuthInfo {
	fn encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.master_keys.encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		self.master_keys.decrypt_meta_into(meta, out)
	}
}

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
	let (master_key, pwd) = crypto::v2::derive_password_and_mk(pwd, info.salt.as_ref())?;

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
		super::AuthInfo::V2(AuthInfo { master_keys }),
		response.private_key,
		response.public_key,
	))
}

pub(crate) fn hash_name(name: &str) -> String {
	faster_hex::hex_string(&hash(name.to_lowercase().as_bytes()))
}

pub(super) fn generate_file_key() -> crypto::v2::FileKey {
	crypto::v2::FileKey::generate()
}
