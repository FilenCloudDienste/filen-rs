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
		matches!(
			(existing, new),
			(super::Entry::Dir(_), super::Entry::File(_))
		)
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::{process::Command, sync::atomic::AtomicBool};

	use crate::{
		ErrorKind,
		io::fs_tree::{WalkError, build_fs_tree_from_walkdir_iterator},
	};

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
			&mut |errs| errors.extend(errs),
			&mut |_, _, _| {},
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
