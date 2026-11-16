//! The OS-neutral checks on a root the parent asks to register: that it overlaps no directory the
//! helper writes and no other user's root, and that it holds nothing that would be uploaded into
//! the account unasked.

use std::path::{Component, Path, PathBuf, Prefix};

use crate::helper::{
	index::RootEntry,
	protocol::{ErrorCode, ReplyError, UserId},
};

/// `path`'s components as Windows compares them: without case, a verbatim drive prefix
/// (`\\?\C:`) as the drive's, `.` dropped and `..` taking back the component before it (never
/// the prefix or the root).
fn folded(path: &Path) -> Vec<String> {
	let mut parts: Vec<String> = Vec::new();
	let mut fixed = 0;
	for component in path.components() {
		match component {
			Component::CurDir => {}
			Component::ParentDir => {
				if parts.len() > fixed {
					parts.pop();
				}
			}
			Component::Prefix(prefix) => {
				let part = match prefix.kind() {
					Prefix::VerbatimDisk(drive) | Prefix::Disk(drive) => {
						format!("{}:", char::from(drive))
					}
					_ => prefix.as_os_str().to_string_lossy().into_owned(),
				};
				parts.push(part.to_lowercase());
				fixed += 1;
			}
			Component::RootDir => {
				parts.push(String::from("/"));
				fixed += 1;
			}
			Component::Normal(name) => parts.push(name.to_string_lossy().to_lowercase()),
		}
	}
	parts
}

/// `path` with its nearest existing ancestor resolved by the OS: links and junctions followed,
/// short names made long. `None` when nothing of it exists or the OS cannot resolve it.
fn resolved(path: &Path) -> Option<PathBuf> {
	let existing = path.ancestors().find(|at| at.exists())?;
	let rest = path.strip_prefix(existing).ok()?;
	Some(std::fs::canonicalize(existing).ok()?.join(rest))
}

/// `path` both as given and as the OS resolves it.
fn spellings(path: &Path) -> Vec<Vec<String>> {
	let mut spellings = vec![folded(path)];
	if let Some(resolved) = resolved(path) {
		spellings.push(folded(&resolved));
	}
	spellings
}

/// Whether `a` and `b` name the same path, ignoring case.
pub(crate) fn same_path(a: &Path, b: &Path) -> bool {
	folded(a) == folded(b)
}

/// Whether `a` and `b` are the same path or one lies inside the other, ignoring case, as given or
/// as the OS resolves them. Reads the file system.
pub(crate) fn overlaps(a: &Path, b: &Path) -> bool {
	let (a, b) = (spellings(a), spellings(b));
	a.iter()
		.any(|a| b.iter().any(|b| a.starts_with(b) || b.starts_with(a)))
}

/// Refuses `root` for `user` when it overlaps the data directory `data_dir`, the state directory
/// `state_dir` or another user's root or data directory in `entries` (`rootOverlap`), or is another
/// user's root (`rootInUse`); and refuses `data_dir` when it overlaps another user's root or data
/// directory (`rootOverlap`), as unregistering that user would wipe it. The user's own entry is not
/// compared: the command loop takes a register only of the root and data directory it records.
/// Reads the file system.
pub(crate) fn check_overlaps(
	root: &Path,
	data_dir: &Path,
	state_dir: &Path,
	user: UserId,
	entries: &[RootEntry],
) -> Result<(), ReplyError> {
	if overlaps(root, data_dir) {
		return Err(ReplyError::new(
			ErrorCode::RootOverlap,
			"the root overlaps the data directory",
		));
	}
	if overlaps(root, state_dir) {
		return Err(ReplyError::new(
			ErrorCode::RootOverlap,
			"the root overlaps the state directory",
		));
	}
	for entry in entries.iter().filter(|entry| entry.user != user) {
		if same_path(root, &entry.root) {
			return Err(ReplyError::new(
				ErrorCode::RootInUse,
				format!("the root is user {}'s", entry.user),
			));
		}
		if overlaps(root, &entry.root) || overlaps(root, &entry.data_dir) {
			return Err(ReplyError::new(
				ErrorCode::RootOverlap,
				format!("the root overlaps user {}'s root or data", entry.user),
			));
		}
		if overlaps(data_dir, &entry.data_dir) || overlaps(data_dir, &entry.root) {
			return Err(ReplyError::new(
				ErrorCode::RootOverlap,
				format!(
					"the data directory overlaps user {}'s root or data",
					entry.user
				),
			));
		}
	}
	Ok(())
}

/// Makes sure the root directory exists, refusing a root that is not a directory (`rootInvalid`)
/// and, unless the OS holds this provider's registration of it (`ours`), one that holds anything
/// (`rootNotEmpty`): its contents would be uploaded into the account.
pub(crate) fn prepare_root(root: &Path, ours: bool) -> Result<(), ReplyError> {
	let io = |e: std::io::Error| ReplyError::new(ErrorCode::Io, format!("{root:?}: {e}"));
	match std::fs::symlink_metadata(root) {
		Ok(metadata) if !metadata.is_dir() => Err(ReplyError::new(
			ErrorCode::RootInvalid,
			"the root exists and is not a directory",
		)),
		Ok(_) if ours => Ok(()),
		Ok(_) => match std::fs::read_dir(root).map_err(io)?.next() {
			None => Ok(()),
			Some(_) => Err(ReplyError::new(
				ErrorCode::RootNotEmpty,
				"the root holds items and is not this provider's root",
			)),
		},
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			std::fs::create_dir_all(root).map_err(io)
		}
		Err(e) => Err(io(e)),
	}
}

#[cfg(test)]
mod tests {
	use std::path::PathBuf;

	use super::*;
	use crate::helper::protocol::RootDir;

	fn entry(user: u64, root: &str) -> RootEntry {
		RootEntry {
			user: UserId::from(user),
			root: RootDir::parse(root.to_owned()).unwrap(),
			data_dir: PathBuf::from(format!("/data/{user}")),
			account_root: None,
			kept: Vec::new(),
			unregistered: false,
		}
	}

	fn code(result: Result<(), ReplyError>) -> Option<ErrorCode> {
		result.err().map(|error| error.code)
	}

	#[test]
	fn paths_compare_without_case_and_by_whole_components() {
		assert!(same_path(
			Path::new("/Users/A/Filen"),
			Path::new("/users/a/filen")
		));
		assert!(overlaps(Path::new("/a/b"), Path::new("/a/b/c")));
		assert!(overlaps(Path::new("/a/b/c"), Path::new("/A/B")));
		assert!(!overlaps(Path::new("/a/b"), Path::new("/a/bc")));
	}

	#[test]
	fn a_root_overlapping_a_directory_the_helper_writes_is_refused() {
		let user = UserId::from(42);
		let check = |root: &str| {
			code(check_overlaps(
				Path::new(root),
				Path::new("/home/u/data"),
				Path::new("/home/u/state"),
				user,
				&[],
			))
		};
		assert_eq!(check("/home/u/data"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/home/u"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/home/u/state/x"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/home/u/Filen"), None);
	}

	#[test]
	fn another_users_root_is_in_use_and_overlapping_it_is_refused() {
		let entries = [entry(43, "/home/u/Filen"), entry(42, "/home/u/Old")];
		let check = |root: &str| {
			code(check_overlaps(
				Path::new(root),
				Path::new("/d"),
				Path::new("/s"),
				UserId::from(42),
				&entries,
			))
		};
		assert_eq!(check("/home/u/filen"), Some(ErrorCode::RootInUse));
		assert_eq!(check("/home/u/Filen/sub"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/data/43/x"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/home/u/Old/sub"), None);
	}

	#[test]
	fn a_parent_component_is_resolved_before_comparing() {
		assert!(same_path(
			Path::new("/home/u/x/../Data"),
			Path::new("/home/u/data")
		));
		assert!(overlaps(
			Path::new("/home/u/x/../data/Filen"),
			Path::new("/home/u/data")
		));
		assert!(same_path(Path::new("/../a"), Path::new("/a")));
	}

	#[cfg(windows)]
	#[test]
	fn a_verbatim_drive_prefix_names_the_same_path() {
		assert!(same_path(
			Path::new(r"\\?\C:\Users\u\Data"),
			Path::new(r"c:\users\u\data")
		));
		assert!(overlaps(
			Path::new(r"\\?\C:\Users\u\Data\Filen"),
			Path::new(r"C:\Users\u\Data")
		));
	}

	// Unix only: creating a link on Windows needs a privilege the test session may not hold.
	#[cfg(unix)]
	#[test]
	fn a_path_through_a_link_overlaps_what_the_link_names() {
		let temp = tempfile::tempdir().unwrap();
		let real = temp.path().join("real");
		std::fs::create_dir_all(&real).unwrap();
		let link = temp.path().join("link");
		std::os::unix::fs::symlink(&real, &link).unwrap();
		assert!(overlaps(&link.join("Filen"), &real));
		assert!(!overlaps(&temp.path().join("other"), &real));
	}

	#[test]
	fn a_data_directory_another_users_entry_uses_is_refused() {
		let entries = [entry(43, "/home/u/Filen")];
		let check = |data: &str| {
			code(check_overlaps(
				Path::new("/home/u/Mine"),
				Path::new(data),
				Path::new("/s"),
				UserId::from(42),
				&entries,
			))
		};
		assert_eq!(check("/data/43"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/data/43/sub"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/home/u/Filen/d"), Some(ErrorCode::RootOverlap));
		assert_eq!(check("/data/42"), None);
	}

	#[test]
	fn a_missing_root_is_created_and_an_empty_one_taken() {
		let temp = tempfile::tempdir().unwrap();
		let root = temp.path().join("a").join("Filen");
		prepare_root(&root, false).unwrap();
		assert!(root.is_dir());
		prepare_root(&root, false).unwrap();
	}

	#[test]
	fn a_root_holding_anything_is_refused_unless_it_is_ours() {
		let temp = tempfile::tempdir().unwrap();
		std::fs::write(temp.path().join("x.txt"), b"x").unwrap();
		assert_eq!(
			code(prepare_root(temp.path(), false)),
			Some(ErrorCode::RootNotEmpty)
		);
		prepare_root(temp.path(), true).unwrap();
	}

	#[test]
	fn a_root_that_is_a_file_is_invalid() {
		let temp = tempfile::tempdir().unwrap();
		let file = temp.path().join("f");
		std::fs::write(&file, b"x").unwrap();
		assert_eq!(
			code(prepare_root(&file, false)),
			Some(ErrorCode::RootInvalid)
		);
	}
}
