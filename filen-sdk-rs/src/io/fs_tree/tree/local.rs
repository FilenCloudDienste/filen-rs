use std::{path::Path, sync::atomic::AtomicBool};

use crate::Error;

use super::{
	super::entry::local::{ExtraLocalDirData, ExtraLocalFileData},
	FSStats, FSTree, WalkError,
};

pub(crate) fn build_fs_tree_from_walkdir_iterator(
	root_path: &Path,
	error_callback: &mut impl FnMut(Vec<WalkError>),
	progress_callback: &mut impl FnMut(u64, u64, u64),
	should_cancel: &AtomicBool,
) -> Result<(FSTree<ExtraLocalDirData, ExtraLocalFileData>, FSStats), Error> {
	let iter = walkdir::WalkDir::new(root_path)
		.follow_links(true)
		.into_iter()
		.filter_map(|res| match res {
			Ok(v) => Some(Ok(v)),
			// hope we get if let guards to make this cleaner someday
			// https://github.com/rust-lang/rust/issues/51114
			Err(e) if e.loop_ancestor().is_some() => {
				let path = e.loop_ancestor().unwrap().to_path_buf();
				Some(Err(WalkError::Loop(path)))
			}
			Err(e) => {
				let path = e.path().map(|p| p.to_path_buf());
				e.into_io_error()
					.map(|io_err| Err(WalkError::IO(path, io_err)))
			}
		});

	super::build_fs_tree(iter, error_callback, progress_callback, should_cancel)
}
