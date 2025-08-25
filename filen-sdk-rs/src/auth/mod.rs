use std::{
	borrow::Cow,
	fmt::{Debug, Display},
	str::FromStr,
	sync::{Arc, Weak},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use digest::Digest;
use filen_types::{
	auth::{APIKey, AuthVersion, FileEncryptionVersion, FilenSDKConfig, MetaEncryptionVersion},
	crypto::{EncryptedMetaKey, EncryptedString},
	fs::UuidStr,
};
use http::{AuthClient, UnauthClient};
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey};
use rsa::{pkcs1::EncodeRsaPublicKey, pkcs8::EncodePrivateKey};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
	api,
	consts::{V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION},
	crypto::{
		self,
		error::ConversionError,
		file::FileKey,
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
		v2::MasterKeys,
	},
	error::Error,
	fs::{HasUUID, dir::RootDirectory},
	sync::lock::ResourceLock,
};

pub mod http;
pub mod v1;
pub mod v2;
pub mod v3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetaKey {
	V1(v2::MetaKey),
	V2(v2::MetaKey),
	V3(v3::MetaKey),
}

impl MetaCrypter for MetaKey {
	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		match self {
			MetaKey::V1(info) | MetaKey::V2(info) => info.decrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.decrypt_meta_into(meta, out),
		}
	}

	fn encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString {
		match self {
			MetaKey::V1(info) | MetaKey::V2(info) => info.encrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.encrypt_meta_into(meta, out),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthInfo {
	V1(v2::AuthInfo),
	V2(v2::AuthInfo),
	V3(v3::AuthInfo),
}

impl Display for AuthInfo {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => {
				write!(f, "{}", info.master_keys.to_decrypted_string())
			}
			AuthInfo::V3(info) => Display::fmt(&info.dek, f),
		}
	}
}

impl AuthInfo {
	pub fn from_string_and_version(s: &str, version: u32) -> Result<Self, ConversionError> {
		match version {
			1 => Ok(AuthInfo::V1(v2::AuthInfo {
				master_keys: MasterKeys::from_decrypted_string(s)?,
			})),
			2 => Ok(AuthInfo::V2(v2::AuthInfo {
				master_keys: MasterKeys::from_decrypted_string(s)?,
			})),
			3 => Ok(AuthInfo::V3(v3::AuthInfo {
				dek: v3::MetaKey::from_str(s)?,
			})),
			_ => unimplemented!("Unsupported auth version: {}", version),
		}
	}
}

impl MetaCrypter for AuthInfo {
	fn decrypt_meta_into(
		&self,
		meta: &EncryptedString,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => info.decrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.decrypt_meta_into(meta, out),
		}
	}

	fn encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString {
		match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => info.encrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.encrypt_meta_into(meta, out),
		}
	}
}

// #[derive(Clone)]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
#[cfg_attr(feature = "node", napi_derive::napi)]
pub struct Client {
	email: String,

	root_dir: RootDirectory,

	auth_info: AuthInfo,
	file_encryption_version: FileEncryptionVersion,
	meta_encryption_version: MetaEncryptionVersion,

	public_key: RsaPublicKey,
	private_key: RsaPrivateKey,
	pub(crate) hmac_key: HMACKey,

	http_client: Arc<AuthClient>,

	pub(crate) drive_lock: Mutex<Option<Weak<ResourceLock>>>,
	pub(crate) api_semaphore: tokio::sync::Semaphore,
	pub(crate) memory_semaphore: tokio::sync::Semaphore,
	#[cfg_attr(all(target_arch = "wasm32", target_os = "unknown"), wasm_bindgen(skip))]
	#[cfg_attr(feature = "node", napi_derive::napi(skip))]
	pub open_file_semaphore: tokio::sync::Semaphore,
}

impl PartialEq for Client {
	fn eq(&self, other: &Self) -> bool {
		self.email == other.email
			&& self.root_dir == other.root_dir
			&& self.auth_info == other.auth_info
			&& self.file_encryption_version == other.file_encryption_version
			&& self.meta_encryption_version == other.meta_encryption_version
			&& self.public_key == other.public_key
			&& self.private_key == other.private_key
			&& self.hmac_key == other.hmac_key
			&& self.http_client.api_key == other.http_client.api_key
	}
}

impl Eq for Client {}

#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	derive(tsify::Tsify)
)]
#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	tsify(from_wasm_abi, into_wasm_abi)
)]
#[cfg_attr(feature = "node", napi_derive::napi(object))]
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StringifiedClient {
	pub email: String,
	pub root_uuid: String,
	pub auth_info: String,
	pub private_key: String,
	pub api_key: String,
	pub auth_version: u32,
}

impl Client {
	pub fn from_stringified(stringified: StringifiedClient) -> Result<Self, ConversionError> {
		Client::from_strings(
			stringified.email,
			&stringified.root_uuid,
			&stringified.auth_info,
			&stringified.private_key,
			stringified.api_key,
			stringified.auth_version,
		)
	}

	pub fn client(&self) -> &AuthClient {
		&self.http_client
	}

	pub fn arc_client(&self) -> Arc<AuthClient> {
		self.http_client.clone()
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
			AuthInfo::V1(_) | AuthInfo::V2(_) => v2::hash_name(name),
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

	pub fn auth_version(&self) -> AuthVersion {
		match self.auth_info {
			AuthInfo::V1(_) => AuthVersion::V1,
			AuthInfo::V2(_) => AuthVersion::V2,
			AuthInfo::V3(_) => AuthVersion::V3,
		}
	}

	pub fn make_file_key(&self) -> FileKey {
		match self.auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => FileKey::V2(v2::generate_file_key()),
			AuthInfo::V3(_) => FileKey::V3(v3::generate_file_key()),
		}
	}

	pub(crate) fn make_meta_key(&self) -> MetaKey {
		match self.auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => MetaKey::V2(v2::MetaKey::generate()),
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
			MetaEncryptionVersion::V1 | MetaEncryptionVersion::V2 => {
				Ok(MetaKey::V2(v2::MetaKey::from_str(decrypted_key_str)?))
			}
			MetaEncryptionVersion::V3 => Ok(MetaKey::V3(v3::MetaKey::from_str(decrypted_key_str)?)),
		}
	}

	pub(crate) fn decrypt_meta_key(
		&self,
		key_str: &EncryptedMetaKey,
	) -> Result<MetaKey, ConversionError> {
		let decrypted_str = self.crypter().decrypt_meta(&key_str.0)?;
		self.get_meta_key_from_str(&decrypted_str)
	}

	pub(crate) fn encrypt_meta_key(&self, key: &MetaKey) -> EncryptedMetaKey {
		EncryptedMetaKey(match key {
			MetaKey::V1(_) => {
				unimplemented!("V1 encryption is not supported in this version of the SDK")
			}
			MetaKey::V2(key) => self.crypter().encrypt_meta(key.as_ref()),
			MetaKey::V3(key) => self.crypter().encrypt_meta(&key.to_string()),
		})
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
			AuthVersion::V1 => {
				v1::login(&email, pwd, two_factor_code, &info_response, client).await?
			}
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
			http_client: Arc::new(client),
			drive_lock: Mutex::new(None),
			api_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_SMALL_PARALLEL_REQUESTS),
			memory_semaphore: tokio::sync::Semaphore::new(
				crate::consts::MAX_DEFAULT_MEMORY_USAGE_TARGET,
			),
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
		})
	}

	pub fn from_strings(
		email: String,
		root_uuid: &str,
		auth_info: &str,
		private_key: &str,
		api_key: String,
		version: u32,
	) -> Result<Self, ConversionError> {
		let auth_info = AuthInfo::from_string_and_version(auth_info, version)?;
		let file_encryption_version = match auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => V2FILE_ENCRYPTION_VERSION,
			AuthInfo::V3(_) => FileEncryptionVersion::V3,
		};
		let meta_encryption_version = match auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => V2META_ENCRYPTION_VERSION,
			AuthInfo::V3(_) => MetaEncryptionVersion::V3,
		};

		let private_key = RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(private_key)?)?;

		Ok(Client {
			email,
			root_dir: RootDirectory::new(UuidStr::from_str(root_uuid)?),
			auth_info,
			file_encryption_version,
			meta_encryption_version,
			public_key: RsaPublicKey::from(&private_key),
			hmac_key: HMACKey::new(&private_key),
			private_key,
			http_client: Arc::new(AuthClient::new(APIKey(api_key))),
			drive_lock: Mutex::new(None),
			api_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_SMALL_PARALLEL_REQUESTS),
			memory_semaphore: tokio::sync::Semaphore::new(
				crate::consts::MAX_DEFAULT_MEMORY_USAGE_TARGET,
			),
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
		})
	}

	pub fn to_sdk_config(&self) -> FilenSDKConfig {
		FilenSDKConfig {
			email: self.email.clone(),
			password: "".to_string(), // we should not be storing passwords in the client
			two_factor_code: "".to_string(),
			master_keys: match &self.auth_info {
				AuthInfo::V1(info) | AuthInfo::V2(info) => info
					.master_keys
					.to_decrypted_string()
					.split('|')
					.fold(Vec::new(), |mut acc, key| {
						acc.push(key.to_string());
						acc
					}),
				AuthInfo::V3(info) => vec![info.dek.to_string()],
			},
			api_key: self.client().api_key.to_string(),
			private_key: BASE64_STANDARD
				.encode(self.private_key.to_pkcs8_der().unwrap().as_bytes()),
			public_key: BASE64_STANDARD.encode(self.public_key.to_pkcs1_der().unwrap().as_bytes()),
			auth_version: self.auth_version(),
			base_folder_uuid: self.root_dir.uuid().to_string(),
			user_id: 0,
			metadata_cache: false,
			tmp_path: "".to_string(), // ?
			connect_to_socket: false,
		}
	}
}

#[cfg_attr(
	all(target_arch = "wasm32", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
impl Client {
	#[cfg_attr(
		all(target_arch = "wasm32", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(js_name = "toStringified")
	)]
	pub fn to_stringified(&self) -> StringifiedClient {
		StringifiedClient {
			email: self.email.clone(),
			root_uuid: self.root_dir.uuid().to_string(),
			auth_info: self.auth_info.to_string(),
			private_key: BASE64_STANDARD
				.encode(self.private_key.to_pkcs8_der().unwrap().as_bytes()),
			api_key: self.http_client.api_key.0.clone(),
			auth_version: match self.auth_info {
				AuthInfo::V1(_) => 1,
				AuthInfo::V2(_) => 2,
				AuthInfo::V3(_) => 3,
			},
		}
	}
}

impl std::fmt::Debug for Client {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Client")
			.field("email", &self.email)
			.field("root_dir", &self.root_dir)
			.field("auth_info", &self.auth_info)
			.field("file_encryption_version", &self.file_encryption_version)
			.field("meta_encryption_version", &self.meta_encryption_version)
			.field(
				"public_key",
				&faster_hex::hex_string(&sha2::Sha256::digest(
					self.public_key.to_pkcs1_der().unwrap(),
				)),
			)
			.field(
				"private_key",
				&faster_hex::hex_string(&sha2::Sha256::digest(
					self.private_key.to_pkcs8_der().unwrap().as_bytes(),
				)),
			)
			.field("hmac_key", &self.hmac_key)
			.field("http_client", &self.http_client)
			.finish()
	}
}
