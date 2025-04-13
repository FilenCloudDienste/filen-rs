use std::{borrow::Cow, str::FromStr};

use chrono::{DateTime, Utc};
use filen_types::crypto::Sha512Hash;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{self};

use super::HasMeta;

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileKey {
	V2(crypto::v2::FileKey),
	V3(crypto::v3::EncryptionKey),
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
				crypto::v2::FileKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			64 => Ok(FileKey::V3(
				crypto::v3::EncryptionKey::from_str(&key).map_err(serde::de::Error::custom)?,
			)),
			_ => Err(serde::de::Error::custom(format!(
				"Invalid key length: {}",
				key.len()
			))),
		}
	}
}

impl crypto::shared::DataCrypter for FileKey {
	fn encrypt_data(&self, data: &mut Vec<u8>) -> Result<(), crypto::error::ConversionError> {
		match self {
			FileKey::V2(key) => key.encrypt_data(data),
			FileKey::V3(key) => key.encrypt_data(data),
		}
	}
	fn decrypt_data(&self, data: &mut Vec<u8>) -> Result<(), crypto::error::ConversionError> {
		match self {
			FileKey::V2(key) => key.decrypt_data(data),
			FileKey::V3(key) => key.decrypt_data(data),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
	uuid: Uuid,
	name: String,
	parent: Uuid,

	mime: String,
	key: FileKey,
	created: DateTime<Utc>,
	modified: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]

pub struct RemoteFile {
	file: File,
	size: u64,
	favorited: bool,
	region: String,
	bucket: String,
	chunks: u64,
	hash: Sha512Hash,
}

impl HasMeta for &RemoteFile {
	fn name(&self) -> &str {
		&self.file.name
	}

	fn meta(
		&self,
		crypter: impl crypto::shared::MetaCrypter,
	) -> Result<filen_types::crypto::EncryptedString, crypto::error::ConversionError> {
		// SAFETY if this fails, I want it to panic
		// as this is a logic error
		let string = serde_json::to_string(&FileMeta {
			name: Cow::Borrowed(&self.file.name),
			size: self.size,
			mime: Cow::Borrowed(&self.file.mime),
			key: Cow::Borrowed(&self.file.key),
			created: self.file.created,
			last_modified: self.file.modified,
			hash: self.hash,
		})
		.unwrap();
		crypter.encrypt_meta(&string)
	}
}

impl RemoteFile {
	pub fn from_encrypted(
		file: filen_types::api::v3::dir::content::File,
		decrypter: impl crypto::shared::MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let meta = FileMeta::from_encrypted(&file.metadata, decrypter)?;
		Ok(Self {
			file: File {
				name: meta.name.into_owned(),
				uuid: file.uuid,
				parent: file.parent,
				mime: meta.mime.into_owned(),
				key: meta.key.into_owned(),
				created: meta.created,
				modified: meta.last_modified,
			},
			size: file.size,
			favorited: file.favorited,
			region: file.region,
			bucket: file.bucket,
			chunks: file.chunks,
			hash: meta.hash,
		})
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileMeta<'a> {
	name: Cow<'a, str>,
	size: u64,
	mime: Cow<'a, str>,
	key: Cow<'a, FileKey>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	last_modified: DateTime<Utc>,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	#[serde(rename = "creation")]
	created: DateTime<Utc>,
	hash: Sha512Hash,
}

impl FileMeta<'_> {
	fn from_encrypted(
		meta: &filen_types::crypto::EncryptedString,
		decrypter: impl crypto::shared::MetaCrypter,
	) -> Result<Self, crate::error::Error> {
		let decrypted = decrypter.decrypt_meta(meta)?;
		let meta: FileMeta = serde_json::from_str(&decrypted)?;
		Ok(meta)
	}
}
