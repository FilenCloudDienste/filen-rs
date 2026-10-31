//! The files rclone's VFS cache metadata marks written and not uploaded yet.
//!
//! rclone keeps one JSON file per cached file under the VFS's metadata root (`vfs/stats`'
//! `diskCache.pathMeta`), its `vfscache.Info`: `Dirty` is saved as `true` on the first write, before
//! the write returns, and as `false` once the upload has finished. A written file is queued for
//! upload only when rclone handles its last close, which FUSE delivers after `close` has
//! returned, so until then only its metadata shows it. The format is rclone's own, internal one,
//! read defensively: anything but the shape known here means it cannot be read.

use std::{
	io,
	path::{Path, PathBuf},
};

use serde_json::{Map, Value};

/// Why the metadata could not be read: the format is not the one known here, or a directory or
/// file could not be read.
#[derive(Debug)]
pub(super) struct Unreadable {
	pub(super) path: PathBuf,
	pub(super) reason: String,
}

/// How many files under the metadata root `root` are dirty. A root that does not exist holds
/// none. A file being rewritten is empty or cut short for a moment, as rclone truncates it first,
/// and counts as dirty: it is read again on the next poll. Symlinks are not followed.
pub(super) fn count_dirty(root: &Path) -> Result<u32, Unreadable> {
	let unreadable = |path: &Path, reason: String| Unreadable {
		path: path.to_path_buf(),
		reason,
	};
	let mut dirty = 0_u32;
	let mut dirs = vec![root.to_path_buf()];
	while let Some(dir) = dirs.pop() {
		let entries = match std::fs::read_dir(&dir) {
			Ok(entries) => entries,
			// Removed since it was listed, or no file cached yet.
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => return Err(unreadable(&dir, e.to_string())),
		};
		for entry in entries {
			let entry = entry.map_err(|e| unreadable(&dir, e.to_string()))?;
			let path = entry.path();
			let kind = entry
				.file_type()
				.map_err(|e| unreadable(&path, e.to_string()))?;
			if kind.is_dir() {
				dirs.push(path);
			} else if kind.is_file() && is_dirty(&path)? {
				dirty = dirty.saturating_add(1);
			}
		}
	}
	Ok(dirty)
}

/// Whether the metadata file at `path` marks its file dirty. One removed since it was listed is
/// not: its file was removed or renamed, and a renamed one is listed again under its new name.
fn is_dirty(path: &Path) -> Result<bool, Unreadable> {
	let unreadable = |reason: String| Unreadable {
		path: path.to_path_buf(),
		reason,
	};
	let bytes = match std::fs::read(path) {
		Ok(bytes) => bytes,
		Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
		Err(e) => return Err(unreadable(e.to_string())),
	};
	// An object, which a struct would not insist on: serde reads one from an array too.
	match serde_json::from_slice::<Map<String, Value>>(&bytes) {
		Ok(info) => info
			.get("Dirty")
			.and_then(Value::as_bool)
			.ok_or_else(|| unreadable("no boolean `Dirty` field".to_owned())),
		// Truncated and not yet written again.
		Err(e) if e.is_eof() || e.is_syntax() => Ok(true),
		Err(e) => Err(unreadable(e.to_string())),
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use super::count_dirty;

	/// rclone 1.74's metadata for a file, as `vfscache.Item._save` writes it.
	fn info(dirty: bool) -> String {
		format!(
			"{{\n\t\"ModTime\": \"2026-10-30T20:46:47.4279801Z\",\n\t\"ATime\": \
			 \"2026-10-30T20:46:47.4279801Z\",\n\t\"Size\": 7,\n\t\"Rs\": [\n\t\t{{\n\t\t\t\"Pos\": \
			 0,\n\t\t\t\"Size\": 7\n\t\t}}\n\t],\n\t\"Fingerprint\": \"\",\n\t\"Dirty\": {dirty}\n}}\n"
		)
	}

	fn write(root: &Path, name: &str, content: &str) {
		let path = root.join(name);
		std::fs::create_dir_all(path.parent().unwrap()).unwrap();
		std::fs::write(path, content).unwrap();
	}

	#[test]
	fn counts_the_dirty_files_in_every_directory() {
		let root = tempfile::tempdir().unwrap();
		write(root.path(), "a.txt", &info(true));
		write(root.path(), "b.txt", &info(false));
		write(root.path(), "dir/c.txt", &info(true));
		write(root.path(), "dir/sub/d.txt", &info(false));
		std::fs::create_dir(root.path().join("empty")).unwrap();

		assert_eq!(count_dirty(root.path()).unwrap(), 2);
	}

	#[test]
	fn a_missing_root_holds_nothing() {
		let root = tempfile::tempdir().unwrap();

		assert_eq!(count_dirty(&root.path().join("vfsMeta")).unwrap(), 0);
	}

	#[test]
	fn a_file_being_rewritten_counts_as_dirty() {
		let root = tempfile::tempdir().unwrap();
		write(root.path(), "empty.txt", "");
		let whole = info(false);
		write(
			root.path(),
			"cut.txt",
			whole.get(..whole.len() / 2).unwrap(),
		);

		assert_eq!(count_dirty(root.path()).unwrap(), 2);
	}

	#[test]
	fn another_format_is_unreadable() {
		for content in [
			"{\"Size\": 7}",
			"{\"Dirty\": \"yes\"}",
			"[true]",
			"{\"dirty\": true}",
		] {
			let root = tempfile::tempdir().unwrap();
			write(root.path(), "file.txt", content);

			let unreadable = count_dirty(root.path()).unwrap_err();

			assert_eq!(unreadable.path, root.path().join("file.txt"), "{content}");
		}
	}

	#[cfg(unix)]
	#[test]
	fn symlinks_are_not_followed() {
		let root = tempfile::tempdir().unwrap();
		let outside = tempfile::tempdir().unwrap();
		write(outside.path(), "dir/a.txt", &info(true));
		write(outside.path(), "b.txt", &info(true));
		std::os::unix::fs::symlink(outside.path().join("dir"), root.path().join("dir")).unwrap();
		std::os::unix::fs::symlink(outside.path().join("b.txt"), root.path().join("b.txt"))
			.unwrap();

		assert_eq!(count_dirty(root.path()).unwrap(), 0);
	}
}
