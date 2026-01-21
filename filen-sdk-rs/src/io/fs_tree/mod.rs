use std::{borrow::Cow, path::PathBuf};

use uuid::Uuid;

mod entry;
mod tree;

pub(crate) use entry::{
	DirChildrenInfo, Entry,
	local::{ExtraLocalDirData, ExtraLocalFileData},
};
pub(crate) use tree::{
	FSTree, local::build_fs_tree_from_walkdir_iterator, remote::build_fs_tree_from_remote_iterator,
};

use crate::ErrorKind;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WalkError {
	#[error("detected a symlink loop at path {0:?}")]
	Loop(PathBuf),
	#[error("IO error at path {0:?}: {1}")]
	IO(Option<PathBuf>, std::io::Error),
	#[error("invalid file name at path {0:?}")]
	InvalidName(PathBuf),
	#[error("encrypted metadata could not be read for UUID {0}")]
	EncryptedMeta(Uuid),
	#[error("Multiple entries with the same path {0} were detected, these entries were skipped")]
	DuplicateName(String),
}

impl From<WalkError> for crate::Error {
	fn from(value: WalkError) -> Self {
		crate::Error::custom_with_source(ErrorKind::Walk, value, None::<Cow<'static, str>>)
	}
}
