//! The scan's directory walk: depth-first, in pre-order and in the order each directory lists its
//! entries, following symlinks.
//!
//! An entry reached through a symlink is stat'd to its target and classified by it, a link that
//! does not resolve is an error at its path, and a followed directory that is one of the walk's own
//! open ancestors is a loop error rather than a second descent.
//!
//! A directory is not opened when it is handed over, only when the walk descends into it: on the
//! next [`next`](Iterator::next) unless the caller skipped it first, or at [`Walk::enter`]. A
//! directory the caller skips is never opened, by its listing or by the loop check, so a link to a
//! folder the operating system guards (macOS asks the user before an app reads `~/Documents`) asks
//! nothing when the scan ignores it.
//!
//! The loop check is `walkdir`'s own: a [`Handle`] opened on the followed directory, compared with
//! one opened on each ancestor in turn (device and inode on unix, volume serial number and file
//! index on Windows), both held open while they are compared. A handle that cannot be opened fails
//! the check: at the followed directory's path for its own handle, as an error that names no path
//! for an ancestor's.
//!
//! On unix it yields what `walkdir`'s `WalkDir::new(start).follow_links(true)` yields, entry for
//! entry and error for error (the parity test below, unix-only, holds it to that), except where the
//! descent decides. `walkdir` opens a directory and makes the loop check as it hands the directory
//! over, so a followed directory that fails the check is, there, one error in place of the entry,
//! and one the caller would have skipped is reported all the same. Here the entry is handed over,
//! then the error follows if the caller descends, and nothing follows if it skips. A directory
//! removed between its hand-over and its descent is a listing error here, where `walkdir` had it
//! open already.
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
	/// has no one entry to name: a listing that failed partway, or an ancestor's handle the loop
	/// check could not open.
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

/// A directory handed over and not yet descended into.
struct Unentered {
	path: PathBuf,
	depth: usize,
	/// Whether it was reached through a symlink, and so is checked for a loop.
	followed: bool,
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
	/// The directory last handed over, until the walk descends into it or the caller skips it.
	unentered: Option<Unentered>,
}

impl Walk {
	pub(super) fn new(start: &Path) -> Self {
		Self {
			start: Some(start.to_owned()),
			inside: Vec::new(),
			oldest_open: 0,
			unentered: None,
		}
	}

	/// Leave the directory last handed over unread from here on, everything under it included. It
	/// is never opened, unless [`enter`](Self::enter) opened it already.
	pub(super) fn skip_current_dir(&mut self) {
		if self.unentered.take().is_none() {
			self.leave();
		}
	}

	/// Descend into the directory last handed over now rather than on the next
	/// [`next`](Iterator::next): check it for a loop if it was reached through a symlink, and open
	/// it. A caller that records each directory it admits calls this first, so a loop is known
	/// before the directory is recorded. On an error the directory is left unread and the walk goes
	/// on after it, as if it had been skipped; the caller must not skip it again. Nothing to do if
	/// the last thing handed over was not a directory, or was entered already.
	pub(super) fn enter(&mut self) -> Result<(), WalkError> {
		let Some(dir) = self.unentered.take() else {
			return Ok(());
		};
		// Only a directory can loop, and only one reached through a symlink can be an ancestor.
		if dir.followed {
			self.check_loop(&dir.path)?;
		}
		self.open(dir);
		Ok(())
	}

	fn leave(&mut self) {
		self.inside.pop();
		self.oldest_open = self.oldest_open.min(self.inside.len());
	}

	/// Follow `entry` if it is a symlink, and hold it for the descent if it is a directory, which
	/// the walk then lists before anything after it. Following a link stats its target, which does
	/// not open it.
	fn reach(&mut self, entry: Entry) -> Listing {
		let entry = if entry.file_type.is_symlink() {
			Entry::at(entry.path, entry.depth, true)?
		} else {
			entry
		};
		if entry.file_type.is_dir() {
			self.unentered = Some(Unentered {
				// Its own copy: the entry goes to the caller, and the walk needs the path until the
				// descent.
				path: entry.path.clone(),
				depth: entry.depth,
				followed: entry.followed,
			});
		}
		Ok(entry)
	}

	fn check_loop(&self, child: &Path) -> Result<(), WalkError> {
		let child_handle = Handle::from_path(child).map_err(|source| WalkError::Io {
			path: Some(child.to_owned()),
			source,
		})?;
		for ancestor in self.inside.iter().rev() {
			let ancestor_handle = Handle::from_path(&ancestor.path)
				.map_err(|source| WalkError::Io { path: None, source })?;
			if ancestor_handle == child_handle {
				return Err(WalkError::Loop {
					ancestor: ancestor.path.clone(),
					child: child.to_owned(),
				});
			}
		}
		Ok(())
	}

	fn open(&mut self, dir: Unentered) {
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
			// Reported as the next thing listed.
			Err(source) => Remaining::Read(
				vec![Err(WalkError::Io {
					path: Some(dir.path.clone()),
					source,
				})]
				.into_iter(),
			),
		};
		self.inside.push(Dir {
			path: dir.path,
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
		if let Err(err) = self.enter() {
			return Some(Err(err));
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

	/// What `walkdir` yields where this walk yields `seen`. The two differ only where `walkdir` decides
	/// at hand-over what this walk decides at the descent (see the module docs). A followed
	/// directory this walk hands over and then fails the loop check of is, in `walkdir`, the check's
	/// error alone in place of the entry; none of the directories that fail it here are skipped, so
	/// `walkdir` reports the same error (one that is skipped is where the two disagree outright, see
	/// `a_followed_directory_the_caller_skips_is_never_opened`). And `gone`, removed once handed
	/// over, was open in `walkdir` already, which then lists nothing from it; here it is opened after
	/// its removal, and its listing fails. A handle the check cannot open on a followed directory
	/// is an error at its path here and one with no path in `walkdir`.
	fn as_walkdir_orders_it(seen: Vec<Seen>) -> Vec<Seen> {
		let mut ordered: Vec<Seen> = Vec::with_capacity(seen.len());
		for item in seen {
			let item = match item {
				Seen::Error {
					path: Some(path),
					loop_ancestor: None,
					kind,
				} if matches!(
					ordered.last(),
					Some(Seen::Entry {
						path: followed,
						is_dir: true,
						path_is_symlink: true,
						..
					}) if *followed == path
				) =>
				{
					Seen::Error {
						path: None,
						loop_ancestor: None,
						kind,
					}
				}
				item => item,
			};
			if let Seen::Error {
				path: Some(path),
				loop_ancestor: None,
				kind: Some(io::ErrorKind::NotFound),
			} = &item && path.file_name() == Some(OsStr::new("gone"))
			{
				continue;
			}
			let failed_check = matches!(
				item,
				Seen::Error { path: None, .. }
					| Seen::Error {
						loop_ancestor: Some(_),
						..
					}
			);
			if failed_check
				&& matches!(
					ordered.last(),
					Some(Seen::Entry {
						is_dir: true,
						path_is_symlink: true,
						..
					})
				) {
				ordered.pop();
			}
			ordered.push(item);
		}
		ordered
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
		let mine = as_walkdir_orders_it(ours(&ours_base, &start(&ours_base)));
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
	fn a_followed_directory_the_caller_skips_is_never_opened() {
		let base = TempRoot::new();
		let (root, locked) = (base.join("root"), base.join("locked"));
		fs::create_dir_all(&root).unwrap();
		fs::create_dir_all(&locked).unwrap();
		let link = root.join("skip_link");
		symlink(&locked, &link).unwrap();
		// A stat through the link still answers; opening the directory does not.
		fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
		let unopenable = fs::read_dir(&locked).is_err();

		let theirs = walkdir::WalkDir::new(&root)
			.follow_links(true)
			.into_iter()
			.map(|item| {
				item.map(|entry| entry.path().to_owned()).map_err(|err| {
					(
						err.path().map(Path::to_owned),
						err.io_error().map(io::Error::kind),
					)
				})
			})
			.collect::<Vec<_>>();
		let mut walk = Walk::new(&root);
		let mut mine = Vec::new();
		while let Some(item) = walk.next() {
			let item = item.map(|entry| entry.path().to_owned());
			if item.as_ref().is_ok_and(|path| *path == link) {
				walk.skip_current_dir();
			}
			mine.push(item.map_err(|err| err.path().map(Path::to_owned)));
		}

		assert!(
			unopenable,
			"run as non-root: the case needs permissions enforced"
		);
		// `walkdir` opens the target for its loop check before the caller can skip the link, and
		// fails there: an error in place of the entry, which no skip can take back. This walk is
		// meant to differ here.
		assert_eq!(
			theirs,
			vec![
				Ok(root.clone()),
				Err((None, Some(io::ErrorKind::PermissionDenied)))
			]
		);
		assert_eq!(mine, vec![Ok(root), Ok(link)]);
	}

	#[test]
	fn a_loop_under_an_entered_link_is_reported_at_the_link_after_its_entry() {
		let base = TempRoot::new();
		let root = base.join("root");
		fs::create_dir_all(root.join("a")).unwrap();
		let link = root.join("a").join("up");
		symlink("..", &link).unwrap();

		let theirs = walkdir::WalkDir::new(&root)
			.follow_links(true)
			.into_iter()
			.map(|item| {
				item.map(|entry| entry.path().to_owned()).map_err(|err| {
					(
						err.path().map(Path::to_owned),
						err.loop_ancestor().map(Path::to_owned),
					)
				})
			})
			.collect::<Vec<_>>();
		let loops = |enter: bool| {
			let mut walk = Walk::new(&root);
			let mut seen = Vec::new();
			while let Some(item) = walk.next() {
				match item {
					Ok(entry) => {
						seen.push(Ok(entry.path().to_owned()));
						if enter && entry.path() == link {
							match walk.enter() {
								Err(WalkError::Loop { ancestor, child }) => {
									seen.push(Err((Some(child), Some(ancestor))));
								}
								other => panic!("expected the loop, got {other:?}"),
							}
						}
					}
					Err(WalkError::Loop { ancestor, child }) => {
						seen.push(Err((Some(child), Some(ancestor))));
					}
					Err(err) => panic!("unexpected {err:?}"),
				}
			}
			seen
		};
		let (on_next, on_enter) = (loops(false), loops(true));

		let loop_error = Err((Some(link.clone()), Some(root.clone())));
		// The same error at the same link as `walkdir`'s, which reports it in place of the entry.
		assert_eq!(
			theirs,
			vec![Ok(root.clone()), Ok(root.join("a")), loop_error.clone()]
		);
		let expected = vec![
			Ok(root.clone()),
			Ok(root.join("a")),
			Ok(link.clone()),
			loop_error,
		];
		assert_eq!(on_next, expected, "found as the walk descends");
		assert_eq!(on_enter, expected, "found when the caller enters it");
	}

	/// Walks a root holding one plain directory, removes that directory once it is handed over, and
	/// returns what the walk yields after it: the caller skips it first when `skip` is set.
	fn after_removing_a_handed_over_dir(skip: bool) -> (PathBuf, Vec<Listing>) {
		let base = TempRoot::new();
		let dir = base.join("d");
		fs::create_dir_all(&dir).unwrap();
		let mut walk = Walk::new(&base);
		assert_eq!(walk.next().unwrap().unwrap().path(), &*base);
		assert_eq!(walk.next().unwrap().unwrap().path(), dir);
		fs::remove_dir(&dir).unwrap();
		if skip {
			walk.skip_current_dir();
		}
		(dir, walk.collect())
	}

	#[test]
	fn a_plain_directory_is_listed_only_once_the_walk_descends_into_it() {
		let (dir, rest) = after_removing_a_handed_over_dir(false);
		// Listed at hand-over, the removed directory would read as empty, with no error.
		match rest.as_slice() {
			[
				Err(WalkError::Io {
					path: Some(path),
					source,
				}),
			] => {
				assert_eq!(*path, dir);
				assert_eq!(source.kind(), io::ErrorKind::NotFound);
			}
			other => panic!("expected the directory's listing error, got {other:?}"),
		}
	}

	#[test]
	fn a_plain_directory_the_caller_skips_is_never_listed() {
		let (_dir, rest) = after_removing_a_handed_over_dir(true);
		assert!(rest.is_empty(), "{rest:?}");
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
