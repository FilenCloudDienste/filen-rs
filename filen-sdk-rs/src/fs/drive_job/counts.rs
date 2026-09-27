//! What a job that creates drive items counts as it runs.

use filen_macros::js_type;

/// Running counts. Once the job is over, everything planned is done, failed or not attempted:
/// `created + failed + not_attempted == totals` for directories, and likewise for files and
/// bytes. Skipped entries are not part of the totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ItemCounts {
	pub dirs_created: u64,
	/// Includes the directories below a failed one, which are never attempted.
	pub dirs_failed: u64,
	pub files_done: u64,
	/// Includes the files below a failed directory, which are never attempted.
	pub files_failed: u64,
	pub bytes_done: u64,
	pub bytes_failed: u64,
	/// What a job that ended early (cancelled, or stopped by an error) never created, including
	/// files it had started: their partial uploads never become visible. Zero while running.
	pub dirs_not_attempted: u64,
	pub files_not_attempted: u64,
	pub bytes_not_attempted: u64,
	pub entries_skipped: u64,
	pub bytes_skipped: u64,
}
