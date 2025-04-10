use filen_types::auth::AuthVersion;
use http::{AuthClient, UnauthClient};
use rsa::{RsaPrivateKey, RsaPublicKey};

use crate::{
	api,
	consts::{V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION},
	crypto::{self, rsa::HMACKey, shared::MetaCrypter},
	error::Error,
	fs::dir::RootDirectory,
};

pub mod http;
pub mod v2;
pub mod v3;

#[allow(clippy::large_enum_variant)]
pub(crate) enum AuthInfo {
	V1,
	V2(v2::AuthInfo),
	V3(v3::AuthInfo),
}

impl MetaCrypter for AuthInfo {
	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString,
		out: &mut String,
	) -> Result<(), crate::crypto::error::ConversionError> {
		match self {
			AuthInfo::V1 => unimplemented!(),
			AuthInfo::V2(info) => info.decrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.decrypt_meta_into(meta, out),
		}
	}

	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crate::crypto::error::ConversionError> {
		match self {
			AuthInfo::V1 => unimplemented!(),
			AuthInfo::V2(info) => info.encrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.encrypt_meta_into(meta, out),
		}
	}
}

pub enum FileEncryptionVersion {
	V1,
	V2,
	V3,
}

pub enum MetaEncryptionVersion {
	V1,
	V2,
	V3,
}

pub struct Client {
	email: String,

	root_dir: RootDirectory,

	auth_info: AuthInfo,
	file_encryption_version: FileEncryptionVersion,
	meta_encryption_version: MetaEncryptionVersion,

	public_key: RsaPublicKey,
	private_key: RsaPrivateKey,
	hmac_key: HMACKey,

	http_client: AuthClient,
}

pub async fn login(email: String, pwd: &str, two_factor_code: &str) -> Result<Client, Error> {
	let client = UnauthClient::default();

	let info_response =
		api::v3::auth::info::post(&client, api::v3::auth::info::Request { email: &email }).await?;

	println!("info_response: {:?}", info_response);

	let (client, auth_info, private_key, public_key) = match info_response.auth_version {
		AuthVersion::V1 => unimplemented!(),
		AuthVersion::V2 => v2::login(&email, pwd, two_factor_code, &info_response, client).await?,
		AuthVersion::V3 => v3::login(&email, pwd, two_factor_code, &info_response, client).await?,
	};

	let (private_key, public_key, hmac) =
		crypto::rsa::get_key_pair(&public_key, &private_key, &auth_info)?;

	let base_folder_uuid = api::v3::user::base_folder::get(&client).await?.uuid;
	let root_dir = RootDirectory::new(base_folder_uuid);

	let (file_encryption_version, meta_encryption_version) = match &info_response.auth_version {
		AuthVersion::V1 | AuthVersion::V2 => (V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION),
		AuthVersion::V3 => (FileEncryptionVersion::V3, MetaEncryptionVersion::V3),
	};

	Ok(Client {
		email,
		root_dir,
		auth_info,
		file_encryption_version,
		meta_encryption_version,
		public_key,
		private_key,
		hmac_key: hmac,
		http_client: client,
	})
}
