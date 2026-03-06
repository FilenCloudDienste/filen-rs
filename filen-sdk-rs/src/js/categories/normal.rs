use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_macros::js_type;
use filen_types::fs::{ParentUuid, UuidStr};

use crate::{
	crypto::error::ConversionError,
	fs::{
		HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		dir::RootDirectory,
	},
	io::RemoteDirectory,
	js::{DirColor, DirMeta, File, categories::CategoryJSExt},
};

impl CategoryJSExt for Normal {
	type RootJS = Root;
	type DirJS = Dir;
	type FileJS = File;
	type RootFileJS = File;
}

#[js_type(export)]
pub struct Root {
	pub uuid: UuidStr,
}

impl From<RootDirectory> for Root {
	fn from(dir: RootDirectory) -> Self {
		Root { uuid: *dir.uuid() }
	}
}

impl From<Root> for RootDirectory {
	fn from(root: Root) -> Self {
		RootDirectory::new(root.uuid)
	}
}

#[js_type(import, export)]
pub struct Dir {
	pub uuid: UuidStr,
	pub parent: ParentUuid,
	pub color: DirColor,
	#[cfg_attr(
		feature = "wasm-full",
		tsify(type = "bigint"),
		serde(with = "chrono::serde::ts_milliseconds")
	)]
	pub timestamp: DateTime<Utc>,
	pub favorited: bool,
	pub meta: DirMeta,
}

impl From<RemoteDirectory> for Dir {
	fn from(dir: RemoteDirectory) -> Self {
		Dir {
			uuid: dir.uuid,
			parent: dir.parent,
			color: dir.color.into(),
			favorited: dir.favorited,
			timestamp: dir.timestamp,
			meta: dir.meta.into(),
		}
	}
}

impl From<Dir> for RemoteDirectory {
	fn from(dir: Dir) -> Self {
		RemoteDirectory::from_meta(
			dir.uuid,
			dir.parent,
			dir.color.into(),
			dir.favorited,
			dir.timestamp,
			dir.meta.into(),
		)
	}
}

#[js_type(import)]
pub enum AnyNormalDir {
	Dir(Dir),
	Root(Root),
}

impl From<AnyNormalDir> for DirType<'static, Normal> {
	fn from(value: AnyNormalDir) -> Self {
		match value {
			AnyNormalDir::Dir(dir) => DirType::Dir(Cow::Owned(dir.into())),
			AnyNormalDir::Root(root) => DirType::Root(Cow::Owned(root.into())),
		}
	}
}

impl From<DirType<'static, Normal>> for AnyNormalDir {
	fn from(value: DirType<'static, Normal>) -> Self {
		match value {
			DirType::Dir(dir) => AnyNormalDir::Dir(dir.into_owned().into()),
			DirType::Root(root) => AnyNormalDir::Root(root.into_owned().into()),
		}
	}
}

#[js_type(import, export)]
pub enum NonRootNormalItem {
	Dir(Dir),
	File(File),
}

impl TryFrom<NonRootNormalItem> for NonRootItemType<'static, Normal> {
	type Error = ConversionError;

	fn try_from(value: NonRootNormalItem) -> Result<Self, Self::Error> {
		Ok(match value {
			NonRootNormalItem::Dir(dir) => NonRootItemType::Dir(Cow::Owned(dir.into())),
			NonRootNormalItem::File(file) => NonRootItemType::File(Cow::Owned(file.try_into()?)),
		})
	}
}

impl From<NonRootItemType<'static, Normal>> for NonRootNormalItem {
	fn from(value: NonRootItemType<'static, Normal>) -> Self {
		match value {
			NonRootItemType::Dir(dir) => NonRootNormalItem::Dir(dir.into_owned().into()),
			NonRootItemType::File(file) => NonRootNormalItem::File(file.into_owned().into()),
		}
	}
}
