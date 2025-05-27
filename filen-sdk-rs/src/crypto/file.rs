use std::{
	fmt::{Debug, Display},
	str::FromStr,
};

use serde::{Deserialize, Serialize};

use super::{error::ConversionError, shared::DataCrypter, v2, v3};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileKey {
	V2(v2::FileKey),
	V3(v3::EncryptionKey),
}

impl Display for FileKey {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			FileKey::V2(key) => key.fmt(f),
			FileKey::V3(key) => Display::fmt(&key, f),
		}
	}
}

impl Serialize for FileKey {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			FileKey::V2(key) => key.serialize(serializer),
			FileKey::V3(key) => key.serialize(serializer),
		}
	}
}

impl<'de> Deserialize<'de> for FileKey {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let key = String::deserialize(deserializer)?;
		match key.len() {
			32 => Ok(FileKey::V2(
				v2::FileKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			64 => Ok(FileKey::V3(
				v3::EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			_ => Err(serde::de::Error::custom(format!(
				"Invalid key length: {}",
				key.len()
			))),
		}
	}
}

impl FromStr for FileKey {
	type Err = ConversionError;
	fn from_str(key: &str) -> Result<Self, Self::Err> {
		if key.len() == 32 {
			Ok(FileKey::V2(v2::FileKey::from_str(key)?))
		} else if key.len() == 64 {
			Ok(FileKey::V3(v3::EncryptionKey::from_str(key)?))
		} else {
			Err(ConversionError::InvalidStringLength(key.len(), 32))
		}
	}
}

impl DataCrypter for FileKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		match self {
			FileKey::V2(key) => key.encrypt_data(data),
			FileKey::V3(key) => key.encrypt_data(data),
		}
	}
	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), ConversionError> {
		match self {
			FileKey::V2(key) => key.decrypt_data(data),
			FileKey::V3(key) => key.decrypt_data(data),
		}
	}
}
