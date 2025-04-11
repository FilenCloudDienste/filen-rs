use filen_types::auth::{AuthVersion, FileEncryptionVersion, MetaEncryptionVersion};
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

impl MetaCrypter for &AuthInfo {
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

impl MetaCrypter for AuthInfo {
	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString,
		out: &mut String,
	) -> Result<(), crypto::error::ConversionError> {
		(&self).decrypt_meta_into(meta, out)
	}

	fn encrypt_meta_into(
		&self,
		meta: &str,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crypto::error::ConversionError> {
		(&self).encrypt_meta_into(meta, out)
	}
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

impl Client {
	pub fn client(&self) -> &AuthClient {
		&self.http_client
	}

	pub fn crypter(&self) -> impl MetaCrypter {
		&self.auth_info
	}

	pub fn hash_name(&self, name: &str) -> String {
		match self.auth_info {
			AuthInfo::V1 | AuthInfo::V2(_) => v2::hash_name(name),
			AuthInfo::V3(_) => v3::hash_name(name, &self.hmac_key),
		}
	}

	pub fn root(&self) -> &RootDirectory {
		&self.root_dir
	}
}

impl std::fmt::Debug for Client {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Client")
			.field("email", &self.email)
			.field("root_dir", &self.root_dir)
			.field(
				"auth_info",
				&match self.auth_info {
					AuthInfo::V1 => "V1",
					AuthInfo::V2(_) => "V2",
					AuthInfo::V3(_) => "V3",
				},
			)
			.field("file_encryption_version", &self.file_encryption_version)
			.field("meta_encryption_version", &self.meta_encryption_version)
			.finish()
	}
}

pub async fn login(email: String, pwd: &str, two_factor_code: &str) -> Result<Client, Error> {
	let client = UnauthClient::default();

	let info_response =
		api::v3::auth::info::post(&client, &api::v3::auth::info::Request { email: &email }).await?;

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
