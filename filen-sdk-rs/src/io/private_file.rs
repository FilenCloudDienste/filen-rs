//! Files only the current user may read, and removing them.

#[cfg(feature = "rclone")]
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::{fs::File, path::Path};

/// Creates or truncates the file at `path` for writing, with owner-only permissions (mode `0o600`
/// on Unix), so that nothing written to it is ever group- or world-readable, or writable, for
/// other users on the machine.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<File> {
	let mut options = std::fs::OpenOptions::new();
	options.write(true).create(true).truncate(true);
	#[cfg(unix)]
	options.mode(0o600);
	let file = options.open(path)?;
	// A pre-existing file keeps its previous mode through create+truncate, so tighten it
	// explicitly while it is still empty, before any secret bytes are written.
	#[cfg(unix)]
	file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
	Ok(file)
}

/// Write `contents` to `path` through [`create_private_file`]. The files written with it are
/// rclone's rc password hash and its pidfile.
#[cfg(feature = "rclone")]
pub(crate) fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
	create_private_file(path)?.write_all(contents.as_bytes())
}
// copied from filen-cli/src/auth.rs

/// Removes the file at `path`. A file that is not there is not an error.
#[cfg(feature = "rclone")]
pub(crate) fn remove_file_if_present(path: &Path) -> std::io::Result<()> {
	match std::fs::remove_file(path) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
		removed => removed,
	}
}
