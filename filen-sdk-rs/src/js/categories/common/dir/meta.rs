use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::crypto::{EncryptedString, rsa::RSAEncryptedString};

use crate::fs::dir::{DecryptedDirectoryMeta as DecryptedDirectoryMetaRS, meta::DirectoryMeta};

#[js_type]
pub struct DecryptedDirMeta {
	pub name: String,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(
			with = "filen_types::serde::time::optional",
			skip_serializing_if = "Option::is_none",
			default
		)
	)]
	pub created: Option<DateTime<Utc>>,
}

impl From<DecryptedDirectoryMetaRS<'_>> for DecryptedDirMeta {
	fn from(meta: DecryptedDirectoryMetaRS) -> Self {
		DecryptedDirMeta {
			name: meta.name.into_owned(),
			created: meta.created,
		}
	}
}

impl From<DecryptedDirMeta> for DecryptedDirectoryMetaRS<'static> {
	fn from(meta: DecryptedDirMeta) -> Self {
		DecryptedDirectoryMetaRS {
			name: Cow::Owned(meta.name),
			created: meta.created,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	// we have to set content due to:
	// https://github.com/serde-rs/serde/issues/1307
	serde(tag = "type", content = "data", rename_all = "camelCase"),
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum DirMeta {
	Decoded(DecryptedDirMeta),
	DecryptedRaw(Vec<u8>),
	DecryptedUTF8(String),
	Encrypted(String),
	RSAEncrypted(String),
}

impl From<DirectoryMeta<'_>> for DirMeta {
	fn from(meta: DirectoryMeta) -> Self {
		match meta {
			DirectoryMeta::Decoded(meta) => DirMeta::Decoded(meta.into()),
			DirectoryMeta::DecryptedRaw(meta) => DirMeta::DecryptedRaw(meta.into_owned()),
			DirectoryMeta::DecryptedUTF8(meta) => DirMeta::DecryptedUTF8(meta.into_owned()),
			DirectoryMeta::Encrypted(meta) => DirMeta::Encrypted(meta.0.into_owned()),
			DirectoryMeta::RSAEncrypted(meta) => DirMeta::RSAEncrypted(meta.0.into_owned()),
		}
	}
}

impl From<DirMeta> for DirectoryMeta<'static> {
	fn from(meta: DirMeta) -> Self {
		match meta {
			DirMeta::Decoded(meta) => DirectoryMeta::Decoded(meta.into()),
			DirMeta::DecryptedRaw(meta) => DirectoryMeta::DecryptedRaw(Cow::Owned(meta)),
			DirMeta::DecryptedUTF8(meta) => DirectoryMeta::DecryptedUTF8(Cow::Owned(meta)),
			DirMeta::Encrypted(meta) => DirectoryMeta::Encrypted(EncryptedString(Cow::Owned(meta))),
			DirMeta::RSAEncrypted(meta) => {
				DirectoryMeta::RSAEncrypted(RSAEncryptedString(Cow::Owned(meta)))
			}
		}
	}
}
