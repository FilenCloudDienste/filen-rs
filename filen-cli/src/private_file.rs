//! Files only their owner can read or write: the log files, the REPL history and the auth config,
//! which hold paths, file names, typed commands or the account's keys.

#[cfg(unix)]
use std::{
	fs::Permissions,
	os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
};
use std::{
	fs::{File, OpenOptions},
	io,
	path::Path,
};

/// The mode of a private file on unix: only its owner can read or write it.
#[cfg(unix)]
pub(crate) const PRIVATE_FILE_MODE: u32 = 0o600;

/// Options that create a file with [`PRIVATE_FILE_MODE`] on unix.
pub(crate) fn private_file_options() -> OpenOptions {
	let options = OpenOptions::new();
	#[cfg(unix)]
	let options = {
		let mut options = options;
		options.mode(PRIVATE_FILE_MODE);
		options
	};
	options
}

/// Opens `path` with `options`, then sets [`PRIVATE_FILE_MODE`] on it on unix, before anything is
/// written to it: the mode in `options` applies only to a file the open creates, and a file that
/// already exists may have been created with the default mode, by an older CLI or by hand.
pub(crate) fn open_private(options: &OpenOptions, path: &Path) -> io::Result<File> {
	let file = options.open(path)?;
	#[cfg(unix)]
	file.set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))?;
	Ok(file)
}

/// Opens `path` for appending with [`open_private`], creating it if it does not exist.
pub(crate) fn open_private_append(path: &Path) -> io::Result<File> {
	open_private(private_file_options().create(true).append(true), path)
}
