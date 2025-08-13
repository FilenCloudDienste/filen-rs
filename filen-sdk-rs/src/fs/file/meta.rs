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
	error::{Error, InvalidNameError, MetadataWasNotDecryptedError},
	fs::file::make_mime,
};

pub(crate) struct FileMetaSeed(pub(crate) FileEncryptionVersion);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFileMeta<'a> {
	pub(super) name: Cow<'a, str>,
	pub(super) size: u64,
	pub(super) mime: Cow<'a, str>,
	pub(super) key: Cow<'a, str>,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	pub(super) last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub(super) created: Option<DateTime<Utc>>,
	pub(super) hash: Option<Sha512Hash>,
}

impl<'de> DeserializeSeed<'de> for FileMetaSeed {
	type Value = DecryptedFileMeta<'de>;

	fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let raw_meta = RawFileMeta::deserialize(deserializer)?;
		let key = FileKeySeed(self.0).deserialize(raw_meta.key.into_deserializer())?;
		let meta = DecryptedFileMeta {
			name: raw_meta.name,
			size: raw_meta.size,
			mime: raw_meta.mime,
			key: Cow::Owned(key),
			last_modified: raw_meta.last_modified,
			created: raw_meta.created,
			hash: raw_meta.hash,
		};
		Ok(meta)
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMeta<'a> {
	Decoded(DecryptedFileMeta<'a>),
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(Cow<'a, EncryptedString>),
	RSAEncrypted(Cow<'a, RSAEncryptedString>),
}

macro_rules! get_value_from_decrypted {
	($field:ident, $out:ty) => {
		pub fn $field(&self) -> Option<$out> {
			match self {
				Self::Decoded(meta) => Some(meta.$field()),
				Self::DecryptedRaw(_)
				| Self::DecryptedUTF8(_)
				| Self::Encrypted(_)
				| Self::RSAEncrypted(_) => None,
			}
		}
	};
}

macro_rules! get_value_from_decrypted_optional {
	($field:ident, $out:ty) => {
		pub fn $field(&self) -> $out {
			match self {
				Self::Decoded(meta) => meta.$field(),
				Self::DecryptedRaw(_)
				| Self::DecryptedUTF8(_)
				| Self::Encrypted(_)
				| Self::RSAEncrypted(_) => None,
			}
		}
	};
}

impl FileMeta<'static> {
	pub fn from_encrypted(
		meta: Cow<'_, EncryptedString>,
		decrypter: &impl MetaCrypter,
		file_encryption_version: FileEncryptionVersion,
	) -> Self {
		let Ok(decrypted) = decrypter.decrypt_meta(&meta) else {
			return Self::Encrypted(Cow::Owned(meta.into_owned()));
		};
		let seed = FileMetaSeed(file_encryption_version);
		let Ok(meta) = seed.deserialize(&mut serde_json::Deserializer::from_str(&decrypted)) else {
			return Self::DecryptedUTF8(Cow::Owned(decrypted));
		};
		Self::Decoded(meta.into_owned())
	}

	pub fn from_rsa_encrypted(
		meta: Cow<'_, RSAEncryptedString>,
		private_key: &RsaPrivateKey,
		file_encryption_version: FileEncryptionVersion,
	) -> Self {
		let Ok(decrypted) = crypto::rsa::decrypt_with_private_key(private_key, &meta) else {
			return Self::RSAEncrypted(Cow::Owned(meta.into_owned()));
		};
		let seed = FileMetaSeed(file_encryption_version);
		let Ok(meta) = seed.deserialize(&mut serde_json::Deserializer::from_slice(&decrypted))
		else {
			match String::from_utf8(decrypted) {
				Ok(decrypted) => return Self::DecryptedUTF8(Cow::Owned(decrypted)),
				Err(err) => return Self::DecryptedRaw(Cow::Owned(err.into_bytes())),
			}
		};
		Self::Decoded(meta.into_owned())
	}
}

impl<'a> FileMeta<'a> {
	pub fn try_to_string(&self) -> Option<Cow<'_, str>> {
		match self {
			Self::Decoded(meta) => Some(Cow::Owned(serde_json::to_string(meta).unwrap())),
			Self::DecryptedUTF8(utf8) => Some(Cow::Borrowed(utf8)),
			Self::DecryptedRaw(_) | Self::Encrypted(_) | Self::RSAEncrypted(_) => None,
		}
	}

	pub(crate) fn apply_changes(&mut self, changes: FileMetaChanges) -> Result<(), Error> {
		match self {
			Self::Decoded(meta) => {
				meta.apply_changes(changes);
				Ok(())
			}
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => Err(MetadataWasNotDecryptedError.into()),
		}
	}

	pub(crate) fn borrow_with_changes(
		&'a self,
		changes: &'a FileMetaChanges,
	) -> Result<Self, Error> {
		match self {
			Self::Decoded(meta) => Ok(Self::Decoded(meta.borrowed_with_changes(changes))),
			Self::DecryptedRaw(_)
			| Self::DecryptedUTF8(_)
			| Self::Encrypted(_)
			| Self::RSAEncrypted(_) => Err(MetadataWasNotDecryptedError.into()),
		}
	}

	get_value_from_decrypted!(name, &str);
	get_value_from_decrypted!(mime, &str);
	get_value_from_decrypted!(last_modified, DateTime<Utc>);
	get_value_from_decrypted!(key, &FileKey);
	get_value_from_decrypted_optional!(created, Option<DateTime<Utc>>);
	get_value_from_decrypted_optional!(hash, Option<Sha512Hash>);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecryptedFileMeta<'a> {
	pub name: Cow<'a, str>,
	pub size: u64,
	pub mime: Cow<'a, str>,
	pub key: Cow<'a, FileKey>,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	pub last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub created: Option<DateTime<Utc>>,
	pub hash: Option<Sha512Hash>,
}

impl<'a> DecryptedFileMeta<'a> {
	pub fn into_owned(self) -> DecryptedFileMeta<'static> {
		DecryptedFileMeta {
			name: Cow::Owned(self.name.into_owned()),
			size: self.size,
			mime: Cow::Owned(self.mime.into_owned()),
			key: Cow::Owned(self.key.into_owned()),
			last_modified: self.last_modified,
			created: self.created,
			hash: self.hash,
		}
	}

	fn apply_changes(&mut self, changes: FileMetaChanges) {
		if let Some(name) = changes.name {
			// don't need to check for empty name here,
			// because it was already checked in FileMetaChanger::set_name
			self.name = Cow::Owned(name);
		}
		if let Some(mime) = changes.mime {
			self.mime = Cow::Owned(mime);
		}
		if let Some(last_modified) = changes.last_modified {
			self.last_modified = last_modified;
		}
		if let Some(created) = changes.created {
			self.created = created;
		}
	}

	pub fn borrowed_with_changes(&'a self, changes: &'a FileMetaChanges) -> Self {
		Self {
			name: Cow::Borrowed(changes.name.as_deref().unwrap_or(&self.name)),
			mime: Cow::Borrowed(changes.mime.as_deref().unwrap_or(&self.mime)),
			last_modified: changes.last_modified.unwrap_or(self.last_modified),
			created: changes.created.unwrap_or(self.created),
			key: Cow::Borrowed(&self.key),
			size: self.size,
			hash: self.hash,
		}
	}

	pub fn name(&self) -> &str {
		&self.name
	}

	pub fn set_name(&mut self, name: impl Into<Cow<'a, str>>) -> Result<(), Error> {
		let name = name.into();
		if name.is_empty() {
			return Err(InvalidNameError(name.into_owned()).into());
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

#[derive(Debug, PartialEq, Eq, Clone, Default)]
pub struct FileMetaChanges {
	name: Option<String>,
	mime: Option<String>,
	last_modified: Option<DateTime<Utc>>,
	created: Option<Option<DateTime<Utc>>>,
}

impl FileMetaChanges {
	pub fn name(mut self, name: String) -> Result<Self, Error> {
		if name.is_empty() {
			return Err(InvalidNameError(name).into());
		}
		if self.mime.is_none() {
			self.mime = Some(make_mime(&name, None));
		}
		self.name = Some(name);
		Ok(self)
	}

	pub fn mime(mut self, mime: String) -> Self {
		self.mime = Some(mime);
		self
	}

	pub fn last_modified(mut self, last_modified: DateTime<Utc>) -> Self {
		self.last_modified = Some(last_modified.round_subsecs(3));
		self
	}

	pub fn created(mut self, created: Option<DateTime<Utc>>) -> Self {
		self.created = Some(created.map(|t| t.round_subsecs(3)));
		self
	}
}
