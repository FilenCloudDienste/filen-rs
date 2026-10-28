//! The paths that left a local read incomplete, and why: what a host shows when the guard holds a
//! pair's deletions as [`ScanIncomplete`](super::super::GuardReason::ScanIncomplete).

use std::io;

/// A path the local read could not vouch for, which made it incomplete: every deletion of the pass
/// is held while any is reported (see [`SyncReport::scan_gaps`](super::super::SyncReport::scan_gaps)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanGap {
	/// The path relative to the local root, `/`-joined; `""` is the local root itself. For a name
	/// that is not valid UTF-8, its lossy rendering.
	pub rel_path: String,
	pub cause: ScanGapCause,
}

/// Why the local read could not vouch for a [`ScanGap`]'s path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanGapCause {
	/// Reading it, or listing it, failed with an error of this kind: a permission denial above all.
	/// A dangling symlink where the last pass synced a directory is one too, as `NotFound`.
	Unreadable { kind: io::ErrorKind },
	/// It changed while the read was looking at it: a directory removed or replaced under the walk,
	/// or an entry whose type changed between the listing and the read. The next pass reads it as
	/// it settled.
	ChangedDuringScan,
	/// Its name is not valid UTF-8, which a Filen name has to be.
	NonUtf8Name,
}

/// How many [`ScanGap`]s a read keeps: the first ones it met. A tree that cannot be read at all
/// would otherwise list every entry under it.
pub(crate) const MAX_SCAN_GAPS: usize = 100;

/// The first [`MAX_SCAN_GAPS`] gaps a read met, in the order it met them, and how many it met.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScanGaps {
	first: Vec<ScanGap>,
	total: usize,
}

impl ScanGaps {
	/// One more gap, at `rel_path`.
	pub(crate) fn note(&mut self, rel_path: &str, cause: ScanGapCause) {
		self.total = self.total.saturating_add(1);
		if self.first.len() < MAX_SCAN_GAPS {
			self.first.push(ScanGap {
				rel_path: rel_path.to_owned(),
				cause,
			});
		}
	}

	/// [`note`](Self::note) an `io::Error` of reading `rel_path`.
	pub(crate) fn unreadable(&mut self, rel_path: &str, source: &io::Error) {
		self.note(
			rel_path,
			ScanGapCause::Unreadable {
				kind: source.kind(),
			},
		);
	}

	/// Whether a gap kept is at `rel_path`.
	pub(crate) fn names(&self, rel_path: &str) -> bool {
		self.first.iter().any(|gap| gap.rel_path == rel_path)
	}

	/// `other`'s gaps after these, as if this read had met them next.
	pub(crate) fn append(&mut self, other: Self) {
		self.total = self.total.saturating_add(other.total);
		let room = MAX_SCAN_GAPS.saturating_sub(self.first.len());
		self.first.extend(other.first.into_iter().take(room));
	}

	/// The gaps kept, and how many there were.
	pub(crate) fn into_parts(self) -> (Vec<ScanGap>, usize) {
		(self.first, self.total)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn denied(at: usize) -> ScanGap {
		ScanGap {
			rel_path: format!("dir/{at}"),
			cause: ScanGapCause::Unreadable {
				kind: io::ErrorKind::PermissionDenied,
			},
		}
	}

	#[test]
	fn the_first_hundred_gaps_are_kept_and_every_one_is_counted() {
		let mut gaps = ScanGaps::default();
		for at in 0..250 {
			gaps.unreadable(
				&format!("dir/{at}"),
				&io::Error::from(io::ErrorKind::PermissionDenied),
			);
		}
		let (first, total) = gaps.into_parts();
		assert_eq!(total, 250);
		assert_eq!(first, (0..100).map(denied).collect::<Vec<_>>());
	}

	#[test]
	fn appended_gaps_fill_what_room_is_left_and_all_count() {
		let mut gaps = ScanGaps::default();
		for at in 0..99 {
			gaps.note(
				&format!("dir/{at}"),
				ScanGapCause::Unreadable {
					kind: io::ErrorKind::PermissionDenied,
				},
			);
		}
		let mut later = ScanGaps::default();
		later.note("moved", ScanGapCause::ChangedDuringScan);
		later.note("bad", ScanGapCause::NonUtf8Name);
		gaps.append(later);

		let (first, total) = gaps.into_parts();
		assert_eq!(total, 101);
		assert_eq!(first.len(), 100);
		assert_eq!(
			first.last(),
			Some(&ScanGap {
				rel_path: "moved".to_owned(),
				cause: ScanGapCause::ChangedDuringScan,
			})
		);
	}
}
