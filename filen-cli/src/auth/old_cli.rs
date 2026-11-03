//! Credentials saved by the old Filen CLI, written in TypeScript, which a login without any
//! credentials of this CLI imports.

use std::{
	path::{Path, PathBuf},
	str::FromStr as _,
};

use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use filen_sdk_rs::{
	auth::StringifiedClient,
	crypto::{shared::DataCrypter as _, v3::EncryptionKey},
};
use filen_types::auth::FilenSDKConfig;

/// The auth config the old CLI exported, into the working directory or its data directory: its
/// SDK config as base64 JSON.
const AUTH_CONFIG_FILENAME: &str = ".filen-cli-auth-config";
/// The login the old CLI kept after "Keep me logged in?", in its data directory: its SDK config as
/// JSON, encrypted with AES-256-GCM (nonce, ciphertext, tag) under the key in the keychain, as
/// base64.
const KEPT_LOGIN_FILENAME: &str = ".filen-cli-keep-me-logged-in";
/// The keychain service of the key the old CLI encrypted its kept login with.
pub(super) const KEYCHAIN_SERVICE: &str = "filen-cli";

/// The keychain account of the key the old CLI encrypted its kept login with, in a dev build or
/// not.
pub(super) fn keychain_key_name(is_dev: bool) -> &'static str {
	match is_dev {
		true => "auth-config-crypto-key-dev",
		false => "auth-config-crypto-key",
	}
}

/// The data directory of the old CLI: `home_install_dir` (`~/.filen-cli`, made by its install
/// script) if it exists, or else `filen-cli` in the system's config directory, with `dev` below it
/// in a dev build.
pub(super) fn data_dir(
	is_dev: bool,
	system_config_dir: Option<&Path>,
	home_install_dir: Option<&Path>,
) -> Option<PathBuf> {
	let dir = match home_install_dir {
		Some(dir) => dir.to_path_buf(),
		None => system_config_dir?.join("filen-cli"),
	};
	Some(match is_dev {
		true => dir.join("dev"),
		false => dir,
	})
}

/// A file of credentials the old CLI saved.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct SavedLogin {
	pub(super) path: PathBuf,
	kind: SavedLoginKind,
}

#[derive(Debug, PartialEq, Eq)]
enum SavedLoginKind {
	/// `.filen-cli-auth-config`
	AuthConfig,
	/// `.filen-cli-keep-me-logged-in`
	KeptLogin,
}

/// The files of credentials the old CLI saved that exist, in the order it used them: the auth
/// configs in `working_dir` and in `data_dir`, then the login kept in `data_dir`.
pub(super) fn find(working_dir: Option<&Path>, data_dir: Option<&Path>) -> Vec<SavedLogin> {
	[
		working_dir.map(|dir| (dir.join(AUTH_CONFIG_FILENAME), SavedLoginKind::AuthConfig)),
		data_dir.map(|dir| (dir.join(AUTH_CONFIG_FILENAME), SavedLoginKind::AuthConfig)),
		data_dir.map(|dir| (dir.join(KEPT_LOGIN_FILENAME), SavedLoginKind::KeptLogin)),
	]
	.into_iter()
	.flatten()
	.filter(|(path, _)| path.exists())
	.map(|(path, kind)| SavedLogin { path, kind })
	.collect()
}

impl SavedLogin {
	/// The login saved in this file. `key` looks up the key in the keychain that a kept login is
	/// encrypted with.
	pub(super) fn read(
		&self,
		key: impl FnOnce() -> Result<Option<String>>,
	) -> Result<StringifiedClient> {
		let contents = std::fs::read_to_string(&self.path).context("Failed to read the file")?;
		let sdk_config = match self.kind {
			SavedLoginKind::AuthConfig => decode_auth_config(&contents)?,
			SavedLoginKind::KeptLogin => {
				let key = key()?.ok_or_else(|| anyhow!("Its key is missing from the keychain"))?;
				decrypt_kept_login(&contents, &key)?
			}
		};
		Ok(sdk_config.into())
	}
}

/// The SDK config in the contents of an auth config file of the old CLI.
fn decode_auth_config(contents: &str) -> Result<FilenSDKConfig> {
	let json = BASE64
		.decode(contents.trim())
		.context("Failed to decode the auth config")?;
	serde_json::from_slice(&json).context("Failed to parse the auth config")
}

/// The SDK config in the contents of the old CLI's kept login file, with `key` the hex key from the
/// keychain.
fn decrypt_kept_login(contents: &str, key: &str) -> Result<FilenSDKConfig> {
	let key = EncryptionKey::from_str(key.trim()).context("Failed to parse its key")?;
	let mut data = BASE64
		.decode(contents.trim())
		.context("Failed to decode the kept login")?;
	key.blocking_decrypt_data(&mut data)
		.context("Failed to decrypt the kept login")?;
	serde_json::from_slice(&data).context("Failed to parse the kept login")
}

#[cfg(test)]
mod tests {
	use filen_types::auth::AuthVersion;

	use super::*;

	fn sdk_config() -> FilenSDKConfig {
		FilenSDKConfig {
			email: "old@example.com".to_owned(),
			password: "redacted".to_owned(),
			two_factor_code: "redacted".to_owned(),
			master_keys: vec!["key-a".to_owned(), "key-b".to_owned()],
			api_key: "api-key".to_owned(),
			public_key: "public-key".to_owned(),
			private_key: "private-key".to_owned(),
			auth_version: AuthVersion::V2,
			base_folder_uuid: "00000000-0000-0000-0000-000000000001".to_owned(),
			user_id: 42,
			metadata_cache: true,
			tmp_path: "/tmp/filen-cli".to_owned(),
			connect_to_socket: true,
		}
	}

	/// The key the tests encrypt a kept login with.
	fn key() -> EncryptionKey {
		EncryptionKey::new([7; 32])
	}

	/// What the old CLI wrote into its kept login file for [`sdk_config`], encrypted with `key`.
	fn kept_login_contents(key: &EncryptionKey) -> String {
		let mut data = serde_json::to_vec(&sdk_config()).unwrap();
		key.blocking_encrypt_data(&mut data).unwrap();
		BASE64.encode(data)
	}

	/// `contents` saved as a `kind` file in a fresh directory.
	fn saved_login(contents: &str, kind: SavedLoginKind) -> (assert_fs::TempDir, SavedLogin) {
		let dir = assert_fs::TempDir::new().unwrap();
		let path = dir.path().join(match kind {
			SavedLoginKind::AuthConfig => AUTH_CONFIG_FILENAME,
			SavedLoginKind::KeptLogin => KEPT_LOGIN_FILENAME,
		});
		std::fs::write(&path, contents).unwrap();
		(dir, SavedLogin { path, kind })
	}

	fn assert_is_the_sdk_config(client: &StringifiedClient) {
		assert_eq!(client.email, "old@example.com");
		assert_eq!(client.user_id, 42);
		assert_eq!(client.root_uuid, "00000000-0000-0000-0000-000000000001");
		assert_eq!(client.auth_info, "key-a|key-b");
		assert_eq!(client.private_key, "private-key");
		assert_eq!(client.api_key, "api-key");
		assert_eq!(client.auth_version, 2);
	}

	#[test]
	fn an_auth_config_of_the_old_cli_is_read() {
		let contents = BASE64.encode(serde_json::to_vec(&sdk_config()).unwrap());
		let (_dir, saved) = saved_login(&contents, SavedLoginKind::AuthConfig);

		let client = saved
			.read(|| panic!("an auth config needs no key"))
			.unwrap();

		assert_is_the_sdk_config(&client);
	}

	#[test]
	fn a_corrupt_auth_config_of_the_old_cli_fails() {
		let (_dir, saved) = saved_login("not base64!", SavedLoginKind::AuthConfig);

		let err = saved.read(|| Ok(None)).unwrap_err();

		assert!(
			format!("{err:#}").starts_with("Failed to decode the auth config"),
			"{err:#}"
		);
	}

	#[test]
	fn a_kept_login_of_the_old_cli_is_decrypted_with_the_key_from_the_keychain() {
		let (_dir, saved) = saved_login(&kept_login_contents(&key()), SavedLoginKind::KeptLogin);

		let client = saved.read(|| Ok(Some(key().to_string()))).unwrap();

		assert_is_the_sdk_config(&client);
	}

	#[test]
	fn a_kept_login_with_another_key_fails() {
		let (_dir, saved) = saved_login(&kept_login_contents(&key()), SavedLoginKind::KeptLogin);

		let err = saved
			.read(|| Ok(Some(EncryptionKey::new([8; 32]).to_string())))
			.unwrap_err();

		assert!(
			format!("{err:#}").starts_with("Failed to decrypt the kept login"),
			"{err:#}"
		);
	}

	#[test]
	fn a_kept_login_without_its_key_fails() {
		let (_dir, saved) = saved_login(&kept_login_contents(&key()), SavedLoginKind::KeptLogin);

		let err = saved.read(|| Ok(None)).unwrap_err();

		assert_eq!(format!("{err:#}"), "Its key is missing from the keychain");
	}

	#[test]
	fn the_data_dir_of_a_release_build_is_in_the_system_config_dir() {
		assert_eq!(
			data_dir(false, Some(Path::new("/home/user/.config")), None),
			Some(PathBuf::from("/home/user/.config/filen-cli"))
		);
	}

	#[test]
	fn the_data_dir_of_a_dev_build_is_below_the_one_of_a_release_build() {
		assert_eq!(
			data_dir(true, Some(Path::new("/home/user/.config")), None),
			Some(PathBuf::from("/home/user/.config/filen-cli/dev"))
		);
	}

	#[test]
	fn the_install_scripts_dir_is_the_data_dir_when_it_exists() {
		assert_eq!(
			data_dir(
				false,
				Some(Path::new("/home/user/.config")),
				Some(Path::new("/home/user/.filen-cli"))
			),
			Some(PathBuf::from("/home/user/.filen-cli"))
		);
	}

	#[test]
	fn there_is_no_data_dir_without_a_system_config_dir() {
		assert_eq!(data_dir(false, None, None), None);
	}

	#[test]
	fn the_saved_logins_are_found_in_the_order_the_old_cli_used_them() {
		let working_dir = assert_fs::TempDir::new().unwrap();
		let data_dir = assert_fs::TempDir::new().unwrap();
		for path in [
			working_dir.path().join(AUTH_CONFIG_FILENAME),
			data_dir.path().join(KEPT_LOGIN_FILENAME),
			data_dir.path().join(AUTH_CONFIG_FILENAME),
		] {
			std::fs::write(path, "").unwrap();
		}

		assert_eq!(
			find(Some(working_dir.path()), Some(data_dir.path())),
			vec![
				SavedLogin {
					path: working_dir.path().join(AUTH_CONFIG_FILENAME),
					kind: SavedLoginKind::AuthConfig,
				},
				SavedLogin {
					path: data_dir.path().join(AUTH_CONFIG_FILENAME),
					kind: SavedLoginKind::AuthConfig,
				},
				SavedLogin {
					path: data_dir.path().join(KEPT_LOGIN_FILENAME),
					kind: SavedLoginKind::KeptLogin,
				},
			]
		);
	}

	#[test]
	fn files_that_do_not_exist_are_not_found() {
		let working_dir = assert_fs::TempDir::new().unwrap();
		assert_eq!(find(Some(working_dir.path()), None), vec![]);
	}
}
