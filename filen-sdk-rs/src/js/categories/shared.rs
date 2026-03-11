use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::UuidStr;

use crate::{
	connect::fs::{
		SharedDirInfo, SharedDirectory, SharedRootDirectory, SharedRootFile as SharedRootFileRS,
		SharingRole,
	},
	crypto::error::ConversionError,
	fs::{
		categories::{DirType, RootItemType, Shared},
		file::RemoteRootFile,
	},
	js::{
		File, FileMeta,
		categories::{CategoryJSExt, common::dir::RootDirWithMeta},
	},
};

impl CategoryJSExt for Shared {
	type RootJS = SharedRootDir;
	type DirJS = SharedDir;
	type FileJS = File;
	type RootFileJS = SharedFile;
}

#[js_type(export)]
pub struct SharedRootDir {
	inner: RootDirWithMeta,
	sharing_role: SharingRole,
	write_access: bool,
}

impl From<SharedRootDirectory> for SharedRootDir {
	fn from(value: SharedRootDirectory) -> Self {
		Self {
			inner: value.dir.into(),
			sharing_role: value.info.sharing_role,
			write_access: value.info.write_access,
		}
	}
}

impl From<SharedRootDir> for SharedRootDirectory {
	fn from(value: SharedRootDir) -> Self {
		Self {
			dir: value.inner.into(),
			info: SharedDirInfo {
				sharing_role: value.sharing_role,
				write_access: value.write_access,
			},
		}
	}
}

#[js_type]
pub struct SharedDir {
	inner: super::normal::Dir,
	__shared_tag: bool,
}

impl From<SharedDirectory> for SharedDir {
	fn from(value: SharedDirectory) -> Self {
		Self {
			__shared_tag: true,
			inner: value.inner.into(),
		}
	}
}

impl From<SharedDir> for SharedDirectory {
	fn from(value: SharedDir) -> Self {
		Self {
			inner: value.inner.into(),
		}
	}
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
#[js_type(export, wasm_all, no_deser, no_ser)]
pub struct SharedFile {
	uuid: UuidStr,
	size: u64,
	region: String,
	bucket: String,
	chunks: u64,
	#[serde(with = "chrono::serde::ts_milliseconds")]
	#[cfg_attr(target_family = "wasm", tsify(type = "bigint"))]
	timestamp: DateTime<Utc>,
	meta: FileMeta,
	sharing_role: SharingRole,
	__shared_tag: bool,
}

impl From<SharedRootFileRS> for SharedFile {
	fn from(value: SharedRootFileRS) -> Self {
		Self {
			__shared_tag: true,
			uuid: value.file.uuid,
			size: value.file.size,
			region: value.file.region,
			bucket: value.file.bucket,
			chunks: value.file.chunks,
			timestamp: value.file.timestamp,
			meta: value.file.meta.into(),
			sharing_role: value.sharing_role,
		}
	}
}

impl TryFrom<SharedFile> for SharedRootFileRS {
	type Error = ConversionError;

	fn try_from(value: SharedFile) -> Result<Self, Self::Error> {
		Ok(Self {
			file: RemoteRootFile {
				uuid: value.uuid,
				size: value.size,
				region: value.region,
				bucket: value.bucket,
				chunks: value.chunks,
				timestamp: value.timestamp,
				meta: value.meta.try_into()?,
			},
			sharing_role: value.sharing_role,
		})
	}
}

#[js_type(import)]
pub enum SharedRootItem {
	Dir(SharedRootDir),
	File(SharedFile),
}

impl TryFrom<SharedRootItem> for RootItemType<'static, Shared> {
	type Error = ConversionError;
	fn try_from(value: SharedRootItem) -> Result<Self, Self::Error> {
		Ok(match value {
			SharedRootItem::Dir(dir) => Self::Dir(Cow::Owned(dir.into())),
			SharedRootItem::File(file) => Self::File(Cow::Owned(file.try_into()?)),
		})
	}
}

impl From<RootItemType<'static, Shared>> for SharedRootItem {
	fn from(value: RootItemType<'static, Shared>) -> Self {
		match value {
			RootItemType::Dir(dir) => Self::Dir(dir.into_owned().into()),
			RootItemType::File(file) => Self::File(file.into_owned().into()),
		}
	}
}

#[js_type(import, export)]
pub enum AnySharedDir {
	Dir(SharedDir),
	Root(SharedRootDir),
}

impl From<AnySharedDir> for DirType<'static, Shared> {
	fn from(value: AnySharedDir) -> Self {
		match value {
			AnySharedDir::Root(dir) => Self::Root(Cow::Owned(dir.into())),
			AnySharedDir::Dir(dir) => Self::Dir(Cow::Owned(dir.into())),
		}
	}
}

impl From<DirType<'static, Shared>> for AnySharedDir {
	fn from(value: DirType<'static, Shared>) -> Self {
		match value {
			DirType::Root(dir) => Self::Root(dir.into_owned().into()),
			DirType::Dir(dir) => Self::Dir(dir.into_owned().into()),
		}
	}
}
