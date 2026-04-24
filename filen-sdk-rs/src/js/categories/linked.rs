use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::{
	api::v3::dir::link::PublicLinkExpiration,
	auth::{FileEncryptionVersion, MetaEncryptionVersion},
	crypto::MaybeEncrypted,
	fs::UuidStr,
};

use crate::{
	Error,
	auth::MetaKey,
	connect::PasswordState,
	crypto::{error::ConversionError, file::FileKey},
	error::ResultExt,
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
			file_key: value.file_key.to_string(),
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

#[js_type(wasm_all)]
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

#[js_type(wasm_all)]
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

#[js_type(import, export, wasm_all)]
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

#[js_type(import, export, wasm_all)]
/// Converting from a DirPublicLinkRW to a DirPublicLink IS allowed
/// if all fields are compatible
/// but converting from a DirPublicLink to a DirPublicLinkRW is not allowed
pub struct DirPublicLink {
	link_uuid: UuidStr,
	link_key: String,
	link_key_version: u8,
	password: Option<String>,
	enable_download: bool,
	salt: Option<Vec<u8>>,
}

impl From<crate::connect::DirPublicLink> for DirPublicLink {
	fn from(value: crate::connect::DirPublicLink) -> Self {
		Self {
			link_uuid: value.link_uuid,
			link_key: value.link_key.to_string(),
			link_key_version: value.link_key.version() as u8,
			password: value.password,
			enable_download: value.enable_download,
			salt: value.salt,
		}
	}
}

impl TryFrom<DirPublicLink> for crate::connect::DirPublicLink {
	type Error = Error;

	fn try_from(value: DirPublicLink) -> Result<Self, Self::Error> {
		Ok(Self {
			link_uuid: value.link_uuid,
			link_key: MetaKey::from_str_and_version(
				&value.link_key,
				MetaEncryptionVersion::try_from(value.link_key_version)
					.context("DirPublicLink from JS")?,
			)
			.context("DirPublicLink from JS")?,
			password: value.password,
			enable_download: value.enable_download,
			salt: value.salt,
		})
	}
}

#[js_type(import, export, wasm_all)]
/// Converting from a DirPublicLinkRW to a DirPublicLink IS allowed
/// if all fields are compatible
/// but converting from a DirPublicLink to a DirPublicLinkRW is not allowed
pub struct DirPublicLinkRW {
	link_uuid: UuidStr,
	link_key: Option<String>,
	link_key_version: Option<u8>,
	password: PasswordState,
	expiration: PublicLinkExpiration,
	enable_download: bool,
	salt: Option<Vec<u8>>,
}

impl From<crate::connect::DirPublicLinkRW> for DirPublicLinkRW {
	fn from(value: crate::connect::DirPublicLinkRW) -> Self {
		Self {
			link_uuid: value.link_uuid,
			link_key_version: value.link_key.as_ref().map(|k| k.version() as u8),
			link_key: value.link_key.map(|k| k.to_string()),
			password: value.password,
			expiration: value.expiration,
			enable_download: value.enable_download,
			salt: value.salt,
		}
	}
}

impl TryFrom<DirPublicLinkRW> for crate::connect::DirPublicLinkRW {
	type Error = Error;

	fn try_from(value: DirPublicLinkRW) -> Result<Self, Self::Error> {
		Ok(Self {
			link_uuid: value.link_uuid,
			link_key: match (value.link_key, value.link_key_version) {
				(Some(k), Some(v)) => Some(
					MetaKey::from_str_and_version(
						&k,
						MetaEncryptionVersion::try_from(v).context("DirPublicLinkRW from JS")?,
					)
					.context("DirPublicLinkRW from JS")?,
				),
				_ => None,
			},
			password: value.password,
			expiration: value.expiration,
			enable_download: value.enable_download,
			salt: value.salt,
		})
	}
}
