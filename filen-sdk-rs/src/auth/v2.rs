use std::borrow::Cow;

use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{
	api,
	crypto::{
		self,
		shared::{CreateRandom, MetaCrypter},
	},
	error::Error,
};
use sha1::Digest;

use super::http::UnauthClient;

#[derive(Clone)]
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
	let (master_key, pwd) = crypto::v2::derive_password_and_mk(pwd, info.salt.as_ref())?;

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
		super::AuthInfo::V2(AuthInfo { master_keys }),
		response.private_key.into_owned(),
		response.public_key.into_owned(),
	))
}

pub(super) fn hash_name(name: impl AsRef<[u8]>) -> String {
	let mut outer_hasher = sha1::Sha1::new();
	let mut inner_hasher = sha2::Sha512::new();
	inner_hasher.update(name.as_ref());
	let mut hashed_name = [0u8; 128];
	// SAFETY: The length of hashed_named must be 2x the length of a Sha512 hash, which is 128 bytes
	faster_hex::hex_encode(inner_hasher.finalize().as_slice(), &mut hashed_name).unwrap();
	outer_hasher.update(hashed_name);
	faster_hex::hex_string(outer_hasher.finalize().as_slice())
}

pub(super) fn generate_file_key() -> crypto::v2::FileKey {
	crypto::v2::FileKey::generate()
}
