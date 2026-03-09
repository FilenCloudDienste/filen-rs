use filen_macros::js_type;

use crate::{
	connect::{
		DirPublicLink,
		fs::{SharedDirectory, SharingRole},
	},
	fs::{
		categories::{DirType, Linked, Normal, Shared},
		dir::LinkedDirectory,
	},
	io::RemoteDirectory,
	js::{AnyLinkedDir, AnyNormalDir, AnySharedDir, Dir, LinkedDir, SharedDir},
};

#[js_type(import)]
pub enum AnyDirWithContext {
	Shared(AnySharedDirWithContext),
	Linked(AnyLinkedDirWithContext),
	Normal(AnyNormalDir),
}

#[js_type(import)]
pub struct AnySharedDirWithContext {
	#[js_type_tagged]
	dir: AnySharedDir,
	share_info: SharingRole,
}

#[js_type(import)]
pub struct AnyLinkedDirWithContext {
	#[js_type_tagged]
	pub(crate) dir: AnyLinkedDir,
	pub(crate) link: DirPublicLink,
}

pub(crate) enum DirByCategoryWithContext {
	Normal(DirType<'static, Normal>),
	Shared(DirType<'static, Shared>, SharingRole),
	Linked(DirType<'static, Linked>, DirPublicLink),
}

impl From<AnyDirWithContext> for DirByCategoryWithContext {
	fn from(value: AnyDirWithContext) -> Self {
		match value {
			AnyDirWithContext::Normal(dir) => Self::Normal(DirType::from(dir)),
			AnyDirWithContext::Shared(shared) => {
				Self::Shared(DirType::from(shared.dir), shared.share_info)
			}
			AnyDirWithContext::Linked(linked) => {
				Self::Linked(DirType::from(linked.dir), linked.link)
			}
		}
	}
}

#[js_type(export)]
pub enum NonRootDir {
	Normal(Dir),
	Shared(SharedDir),
	Linked(LinkedDir),
}

impl From<RemoteDirectory> for NonRootDir {
	fn from(value: RemoteDirectory) -> Self {
		Self::Normal(value.into())
	}
}

impl From<SharedDirectory> for NonRootDir {
	fn from(value: SharedDirectory) -> Self {
		Self::Shared(value.into())
	}
}

impl From<LinkedDirectory> for NonRootDir {
	fn from(value: LinkedDirectory) -> Self {
		Self::Linked(value.into())
	}
}
