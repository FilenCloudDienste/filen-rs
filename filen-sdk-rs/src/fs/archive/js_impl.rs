//! `extractArchive` / `compressItems` for the wasm and uniffi bindings, and the archive helpers
//! (`archiveExtension`, `archiveEncoderMemory`, `archiveDefaultName`). Both platforms take the
//! same arguments and report the same types; a job's callbacks reach the caller in the order the
//! job made them, all before the call returns.
//!
//! A password is always an argument of its own, never a field of a record: uniffi prints a
//! record's fields in the foreign `toString`, and a record could end up serialized.

use std::{borrow::Cow, sync::Arc, time::Duration};

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
			plan::{PlanTotals, RenameReason, SkippedEntry},
		},
		file::{RemoteFile, enums::RemoteFileType},
		name::ValidatedName,
	},
	job::{JobControl, JobError, job_error, report::RunState},
	js::{
		AnyDirWithContext, AnyFile, AnyItemWithContext, AnyNormalDir, File, NonRootNormalItemTagged,
	},
};

use super::{
	compress::{
		self, CompressCallback, CompressConfig, CompressCounts, CompressFormat, CompressPhase,
		CompressRequest, CompressSources,
	},
	dispose::{self, SourceDisposal},
	extract::{
		self, ArchiveEntryId, ArchiveSource, ArchiveTotals, DuplicateEntries, ExpansionLimit,
		ExtractActiveFile, ExtractCallback, ExtractConfig, ExtractMisleadingName, ExtractPhase,
		ExtractRenamedEntry, ExtractRequest, ExtractRoot, ExtractSkippedEntry, ExtractStage,
		ExtractTopLevelKey, OmittedRecords,
	},
	password::ArchivePassword,
};

/// Where an archive's entries land in the destination.
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
pub enum ExtractInto {
	/// A new folder holding every entry, named `name`, or after the archive without its
	/// extension.
	NewFolder {
		#[cfg_attr(feature = "wasm-full", serde(default), tsify(optional))]
		name: Option<String>,
	},
	/// The destination itself.
	Destination,
}

/// A created item that could not be added to one of the destination's public links or shares.
#[derive(Debug, Clone)]
#[js_type(export, no_deser, no_default)]
pub struct ArchiveItemError {
	pub dest_uuid: Uuid,
	pub error: JobError,
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
	/// The archive holds data that belongs to no entry.
	UnaccountedData { bytes: u64 },
	/// What was read does not match the hash in the source's metadata.
	HashMismatch,
	/// The source's metadata holds no hash, which a permanent deletion requires.
	HashUnavailable,
	/// The source changed since the job read it.
	Changed,
	/// The job's output could not be confirmed with the server.
	Unconfirmed,
	/// Deleting it for good would lose the older versions of a file in it.
	HasVersions,
	/// The job was cancelled before it removed it: before the removal began (the job ended
	/// early), or while checking or removing it.
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
		bytes_freed: u64,
	},
}

/// What became of one source of a job that was to remove its sources.
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
	PropagationFailed(ArchiveItemError),
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
	/// callback delivered.
	pub top_level: Vec<ExtractedTopLevelItem>,
	pub failures: Vec<ExtractFailureInfo>,
	pub skipped: Vec<ExtractSkippedEntry>,
	pub renamed: Vec<ExtractRenamedEntry>,
	pub omitted: OmittedRecords,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Bytes of the archive that belong to no entry.
	pub unaccounted_bytes: u64,
	pub duplicates: Option<DuplicateEntries>,
	/// What became of the archive, when it was to be removed.
	pub dispositions: Vec<ArchiveSourceDisposition>,
	/// Why the extract ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it. `undefined` when it ran to the end, failures of single entries included.
	pub error: Option<JobError>,
}

#[js_type(export, no_deser)]
pub struct CompressRenamedEntry {
	pub source_uuid: Uuid,
	pub source_path: String,
	/// The name the item has in the archive.
	pub name: String,
	pub reason: RenameReason,
}

#[js_type(export, no_deser)]
pub struct CompressHashMismatch {
	pub source_uuid: Uuid,
	pub path: String,
}

#[derive(Debug, Clone)]
#[js_type(export, no_deser, tagged, no_default)]
pub enum CompressEvent {
	Skipped(SkippedEntry),
	Renamed(CompressRenamedEntry),
	/// A source's data did not match the hash in its metadata; it went into the archive as it
	/// was read, and is kept.
	SourceHashMismatch(CompressHashMismatch),
	SourceDisposition(ArchiveSourceDisposition),
	PropagationFailed(ArchiveItemError),
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
	pub renamed: Vec<CompressRenamedEntry>,
	pub totals: PlanTotals,
	pub counts: CompressCounts,
	/// The storage the archive needed, when a storage limit refused it up front.
	pub needed_bytes: Option<u64>,
	/// What became of the sources, when they were to be removed.
	pub dispositions: Vec<ArchiveSourceDisposition>,
	/// Source files whose data did not match the hash in their metadata (up to 1000).
	pub hash_mismatches: Vec<CompressHashMismatch>,
	/// Why the compress ended early: kind `Cancelled` when cancelled, or the error that stopped
	/// it. `undefined` when it ran to the end.
	pub error: Option<JobError>,
}

fn millis(duration: Duration) -> u64 {
	u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
				Self::PropagationFailed(ArchiveItemError {
					dest_uuid,
					error: job_error(error),
				})
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

impl From<compress::CompressEvent> for CompressEvent {
	fn from(event: compress::CompressEvent) -> Self {
		use compress::CompressEvent as Event;
		match event {
			Event::Skipped(entry) => Self::Skipped(entry),
			Event::Renamed(entry) => Self::Renamed(CompressRenamedEntry {
				source_uuid: entry.source_uuid,
				source_path: entry.source_path,
				name: entry.name.into(),
				reason: entry.reason,
			}),
			Event::SourceHashMismatch { source_uuid, path } => {
				Self::SourceHashMismatch(CompressHashMismatch { source_uuid, path })
			}
			Event::SourceDisposition(disposition) => Self::SourceDisposition(disposition.into()),
			Event::PropagationFailed { dest_uuid, error } => {
				Self::PropagationFailed(ArchiveItemError {
					dest_uuid,
					error: job_error(error),
				})
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
			renamed: report
				.renamed
				.into_iter()
				.map(|entry| CompressRenamedEntry {
					source_uuid: entry.source_uuid,
					source_path: entry.source_path,
					name: entry.name.into(),
					reason: entry.reason,
				})
				.collect(),
			totals: report.totals,
			counts: report.counts,
			needed_bytes: report.needed_bytes,
			dispositions: report.dispositions.into_iter().map(Into::into).collect(),
			hash_mismatches: report
				.hash_mismatches
				.into_iter()
				.map(|mismatch| CompressHashMismatch {
					source_uuid: mismatch.source_uuid,
					path: mismatch.path,
				})
				.collect(),
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

/// What `extractArchive` extracts, where, and whether the archive goes afterwards.
fn extract_request(
	archive: AnyFile,
	destination: AnyNormalDir,
	into: ExtractInto,
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
	let root = match into {
		ExtractInto::NewFolder { name } => ExtractRoot::NewFolder {
			name: name.as_deref().map(ValidatedName::try_from).transpose()?,
		},
		ExtractInto::Destination => ExtractRoot::Destination,
	};
	Ok(ExtractRequest::All {
		archive,
		destination: DirType::from(destination),
		root,
	})
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
	fn new(
		items: Vec<AnyItemWithContext>,
		destination: AnyNormalDir,
		name: &str,
		format: CompressFormat,
		max_bytes: Option<u64>,
		dispose: Option<SourceDisposal>,
		password: Option<ArchivePassword>,
	) -> Result<Self, Error> {
		// the format's own rules reject the call, as an invalid argument, rather than end in a
		// failed job
		format.check_name(name)?;
		format.check(password.is_some())?;
		Ok(Self {
			sources: compress_sources(items, dispose)?,
			destination: DirType::from(destination),
			name: ValidatedName::try_from(name)?,
			config: CompressConfig {
				format,
				max_bytes,
				password,
			},
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
			CompressRequest {
				sources: call.sources,
				destination: call.destination,
				name: call.name,
			},
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
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveEncoderMemory")
)]
pub fn archive_encoder_memory(format: CompressFormat) -> Result<u64, Error> {
	format.encoder_memory()
}

/// The name of the folder an archive extracts to by default: its name without its archive
/// extension (`photos.tar.gz` → `photos`), made into a valid name (`Archive` when nothing is
/// left).
#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg_attr(
	feature = "wasm-full",
	wasm_bindgen::prelude::wasm_bindgen(js_name = "archiveDefaultName")
)]
pub fn archive_default_name(name: String) -> String {
	super::format::extract_folder_name(Some(&name)).into()
}

#[cfg(feature = "uniffi")]
mod uniffi_impl {
	use std::sync::Arc;

	use crate::{
		Error,
		auth::JsClient,
		js::{
			AnyFile, AnyItemWithContext, AnyNormalDir, File, ManagedFuture, spawn_ordered_dispatch,
		},
	};

	use super::{
		CompressCall, CompressDelivery, CompressFormat, CompressReport, CompressUpdate,
		ExpansionLimit, ExtractConfig, ExtractDelivery, ExtractInto, ExtractReport, ExtractUpdate,
		ExtractedTopLevelItem, SourceDisposal, compress_job, extract_job, extract_request,
		password,
	};

	/// Receives an extract's progress, in the order the extract made it, before the call
	/// returns.
	#[uniffi::export(with_foreign)]
	pub trait ExtractArchiveCallback: Send + Sync {
		/// Top-level items created, in batches: every one of them, also past the 1000 the report
		/// keeps.
		fn on_top_level_created(&self, items: Vec<ExtractedTopLevelItem>);
		fn on_update(&self, update: ExtractUpdate);
	}

	/// Receives a compress's progress, in the order the compress made it, before the call
	/// returns.
	#[uniffi::export(with_foreign)]
	pub trait CompressItemsCallback: Send + Sync {
		/// The archive is registered in the destination.
		fn on_archive_created(&self, archive: File);
		fn on_update(&self, update: CompressUpdate);
	}

	#[derive(uniffi::Record, Default)]
	pub struct ExtractArchiveConfig {
		/// Storage still free on the account, if known: a zip or 7z stating more fails before
		/// anything is written; a streaming archive (tar, one compressed file) fails once it has
		/// written that much, keeping what it extracted.
		#[uniffi(default = None)]
		pub max_bytes: Option<u64>,
		/// Most items to create.
		#[uniffi(default = None)]
		pub max_items: Option<u64>,
		/// The guard against decompression bombs; `None` keeps the SDK's (1000 times the
		/// archive, at least 256 MiB).
		#[uniffi(default = None)]
		pub expansion_limit: Option<ExpansionLimit>,
		/// Removes the archive once everything in it was extracted and verified.
		#[uniffi(default = None)]
		pub dispose: Option<SourceDisposal>,
	}

	#[derive(uniffi::Record)]
	pub struct CompressItemsConfig {
		pub format: CompressFormat,
		/// Storage still free on the account, if known.
		#[uniffi(default = None)]
		pub max_bytes: Option<u64>,
		/// Removes the items once the archive is registered and verified. The archive is not read
		/// back first: with an encrypted format, have the user confirm the password (type it twice)
		/// before removing anything for good, as a mistyped one leaves an archive nobody can open.
		#[uniffi(default = None)]
		pub dispose: Option<SourceDisposal>,
	}

	#[uniffi::export]
	impl JsClient {
		/// Extracts `archive` (zip, 7z, tar and its compressed forms, or one compressed file)
		/// into `destination`, entirely on this device: the archive is downloaded, decrypted and
		/// decoded as a stream, and every entry uploaded as a new item. A name taken at the
		/// destination is kept, and the entry is named `name (1)`, ...
		///
		/// `password` opens an encrypted archive; without one the extract fails before anything is
		/// created. A wrong one is found before anything is created too when the archive has an
		/// encrypted entry small enough to check it on (16 MiB), else as the first encrypted entry
		/// is read; folders created by then go to the trash. The report is returned whether the extract
		/// completed, was cancelled or failed. Only an abort through `managed_future` gets
		/// that report: cancelling the calling coroutine or task drops the call, and with it the
		/// report (the job is stopped at once).
		#[allow(clippy::too_many_arguments)]
		pub async fn extract_archive(
			&self,
			archive: AnyFile,
			destination: AnyNormalDir,
			into: ExtractInto,
			config: ExtractArchiveConfig,
			password: Option<String>,
			callback: Arc<dyn ExtractArchiveCallback>,
			managed_future: ManagedFuture,
		) -> Result<ExtractReport, Error> {
			// wrapped first, so an argument refused below still drops it wiped
			let password = self::password(password)?;
			let request = extract_request(archive, destination, into, config.dispose)?;
			let config = ExtractConfig {
				max_bytes: config.max_bytes,
				max_items: config.max_items,
				expansion_limit: config
					.expansion_limit
					.or(ExtractConfig::default().expansion_limit),
				password,
				// the bindings do not expose the option yet: the SDK's default
				skip_mac_metadata: ExtractConfig::default().skip_mac_metadata,
			};
			let client = self.inner();
			// the foreign callbacks run on the dispatch thread, in order; waiting for them is no
			// part of the job, so a cancel's grace never cuts off a report already made
			let (sender, delivered) = spawn_ordered_dispatch(move |delivery| match delivery {
				ExtractDelivery::TopLevelCreated(items) => callback.on_top_level_created(items),
				ExtractDelivery::Update(update) => callback.on_update(update),
			});
			let result = managed_future
				.into_js_managed_commander_job(move |control| {
					extract_job(client, request, config, sender, control)
				})
				.await;
			// the job has ended and dropped its sender: this returns once all it reported was
			// delivered
			let _ = delivered.await;
			result
		}

		/// Compresses `items` into a new archive `name` in `destination`, entirely on this
		/// device. `name` has to end in the format's extension (see `archive_extension`); a name
		/// taken at the destination is kept, and the archive is named `name (1).ext`, ...
		///
		/// `password` is required exactly when the format is encrypted. The report is returned
		/// whether the compress completed, was cancelled or failed; a compress that did not
		/// complete leaves nothing in the drive. Only an abort through `managed_future` gets
		/// that report: cancelling the calling coroutine or task drops the call, and with it the
		/// report (the job is stopped at once).
		#[allow(clippy::too_many_arguments)]
		pub async fn compress_items(
			&self,
			items: Vec<AnyItemWithContext>,
			destination: AnyNormalDir,
			name: String,
			config: CompressItemsConfig,
			password: Option<String>,
			callback: Arc<dyn CompressItemsCallback>,
			managed_future: ManagedFuture,
		) -> Result<CompressReport, Error> {
			let call = CompressCall::new(
				items,
				destination,
				&name,
				config.format,
				config.max_bytes,
				config.dispose,
				self::password(password)?,
			)?;
			let client = self.inner();
			let (sender, delivered) = spawn_ordered_dispatch(move |delivery| match delivery {
				CompressDelivery::ArchiveCreated(archive) => callback.on_archive_created(archive),
				CompressDelivery::Update(update) => callback.on_update(update),
			});
			let result = managed_future
				.into_js_managed_commander_job(move |control| {
					compress_job(client, call, sender, control)
				})
				.await;
			let _ = delivered.await;
			result
		}
	}
}

#[cfg(feature = "wasm-full")]
mod wasm_impl {
	use std::sync::Arc;

	use filen_macros::js_type;
	use serde::Serialize;
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	use crate::{
		Error,
		auth::JsClient,
		js::{AnyFile, AnyItemWithContext, AnyNormalDir, ManagedFuture},
	};

	use super::{
		Client, CompressCall, CompressDelivery, CompressFormat, CompressReport, ExpansionLimit,
		ExtractConfig, ExtractDelivery, ExtractInto, ExtractReport, ExtractRequest, SourceDisposal,
		compress_job, extract_job, extract_request, password,
	};

	#[js_type(import, no_ser, no_default)]
	pub struct ExtractArchiveParams {
		pub archive: AnyFile,
		pub destination: AnyNormalDir,
		pub into: ExtractInto,
		/// Storage still free on the account, if known: a zip or 7z stating more fails before
		/// anything is written; a streaming archive (tar, one compressed file) fails once it has
		/// written that much, keeping what it extracted.
		#[serde(default)]
		#[tsify(type = "number | bigint", optional)]
		pub max_bytes: Option<u64>,
		/// Most items to create.
		#[serde(default)]
		#[tsify(type = "number | bigint", optional)]
		pub max_items: Option<u64>,
		/// The guard against decompression bombs; when left out, the SDK's (1000 times the
		/// archive, at least 256 MiB).
		#[serde(default)]
		#[tsify(optional)]
		pub expansion_limit: Option<ExpansionLimit>,
		/// Removes the archive once everything in it was extracted and verified.
		#[serde(default)]
		#[tsify(optional)]
		pub dispose: Option<SourceDisposal>,
		#[tsify(type = "(update: ExtractUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// Top-level items created, in batches: every one of them, also past the 1000 the report
		/// keeps.
		#[tsify(type = "(items: ExtractedTopLevelItem[]) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_top_level_created: Option<js_sys::Function>,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	#[js_type(import, no_ser, no_default)]
	pub struct CompressItemsParams {
		pub items: Vec<AnyItemWithContext>,
		pub destination: AnyNormalDir,
		/// The archive's name, ending in the format's extension (see `archiveExtension`).
		pub name: String,
		pub format: CompressFormat,
		/// Storage still free on the account, if known.
		#[serde(default)]
		#[tsify(type = "number | bigint", optional)]
		pub max_bytes: Option<u64>,
		/// Removes the items once the archive is registered and verified. The archive is not read
		/// back first: with an encrypted format, have the user confirm the password (type it twice)
		/// before removing anything for good, as a mistyped one leaves an archive nobody can open.
		#[serde(default)]
		#[tsify(optional)]
		pub dispose: Option<SourceDisposal>,
		#[tsify(type = "(update: CompressUpdate) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_update: Option<js_sys::Function>,
		/// The archive is registered in the destination.
		#[tsify(type = "(archive: File) => void", optional)]
		#[serde(default, deserialize_with = "crate::js::optional_function")]
		pub on_archive_created: Option<js_sys::Function>,
		// A direct (never flattened) field, so the abort and pause signals stay live JS values.
		#[serde(default)]
		pub managed_future: ManagedFuture,
	}

	/// Calls `callback` with `value`, if the caller passed one.
	fn call(callback: Option<&js_sys::Function>, value: &impl Serialize) {
		let Some(callback) = callback else {
			return;
		};
		let serializer = serde_wasm_bindgen::Serializer::new()
			.serialize_maps_as_objects(true)
			.serialize_large_number_types_as_bigints(true);
		let value = value
			.serialize(&serializer)
			.expect("failed to serialize an archive callback (should be impossible)");
		let _ = callback.call1(&JsValue::UNDEFINED, &value);
	}

	async fn run_extract(
		client: Arc<Client>,
		request: ExtractRequest,
		config: ExtractConfig,
		on_update: Option<js_sys::Function>,
		on_top_level_created: Option<js_sys::Function>,
		managed_future: ManagedFuture,
	) -> Result<ExtractReport, Error> {
		// The JS functions never leave this thread: the job sends its callbacks over a channel,
		// and this task calls them here, in order.
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
		let (drained, drained_receiver) = tokio::sync::oneshot::channel::<()>();
		crate::runtime::spawn_local(async move {
			while let Some(delivery) = receiver.recv().await {
				match delivery {
					ExtractDelivery::TopLevelCreated(items) => {
						call(on_top_level_created.as_ref(), &items)
					}
					ExtractDelivery::Update(update) => call(on_update.as_ref(), &update),
				}
			}
			let _ = drained.send(());
		});
		let result = managed_future
			.into_js_managed_commander_job(move |control| {
				extract_job(client, request, config, sender, control)
			})?
			.await;
		let _ = drained_receiver.await;
		result
	}

	async fn run_compress(
		client: Arc<Client>,
		call_: CompressCall,
		on_update: Option<js_sys::Function>,
		on_archive_created: Option<js_sys::Function>,
		managed_future: ManagedFuture,
	) -> Result<CompressReport, Error> {
		let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
		let (drained, drained_receiver) = tokio::sync::oneshot::channel::<()>();
		crate::runtime::spawn_local(async move {
			while let Some(delivery) = receiver.recv().await {
				match delivery {
					CompressDelivery::ArchiveCreated(archive) => {
						call(on_archive_created.as_ref(), &archive)
					}
					CompressDelivery::Update(update) => call(on_update.as_ref(), &update),
				}
			}
			let _ = drained.send(());
		});
		let result = managed_future
			.into_js_managed_commander_job(move |control| {
				compress_job(client, call_, sender, control)
			})?
			.await;
		let _ = drained_receiver.await;
		result
	}

	#[wasm_bindgen(js_class = "Client")]
	impl JsClient {
		/// Extracts an archive (zip, 7z, tar and its compressed forms, or one compressed file)
		/// into a directory, entirely in this browser: the archive is downloaded, decrypted and
		/// decoded as a stream, and every entry uploaded as a new item. A name taken at the
		/// destination is kept, and the entry is named `name (1)`, ...
		///
		/// `password` opens an encrypted archive; without one the extract fails before anything is
		/// created. A wrong one is found before anything is created too when the archive has an
		/// encrypted entry small enough to check it on (16 MiB), else as the first encrypted entry
		/// is read; folders created by then go to the trash. The report is returned whether the extract
		/// completed, was cancelled or failed.
		#[wasm_bindgen(js_name = "extractArchive")]
		pub async fn extract_archive(
			&self,
			params: ExtractArchiveParams,
			password: Option<String>,
		) -> Result<ExtractReport, Error> {
			// wrapped first, so an argument refused below still drops it wiped
			let password = self::password(password)?;
			let request = extract_request(
				params.archive,
				params.destination,
				params.into,
				params.dispose,
			)?;
			let config = ExtractConfig {
				max_bytes: params.max_bytes,
				max_items: params.max_items,
				expansion_limit: params
					.expansion_limit
					.or(ExtractConfig::default().expansion_limit),
				password,
				// the bindings do not expose the option yet: the SDK's default
				skip_mac_metadata: ExtractConfig::default().skip_mac_metadata,
			};
			run_extract(
				self.inner(),
				request,
				config,
				params.on_update,
				params.on_top_level_created,
				params.managed_future,
			)
			.await
		}

		/// Compresses items into a new archive in a directory, entirely in this browser. `name`
		/// has to end in the format's extension (see `archiveExtension`); a name taken at the
		/// destination is kept, and the archive is named `name (1).ext`, ...
		///
		/// `password` is required exactly when the format is encrypted. The report is returned
		/// whether the compress completed, was cancelled or failed; a compress that did not
		/// complete leaves nothing in the drive.
		#[wasm_bindgen(js_name = "compressItems")]
		pub async fn compress_items(
			&self,
			params: CompressItemsParams,
			password: Option<String>,
		) -> Result<CompressReport, Error> {
			let call = CompressCall::new(
				params.items,
				params.destination,
				&params.name,
				params.format,
				params.max_bytes,
				params.dispose,
				self::password(password)?,
			)?;
			run_compress(
				self.inner(),
				call,
				params.on_update,
				params.on_archive_created,
				params.managed_future,
			)
			.await
		}
	}
}

#[cfg(all(test, feature = "uniffi"))]
mod tests {
	use chrono::Utc;
	use filen_types::fs::ParentUuid;

	use super::*;
	use crate::{
		crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
		fs::{
			HasUUID,
			archive::{
				compress::{SevenZMethod, ZipMethod},
				dispose::{DisposalOutcome, KeptReason, SourceDisposition},
				zip::crypto::AesStrength,
			},
			dir::{
				RemoteDirectory, RootDirectory,
				meta::{DecryptedDirectoryMeta, DirectoryMeta},
			},
			file::meta::{DecryptedFileMeta, FileMeta},
		},
		job::report::JobFailed,
		js::Root,
	};

	fn remote_file() -> RemoteFile {
		RemoteFile::from_meta(
			Uuid::new_v4(),
			filen_types::fs::StableUuid::new_for_test(Uuid::new_v4()),
			Uuid::new_v4().into(),
			10,
			1,
			"de-1",
			"bucket",
			Utc::now(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Borrowed("a.zip"),
				size: 10,
				mime: Cow::Borrowed("application/zip"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: Utc::now(),
				created: None,
				hash: None,
			}),
		)
	}

	fn dir() -> RemoteDirectory {
		RemoteDirectory::from_meta(
			Uuid::new_v4(),
			ParentUuid::Uuid(Uuid::new_v4()),
			filen_types::api::v3::dir::color::DirColor::Blue,
			false,
			Utc::now(),
			DirectoryMeta::Decoded(DecryptedDirectoryMeta {
				name: Cow::Borrowed("Photos"),
				created: None,
			}),
		)
	}

	fn destination() -> AnyNormalDir {
		AnyNormalDir::Dir(dir().into())
	}

	#[test]
	fn an_archive_to_remove_has_to_be_the_users_own() {
		let file = remote_file();
		let request = extract_request(
			AnyFile::File(file.clone().into()),
			destination(),
			ExtractInto::NewFolder { name: None },
			Some(SourceDisposal::Trash),
		)
		.unwrap();
		let ExtractRequest::All {
			archive: ArchiveSource::Dispose {
				file: disposed,
				how,
			},
			root: ExtractRoot::NewFolder { name: None },
			..
		} = request
		else {
			panic!("an archive to remove, into a new folder");
		};
		assert_eq!((disposed.uuid(), how), (file.uuid(), SourceDisposal::Trash));

		let error = extract_request(
			AnyFile::File(remote_file().into()),
			destination(),
			ExtractInto::NewFolder {
				name: Some(String::new()),
			},
			Some(SourceDisposal::DeletePermanently),
		)
		.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidName, "an empty folder name");
	}

	#[test]
	fn names_and_passwords_are_checked_at_the_edge() {
		let error = extract_request(
			AnyFile::File(remote_file().into()),
			destination(),
			ExtractInto::NewFolder {
				name: Some("a/b".into()),
			},
			None,
		)
		.unwrap_err();
		assert_eq!(error.kind(), ErrorKind::InvalidName);
		assert_eq!(
			password(Some(String::new())).unwrap_err().kind(),
			ErrorKind::InvalidState
		);
		assert!(password(None).unwrap().is_none());
		let call = CompressCall::new(
			Vec::new(),
			destination(),
			"a/b.zip",
			CompressFormat::Zip {
				method: ZipMethod::Stored,
				encryption: None,
			},
			None,
			None,
			None,
		);
		assert_eq!(call.err().unwrap().kind(), ErrorKind::InvalidName);
	}

	#[test]
	fn items_to_remove_have_to_be_the_users_own_and_not_the_root() {
		let file = remote_file();
		let dir = dir();
		let sources = compress_sources(
			vec![
				AnyItemWithContext::File(AnyFile::File(file.clone().into())),
				AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Dir(
					dir.clone().into(),
				))),
			],
			Some(SourceDisposal::DeletePermanently),
		)
		.unwrap();
		let CompressSources::Dispose { how, items } = sources else {
			panic!("sources to remove");
		};
		assert_eq!(how, SourceDisposal::DeletePermanently);
		assert_eq!(
			items.iter().map(HasUUID::uuid).collect::<Vec<_>>(),
			[file.uuid(), dir.uuid()]
		);
		let root = AnyItemWithContext::Dir(AnyDirWithContext::Normal(AnyNormalDir::Root(
			Root::from(RootDirectory::new(Uuid::new_v4())),
		)));
		for dispose in [None, Some(SourceDisposal::Trash)] {
			let error = compress_sources(vec![root.clone()], dispose).unwrap_err();
			assert_eq!(error.kind(), ErrorKind::InvalidState, "{dispose:?}");
		}
	}

	#[test]
	fn helpers_name_the_formats() {
		assert_eq!(
			archive_extension(CompressFormat::SevenZ {
				method: SevenZMethod::Lzma2 { level: 5 },
				solid: true,
				encryption: None,
			}),
			".7z"
		);
		assert_eq!(
			archive_encoder_memory(CompressFormat::Zip {
				method: ZipMethod::Deflate { level: 10 },
				encryption: Some(AesStrength::Aes256),
			})
			.unwrap_err()
			.kind(),
			ErrorKind::InvalidState
		);
		assert_eq!(archive_default_name("photos.tar.gz".into()), "photos");
	}

	#[test]
	fn a_report_carries_why_the_job_ended_and_what_became_of_its_sources() {
		let removal = Arc::new(Error::custom(ErrorKind::Server, "no"));
		let report = extract::ExtractReport {
			top_level: Vec::new(),
			failures: Vec::new(),
			skipped: Vec::new(),
			renamed: Vec::new(),
			misleading_names: Vec::new(),
			omitted: OmittedRecords::default(),
			totals: ArchiveTotals::Streaming { archive_bytes: 0 },
			counts: ItemCounts::default(),
			unaccounted_bytes: 0,
			duplicates: None,
			dispositions: vec![
				SourceDisposition {
					uuid: Uuid::new_v4(),
					outcome: DisposalOutcome::Kept {
						reason: KeptReason::Failed {
							error: Arc::clone(&removal),
						},
						bytes_freed: 3,
					},
				},
				SourceDisposition {
					uuid: Uuid::new_v4(),
					outcome: DisposalOutcome::Disposed {
						how: SourceDisposal::DeletePermanently,
						bytes_freed: 7,
					},
				},
			],
		};
		let ended = Arc::new(Error::custom(ErrorKind::Cancelled, "cancelled"));
		let report = ExtractReport::from(JobFailed {
			report,
			error: Arc::clone(&ended),
		});
		assert!(Arc::ptr_eq(report.error.as_ref().unwrap(), &ended));
		let [first, second] = report.dispositions.as_slice() else {
			panic!("two dispositions");
		};
		let ArchiveDisposalOutcome::Kept {
			reason: ArchiveKeptReason::Failed { error },
			bytes_freed: 3,
		} = &first.outcome
		else {
			panic!("{:?}", first.outcome);
		};
		assert!(Arc::ptr_eq(error, &removal));
		assert!(matches!(
			second.outcome,
			ArchiveDisposalOutcome::Disposed { bytes_freed: 7, .. }
		));
		assert!(
			CompressReport::from(compress::CompressReport::default())
				.error
				.is_none()
		);
	}
}
