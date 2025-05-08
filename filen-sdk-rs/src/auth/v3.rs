use std::{borrow::Cow, str::FromStr};

use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{
	api,
	crypto::{
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
		v3::EncryptionKey,
	},
	error::Error,
};

use super::http::UnauthClient;

#[derive(Clone)]
pub(crate) struct AuthInfo {
	dek: crate::crypto::v3::EncryptionKey,
}

impl MetaCrypter for AuthInfo {
	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crate::crypto::error::ConversionError> {
		self.dek.encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString,
		out: &mut String,
	) -> Result<(), crate::crypto::error::ConversionError> {
		self.dek.decrypt_meta_into(meta, out)
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
	let (kek, pwd) = crate::crypto::v3::derive_password_and_kek(pwd, info.salt.as_ref())?;

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

	let dek_str = response
		.dek
		.ok_or(Error::Custom("Missing dek in login response".to_string()))?;

	let dek_str = kek.decrypt_meta(&dek_str.0)?;
	let dek = EncryptionKey::from_str(&dek_str)?;

	Ok((
		auth_client,
		super::AuthInfo::V3(AuthInfo { dek }),
		response.private_key.into_owned(),
		response.public_key.into_owned(),
	))
}

pub(super) fn hash_name(name: impl AsRef<[u8]>, hmac_key: &HMACKey) -> String {
	hmac_key.hash_to_string(name.as_ref())
}

pub(super) fn generate_file_key() -> EncryptionKey {
	EncryptionKey::generate()
}
