//! Files only their owner can read or write: the log files, the REPL history, the auth config and
//! the self-signed certificate's key, which hold paths, file names, typed commands or secrets.

#[cfg(unix)]
use std::{
	fs::Permissions,
	os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _},
};
use std::{
	fs::{self, File, OpenOptions},
	io::{self, Write as _},
	path::{Path, PathBuf},
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

/// Replaces the file at `path` with `contents` whole: they are written to a temporary file in the
/// same directory, created with [`PRIVATE_FILE_MODE`] on unix, which is then renamed over it, so
/// an interrupted write leaves the old file, never a truncated one. A symlink at `path` is
/// followed and its target replaced, or created when the link dangles. The directory must be
/// writable, as there is no fallback to writing the file in place; on failure the temporary file
/// is removed.
pub(crate) fn replace_private(path: &Path, contents: &[u8]) -> io::Result<()> {
	let target = replace_target(path)?;
	let dir = parent_dir(&target);
	let mut file = tempfile::Builder::new().prefix(".tmp-").tempfile_in(dir)?;
	file.write_all(contents)?;
	file.as_file().sync_all()?;
	file.persist(&target).map_err(|e| e.error)?;
	Ok(())
}

/// The file [`replace_private`] renames over: `path` itself, unless it is a symlink, which is
/// resolved. Only a symlink is canonicalized, as canonicalizing fails on some Windows volumes
/// (those without a mount manager, such as RAM disks) where a plain path works. A dangling link
/// resolves to its target, relative to the link's directory when the target is relative.
fn replace_target(path: &Path) -> io::Result<PathBuf> {
	match fs::symlink_metadata(path) {
		Ok(meta) if meta.is_symlink() => match fs::canonicalize(path) {
			Ok(target) => Ok(target),
			// `join` keeps an absolute link target as it is
			Err(e) if e.kind() == io::ErrorKind::NotFound => {
				Ok(parent_dir(path).join(fs::read_link(path)?))
			}
			Err(e) => Err(e),
		},
		Ok(_) => Ok(path.to_owned()),
		Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(path.to_owned()),
		Err(e) => Err(e),
	}
}

/// The directory `path` is in, `.` for a bare file name.
fn parent_dir(path: &Path) -> PathBuf {
	match path.parent() {
		Some(parent) if !parent.as_os_str().is_empty() => parent.to_owned(),
		_ => PathBuf::from("."),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The names of the entries in `dir`, sorted.
	fn entries(dir: &Path) -> Vec<String> {
		let mut names = fs::read_dir(dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		names.sort();
		names
	}

	#[test]
	fn replacing_leaves_no_other_file_in_the_directory() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("config.txt");
		fs::write(&path, "old").unwrap();
		replace_private(&path, b"new").unwrap();
		assert_eq!(fs::read_to_string(&path).unwrap(), "new");
		assert_eq!(entries(dir.path()), ["config.txt"]);
	}

	#[test]
	fn replacing_creates_a_missing_file() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("config.txt");
		replace_private(&path, b"new").unwrap();
		assert_eq!(fs::read_to_string(&path).unwrap(), "new");
		assert_eq!(entries(dir.path()), ["config.txt"]);
	}

	/// The file is replaced, not rewritten in place: a hard link to the old one keeps the old
	/// contents, as a reader that opened the old file keeps reading it whole.
	#[test]
	fn replacing_leaves_a_hard_link_to_the_old_file_as_it_was() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("config.txt");
		let link = dir.path().join("link.txt");
		fs::write(&path, "old").unwrap();
		fs::hard_link(&path, &link).unwrap();
		replace_private(&path, b"new").unwrap();
		assert_eq!(fs::read_to_string(&path).unwrap(), "new");
		assert_eq!(fs::read_to_string(&link).unwrap(), "old");
	}

	#[test]
	fn a_failed_replace_keeps_the_old_file() {
		let dir = tempfile::tempdir().unwrap();
		// a directory, which no file can be renamed over
		let path = dir.path().join("config.txt");
		fs::create_dir(&path).unwrap();
		fs::write(path.join("inside.txt"), "old").unwrap();
		replace_private(&path, b"new").unwrap_err();
		assert_eq!(fs::read_to_string(path.join("inside.txt")).unwrap(), "old");
		assert_eq!(entries(dir.path()), ["config.txt"]);
	}

	#[cfg(unix)]
	#[test]
	fn replacing_through_a_symlink_keeps_the_link() {
		let dir = tempfile::tempdir().unwrap();
		let target_dir = tempfile::tempdir().unwrap();
		let target = target_dir.path().join("config.txt");
		let link = dir.path().join("config.txt");
		fs::write(&target, "old").unwrap();
		std::os::unix::fs::symlink(&target, &link).unwrap();
		replace_private(&link, b"new").unwrap();
		assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
		assert_eq!(fs::read_to_string(&target).unwrap(), "new");
		assert_eq!(entries(dir.path()), ["config.txt"]);
		assert_eq!(entries(target_dir.path()), ["config.txt"]);
	}

	#[cfg(unix)]
	#[test]
	fn replacing_through_a_dangling_symlink_keeps_the_link_and_creates_its_target() {
		let dir = tempfile::tempdir().unwrap();
		let link = dir.path().join("config.txt");
		std::os::unix::fs::symlink("target.txt", &link).unwrap();
		replace_private(&link, b"new").unwrap();
		assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
		assert_eq!(
			fs::read_to_string(dir.path().join("target.txt")).unwrap(),
			"new"
		);
		assert_eq!(entries(dir.path()), ["config.txt", "target.txt"]);
	}

	#[cfg(unix)]
	#[test]
	fn a_replaced_file_is_owner_only() {
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("config.txt");
		fs::write(&path, "old").unwrap();
		fs::set_permissions(&path, Permissions::from_mode(0o644)).unwrap();
		replace_private(&path, b"new").unwrap();
		let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode, PRIVATE_FILE_MODE);
	}
}
