//! The pair's local bin, [`QUARANTINE_DIR`] under the local root: where every local copy the
//! engine moves out of the way goes instead of being destroyed — a propagated remote deletion, the
//! occupant of a download's or a move's target, the losing side of a resolved conflict.
//!
//! The bin is filed by the UTC day an item entered it:
//! `<root>/.filen-sync-trash/YYYY-MM-DD/<rel_path>`. Each item keeps its tree position under its
//! day's bucket, and two items binned at one path on one day are told apart by a ` (N)` suffix.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use super::{
	apply::{io_err, local_path},
	scan::QUARANTINE_DIR,
};

#[cfg(test)]
mod tests;

/// The bucket name for the items binned on `day`: `YYYY-MM-DD`.
fn bucket_name(day: NaiveDate) -> String {
	day.format("%Y-%m-%d").to_string()
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
	let dest = unique_quarantine_dest(local_path(&bucket_dir(root, day), rel_path));
	if let Some(parent) = dest.parent() {
		std::fs::create_dir_all(parent).map_err(io_err)?;
	}
	std::fs::rename(&source, &dest).map_err(io_err)?;
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

#[cfg(test)]
pub(super) mod test_support {
	use std::path::{Path, PathBuf};

	use chrono::NaiveDate;

	/// The fixed day the unit tests bin on, so no test depends on the clock.
	pub(crate) fn day() -> NaiveDate {
		NaiveDate::from_ymd_opt(2026, 10, 26).expect("a valid date")
	}

	/// The bucket the items binned on `day` go to.
	pub(crate) fn bucket(root: &Path, day: NaiveDate) -> PathBuf {
		super::bucket_dir(root, day)
	}
}
