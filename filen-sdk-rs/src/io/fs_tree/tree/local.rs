use std::{
	path::{Path, PathBuf},
	sync::atomic::AtomicBool,
};

use crate::{
	Error,
	io::{SymlinkPolicy, filter::TransferFilter},
};

use super::{
	super::entry::local::{ExtraLocalDirData, ExtraLocalFileData},
	FSStats, FSTree, WalkError,
};

/// Walks the tree below `root_path`, which is itself followed when it is a symlink, taking what
/// `filter` takes. Symlinks below it are followed or, under [`SymlinkPolicy::Skip`], left out of
/// the tree and handed to `skipped_symlink_callback`.
pub(crate) fn build_fs_tree_from_walkdir_iterator(
	root_path: &Path,
	symlinks: SymlinkPolicy,
	filter: &TransferFilter,
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
		// Not descending into an excluded directory at all spares the walk its contents and the
		// errors reading them would report. The tree builder's filter drops the directory itself.
		.filter_entry(|entry| {
			entry.depth() == 0
				|| !relative_path(root_path, entry.path())
					.is_some_and(|rel_path| filter.excludes(&rel_path, entry.file_type().is_dir()))
		})
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

	super::build_filtered_fs_tree(
		iter,
		filter,
		error_callback,
		progress_callback,
		should_cancel,
	)
}

/// `path` relative to `root`, its components joined with `/`, or `None` when it is not below
/// `root` or a component is not UTF-8.
fn relative_path(root: &Path, path: &Path) -> Option<String> {
	path.strip_prefix(root)
		.ok()?
		.components()
		.map(|component| component.as_os_str().to_str())
		.collect::<Option<Vec<_>>>()
		.map(|components| components.join("/"))
}

#[cfg(test)]
mod tests {
	use std::{
		path::Path,
		sync::{Arc, Mutex, atomic::AtomicBool},
	};

	use super::{build_fs_tree_from_walkdir_iterator, relative_path};
	use crate::io::{PathMatcher, SymlinkPolicy, TransferFilter};

	/// Matches the paths it lists, and notes every path it is asked about.
	struct Recording {
		matched: &'static [&'static str],
		asked: Mutex<Vec<String>>,
	}

	impl PathMatcher for Recording {
		fn matches(&self, rel_path: &str, _is_dir: bool) -> bool {
			self.asked.lock().unwrap().push(rel_path.to_owned());
			self.matched.contains(&rel_path)
		}
	}

	fn recording(matched: &'static [&'static str]) -> Arc<Recording> {
		Arc::new(Recording {
			matched,
			asked: Mutex::new(Vec::new()),
		})
	}

	/// The paths, relative to `root`, of the files in the tree walking it under `filter` builds.
	fn walked_files(root: &Path, filter: &TransferFilter) -> Vec<String> {
		let (tree, _) = build_fs_tree_from_walkdir_iterator(
			root,
			SymlinkPolicy::Follow,
			filter,
			&mut |errors| panic!("the walk reported {errors:?}"),
			&mut |_, _, _| {},
			&mut |_| {},
			&AtomicBool::new(false),
		)
		.expect("the walk should succeed");
		let mut files = tree
			.dfs_iter_with_path("")
			.filter(|(entry, _)| matches!(entry, crate::io::fs_tree::Entry::File(_)))
			.map(|(_, path)| path)
			.collect::<Vec<_>>();
		files.sort();
		files
	}

	/// `keep/a.txt`, `keep/b.jpg` and `skip/inner/c.txt` under a new directory.
	fn tree() -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		std::fs::create_dir_all(dir.path().join("keep")).unwrap();
		std::fs::create_dir_all(dir.path().join("skip").join("inner")).unwrap();
		std::fs::write(dir.path().join("keep").join("a.txt"), b"a").unwrap();
		std::fs::write(dir.path().join("keep").join("b.jpg"), b"b").unwrap();
		std::fs::write(dir.path().join("skip").join("inner").join("c.txt"), b"c").unwrap();
		dir
	}

	#[test]
	fn the_walk_does_not_descend_into_an_excluded_directory() {
		let dir = tree();
		let exclude = recording(&["skip"]);
		let filter = TransferFilter {
			include: None,
			exclude: Some(exclude.clone()),
		};
		assert_eq!(
			walked_files(dir.path(), &filter),
			["keep/a.txt", "keep/b.jpg"]
		);
		let asked = exclude.asked.lock().unwrap();
		assert!(
			!asked.iter().any(|path| path.starts_with("skip/")),
			"{asked:?}"
		);
	}

	#[test]
	fn an_include_takes_only_the_files_it_matches() {
		let dir = tree();
		let filter = TransferFilter {
			include: Some(recording(&["keep/b.jpg"])),
			exclude: None,
		};
		assert_eq!(walked_files(dir.path(), &filter), ["keep/b.jpg"]);
	}

	#[test]
	fn a_path_below_the_root_is_joined_with_slashes() {
		let root = Path::new("root");
		assert_eq!(
			relative_path(root, &root.join("a").join("b.txt")),
			Some("a/b.txt".to_owned())
		);
		assert_eq!(relative_path(root, Path::new("elsewhere")), None);
	}
}
