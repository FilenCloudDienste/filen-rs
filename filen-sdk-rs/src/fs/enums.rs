use std::borrow::Cow;

use filen_macros::CowFrom;

use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::fs::file::enums::RemoteFileType;

use super::{
	HasMeta, HasName, HasParent, HasType, HasUUID,
	dir::{DirectoryType, RemoteDirectory, RootDirectory, RootDirectoryWithMeta},
	file::{RemoteFile, RemoteRootFile},
};

#[derive(Debug, Clone, PartialEq, Eq, CowFrom, HasUUID, CowHelpers)]
pub enum UnsharedFSObject<'a> {
	Dir(Cow<'a, RemoteDirectory>),
	Root(Cow<'a, RootDirectory>),
	File(Cow<'a, RemoteFile>),
}

impl<'a> From<UnsharedFSObject<'a>> for FSObject<'a> {
	fn from(item: UnsharedFSObject<'a>) -> Self {
		match item {
			UnsharedFSObject::Dir(dir) => FSObject::Dir(dir),
			UnsharedFSObject::Root(dir) => FSObject::Root(dir),
			UnsharedFSObject::File(file) => FSObject::File(file),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowFrom, CowHelpers)]
pub enum FSObject<'a> {
	Dir(Cow<'a, RemoteDirectory>),
	Root(Cow<'a, RootDirectory>),
	RootWithMeta(Cow<'a, RootDirectoryWithMeta>),
	File(Cow<'a, RemoteFile>),
	SharedFile(Cow<'a, RemoteRootFile>),
}

impl<'a> From<DirectoryType<'a>> for FSObject<'a> {
	fn from(dir: DirectoryType<'a>) -> Self {
		match dir {
			DirectoryType::Root(dir) => FSObject::Root(dir),
			DirectoryType::Dir(dir) => FSObject::Dir(dir),
			DirectoryType::RootWithMeta(cow) => FSObject::RootWithMeta(cow),
		}
	}
}

impl<'a> From<&'a FSObject<'_>> for FSObject<'a> {
	fn from(item: &'a FSObject<'_>) -> Self {
		match item {
			FSObject::Dir(cow) => FSObject::Dir(Cow::Borrowed(cow)),
			FSObject::Root(cow) => FSObject::Root(Cow::Borrowed(cow)),
			FSObject::RootWithMeta(cow) => FSObject::RootWithMeta(Cow::Borrowed(cow)),
			FSObject::File(cow) => FSObject::File(Cow::Borrowed(cow)),
			FSObject::SharedFile(cow) => FSObject::SharedFile(Cow::Borrowed(cow)),
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq, CowFrom, CowHelpers)]
pub(crate) enum FsObjectIntoTypes<'a> {
	Dir(DirectoryType<'a>),
	File(RemoteFileType<'a>),
}

impl<'a> From<FSObject<'a>> for FsObjectIntoTypes<'a> {
	fn from(item: FSObject<'a>) -> Self {
		match item {
			FSObject::Dir(cow) => FsObjectIntoTypes::Dir(DirectoryType::Dir(cow)),
			FSObject::Root(cow) => FsObjectIntoTypes::Dir(DirectoryType::Root(cow)),
			FSObject::RootWithMeta(cow) => FsObjectIntoTypes::Dir(DirectoryType::RootWithMeta(cow)),
			FSObject::File(cow) => FsObjectIntoTypes::File(RemoteFileType::File(cow)),
			FSObject::SharedFile(cow) => FsObjectIntoTypes::File(RemoteFileType::SharedFile(cow)),
		}
	}
}

#[allow(clippy::large_enum_variant)]
#[derive(
	Clone, Debug, PartialEq, Eq, HasParent, HasUUID, HasName, HasMeta, CowFrom, CowHelpers,
)]
pub enum NonRootFSObject<'a> {
	Dir(Cow<'a, RemoteDirectory>),
	File(Cow<'a, RemoteFile>),
}

impl HasType for NonRootFSObject<'_> {
	fn object_type(&self) -> ObjectType {
		match self {
			NonRootFSObject::Dir(_) => ObjectType::Dir,
			NonRootFSObject::File(_) => ObjectType::File,
		}
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FSObject1 {
	Dir(RemoteDirectory),
	Root(RootDirectory),
	RootWithMeta(RootDirectoryWithMeta),
	File(RemoteFile),
	SharedFile(RemoteRootFile),
}
