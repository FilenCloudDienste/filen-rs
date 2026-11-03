use std::{
	path::{Path, PathBuf},
	sync::atomic::AtomicBool,
};

use crate::{Error, io::SymlinkPolicy};

use super::{
	super::entry::local::{ExtraLocalDirData, ExtraLocalFileData},
	FSStats, FSTree, WalkError,
};

/// Walks the tree below `root_path`, which is itself followed when it is a symlink. Symlinks
/// below it are followed or, under [`SymlinkPolicy::Skip`], left out of the tree and handed to
/// `skipped_symlink_callback`.
pub(crate) fn build_fs_tree_from_walkdir_iterator(
	root_path: &Path,
	symlinks: SymlinkPolicy,
	error_callback: &mut impl FnMut(Vec<Error>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	skipped_symlink_callback: &mut impl FnMut(PathBuf),
	should_cancel: &AtomicBool,
) -> Result<(FSTree<ExtraLocalDirData, ExtraLocalFileData>, FSStats), Error> {
	let follow = match symlinks {
		SymlinkPolicy::Follow => true,
		SymlinkPolicy::Skip => false,
	};
	let mut iter = walkdir::WalkDir::new(root_path)
		.follow_links(follow)
		.into_iter()
		.filter_map(|res| match res {
			// The root (depth 0) is always followed, as walkdir does.
			Ok(v) if !follow && v.depth() > 0 && v.path_is_symlink() => {
				skipped_symlink_callback(v.into_path());
				None
			}
			Ok(v) => Some(Ok(v)),
			Err(e) if let Some(path) = e.loop_ancestor().map(|p| p.to_path_buf()) => {
				Some(Err(WalkError::Loop(path)))
			}
			Err(e) => {
				let path = e.path().map(|p| p.to_path_buf());
				e.into_io_error()
					.map(|io_err| Err(WalkError::IO(path, io_err)))
			}
		});
	if let Some(Err(e)) = iter.next() {
		return Err(Error::custom(
			crate::ErrorKind::IO,
			format!("Failed to start walking directory: {}", e),
		));
	} // skip root entry

	super::build_fs_tree(iter, error_callback, progress_callback, should_cancel)
}
