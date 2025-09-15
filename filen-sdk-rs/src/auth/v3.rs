use std::{borrow::Cow, str::FromStr};

use filen_types::crypto::{EncryptedString, rsa::EncryptedPrivateKey};
use rsa::RsaPublicKey;

use crate::{
	ErrorKind, api,
	crypto::{
		error::ConversionError,
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
		v3::EncryptionKey,
	},
	error::Error,
};

use super::http::UnauthClient;
pub(crate) use crate::crypto::v3::EncryptionKey as MetaKey;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthInfo {
	pub(crate) dek: MetaKey,
}

impl MetaCrypter for AuthInfo {
	fn encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.dek.encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
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
		EncryptedPrivateKey<'static>,
		RsaPublicKey,
	),
	Error,
> {
	let (kek, pwd) =
		crate::crypto::v3::derive_password_and_kek(pwd.as_bytes(), info.salt.as_bytes())?;

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

	let dek_str = response.dek.ok_or(Error::custom(
		ErrorKind::Response,
		"Missing dek in login response",
	))?;

	let dek_str = kek.decrypt_meta(&dek_str.0)?;
	let dek = EncryptionKey::from_str(&dek_str)?;

	Ok((
		auth_client,
		super::AuthInfo::V3(AuthInfo { dek }),
		response.private_key,
		response.public_key,
	))
}

pub(super) fn hash_name(name: &str, hmac_key: &HMACKey) -> String {
	hmac_key.hash_to_string(name.to_lowercase().as_bytes())
}

pub(super) fn generate_file_key() -> EncryptionKey {
	EncryptionKey::generate()
}
