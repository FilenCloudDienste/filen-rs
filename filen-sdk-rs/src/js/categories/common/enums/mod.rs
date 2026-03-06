use filen_macros::js_type;

use crate::{
	fs::categories::{Linked, NonRootItemType, Normal, Shared},
	js::{Dir, File, LinkedDir, SharedDir},
};

pub(crate) mod dir;
pub(crate) mod file;

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
