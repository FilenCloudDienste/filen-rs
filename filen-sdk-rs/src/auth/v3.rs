use std::str::FromStr;

use filen_types::crypto::rsa::{EncodedPublicKey, EncryptedPrivateKey};

use crate::{api, crypto::shared::MetaCrypter, error::Error};

use super::http::UnauthClient;

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
	let (kek, pwd) = crate::crypto::v3::derive_password_and_kek(pwd, &info.salt)?;

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

	let dek_str = response
		.dek
		.ok_or(Error::Custom("Missing dek in login response".to_string()))?;

	let dek_str = kek.decrypt_meta(&dek_str.0)?;
	let dek = crate::crypto::v3::EncryptionKey::from_str(&dek_str)?;

	Ok((
		auth_client,
		super::AuthInfo::V3(AuthInfo { dek }),
		response.private_key,
		response.public_key,
	))
}
