use filen_macros::js_type;

use crate::{
	fs::categories::{Linked, NonRootItemType, Normal, Shared},
	js::{AnyDirWithContext, AnyFile, Dir, File, LinkedDir, SharedDir},
};

pub(crate) mod dir;
pub(crate) mod file;

/// A file, or a directory with what is needed to list it (its share or public link). Untagged
/// both ways, so an item the SDK hands out can be passed back as it is.
#[js_type(import, untagged, wasm_all)]
pub enum AnyItemWithContext {
	File(AnyFile),
	Dir(AnyDirWithContext),
}

#[js_type(export)]
pub enum NonRootItem {
	NormalDir(Dir),
	File(File),
	SharedDir(SharedDir),
	LinkedDir(LinkedDir),
}

impl From<NonRootItemType<'static, Normal>> for NonRootItem {
	fn from(value: NonRootItemType<'static, Normal>) -> Self {
		match value {
			NonRootItemType::Dir(dir) => Self::NormalDir(dir.into_owned().into()),
			NonRootItemType::File(file) => Self::File(file.into_owned().into()),
		}
	}
}

impl From<NonRootItemType<'static, Shared>> for NonRootItem {
	fn from(value: NonRootItemType<'static, Shared>) -> Self {
		match value {
			NonRootItemType::Dir(dir) => Self::SharedDir(dir.into_owned().into()),
			NonRootItemType::File(file) => Self::File(file.into_owned().into()),
		}
	}
}

impl From<NonRootItemType<'static, Linked>> for NonRootItem {
	fn from(value: NonRootItemType<'static, Linked>) -> Self {
		match value {
			NonRootItemType::Dir(dir) => Self::LinkedDir(dir.into_owned().into()),
			NonRootItemType::File(file) => Self::File(file.into_owned().into()),
		}
	}
}
