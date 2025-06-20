use std::{borrow::Cow, fmt::Debug, str::FromStr};

use filen_types::auth::FileEncryptionVersion;
use serde::{Deserialize, Serialize, de::DeserializeSeed};

use crate::crypto::v1;

use super::{error::ConversionError, shared::DataCrypter, v2, v3};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKey {
	V1(v1::FileKey),
	V2(v2::FileKey),
	V3(v3::EncryptionKey),
}

impl FileKey {
	pub fn to_str(&self) -> Cow<'_, str> {
		match self {
			FileKey::V1(key) => Cow::Borrowed(key.as_ref()),
			FileKey::V2(key) => Cow::Borrowed(key.as_ref()),
			FileKey::V3(key) => Cow::Owned(key.to_string()),
		}
	}

	pub fn version(&self) -> FileEncryptionVersion {
		match self {
			FileKey::V1(_) => FileEncryptionVersion::V1,
			FileKey::V2(_) => FileEncryptionVersion::V2,
			FileKey::V3(_) => FileEncryptionVersion::V3,
		}
	}
}

impl Serialize for FileKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			FileKey::V1(key) => key.serialize(serializer),
			FileKey::V2(key) => key.serialize(serializer),
			FileKey::V3(key) => key.serialize(serializer),
		}
	}
}

// todo, handle v1?
pub(crate) struct FileKeySeed(pub(crate) FileEncryptionVersion);

impl<'de> DeserializeSeed<'de> for FileKeySeed {
	type Value = FileKey;

	fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		match self.0 {
			FileEncryptionVersion::V1 => {
				let v1_key = v1::FileKey::from_str(&key).map_err(serde::de::Error::custom)?;
				Ok(FileKey::V1(v1_key))
			}
			FileEncryptionVersion::V2 => {
				let v2_key = v2::FileKey::from_str(&key).map_err(serde::de::Error::custom)?;
				Ok(FileKey::V2(v2_key))
			}
			FileEncryptionVersion::V3 => {
				let v3_key = v3::EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?;
				Ok(FileKey::V3(v3_key))
			}
		}
	}
}

impl FileKey {
	pub fn from_str_with_version(
		key: &str,
		version: FileEncryptionVersion,
	) -> Result<Self, ConversionError> {
		match version {
			FileEncryptionVersion::V1 => v1::FileKey::from_str(key).map(FileKey::V1),
			FileEncryptionVersion::V2 => v2::FileKey::from_str(key).map(FileKey::V2),
			FileEncryptionVersion::V3 => v3::EncryptionKey::from_str(key).map(FileKey::V3),
		}
	}
}

impl DataCrypter for FileKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		match self {
			FileKey::V1(key) => key.encrypt_data(data),
			FileKey::V2(key) => key.encrypt_data(data),
			FileKey::V3(key) => key.encrypt_data(data),
		}
	}
	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		match self {
			FileKey::V1(key) => key.decrypt_data(data),
			FileKey::V2(key) => key.decrypt_data(data),
			FileKey::V3(key) => key.decrypt_data(data),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stringify_file_key() {
		assert!(FileKey::from_str_with_version("ab", FileEncryptionVersion::V2).is_err());
		let a64 = "a".repeat(64);
		let a32 = "a".repeat(32);
		let v2 = FileKey::from_str_with_version(&a32, FileEncryptionVersion::V2).unwrap();
		assert_eq!(v2.to_str(), a32);
		let v3 = FileKey::from_str_with_version(&a64, FileEncryptionVersion::V3).unwrap();
		assert_eq!(v3.to_str(), a64);
	}
}
