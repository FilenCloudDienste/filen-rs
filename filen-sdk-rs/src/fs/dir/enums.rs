use std::borrow::Cow;

use filen_types::fs::ObjectType;

use crate::fs::{HasMeta, HasName, HasRemoteInfo, HasType, HasUUID};

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
	fn uuid(&self) -> uuid::Uuid {
		match self {
			DirectoryType::Root(dir) => dir.uuid(),
			DirectoryType::Dir(dir) => dir.uuid(),
			DirectoryType::RootWithMeta(dir) => dir.uuid(),
		}
	}
}
impl HasContents for DirectoryType<'_> {}

impl HasType for DirectoryType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryMetaType<'a> {
	Root(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl HasUUID for DirectoryMetaType<'_> {
	fn uuid(&self) -> uuid::Uuid {
		match self {
			DirectoryMetaType::Root(dir) => dir.uuid(),
			DirectoryMetaType::Dir(dir) => dir.uuid(),
		}
	}
}

impl HasContents for DirectoryMetaType<'_> {}

impl HasType for DirectoryMetaType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

impl HasName for DirectoryMetaType<'_> {
	fn name(&self) -> &str {
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
}

impl HasMeta for DirectoryMetaType<'_> {
	fn get_meta_string(&self) -> String {
		match self {
			DirectoryMetaType::Root(dir) => dir.get_meta_string(),
			DirectoryMetaType::Dir(dir) => dir.get_meta_string(),
		}
	}
}

impl HasDirMeta for DirectoryMetaType<'_> {
	fn borrow_meta(&self) -> super::DirectoryMeta<'_> {
		match self {
			DirectoryMetaType::Root(dir) => dir.borrow_meta(),
			DirectoryMetaType::Dir(dir) => dir.borrow_meta(),
		}
	}

	fn get_meta(&self) -> super::DirectoryMeta<'static> {
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
