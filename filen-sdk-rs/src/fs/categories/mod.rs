use std::{borrow::Cow, fmt::Debug};

use filen_types::traits::CowHelpers;

use crate::{
	auth::shared_client::SharedClient,
	fs::{
		HasName, HasParent, HasRemoteInfo, HasUUID, dir::traits::HasDirInfo,
		file::traits::File as FileTrait,
	},
};

pub mod fs;
pub(crate) mod linked;
pub(crate) mod normal;
pub(crate) mod shared;

pub use {linked::Linked, normal::Normal, shared::Shared};

pub trait Category: 'static {
	#[allow(private_bounds)]
	type Client: SharedClient + Send + Sync + 'static;
	type Root: Debug + PartialEq + Eq + Clone + Send + Sync + HasUUID + 'static;
	type Dir: Debug
		+ PartialEq
		+ Eq
		+ Clone
		+ Send
		+ Sync
		+ HasUUID
		+ HasName
		+ HasParent
		+ HasRemoteInfo
		+ HasDirInfo
		+ 'static;
	type RootFile: Debug + PartialEq + Eq + Clone + Send + Sync + FileTrait + 'static;
	type File: Debug + PartialEq + Eq + Clone + Send + Sync + HasParent + FileTrait + 'static;
}

#[derive(Debug, PartialEq, Eq, Clone, HasUUID, CowHelpers)]
pub enum DirType<'a, Cat: Category + ?Sized> {
	Root(Cow<'a, Cat::Root>),
	Dir(Cow<'a, Cat::Dir>),
}

#[derive(Debug, PartialEq, Eq, Clone, HasUUID, HasName, HasParent, HasRemoteInfo, CowHelpers)]
pub enum NonRootItemType<'a, Cat: Category + ?Sized> {
	Dir(Cow<'a, Cat::Dir>),
	File(Cow<'a, Cat::File>),
}

#[derive(Debug, PartialEq, Eq, Clone, HasUUID, CowHelpers)]
pub enum NonRootFileType<'a, Cat: Category + ?Sized> {
	Root(Cow<'a, Cat::Root>),
	Dir(Cow<'a, Cat::Dir>),
	File(Cow<'a, Cat::File>),
}

#[derive(Debug, PartialEq, Eq, Clone, HasUUID, CowHelpers)]
pub enum RootItemType<'a, Cat: Category + ?Sized> {
	Dir(Cow<'a, Cat::Root>),
	File(Cow<'a, Cat::RootFile>),
}

impl<'a, Cat: Category + ?Sized> From<DirType<'a, Cat>> for NonRootFileType<'a, Cat> {
	fn from(value: DirType<'a, Cat>) -> Self {
		match value {
			DirType::Root(root) => NonRootFileType::Root(root),
			DirType::Dir(dir) => NonRootFileType::Dir(dir),
		}
	}
}

impl<'a, Cat: Category + ?Sized> From<NonRootItemType<'a, Cat>> for NonRootFileType<'a, Cat> {
	fn from(value: NonRootItemType<'a, Cat>) -> Self {
		match value {
			NonRootItemType::Dir(dir) => NonRootFileType::Dir(dir),
			NonRootItemType::File(file) => NonRootFileType::File(file),
		}
	}
}
