use std::borrow::Cow;
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::path::PathBuf;

use uuid::Uuid;

mod entry;
mod tree;

pub(crate) use entry::Entry;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use entry::{
	DirChildrenInfo,
	local::{ExtraLocalDirData, ExtraLocalFileData},
};

pub(crate) use tree::{FSTree, remote::build_fs_tree_from_remote_iterator};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use tree::local::build_fs_tree_from_walkdir_iterator;

use crate::ErrorKind;

#[derive(Debug, thiserror::Error)]
pub(crate) enum WalkError {
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[error("detected a symlink loop at path {0:?}")]
	Loop(PathBuf),
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[error("IO error at path {0:?}: {1}")]
	IO(Option<PathBuf>, std::io::Error),
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[error("invalid file name at path {0:?}")]
	InvalidName(PathBuf),
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	#[error("unsupported file type (not a regular file or directory) at path {0:?}")]
	UnsupportedFileType(PathBuf),
	#[error("encrypted metadata could not be read for UUID {0}")]
	EncryptedMeta(Uuid),
	#[error("Multiple entries with the same path {0} were detected, these entries were skipped")]
	DuplicateName(String),
	#[error(
		"{count} remote entries had a parent not reachable from the requested root (malformed listing or cyclic parent) and were omitted from the tree"
	)]
	UnreachableEntries { count: usize },
}

impl From<WalkError> for crate::Error {
	fn from(value: WalkError) -> Self {
		crate::Error::custom_with_source(ErrorKind::Walk, value, None::<Cow<'static, str>>)
	}
}
