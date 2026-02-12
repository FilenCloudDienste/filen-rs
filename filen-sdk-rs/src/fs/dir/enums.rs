use std::borrow::Cow;

use filen_macros::CowFrom;
use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::{
	connect::fs::SharedDirectory,
	fs::{HasMeta, HasName, HasRemoteInfo, HasType, HasUUID, UnsharedFSObject},
};

use super::{
	HasContents, RemoteDirectory, RootDirectory, RootDirectoryWithMeta,
	traits::{HasDirInfo, HasDirMeta},
};

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers, CowFrom, HasUUID, HasContents)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	RootWithMeta(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl HasType for DirectoryType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers, CowFrom)]
pub enum DirectoryTypeWithShareInfo<'a> {
	Root(Cow<'a, RootDirectory>),
	SharedDir(Cow<'a, SharedDirectory>),
	Dir(Cow<'a, RemoteDirectory>),
}

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers, CowFrom, HasUUID, HasContents)]
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

#[derive(
	Clone,
	Debug,
	PartialEq,
	Eq,
	CowHelpers,
	CowFrom,
	HasUUID,
	HasContents,
	HasName,
	HasDirInfo,
	HasRemoteInfo,
	HasMeta,
	HasDirMeta,
)]
pub enum DirectoryMetaType<'a> {
	Root(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, RemoteDirectory>),
}

impl HasType for DirectoryMetaType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
	}
}
