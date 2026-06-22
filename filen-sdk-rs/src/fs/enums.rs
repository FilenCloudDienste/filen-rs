use std::borrow::Cow;

use filen_macros::CowFrom;

use filen_types::{fs::ObjectType, traits::CowHelpers};

use crate::{
	connect::fs::{SharedDirectory, SharedRootFile},
	fs::{
		categories::{Category, NonRootItemType},
		dir::LinkedDirectory,
		file::LinkedFile,
	},
};

use super::{
	HasType,
	dir::{RemoteDirectory, RootDirectory, RootDirectoryWithMeta},
	file::{RemoteFile, RemoteRootFile},
};

#[derive(Debug, Clone, PartialEq, Eq, CowFrom, CowHelpers)]
pub enum FSObject<'a> {
	Dir(Cow<'a, RemoteDirectory>),
	Root(Cow<'a, RootDirectory>),
	RootWithMeta(Cow<'a, RootDirectoryWithMeta>),
	File(Cow<'a, RemoteFile>),
	SharedFile(Cow<'a, RemoteRootFile>),
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

impl<Cat: Category> HasType for NonRootItemType<'_, Cat> {
	fn object_type(&self) -> ObjectType {
		match self {
			Self::Dir(_) => ObjectType::Dir,
			Self::File(_) => ObjectType::File,
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

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum SharedItem<'a> {
	RootFile(Cow<'a, SharedRootFile>),
	Dir(Cow<'a, SharedDirectory>),
	File(Cow<'a, RemoteDirectory>),
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub enum LinkedItem<'a> {
	RootDir(Cow<'a, RootDirectoryWithMeta>),
	RootFile(Cow<'a, LinkedFile>),
	Dir(Cow<'a, LinkedDirectory>),
	File(Cow<'a, RemoteDirectory>),
}
