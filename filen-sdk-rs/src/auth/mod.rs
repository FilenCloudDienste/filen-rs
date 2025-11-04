use std::{
	borrow::Cow,
	fmt::{Debug, Display},
	str::FromStr,
	sync::{Arc, Weak},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::{DateTime, Utc};
use digest::{Digest, FixedOutput, KeyInit, Update};
use filen_types::{
	auth::{APIKey, AuthVersion, FileEncryptionVersion, FilenSDKConfig, MetaEncryptionVersion},
	crypto::{EncryptedMetaKey, EncryptedString},
	fs::UuidStr,
	serde::rsa::RsaDerPublicKey,
	traits::CowHelpers,
};
use http::{AuthClient, UnauthClient};
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey};
use rsa::{pkcs1::EncodeRsaPublicKey, pkcs8::EncodePrivateKey};
use serde::{Deserialize, Serialize};

use crate::{
	api,
	auth::http::AuthorizedClient,
	consts::{
		NEW_ACCOUNT_AUTH_VERSION, RSA_KEY_SIZE, V2FILE_ENCRYPTION_VERSION,
		V2META_ENCRYPTION_VERSION,
	},
	crypto::{
		self,
		error::ConversionError,
		file::FileKey,
		rsa::HMACKey,
		shared::{CreateRandom, MetaCrypter},
		v2::{MasterKey, MasterKeys},
		v3::EncryptionKey,
	},
	error::Error,
	fs::{HasUUID, dir::RootDirectory},
	sync::lock::ResourceLock,
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use crate::sockets::{SocketConfig, SocketConnectionState};

pub mod http;
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
pub mod js_impls;
pub mod v1;
pub mod v2;
pub mod v3;

#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
pub use js_impls::JsClient;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub(crate) enum MetaKey {
	V1(v2::MetaKey),
	V2(v2::MetaKey),
	V3(v3::MetaKey),
}

impl MetaCrypter for MetaKey {
	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		match self {
			MetaKey::V1(info) | MetaKey::V2(info) => info.blocking_decrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.blocking_decrypt_meta_into(meta, out),
		}
	}

	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		match self {
			MetaKey::V1(info) | MetaKey::V2(info) => info.blocking_encrypt_meta_into(meta, out),
			MetaKey::V3(info) => info.blocking_encrypt_meta_into(meta, out),
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
	pub fn from_string_and_version(s: &str, version: u8) -> Result<Self, ConversionError> {
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

	pub fn version(&self) -> AuthVersion {
		match self {
			AuthInfo::V1(_) => AuthVersion::V1,
			AuthInfo::V2(_) => AuthVersion::V2,
			AuthInfo::V3(_) => AuthVersion::V3,
		}
	}
}

impl MetaCrypter for AuthInfo {
	fn blocking_decrypt_meta_into(
		&self,
		meta: &EncryptedString<'_>,
		out: Vec<u8>,
	) -> Result<String, (ConversionError, Vec<u8>)> {
		match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => info.blocking_decrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.blocking_decrypt_meta_into(meta, out),
		}
	}

	fn blocking_encrypt_meta_into(&self, meta: &str, out: String) -> EncryptedString<'static> {
		match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => info.blocking_encrypt_meta_into(meta, out),
			AuthInfo::V3(info) => info.blocking_encrypt_meta_into(meta, out),
		}
	}
}

impl AuthInfo {
	pub fn convert_into_exportable(&self, user_id: u64) -> Result<String, Error> {
		let exported_keys_string = match self {
			AuthInfo::V1(info) | AuthInfo::V2(info) => {
				let mut iter = info.master_keys.0.iter().map(|k| {
					format!(
						"_VALID_FILEN_MASTERKEY_{}@{}_VALID_FILEN_MASTERKEY_",
						k.as_ref(),
						user_id
					)
				});
				let first = iter.next().ok_or_else(|| {
					Error::custom(
						crate::ErrorKind::InvalidState,
						"Account has no master keys to export",
					)
				})?;
				iter.fold(first, |acc, x| format!("{}|{}", acc, x))
			}
			AuthInfo::V3(_) => panic!("Exporting V3 accounts is not supported"),
		};
		let encoded = BASE64_STANDARD.encode(exported_keys_string);
		Ok(encoded)
	}
}

fn master_keys_from_exportable(recovery_key: &str, user_id: u64) -> Result<Vec<MasterKey>, Error> {
	let decoded = BASE64_STANDARD.decode(recovery_key).map_err(|_| {
		Error::custom(
			crate::ErrorKind::BadRecoveryKey,
			"Failed to decode recovery key from base64",
		)
	})?;
	let decoded = String::from_utf8(decoded).map_err(|_| {
		Error::custom(
			crate::ErrorKind::BadRecoveryKey,
			"Failed to decode recovery key from UTF-8",
		)
	})?;
	let regex =
		regex::Regex::new(r"_VALID_FILEN_MASTERKEY_([A-Fa-f0-9]{64})@(\d+)_VALID_FILEN_MASTERKEY_")
			.expect("Failed to compile recovery key regex");

	let mut caps = regex.captures_iter(&decoded).peekable();
	if caps.peek().is_none() {
		return Err(Error::custom(
			crate::ErrorKind::BadRecoveryKey,
			"Recovery key did not contain any valid master keys",
		));
	}
	caps.map(|cap| {
		let key = cap
			.get(1)
			.expect("Failed to get master key from recovery key (should be impossible)");

		let cap_user_id = cap
			.get(2)
			.expect("Failed to get user ID from recovery key (should be impossible)")
			.as_str()
			.parse::<u64>()
			.map_err(|_| {
				Error::custom(
					crate::ErrorKind::BadRecoveryKey,
					"Failed to parse user ID from recovery key",
				)
			})?;
		if user_id != cap_user_id {
			return Err(Error::custom(
				crate::ErrorKind::BadRecoveryKey,
				"User ID in recovery key does not match the account's user ID",
			));
		}
		MasterKey::from_str(key.as_str()).map_err(|_| {
			Error::custom(
				crate::ErrorKind::BadRecoveryKey,
				"Failed to parse master key from recovery key",
			)
		})
	})
	.collect::<Result<Vec<MasterKey>, Error>>()
}

pub struct Client {
	email: String,
	pub(crate) user_id: u64,

	root_dir: RootDirectory,

	auth_info: std::sync::RwLock<Arc<AuthInfo>>,
	file_encryption_version: FileEncryptionVersion,
	meta_encryption_version: MetaEncryptionVersion,

	public_key: RsaPublicKey,
	private_key: RsaPrivateKey,
	pub(crate) hmac_key: HMACKey,

	http_client: Arc<AuthClient>,

	pub(crate) drive_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) notes_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) chats_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) auth_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,

	pub(crate) api_semaphore: tokio::sync::Semaphore,
	pub(crate) memory_semaphore: tokio::sync::Semaphore,
	pub open_file_semaphore: tokio::sync::Semaphore,

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	pub(crate) socket_connection: SocketConnectionState,
}

impl PartialEq for Client {
	fn eq(&self, other: &Self) -> bool {
		self.email == other.email
			&& self.root_dir == other.root_dir
			&& *self.auth_info.read().unwrap_or_else(|e| e.into_inner())
				== *other.auth_info.read().unwrap_or_else(|e| e.into_inner())
			&& self.file_encryption_version == other.file_encryption_version
			&& self.meta_encryption_version == other.meta_encryption_version
			&& self.public_key == other.public_key
			&& self.private_key == other.private_key
			&& self.hmac_key == other.hmac_key
			&& *self.get_api_key() == *other.get_api_key()
	}
}

impl Eq for Client {}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(tsify::Tsify),
	tsify(from_wasm_abi, into_wasm_abi, large_number_types_as_bigints)
)]
#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[serde(rename_all = "camelCase")]
pub struct StringifiedClient {
	pub email: String,
	pub user_id: u64,
	pub root_uuid: String,
	pub auth_info: String,
	pub private_key: String,
	pub api_key: String,
	pub auth_version: u8,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "number")
	)]
	#[serde(default)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub max_parallel_requests: Option<u32>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "number")
	)]
	#[serde(default)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	pub max_io_memory_usage: Option<u32>,
}

impl From<FilenSDKConfig> for StringifiedClient {
	fn from(value: FilenSDKConfig) -> Self {
		StringifiedClient {
			email: value.email,
			user_id: value.user_id,
			root_uuid: value.base_folder_uuid,
			auth_info: value.master_keys.join("|"),
			private_key: value.private_key,
			api_key: value.api_key,
			auth_version: value.auth_version as u8,
			max_parallel_requests: None,
			max_io_memory_usage: None,
		}
	}
}

impl Client {
	pub fn from_stringified(stringified: StringifiedClient) -> Result<Self, ConversionError> {
		let auth_info =
			AuthInfo::from_string_and_version(&stringified.auth_info, stringified.auth_version)?;
		let file_encryption_version = match auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => V2FILE_ENCRYPTION_VERSION,
			AuthInfo::V3(_) => FileEncryptionVersion::V3,
		};
		let meta_encryption_version = match auth_info {
			AuthInfo::V1(_) | AuthInfo::V2(_) => V2META_ENCRYPTION_VERSION,
			AuthInfo::V3(_) => MetaEncryptionVersion::V3,
		};

		let private_key =
			RsaPrivateKey::from_pkcs8_der(&BASE64_STANDARD.decode(stringified.private_key)?)?;

		let http_client = Arc::new(AuthClient::new(APIKey(Cow::Owned(stringified.api_key))));

		Ok(Client {
			email: stringified.email,
			user_id: stringified.user_id,
			root_dir: RootDirectory::new(UuidStr::from_str(&stringified.root_uuid)?),
			auth_info: std::sync::RwLock::new(Arc::new(auth_info)),
			file_encryption_version,
			meta_encryption_version,
			public_key: RsaPublicKey::from(&private_key),
			hmac_key: HMACKey::new(&private_key),
			private_key,
			http_client: http_client.clone(),
			drive_lock: tokio::sync::RwLock::new(None),
			notes_lock: tokio::sync::RwLock::new(None),
			chats_lock: tokio::sync::RwLock::new(None),
			auth_lock: tokio::sync::RwLock::new(None),
			api_semaphore: tokio::sync::Semaphore::new(
				stringified
					.max_parallel_requests
					.map(|v| {
						usize::try_from(v).unwrap_or(crate::consts::MAX_SMALL_PARALLEL_REQUESTS)
					})
					.unwrap_or(crate::consts::MAX_SMALL_PARALLEL_REQUESTS),
			),
			memory_semaphore: tokio::sync::Semaphore::new(
				stringified
					.max_io_memory_usage
					.map(|v| {
						usize::try_from(v).unwrap_or(crate::consts::MAX_DEFAULT_MEMORY_USAGE_TARGET)
					})
					.unwrap_or(crate::consts::MAX_DEFAULT_MEMORY_USAGE_TARGET),
			),
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			socket_connection: SocketConnectionState::new(http_client, SocketConfig::default()),
		})
	}

	pub fn client(&self) -> &AuthClient {
		&self.http_client
	}

	pub fn arc_client(&self) -> Arc<AuthClient> {
		self.http_client.clone()
	}

	pub fn crypter(&self) -> Arc<impl MetaCrypter> {
		self.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.clone()
	}

	pub fn private_key(&self) -> &RsaPrivateKey {
		&self.private_key
	}

	pub fn public_key(&self) -> &RsaPublicKey {
		&self.public_key
	}

	pub fn hash_name(&self, name: &str) -> String {
		match self
			.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
		{
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
		match self
			.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
		{
			AuthInfo::V1(_) => AuthVersion::V1,
			AuthInfo::V2(_) => AuthVersion::V2,
			AuthInfo::V3(_) => AuthVersion::V3,
		}
	}

	pub fn make_file_key(&self) -> FileKey {
		match self
			.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
		{
			AuthInfo::V1(_) | AuthInfo::V2(_) => FileKey::V2(v2::generate_file_key()),
			AuthInfo::V3(_) => FileKey::V3(v3::generate_file_key()),
		}
	}

	pub(crate) fn make_meta_key(&self) -> MetaKey {
		match self
			.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
		{
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

	pub(crate) async fn decrypt_meta_key(
		&self,
		key_str: &EncryptedMetaKey<'_>,
	) -> Result<MetaKey, ConversionError> {
		let decrypted_str = self.crypter().decrypt_meta(&key_str.0).await?;
		self.get_meta_key_from_str(&decrypted_str)
	}

	pub(crate) async fn encrypt_meta_key(&self, key: &MetaKey) -> EncryptedMetaKey<'static> {
		EncryptedMetaKey(match key {
			MetaKey::V1(_) => {
				unimplemented!("V1 encryption is not supported in this version of the SDK")
			}
			MetaKey::V2(key) => self.crypter().encrypt_meta(key.as_ref()).await,
			MetaKey::V3(key) => self.crypter().encrypt_meta(&key.to_string()).await,
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
			AuthVersion::V2 | AuthVersion::V3 => {
				let (client, auth_info, private_key, public_key) = match info_response.auth_version
				{
					AuthVersion::V2 => {
						v2::login(&email, pwd, two_factor_code, &info_response, client).await?
					}
					AuthVersion::V3 => {
						v3::login(&email, pwd, two_factor_code, &info_response, client).await?
					}
					_ => unreachable!(),
				};

				match (public_key, private_key) {
					(Some(public_key), Some(private_key)) => {
						(client, auth_info, private_key, public_key)
					}
					_ => {
						let new_private_key =
							rsa::RsaPrivateKey::new(&mut old_rng::thread_rng(), RSA_KEY_SIZE)
								.expect("Failed to generate RSA key pair");

						let new_public_key = new_private_key.to_public_key();
						let encrypted_private_key =
							crypto::rsa::encrypt_private_key(&new_private_key, &auth_info).await?;

						api::v3::user::key_pair::set::post(
							&client,
							&api::v3::user::key_pair::set::Request {
								public_key: RsaDerPublicKey(Cow::Borrowed(&new_public_key)),
								private_key: encrypted_private_key.as_borrowed_cow(),
							},
						)
						.await?;
						(client, auth_info, encrypted_private_key, new_public_key)
					}
				}
			}
		};

		let (private_key, public_key, hmac) =
			crypto::rsa::get_key_pair(public_key, &private_key, &auth_info).await?;
		let base_folder_uuid = api::v3::user::base_folder::get(&client).await?.uuid;
		let root_dir = RootDirectory::new(base_folder_uuid);

		let (file_encryption_version, meta_encryption_version) = match &info_response.auth_version {
			AuthVersion::V1 | AuthVersion::V2 => {
				(V2FILE_ENCRYPTION_VERSION, V2META_ENCRYPTION_VERSION)
			}
			AuthVersion::V3 => (FileEncryptionVersion::V3, MetaEncryptionVersion::V3),
		};

		let user_info = api::v3::user::info::get(&client).await?;

		let http_client = Arc::new(client);

		Ok(Client {
			email,
			user_id: user_info.id,
			root_dir,
			auth_info: std::sync::RwLock::new(Arc::new(auth_info)),
			file_encryption_version,
			meta_encryption_version,
			public_key,
			private_key,
			hmac_key: hmac,
			http_client: http_client.clone(),
			drive_lock: tokio::sync::RwLock::new(None),
			notes_lock: tokio::sync::RwLock::new(None),
			chats_lock: tokio::sync::RwLock::new(None),
			auth_lock: tokio::sync::RwLock::new(None),
			api_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_SMALL_PARALLEL_REQUESTS),
			memory_semaphore: tokio::sync::Semaphore::new(
				crate::consts::MAX_DEFAULT_MEMORY_USAGE_TARGET,
			),
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			socket_connection: SocketConnectionState::new(http_client, SocketConfig::default()),
		})
	}

	pub fn to_sdk_config(&self) -> FilenSDKConfig {
		FilenSDKConfig {
			email: self.email.clone(),
			password: "".to_string(), // we should not be storing passwords in the client
			two_factor_code: "".to_string(),
			master_keys: match self
				.auth_info
				.read()
				.unwrap_or_else(|e| e.into_inner())
				.as_ref()
			{
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
			api_key: self.client().get_api_key().to_string(),
			private_key: BASE64_STANDARD
				.encode(self.private_key.to_pkcs8_der().unwrap().as_bytes()),
			public_key: BASE64_STANDARD.encode(self.public_key.to_pkcs1_der().unwrap().as_bytes()),
			auth_version: self.auth_version(),
			base_folder_uuid: self.root_dir.uuid().to_string(),
			user_id: self.user_id,
			metadata_cache: false,
			tmp_path: "".to_string(), // ?
			connect_to_socket: false,
		}
	}

	pub async fn generate_2fa_secret(&self) -> Result<TwoFASecret, Error> {
		let resp = api::v3::user::settings::get(self.client()).await?;

		Ok(TwoFASecret::new(
			resp.two_factor_key.into_owned(),
			&resp.email,
		))
	}

	/// Enables 2FA for the account. Returns the recovery key which must be stored safely.
	pub async fn enable_2fa(&self, current_2fa_code: &str) -> Result<String, Error> {
		let _lock = self.lock_auth().await?;
		let resp = api::v3::user::two_fa::enable::post(
			self.client(),
			&api::v3::user::two_fa::enable::Request {
				code: Cow::Borrowed(current_2fa_code),
			},
		)
		.await?;

		Ok(resp.recovery_key.into_owned())
	}

	pub async fn disable_2fa(&self, current_2fa_code: &str) -> Result<(), Error> {
		let _lock = self.lock_auth().await?;
		api::v3::user::two_fa::disable::post(
			self.client(),
			&api::v3::user::two_fa::disable::Request {
				code: Cow::Borrowed(current_2fa_code),
			},
		)
		.await?;
		Ok(())
	}

	pub async fn delete_account(&self, two_factor_code: &str) -> Result<(), Error> {
		api::v3::user::delete::post(
			self.client(),
			&api::v3::user::delete::Request {
				two_factor_key: Cow::Borrowed(two_factor_code),
			},
		)
		.await?;
		Ok(())
	}

	pub async fn change_password(
		&self,
		current_password: &str,
		new_password: &str,
	) -> Result<(), Error> {
		let _lock = self.lock_auth().await?;
		let auth_info_resp = api::v3::auth::info::post(
			self.client(),
			&api::v3::auth::info::Request {
				email: Cow::Borrowed(&self.email),
			},
		)
		.await?;

		let auth_info = (**self.auth_info.read().unwrap_or_else(|e| e.into_inner())).clone();
		let mut master_keys = match auth_info {
			AuthInfo::V1(info) => info.master_keys,
			AuthInfo::V2(info) => info.master_keys,
			AuthInfo::V3(_) => {
				return Err(Error::custom(
					crate::ErrorKind::InvalidState,
					"Changing password is not supported for V3 accounts",
				));
			}
		};
		let new_salt: [u8; 256] = rand::random();
		let new_salt = faster_hex::hex_string(&new_salt);

		let current_derived = match auth_info_resp.auth_version {
			AuthVersion::V1 => crypto::v1::derive_password_and_mk(current_password.as_bytes())?.1,
			AuthVersion::V2 => {
				crypto::v2::derive_password_and_mk(
					current_password.as_bytes(),
					auth_info_resp.salt.as_bytes(),
				)?
				.1
			}
			AuthVersion::V3 => unreachable!("we checked for v3 above"),
		};

		let (new_master_key, new_derive) =
			crypto::v2::derive_password_and_mk(new_password.as_bytes(), new_salt.as_bytes())?;

		master_keys.0.insert(0, new_master_key);
		let private_key_encrypted =
			crypto::rsa::encrypt_private_key(&self.private_key, &master_keys).await?;

		let encrypted = master_keys.to_encrypted().await;

		let resp = api::v3::user::settings::password::change::post(
			self.client(),
			&api::v3::user::settings::password::change::Request {
				current_password: current_derived,
				password: new_derive,
				auth_version: AuthVersion::V2,
				salt: Cow::Borrowed(&new_salt),
				master_keys: encrypted,
			},
		)
		.await?;

		self.update_api_key(resp.new_api_key);

		api::v3::user::key_pair::update::post(
			self.client(),
			&api::v3::user::key_pair::update::Request {
				public_key: RsaDerPublicKey(Cow::Borrowed(&self.public_key)),
				private_key: private_key_encrypted,
			},
		)
		.await?;

		let mut write_lock = self.auth_info.write().unwrap_or_else(|e| e.into_inner());
		*write_lock = Arc::new(AuthInfo::V2(v2::AuthInfo { master_keys }));
		self.auth_info.clear_poison();
		std::mem::drop(write_lock);

		Ok(())
	}

	pub async fn export_master_keys(&self) -> Result<String, Error> {
		let exportable = self
			.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.as_ref()
			.convert_into_exportable(self.user_id)?;
		api::v3::user::did_export_master_keys::post(self.client())
			.await
			.map(|_| exportable)
	}

	pub async fn complete_password_reset(
		token: &str,
		email: String,
		new_password: &str,
		recovery_key: Option<&str>,
	) -> Result<Self, Error> {
		let client = UnauthClient::default();

		let auth_info_resp = api::v3::auth::info::post(
			&client,
			&api::v3::auth::info::Request {
				email: Cow::Borrowed(&email),
			},
		)
		.await?;

		let salt: [u8; 256] = rand::random();
		let salt = faster_hex::hex_string(&salt);
		let (mk, password) =
			crypto::v2::derive_password_and_mk(new_password.as_bytes(), salt.as_bytes())?;

		let mut master_keys = MasterKeys::new_from_key(mk);

		if let Some(recovery_key) = recovery_key {
			let old_keys_vec = master_keys_from_exportable(recovery_key, auth_info_resp.user_id)?;
			master_keys.0.extend(old_keys_vec.into_iter());
		}

		let encrypted = master_keys.to_encrypted().await;

		api::v3::user::password::forgot::reset::post(
			&client,
			&api::v3::user::password::forgot::reset::Request {
				token: Cow::Borrowed(token),
				password,
				auth_version: AuthVersion::V2,
				salt: Cow::Borrowed(&salt),
				has_recovery_keys: recovery_key.is_some(),
				new_master_keys: encrypted,
			},
		)
		.await?;

		// I could try and log in here without using a login call
		// but it's annoying with the state management
		// we can do it properly with v4

		Client::login(email, new_password, "XXXXXX").await
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(Serialize, tsify::Tsify),
	tsify(into_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
pub struct TwoFASecret {
	secret: String,
	url: String,
}

impl TwoFASecret {
	pub fn new(secret: String, email: &str) -> Self {
		Self {
			url: format!(
				"otpauth://totp/Filen:{}?secret={}&issuer=Filen&digits=6&period=30",
				urlencoding::encode(email),
				secret
			),
			secret,
		}
	}
}

impl TwoFASecret {
	pub fn secret(&self) -> &str {
		&self.secret
	}

	pub fn url(&self) -> &str {
		&self.url
	}

	pub fn make_totp_code(&self, for_time: DateTime<Utc>) -> Result<u32, Error> {
		let decoded_secret =
			base32::decode(base32::Alphabet::Rfc4648 { padding: false }, &self.secret).ok_or_else(
				|| {
					Error::custom(
						crate::ErrorKind::Conversion,
						"Failed to decode 2FA secret from base32",
					)
				},
			)?;

		let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(&decoded_secret).map_err(|_| {
			Error::custom(
				crate::ErrorKind::Conversion,
				format!(
					"Failed to create HMAC instance for TOTP generation (invalid key length: {})",
					decoded_secret.len()
				),
			)
		})?;

		let counter = for_time.timestamp() / 30;
		mac.update(&counter.to_be_bytes());
		let hash = mac.finalize_fixed();
		let offset = (hash[hash.len() - 1] & 0x0f) as usize;
		let code = ((hash[offset] & 0x7f) as u32) << 24
			| (hash[offset + 1] as u32) << 16
			| (hash[offset + 2] as u32) << 8
			| (hash[offset + 3] as u32);

		Ok(code % 1_000_000)
	}
}

impl Client {
	pub fn to_stringified(&self) -> StringifiedClient {
		let auth_info = self.auth_info.read().unwrap_or_else(|e| e.into_inner());
		StringifiedClient {
			email: self.email.clone(),
			user_id: self.user_id,
			root_uuid: self.root_dir.uuid().to_string(),
			auth_info: auth_info.to_string(),
			private_key: BASE64_STANDARD
				.encode(self.private_key.to_pkcs8_der().unwrap().as_bytes()),
			api_key: self.get_api_key().to_string(),
			auth_version: match **auth_info {
				AuthInfo::V1(_) => 1,
				AuthInfo::V2(_) => 2,
				AuthInfo::V3(_) => 3,
			},
			max_parallel_requests: None,
			max_io_memory_usage: None,
		}
	}
}

enum RegisteredAuthInfo {
	V2(MasterKey),
	V3(EncryptionKey), // kek
}

impl RegisteredAuthInfo {
	fn version(&self) -> AuthVersion {
		match self {
			RegisteredAuthInfo::V2(_) => AuthVersion::V2,
			RegisteredAuthInfo::V3(_) => AuthVersion::V3,
		}
	}
}

pub struct RegisteredInfo {
	email: String,
	salt: String,
	auth_info: RegisteredAuthInfo,
	api_key: APIKey<'static>,
}

impl RegisteredInfo {
	pub async fn register(
		email: String,
		password: &str,
		ref_id: Option<&str>,
		aff_id: Option<&str>,
	) -> Result<Self, Error> {
		let client = UnauthClient::default();

		let (derived_pwd, salt, auth_info) = match NEW_ACCOUNT_AUTH_VERSION {
			AuthVersion::V1 => unreachable!("V1 is not supported for new accounts"),
			AuthVersion::V2 => {
				let salt: [u8; 128] = rand::random();
				let salt = faster_hex::hex_string(&salt);
				let (mk, pwd) =
					crypto::v2::derive_password_and_mk(password.as_bytes(), salt.as_bytes())?;
				(pwd, salt, RegisteredAuthInfo::V2(mk))
			}
			AuthVersion::V3 => {
				let salt: [u8; 256] = rand::random();
				let salt = faster_hex::hex_string(&salt);
				let (kek, pwd) =
					crypto::v3::derive_password_and_kek(password.as_bytes(), salt.as_bytes())?;
				(pwd, salt, RegisteredAuthInfo::V3(kek))
			}
		};

		let resp = api::v3::register::post(
			&client,
			&api::v3::register::Request {
				email: Cow::Borrowed(&email),
				salt: Cow::Borrowed(&salt),
				auth_version: auth_info.version(),
				password: derived_pwd.as_borrowed_cow(),
				ref_id: ref_id.map(Cow::Borrowed),
				aff_id: aff_id.map(Cow::Borrowed),
			},
		)
		.await?;

		Ok(RegisteredInfo {
			email,
			salt,
			auth_info,
			api_key: resp.api_key,
		})
	}
}

pub async fn start_password_reset(email: &str) -> Result<(), Error> {
	let client = UnauthClient::default();
	api::v3::user::password::forgot::post(
		&client,
		&api::v3::user::password::forgot::Request {
			email: Cow::Borrowed(email),
		},
	)
	.await
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn auth_info_convert_into_exportable() {
		let auth_info = AuthInfo::V2(v2::AuthInfo {
			master_keys: MasterKeys(vec![
				MasterKey::from_str(
					"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
				)
				.unwrap(),
				MasterKey::from_str(
					"fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210",
				)
				.unwrap(),
			]),
		});
		let exported = auth_info.convert_into_exportable(123456).unwrap();
		assert_eq!(
			exported,
			"X1ZBTElEX0ZJTEVOX01BU1RFUktFWV8wMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWYwMTIzNDU2Nzg5YWJjZGVmQDEyMzQ1Nl9WQUxJRF9GSUxFTl9NQVNURVJLRVlffF9WQUxJRF9GSUxFTl9NQVNURVJLRVlfZmVkY2JhOTg3NjU0MzIxMGZlZGNiYTk4NzY1NDMyMTBmZWRjYmE5ODc2NTQzMjEwZmVkY2JhOTg3NjU0MzIxMEAxMjM0NTZfVkFMSURfRklMRU5fTUFTVEVSS0VZXw=="
		);
		let expected_master_keys = match auth_info {
			AuthInfo::V2(info) => info.master_keys.0,
			_ => unreachable!(),
		};
		let master_keys_vec = master_keys_from_exportable(&exported, 123456).unwrap();
		assert_eq!(master_keys_vec, expected_master_keys);
	}
}
