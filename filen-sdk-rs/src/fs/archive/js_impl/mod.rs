//! `extractArchive` / `extractArchiveEntries` / `listArchive` / `compressItems` for the wasm and
//! uniffi bindings, and the archive helpers (`archiveExtension`, `archiveEncoderMemory`,
//! `archiveFormatLevels`, `archiveMaxLevel`, `archiveFormatOfName`, `archiveDefaultName`). Both
//! platforms take the same arguments and report the same types; a job's callbacks reach the
//! caller in the order the job made them, all before the call returns, and the call resolves
//! with the job's report even when the job failed or was cancelled.
//!
//! A password is always an argument of its own, never a field of a record: uniffi prints a
//! record's fields in the foreign `toString`, and a record could end up serialized.

use std::{borrow::Cow, sync::Arc};

use filen_macros::js_type;
use filen_types::fs::Uuid;
use tokio::sync::mpsc::UnboundedSender;

use crate::{
	Error, ErrorKind,
	auth::Client,
	fs::{
		categories::{DirType, NonRootItemType, Normal},
		drive_job::{
			counts::ItemCounts,
			listing::{ItemSource, ScanProgress},
			plan::{PlanTotals, RenamedEntry, SkippedEntry},
		},
		file::{RemoteFile, enums::RemoteFileType},
		name::ValidatedName,
	},
	job::{ItemError, JobControl, JobError, job_error, millis, report::RunState},
	js::{
		AnyDirWithContext, AnyFile, AnyItemWithContext, AnyNormalDir, File, NonRootNormalItemTagged,
	},
};

use super::{
	compress::{
		self, CompressActiveFile, CompressCallback, CompressConfig, CompressCounts, CompressFormat,
		CompressPhase, CompressSources, HashMismatch,
	},
	dispose::{self, SourceDisposal},
	extract::{
		self, ArchiveEntry, ArchiveEntryId, ArchiveFormat, ArchiveSource, ArchiveTotals,
		DuplicateEntries, ExpansionLimit, ExtractActiveFile, ExtractCallback, ExtractConfig,
		ExtractMisleadingName, ExtractPhase, ExtractRenamedEntry, ExtractRequest, ExtractRetry,
		ExtractSkippedEntry, ExtractStage, ExtractTopLevelKey, ListCallback, ListPhase, ListTotals,
		OmittedRecords, PasswordCheck,
	},
	password::ArchivePassword,
};

/// Where an archive's entries are created in the destination.
#[derive(Debug, Clone)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Deserialize, tsify::Tsify),
	tsify(from_wasm_abi),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ArchiveKeptReason {
	/// Something was not carried over: an entry failed or was skipped.
	Incomplete,
	/// The archive holds data after its last entry that belongs to none.
	UnaccountedData { bytes: u64 },
	/// What was read does not match the hash in the source's metadata.
	HashMismatch,
	/// The source's metadata holds no hash to check what was read against, which a permanent
	/// deletion requires.
	HashUnavailable,
	/// The source changed since the job read it: it moved, was trashed, got a new version, or
	/// holds other items now.
	Changed,
	/// The job's output could not be confirmed: the server did not hold what was created, the
	/// archive was not read in full, or something the job extracted was checked by nothing (a
	/// 7z entry without a CRC-32, or the files of a brotli or LZMA-alone stream, or of an lz4,
	/// xz or zstd stream written without its optional checksum).
	Unconfirmed,
	/// Deleting it for good would lose the older versions of a file in it.
	HasVersions,
	/// The job was cancelled while removing it.
	Interrupted,
	/// Removing it failed.
	Failed { error: JobError },
}

#[derive(Debug, Clone)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(
		tag = "type",
		rename_all = "camelCase",
		rename_all_fields = "camelCase"
	)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ArchiveDisposalOutcome {
	Disposed {
		how: SourceDisposal,
		/// Bytes of files deleted for good (0 when trashed), for the caller's usage figures.
		bytes_freed: u64,
	},
	Kept {
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
	pub uuid: Uuid,
	pub outcome: ArchiveDisposalOutcome,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractFailureInfo {
	pub entry: ArchiveEntryId,
	/// The entry's path in the archive, for display.
	pub path: String,
	/// The directory the entry was to be created in.
	pub dest_parent: Uuid,
	pub dest_name: String,
	pub stage: ExtractStage,
	/// Where to extract the entry again for it to land where it was meant to: pass its `entry`
	/// to `extractArchiveEntries` with this `base`, the directory `destination` names as the
	/// destination, and the root `destination`. Failures sharing a retry go again in one call.
	/// A tar's hard link that failed does not go again this way: it is a copy of a file stored
	/// before it, which is in the drive by then, where it can be copied.
	pub retry: ExtractRetry,
	pub error: JobError,
}

#[js_type(export, no_deser)]
pub struct ExtractDirCreated {
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractFileDone {
	pub entry: ArchiveEntryId,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
	pub size: u64,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum ExtractEvent {
	DirCreated(ExtractDirCreated),
	DirFailed(ExtractFailureInfo),
	FileStarted(ExtractActiveFile),
	FileDone(ExtractFileDone),
	FileFailed(ExtractFailureInfo),
	Skipped(ExtractSkippedEntry),
	Renamed(ExtractRenamedEntry),
	MisleadingName(ExtractMisleadingName),
	SourceDisposition(ArchiveSourceDisposition),
	PropagationFailed(ItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractUpdate {
	pub phase: ExtractPhase,
	pub run_state: RunState,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Archive bytes read so far.
	pub bytes_read: u64,
	pub active: Vec<ExtractActiveFile>,
	pub events: Vec<ExtractEvent>,
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds.
	pub eta_ms: Option<u64>,
	/// Time spent running, paused time left out, in milliseconds.
	pub active_time_ms: u64,
}

/// An item created at the top of the destination: the new folder, or (extracting into the
/// destination itself) each top-level entry.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractedTopLevelItem {
	pub key: ExtractTopLevelKey,
	pub item: NonRootNormalItemTagged,
}

/// The outcome of an extract, whether it completed, was cancelled or failed.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractReport {
	/// Top-level items created, up to 1000; `omitted.topLevel` counts the rest, which the
	/// callback delivered. An extract that failed with `ArchiveWrongPassword` before extracting
	/// any file moved the folders it had created to the trash, for a retry to start clean: they
	/// are left out here, though the callback delivered them.
	pub top_level: Vec<ExtractedTopLevelItem>,
	pub failures: Vec<ExtractFailureInfo>,
	pub skipped: Vec<ExtractSkippedEntry>,
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
	pub error: Option<JobError>,
}

/// One progress callback of a listing.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ListUpdate {
	pub phase: ListPhase,
	pub run_state: RunState,
	/// Bytes of the archive read so far, of `archive_bytes`.
	pub bytes_read: u64,
	pub archive_bytes: u64,
	/// Entries listed so far.
	pub entries: u64,
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds.
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
	pub password: PasswordCheck,
	/// The first 10 000 entries, as long as their text (paths, targets, methods) fits 16 MiB;
	/// the callback received every one.
	pub entries: Vec<ArchiveEntry>,
	/// Entries the callback received that `entries` leaves out.
	pub omitted_entries: u64,
	pub totals: ListTotals,
	/// Bytes of the archive that belong to no entry (see the extract report's).
	pub unaccounted_bytes: u64,
	/// Names a zip lists more than once: only the last entry of each is listed, and extracted.
	pub duplicates: Option<DuplicateEntries>,
	/// Why the listing ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it (a damaged archive, a wrong password for a 7z whose index is encrypted). `undefined`
	/// when it read the archive to its end.
	pub error: Option<JobError>,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum CompressEvent {
	Skipped(SkippedEntry),
	Renamed(RenamedEntry),
	/// A source's data did not match the hash in its metadata; it went into the archive as it
	/// was read, and is kept.
	SourceHashMismatch(HashMismatch),
	SourceDisposition(ArchiveSourceDisposition),
	PropagationFailed(ItemError),
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct CompressUpdate {
	pub phase: CompressPhase,
	pub run_state: RunState,
	pub scan: ScanProgress,
	pub totals: PlanTotals,
	pub counts: CompressCounts,
	/// The source being read, as the copy and extract updates list theirs: at most one, since
	/// an archive is written one file at a time.
	pub active: Vec<CompressActiveFile>,
	pub events: Vec<CompressEvent>,
	pub bytes_per_second: Option<u64>,
	/// Estimated time left, in milliseconds.
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
	pub skipped: Vec<SkippedEntry>,
	pub renamed: Vec<RenamedEntry>,
	pub totals: PlanTotals,
	pub counts: CompressCounts,
	/// The storage the archive needed, when a storage limit refused it up front.
	pub needed_bytes: Option<u64>,
	/// What became of the sources, when they were to be removed.
	pub dispositions: Vec<ArchiveSourceDisposition>,
	/// Source files whose data did not match the hash in their metadata (up to 1000).
	pub hash_mismatches: Vec<HashMismatch>,
	/// Why the compress ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it. `undefined` when it ran to the end.
	pub error: Option<JobError>,
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
				error: job_error(error),
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

impl From<extract::ExtractFailure> for ExtractFailureInfo {
	fn from(failure: extract::ExtractFailure) -> Self {
		Self {
			entry: failure.entry,
			path: failure.path,
			dest_parent: failure.dest_parent,
			dest_name: failure.dest_name,
			stage: failure.stage,
			retry: failure.retry,
			error: job_error(failure.error),
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
			error: Some(job_error(failed.error)),
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
			totals: listing.totals,
			unaccounted_bytes: listing.unaccounted_bytes,
			duplicates: listing.duplicates,
			error: None,
		}
	}
}

impl From<extract::ListFailed> for ArchiveListing {
	fn from(failed: extract::ListFailed) -> Self {
		Self {
			error: Some(job_error(failed.error)),
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
			Event::SourceHashMismatch { source_uuid, path } => {
				Self::SourceHashMismatch(HashMismatch { source_uuid, path })
			}
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
			error: Some(job_error(failed.error)),
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

/// What `extractArchiveEntries` extracts, and where. The entries and `base` are checked by the
/// extract itself, which fails with the report of what it did.
fn entries_request(
	archive: AnyFile,
	entries: Vec<ArchiveEntryId>,
	base: String,
	destination: AnyNormalDir,
	root: ExtractRoot,
) -> Result<ExtractRequest, Error> {
	Ok(ExtractRequest::Entries {
		archive: RemoteFileType::try_from(archive)?,
		ids: entries,
		base,
		destination: DirType::from(destination),
		root: root.try_into()?,
	})
}

/// An extract's (or a listing's) config from the call's arguments: what the call leaves out is
/// the SDK's default.
fn extract_config(
	max_bytes: Option<u64>,
	max_items: Option<u64>,
	expansion_limit: Option<ExpansionLimit>,
	skip_mac_metadata: Option<bool>,
	password: Option<ArchivePassword>,
) -> ExtractConfig {
	let mut config = ExtractConfig {
		max_bytes,
		max_items,
		password,
		..ExtractConfig::default()
	};
	if let Some(limit) = expansion_limit {
		config.expansion_limit = Some(limit);
	}
	if let Some(skip) = skip_mac_metadata {
		config.skip_mac_metadata = skip;
	}
	config
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

/// An extract's callback, converted for the bindings.
enum ExtractDelivery {
	TopLevelCreated(Vec<ExtractedTopLevelItem>),
	Update(ExtractUpdate),
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
		let _ = self.0.send(ExtractDelivery::Update(update.into()));
	}
}

/// Runs the extract as the job of a managed future, its callbacks going to `sender`.
async fn extract_job(
	client: Arc<Client>,
	request: ExtractRequest,
	config: ExtractConfig,
	sender: UnboundedSender<ExtractDelivery>,
	control: JobControl,
) -> Result<ExtractReport, Error> {
	let result = client
		.extract_archive(request, config, ExtractChannel(sender), control)
		.await;
	// an extract that ended early still resolves, with the report of what it did
	Ok(match result {
		Ok(report) => report.into(),
		Err(failed) => failed.into(),
	})
}

/// A listing's callback, converted for the bindings.
enum ListDelivery {
	Entries(Vec<ArchiveEntry>),
	Update(ListUpdate),
}

/// Passes a listing's callbacks to the binding's delivery task over one channel, which keeps
/// their order.
struct ListChannel(UnboundedSender<ListDelivery>);

impl ListCallback for ListChannel {
	fn on_entries(&self, entries: Vec<ArchiveEntry>) {
		let _ = self.0.send(ListDelivery::Entries(entries));
	}

	fn on_update(&self, update: extract::ListUpdate) {
		let _ = self.0.send(ListDelivery::Update(update.into()));
	}
}

/// Runs the listing as the job of a managed future, its callbacks going to `sender`.
async fn list_job(
	client: Arc<Client>,
	archive: RemoteFileType<'static>,
	config: ExtractConfig,
	sender: UnboundedSender<ListDelivery>,
	control: JobControl,
) -> Result<ArchiveListing, Error> {
	let result = client
		.list_archive(archive, config, ListChannel(sender), control)
		.await;
	// a listing that ended early still resolves, with the entries it read
	Ok(match result {
		Ok(listing) => listing.into(),
		Err(failed) => failed.into(),
	})
}

/// A compress's callback, converted for the bindings.
enum CompressDelivery {
	ArchiveCreated(File),
	Update(CompressUpdate),
}

struct CompressChannel(UnboundedSender<CompressDelivery>);

impl CompressCallback for CompressChannel {
	fn on_archive_created(&self, archive: RemoteFile) {
		let _ = self
			.0
			.send(CompressDelivery::ArchiveCreated(archive.into()));
	}

	fn on_update(&self, update: compress::CompressUpdate) {
		let _ = self.0.send(CompressDelivery::Update(update.into()));
	}
}

/// What `compressItems` does, checked before it starts.
struct CompressCall {
	sources: CompressSources,
	destination: DirType<'static, Normal>,
	name: ValidatedName,
	config: CompressConfig,
}

impl CompressCall {
	/// The call's arguments, checked against the format's own rules and the client's
	/// `codec_mem_budget` for its encoder: a call breaking them is refused, as an invalid
	/// argument, rather than started as a job that fails.
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
		Ok(Self {
			sources: compress_sources(items, dispose)?,
			destination: DirType::from(destination),
			name: ValidatedName::try_from(name)?,
			config,
		})
	}
}

async fn compress_job(
	client: Arc<Client>,
	call: CompressCall,
	sender: UnboundedSender<CompressDelivery>,
	control: JobControl,
) -> Result<CompressReport, Error> {
	let result = client
		.compress_items(
			call.sources,
			call.destination,
			call.name,
			call.config,
			CompressChannel(sender),
			control,
		)
		.await;
	Ok(match result {
		Ok(report) => report.into(),
		Err(failed) => failed.into(),
	})
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
	pub min: u32,
	pub max: u32,
}

/// The levels `format` takes (its own level ignored), for a UI to offer; `undefined` for a
/// format without levels (a bare tar, a stored zip, a 7z copy). Which of them this device runs
/// is `archiveMaxLevel`.
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveFormatLevels")
)]
pub fn archive_format_levels(format: CompressFormat) -> Option<ArchiveLevels> {
	format.levels().map(|levels| ArchiveLevels {
		min: *levels.start(),
		max: *levels.end(),
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
	extract::archive_default_name(&name).into()
}

#[cfg(feature = "uniffi")]
mod uniffi_impl;

#[cfg(feature = "wasm-full")]
mod wasm_impl;

#[cfg(all(test, feature = "uniffi"))]
mod tests;
