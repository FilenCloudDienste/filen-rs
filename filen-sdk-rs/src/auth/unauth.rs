use std::{
	borrow::Cow,
	str::FromStr,
	sync::{Arc, RwLock},
};

use base64::{Engine, prelude::BASE64_STANDARD};
use filen_types::{
	auth::{APIKey, AuthVersion, FileEncryptionVersion, MetaEncryptionVersion},
	fs::UuidStr,
	serde::rsa::RsaDerPublicKey,
	traits::CowHelpers,
};
use rsa::{RsaPrivateKey, RsaPublicKey, pkcs8::DecodePrivateKey};

#[cfg(any(
	not(all(target_family = "wasm", target_os = "unknown")),
	feature = "wasm-full"
))]
use crate::socket::WebSocketHandle;
use crate::{
	Error, api,
	auth::{
		AuthInfo, Client, StringifiedClient,
		http::{AuthClient, ClientConfig, SharedClientState},
	},
	consts::{
		NEW_ACCOUNT_AUTH_VERSION, RSA_KEY_SIZE, V2FILE_ENCRYPTION_VERSION,
		V2META_ENCRYPTION_VERSION,
	},
	crypto::{
		self,
		error::ConversionError,
		rsa::HMACKey,
		v2::{MasterKey, MasterKeys},
		v3::EncryptionKey,
	},
	fs::dir::RootDirectory,
};

use super::{v1, v2, v3};

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[derive(Clone)]
pub struct UnauthClient {
	pub(crate) state: SharedClientState,
	pub(crate) reqwest_client: reqwest::Client,
}

impl UnauthClient {
	pub fn from_config(client_config: ClientConfig) -> Result<Self, Error> {
		let state = SharedClientState::new(client_config)?;
		Ok(Self {
			reqwest_client: reqwest::Client::new(),
			state,
		})
	}

	pub fn from_stringified(&self, stringified: StringifiedClient) -> Result<Client, Error> {
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

		let private_key = RsaPrivateKey::from_pkcs8_der(
			&BASE64_STANDARD
				.decode(stringified.private_key)
				.map_err(ConversionError::from)?,
		)
		.map_err(ConversionError::from)?;

		let max_parallel_requests = stringified
			.max_parallel_requests
			.map(|v| usize::try_from(v).unwrap_or(crate::consts::MAX_SMALL_PARALLEL_REQUESTS))
			.unwrap_or(crate::consts::MAX_SMALL_PARALLEL_REQUESTS);

		let http_client = Arc::new(AuthClient::from_unauthed(
			self.clone(),
			Arc::new(RwLock::new(APIKey(Cow::Owned(stringified.api_key)))),
		));

		Ok(Client {
			email: stringified.email,
			user_id: stringified.user_id,
			root_dir: RootDirectory::new(
				UuidStr::from_str(&stringified.root_uuid).map_err(ConversionError::from)?,
			),
			auth_info: std::sync::RwLock::new(Arc::new(auth_info)),
			file_encryption_version,
			meta_encryption_version,
			public_key: RsaPublicKey::from(&private_key),
			hmac_key: HMACKey::new(&private_key),
			private_key: Arc::new(private_key),
			http_client,
			drive_lock: tokio::sync::RwLock::new(None),
			notes_lock: tokio::sync::RwLock::new(None),
			chats_lock: tokio::sync::RwLock::new(None),
			auth_lock: tokio::sync::RwLock::new(None),
			max_parallel_requests,
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
			#[cfg(any(
				not(all(target_family = "wasm", target_os = "unknown")),
				feature = "wasm-full"
			))]
			socket_handle: std::sync::Mutex::new(WebSocketHandle::default()),
		})
	}

	pub async fn login(
		&self,
		email: String,
		pwd: &str,
		two_factor_code: &str,
	) -> Result<Client, Error> {
		let info_response = api::v3::auth::info::post(
			self,
			&api::v3::auth::info::Request {
				email: Cow::Borrowed(&email),
			},
		)
		.await?;

		let (client, auth_info, private_key, public_key) = match info_response.auth_version {
			AuthVersion::V1 => {
				v1::login(&email, pwd, two_factor_code, &info_response, self).await?
			}
			AuthVersion::V2 | AuthVersion::V3 => {
				let (client, auth_info, private_key, public_key) = match info_response.auth_version
				{
					AuthVersion::V2 => {
						v2::login(&email, pwd, two_factor_code, &info_response, self).await?
					}
					AuthVersion::V3 => {
						v3::login(&email, pwd, two_factor_code, &info_response, self).await?
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
			private_key: Arc::new(private_key),
			hmac_key: hmac,
			http_client: http_client.clone(),
			drive_lock: tokio::sync::RwLock::new(None),
			notes_lock: tokio::sync::RwLock::new(None),
			chats_lock: tokio::sync::RwLock::new(None),
			auth_lock: tokio::sync::RwLock::new(None),
			max_parallel_requests: crate::consts::MAX_SMALL_PARALLEL_REQUESTS,
			open_file_semaphore: tokio::sync::Semaphore::new(crate::consts::MAX_OPEN_FILES),
			#[cfg(any(
				not(all(target_family = "wasm", target_os = "unknown")),
				feature = "wasm-full"
			))]
			socket_handle: std::sync::Mutex::new(WebSocketHandle::default()),
		})
	}

	pub async fn complete_password_reset(
		&self,
		token: &str,
		email: String,
		new_password: &str,
		recovery_key: Option<&str>,
	) -> Result<Client, Error> {
		let auth_info_resp = api::v3::auth::info::post(
			self,
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
			self,
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
		self.login(email, new_password, "XXXXXX").await
	}

	pub async fn register(
		&self,
		email: String,
		password: &str,
		ref_id: Option<&str>,
		aff_id: Option<&str>,
	) -> Result<RegisteredInfo, Error> {
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
			self,
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

	pub async fn start_password_reset(&self, email: &str) -> Result<(), Error> {
		api::v3::user::password::forgot::post(
			self,
			&api::v3::user::password::forgot::Request {
				email: Cow::Borrowed(email),
			},
		)
		.await
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
