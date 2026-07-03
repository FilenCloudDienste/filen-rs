use std::borrow::Cow;

use chrono::{DateTime, SubsecRound, Utc};
use filen_macros::js_type;
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{Blake3Hash, EncryptedString, rsa::RSAEncryptedString},
	rkyv::date_time::DateTimeUtcDef,
	traits::CowHelpers,
};
use rkyv::with::{AsOwned, Map};
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
	fs::{
		file::make_mime,
		meta_recovery,
		name::{EntryNameError, ValidatedName},
	},
};

pub(crate) struct FileMetaSeed(pub(crate) FileEncryptionVersion);

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFileMeta<'a> {
	#[serde(borrow)]
	pub(super) name: Cow<'a, str>,
	pub(super) size: u64,
	#[serde(borrow)]
	pub(super) mime: Cow<'a, str>,
	#[serde(borrow)]
	pub(super) key: Cow<'a, str>,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	pub(super) last_modified: DateTime<Utc>,
	#[serde(with = "filen_types::serde::time::optional")]
	#[serde(rename = "creation")]
	#[serde(default)]
	pub(super) created: Option<DateTime<Utc>>,
	#[serde(default, with = "empty_hash_is_none", rename = "blake3")]
	pub(super) hash: Option<Blake3Hash>,
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
			key,
			last_modified: raw_meta.last_modified,
			created: raw_meta.created,
			hash: raw_meta.hash,
		};
		Ok(meta)
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
#[cfg_attr(
	feature = "http-provider",
	derive(serde::Serialize),
	serde(rename_all = "camelCase")
)]
pub enum FileMeta<'a> {
	Decoded(DecryptedFileMeta<'a>),
	DecryptedRaw(Cow<'a, [u8]>),
	DecryptedUTF8(Cow<'a, str>),
	Encrypted(EncryptedString<'a>),
	RSAEncrypted(RSAEncryptedString<'a>),
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

impl<'a> FileMeta<'a> {
	pub fn blocking_from_encrypted(
		meta: EncryptedString<'a>,
		decrypter: &impl MetaCrypter,
		file_encryption_version: FileEncryptionVersion,
	) -> Self {
		let Ok(decrypted) = decrypter.blocking_decrypt_meta(&meta) else {
			return Self::Encrypted(meta);
		};
		let seed = FileMetaSeed(file_encryption_version);
		let Ok(meta) = seed.deserialize(&mut serde_json::Deserializer::from_str(&decrypted)) else {
			if let Some(meta) =
				Self::retry_with_sanitized_surrogates(&decrypted, file_encryption_version)
			{
				return meta;
			}
			return Self::DecryptedUTF8(Cow::Owned(decrypted));
		};
		Self::Decoded(meta.into_owned_cow())
	}

	pub fn blocking_from_rsa_encrypted(
		meta: RSAEncryptedString<'a>,
		private_key: &RsaPrivateKey,
		file_encryption_version: FileEncryptionVersion,
	) -> Self {
		let Ok(decrypted) = crypto::rsa::blocking_decrypt_with_private_key(private_key, &meta)
		else {
			return Self::RSAEncrypted(meta);
		};
		let seed = FileMetaSeed(file_encryption_version);
		let Ok(meta) = seed.deserialize(&mut serde_json::Deserializer::from_slice(&decrypted))
		else {
			match String::from_utf8(decrypted) {
				Ok(decrypted) => {
					if let Some(meta) =
						Self::retry_with_sanitized_surrogates(&decrypted, file_encryption_version)
					{
						return meta;
					}
					return Self::DecryptedUTF8(Cow::Owned(decrypted));
				}
				Err(err) => {
					let latin1 = meta_recovery::latin1_to_string(err.as_bytes());
					let seed = FileMetaSeed(file_encryption_version);
					return match seed.deserialize(&mut serde_json::Deserializer::from_str(&latin1))
					{
						Ok(meta) => Self::Decoded(meta.into_owned_cow()),
						Err(_) => {
							Self::retry_with_sanitized_surrogates(&latin1, file_encryption_version)
								.unwrap_or_else(|| Self::DecryptedRaw(Cow::Owned(err.into_bytes())))
						}
					};
				}
			}
		};
		Self::Decoded(meta.into_owned_cow())
	}

	/// Retries a failed metadata JSON parse after replacing unpaired UTF-16
	/// surrogate escapes, which JS clients emit for malformed names and
	/// serde_json rejects.
	fn retry_with_sanitized_surrogates(
		json: &str,
		file_encryption_version: FileEncryptionVersion,
	) -> Option<FileMeta<'static>> {
		let sanitized = meta_recovery::replace_unpaired_surrogate_escapes(json)?;
		let meta = FileMetaSeed(file_encryption_version)
			.deserialize(&mut serde_json::Deserializer::from_str(&sanitized))
			.ok()?;
		Some(FileMeta::Decoded(meta.into_owned_cow()))
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
	get_value_from_decrypted_optional!(hash, Option<Blake3Hash>);
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::fs::meta_recovery::test_support::{TEST_RSA_KEY, latin1_bytes, rsa_encrypt};

	const FILE_META_JSON: &str = r#"{"name":"Résumé.txt","size":3,"mime":"text/plain","key":"12345678901234567890123456789012","lastModified":1719000000000}"#;

	#[test]
	fn rsa_file_metadata_valid_utf8_decodes() {
		let meta = FileMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(FILE_META_JSON.as_bytes()),
			&TEST_RSA_KEY,
			FileEncryptionVersion::V2,
		);
		assert_eq!(meta.name(), Some("Résumé.txt"));
	}

	// Same TS react-native sharer bug as the directory twin: the plaintext
	// arrives Latin-1-encoded instead of UTF-8.
	#[test]
	fn rsa_file_metadata_latin1_recovers_original_name() {
		let meta = FileMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(&latin1_bytes(FILE_META_JSON)),
			&TEST_RSA_KEY,
			FileEncryptionVersion::V2,
		);
		assert_eq!(meta.name(), Some("Résumé.txt"));
		assert_eq!(meta.mime(), Some("text/plain"));
	}

	// Same JS lone-surrogate JSON.stringify output as the directory twin:
	// serde_json rejects the \udXXX escape, so it must be replaced with
	// U+FFFD rather than discarding the whole metadata.
	#[test]
	fn rsa_file_metadata_lone_surrogate_escape_decodes_with_replacement() {
		let json = r#"{"name":"a\ud800b.txt","size":3,"mime":"text/plain","key":"12345678901234567890123456789012","lastModified":1719000000000}"#;
		let meta = FileMeta::blocking_from_rsa_encrypted(
			rsa_encrypt(json.as_bytes()),
			&TEST_RSA_KEY,
			FileEncryptionVersion::V2,
		);
		assert_eq!(meta.name(), Some("a\u{FFFD}b.txt"));
	}
}

#[derive(
	Debug,
	Clone,
	PartialEq,
	Eq,
	Serialize,
	CowHelpers,
	rkyv::Serialize,
	rkyv::Deserialize,
	rkyv::Archive,
)]
#[serde(rename_all = "camelCase")]
pub struct DecryptedFileMeta<'a> {
	#[rkyv(with = AsOwned)]
	pub name: Cow<'a, str>,
	pub size: u64,
	#[rkyv(with = AsOwned)]
	pub mime: Cow<'a, str>,
	pub key: FileKey,
	#[serde(with = "filen_types::serde::time::seconds_or_millis")]
	#[rkyv(with = DateTimeUtcDef)]
	pub last_modified: DateTime<Utc>,
	#[serde(
		with = "filen_types::serde::time::optional",
		rename = "creation",
		default
	)]
	#[rkyv(with = Map<DateTimeUtcDef>)]
	pub created: Option<DateTime<Utc>>,
	#[serde(rename = "blake3")]
	pub hash: Option<Blake3Hash>,
}

impl<'a> DecryptedFileMeta<'a> {
	fn apply_changes(&mut self, changes: FileMetaChanges) {
		if let Some(name) = changes.name {
			// don't need to check for empty name here,
			// because it was already checked in FileMetaChanger::set_name
			self.name = Cow::Owned(name.into());
		}
		if let Some(mime) = changes.mime {
			self.mime = Cow::Owned(mime);
		}
		if let Some(last_modified) = changes.last_modified {
			self.last_modified = last_modified;
		}
		#[cfg(not(feature = "uniffi"))]
		{
			if let Some(created) = changes.created {
				self.created = created;
			}
		}

		#[cfg(feature = "uniffi")]
		{
			self.created = match changes.created {
				CreatedTime::Keep => self.created,
				CreatedTime::Unset => None,
				CreatedTime::Set(t) => Some(t),
			};
		}
	}

	pub fn borrowed_with_changes(&'a self, changes: &'a FileMetaChanges) -> Self {
		let created = {
			#[cfg(feature = "uniffi")]
			{
				match &changes.created {
					CreatedTime::Keep => self.created,
					CreatedTime::Unset => None,
					CreatedTime::Set(t) => Some(*t),
				}
			}
			#[cfg(not(feature = "uniffi"))]
			{
				changes.created.unwrap_or(self.created)
			}
		};
		Self {
			name: if let Some(name) = &changes.name {
				Cow::Borrowed(name.as_ref())
			} else {
				Cow::Borrowed(self.name.as_ref())
			},
			mime: Cow::Borrowed(changes.mime.as_deref().unwrap_or(&self.mime)),
			last_modified: changes.last_modified.unwrap_or(self.last_modified),
			created,
			key: self.key,
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

	pub fn hash(&self) -> Option<Blake3Hash> {
		self.hash
	}

	pub fn size(&self) -> u64 {
		self.size
	}

	pub fn key(&self) -> &FileKey {
		&self.key
	}
}

#[cfg(feature = "uniffi")]
#[derive(Debug, PartialEq, Eq, Clone, Default, Deserialize, uniffi::Enum)]
pub enum CreatedTime {
	#[default]
	Keep,
	Unset,
	Set(DateTime<Utc>),
}

#[derive(Default)]
#[js_type(import)]
pub struct FileMetaChanges {
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	name: Option<ValidatedName>,
	#[cfg_attr(feature = "wasm-full", tsify(type = "string"), serde(default))]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	mime: Option<String>,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(default, with = "filen_types::serde::time::optional")
	)]
	#[cfg_attr(feature = "uniffi", uniffi(default = None))]
	last_modified: Option<DateTime<Utc>>,
	#[cfg(not(feature = "uniffi"))]
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint | null"),
		serde(
			default,
			deserialize_with = "crate::serde::deserialize_double_option_timestamp"
		)
	)]
	created: Option<Option<DateTime<Utc>>>,
	#[cfg(feature = "uniffi")]
	created: CreatedTime,
}

impl FileMetaChanges {
	pub fn name(mut self, name: &str) -> Result<Self, EntryNameError> {
		let name = ValidatedName::try_from(name)?;
		if self.mime.is_none() {
			self.mime = Some(make_mime(name.as_ref(), None));
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
		#[cfg(feature = "uniffi")]
		{
			self.created = match created {
				Some(t) => CreatedTime::Set(t.round_subsecs(3)),
				None => CreatedTime::Unset,
			};
		}
		#[cfg(not(feature = "uniffi"))]
		{
			self.created = Some(created.map(|t| t.round_subsecs(3)));
		}
		self
	}
}

#[cfg(feature = "http-provider")]
pub mod serde_stateless {
	use std::borrow::Cow;

	use chrono::{DateTime, Utc};
	use filen_types::{
		auth::FileEncryptionVersion,
		crypto::{Blake3Hash, EncryptedString, rsa::RSAEncryptedString},
	};
	use serde::{Deserialize, Deserializer, Serialize, Serializer};

	use crate::crypto::file::FileKey;

	use super::{DecryptedFileMeta, FileMeta};

	#[derive(Serialize)]
	#[serde(rename_all = "camelCase")]
	struct DecodedSerHelper<'a> {
		name: &'a str,
		size: u64,
		mime: &'a str,
		key_version: FileEncryptionVersion,
		key: &'a FileKey,
		#[serde(with = "chrono::serde::ts_milliseconds")]
		last_modified: DateTime<Utc>,
		#[serde(with = "filen_types::serde::time::optional")]
		#[serde(rename = "creation")]
		#[serde(default)]
		created: Option<DateTime<Utc>>,
		#[serde(default, with = "super::empty_hash_is_none", rename = "blake3")]
		hash: Option<Blake3Hash>,
	}

	#[derive(Serialize)]
	#[serde(rename_all = "camelCase")]
	enum FileMetaSerHelper<'a> {
		Decoded(DecodedSerHelper<'a>),
		DecryptedRaw(&'a [u8]),
		DecryptedUTF8(&'a str),
		Encrypted(&'a EncryptedString<'a>),
		RSAEncrypted(&'a RSAEncryptedString<'a>),
	}

	#[derive(Deserialize)]
	#[serde(rename_all = "camelCase")]
	struct DecodedDeserHelper {
		name: String,
		size: u64,
		mime: String,
		key_version: FileEncryptionVersion,
		key: String,
		#[serde(with = "chrono::serde::ts_milliseconds")]
		last_modified: DateTime<Utc>,
		#[serde(with = "filen_types::serde::time::optional")]
		#[serde(rename = "creation")]
		#[serde(default)]
		created: Option<DateTime<Utc>>,
		#[serde(default, with = "super::empty_hash_is_none", rename = "blake3")]
		hash: Option<Blake3Hash>,
	}

	#[derive(Deserialize)]
	#[serde(rename_all = "camelCase")]
	enum FileMetaDeserHelper {
		Decoded(DecodedDeserHelper),
		DecryptedRaw(Vec<u8>),
		DecryptedUTF8(String),
		Encrypted(EncryptedString<'static>),
		RSAEncrypted(RSAEncryptedString<'static>),
	}

	pub fn serialize<S>(meta: &FileMeta<'_>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		let helper = match meta {
			FileMeta::Decoded(m) => FileMetaSerHelper::Decoded(DecodedSerHelper {
				name: m.name.as_ref(),
				size: m.size,
				mime: m.mime.as_ref(),
				key_version: m.key.version(),
				key: &m.key,
				last_modified: m.last_modified,
				created: m.created,
				hash: m.hash,
			}),
			FileMeta::DecryptedRaw(b) => FileMetaSerHelper::DecryptedRaw(b.as_ref()),
			FileMeta::DecryptedUTF8(s) => FileMetaSerHelper::DecryptedUTF8(s.as_ref()),
			FileMeta::Encrypted(e) => FileMetaSerHelper::Encrypted(e),
			FileMeta::RSAEncrypted(e) => FileMetaSerHelper::RSAEncrypted(e),
		};
		helper.serialize(serializer)
	}

	pub fn deserialize<'de, D>(deserializer: D) -> Result<FileMeta<'static>, D::Error>
	where
		D: Deserializer<'de>,
	{
		let helper = FileMetaDeserHelper::deserialize(deserializer)?;
		match helper {
			FileMetaDeserHelper::Decoded(h) => {
				let key = FileKey::from_str_with_version(&h.key, h.key_version)
					.map_err(serde::de::Error::custom)?;
				Ok(FileMeta::Decoded(DecryptedFileMeta {
					name: Cow::Owned(h.name),
					size: h.size,
					mime: Cow::Owned(h.mime),
					key,
					last_modified: h.last_modified,
					created: h.created,
					hash: h.hash,
				}))
			}
			FileMetaDeserHelper::DecryptedRaw(b) => Ok(FileMeta::DecryptedRaw(Cow::Owned(b))),
			FileMetaDeserHelper::DecryptedUTF8(s) => Ok(FileMeta::DecryptedUTF8(Cow::Owned(s))),
			FileMetaDeserHelper::Encrypted(e) => Ok(FileMeta::Encrypted(e)),
			FileMetaDeserHelper::RSAEncrypted(e) => Ok(FileMeta::RSAEncrypted(e)),
		}
	}
}

pub(super) mod empty_hash_is_none {
	use std::borrow::Cow;

	use filen_types::crypto::Blake3Hash;
	use serde::{Deserialize, de::IntoDeserializer};
	#[cfg(feature = "http-provider")]
	use serde::{Serialize, Serializer};

	// Only reachable through the http-provider-gated `serde_stateless::DecodedSerHelper`.
	#[cfg(feature = "http-provider")]
	pub(crate) fn serialize<S>(value: &Option<Blake3Hash>, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		if let Some(value) = value {
			Blake3Hash::serialize(value, serializer)
		} else {
			serializer.serialize_none()
		}
	}

	pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Blake3Hash>, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let option = Option::<Cow<str>>::deserialize(deserializer)?;
		match option {
			Some(cow) if !cow.is_empty() => {
				let hash = Blake3Hash::deserialize(cow.into_deserializer())?;
				Ok(Some(hash))
			}
			_ => Ok(None),
		}
	}
}
