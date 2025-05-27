use std::{borrow::Cow, str::FromStr};

use filen_types::crypto::{
	EncryptedString,
	rsa::{EncodedPublicKey, EncryptedPrivateKey},
};

use crate::{
	api,
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
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AuthInfo {
	pub(crate) dek: MetaKey,
}

impl MetaCrypter for AuthInfo {
	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut EncryptedString,
	) -> Result<(), ConversionError> {
		self.dek.encrypt_meta_into(meta, out)
	}

	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: &mut String,
	) -> Result<(), ConversionError> {
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
