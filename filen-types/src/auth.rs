use std::{
	borrow::Cow,
	fmt::{Debug, Display},
};

use serde::{Deserialize, Serialize};
use serde_repr::{Deserialize_repr, Serialize_repr};

use crate::impl_cow_helpers_for_newtype;

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct APIKey<'a>(pub Cow<'a, str>);
impl_cow_helpers_for_newtype!(APIKey);

impl Display for APIKey<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum AuthVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum FileEncryptionVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[wasm_bindgen::prelude::wasm_bindgen(typescript_custom_section)]
const TS_FILE_ENCRYPTION_VERSION: &'static str =
	r#"export type FileEncryptionVersion = 1 | 2 | 3;"#;

impl From<u8> for FileEncryptionVersion {
	fn from(value: u8) -> Self {
		match value {
			1 => FileEncryptionVersion::V1,
			2 => FileEncryptionVersion::V2,
			3 => FileEncryptionVersion::V3,
			o => panic!("Invalid FileEncryptionVersion value {o}"),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize_repr, Deserialize_repr)]
#[repr(u8)]
pub enum MetaEncryptionVersion {
	V1 = 1,
	V2 = 2,
	V3 = 3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilenSDKConfig {
	pub email: String,
	pub password: String,
	pub two_factor_code: String,
	pub master_keys: Vec<String>,
	pub api_key: String,
	pub public_key: String,
	pub private_key: String,
	pub auth_version: AuthVersion,
	#[serde(rename = "baseFolderUUID")]
	pub base_folder_uuid: String,
	pub user_id: u64,
	pub metadata_cache: bool,
	pub tmp_path: String,
	pub connect_to_socket: bool,
}
