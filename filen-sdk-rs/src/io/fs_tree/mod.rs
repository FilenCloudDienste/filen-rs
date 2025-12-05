use std::path::PathBuf;

use uuid::Uuid;

mod entry;
mod tree;

pub(crate) use entry::{
	DirChildrenInfo, Entry,
	local::{ExtraLocalDirData, ExtraLocalFileData},
};
pub(crate) use tree::{FSTree, local::build_fs_tree_from_walkdir_iterator};

#[derive(Debug, thiserror::Error)]
pub enum WalkError {
	#[error("detected a symlink loop at path {0:?}")]
	Loop(PathBuf),
	#[error("IO error at path {0:?}: {1}")]
	IO(Option<PathBuf>, std::io::Error),
	#[error("invalid file name at path {0:?}")]
	InvalidName(PathBuf),
	#[error("encrypted metadata could not be read for UUID {0}")]
	EncryptedMeta(Uuid),
}
