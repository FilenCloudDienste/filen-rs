use walkdir::DirEntry;

use crate::io::fs_tree::WalkError;

pub(crate) struct LocalDirEntry;

pub(crate) struct LocalFileEntry(DirEntry);

impl super::DFSWalkerEntry for DirEntry {
	type WalkerFileEntry = LocalFileEntry;
	type WalkerDirEntry = LocalDirEntry;
	type CompareStrategy = LocalCompareStrategy;
	fn depth(&self) -> usize {
		self.depth()
	}

	fn name(&self) -> Result<&str, WalkError> {
		self.file_name()
			.to_str()
			.ok_or_else(|| WalkError::InvalidName(self.path().to_path_buf()))
	}

	fn into_entry_type(self) -> Result<super::EntryType<LocalDirEntry, LocalFileEntry>, WalkError> {
		let file_type = self.file_type();
		if file_type.is_dir() {
			Ok(super::EntryType::Dir(LocalDirEntry))
		} else if file_type.is_file() {
			Ok(super::EntryType::File(LocalFileEntry(self)))
		} else {
			Err(WalkError::UnsupportedFileType(self.path().to_path_buf()))
		}
	}
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct ExtraLocalFileData;

impl super::DFSWalkerFileEntry for LocalFileEntry {
	type Extra = ExtraLocalFileData;

	fn into_extra_data(self) -> Self::Extra {
		ExtraLocalFileData
	}

	fn size(&self) -> Result<u64, WalkError> {
		self.0.metadata().map(|m| m.len()).map_err(|e| {
			// walkdir only ever returns `None` here for a symlink-loop error,
			// which `metadata()` cannot produce; default defensively instead
			// of relying on that cross-crate invariant holding forever.
			let io_err = e
				.into_io_error()
				.unwrap_or_else(|| std::io::Error::other("walkdir metadata error"));
			WalkError::IO(Some(self.0.path().to_path_buf()), io_err)
		})
	}
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct ExtraLocalDirData;

impl super::DFSWalkerDirEntry for LocalDirEntry {
	type Extra = ExtraLocalDirData;

	fn into_extra_data(self) -> Self::Extra {
		ExtraLocalDirData
	}
}

pub(crate) struct LocalCompareStrategy;
impl super::CompareStrategy<ExtraLocalDirData, ExtraLocalFileData> for LocalCompareStrategy {
	fn should_replace(
		existing: &super::Entry<ExtraLocalDirData, ExtraLocalFileData>,
		new: &super::Entry<ExtraLocalDirData, ExtraLocalFileData>,
	) -> bool {
		// On a dir-vs-file name collision (same normalized key) prefer the
		// directory, matching `RemoteCompareStrategy`. Keeping the file instead
		// would drop the entire colliding directory subtree from the transfer —
		// the maximal-loss choice — and an upload/download round trip would flip
		// which entry survives.
		matches!(
			(existing, new),
			(super::Entry::File(_), super::Entry::Dir(_))
		)
	}
}

#[cfg(test)]
mod compare_strategy_tests {
	use string_interner::{DefaultBackend, StringInterner};

	use super::{ExtraLocalDirData, ExtraLocalFileData, LocalCompareStrategy};
	use crate::io::fs_tree::entry::{
		CompareStrategy, DirChildrenInfo, DirEntry, Entry, FileEntry, UnfinalizedDirEntry,
	};

	fn dir_and_file() -> (
		Entry<ExtraLocalDirData, ExtraLocalFileData>,
		Entry<ExtraLocalDirData, ExtraLocalFileData>,
	) {
		let mut interner = StringInterner::<DefaultBackend>::default();
		// Same interned name: a real dir-vs-file collision key.
		let name = interner.get_or_intern("collision");
		let dir = Entry::Dir(DirEntry::from_unfinalized(
			UnfinalizedDirEntry::new(name, ExtraLocalDirData),
			DirChildrenInfo::new(0, 0),
		));
		let file = Entry::File(FileEntry::new(name, ExtraLocalFileData, 0));
		(dir, file)
	}

	#[test]
	fn prefers_dir_over_file_like_remote() {
		let (dir, file) = dir_and_file();
		// existing File, new Dir -> replace so the Dir (and its subtree) wins.
		assert!(LocalCompareStrategy::should_replace(&file, &dir));
		// existing Dir, new File -> keep the Dir.
		assert!(!LocalCompareStrategy::should_replace(&dir, &file));
	}

	#[test]
	fn same_type_collisions_keep_existing() {
		let (dir, file) = dir_and_file();
		assert!(!LocalCompareStrategy::should_replace(&dir, &dir));
		assert!(!LocalCompareStrategy::should_replace(&file, &file));
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::{
		os::unix::fs::symlink,
		path::{Path, PathBuf},
		process::Command,
		sync::atomic::AtomicBool,
	};

	use crate::{
		Error, ErrorKind,
		io::{
			SymlinkPolicy,
			fs_tree::{WalkError, build_fs_tree_from_walkdir_iterator},
		},
	};

	/// What walking `root` under `symlinks` found: the names in the tree (sorted), the stats, the
	/// errors and the skipped symlinks.
	struct Walked {
		names: Vec<String>,
		stats: (u64, u64, u64),
		errors: Vec<Error>,
		skipped: Vec<PathBuf>,
	}

	fn walk(root: &Path, symlinks: SymlinkPolicy) -> Walked {
		let mut errors = Vec::new();
		let mut skipped = Vec::new();
		let (tree, stats) = build_fs_tree_from_walkdir_iterator(
			root,
			symlinks,
			&mut |errs| errors.extend(errs),
			&mut |_, _, _| {},
			&mut |path| skipped.push(path),
			&AtomicBool::new(false),
		)
		.expect("the walk should succeed");
		let mut names = tree
			.dfs_iter()
			.map(|(entry, _)| tree.get_name(entry).to_owned())
			.collect::<Vec<_>>();
		names.sort();
		skipped.sort();
		Walked {
			names,
			stats: stats.snapshot(),
			errors,
			skipped,
		}
	}

	/// A directory holding `a.txt` (1 byte), `d/inner.txt` (2 bytes), and the symlinks
	/// `link-file` -> `a.txt` and `link-dir` -> `d`.
	fn tree_with_symlinks() -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
		std::fs::create_dir(dir.path().join("d")).unwrap();
		std::fs::write(dir.path().join("d").join("inner.txt"), b"in").unwrap();
		symlink(dir.path().join("a.txt"), dir.path().join("link-file")).unwrap();
		symlink(dir.path().join("d"), dir.path().join("link-dir")).unwrap();
		dir
	}

	#[test]
	fn a_followed_walk_takes_in_what_symlinks_point_to() {
		let dir = tree_with_symlinks();
		let walked = walk(dir.path(), SymlinkPolicy::Follow);
		assert_eq!(
			walked.names,
			[
				"a.txt",
				"d",
				"inner.txt",
				"inner.txt",
				"link-dir",
				"link-file"
			]
		);
		assert_eq!(walked.stats, (2, 4, 6));
		assert!(walked.errors.is_empty(), "{:?}", walked.errors);
		assert!(walked.skipped.is_empty());
	}

	#[test]
	fn a_walk_skipping_symlinks_leaves_them_out_without_errors() {
		let dir = tree_with_symlinks();
		let walked = walk(dir.path(), SymlinkPolicy::Skip);
		assert_eq!(walked.names, ["a.txt", "d", "inner.txt"]);
		assert_eq!(walked.stats, (1, 2, 3));
		assert!(walked.errors.is_empty(), "{:?}", walked.errors);
		assert_eq!(
			walked.skipped,
			[dir.path().join("link-dir"), dir.path().join("link-file")]
		);
	}

	#[test]
	fn a_walk_skipping_symlinks_skips_a_dangling_one() {
		let dir = tempfile::tempdir().unwrap();
		symlink(dir.path().join("missing"), dir.path().join("dangling")).unwrap();
		let walked = walk(dir.path(), SymlinkPolicy::Skip);
		assert!(walked.names.is_empty());
		assert!(walked.errors.is_empty(), "{:?}", walked.errors);
		assert_eq!(walked.skipped, [dir.path().join("dangling")]);
	}

	#[test]
	fn a_walk_skipping_symlinks_still_follows_a_symlinked_root() {
		let dir = tree_with_symlinks();
		let walked = walk(&dir.path().join("link-dir"), SymlinkPolicy::Skip);
		assert_eq!(walked.names, ["inner.txt"]);
		assert!(walked.skipped.is_empty());
	}

	#[test]
	fn walk_reports_special_files_and_continues() {
		let dir = tempfile::tempdir().unwrap();
		std::fs::write(dir.path().join("regular.txt"), b"data").unwrap();
		let fifo_path = dir.path().join("fifo");
		let status = Command::new("mkfifo")
			.arg(&fifo_path)
			.status()
			.expect("mkfifo should be runnable");
		assert!(status.success());

		let mut errors = Vec::new();
		let (tree, stats) = build_fs_tree_from_walkdir_iterator(
			dir.path(),
			SymlinkPolicy::Follow,
			&mut |errs| errors.extend(errs),
			&mut |_, _, _| {},
			&mut |path| panic!("no symlink is skipped when following: {}", path.display()),
			&AtomicBool::new(false),
		)
		.expect("walk should survive special files");

		assert_eq!(stats.snapshot(), (0, 1, 4));
		let names = tree
			.dfs_iter()
			.map(|(entry, _)| tree.get_name(entry).to_owned())
			.collect::<Vec<_>>();
		assert_eq!(names, ["regular.txt"]);

		assert_eq!(errors.len(), 1);
		assert_eq!(errors[0].kind(), ErrorKind::Walk);
		assert!(matches!(
			errors[0].downcast_ref::<WalkError>(),
			Some(WalkError::UnsupportedFileType(path)) if path == &fifo_path
		));
	}
}
