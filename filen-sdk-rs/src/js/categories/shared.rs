use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	connect::fs::{
		SharedDirInfo, SharedDirectory, SharedInItem as SharedInItemRS, SharedRootDirectory,
		SharedRootFile as SharedRootFileRS, SharingRole,
	},
	crypto::error::ConversionError,
	fs::{
		HasName,
		categories::{DirType, RootItemType, Shared},
		file::{RemoteRootFile, traits::HasFileInfo},
	},
	js::{File, FileMeta, categories::common::dir::RootDirWithMeta},
	thumbnail::might_be_thumbnailable,
};

#[js_type(import, export, wasm_all, napi)]
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

#[js_type(import, export, wasm_all, napi)]
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

#[js_type(import, export, wasm_all, napi)]
pub struct SharedFile {
	uuid: Uuid,
	size: u64,
	region: String,
	bucket: String,
	chunks: u64,
	#[cfg_attr(
		target_family = "wasm",
		serde(with = "chrono::serde::ts_milliseconds"),
		tsify(type = "bigint")
	)]
	timestamp: DateTime<Utc>,
	meta: FileMeta,
	sharing_role: SharingRole,
	__shared_tag: bool,
	// JS only field, indicates if the file can have a thumbnail generated;
	// the same gate `File.canMakeThumbnail` answers, so a caller never has to
	// call into WASM to check the name.
	can_make_thumbnail: bool,
}

impl From<SharedRootFileRS> for SharedFile {
	fn from(value: SharedRootFileRS) -> Self {
		Self {
			can_make_thumbnail: might_be_thumbnailable(value.name(), value.mime()),
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

#[js_type(import, export, wasm_all, napi)]
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

/// FFI mirror of [`SharedInItemRS`]: an item shared with this account, as `getSharedInItem` finds
/// it by uuid, in the shapes the shared listings return, with the sharer beside the ones that do
/// not hold it.
#[js_type(export, no_deser, tagged, camel_case_fields)]
pub enum SharedInItem {
	/// A folder shared with this account directly, as `listInShared` lists it. Its `writeAccess`
	/// is the share's stored value, which nothing enforces: receivers cannot change shared items.
	RootDir { dir: SharedRootDir },
	/// A file shared with this account directly, as `listInShared` lists it.
	RootFile { file: SharedFile },
	/// A folder inside a shared folder, as `listSharedDir` lists it.
	Dir {
		dir: SharedDir,
		/// The account that shared it, as `listSharedDir` takes it.
		role: SharingRole,
	},
	/// A file inside a shared folder, as `listSharedDir` lists it.
	File {
		file: File,
		/// The account that shared it.
		role: SharingRole,
	},
}

impl From<SharedInItemRS> for SharedInItem {
	fn from(value: SharedInItemRS) -> Self {
		match value {
			SharedInItemRS::RootDir(dir) => Self::RootDir { dir: dir.into() },
			SharedInItemRS::RootFile(file) => Self::RootFile { file: file.into() },
			SharedInItemRS::Dir { dir, role } => Self::Dir {
				dir: dir.into(),
				role,
			},
			SharedInItemRS::File { file, role } => Self::File {
				file: file.into(),
				role,
			},
		}
	}
}
