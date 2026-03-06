use std::borrow::Cow;

use filen_macros::CowFrom;
use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::{
	connect::fs::SharedDirectory,
	fs::{
		HasMeta, HasName, HasRemoteInfo, HasType, HasUUID,
		categories::{DirType, Normal},
		dir::LinkedDirectory,
	},
};

use super::{
	RemoteDirectory, RootDirectory, RootDirectoryWithMeta,
	traits::{HasDirInfo, HasDirMeta},
};

#[derive(Clone, Debug, PartialEq, Eq, CowHelpers, CowFrom, HasUUID)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	LinkedRoot(Cow<'a, RootDirectoryWithMeta>),
	LinkedDir(Cow<'a, LinkedDirectory>),
	SharedDir(Cow<'a, SharedDirectory>),
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

impl<'a> From<DirType<'a, Normal>> for DirectoryType<'a> {
	fn from(dir: DirType<'a, Normal>) -> Self {
		match dir {
			DirType::Root(dir) => DirectoryType::Root(dir),
			DirType::Dir(dir) => DirectoryType::Dir(dir),
		}
	}
}

impl HasType for DirType<'_, Normal> {
	fn object_type(&self) -> ObjectType {
		ObjectType::Dir
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
