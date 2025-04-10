use std::{borrow::Cow, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::crypto::{self, shared::Sha512Hash};

use super::{FSObject, Metadata, NonRootFSObject};

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

pub(crate) struct File {
	uuid: Uuid,
	name: String,
	parent: Uuid,

	mime: String,
	key: FileKey,
	created: DateTime<Utc>,
	modified: DateTime<Utc>,
}

pub(crate) struct RemoteFile {
	file: File,
	size: u64,
	favorited: bool,
	region: String,
	bucket: String,
	chunks: u64,
	hash: Sha512Hash,
}

impl FSObject for RemoteFile {
	fn name(&self) -> &str {
		&self.file.name
	}

	fn uuid(&self) -> &uuid::Uuid {
		&self.file.uuid
	}
}

impl NonRootFSObject for RemoteFile {
	fn parent(&self) -> &uuid::Uuid {
		&self.file.parent
	}

	fn get_meta(&self) -> impl super::Metadata<'_> {
		FileMeta {
			name: Cow::Borrowed(&self.file.name),
			size: self.size,
			mime: Cow::Borrowed(&self.file.mime),
			key: Cow::Borrowed(&self.file.key),
			last_modified: self.file.modified,
			created: self.file.created,
			hash: self.hash,
		}
	}
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FileMeta<'a> {
	name: Cow<'a, str>,
	size: u64,
	mime: Cow<'a, str>,
	key: Cow<'a, FileKey>,
	last_modified: DateTime<Utc>,
	created: DateTime<Utc>,
	hash: Sha512Hash,
}

impl Metadata<'_> for FileMeta<'_> {
	fn make_string(&self) -> String {
		serde_json::to_string(self).unwrap()
	}
}
