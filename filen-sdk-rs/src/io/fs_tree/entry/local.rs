use walkdir::DirEntry;

use crate::io::WalkError;

pub(crate) struct LocalDirEntry<'a>(&'a DirEntry);

pub(crate) struct LocalFileEntry<'a>(&'a DirEntry);

impl super::DFSWalkerEntry for DirEntry {
	type WalkerFileEntry<'a> = LocalFileEntry<'a>;
	type WalkerDirEntry<'a> = LocalDirEntry<'a>;

	fn depth(&self) -> usize {
		self.depth()
	}

	fn name(&self) -> Result<&str, WalkError> {
		self.file_name()
			.to_str()
			.ok_or_else(|| WalkError::InvalidName(self.path().to_path_buf()))
	}

	fn entry_type<'a>(&'a self) -> super::EntryType<LocalDirEntry<'a>, LocalFileEntry<'a>> {
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

impl super::DFSWalkerFileEntry for LocalFileEntry<'_> {
	type Extra = ExtraLocalFileData;

	fn into_extra_data(self) -> Result<Self::Extra, WalkError> {
		Ok(ExtraLocalFileData)
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

impl super::DFSWalkerDirEntry for LocalDirEntry<'_> {
	type Extra = ExtraLocalDirData;

	fn into_extra_data(self) -> Result<Self::Extra, WalkError> {
		Ok(ExtraLocalDirData)
	}
}
