use std::borrow::Cow;

use filen_types::fs::{ObjectType, UuidStr};

use crate::{
	connect::fs::SharedDirectory,
	fs::{
		HasMeta, HasName, HasRemoteInfo, HasType, HasUUID, UnsharedFSObject,
		dir::meta::DirectoryMeta,
	},
};

use super::{
	HasContents, RemoteDirectory, RootDirectory, RootDirectoryWithMeta,
	traits::{HasDirInfo, HasDirMeta},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	RootWithMeta(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl HasUUID for DirectoryType<'_> {
	fn uuid(&self) -> &UuidStr {
		match self {
			DirectoryType::Root(dir) => dir.uuid(),
			DirectoryType::Dir(dir) => dir.uuid(),
			DirectoryType::RootWithMeta(dir) => dir.uuid(),
		}
	}
}
impl HasContents for DirectoryType<'_> {
	fn uuid_as_parent(&self) -> filen_types::fs::ParentUuid {
		match self {
			DirectoryType::Root(dir) => dir.uuid_as_parent(),
			DirectoryType::Dir(dir) => dir.uuid_as_parent(),
			DirectoryType::RootWithMeta(dir) => dir.uuid_as_parent(),
		}
	}
}

impl HasType for DirectoryType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl From<RootDirectory> for DirectoryType<'static> {
	fn from(dir: RootDirectory) -> Self {
		DirectoryType::Root(Cow::Owned(dir))
	}
}
impl From<RootDirectoryWithMeta> for DirectoryType<'static> {
	fn from(dir: RootDirectoryWithMeta) -> Self {
		DirectoryType::RootWithMeta(Cow::Owned(dir))
	}
}
impl From<RemoteDirectory> for DirectoryType<'static> {
	fn from(dir: RemoteDirectory) -> Self {
		DirectoryType::Dir(Cow::Owned(dir))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryTypeWithShareInfo<'a> {
	Root(Cow<'a, RootDirectory>),
	SharedDir(Cow<'a, SharedDirectory>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl<'a> From<&'a RootDirectory> for DirectoryTypeWithShareInfo<'a> {
	fn from(dir: &'a RootDirectory) -> Self {
		DirectoryTypeWithShareInfo::Root(Cow::Borrowed(dir))
	}
}

impl<'a> From<&'a SharedDirectory> for DirectoryTypeWithShareInfo<'a> {
	fn from(dir: &'a SharedDirectory) -> Self {
		DirectoryTypeWithShareInfo::SharedDir(Cow::Borrowed(dir))
	}
}

impl<'a> From<&'a RemoteDirectory> for DirectoryTypeWithShareInfo<'a> {
	fn from(dir: &'a RemoteDirectory) -> Self {
		DirectoryTypeWithShareInfo::Dir(Cow::Borrowed(dir))
	}
}

impl<'a> From<&'a DirectoryTypeWithShareInfo<'_>> for DirectoryTypeWithShareInfo<'a> {
	fn from(dir: &'a DirectoryTypeWithShareInfo<'_>) -> Self {
		match dir {
			DirectoryTypeWithShareInfo::Root(dir) => {
				DirectoryTypeWithShareInfo::Root(Cow::Borrowed(dir))
			}
			DirectoryTypeWithShareInfo::SharedDir(dir) => {
				DirectoryTypeWithShareInfo::SharedDir(Cow::Borrowed(dir))
			}
			DirectoryTypeWithShareInfo::Dir(dir) => {
				DirectoryTypeWithShareInfo::Dir(Cow::Borrowed(dir))
			}
		}
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UnsharedDirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl<'a> From<UnsharedDirectoryType<'a>> for DirectoryType<'a> {
	fn from(dir: UnsharedDirectoryType<'a>) -> Self {
		match dir {
			UnsharedDirectoryType::Root(dir) => DirectoryType::Root(dir),
			UnsharedDirectoryType::Dir(dir) => DirectoryType::Dir(dir),
		}
	}
}

impl HasUUID for UnsharedDirectoryType<'_> {
	fn uuid(&self) -> &UuidStr {
		match self {
			UnsharedDirectoryType::Root(dir) => dir.uuid(),
			UnsharedDirectoryType::Dir(dir) => dir.uuid(),
		}
	}
}
impl HasContents for UnsharedDirectoryType<'_> {
	fn uuid_as_parent(&self) -> filen_types::fs::ParentUuid {
		match self {
			UnsharedDirectoryType::Root(dir) => dir.uuid_as_parent(),
			UnsharedDirectoryType::Dir(dir) => dir.uuid_as_parent(),
		}
	}
}

impl HasType for UnsharedDirectoryType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl<'a> From<UnsharedDirectoryType<'a>> for UnsharedFSObject<'a> {
	fn from(dir: UnsharedDirectoryType<'a>) -> Self {
		match dir {
			UnsharedDirectoryType::Root(dir) => UnsharedFSObject::Root(dir),
			UnsharedDirectoryType::Dir(dir) => UnsharedFSObject::Dir(dir),
		}
	}
}

impl From<RemoteDirectory> for UnsharedDirectoryType<'static> {
	fn from(dir: RemoteDirectory) -> Self {
		UnsharedDirectoryType::Dir(Cow::Owned(dir))
	}
}

impl From<RootDirectory> for UnsharedDirectoryType<'static> {
	fn from(dir: RootDirectory) -> Self {
		UnsharedDirectoryType::Root(Cow::Owned(dir))
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryMetaType<'a> {
	Root(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl HasUUID for DirectoryMetaType<'_> {
	fn uuid(&self) -> &UuidStr {
		match self {
			DirectoryMetaType::Root(dir) => dir.uuid(),
			DirectoryMetaType::Dir(dir) => dir.uuid(),
		}
	}
}

impl HasContents for DirectoryMetaType<'_> {
	fn uuid_as_parent(&self) -> filen_types::fs::ParentUuid {
		match self {
			DirectoryMetaType::Root(dir) => dir.uuid_as_parent(),
			DirectoryMetaType::Dir(dir) => dir.uuid_as_parent(),
		}
	}
}

impl HasType for DirectoryMetaType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for DirectoryMetaType<'_> {
	fn name(&self) -> Option<&str> {
		match self {
			DirectoryMetaType::Root(dir) => dir.name(),
			DirectoryMetaType::Dir(dir) => dir.name(),
		}
	}
}

impl HasDirInfo for DirectoryMetaType<'_> {
	fn created(&self) -> Option<chrono::DateTime<chrono::Utc>> {
		match self {
			DirectoryMetaType::Root(dir) => dir.created(),
			DirectoryMetaType::Dir(dir) => dir.created(),
		}
	}
}

impl HasRemoteInfo for DirectoryMetaType<'_> {
	fn favorited(&self) -> bool {
		match self {
			DirectoryMetaType::Root(dir) => dir.favorited(),
			DirectoryMetaType::Dir(dir) => dir.favorited(),
		}
	}

	fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
		match self {
			DirectoryMetaType::Root(dir) => dir.timestamp(),
			DirectoryMetaType::Dir(dir) => dir.timestamp(),
		}
	}
}

impl HasMeta for DirectoryMetaType<'_> {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		match self {
			DirectoryMetaType::Root(dir) => dir.get_meta_string(),
			DirectoryMetaType::Dir(dir) => dir.get_meta_string(),
		}
	}
}

impl HasDirMeta for DirectoryMetaType<'_> {
	fn get_meta(&self) -> &DirectoryMeta<'_> {
		match self {
			DirectoryMetaType::Root(dir) => dir.get_meta(),
			DirectoryMetaType::Dir(dir) => dir.get_meta(),
		}
	}
}

impl From<RemoteDirectory> for DirectoryMetaType<'static> {
	fn from(dir: RemoteDirectory) -> Self {
		DirectoryMetaType::Dir(Cow::Owned(dir))
	}
}

impl From<RootDirectoryWithMeta> for DirectoryMetaType<'static> {
	fn from(dir: RootDirectoryWithMeta) -> Self {
		DirectoryMetaType::Root(Cow::Owned(dir))
	}
}

impl<'a> From<&'a RemoteDirectory> for DirectoryMetaType<'a> {
	fn from(dir: &'a RemoteDirectory) -> Self {
		DirectoryMetaType::Dir(Cow::Borrowed(dir))
	}
}
impl<'a> From<&'a RootDirectoryWithMeta> for DirectoryMetaType<'a> {
	fn from(dir: &'a RootDirectoryWithMeta) -> Self {
		DirectoryMetaType::Root(Cow::Borrowed(dir))
	}
}
