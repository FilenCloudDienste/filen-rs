//! What the helper asks Windows of its own process and of the volume a root is on.

use std::{
	fs::OpenOptions,
	io,
	os::windows::fs::OpenOptionsExt,
	path::{Path, PathBuf},
};

use windows_sys::Win32::Storage::FileSystem::{
	FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
	FILE_SHARE_WRITE,
};

use super::ffi;

/// Whether this process runs elevated: as administrator, or as SYSTEM.
pub fn is_elevated() -> io::Result<bool> {
	ffi::token_elevated()
}

/// The name of the file system (`NTFS`, `ReFS`, `FAT32`) of the volume `path` is on, read at
/// `path` or, when it does not exist yet, at its nearest existing ancestor; with that path.
pub fn volume_file_system(path: &Path) -> io::Result<(String, PathBuf)> {
	for at in path.ancestors() {
		let opened = OpenOptions::new()
			.access_mode(FILE_READ_ATTRIBUTES)
			.share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
			// A directory opens only with backup semantics.
			.custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
			.open(at);
		match opened {
			Ok(file) => {
				let name = ffi::volume_file_system(&file)?;
				return Ok((String::from_utf16_lossy(&name), at.to_path_buf()));
			}
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => return Err(error),
		}
	}
	Err(io::Error::from(io::ErrorKind::NotFound))
}
