use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	auth::FileEncryptionVersion,
	crypto::{Blake3Hash, EncryptedString, rsa::RSAEncryptedString},
};

use crate::{
	crypto::{error::ConversionError, file::FileKey},
	fs::file::meta::{DecryptedFileMeta as DecryptedFileMetaRs, FileMeta as FileMetaRs},
};

#[js_type(wasm_all)]
pub struct DecryptedFileMeta {
	pub name: String,
	pub mime: String,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(
			with = "filen_types::serde::time::optional",
			skip_serializing_if = "Option::is_none",
			default
		)
	)]
	pub created: Option<DateTime<Utc>>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub modified: DateTime<Utc>,
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "Uint8Array"),
		serde(skip_serializing_if = "Option::is_none", default)
	)]
	pub hash: Option<Blake3Hash>,

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		tsify(type = "bigint")
	)]
	pub size: u64,
	pub key: String,
	pub version: FileEncryptionVersion,
}

impl From<DecryptedFileMetaRs<'_>> for DecryptedFileMeta {
	fn from(meta: DecryptedFileMetaRs) -> Self {
		DecryptedFileMeta {
			name: meta.name.into_owned(),
			mime: meta.mime.into_owned(),
			created: meta.created,
			modified: meta.last_modified,
			hash: meta.hash,
			size: meta.size,
			version: meta.key.version(),
			key: meta.key.to_str().into_owned(),
		}
	}
}

impl TryFrom<DecryptedFileMeta> for DecryptedFileMetaRs<'static> {
	type Error = ConversionError;
	fn try_from(meta: DecryptedFileMeta) -> Result<Self, Self::Error> {
		Ok(DecryptedFileMetaRs {
			name: Cow::Owned(meta.name),
			mime: Cow::Owned(meta.mime),
			created: meta.created,
			last_modified: meta.modified,
			hash: meta.hash,
			size: meta.size,
			key: Cow::Owned(FileKey::from_string_with_version(
				Cow::Owned(meta.key),
				meta.version,
			)?),
		})
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	// we have to set content due to:
	// https://github.com/serde-rs/serde/issues/1307
	serde(tag = "type", content = "data", rename_all = "camelCase"),
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum FileMeta {
	Decoded(DecryptedFileMeta),
	DecryptedUTF8(String),
	DecryptedRaw(Vec<u8>),
	Encrypted(String),
	RSAEncrypted(String),
}

impl From<FileMetaRs<'_>> for FileMeta {
	fn from(meta: FileMetaRs) -> Self {
		match meta {
			FileMetaRs::Decoded(meta) => FileMeta::Decoded(meta.into()),
			FileMetaRs::DecryptedUTF8(meta) => FileMeta::DecryptedUTF8(meta.into_owned()),
			FileMetaRs::DecryptedRaw(meta) => FileMeta::DecryptedRaw(meta.into_owned()),
			FileMetaRs::Encrypted(meta) => FileMeta::Encrypted(meta.0.into_owned()),
			FileMetaRs::RSAEncrypted(meta) => FileMeta::RSAEncrypted(meta.0.into_owned()),
		}
	}
}

impl TryFrom<FileMeta> for FileMetaRs<'static> {
	type Error = ConversionError;
	fn try_from(meta: FileMeta) -> Result<Self, Self::Error> {
		Ok(match meta {
			FileMeta::Decoded(meta) => FileMetaRs::Decoded(meta.try_into()?),
			FileMeta::DecryptedUTF8(meta) => FileMetaRs::DecryptedUTF8(Cow::Owned(meta)),
			FileMeta::DecryptedRaw(meta) => FileMetaRs::DecryptedRaw(Cow::Owned(meta)),
			FileMeta::Encrypted(meta) => FileMetaRs::Encrypted(EncryptedString(Cow::Owned(meta))),
			FileMeta::RSAEncrypted(meta) => {
				FileMetaRs::RSAEncrypted(RSAEncryptedString(Cow::Owned(meta)))
			}
		})
	}
}
