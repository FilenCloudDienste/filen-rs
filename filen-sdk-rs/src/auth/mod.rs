use std::{borrow::Cow, str::FromStr};

use filen_types::{
	auth::{AuthVersion, FileEncryptionVersion, MetaEncryptionVersion},
	crypto::EncryptedMetaKey,
};
use http::{AuthClient, UnauthClient};
use rsa::{RsaPrivateKey, RsaPublicKey};

use crate::{
	api,
	consts::{V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION},
	crypto::{
		self,
		error::ConversionError,
		file::FileKey,
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
	},
	error::Error,
	fs::dir::RootDirectory,
};

pub mod http;
pub mod v2;
pub mod v3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetaKey {
	V2(v2::MetaKey),
	V3(v3::MetaKey),
}

impl MetaCrypter for MetaKey {
	fn decrypt_meta_into(
		&self,
		meta: &filen_types::crypto::EncryptedString,
		out: &mut String,
	) -> Result<(), crypto::error::ConversionError> {
		match self {
			MetaKey::V2(info) => info.decrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.decrypt_meta_into(meta, out),
		}
	}

	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crate::crypto::error::ConversionError> {
		match self {
			MetaKey::V2(info) => info.encrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.encrypt_meta_into(meta, out),
		}
	}
}

#[derive(Clone)]
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
	) -> Result<(), crypto::error::ConversionError> {
		match self {
			AuthInfo::V1 => unimplemented!(),
			AuthInfo::V2(info) => info.decrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.decrypt_meta_into(meta, out),
		}
	}

	fn encrypt_meta_into(
		&self,
		meta: impl AsRef<str>,
		out: &mut filen_types::crypto::EncryptedString,
	) -> Result<(), crate::crypto::error::ConversionError> {
		match self {
			AuthInfo::V1 => unimplemented!(),
			AuthInfo::V2(info) => info.encrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.encrypt_meta_into(meta, out),
		}
	}
}

#[derive(Clone)]
pub struct Client {
	email: String,

	root_dir: RootDirectory,

	auth_info: AuthInfo,
	file_encryption_version: FileEncryptionVersion,
	meta_encryption_version: MetaEncryptionVersion,

	public_key: RsaPublicKey,
	private_key: RsaPrivateKey,
	pub(crate) hmac_key: HMACKey,

	http_client: AuthClient,
}

impl Client {
	pub fn client(&self) -> &AuthClient {
		&self.http_client
	}

	pub fn crypter(&self) -> &impl MetaCrypter {
		&self.auth_info
	}

	pub fn private_key(&self) -> &RsaPrivateKey {
		&self.private_key
	}

	pub fn public_key(&self) -> &RsaPublicKey {
		&self.public_key
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

	pub fn email(&self) -> &str {
		&self.email
	}

	pub fn file_encryption_version(&self) -> FileEncryptionVersion {
		self.file_encryption_version
	}

	pub fn meta_encryption_version(&self) -> MetaEncryptionVersion {
		self.meta_encryption_version
	}

	pub fn make_file_key(&self) -> FileKey {
		match self.auth_info {
			AuthInfo::V1 | AuthInfo::V2(_) => FileKey::V2(v2::generate_file_key()),
			AuthInfo::V3(_) => FileKey::V3(v3::generate_file_key()),
		}
	}

	pub(crate) fn make_meta_key(&self) -> MetaKey {
		match self.auth_info {
			AuthInfo::V1 | AuthInfo::V2(_) => MetaKey::V2(v2::MetaKey::generate()),
			AuthInfo::V3(_) => MetaKey::V3(v3::MetaKey::generate()),
		}
	}

	pub(crate) fn get_meta_key_from_str(
		&self,
		decrypted_key_str: &str,
	) -> Result<MetaKey, ConversionError> {
		let mut meta_version = self.meta_encryption_version();
		if meta_version == MetaEncryptionVersion::V3
			&& (!faster_hex::hex_check(decrypted_key_str.as_bytes())
				|| decrypted_key_str.len() != 64)
		{
			meta_version = MetaEncryptionVersion::V2;
		}

		match meta_version {
			MetaEncryptionVersion::V2 => Ok(MetaKey::V2(v2::MetaKey::from_str(decrypted_key_str)?)),
			MetaEncryptionVersion::V3 => Ok(MetaKey::V3(v3::MetaKey::from_str(decrypted_key_str)?)),
			_ => unimplemented!(),
		}
	}

	pub(crate) fn decrypt_meta_key(
		&self,
		key_str: &EncryptedMetaKey,
	) -> Result<MetaKey, ConversionError> {
		let decrypted_str = self.crypter().decrypt_meta(&key_str.0)?;
		self.get_meta_key_from_str(&decrypted_str)
	}

	pub(crate) fn encrypt_meta_key(
		&self,
		key: &MetaKey,
	) -> Result<EncryptedMetaKey, ConversionError> {
		Ok(EncryptedMetaKey(match key {
			MetaKey::V2(key) => self.crypter().encrypt_meta(key.as_ref()),
			MetaKey::V3(key) => self.crypter().encrypt_meta(Into::<String>::into(key)),
		}?))
	}

	pub async fn login(email: String, pwd: &str, two_factor_code: &str) -> Result<Self, Error> {
		let client = UnauthClient::default();

		let info_response = api::v3::auth::info::post(
			&client,
			&api::v3::auth::info::Request {
				email: Cow::Borrowed(&email),
			},
		)
		.await?;

		let (client, auth_info, private_key, public_key) = match info_response.auth_version {
			AuthVersion::V1 => unimplemented!(),
			AuthVersion::V2 => {
				v2::login(&email, pwd, two_factor_code, &info_response, client).await?
			}
			AuthVersion::V3 => {
				v3::login(&email, pwd, two_factor_code, &info_response, client).await?
			}
		};

		let (private_key, public_key, hmac) =
			crypto::rsa::get_key_pair(&public_key, &private_key, &auth_info)?;

		let base_folder_uuid = api::v3::user::base_folder::get(&client).await?.uuid;
		let root_dir = RootDirectory::new(base_folder_uuid);

		let (file_encryption_version, meta_encryption_version) = match &info_response.auth_version {
			AuthVersion::V1 | AuthVersion::V2 => {
				(V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION)
			}
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
