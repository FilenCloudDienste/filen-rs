use std::borrow::Cow;

use filen_types::fs::{ObjectType, ParentUuid};
use uuid::Uuid;

use super::{
	HasMeta, HasName, HasParent, HasType, HasUUID,
	dir::{DirectoryType, RemoteDirectory, RootDirectory, RootDirectoryWithMeta},
	file::{RemoteFile, RemoteRootFile},
};

#[derive(Debug, Clone, PartialEq, Eq)]
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

impl From<RemoteFile> for UnsharedFSObject<'static> {
	fn from(file: RemoteFile) -> Self {
		UnsharedFSObject::File(Cow::Owned(file))
	}
}

impl From<RemoteDirectory> for UnsharedFSObject<'static> {
	fn from(dir: RemoteDirectory) -> Self {
		UnsharedFSObject::Dir(Cow::Owned(dir))
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NonRootFSObject<'a> {
	Dir(Cow<'a, RemoteDirectory>),
	File(Cow<'a, RemoteFile>),
}

impl<'a> From<&'a RemoteFile> for NonRootFSObject<'a> {
	fn from(file: &'a RemoteFile) -> Self {
		NonRootFSObject::File(Cow::Borrowed(file))
	}
}

impl From<RemoteFile> for NonRootFSObject<'_> {
	fn from(file: RemoteFile) -> Self {
		NonRootFSObject::File(Cow::Owned(file))
	}
}

impl<'a> From<&'a RemoteDirectory> for NonRootFSObject<'a> {
	fn from(dir: &'a RemoteDirectory) -> Self {
		NonRootFSObject::Dir(Cow::Borrowed(dir))
	}
}

impl From<RemoteDirectory> for NonRootFSObject<'_> {
	fn from(dir: RemoteDirectory) -> Self {
		NonRootFSObject::Dir(Cow::Owned(dir))
	}
}

impl<'a, 'b> From<&'b NonRootFSObject<'a>> for NonRootFSObject<'b> {
	fn from(item: &'b NonRootFSObject<'a>) -> Self {
		match item {
			NonRootFSObject::Dir(cow) => NonRootFSObject::Dir(Cow::Borrowed(cow)),
			NonRootFSObject::File(cow) => NonRootFSObject::File(Cow::Borrowed(cow)),
		}
	}
}

impl HasParent for NonRootFSObject<'_> {
	fn parent(&self) -> ParentUuid {
		match self {
			NonRootFSObject::Dir(dir) => dir.parent(),
			NonRootFSObject::File(file) => file.parent(),
		}
	}
}

impl HasName for NonRootFSObject<'_> {
	fn name(&self) -> &str {
		match self {
			NonRootFSObject::Dir(dir) => dir.name(),
			NonRootFSObject::File(file) => file.name(),
		}
	}
}

impl HasMeta for NonRootFSObject<'_> {
	fn get_meta_string(&self) -> String {
		match self {
			NonRootFSObject::Dir(dir) => dir.get_meta_string(),
			NonRootFSObject::File(file) => file.get_meta_string(),
		}
	}
}

impl HasUUID for NonRootFSObject<'_> {
	fn uuid(&self) -> Uuid {
		match self {
			NonRootFSObject::Dir(dir) => dir.uuid(),
			NonRootFSObject::File(file) => file.uuid(),
		}
	}
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
