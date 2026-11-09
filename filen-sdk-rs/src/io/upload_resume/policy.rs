//! Whether a journaled upload is resumed or started over.

use chrono::{DateTime, TimeDelta, Utc};

use super::{ESTIMATE_SAFETY_FACTOR, RESUME_MARGIN, SERVER_RETENTION, record::UploadJournalRecord};
use crate::{consts::CHUNK_SIZE_U64, fs::file::BaseFile, io::FilenMetaExt};

/// How far in the future a journal's first acknowledgement may lie before the journal is taken
/// for one written under a wrong clock.
const CLOCK_SKEW_ALLOWANCE: TimeDelta = TimeDelta::minutes(5);

/// What identifies the contents of a source file without reading it: an upload of it is resumed
/// only while all of this is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SourceStamp {
	/// Size in bytes.
	pub(crate) size: u64,
	/// Last modification, in milliseconds since the Unix epoch.
	pub(crate) mtime_millis: i64,
	/// The file's inode on Unix, so that a file replaced by another of the same size and time is
	/// not taken for it. `None` elsewhere.
	pub(crate) inode: Option<u64>,
}

impl SourceStamp {
	pub(crate) fn of(meta: &std::fs::Metadata) -> Self {
		#[cfg(unix)]
		let inode = Some(std::os::unix::fs::MetadataExt::ino(meta));
		#[cfg(not(unix))]
		let inode = None;
		Self {
			size: FilenMetaExt::size(meta),
			mtime_millis: FilenMetaExt::modified(meta).timestamp_millis(),
			inode,
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeDecision {
	Resume,
	StartFresh(FreshReason),
}

/// Why a journaled upload is started over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FreshReason {
	/// The source's size, modification time or inode changed.
	SourceChanged,
	/// The created or modified time the file is uploaded with changed.
	TimesChanged,
	MimeChanged,
	/// The journal was written with another chunk size.
	ChunkSizeChanged,
	/// The journal records an upload into another directory or under another name.
	OtherUpload,
	/// The journal's first acknowledgement lies in the future: a clock was wrong.
	FromTheFuture,
	/// The upload might not finish before the server drops its chunks, or how long it takes
	/// cannot be estimated.
	PastWindow,
}

/// Whether the upload `record` journals can be resumed as the upload of `file` from a source that
/// looks like `source` at `now`.
pub(crate) fn decide(
	record: &UploadJournalRecord,
	source: SourceStamp,
	file: &BaseFile,
	now: DateTime<Utc>,
) -> ResumeDecision {
	let fresh = |reason| ResumeDecision::StartFresh(reason);
	if record.source() != source {
		return fresh(FreshReason::SourceChanged);
	}
	if record.times() != (file.created(), file.last_modified()) {
		return fresh(FreshReason::TimesChanged);
	}
	if record.mime() != file.mime() {
		return fresh(FreshReason::MimeChanged);
	}
	if record.chunk_size() != CHUNK_SIZE_U64 {
		return fresh(FreshReason::ChunkSizeChanged);
	}
	if record.parent() != file.parent() || record.name() != file.name() {
		return fresh(FreshReason::OtherUpload);
	}
	let Some(first_ack_at) = record.first_ack_at() else {
		return fresh(FreshReason::PastWindow);
	};
	if now
		.checked_add_signed(CLOCK_SKEW_ALLOWANCE)
		.is_some_and(|latest| first_ack_at > latest)
	{
		return fresh(FreshReason::FromTheFuture);
	}
	let Some(estimate) = estimate_to_finish(record) else {
		return fresh(FreshReason::PastWindow);
	};
	if !fits_window(now.signed_duration_since(first_ack_at), estimate) {
		return fresh(FreshReason::PastWindow);
	}
	ResumeDecision::Resume
}

/// Whether an upload whose first chunk was acknowledged `age` ago and that needs `estimate` more
/// to finish does so before the server may drop its chunks, with [`RESUME_MARGIN`] to spare.
pub(crate) fn fits_window(age: TimeDelta, estimate: TimeDelta) -> bool {
	let Some(limit) = SERVER_RETENTION.checked_sub(&RESUME_MARGIN) else {
		return false;
	};
	age.checked_add(&estimate)
		.is_some_and(|total| total < limit)
}

/// How long the rest of `record`'s upload takes at the rate its acknowledged chunks were sent,
/// slowed by [`ESTIMATE_SAFETY_FACTOR`]. `None` when nothing was acknowledged or the arithmetic
/// overflows.
fn estimate_to_finish(record: &UploadJournalRecord) -> Option<TimeDelta> {
	let acked = u128::from(record.acked_bytes()?);
	if acked == 0 {
		return None;
	}
	let remaining = u128::from(record.source().size).checked_sub(acked)?;
	let millis = remaining
		.checked_mul(u128::from(record.active_millis()))?
		.checked_div(acked)?
		.checked_mul(u128::from(ESTIMATE_SAFETY_FACTOR))?;
	TimeDelta::try_milliseconds(i64::try_from(millis).ok()?)
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use filen_types::fs::Uuid;

	use super::*;
	use crate::{
		crypto::{file::FileKey, v2},
		fs::{file::RootFile, file::write::RemoteFileInfo, name::ValidatedName},
		io::upload_resume::store::UploadJournalId,
	};

	const SIZE: u64 = CHUNK_SIZE_U64 * 10 + 123;

	fn time(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	fn now() -> DateTime<Utc> {
		time(1_700_000_000_000)
	}

	fn stamp() -> SourceStamp {
		SourceStamp {
			size: SIZE,
			mtime_millis: 1_600_000_000_000,
			inode: Some(42),
		}
	}

	fn file() -> BaseFile {
		BaseFile {
			root: RootFile {
				uuid: Uuid::from_u128(7),
				name: ValidatedName::try_from("video.mp4").unwrap(),
				mime: "video/mp4".to_owned(),
				key: FileKey::V2(
					"0123456789abcdef0123456789abcdef"
						.parse::<v2::FileKey>()
						.unwrap(),
				),
				created: time(1_500_000_000_000),
				modified: time(1_600_000_000_000),
			},
			parent: Uuid::from_u128(1),
		}
	}

	/// A record of `file()` from `stamp()` whose first `acked` chunks were acknowledged at
	/// `first_ack_at`, after `active_millis` of sending.
	fn record_with(
		source: SourceStamp,
		acked: u64,
		first_ack_at: DateTime<Utc>,
		active_millis: u64,
	) -> UploadJournalRecord {
		let file = file();
		let id = UploadJournalId::new(Path::new("/video.mp4"), file.parent(), file.name());
		let mut record =
			UploadJournalRecord::new(id, &file, "u".repeat(32), source, CHUNK_SIZE_U64).unwrap();
		for idx in 0..acked {
			record
				.ack(idx, RemoteFileInfo::default(), first_ack_at)
				.unwrap();
		}
		record.add_active_millis(active_millis);
		record
	}

	/// Half the chunks sent in a minute, an hour ago.
	fn record() -> UploadJournalRecord {
		record_with(stamp(), 5, now() - TimeDelta::hours(1), 60_000)
	}

	#[test]
	fn an_unchanged_source_inside_the_window_resumes() {
		assert_eq!(
			decide(&record(), stamp(), &file(), now()),
			ResumeDecision::Resume
		);
	}

	#[test]
	fn fits_window_refuses_exactly_22_hours_and_takes_one_millisecond_less() {
		assert!(!fits_window(TimeDelta::hours(21), TimeDelta::hours(1)));
		assert!(fits_window(
			TimeDelta::hours(21),
			TimeDelta::hours(1) - TimeDelta::milliseconds(1)
		));
		assert!(!fits_window(TimeDelta::MAX, TimeDelta::MAX));
	}

	#[test]
	fn a_source_whose_size_changed_starts_fresh() {
		let source = SourceStamp {
			size: SIZE + 1,
			..stamp()
		};
		assert_eq!(
			decide(&record(), source, &file(), now()),
			ResumeDecision::StartFresh(FreshReason::SourceChanged)
		);
	}

	#[test]
	fn a_source_whose_mtime_changed_starts_fresh() {
		let source = SourceStamp {
			mtime_millis: stamp().mtime_millis + 1,
			..stamp()
		};
		assert_eq!(
			decide(&record(), source, &file(), now()),
			ResumeDecision::StartFresh(FreshReason::SourceChanged)
		);
	}

	#[test]
	fn a_source_whose_inode_changed_starts_fresh() {
		let source = SourceStamp {
			inode: Some(43),
			..stamp()
		};
		assert_eq!(
			decide(&record(), source, &file(), now()),
			ResumeDecision::StartFresh(FreshReason::SourceChanged)
		);
	}

	#[test]
	fn changed_times_or_mime_start_fresh() {
		let mut created = file();
		created.root.created = time(1_500_000_000_001);
		assert_eq!(
			decide(&record(), stamp(), &created, now()),
			ResumeDecision::StartFresh(FreshReason::TimesChanged)
		);
		let mut modified = file();
		modified.root.modified = time(1_600_000_000_001);
		assert_eq!(
			decide(&record(), stamp(), &modified, now()),
			ResumeDecision::StartFresh(FreshReason::TimesChanged)
		);
		let mut mime = file();
		mime.root.mime = "application/octet-stream".to_owned();
		assert_eq!(
			decide(&record(), stamp(), &mime, now()),
			ResumeDecision::StartFresh(FreshReason::MimeChanged)
		);
	}

	#[test]
	fn another_chunk_size_starts_fresh() {
		let file = file();
		let id = UploadJournalId::new(Path::new("/video.mp4"), file.parent(), file.name());
		let mut record =
			UploadJournalRecord::new(id, &file, "u".repeat(32), stamp(), CHUNK_SIZE_U64 / 2)
				.unwrap();
		record
			.ack(0, RemoteFileInfo::default(), now() - TimeDelta::hours(1))
			.unwrap();
		record.add_active_millis(1_000);
		assert_eq!(
			decide(&record, stamp(), &file, now()),
			ResumeDecision::StartFresh(FreshReason::ChunkSizeChanged)
		);
	}

	#[test]
	fn a_record_of_another_parent_or_name_starts_fresh() {
		let mut parent = file();
		parent.parent = Uuid::from_u128(2);
		assert_eq!(
			decide(&record(), stamp(), &parent, now()),
			ResumeDecision::StartFresh(FreshReason::OtherUpload)
		);
		let mut name = file();
		name.root.name = ValidatedName::try_from("other.mp4").unwrap();
		assert_eq!(
			decide(&record(), stamp(), &name, now()),
			ResumeDecision::StartFresh(FreshReason::OtherUpload)
		);
	}

	#[test]
	fn a_journal_started_in_the_future_starts_fresh() {
		let record = record_with(
			stamp(),
			5,
			now() + TimeDelta::minutes(5) + TimeDelta::milliseconds(1),
			60_000,
		);
		assert_eq!(
			decide(&record, stamp(), &file(), now()),
			ResumeDecision::StartFresh(FreshReason::FromTheFuture)
		);
		let record = record_with(stamp(), 5, now() + TimeDelta::minutes(5), 60_000);
		assert_eq!(
			decide(&record, stamp(), &file(), now()),
			ResumeDecision::Resume
		);
	}

	#[test]
	fn a_slow_upload_whose_estimate_overruns_the_window_starts_fresh() {
		// Half the bytes took 6 hours, so the rest is estimated at twice that: 12 hours. Started 10
		// hours ago, that ends at 22 hours, the limit.
		let half = 5 * CHUNK_SIZE_U64;
		let rest = SIZE - half;
		let active = u64::try_from(TimeDelta::hours(6).num_milliseconds()).unwrap();
		// active such that rest * active / half * 2 is exactly 12 hours
		let active = active * half / rest;
		let record = record_with(stamp(), 5, now() - TimeDelta::hours(10), active);
		let estimate = estimate_to_finish(&record).unwrap();
		assert!(estimate <= TimeDelta::hours(12), "{estimate}");
		assert!(
			estimate > TimeDelta::hours(12) - TimeDelta::seconds(1),
			"{estimate}"
		);
		let record = record_with(
			stamp(),
			5,
			now() - TimeDelta::hours(10) - TimeDelta::seconds(1),
			active,
		);
		assert_eq!(
			decide(&record, stamp(), &file(), now()),
			ResumeDecision::StartFresh(FreshReason::PastWindow)
		);
		let record = record_with(stamp(), 5, now() - TimeDelta::hours(9), active);
		assert_eq!(
			decide(&record, stamp(), &file(), now()),
			ResumeDecision::Resume
		);
	}

	#[test]
	fn an_estimate_that_overflows_starts_fresh() {
		// A TiB, of which one chunk took u64::MAX milliseconds: the estimate is past what a
		// TimeDelta holds. (A source of u64::MAX bytes would need a bitmap of 2 TB.)
		let huge = SourceStamp {
			size: 1 << 40,
			..stamp()
		};
		let record = record_with(huge, 1, now() - TimeDelta::hours(1), u64::MAX);
		assert_eq!(estimate_to_finish(&record), None);
		assert_eq!(
			decide(&record, huge, &file(), now()),
			ResumeDecision::StartFresh(FreshReason::PastWindow)
		);
	}
}
