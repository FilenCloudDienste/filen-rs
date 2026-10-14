//! The scan's directory walk: depth-first, in pre-order and in the order each directory lists its
//! entries, following symlinks.
//!
//! On unix it yields what `walkdir`'s `WalkDir::new(start).follow_links(true)` yields, entry for
//! entry and error for error (the parity test below, unix-only, holds it to that): an entry reached
//! through a symlink is stat'd to its target and classified by it, a link that does not resolve is
//! an error at its path, and a followed directory that is one of the walk's own open ancestors is a
//! loop error rather than a second descent. A directory is opened as it is handed over, so one the
//! caller skips is never read and its listing error is never reported.
//!
//! The loop check is `walkdir`'s own: a [`Handle`] opened on the followed directory, compared with
//! one opened on each ancestor in turn (device and inode on unix, volume serial number and file
//! index on Windows), both held open while they are compared. A handle that cannot be opened fails
//! the check as an error that names no path.
//!
//! On Windows `walkdir` opens and keeps a handle to each directory as it descends into it, where
//! this walk, on every platform, opens one only for a loop check, as `walkdir` does on unix. So a
//! directory `walkdir` cannot open a handle to is, there, one error with no path in place of the
//! directory; here it is the directory, then its listing error at its path if it cannot be listed
//! either. And a loop check costs a handle opened per ancestor, where `walkdir` reuses the ones it
//! keeps: it is made only for a symlink or junction to a directory.

use std::{
	ffi::OsStr,
	fs::{self, FileType, ReadDir},
	io,
	path::{Path, PathBuf},
	vec,
};

use same_file::Handle;

/// How many directory streams the walk keeps open at once. A walk deeper than this reads the
/// listing of its outermost open directory into memory and closes it, so the walk's file
/// descriptors stay bounded however deep the tree is (`walkdir`'s default too).
const MAX_OPEN: usize = 10;

/// One entry the walk reached.
#[derive(Debug)]
pub(super) struct Entry {
	path: PathBuf,
	/// The type of what the entry leads to: a symlink is classified by its target.
	file_type: FileType,
	/// Whether the walk reached this through a symlink.
	followed: bool,
	/// How far below the start: `0` for the start itself.
	depth: usize,
	/// What the listing said about an entry not reached through a symlink. On Windows the listing
	/// carries it, so asking for it again would cost a call per entry.
	#[cfg(windows)]
	listed_metadata: fs::Metadata,
}

impl Entry {
	/// The entry at `path`, stat'd through a symlink when `follow` is set.
	fn at(path: PathBuf, depth: usize, follow: bool) -> Result<Self, WalkError> {
		let metadata = if follow {
			fs::metadata(&path)
		} else {
			fs::symlink_metadata(&path)
		};
		match metadata {
			Ok(metadata) => Ok(Self {
				path,
				file_type: metadata.file_type(),
				followed: follow,
				depth,
				#[cfg(windows)]
				listed_metadata: metadata,
			}),
			Err(source) => Err(WalkError::Io {
				path: Some(path),
				source,
			}),
		}
	}

	/// The entry a directory listed, not yet followed.
	fn listed(entry: &fs::DirEntry, depth: usize) -> Result<Self, WalkError> {
		let failed = |source| WalkError::Io {
			path: Some(entry.path()),
			source,
		};
		Ok(Self {
			path: entry.path(),
			file_type: entry.file_type().map_err(failed)?,
			followed: false,
			depth,
			#[cfg(windows)]
			listed_metadata: entry.metadata().map_err(failed)?,
		})
	}

	pub(super) fn path(&self) -> &Path {
		&self.path
	}

	pub(super) fn file_name(&self) -> &OsStr {
		self.path.file_name().unwrap_or(self.path.as_os_str())
	}

	/// The type of what the entry leads to (a directory or a file for a symlink to one).
	pub(super) fn file_type(&self) -> FileType {
		self.file_type
	}

	pub(super) fn path_is_symlink(&self) -> bool {
		self.followed
	}

	pub(super) fn depth(&self) -> usize {
		self.depth
	}

	/// The entry's metadata, read now: through the symlink for an entry reached through one, of
	/// the entry itself otherwise (on Windows, what its listing said).
	pub(super) fn metadata(&self) -> io::Result<fs::Metadata> {
		if self.followed {
			return fs::metadata(&self.path);
		}
		cfg_select! {
			windows => { Ok(self.listed_metadata.clone()) }
			_ => { fs::symlink_metadata(&self.path) }
		}
	}
}

/// What the walk could not read.
#[derive(Debug)]
pub(super) enum WalkError {
	/// An entry, a listing or a symlink that could not be read. `path` is `None` when the failure
	/// has no one entry to name: a listing that failed partway, or an open the loop check made.
	Io {
		path: Option<PathBuf>,
		source: io::Error,
	},
	/// A symlink at `child` leading back onto `ancestor`, a directory the walk is inside.
	Loop { ancestor: PathBuf, child: PathBuf },
}

impl WalkError {
	/// The path the error is about: the link, for a loop.
	pub(super) fn path(&self) -> Option<&Path> {
		match self {
			Self::Io { path, .. } => path.as_deref(),
			Self::Loop { child, .. } => Some(child),
		}
	}
}

type Listing = Result<Entry, WalkError>;

/// What is left of one directory's listing.
enum Remaining {
	/// Still being read from the directory's open stream.
	Open { stream: ReadDir, depth: usize },
	/// Read into memory: the stream was closed to stay under [`MAX_OPEN`], or failed to open.
	Read(vec::IntoIter<Listing>),
}

impl Iterator for Remaining {
	type Item = Listing;

	fn next(&mut self) -> Option<Listing> {
		match self {
			Self::Open { stream, depth } => stream.next().map(|listed| {
				listed
					.map_err(|source| WalkError::Io { path: None, source })
					.and_then(|listed| Entry::listed(&listed, *depth))
			}),
			Self::Read(rest) => rest.next(),
		}
	}
}

/// A directory the walk is inside.
struct Dir {
	path: PathBuf,
	remaining: Remaining,
}

/// The walk from one start (see the module docs).
pub(super) struct Walk {
	/// The start, until the first [`next`](Iterator::next) reads it.
	start: Option<PathBuf>,
	/// The directories the walk is inside, innermost last: the ones it lists from, and the
	/// ancestors a followed directory is checked against.
	inside: Vec<Dir>,
	/// The outermost of `inside` whose stream is still open; every one before it was read into
	/// memory.
	oldest_open: usize,
}

impl Walk {
	pub(super) fn new(start: &Path) -> Self {
		Self {
			start: Some(start.to_owned()),
			inside: Vec::new(),
			oldest_open: 0,
		}
	}

	/// Leave the directory last handed over unread from here on, everything under it included.
	pub(super) fn skip_current_dir(&mut self) {
		self.leave();
	}

	fn leave(&mut self) {
		self.inside.pop();
		self.oldest_open = self.oldest_open.min(self.inside.len());
	}

	/// Follow `entry` if it is a symlink, and open it if it is a directory, which the walk then
	/// lists before anything after it.
	fn reach(&mut self, entry: Entry) -> Listing {
		let entry = if entry.file_type.is_symlink() {
			self.follow(entry)?
		} else {
			entry
		};
		if entry.file_type.is_dir() {
			self.open(&entry);
		}
		Ok(entry)
	}

	fn follow(&self, entry: Entry) -> Listing {
		let entry = Entry::at(entry.path, entry.depth, true)?;
		// Only a directory can loop.
		if entry.file_type.is_dir() {
			self.check_loop(&entry.path)?;
		}
		Ok(entry)
	}

	fn check_loop(&self, child: &Path) -> Result<(), WalkError> {
		let unchecked = |source| WalkError::Io { path: None, source };
		let child_handle = Handle::from_path(child).map_err(unchecked)?;
		for ancestor in self.inside.iter().rev() {
			if Handle::from_path(&ancestor.path).map_err(unchecked)? == child_handle {
				return Err(WalkError::Loop {
					ancestor: ancestor.path.clone(),
					child: child.to_owned(),
				});
			}
		}
		Ok(())
	}

	fn open(&mut self, dir: &Entry) {
		if self.inside.len() - self.oldest_open == MAX_OPEN {
			if let Some(oldest) = self.inside.get_mut(self.oldest_open) {
				let rest = (&mut oldest.remaining).collect::<Vec<_>>();
				oldest.remaining = Remaining::Read(rest.into_iter());
			}
			self.oldest_open += 1;
		}
		let remaining = match fs::read_dir(&dir.path) {
			Ok(stream) => Remaining::Open {
				stream,
				depth: dir.depth + 1,
			},
			// Reported as the next thing listed, which a caller skipping the directory never sees.
			Err(source) => Remaining::Read(
				vec![Err(WalkError::Io {
					path: Some(dir.path.clone()),
					source,
				})]
				.into_iter(),
			),
		};
		self.inside.push(Dir {
			path: dir.path.clone(),
			remaining,
		});
	}
}

impl Iterator for Walk {
	type Item = Listing;

	fn next(&mut self) -> Option<Listing> {
		if let Some(start) = self.start.take() {
			return Some(Entry::at(start, 0, false).and_then(|start| self.reach(start)));
		}
		while let Some(dir) = self.inside.last_mut() {
			match dir.remaining.next() {
				None => self.leave(),
				Some(listed) => return Some(listed.and_then(|entry| self.reach(entry))),
			}
		}
		None
	}
}

#[cfg(all(test, unix))]
mod tests {
	use std::{
		cell::RefCell,
		os::unix::fs::{PermissionsExt, symlink},
	};

	use uuid::Uuid;

	use super::*;

	/// One yielded item, with every path relative to the base of the tree, as both walks are compared.
	#[derive(Debug, PartialEq, Eq)]
	enum Seen {
		Entry {
			path: PathBuf,
			depth: usize,
			is_dir: bool,
			is_file: bool,
			is_symlink: bool,
			path_is_symlink: bool,
			metadata_is_dir: Option<bool>,
		},
		Error {
			path: Option<PathBuf>,
			loop_ancestor: Option<PathBuf>,
			kind: Option<io::ErrorKind>,
		},
	}

	fn relative(root: &Path, path: &Path) -> PathBuf {
		path.strip_prefix(root).unwrap_or(path).to_owned()
	}

	/// A tree with what the scan meets: nested and deep directories, a link to a directory inside
	/// the root and one outside it, a loop, a dangling link, a link to a file, a link to a
	/// directory that cannot be read, an unreadable directory, an unreadable rule file, and one
	/// directory (`gone`) the walks remove as they reach it. Directories named `skip*` are skipped
	/// by the caller, one of them unreadable.
	fn build(base: &Path) {
		let root = base.join("root");
		let outside = base.join("outside");
		fs::create_dir_all(outside.join("o")).unwrap();
		fs::write(outside.join("o").join("f.txt"), b"o").unwrap();
		let mut deep = root.join("deep");
		for level in 0..14 {
			deep = deep.join(format!("l{level}"));
		}
		fs::create_dir_all(&deep).unwrap();
		fs::write(deep.join("bottom.txt"), b"b").unwrap();
		for dir in [
			"a/b/c",
			"real/sub",
			"skip_me/x",
			"skip_locked",
			"locked",
			"gone/g",
		] {
			fs::create_dir_all(root.join(dir)).unwrap();
		}
		for (index, file) in [
			"a/1.txt",
			"a/b/2.txt",
			"a/b/c/3.txt",
			"real/sub/r.txt",
			"skip_me/x/s.txt",
			"gone/g/z.txt",
		]
		.iter()
		.enumerate()
		{
			fs::write(root.join(file), index.to_string()).unwrap();
		}
		for wide in 0..40 {
			fs::write(root.join("a").join(format!("w{wide:02}")), b"w").unwrap();
		}
		fs::write(root.join("a").join(".filenignore"), b"*.tmp").unwrap();
		symlink(root.join("real"), root.join("alias")).unwrap();
		symlink("sub", root.join("real").join("inner")).unwrap();
		symlink(&outside, root.join("away")).unwrap();
		symlink(root.join("nowhere"), root.join("dangling")).unwrap();
		symlink(root.join("a").join("1.txt"), root.join("file_link")).unwrap();
		symlink("..", root.join("a").join("b").join("up")).unwrap();
		symlink(root.join("locked"), root.join("to_locked")).unwrap();
		fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();
		fs::set_permissions(root.join("skip_locked"), fs::Permissions::from_mode(0o000)).unwrap();
		fs::set_permissions(
			root.join("a").join(".filenignore"),
			fs::Permissions::from_mode(0o000),
		)
		.unwrap();
		// Root reads them anyway, and both walks would then agree without covering them.
		assert!(
			fs::read_dir(root.join("locked")).is_err(),
			"run as non-root: the unreadable cases need permissions enforced"
		);
	}

	/// A fresh directory under the system temp dir, removed with everything under it when dropped,
	/// so a failed assertion leaves nothing behind, unreadable directories included.
	struct TempRoot(PathBuf);

	impl TempRoot {
		fn new() -> Self {
			let path = std::env::temp_dir().join(format!("filen_walk_test_{}", Uuid::new_v4()));
			fs::create_dir_all(&path).unwrap();
			Self(path)
		}
	}

	impl std::ops::Deref for TempRoot {
		type Target = Path;

		fn deref(&self) -> &Path {
			&self.0
		}
	}

	impl Drop for TempRoot {
		fn drop(&mut self) {
			unlock(&self.0);
			fs::remove_dir_all(&self.0).ok();
		}
	}

	/// Make every directory under `path` readable again, so that it can be removed.
	fn unlock(path: &Path) {
		if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir()) {
			fs::set_permissions(path, fs::Permissions::from_mode(0o755)).ok();
			for entry in fs::read_dir(path).into_iter().flatten().flatten() {
				unlock(&entry.path());
			}
		}
	}

	/// What a walk over `start` yields, skipping `skip*` and removing `gone` once it is handed over.
	fn walk_with<E>(
		root: &Path,
		mut next: impl FnMut() -> Option<Result<Seen, E>>,
		mut skip: impl FnMut(),
		error: impl Fn(E) -> Seen,
	) -> Vec<Seen> {
		let mut seen = Vec::new();
		while let Some(item) = next() {
			let item = item.unwrap_or_else(&error);
			if let Seen::Entry { path, is_dir, .. } = &item {
				let name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
				if *is_dir && name.starts_with("skip") {
					skip();
				}
				if name == "gone" {
					fs::remove_dir_all(root.join(path)).unwrap();
				}
			}
			seen.push(item);
		}
		seen
	}

	fn ours(root: &Path, start: &Path) -> Vec<Seen> {
		let walk = RefCell::new(Walk::new(start));
		walk_with(
			root,
			|| {
				walk.borrow_mut().next().map(|item| {
					item.map(|entry| Seen::Entry {
						path: relative(root, entry.path()),
						depth: entry.depth(),
						is_dir: entry.file_type().is_dir(),
						is_file: entry.file_type().is_file(),
						is_symlink: entry.file_type().is_symlink(),
						path_is_symlink: entry.path_is_symlink(),
						metadata_is_dir: entry.metadata().ok().map(|m| m.is_dir()),
					})
				})
			},
			|| walk.borrow_mut().skip_current_dir(),
			|err| Seen::Error {
				path: err.path().map(|path| relative(root, path)),
				loop_ancestor: match &err {
					WalkError::Loop { ancestor, .. } => Some(relative(root, ancestor.as_path())),
					WalkError::Io { .. } => None,
				},
				kind: match err {
					WalkError::Io { source, .. } => Some(source.kind()),
					WalkError::Loop { .. } => None,
				},
			},
		)
	}

	fn walkdirs(root: &Path, start: &Path) -> Vec<Seen> {
		let walk = RefCell::new(walkdir::WalkDir::new(start).follow_links(true).into_iter());
		walk_with(
			root,
			|| {
				walk.borrow_mut().next().map(|item| {
					item.map(|entry| Seen::Entry {
						path: relative(root, entry.path()),
						depth: entry.depth(),
						is_dir: entry.file_type().is_dir(),
						is_file: entry.file_type().is_file(),
						is_symlink: entry.file_type().is_symlink(),
						path_is_symlink: entry.path_is_symlink(),
						metadata_is_dir: entry.metadata().ok().map(|m| m.is_dir()),
					})
				})
			},
			|| walk.borrow_mut().skip_current_dir(),
			|err| Seen::Error {
				path: err.path().map(|path| relative(root, path)),
				loop_ancestor: err.loop_ancestor().map(|ancestor| relative(root, ancestor)),
				kind: err.into_io_error().map(|source| source.kind()),
			},
		)
	}

	/// Both walks over a fresh copy of the same tree, from `start` (relative to the copy's base).
	/// Each walk lists its own copy, so this holds only where two directories built the same way
	/// list in the same order: APFS, ext4 and tmpfs do; a filesystem salting its hash order per
	/// directory (ZFS) need not.
	fn compare(start: impl Fn(&Path) -> PathBuf) -> Vec<Seen> {
		let base = TempRoot::new();
		let (theirs_base, ours_base) = (base.join("theirs"), base.join("ours"));
		build(&theirs_base);
		build(&ours_base);
		let theirs = walkdirs(&theirs_base, &start(&theirs_base));
		let mine = ours(&ours_base, &start(&ours_base));
		// The first item the two differ at, rather than two whole walks to compare by eye.
		let first_difference = mine
			.iter()
			.zip(&theirs)
			.position(|(mine, theirs)| mine != theirs);
		assert_eq!(
			(first_difference, mine.len()),
			(None, theirs.len()),
			"ours {:#?} walkdir {:#?}",
			first_difference.map(|at| &mine[at]),
			first_difference.map(|at| &theirs[at]),
		);
		theirs
	}

	#[test]
	fn walks_a_tree_as_walkdir_following_links_does() {
		let seen = compare(|base| base.join("root"));
		assert!(seen.len() > 60, "the tree is walked: {seen:#?}");
		assert!(
			seen.iter().any(|seen| matches!(
				seen,
				Seen::Error {
					loop_ancestor: Some(_),
					..
				}
			)),
			"the loop is met: {seen:#?}"
		);
	}

	#[test]
	fn walks_a_root_reached_through_a_symlink_as_walkdir_does() {
		compare(|base| {
			let link = base.join("root_link");
			symlink(base.join("root"), &link).unwrap();
			link
		});
	}

	#[test]
	fn walks_a_subtree_start_as_walkdir_does() {
		compare(|base| base.join("root").join("deep").join("l0"));
	}

	#[test]
	fn a_missing_start_is_one_error_at_its_path() {
		let base = TempRoot::new();
		let start = base.join("missing");
		let mut walk = Walk::new(&start);
		match walk.next() {
			Some(Err(WalkError::Io {
				path: Some(path),
				source,
			})) => {
				assert_eq!(path, start);
				assert_eq!(source.kind(), io::ErrorKind::NotFound);
			}
			other => panic!("expected the start's own error, got {other:?}"),
		}
		assert!(walk.next().is_none());
	}

	#[test]
	fn a_walk_deeper_than_max_open_keeps_at_most_max_open_streams_open() {
		let base = TempRoot::new();
		let mut deep = base.to_path_buf();
		for level in 0..3 * MAX_OPEN {
			deep = deep.join(format!("l{level}"));
			fs::create_dir_all(&deep).unwrap();
			fs::write(deep.join("f.txt"), b"f").unwrap();
		}
		let mut walk = Walk::new(&base);
		let (mut items, mut deepest) = (0, 0);
		while let Some(item) = walk.next() {
			item.unwrap();
			items += 1;
			deepest = deepest.max(walk.inside.len());
			let open = walk
				.inside
				.iter()
				.filter(|dir| matches!(dir.remaining, Remaining::Open { .. }))
				.count();
			assert!(
				open <= MAX_OPEN,
				"{open} streams open at depth {}",
				walk.inside.len()
			);
		}
		assert_eq!(items, 1 + 2 * 3 * MAX_OPEN);
		assert_eq!(deepest, 1 + 3 * MAX_OPEN);
	}
}
