use std::{
	borrow::Cow,
	str::FromStr,
	sync::{Arc, RwLock},
};

use filen_types::crypto::{EncryptedString, rsa::EncryptedPrivateKey};
use rsa::RsaPublicKey;

use crate::{
	ErrorKind, api,
	auth::{http::AuthClient, unauth::UnauthClient},
	crypto::{
		error::ConversionError,
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
		v3::EncryptionKey,
	},
	error::Error,
};

pub(crate) use crate::crypto::v3::EncryptionKey as MetaKey;
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthInfo {
	pub(crate) dek: MetaKey,
}

impl MetaCrypter for AuthInfo {
	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		self.dek.blocking_encrypt_meta_into(meta, out)
	}

	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		self.dek.blocking_decrypt_meta_into(meta, out)
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
	let (kek, pwd) =
		crate::crypto::v3::derive_password_and_kek(pwd.as_bytes(), info.salt.as_bytes())?;

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

	let dek_str = response.dek.ok_or(Error::custom(
		ErrorKind::Response,
		"Missing dek in login response",
	))?;

	let dek_str = kek.decrypt_meta(&dek_str.0).await?;
	let dek = EncryptionKey::from_str(&dek_str)?;

	Ok((
		auth_client,
		super::AuthInfo::V3(AuthInfo { dek }),
		response.private_key,
		response.public_key.map(|k| k.0.into_owned()),
	))
}

pub(super) fn hash_name(name: &str, hmac_key: &HMACKey) -> String {
	hmac_key.hash_to_string(name.to_lowercase().as_bytes())
}

pub(super) fn generate_file_key() -> EncryptionKey {
	EncryptionKey::generate()
}
