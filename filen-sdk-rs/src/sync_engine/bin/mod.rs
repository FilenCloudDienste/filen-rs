//! The pair's local bin, [`QUARANTINE_DIR`] under the local root: where every local copy the
//! engine moves out of the way goes instead of being destroyed on the spot — a propagated remote
//! deletion, the occupant of a download's or a move's target, the losing side of a resolved
//! conflict.
//!
//! The bin is filed by the UTC day an item entered it:
//! `<root>/.filen-sync-trash/YYYY-MM-DD/<rel_path>`. Each item keeps its tree position under its
//! day's bucket, and two items binned at one path on one day are told apart by a ` (N)` suffix.
//!
//! A bin filed this way says so with [`DATED_MARKER`]; a bin without it may be laid out as it was
//! before, one entry per item at the top level, whatever those entries are named.
//!
//! The bin is not kept forever: a bucket more than [`RETENTION_DAYS`] days older than the day the
//! engine looks at it is deleted PERMANENTLY, with everything in it (see [`prune_bin`]).

use std::{
	ffi::OsStr,
	fs::{self, FileType},
	io,
	path::{Path, PathBuf},
};

use chrono::{Days, NaiveDate};

use super::{
	apply::{io_err, local_path},
	scan::QUARANTINE_DIR,
};

#[cfg(test)]
mod tests;

/// The entry at the bin's top level that says the bin is filed by day, so its top-level names that
/// read as days are buckets. Without it they are not: before the bin was filed by day, its top
/// level held the binned items themselves, and a user's folder named `2019-07-14` sat there under
/// that name. Taking such a folder for a bucket would delete it on the spot.
///
/// [`quarantine_local`] writes it when it creates the bin, and [`prune_bin`] once it has filed an
/// unmarked bin's entries under today. An empty directory rather than a file, so it adds nothing
/// to the files a user looks through to recover one.
const DATED_MARKER: &str = ".filed-by-day";

/// Mark the bin at `bin` as filed by day (see [`DATED_MARKER`]); a bin already marked is left as
/// it is.
fn mark_dated(bin: &Path) -> io::Result<()> {
	match fs::create_dir(bin.join(DATED_MARKER)) {
		Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
		marked => marked,
	}
}

/// The bucket name for the items binned on `day`: `YYYY-MM-DD`.
fn bucket_name(day: NaiveDate) -> String {
	day.format("%Y-%m-%d").to_string()
}

/// The day a top-level bin entry named `name` is the bucket of, or `None` when the name is not a
/// bucket's: exactly `YYYY-MM-DD`, naming a real day. Strict, so nothing the engine did not name
/// is ever taken for a bucket — `2026-13-01`, `2026-1-05` and `+2026-01-05` are all `None`.
fn parse_bucket(name: &OsStr) -> Option<NaiveDate> {
	let name = name.to_str()?;
	let day = NaiveDate::parse_from_str(name, "%Y-%m-%d").ok()?;
	// The round trip is the strictness: chrono's parser accepts unpadded and signed fields that
	// the engine never writes.
	(bucket_name(day) == name).then_some(day)
}

/// The directory holding the items binned on `day`.
fn bucket_dir(root: &Path, day: NaiveDate) -> PathBuf {
	root.join(QUARANTINE_DIR).join(bucket_name(day))
}

/// Move a local item into the pair's bin (recoverable) rather than destroying it, filed under
/// `day`'s bucket at its original tree position. The destination is made UNIQUE on collision
/// (` (N)` suffix): two items binned at the same path on one day must not overwrite each other
/// (data loss), and renaming a directory onto an existing non-empty bin entry would otherwise fail
/// and wedge the pass forever (a re-failing, never-advancing deletion).
///
/// `day` is taken once by the caller — per pass, or per resolution — so every item one pass bins
/// lands in one bucket, however long the pass runs past midnight.
///
/// Returns where the item went, so a caller whose write then does NOT happen can put it back;
/// `None` when there was nothing to move.
pub(super) fn quarantine_local(
	root: &Path,
	rel_path: &str,
	day: NaiveDate,
) -> Result<Option<PathBuf>, crate::Error> {
	let source = local_path(root, rel_path);
	// A missing source is a no-op — e.g. it already moved as part of an ancestor's quarantine.
	if source.symlink_metadata().is_err() {
		return Ok(None);
	}
	let bin = root.join(QUARANTINE_DIR);
	// Only a bin this call creates is known to hold nothing but buckets; one that already exists
	// without the marker may be laid out as before (see [`DATED_MARKER`]).
	let new_bin = matches!(bin.symlink_metadata(), Err(e) if e.kind() == io::ErrorKind::NotFound);
	let dest = unique_quarantine_dest(local_path(&bucket_dir(root, day), rel_path));
	if let Some(parent) = dest.parent() {
		fs::create_dir_all(parent).map_err(io_err)?;
	}
	if new_bin && let Err(e) = mark_dated(&bin) {
		// Not worth failing the move over: an unmarked bin only has its buckets filed under the
		// day of its next prune, which delays their deletion and loses nothing.
		tracing::warn!("cannot mark the sync bin {bin:?} as filed by day: {e}");
	}
	fs::rename(&source, &dest).map_err(io_err)?;
	Ok(Some(dest))
}

/// A bin destination that does not already exist: `base`, else `base (1)`, `base (2)`, ...
/// (the suffix is appended to the whole file name, which is fine for a recovery bin).
fn unique_quarantine_dest(base: PathBuf) -> PathBuf {
	if base.symlink_metadata().is_err() {
		return base;
	}
	let parent = base
		.parent()
		.map(Path::to_path_buf)
		.unwrap_or_else(|| PathBuf::from("."));
	let name = base
		.file_name()
		.and_then(|n| n.to_str())
		.unwrap_or("quarantined");
	for n in 1..100_000 {
		let candidate = parent.join(format!("{name} ({n})"));
		if candidate.symlink_metadata().is_err() {
			return candidate;
		}
	}
	base
}

/// How many days a bucket is kept: one more than this many days old (by the UTC calendar) and it
/// is deleted. A bucket exactly this old is kept, so an item is in the bin for at least 30 whole
/// days. Fixed, not a setting.
pub(super) const RETENTION_DAYS: u64 = 30;

/// [`RETENTION_DAYS`] as a span the calendar can subtract.
const RETENTION: Days = Days::new(RETENTION_DAYS);

/// Whether the bucket of `day` has outlived [`RETENTION`] on `today`: `today - day > 30 days`. A
/// bucket dated after `today` (a clock that was ahead) is kept, and ages normally once the clock
/// passes it.
fn expired(day: NaiveDate, today: NaiveDate) -> bool {
	today
		.checked_sub_days(RETENTION)
		.is_some_and(|cutoff| day < cutoff)
}

/// What one [`prune_bin`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct PruneSummary {
	/// Expired buckets deleted, each with everything in it.
	pub(super) removed: usize,
	/// Top-level entries that were not buckets, moved into today's.
	pub(super) adopted: usize,
	/// Entries that could not be removed or adopted, each logged with its cause.
	pub(super) errors: usize,
	/// Whether `keep_going` stopped the prune before it had looked at every entry.
	pub(super) stopped: bool,
}

/// Prune the bin under `root` as of `today`: delete every bucket that has [`expired`], and move
/// every top-level entry that is NOT a bucket into today's bucket, so it ages from now.
///
/// The un-bucketed entries are the bin as it was laid out before it was filed by day, or anything a
/// user put there; neither has a date to age by, so neither is ever deleted directly. A collision
/// in today's bucket gets the ` (N)` suffix binning gives one.
///
/// In a bin marked as filed by day (see [`DATED_MARKER`]), a bucket is recognised by its name
/// alone (see [`parse_bucket`]); whatever is under an expired one is deleted, including a
/// non-directory under a bucket's name. In an unmarked bin no name is a bucket's but today's: every
/// other entry is adopted, and the bin is marked once all of them have been, so a prune that
/// stopped or failed part of the way adopts the rest next time. Nothing here follows a
/// symbolic link: an entry that is one has the LINK removed (or moved), never its target, and
/// [`fs::remove_dir_all`] does not follow the links inside a bucket either. A bin that is itself a
/// link is left alone, since listing it would reach outside the root.
///
/// Only the bin's top level is listed: a `.filen-sync-trash` further down the tree is the user's
/// content. So is a top-level folder whose name differs from the bin's only in case, which a
/// case-insensitive file system hands back for the bin's: that bin is left alone. A missing root
/// or bin is a no-op, and nothing is ever created but today's bucket, to adopt into, and the
/// marker.
///
/// Best effort: an entry that fails is logged with its cause and counted, and the rest go on.
/// `keep_going` is asked before each entry; `false` stops the prune where it is, leaving every
/// entry not yet reached as it was.
pub(super) fn prune_bin(
	root: &Path,
	today: NaiveDate,
	keep_going: impl Fn() -> bool,
) -> PruneSummary {
	let mut summary = PruneSummary::default();
	let bin = root.join(QUARANTINE_DIR);
	match bin.symlink_metadata() {
		Ok(meta) if meta.is_dir() => {}
		Ok(_) => {
			tracing::warn!("the sync bin {bin:?} is not a directory; not pruning it");
			summary.errors = 1;
			return summary;
		}
		Err(e) if e.kind() == io::ErrorKind::NotFound => return summary,
		Err(e) => {
			tracing::warn!("cannot read the sync bin {bin:?}, not pruning it: {e}");
			summary.errors = 1;
			return summary;
		}
	}
	match named_exactly(root) {
		Ok(true) => {}
		Ok(false) => {
			tracing::warn!(
				"the sync bin's path {bin:?} opens a folder whose name differs from the bin's only \
				 in case; that folder is the user's, not pruning it"
			);
			summary.errors = 1;
			return summary;
		}
		Err(e) => {
			tracing::warn!("cannot list {root:?} to find the sync bin, not pruning it: {e}");
			summary.errors = 1;
			return summary;
		}
	}
	let dated = bin.join(DATED_MARKER).symlink_metadata().is_ok();
	// Listed whole before anything moves: adopting renames entries out of the directory being
	// listed, and a listing taken while its directory changes may skip or repeat entries. Bounded
	// by the bin's top level, which is a few dozen buckets.
	let entries = match fs::read_dir(&bin).and_then(|listing| {
		listing
			.map(|entry| entry.and_then(|entry| Ok((entry.file_name(), entry.file_type()?))))
			.collect::<io::Result<Vec<_>>>()
	}) {
		Ok(entries) => entries,
		Err(e) => {
			tracing::warn!("cannot list the sync bin {bin:?}, not pruning it: {e}");
			summary.errors = 1;
			return summary;
		}
	};
	for (name, kind) in entries {
		if name == DATED_MARKER {
			continue;
		}
		if !keep_going() {
			summary.stopped = true;
			break;
		}
		let path = bin.join(&name);
		match parse_bucket(&name) {
			// Today's bucket stays whether the bin is marked or not: nothing can be adopted into
			// itself, and it is the one an unmarked bin's entries go to.
			Some(day) if day == today || (dated && !expired(day, today)) => {}
			Some(_) if dated => match remove_entry(&path, kind) {
				Ok(()) => summary.removed += 1,
				Err(e) => {
					tracing::warn!("cannot delete the expired sync bin bucket {path:?}: {e}");
					summary.errors += 1;
				}
			},
			_ => match adopt(root, &path, &name, today) {
				Ok(()) => summary.adopted += 1,
				Err(e) => {
					tracing::warn!(
						"cannot move {path:?} into the sync bin's bucket for {today}: {e}"
					);
					summary.errors += 1;
				}
			},
		}
	}
	if !dated
		&& summary.errors == 0
		&& !summary.stopped
		&& let Err(e) = mark_dated(&bin)
	{
		tracing::warn!("cannot mark the sync bin {bin:?} as filed by day: {e}");
		summary.errors += 1;
	}
	summary
}

/// Whether `root` holds an entry named exactly [`QUARANTINE_DIR`]. On a case-insensitive file
/// system `root.join(QUARANTINE_DIR)` also opens a folder named, say, `.Filen-Sync-Trash`, which
/// the scanner, comparing names exactly, syncs as the user's. Lists the root's top level, once a
/// day.
fn named_exactly(root: &Path) -> io::Result<bool> {
	for entry in fs::read_dir(root)? {
		if entry?.file_name() == OsStr::new(QUARANTINE_DIR) {
			return Ok(true);
		}
	}
	Ok(false)
}

/// Delete one top-level bin entry of type `kind` (as [`fs::DirEntry::file_type`] reports it, which
/// does not follow links) and everything under it.
fn remove_entry(path: &Path, kind: FileType) -> io::Result<()> {
	// A link is never `is_dir` here, so this only ever recurses into a real directory.
	if kind.is_dir() {
		return fs::remove_dir_all(path);
	}
	let removed = fs::remove_file(path);
	// Windows removes a link to a directory (or a junction) as a directory; `remove_dir` takes the
	// link, never what it points at.
	#[cfg(windows)]
	if removed.is_err() && kind.is_symlink() {
		return fs::remove_dir(path);
	}
	removed
}

/// Move the un-bucketed bin entry `path`, named `name`, into `today`'s bucket under the same name.
fn adopt(root: &Path, path: &Path, name: &OsStr, today: NaiveDate) -> io::Result<()> {
	let bucket = bucket_dir(root, today);
	fs::create_dir_all(&bucket)?;
	let dest = unique_quarantine_dest(bucket.join(name));
	// `unique_quarantine_dest` gives up after its last suffix and hands back an occupied path,
	// which a rename would overwrite.
	if dest.symlink_metadata().is_ok() {
		return Err(io::Error::new(
			io::ErrorKind::AlreadyExists,
			format!("no free name for it in {bucket:?}"),
		));
	}
	fs::rename(path, dest)
}

#[cfg(test)]
pub(super) mod test_support {
	use std::{
		fs,
		path::{Path, PathBuf},
	};

	use chrono::NaiveDate;

	/// The fixed day the unit tests bin on, so no test depends on the clock.
	pub(crate) fn day() -> NaiveDate {
		NaiveDate::from_ymd_opt(2026, 10, 26).expect("a valid date")
	}

	/// The bucket the items binned on `day` go to.
	pub(crate) fn bucket(root: &Path, day: NaiveDate) -> PathBuf {
		super::bucket_dir(root, day)
	}

	/// Create the bin under `root`, marked as filed by day, as the engine creates it.
	pub(crate) fn dated_bin(root: &Path) {
		let bin = root.join(super::QUARANTINE_DIR);
		fs::create_dir_all(&bin).expect("the bin is created");
		super::mark_dated(&bin).expect("the bin is marked");
	}
}
