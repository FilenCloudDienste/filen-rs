use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{auth::FileEncryptionVersion, crypto::MaybeEncrypted, fs::UuidStr};

use crate::{
	crypto::{error::ConversionError, file::FileKey},
	fs::{
		categories::{DirType, Linked},
		dir::{LinkedDirectory, RootDirectoryWithMeta},
		file::LinkedFile as LinkedFileRS,
	},
	js::{File, categories::CategoryJSExt},
};

use super::common::dir::RootDirWithMeta;

impl CategoryJSExt for Linked {
	type RootJS = LinkedRootDir;
	type DirJS = LinkedDir;
	type FileJS = File;
	type RootFileJS = LinkedFile;
}

#[js_type(wasm_all)]
pub struct LinkedFile {
	uuid: UuidStr,
	name: MaybeEncrypted<'static, str>,
	mime: MaybeEncrypted<'static, str>,
	size: u64,
	chunks: u64,
	region: String,
	bucket: String,
	version: FileEncryptionVersion,
	#[cfg_attr(
		target_family = "wasm",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	timestamp: DateTime<Utc>,
	file_key: String,
	__linked_tag: bool,
}

impl From<LinkedFileRS> for LinkedFile {
	fn from(value: LinkedFileRS) -> Self {
		Self {
			uuid: value.uuid,
			name: value.name,
			mime: value.mime,
			size: value.size,
			chunks: value.chunks,
			region: value.region,
			bucket: value.bucket,
			version: value.version,
			timestamp: value.timestamp,
			file_key: value.file_key.to_str().into_owned(),
			__linked_tag: true,
		}
	}
}

impl TryFrom<LinkedFile> for LinkedFileRS {
	type Error = ConversionError;

	fn try_from(value: LinkedFile) -> Result<Self, Self::Error> {
		Ok(Self {
			uuid: value.uuid,
			name: value.name,
			mime: value.mime,
			size: value.size,
			chunks: value.chunks,
			region: value.region,
			bucket: value.bucket,
			version: value.version,
			timestamp: value.timestamp,
			file_key: FileKey::from_string_with_version(Cow::Owned(value.file_key), value.version)?,
		})
	}
}

#[js_type]
pub struct LinkedDir {
	inner: super::normal::Dir,
	__linked_tag: bool,
}

impl From<LinkedDirectory> for LinkedDir {
	fn from(value: LinkedDirectory) -> Self {
		Self {
			inner: value.0.into(),
			__linked_tag: true,
		}
	}
}

impl From<LinkedDir> for LinkedDirectory {
	fn from(value: LinkedDir) -> Self {
		Self(value.inner.into())
	}
}

#[js_type]
pub struct LinkedRootDir {
	inner: RootDirWithMeta,
	__linked_tag: bool,
}

impl From<RootDirectoryWithMeta> for LinkedRootDir {
	fn from(value: RootDirectoryWithMeta) -> Self {
		Self {
			inner: RootDirWithMeta {
				uuid: value.uuid,
				color: value.color.into(),
				timestamp: value.timestamp,
				meta: value.meta.into(),
			},
			__linked_tag: true,
		}
	}
}

impl From<LinkedRootDir> for RootDirectoryWithMeta {
	fn from(value: LinkedRootDir) -> Self {
		Self {
			uuid: value.inner.uuid,
			color: value.inner.color.into(),
			timestamp: value.inner.timestamp,
			meta: value.inner.meta.into(),
		}
	}
}

#[js_type(import, export)]
pub enum AnyLinkedDir {
	Root(LinkedRootDir),
	Dir(LinkedDir),
}

impl From<AnyLinkedDir> for DirType<'static, Linked> {
	fn from(value: AnyLinkedDir) -> Self {
		match value {
			AnyLinkedDir::Root(dir) => Self::Root(Cow::Owned(dir.into())),
			AnyLinkedDir::Dir(dir) => Self::Dir(Cow::Owned(dir.into())),
		}
	}
}

impl From<DirType<'static, Linked>> for AnyLinkedDir {
	fn from(value: DirType<'static, Linked>) -> Self {
		match value {
			DirType::Root(dir) => Self::Root(dir.into_owned().into()),
			DirType::Dir(dir) => Self::Dir(dir.into_owned().into()),
		}
	}
}
