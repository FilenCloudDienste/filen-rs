//! `identify`: which drive item a path in the served root stands for, read from the placeholder's
//! identity and the cache alone. Nothing is hydrated and nothing goes to the server: the
//! placeholder is inspected without opening its data, and the cache answers from what it holds,
//! the remote tree's latest state, even where the local tree has not applied a remote move yet.

use std::sync::Arc;

use filen_mobile_native_cache::{CacheError, ffi::FfiObject};
use filen_types::fs::Uuid;
use tracing::warn;

use crate::{
	drive::Drive,
	helper::{
		blocking,
		platform::Platform,
		protocol::{ErrorCode, IdentifiedItem, ItemPath, ReplyError, RootDir},
	},
	identity::{PlaceholderIdentity, stable_id},
	log_safe,
	os::{LocalEntry, LocalError, LocalFs},
	root_path::RootPath,
};

/// The most directories walked up from an item to the account's root: more is a cycle or a tree
/// no account holds, and the answer is `internal`.
const MAX_HOPS: usize = 4096;

/// How this OS spells a path the parent names.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Spelling {
	/// What separates its components.
	separator: char,
	/// Whether it starts with a drive letter (`X:\`), and takes no `/` either.
	drive: bool,
}

/// Windows's: `X:\` and backslashes, no `/`.
#[cfg(any(windows, test))]
const WINDOWS: Spelling = Spelling {
	separator: '\\',
	drive: true,
};

/// This host's.
#[cfg(windows)]
pub(crate) const HOST: Spelling = WINDOWS;
#[cfg(not(windows))]
pub(crate) const HOST: Spelling = Spelling {
	separator: '/',
	drive: false,
};

/// How each refusal is worded.
pub(crate) const NOT_SERVING: &str = "the root is not served";
pub(crate) const NOT_IN_ROOT: &str = "the path is not inside the served root";
pub(crate) const NOT_SYNCED: &str = "the item is not synced with the drive";
pub(crate) const UNKNOWN_ITEM: &str = "the drive holds no such item, or holds it in the trash";
pub(crate) const NOT_FOUND: &str = "nothing is at the path";
pub(crate) const HELD: &str = "the root is held";

/// The components of `path` below `root`, as `spelling` spells both, compared without case as
/// Windows compares names; empty for the root itself. `None` when `path` is not absolute as
/// `spelling` says, holds a `.` or `..` component (never resolved), or is not `root` or below it.
pub(crate) fn below_root<'a>(
	root: &str,
	path: &'a str,
	spelling: Spelling,
) -> Option<Vec<&'a str>> {
	let absolute = if spelling.drive {
		let mut chars = path.chars();
		let lettered = matches!(
			(chars.next(), chars.next(), chars.next()),
			(Some(letter), Some(':'), Some('\\')) if letter.is_ascii_alphabetic()
		);
		lettered && !path.contains('/')
	} else {
		path.starts_with(spelling.separator)
	};
	if !absolute {
		return None;
	}
	let parts: Vec<&str> = path
		.split(spelling.separator)
		.filter(|part| !part.is_empty())
		.collect();
	if parts.iter().any(|part| *part == "." || *part == "..") {
		return None;
	}
	let root_parts: Vec<String> = root
		.split(spelling.separator)
		.filter(|part| !part.is_empty())
		.map(str::to_lowercase)
		.collect();
	let head = parts.get(..root_parts.len())?;
	let same = head
		.iter()
		.zip(&root_parts)
		.all(|(part, root_part)| part.to_lowercase() == *root_part);
	same.then(|| parts.get(root_parts.len()..).map(<[&str]>::to_vec))
		.flatten()
}

/// A broken invariant of the cache's tree.
fn internal(message: &str) -> ReplyError {
	ReplyError::new(ErrorCode::Internal, message)
}

/// A cache read that failed, logged with its cause; the reply says only that it failed, as the
/// cache's errors may quote what the server answered.
fn cache_failed(error: CacheError) -> ReplyError {
	warn!(error = %log_safe::error(&error), "reading an item from the cache failed");
	ReplyError::new(ErrorCode::Io, "reading the cache failed")
}

/// `text` as a uuid the cache gave, or `internal`.
fn cache_uuid(text: &str) -> Result<Uuid, ReplyError> {
	Uuid::parse_str(text).map_err(|_| internal("the cache holds an id that is not a uuid"))
}

/// The item at `path` in the root at `root`, served for the account whose root directory is
/// `account_root`, read from the root's file system and `drive`'s cache; the root itself is the
/// account's root directory, with no look at the disk.
pub(crate) async fn identify<P: Platform, D: Drive>(
	platform: &Arc<P>,
	root: &RootDir,
	account_root: Uuid,
	drive: &D,
	path: &ItemPath,
) -> Result<IdentifiedItem, ReplyError> {
	let components = below_root(root.as_str(), path.as_str(), HOST)
		.ok_or_else(|| ReplyError::new(ErrorCode::NotInRoot, NOT_IN_ROOT))?;
	if components.is_empty() {
		return Ok(IdentifiedItem::Root { uuid: account_root });
	}
	// A component Windows cannot hold under its spelling is no placeholder's.
	let at = RootPath::parse(components)
		.map_err(|_| ReplyError::new(ErrorCode::NotSynced, NOT_SYNCED))?;
	let entry = {
		let platform = Arc::clone(platform);
		let root = root.clone();
		blocking(move || platform.local_fs(&root).inspect(&at)).await
	};
	let identity = match entry {
		Ok(LocalEntry::Placeholder {
			identity: Ok(identity),
			..
		}) => identity,
		Ok(LocalEntry::Missing) | Err(LocalError::NotFound) => {
			return Err(ReplyError::new(ErrorCode::NotFound, NOT_FOUND));
		}
		Ok(
			LocalEntry::Plain { .. }
			| LocalEntry::Unsupported(_)
			| LocalEntry::Placeholder {
				identity: Err(_), ..
			},
		) => return Err(ReplyError::new(ErrorCode::NotSynced, NOT_SYNCED)),
		Err(error) => return Err(ReplyError::new(ErrorCode::Io, error.to_string())),
	};
	let id = match identity {
		PlaceholderIdentity::Dir { .. } | PlaceholderIdentity::File { .. } => identity.cache_id(),
		// A root's identity below the root, or a local file set apart from the drive.
		PlaceholderIdentity::Root { .. } | PlaceholderIdentity::Detached { .. } => None,
	}
	.ok_or_else(|| ReplyError::new(ErrorCode::NotSynced, NOT_SYNCED))?;
	let cached = drive.query_item(&id).await.map_err(cache_failed)?;
	match (identity, cached) {
		(_, None) => Err(ReplyError::new(ErrorCode::UnknownItem, UNKNOWN_ITEM)),
		(PlaceholderIdentity::File { .. }, Some(FfiObject::File(file))) => {
			if file.original_parent.is_some() {
				return Err(ReplyError::new(ErrorCode::UnknownItem, UNKNOWN_ITEM));
			}
			Ok(IdentifiedItem::File {
				uuid: cache_uuid(&file.uuid)?,
				stable_uuid: cache_uuid(&file.stable_uuid)?,
				parents: parents(drive, &file.parent, account_root).await?,
			})
		}
		(PlaceholderIdentity::Dir { .. }, Some(FfiObject::Dir(dir))) => {
			if dir.original_parent.is_some() {
				return Err(ReplyError::new(ErrorCode::UnknownItem, UNKNOWN_ITEM));
			}
			Ok(IdentifiedItem::Dir {
				uuid: cache_uuid(&dir.uuid)?,
				parents: parents(drive, &dir.parent, account_root).await?,
			})
		}
		(_, Some(FfiObject::File(_) | FfiObject::Dir(_) | FfiObject::Root(_))) => Err(internal(
			"the cache holds the placeholder's item as another kind",
		)),
	}
}

/// The directories from the account's root down to `parent`, the uuid of an item's directory,
/// outermost first, the account's root itself left out: each one the cache holds, not in the
/// trash. `unknownItem` when one is not held or is trashed; `internal` past [`MAX_HOPS`], or on
/// an id that is not a uuid or names no directory.
async fn parents<D: Drive>(
	drive: &D,
	parent: &str,
	account_root: Uuid,
) -> Result<Vec<Uuid>, ReplyError> {
	let mut chain = Vec::new();
	let mut next = cache_uuid(parent)?;
	while next != account_root {
		if chain.len() >= MAX_HOPS {
			return Err(internal("the item is deeper than any directory tree"));
		}
		let cached = drive
			.query_item(&stable_id(next))
			.await
			.map_err(cache_failed)?;
		match cached {
			None => return Err(ReplyError::new(ErrorCode::UnknownItem, UNKNOWN_ITEM)),
			Some(FfiObject::Dir(dir)) if dir.original_parent.is_some() => {
				return Err(ReplyError::new(ErrorCode::UnknownItem, UNKNOWN_ITEM));
			}
			Some(FfiObject::Dir(dir)) => {
				chain.push(next);
				next = cache_uuid(&dir.parent)?;
			}
			Some(FfiObject::File(_) | FfiObject::Root(_)) => {
				return Err(internal("an item's parent is not a directory"));
			}
		}
	}
	chain.reverse();
	Ok(chain)
}

#[cfg(test)]
mod tests {
	use super::*;

	const ROOT: &str = r"C:\Users\u\Filen";

	#[test]
	fn a_windows_path_is_below_the_root_only_as_windows_compares_it() {
		let cases: [(&str, Option<&[&str]>); 15] = [
			(r"C:\Users\u\Filen\a\b.txt", Some(&["a", "b.txt"])),
			(r"c:\users\U\FILEN\a\B.txt", Some(&["a", "B.txt"])),
			(r"C:\Users\u\Filen", Some(&[])),
			(r"C:\Users\u\Filen\", Some(&[])),
			(
				r"C:\Users\u\Filen\my file (1).txt",
				Some(&["my file (1).txt"]),
			),
			(
				r"C:\Users\u\Filen\a&b %x% ^c.txt",
				Some(&["a&b %x% ^c.txt"]),
			),
			(r"C:\Users\u\Filen Dev\a.txt", None),
			(r"C:\Users\u\Filen2\a.txt", None),
			(r"C:\Users\u", None),
			(r"C:\Users\u\Filen\..\x.txt", None),
			(r"C:\Users\u\Filen\.\a.txt", None),
			(r"C:\Users\u\Filen/a.txt", None),
			(r"\\server\share\Filen\a.txt", None),
			(r"\\?\C:\Users\u\Filen\a.txt", None),
			(r"Filen\a.txt", None),
		];
		for (path, expected) in cases {
			assert_eq!(
				below_root(ROOT, path, WINDOWS),
				expected.map(<[&str]>::to_vec),
				"{path:?}"
			);
		}
	}

	#[test]
	fn a_root_spelled_with_a_trailing_separator_holds_the_same_paths() {
		assert_eq!(
			below_root(r"C:\Users\u\Filen\", r"C:\Users\u\Filen\a", WINDOWS),
			Some(vec!["a"])
		);
	}

	#[test]
	fn non_ascii_names_compare_without_case() {
		assert_eq!(
			below_root(
				r"C:\Users\Ünal\Filen",
				r"C:\Users\ünal\Filen\Größe.txt",
				WINDOWS
			),
			Some(vec!["Größe.txt"])
		);
	}
}
