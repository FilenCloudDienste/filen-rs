//! `extractArchive` / `extractArchiveEntries` / `listArchive` / `compressItems` for the wasm and
//! uniffi bindings, and the archive helpers (`archiveExtension`, `archiveEncoderMemory`,
//! `archiveFormatLevels`, `archiveMaxLevel`, `archiveFormatOfName`, `archiveDefaultName`). Both
//! platforms take the same arguments and report the same types; a job's callbacks reach the
//! caller in the order the job made them, all before the call returns, and the call resolves
//! with the job's report even when the job failed or was cancelled.
//!
//! A report or update carries an error as the SDK error itself ([`SdkError`]), as the calls throw
//! it. On wasm only the JS thread can make one, so every record here is built from the job's own
//! types as it is delivered, or once the job has returned, never on the commander thread the
//! job runs on.
//!
//! A password is always an argument of its own, never a field of a record: uniffi prints a
//! record's fields in the foreign `toString`, and a record could end up serialized.

use std::{
	borrow::Cow,
	sync::{
		Arc,
		atomic::{AtomicU64, AtomicUsize, Ordering},
	},
};

use filen_macros::js_type;
use filen_types::fs::Uuid;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	Error, ErrorKind,
	auth::{Client, JsClient},
	fs::{
		HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{
			counts::ItemCounts,
			listing::{ItemSource, ScanProgress},
			plan::{PlanTotals, RenamedEntry, SkippedEntry},
		},
		file::{RemoteFile, enums::RemoteFileType},
		name::ValidatedName,
	},
	job::{ItemError, JobControl, SdkError, millis, report::RunState, sdk_error},
	js::{
		AnyDirWithContext, AnyFile, AnyItemWithContext, AnyNormalDir, File, NonRootNormalItemTagged,
	},
};

use super::{
	ArchiveFormat,
	compress::{
		self, CompressActiveFile, CompressCallback, CompressConfig, CompressCounts, CompressFormat,
		CompressPhase, CompressRequest, CompressSources, HashMismatch,
	},
	dispose::{self, SourceDisposal},
	entry_path::joined,
	extract::{
		self, ArchiveEntry, ArchiveEntryId, ArchiveSource, ArchiveTotals, DuplicateEntries,
		ExpansionLimit, ExtractActiveFile, ExtractCallback, ExtractConfig, ExtractMisleadingName,
		ExtractPhase, ExtractRenamedEntry, ExtractRequest, ExtractSkippedEntry, ExtractStage,
		ExtractTopLevelKey, ExtractTopLevelTrashed, ListCallback, ListConfig, ListPhase,
		ListTotals, MAX_LISTED_BYTES, OmittedRecords, PasswordCheck,
	},
	password::ArchivePassword,
};

/// Where an archive's entries are created in the destination.
#[derive(Debug, Clone)]
#[js_type(import, tagged, camel_case_fields, no_default)]
pub enum ExtractRoot {
	/// In a new folder in the destination, called `name`, or by default what
	/// `archiveDefaultName` makes of the archive's name (`photos.tar.gz` → `photos`); a name the
	/// destination holds gets the next keep-both name. A single compressed file ignores this and
	/// is always written straight into the destination (`archiveFormatOfName` tells one by its
	/// name; the report's `topLevel` holds what was created).
	NewFolder {
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		name: Option<String>,
	},
	/// Straight into the destination; entries whose names it holds get keep-both names.
	Destination,
}

/// Why a source was kept rather than removed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ArchiveKeptReason {
	/// Something was not carried over: an entry failed or was skipped.
	Incomplete,
	/// The archive holds data after its last entry that belongs to none.
	UnaccountedData { bytes: u64 },
	/// What was read does not match the hash in the source's metadata.
	HashMismatch,
	/// The source's metadata holds no hash to check what was read against, which a permanent
	/// deletion requires. Never given for a zip or 7z archive: every entry's own checksum, and
	/// every entry having been extracted, confirm those instead of a whole-archive hash.
	HashUnavailable,
	/// The source changed since the job read it: it moved, was trashed, got a new version, or
	/// holds other items now.
	Changed,
	/// The job's output could not be confirmed: the server did not hold what was created, the
	/// archive was not read in full, or something the job extracted was checked by nothing (a
	/// 7z entry without a CRC-32, or the files of a brotli or LZMA-alone stream, or of an lz4,
	/// xz or zstd stream written without its optional checksum). Before a permanent deletion,
	/// also an archive a compression wrote that does not read back as its sources, or could not
	/// be read back (a request or its reader failed).
	Unconfirmed,
	/// Deleting it for good would lose the older versions of a file in it.
	HasVersions,
	/// The job was cancelled before it removed it: before the removal began (the job ended
	/// early), or while checking or removing it.
	Interrupted,
	/// Removing it failed.
	Failed {
		/// Why.
		error: SdkError,
	},
}

/// What became of a source.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ArchiveDisposalOutcome {
	/// The source was removed.
	Disposed {
		/// How it was removed.
		how: SourceDisposal,
		/// Bytes of files deleted for good (0 when trashed), for the caller's usage figures.
		bytes_freed: u64,
	},
	/// The source was left where it is.
	Kept {
		/// Why it was kept.
		reason: ArchiveKeptReason,
		/// Bytes of its files already deleted for good when a permanent removal stopped part
		/// way (0 otherwise): those files are gone, and only the job's output still holds them.
		/// Empty files may be gone too while this is 0.
		bytes_freed: u64,
	},
}

/// What became of one source of a job that was to remove its sources. A source inside another
/// one the job was given (or given twice) shares that one's outcome with a `bytesFreed` of 0:
/// what the outer removal freed is counted once, on the outer source.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ArchiveSourceDisposition {
	/// The source's uuid, as the caller passed it.
	pub uuid: Uuid,
	/// What became of it.
	pub outcome: ArchiveDisposalOutcome,
}

/// An entry that was not extracted because something went wrong.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractFailureInfo {
	/// The entry that failed.
	pub entry: ArchiveEntryId,
	/// The entry's path in the archive, for display.
	pub path: String,
	/// The directory the entry was to be created in.
	pub dest_parent: Uuid,
	/// The name it was to be created under.
	pub dest_name: String,
	/// What failed.
	pub stage: ExtractStage,
	/// Where to extract the entry again for it to land where it was meant to. `undefined` only
	/// for a tar's hard link that failed, which does not go again: it is a copy of a file stored
	/// before it, which is in the drive by then, where it can be copied (so offer no retry for
	/// it).
	pub retry: Option<ExtractRetry>,
	/// Why it failed.
	pub error: SdkError,
}

/// Where to extract an entry that failed again, for it to land where it was meant to: pass its
/// `entry` to `extractArchiveEntries` with this `base`, `destinationDir` as the destination, and
/// the root `destination`. Failures sharing a retry go again in one call.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractRetry {
	/// The directory nearest the entry that the extraction created, or extracted into (the
	/// drive's root, say): its parent, unless that failed too.
	pub destination: Uuid,
	/// The same directory, to extract into.
	pub destination_dir: AnyNormalDir,
	/// That directory's path in the archive, as drive names separated by `/`.
	pub base: String,
}

/// A directory the extraction created.
#[js_type(export, no_deser)]
pub struct ExtractDirCreated {
	/// The directory's uuid.
	pub dest_uuid: Uuid,
	/// The directory it was created in.
	pub dest_parent: Uuid,
	/// The name it was created under.
	pub name: String,
}

/// A file the extraction created and registered.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractFileDone {
	/// Its entry.
	pub entry: ArchiveEntryId,
	/// The file's uuid.
	pub dest_uuid: Uuid,
	/// The directory it was created in.
	pub dest_parent: Uuid,
	/// The name it was created under.
	pub name: String,
	/// Its size in bytes.
	pub size: u64,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum ExtractEvent {
	/// A directory was created.
	DirCreated(ExtractDirCreated),
	/// A directory could not be created.
	DirFailed(ExtractFailureInfo),
	/// A file started being extracted.
	FileStarted(ExtractActiveFile),
	/// A file was extracted and registered.
	FileDone(ExtractFileDone),
	/// A file could not be extracted.
	FileFailed(ExtractFailureInfo),
	/// An entry was left out on purpose.
	Skipped(ExtractSkippedEntry),
	/// An entry was created under another name than the archive gives it.
	Renamed(ExtractRenamedEntry),
	/// An entry is being extracted under a name that reads as something it is not.
	MisleadingName(ExtractMisleadingName),
	/// A folder handed to the top-level callback was moved to the trash.
	TopLevelTrashed(ExtractTopLevelTrashed),
	/// What became of the archive, when it was to be removed.
	SourceDisposition(ArchiveSourceDisposition),
	/// An item was created but could not be added to one of the destination's public links or
	/// shares.
	PropagationFailed(ItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractUpdate {
	/// Where the job is.
	pub phase: ExtractPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Archive bytes read so far.
	pub bytes_read: u64,
	/// The files being extracted right now.
	pub active: Vec<ExtractActiveFile>,
	/// What happened since the last update, in order.
	pub events: Vec<ExtractEvent>,
	/// Bytes of files extracted per second, over the last 10 seconds of running time;
	/// `undefined` until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds: `undefined` until it can be told, and while a job
	/// an error or a cancel stopped winds down; 0 on the final update, whether the job completed,
	/// was cancelled or failed.
	pub eta_ms: Option<u64>,
	/// Time spent running, paused time left out, in milliseconds.
	pub active_time_ms: u64,
}

/// An item created at the top of the destination: the new folder, or (extracting into the
/// destination itself) each top-level entry.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractedTopLevelItem {
	/// Which created item it is.
	pub key: ExtractTopLevelKey,
	/// The item as created.
	pub item: NonRootNormalItemTagged,
}

/// The outcome of an extract, whether it completed, was cancelled or failed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractReport {
	/// Top-level items created, up to 1000; `omitted.topLevel` counts the rest, which the
	/// callback delivered. An extract that failed with `ArchiveWrongPassword` before extracting
	/// any file tried to move the folders it had created to the trash, for a retry to start
	/// clean: those it moved are left out here, though the callback delivered them.
	pub top_level: Vec<ExtractedTopLevelItem>,
	/// The entries that were not extracted because something went wrong, up to 1000.
	pub failures: Vec<ExtractFailureInfo>,
	/// The entries left out on purpose, up to 1000.
	pub skipped: Vec<ExtractSkippedEntry>,
	/// The entries created under another name than the archive gives them, up to 1000.
	pub renamed: Vec<ExtractRenamedEntry>,
	/// Entries extracted under names that read as something they are not, for the app to warn
	/// about before they are opened (up to 1000).
	pub misleading_names: Vec<ExtractMisleadingName>,
	/// What the lists above only count.
	pub omitted: OmittedRecords,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Bytes of the archive that belong to no entry: after its last one (another archive
	/// appended to it, say), or before a zip's first (a self-extracting stub).
	pub unaccounted_bytes: u64,
	/// Names a zip lists more than once; the last entry of each was extracted.
	pub duplicates: Option<DuplicateEntries>,
	/// What became of the archive, when it was to be removed.
	pub dispositions: Vec<ArchiveSourceDisposition>,
	/// Why the extract ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it. `undefined` when it ran to the end, failures of single entries included.
	pub error: Option<SdkError>,
}

/// One progress callback of a listing.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ListUpdate {
	/// Where the listing is.
	pub phase: ListPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	/// Bytes of the archive read so far, of `archiveBytes`.
	pub bytes_read: u64,
	/// The archive's size in bytes.
	pub archive_bytes: u64,
	/// Entries listed so far.
	pub entries: u64,
	/// Entries the entries callback did not receive, since it had not returned from the earlier
	/// ones yet while 16 MiB of them (the entries and their text) waited for it; they are counted
	/// in `entries` all the same, and a listing of an archive this large is best shown from its
	/// own `entries`.
	pub undelivered_entries: u64,
	/// Bytes of the archive read per second, over the last 10 seconds of running time;
	/// `undefined` until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds: `undefined` until it can be told, and while a job
	/// an error or a cancel stopped winds down; 0 on the final update, whether the job completed,
	/// was cancelled or failed.
	pub eta_ms: Option<u64>,
	/// Time spent running, paused time left out, in milliseconds.
	pub active_time_ms: u64,
}

/// What a listing found, whether it completed, was cancelled or failed: the archive, and its
/// entries in the order of its index (a tar's in the order it stores them).
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ArchiveListing {
	/// What the archive is; `undefined` when the listing ended before it could tell.
	pub format: Option<ArchiveFormat>,
	/// What the listing found out about the password.
	pub password: PasswordCheck,
	/// The first 10 000 entries, as long as their text (paths, targets, methods) fits 16 MiB;
	/// the callback received every one.
	pub entries: Vec<ArchiveEntry>,
	/// Entries the callback received that `entries` leaves out.
	pub omitted_entries: u64,
	/// Entries the callback did not receive (see the update's `undeliveredEntries`).
	pub undelivered_entries: u64,
	/// Counts over every entry listed, those left out of `entries` included.
	pub totals: ListTotals,
	/// Bytes of the archive that belong to no entry (see the extract report's).
	pub unaccounted_bytes: u64,
	/// Names a zip lists more than once: only the last entry of each is listed, and extracted.
	pub duplicates: Option<DuplicateEntries>,
	/// Why the listing ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it (a damaged archive, a wrong password for a 7z whose index is encrypted). `undefined`
	/// when it read the archive to its end.
	pub error: Option<SdkError>,
}

/// Something that happened to one item, reported in the next update.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum CompressEvent {
	/// An item was left out of the archive.
	Skipped(SkippedEntry),
	/// An item got another name in the archive.
	Renamed(RenamedEntry),
	/// A source's data did not match the hash in its metadata; it went into the archive as it
	/// was read, and is kept.
	SourceHashMismatch(HashMismatch),
	/// What became of a source, when the sources were to be removed.
	SourceDisposition(ArchiveSourceDisposition),
	/// The archive was registered but could not be added to one of the destination's public
	/// links or shares.
	PropagationFailed(ItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CompressUpdate {
	/// Where the job is.
	pub phase: CompressPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	/// How far listing the sources got.
	pub scan: ScanProgress,
	/// What the plan holds; zero until the sources are listed.
	pub totals: PlanTotals,
	/// What was done so far.
	pub counts: CompressCounts,
	/// The source being read, as the copy and extract updates list theirs: at most one, since
	/// an archive is written one file at a time.
	pub active: Vec<CompressActiveFile>,
	/// What happened since the last update, in order.
	pub events: Vec<CompressEvent>,
	/// Bytes of the sources read (and, in `verifying`, of the archive read back) per second,
	/// over the last 10 seconds of running time; `undefined` until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds: `undefined` until it can be told, and while a job
	/// an error or a cancel stopped winds down; 0 on the final update, whether the job completed,
	/// was cancelled or failed.
	pub eta_ms: Option<u64>,
	/// Time spent running, paused time left out, in milliseconds.
	pub active_time_ms: u64,
}

/// The outcome of a compress, whether it completed, was cancelled or failed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CompressReport {
	/// The archive, once registered in the destination.
	pub archive: Option<File>,
	/// The items left out of the archive.
	pub skipped: Vec<SkippedEntry>,
	/// The items that got another name in the archive.
	pub renamed: Vec<RenamedEntry>,
	/// What the plan held; zero when the job ended before its sources were listed.
	pub totals: PlanTotals,
	/// What was done.
	pub counts: CompressCounts,
	/// The storage the archive needed, when a storage limit refused it up front.
	pub needed_bytes: Option<u64>,
	/// What became of the sources, when they were to be removed.
	pub dispositions: Vec<ArchiveSourceDisposition>,
	/// Source files whose data did not match the hash in their metadata (up to 1000).
	pub hash_mismatches: Vec<HashMismatch>,
	/// Why the compress ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it. `undefined` when it ran to the end.
	pub error: Option<SdkError>,
}

impl From<dispose::KeptReason> for ArchiveKeptReason {
	fn from(reason: dispose::KeptReason) -> Self {
		use dispose::KeptReason as Kept;
		match reason {
			Kept::Incomplete => Self::Incomplete,
			Kept::UnaccountedData { bytes } => Self::UnaccountedData { bytes },
			Kept::HashMismatch => Self::HashMismatch,
			Kept::HashUnavailable => Self::HashUnavailable,
			Kept::Changed => Self::Changed,
			Kept::Unconfirmed => Self::Unconfirmed,
			Kept::HasVersions => Self::HasVersions,
			Kept::Interrupted => Self::Interrupted,
			Kept::Failed { error } => Self::Failed {
				error: sdk_error(error),
			},
		}
	}
}

impl From<dispose::SourceDisposition> for ArchiveSourceDisposition {
	fn from(disposition: dispose::SourceDisposition) -> Self {
		Self {
			uuid: disposition.uuid,
			outcome: match disposition.outcome {
				dispose::DisposalOutcome::Disposed { how, bytes_freed } => {
					ArchiveDisposalOutcome::Disposed { how, bytes_freed }
				}
				dispose::DisposalOutcome::Kept {
					reason,
					bytes_freed,
				} => ArchiveDisposalOutcome::Kept {
					reason: reason.into(),
					bytes_freed,
				},
			},
		}
	}
}

impl From<extract::ExtractRetry> for ExtractRetry {
	fn from(retry: extract::ExtractRetry) -> Self {
		Self {
			destination: retry.destination.uuid(),
			destination_dir: retry.destination.into(),
			base: joined(&retry.base),
		}
	}
}

impl From<extract::ExtractFailure> for ExtractFailureInfo {
	fn from(failure: extract::ExtractFailure) -> Self {
		Self {
			entry: failure.entry,
			path: failure.path,
			dest_parent: failure.dest_parent,
			dest_name: failure.dest_name,
			stage: failure.stage,
			retry: failure.retry.map(Into::into),
			error: sdk_error(failure.error),
		}
	}
}

impl From<extract::ExtractEvent> for ExtractEvent {
	fn from(event: extract::ExtractEvent) -> Self {
		use extract::ExtractEvent as Event;
		match event {
			Event::DirCreated {
				dest_uuid,
				dest_parent,
				name,
			} => Self::DirCreated(ExtractDirCreated {
				dest_uuid,
				dest_parent,
				name,
			}),
			Event::DirFailed(failure) => Self::DirFailed(failure.into()),
			Event::FileStarted(file) => Self::FileStarted(file),
			Event::FileDone {
				entry,
				dest_uuid,
				dest_parent,
				name,
				size,
			} => Self::FileDone(ExtractFileDone {
				entry,
				dest_uuid,
				dest_parent,
				name,
				size,
			}),
			Event::FileFailed(failure) => Self::FileFailed(failure.into()),
			Event::Skipped(entry) => Self::Skipped(entry),
			Event::Renamed(entry) => Self::Renamed(entry),
			Event::MisleadingName(entry) => Self::MisleadingName(entry),
			Event::TopLevelTrashed(trashed) => Self::TopLevelTrashed(trashed),
			Event::SourceDisposition(disposition) => Self::SourceDisposition(disposition.into()),
			Event::PropagationFailed { dest_uuid, error } => {
				Self::PropagationFailed(ItemError::new(dest_uuid, error))
			}
		}
	}
}

impl From<extract::ExtractUpdate> for ExtractUpdate {
	fn from(update: extract::ExtractUpdate) -> Self {
		Self {
			phase: update.phase,
			run_state: update.run_state,
			totals: update.totals,
			counts: update.counts,
			bytes_read: update.bytes_read,
			active: update.active,
			events: update.events.into_iter().map(Into::into).collect(),
			bytes_per_second: update.bytes_per_second,
			eta_ms: update.eta.map(millis),
			active_time_ms: millis(update.active_time),
		}
	}
}

impl From<extract::ExtractedTopLevel> for ExtractedTopLevelItem {
	fn from(item: extract::ExtractedTopLevel) -> Self {
		Self {
			key: item.key,
			item: item.item.into(),
		}
	}
}

impl From<extract::ExtractReport> for ExtractReport {
	fn from(report: extract::ExtractReport) -> Self {
		Self {
			top_level: report.top_level.into_iter().map(Into::into).collect(),
			failures: report.failures.into_iter().map(Into::into).collect(),
			skipped: report.skipped,
			renamed: report.renamed,
			misleading_names: report.misleading_names,
			omitted: report.omitted,
			totals: report.totals,
			counts: report.counts,
			unaccounted_bytes: report.unaccounted_bytes,
			duplicates: report.duplicates,
			dispositions: report.dispositions.into_iter().map(Into::into).collect(),
			error: None,
		}
	}
}

impl From<extract::ExtractFailed> for ExtractReport {
	fn from(failed: extract::ExtractFailed) -> Self {
		Self {
			error: Some(sdk_error(failed.error)),
			..failed.report.into()
		}
	}
}

impl From<extract::ListUpdate> for ListUpdate {
	fn from(update: extract::ListUpdate) -> Self {
		Self {
			phase: update.phase,
			run_state: update.run_state,
			bytes_read: update.bytes_read,
			archive_bytes: update.archive_bytes,
			entries: update.entries,
			undelivered_entries: 0,
			bytes_per_second: update.bytes_per_second,
			eta_ms: update.eta.map(millis),
			active_time_ms: millis(update.active_time),
		}
	}
}

impl From<extract::ArchiveListing> for ArchiveListing {
	fn from(listing: extract::ArchiveListing) -> Self {
		Self {
			format: listing.format,
			password: listing.password,
			entries: listing.entries,
			omitted_entries: listing.omitted_entries,
			undelivered_entries: 0,
			totals: listing.totals,
			unaccounted_bytes: listing.unaccounted_bytes,
			duplicates: listing.duplicates,
			error: None,
		}
	}
}

impl ArchiveListing {
	/// How a listing ended, for the bindings, with the entries the app did not receive: one
	/// that ended early still resolves, with the entries it read.
	fn new(
		result: Result<extract::ArchiveListing, extract::ListFailed>,
		undelivered_entries: u64,
	) -> Self {
		Self {
			undelivered_entries,
			..match result {
				Ok(listing) => listing.into(),
				Err(failed) => failed.into(),
			}
		}
	}
}

impl ExtractReport {
	/// How an extract ended, for the bindings: one that ended early still resolves, with the
	/// report of what it did.
	fn new(result: Result<extract::ExtractReport, extract::ExtractFailed>) -> Self {
		match result {
			Ok(report) => report.into(),
			Err(failed) => failed.into(),
		}
	}
}

impl CompressReport {
	/// How a compress ended, for the bindings: one that ended early still resolves, with the
	/// report of what it did.
	fn new(result: Result<compress::CompressReport, compress::CompressFailed>) -> Self {
		match result {
			Ok(report) => report.into(),
			Err(failed) => failed.into(),
		}
	}
}

impl From<extract::ListFailed> for ArchiveListing {
	fn from(failed: extract::ListFailed) -> Self {
		Self {
			error: Some(sdk_error(failed.error)),
			..failed.report.into()
		}
	}
}

impl From<compress::CompressEvent> for CompressEvent {
	fn from(event: compress::CompressEvent) -> Self {
		use compress::CompressEvent as Event;
		match event {
			Event::Skipped(entry) => Self::Skipped(entry),
			Event::Renamed(entry) => Self::Renamed(entry),
			Event::SourceHashMismatch(mismatch) => Self::SourceHashMismatch(mismatch),
			Event::SourceDisposition(disposition) => Self::SourceDisposition(disposition.into()),
			Event::PropagationFailed { dest_uuid, error } => {
				Self::PropagationFailed(ItemError::new(dest_uuid, error))
			}
		}
	}
}

impl From<compress::CompressUpdate> for CompressUpdate {
	fn from(update: compress::CompressUpdate) -> Self {
		Self {
			phase: update.phase,
			run_state: update.run_state,
			scan: update.scan,
			totals: update.totals,
			counts: update.counts,
			active: update.active,
			events: update.events.into_iter().map(Into::into).collect(),
			bytes_per_second: update.bytes_per_second,
			eta_ms: update.eta.map(millis),
			active_time_ms: millis(update.active_time),
		}
	}
}

impl From<compress::CompressReport> for CompressReport {
	fn from(report: compress::CompressReport) -> Self {
		Self {
			archive: report.archive.map(File::from),
			skipped: report.skipped,
			renamed: report.renamed,
			totals: report.totals,
			counts: report.counts,
			needed_bytes: report.needed_bytes,
			dispositions: report.dispositions.into_iter().map(Into::into).collect(),
			hash_mismatches: report.hash_mismatches,
			error: None,
		}
	}
}

impl From<compress::CompressFailed> for CompressReport {
	fn from(failed: compress::CompressFailed) -> Self {
		Self {
			error: Some(sdk_error(failed.error)),
			..failed.report.into()
		}
	}
}

/// The password argument of a call, checked; moved into memory wiped on drop.
fn password(password: Option<String>) -> Result<Option<ArchivePassword>, Error> {
	password.map(ArchivePassword::new).transpose()
}

// ExtractRoot carries the folder name unvalidated because neither tsify's from_wasm_abi nor
// uniffi's enum lifting can report an error; an invalid name fails the call here, before the
// extract starts.
impl TryFrom<ExtractRoot> for extract::ExtractRoot {
	type Error = Error;

	fn try_from(root: ExtractRoot) -> Result<Self, Error> {
		Ok(match root {
			ExtractRoot::NewFolder { name } => Self::NewFolder {
				name: name.as_deref().map(ValidatedName::try_from).transpose()?,
			},
			ExtractRoot::Destination => Self::Destination,
		})
	}
}

/// What `extractArchive` extracts, where, and whether the archive goes afterwards.
fn extract_request(
	archive: AnyFile,
	destination: AnyNormalDir,
	root: ExtractRoot,
	dispose: Option<SourceDisposal>,
) -> Result<ExtractRequest, Error> {
	let archive = match (dispose, archive) {
		(None, archive) => ArchiveSource::Keep(RemoteFileType::try_from(archive)?),
		(Some(how), AnyFile::File(file)) => ArchiveSource::Dispose {
			file: RemoteFile::try_from(file)?,
			how,
		},
		(Some(_), _) => {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"only an archive in the user's own drive can be removed after extracting",
			));
		}
	};
	Ok(ExtractRequest::All {
		archive,
		destination: DirType::from(destination),
		root: root.try_into()?,
	})
}

/// What `extractArchiveEntries` extracts, and where: some entries of this archive, below a
/// `base` of valid names (empty segments left out, so `/docs//sub/` is `docs/sub`). A call
/// breaking that is refused before the extract starts.
fn entries_request(
	archive: AnyFile,
	entries: Vec<ArchiveEntryId>,
	base: &str,
	destination: AnyNormalDir,
	root: ExtractRoot,
) -> Result<ExtractRequest, Error> {
	let archive = RemoteFileType::try_from(archive)?;
	extract::check_entries(archive.uuid(), &entries)?;
	Ok(ExtractRequest::Entries {
		archive,
		ids: entries,
		base: base
			.split('/')
			.filter(|segment| !segment.is_empty())
			.map(ValidatedName::try_from)
			.collect::<Result<_, _>>()?,
		destination: DirType::from(destination),
		root: root.try_into()?,
	})
}

/// The settings of an extract call, or of a listing (which leaves out `max_bytes` and
/// `max_items`), by name: what the call leaves out is the SDK's default.
#[derive(Default)]
struct ExtractSettings {
	max_bytes: Option<u64>,
	max_items: Option<u64>,
	expansion_limit: Option<ExpansionLimit>,
	skip_mac_metadata: Option<bool>,
}

impl ExtractSettings {
	fn into_config(self, password: Option<ArchivePassword>) -> ExtractConfig {
		let defaults = ExtractConfig::default();
		ExtractConfig {
			max_bytes: self.max_bytes,
			max_items: self.max_items,
			expansion_limit: self.expansion_limit.or(defaults.expansion_limit),
			password,
			skip_mac_metadata: self.skip_mac_metadata.unwrap_or(defaults.skip_mac_metadata),
		}
	}

	/// A listing's config, which has no `max_bytes` or `max_items`.
	fn into_list_config(self, password: Option<ArchivePassword>) -> ListConfig {
		let ExtractConfig {
			expansion_limit,
			skip_mac_metadata,
			password,
			..
		} = self.into_config(password);
		ListConfig {
			expansion_limit,
			skip_mac_metadata,
			password,
		}
	}
}

/// What `compressItems` compresses, and whether the items go afterwards.
fn compress_sources(
	items: Vec<AnyItemWithContext>,
	dispose: Option<SourceDisposal>,
) -> Result<CompressSources, Error> {
	let Some(how) = dispose else {
		return Ok(CompressSources::Keep(
			items
				.into_iter()
				.map(ItemSource::try_from)
				.collect::<Result<_, _>>()?,
		));
	};
	let items = items
		.into_iter()
		.map(|item| match item {
			AnyItemWithContext::File(AnyFile::File(file)) => Ok(NonRootItemType::File(Cow::Owned(
				RemoteFile::try_from(file)?,
			))),
			AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Dir(dir))) => {
				Ok(NonRootItemType::Dir(Cow::Owned(dir.into())))
			}
			_ => Err(Error::custom(
				ErrorKind::InvalidState,
				"only items in the user's own drive, not its root, can be removed after compressing",
			)),
		})
		.collect::<Result<Vec<NonRootItemType<'static, Normal>>, Error>>()?;
	Ok(CompressSources::Dispose { how, items })
}

/// An extract's callback, as the job made it: the binding's delivery task converts it (see the
/// module docs).
enum ExtractDelivery {
	TopLevelCreated(Vec<ExtractedTopLevelItem>),
	Update(extract::ExtractUpdate),
}

/// Passes an extract's callbacks to the binding's delivery task over one channel, which keeps
/// their order.
struct ExtractChannel(UnboundedSender<ExtractDelivery>);

impl ExtractCallback for ExtractChannel {
	fn on_top_level_created(&self, items: Vec<extract::ExtractedTopLevel>) {
		let _ = self.0.send(ExtractDelivery::TopLevelCreated(
			items.into_iter().map(Into::into).collect(),
		));
	}

	fn on_update(&self, update: extract::ExtractUpdate) {
		let _ = self.0.send(ExtractDelivery::Update(update));
	}
}

/// Runs the extract as the job of a managed future, its callbacks going to `sender`. Its result
/// becomes the binding's report once the job has returned (see the module docs).
async fn extract_job(
	client: Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
	sender: UnboundedSender<ExtractDelivery>,
	control: JobControl,
) -> Result<Result<extract::ExtractReport, extract::ExtractFailed>, Error> {
	Ok(client
		.extract_archive(request, config, ExtractChannel(sender), control)
		.await)
}

/// Memory of listed entries handed to the delivery task and not yet taken by the app (the
/// entries, their text and the message carrying them), past which a listing's later batches are
/// dropped, and counted, rather than queued: the SDK hands over every entry as it reads it, and
/// an app slower than the reading would otherwise buffer a million entries. As much as a listing
/// keeps of their text.
const MAX_UNDELIVERED_ENTRY_BYTES: usize = MAX_LISTED_BYTES;

/// A listing's callback, converted for the bindings.
enum ListDelivery {
	/// Entries, held against [`MAX_UNDELIVERED_ENTRY_BYTES`] until delivered.
	Entries(Vec<ArchiveEntry>, QueuedEntries),
	Update(ListUpdate),
}

/// The memory a batch of entries on its way to the app holds, given back once it is delivered
/// (or dropped undelivered).
struct QueuedEntries {
	bytes: usize,
	queued: Arc<AtomicUsize>,
}

impl Drop for QueuedEntries {
	fn drop(&mut self) {
		self.queued.fetch_sub(self.bytes, Ordering::Relaxed);
	}
}

/// Passes a listing's callbacks to the binding's delivery task over one channel, which keeps
/// their order, holding no more entries than [`MAX_UNDELIVERED_ENTRY_BYTES`] allows.
struct ListChannel {
	sender: UnboundedSender<ListDelivery>,
	/// The memory of the entries sent and not yet delivered.
	queued: Arc<AtomicUsize>,
	/// Entries dropped since the app had not taken the earlier ones yet.
	undelivered: Arc<AtomicU64>,
}

impl ListChannel {
	fn new(sender: UnboundedSender<ListDelivery>) -> Self {
		Self {
			sender,
			queued: Arc::default(),
			undelivered: Arc::default(),
		}
	}
}

impl ListCallback for ListChannel {
	fn on_entries_batch(&self, entries: Vec<ArchiveEntry>) {
		// a short path costs less than the entry holding it, so the text alone would let a
		// lagging app queue several times the limit in entries
		let bytes = size_of::<ListDelivery>()
			+ entries.capacity() * size_of::<ArchiveEntry>()
			+ entries.iter().map(ArchiveEntry::text_bytes).sum::<usize>();
		// a batch always fits an empty queue, however large
		let reserved = self
			.queued
			.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |queued| {
				(queued == 0 || queued + bytes <= MAX_UNDELIVERED_ENTRY_BYTES)
					.then_some(queued + bytes)
			})
			.is_ok();
		if !reserved {
			self.undelivered
				.fetch_add(entries.len() as u64, Ordering::Relaxed);
			return;
		}
		let queued = QueuedEntries {
			bytes,
			queued: Arc::clone(&self.queued),
		};
		let _ = self.sender.send(ListDelivery::Entries(entries, queued));
	}

	fn on_update(&self, update: extract::ListUpdate) {
		let update = ListUpdate {
			undelivered_entries: self.undelivered.load(Ordering::Relaxed),
			..update.into()
		};
		let _ = self.sender.send(ListDelivery::Update(update));
	}
}

/// How a listing ended, and the entries the app did not receive: the binding's report once the
/// job has returned (see the module docs).
type Listed = (Result<extract::ArchiveListing, extract::ListFailed>, u64);

/// Runs the listing as the job of a managed future, its callbacks going to `sender`.
async fn list_job(
	client: Arc<Client>,
	archive: RemoteFileType<'static>,
	config: ListConfig,
	sender: UnboundedSender<ListDelivery>,
	control: JobControl,
) -> Result<Listed, Error> {
	let channel = ListChannel::new(sender);
	let undelivered = Arc::clone(&channel.undelivered);
	let result = client.list_archive(archive, config, channel, control).await;
	Ok((result, undelivered.load(Ordering::Relaxed)))
}

/// A compress's callback, as the job made it: the binding's delivery task converts the update
/// (see the module docs).
enum CompressDelivery {
	ArchiveCreated(File),
	Update(compress::CompressUpdate),
}

/// Passes a compress's callbacks to the binding's delivery task over one channel, which keeps
/// their order.
struct CompressChannel(UnboundedSender<CompressDelivery>);

impl CompressCallback for CompressChannel {
	fn on_archive_created(&self, archive: RemoteFile) {
		let _ = self
			.0
			.send(CompressDelivery::ArchiveCreated(archive.into()));
	}

	fn on_update(&self, update: compress::CompressUpdate) {
		let _ = self.0.send(CompressDelivery::Update(update));
	}
}

/// What `compressItems` does, checked before it starts.
struct CompressCall {
	request: CompressRequest,
	config: CompressConfig,
}

impl CompressCall {
	/// The call's arguments, checked against the format's own rules and the client's
	/// `codec_mem_budget` for its encoder: a call breaking them is refused with the error the
	/// job would fail with (`InsufficientMemory` for an encoder over the budget), rather than
	/// started as a job that fails.
	fn new(
		items: Vec<AnyItemWithContext>,
		destination: AnyNormalDir,
		name: &str,
		config: CompressConfig,
		dispose: Option<SourceDisposal>,
		codec_mem_budget: u64,
	) -> Result<Self, Error> {
		config.format.check_name(name)?;
		config
			.format
			.check_within(config.password.is_some(), codec_mem_budget)?;
		let sources = compress_sources(items, dispose)?;
		sources.check_for(config.format)?;
		Ok(Self {
			request: CompressRequest {
				sources,
				destination: DirType::from(destination),
				name: ValidatedName::try_from(name)?,
			},
			config,
		})
	}
}

/// Runs the compress as the job of a managed future, its callbacks going to `sender`. Its
/// result becomes the binding's report once the job has returned (see the module docs).
async fn compress_job(
	client: Arc<Client>,
	call: CompressCall,
	sender: UnboundedSender<CompressDelivery>,
	control: JobControl,
) -> Result<Result<compress::CompressReport, compress::CompressFailed>, Error> {
	Ok(client
		.compress_items(call.request, call.config, CompressChannel(sender), control)
		.await)
}

#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_class = "Client")
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl JsClient {
	/// The memory for one archive job's codec state in effect, in bytes (see
	/// `JsClientConfig.archiveCodecMemBudget`): pass it to `archiveMaxLevel`, or compare
	/// `archiveEncoderMemory` with it, to offer only what this device runs.
	#[cfg_attr(
		feature = "wasm-full",
		wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveCodecMemBudget")
	)]
	pub fn archive_codec_mem_budget(&self) -> u64 {
		self.inner_ref().archive_config().codec_mem_budget
	}
}

/// The file-name extension an archive in `format` carries, dot included (`.tar.gz`, `.7z`):
/// `compressItems` takes a name ending in it.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveExtension")
)]
pub fn archive_extension(format: CompressFormat) -> String {
	format.extension()
}

/// The memory `format`'s encoder needs, in bytes; fails for a level the format does not take.
/// `compressItems` refuses a format needing more than the client's `archiveCodecMemBudget`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveEncoderMemory")
)]
pub fn archive_encoder_memory(format: CompressFormat) -> Result<u64, Error> {
	format.encoder_memory()
}

/// The levels a format's codec or method takes, both ends included.
#[js_type(export, no_deser)]
pub struct ArchiveLevels {
	/// The fastest level.
	pub min: u32,
	/// The level that compresses most.
	pub max: u32,
	/// The level to preselect: a codec's own default (a 7z's BZip2 takes bzip2's), or 7-Zip's
	/// for a 7z method.
	pub default_level: u32,
}

/// The levels `format` takes (its own level ignored), for a UI to offer; `undefined` for a
/// format without levels (a bare tar, a stored zip, a 7z copy). Which of them this device runs
/// is `archiveMaxLevel`, which may be below the default.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveFormatLevels")
)]
pub fn archive_format_levels(format: CompressFormat) -> Option<ArchiveLevels> {
	let levels = format.levels()?;
	Some(ArchiveLevels {
		min: *levels.start(),
		max: *levels.end(),
		default_level: format.default_level()?,
	})
}

/// The highest of `format`'s levels whose encoder fits `budget` bytes (the client's
/// `archiveCodecMemBudget`); `undefined` when the format has no levels, or not even its lowest
/// fits.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveMaxLevel")
)]
pub fn archive_max_level(format: CompressFormat, budget: u64) -> Option<u32> {
	format.max_level_within(budget)
}

/// What a file named `name` holds as far as its extension tells (`.tar.gz` and `.tgz` a gzip
/// tar, `.gz` one gzip-compressed file), matched case-insensitively; `undefined` for a name
/// with no archive extension. For an app to offer extracting a file, and to know that a single
/// compressed file extracts into the destination itself, never a new folder.
///
/// The file's own bytes decide once it is extracted or listed: a `.zip` that is really a tar
/// is read as a tar, and a tar named without an extension is read too.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveFormatOfName")
)]
pub fn archive_format_of_name(name: String) -> Option<ArchiveFormat> {
	ArchiveFormat::of_name(&name)
}

/// The name of the folder an archive extracts to by default: its name without its archive
/// extension (`photos.tar.gz` → `photos`), made into a valid name (`Archive` when it cannot
/// be).
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveDefaultName")
)]
pub fn archive_default_name(name: String) -> String {
	super::format::archive_default_name(&name).into()
}

#[cfg(feature = "uniffi")]
mod uniffi_impl;

#[cfg(feature = "wasm-full")]
mod wasm_impl;

#[cfg(all(test, feature = "uniffi"))]
mod tests;
