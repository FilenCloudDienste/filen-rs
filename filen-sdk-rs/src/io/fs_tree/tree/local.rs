use std::{path::Path, sync::atomic::AtomicBool};

use crate::Error;

use super::{
	super::entry::local::{ExtraLocalDirData, ExtraLocalFileData},
	FSStats, FSTree, WalkError,
};

pub(crate) fn build_fs_tree_from_walkdir_iterator(
	root_path: &Path,
	error_callback: &mut impl FnMut(Vec<Error>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTree<ExtraLocalDirData, ExtraLocalFileData>, FSStats), Error> {
	let mut iter = walkdir::WalkDir::new(root_path)
		.follow_links(true)
		.into_iter()
		.filter_map(|res| match res {
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
