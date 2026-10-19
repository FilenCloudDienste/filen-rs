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
	/// Still being read from the directory's open stream. Boxed because Windows' `ReadDir` holds
	/// a whole `WIN32_FIND_DATAW` (over 600 bytes), which every directory read into memory would
	/// otherwise carry too.
	Open { stream: Box<ReadDir>, depth: usize },
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
				stream: Box::new(stream),
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
mod tests;
