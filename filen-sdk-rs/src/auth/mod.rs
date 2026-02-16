use std::{
	borrow::Cow,
	fmt::{Debug, Display},
	num::NonZeroU32,
	str::FromStr,
	sync::{Arc, Weak},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use chrono::{DateTime, Utc};
use digest::{Digest, FixedOutput, KeyInit, Update};
use filen_types::{
	auth::{AuthVersion, FileEncryptionVersion, FilenSDKConfig, MetaEncryptionVersion},
	crypto::{EncryptedMetaKey, EncryptedString},
	serde::rsa::RsaDerPublicKey,
};
use http::AuthClient;
use rsa::{RsaPrivateKey, RsaPublicKey};
use rsa::{pkcs1::EncodeRsaPublicKey, pkcs8::EncodePrivateKey};
use serde::{Deserialize, Serialize};

use crate::{
	api,
	auth::unauth::UnauthClient,
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

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use crate::socket::WebSocketHandle;

pub mod http;
#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
pub mod js_impls;
pub mod shared_client;
pub mod unauth;
pub mod v1;
pub mod v2;
pub mod v3;

#[cfg(any(feature = "wasm-full", feature = "uniffi"))]
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

pub struct Client {
	email: String,
	pub(crate) user_id: u64,

	root_dir: RootDirectory,

	auth_info: std::sync::RwLock<Arc<AuthInfo>>,
	file_encryption_version: FileEncryptionVersion,
	meta_encryption_version: MetaEncryptionVersion,

	public_key: RsaPublicKey,
	private_key: Arc<RsaPrivateKey>,
	pub(crate) hmac_key: HMACKey,

	http_client: Arc<AuthClient>,

	pub(crate) drive_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) notes_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) chats_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,
	pub(crate) auth_lock: tokio::sync::RwLock<Option<Weak<ResourceLock>>>,

	pub(crate) max_parallel_requests: usize,
	pub open_file_semaphore: tokio::sync::Semaphore,

	#[cfg(any(
		not(all(target_family = "wasm", target_os = "unknown")),
		feature = "wasm-full"
	))]
	pub(crate) socket_handle: std::sync::Mutex<WebSocketHandle>,
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
	pub fn get_unauthed(&self) -> UnauthClient {
		self.http_client.to_unauthed()
	}

	pub(crate) fn client(&self) -> &AuthClient {
		&self.http_client
	}

	pub(crate) fn arc_client(&self) -> Arc<AuthClient> {
		self.http_client.clone()
	}

	pub(crate) fn arc_client_ref(&self) -> &Arc<AuthClient> {
		&self.http_client
	}

	pub fn crypter(&self) -> Arc<impl MetaCrypter + 'static> {
		self.auth_info
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.clone()
	}

	pub fn private_key(&self) -> &RsaPrivateKey {
		&self.private_key
	}

	pub fn arc_private_key(&self) -> Arc<RsaPrivateKey> {
		self.private_key.clone()
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
			api_key: self
				.http_client
				.api_key()
				.read()
				.unwrap_or_else(|e| e.into_inner())
				.to_string(),
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

	pub async fn set_request_rate_limit(&self, requests_per_second: NonZeroU32) {
		self.client()
			.set_request_rate_limit(requests_per_second)
			.await;
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub async fn set_bandwidth_limits(
		&self,
		upload_kbps: Option<NonZeroU32>,
		download_kbps: Option<NonZeroU32>,
	) {
		self.client()
			.set_bandwidth_limits(upload_kbps, download_kbps)
			.await;
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

		*self
			.client()
			.api_key()
			.write()
			.unwrap_or_else(|e| e.into_inner()) = resp.new_api_key;

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
}

#[cfg_attr(
	feature = "wasm-full",
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

	pub fn make_totp_code(&self, for_time: DateTime<Utc>) -> Result<String, Error> {
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

		Ok(format!("{:06}", code % 1_000_000))
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
			.finish()
	}
}
