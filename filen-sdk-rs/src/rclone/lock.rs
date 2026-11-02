//! Exclusive locks on files, which the system releases when the process holding them ends, however
//! it ends: they tell a directory a live daemon uses from one a crashed host left behind.
//!
//! A lock belongs to the open file, which a child process shares while it is being started: on
//! Unix a child another thread forks holds a copy of every descriptor until it execs. Closing the
//! descriptor alone would leave the lock held by that copy, so a lock is released explicitly.

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt as _;
use std::{
	fs::{File, OpenOptions, TryLockError},
	io,
	path::Path,
};

use tracing::warn;

/// A held lock on a file, released when dropped.
#[derive(Debug)]
pub(crate) struct FileLock {
	file: File,
}

impl Drop for FileLock {
	fn drop(&mut self) {
		// Released here rather than by closing the file, which a child being started may share.
		if let Err(e) = self.file.unlock() {
			warn!("releasing a file lock failed: {e}");
		}
	}
}

/// Takes the lock on the file at `path`, created (0600 on Unix) if it is missing. `None` when
/// another holder has it; a missing parent directory fails with [`io::ErrorKind::NotFound`].
pub(crate) fn try_lock(path: &Path) -> io::Result<Option<FileLock>> {
	let file = open(path)?;
	match file.try_lock() {
		Ok(()) => Ok(Some(FileLock { file })),
		Err(TryLockError::WouldBlock) => Ok(None),
		Err(TryLockError::Error(e)) => Err(e),
	}
}

fn open(path: &Path) -> io::Result<File> {
	let mut options = OpenOptions::new();
	// The file only carries the lock: it is never written, and an existing one is kept as it is.
	options.read(true).write(true).create(true).truncate(false);
	#[cfg(unix)]
	options.mode(0o600);
	options.open(path)
}

#[cfg(test)]
mod tests {
	use super::try_lock;

	#[test]
	fn a_held_lock_is_refused_until_it_is_dropped() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("lock");

		let held = try_lock(&path).unwrap();
		assert!(held.is_some());
		assert!(try_lock(&path).unwrap().is_none());

		drop(held);
		assert!(try_lock(&path).unwrap().is_some());
	}

	#[test]
	fn a_dropped_lock_is_released_while_a_copy_of_its_file_is_open() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("lock");
		let held = try_lock(&path).unwrap().unwrap();
		// What a child forked while the lock is held inherits, until it execs.
		let inherited = held.file.try_clone().unwrap();

		drop(held);

		assert!(try_lock(&path).unwrap().is_some());
		drop(inherited);
	}

	#[test]
	fn a_lock_in_a_missing_directory_is_not_found() {
		let dir = tempfile::tempdir().unwrap();

		let error = try_lock(&dir.path().join("missing/lock")).unwrap_err();

		assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
	}
}
