use walkdir::DirEntry;

use crate::io::fs_tree::WalkError;

pub(crate) struct LocalDirEntry(DirEntry);

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

	fn into_entry_type(self) -> super::EntryType<LocalDirEntry, LocalFileEntry> {
		if self.file_type().is_dir() {
			super::EntryType::Dir(LocalDirEntry(self))
		} else if self.file_type().is_file() {
			super::EntryType::File(LocalFileEntry(self))
		} else {
			panic!("non-file/dir values should be filtered out earliear")
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
			WalkError::IO(
				Some(self.0.path().to_path_buf()),
				e.into_io_error().unwrap(),
			)
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
