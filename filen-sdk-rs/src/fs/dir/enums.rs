use std::borrow::Cow;

use filen_types::fs::ObjectType;

use crate::fs::{HasType, HasUUID};

use super::{Directory, HasContents, RootDirectory, RootDirectoryWithMeta};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectoryType<'a> {
	Root(Cow<'a, RootDirectory>),
	RootWithMeta(Cow<'a, RootDirectoryWithMeta>),
	Dir(Cow<'a, Directory>),
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
