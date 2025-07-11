use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{EncryptedString, Sha512Hash, rsa::RSAEncryptedString},
};
use rsa::RsaPrivateKey;
use serde::{
	Deserialize, Serialize,
	de::{DeserializeSeed, IntoDeserializer},
};

use crate::{
	crypto::{
		self,
		file::{FileKey, FileKeySeed},
		shared::MetaCrypter,
	},
	error::Error,
};

pub(crate) struct FileMetaSeed(pub(crate) FileEncryptionVersion);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFileMeta {
	pub(super) name: String,
	pub(super) size: u64,
	pub(super) mime: String,
	pub(super) key: String,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	pub(super) last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub(super) created: Option<DateTime<Utc>>,
	pub(super) hash: Option<Sha512Hash>,
}

impl<'de> DeserializeSeed<'de> for FileMetaSeed {
	type Value = FileMeta<'static>;

	fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw_meta = RawFileMeta::deserialize(deserializer)?;
		let key = FileKeySeed(self.0).deserialize(raw_meta.key.into_deserializer())?;
		let meta = FileMeta {
			name: raw_meta.name.into(),
			size: raw_meta.size,
			mime: raw_meta.mime.into(),
			key: Cow::Owned(key),
			last_modified: raw_meta.last_modified,
			created: raw_meta.created,
			hash: raw_meta.hash,
		};
		Ok(meta)
	}
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMeta<'a> {
	pub(super) name: Cow<'a, str>,
	pub(super) size: u64,
	pub(super) mime: Cow<'a, str>,
	pub(super) key: Cow<'a, FileKey>,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	pub(super) last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub(super) created: Option<DateTime<Utc>>,
	pub(super) hash: Option<Sha512Hash>,
}

impl<'a> FileMeta<'a> {
	pub(crate) fn from_encrypted(
		meta: &EncryptedString,
		decrypter: &impl MetaCrypter,
		file_encryption_version: FileEncryptionVersion,
	) -> Result<Self, Error> {
		let decrypted = decrypter.decrypt_meta(meta)?;
		let seed = FileMetaSeed(file_encryption_version);
		let meta = seed.deserialize(&mut serde_json::Deserializer::from_str(&decrypted))?;
		Ok(meta)
	}

	pub(crate) fn from_rsa_encrypted(
		meta: &RSAEncryptedString,
		private_key: &RsaPrivateKey,
		file_encryption_version: FileEncryptionVersion,
	) -> Result<Self, Error> {
		let decrypted = crypto::rsa::decrypt_with_private_key(private_key, meta)?;
		let seed = FileMetaSeed(file_encryption_version);
		let meta = seed.deserialize(&mut serde_json::Deserializer::from_slice(&decrypted))?;
		Ok(meta)
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn set_name(&mut self, name: impl Into<Cow<'a, str>>) -> Result<(), Error> {
		let name = name.into();
		if name.is_empty() {
			return Err(Error::InvalidName(name.into()));
		}
		self.name = name;
		Ok(())
	}

	pub fn mime(&self) -> &str {
		&self.mime
	}

	pub fn set_mime(&mut self, mime: impl Into<Cow<'a, str>>) {
		self.mime = mime.into();
	}

	pub fn last_modified(&self) -> DateTime<Utc> {
		self.last_modified
	}

	pub fn set_last_modified(&mut self, last_modified: DateTime<Utc>) {
		self.last_modified = last_modified.round_subsecs(3);
	}

	pub fn created(&self) -> Option<DateTime<Utc>> {
		self.created
	}

	pub fn set_created(&mut self, created: DateTime<Utc>) {
		self.created = Some(created.round_subsecs(3));
	}

	pub fn hash(&self) -> Option<Sha512Hash> {
		self.hash
	}

	pub fn size(&self) -> u64 {
		self.size
	}

	pub fn key(&self) -> &FileKey {
		&self.key
	}
}
