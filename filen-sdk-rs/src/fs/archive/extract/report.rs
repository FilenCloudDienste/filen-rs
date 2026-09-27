//! What an extraction reports while it runs and when it ends, and the [`Reporter`] that turns
//! job state changes into throttled, ordered callbacks.

use std::{sync::Arc, time::Duration};

use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	Error,
	fs::{
		archive::limits::MAX_REPORT_RECORDS,
		categories::{NonRootItemType, Normal},
		drive_job::counts::ItemCounts,
	},
	job::{
		self,
		report::{JobFailed, JobPhase, JobReport, JobState, Progress, RunCore, Snapshot, Units},
	},
	util::{MaybeArc, MaybeSendSync},
};

pub use crate::job::report::RunState;

use super::{DuplicateEntries, ExtractSkipReason};
use crate::fs::archive::dispose::SourceDisposition;

/// Where an extraction is. The last three are where it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum ExtractPhase {
	/// Waiting for another archive job to finish; nothing is held meanwhile.
	WaitingForWorker,
	/// Reading the start of the archive to tell what it holds.
	Scanning,
	/// Reading the entries and creating them in the drive.
	Extracting,
	/// Checking whether the destination became shared or linked during the extraction.
	Finishing,
	/// Removing the archive, once the extraction is verified.
	DisposingSources,
	/// Ran to its end.
	Done,
	/// Ended early by a cancel.
	Cancelled,
	/// Ended early by an error that affects the whole job (a damaged archive, no storage left).
	Failed,
}

impl JobPhase for ExtractPhase {
	const DONE: Self = Self::Done;
	const CANCELLED: Self = Self::Cancelled;
	const FAILED: Self = Self::Failed;
}

/// An archive entry. Only meaningful with the archive it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[js_type(export, no_deser, no_default)]
pub struct ArchiveEntryId {
	/// The archive's uuid.
	pub archive: Uuid,
	/// The entry's position among the archive's members.
	pub index: u32,
}

/// How much there is to extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub enum ArchiveTotals {
	/// Progress is how much of the archive has been read, for every format: a tar's entries
	/// are only known as they come, and a zip's or 7z's entry counts are not reported.
	Streaming { archive_bytes: u64 },
}

/// A file being extracted right now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractActiveFile {
	/// The entry being extracted.
	pub entry: ArchiveEntryId,
	/// The uuid of the file being created.
	pub dest_uuid: Uuid,
	/// The directory it is created in.
	pub dest_parent: Uuid,
	/// The name it is created under.
	pub name: String,
	/// The size in bytes the archive states, if it states one.
	pub size: Option<u64>,
	/// Bytes of it uploaded so far.
	pub bytes_done: u64,
}

/// What failed for an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ExtractStage {
	/// Creating the directory, or for a file the directory it goes in.
	CreateDirectory,
	/// Uploading the file's data (for a tar hard link, reading the file it copies).
	Upload,
	/// Registering the uploaded file in its directory.
	Finalize,
	/// The file was registered, but the server made it a new version of an existing file with
	/// the same name instead of a new file (possible only if a client writing without the drive
	/// lock took the name at the last moment).
	RegisteredAsVersion {
		/// The stable uuid of the file the entry became a version of.
		existing_file: Uuid,
	},
}

/// An entry that was not extracted because something went wrong.
#[derive(Debug, Clone)]
pub struct ExtractFailure {
	/// The entry that failed.
	pub entry: ArchiveEntryId,
	/// The entry's path in the archive, as drive names.
	pub path: String,
	/// The directory it was to be created in.
	pub dest_parent: Uuid,
	/// The name it was to be created under.
	pub dest_name: String,
	/// What failed.
	pub stage: ExtractStage,
	/// Shared because [`Error`] is not `Clone`, and one failure goes both into an event and into
	/// the report.
	pub error: Arc<Error>,
}

/// An entry the extraction left out on purpose.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractSkippedEntry {
	/// The entry left out.
	pub entry: ArchiveEntryId,
	/// The path as the archive stores it, cut to at most 4096 bytes.
	pub path: String,
	/// Whether `path` was cut.
	pub path_truncated: bool,
	/// Bytes of data the archive stores for it.
	pub bytes: u64,
	/// Why it was left out.
	pub reason: ExtractSkipReason,
}

/// Why an entry was created under another name than the archive gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum ExtractRenameReason {
	/// The name was taken in its directory (by another entry, or an item already there), so
	/// the entry got the next keep-both name.
	DuplicateName,
	/// The stored path was made into valid drive names: an absolute or drive prefix stripped,
	/// a name encoded or shortened, or a non-UTF-8 name decoded as Latin-1.
	PathRewritten,
}

/// An entry created under another name than the archive gives it; recorded once it is created,
/// with the name it got then.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractRenamedEntry {
	/// The renamed entry.
	pub entry: ArchiveEntryId,
	/// The entry's path in the archive, as drive names.
	pub path: String,
	/// The name it was created under.
	pub name: String,
	/// Why it got that name.
	pub reason: ExtractRenameReason,
}

/// An entry whose path holds characters that make it read as something it is not: a bidi
/// override showing `invoice\u{202E}fdp.exe` as `invoiceexe.pdf`, a zero-width or other
/// invisible character, or a control character. Its name is kept as the archive gives it (the
/// drive allows these characters); an app may want to warn before the item is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ExtractMisleadingName {
	/// The entry with that name.
	pub entry: ArchiveEntryId,
	/// The entry's path in the archive, as drive names.
	pub path: String,
}

/// Which created item a top-level item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[js_type(export, no_deser, tagged, camel_case_fields, no_default)]
pub enum ExtractTopLevelKey {
	/// The folder the archive was extracted into.
	Root,
	/// An item created directly in the destination, for this entry (a directory named only by
	/// the paths below it takes the first entry that named it).
	Entry {
		/// The entry.
		id: ArchiveEntryId,
	},
}

/// An item the extraction created directly in the destination.
#[derive(Debug, Clone)]
pub struct ExtractedTopLevel {
	/// Which created item it is.
	pub key: ExtractTopLevelKey,
	/// The item as created.
	pub item: NonRootItemType<'static, Normal>,
}

/// Records a report only counts, past [`MAX_REPORT_RECORDS`] of each kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct OmittedRecords {
	/// Skipped entries left out of `skipped`.
	pub skipped: u64,
	/// Renamed entries left out of `renamed`.
	pub renamed: u64,
	/// Misleading names left out of `misleading_names`.
	pub misleading_names: u64,
	/// Failures left out of `failures`.
	pub failures: u64,
	/// Top-level items left out of `top_level`.
	pub top_level: u64,
}

/// Something that happened to one item, reported in the next update.
#[derive(Debug, Clone)]
pub enum ExtractEvent {
	/// A directory was created.
	DirCreated {
		/// The directory's uuid.
		dest_uuid: Uuid,
		/// The directory it was created in.
		dest_parent: Uuid,
		/// The name it was created under.
		name: String,
	},
	/// A directory could not be created.
	DirFailed(ExtractFailure),
	/// A file started being extracted.
	FileStarted(ExtractActiveFile),
	/// A file was extracted and registered.
	FileDone {
		/// Its entry.
		entry: ArchiveEntryId,
		/// The file's uuid.
		dest_uuid: Uuid,
		/// The directory it was created in.
		dest_parent: Uuid,
		/// The name it was created under.
		name: String,
		/// Its size in bytes.
		size: u64,
	},
	/// A file could not be extracted.
	FileFailed(ExtractFailure),
	/// An entry was left out on purpose.
	Skipped(ExtractSkippedEntry),
	/// An entry was created under another name than the archive gives it.
	Renamed(ExtractRenamedEntry),
	/// An entry is being extracted under a name that reads as something it is not.
	MisleadingName(ExtractMisleadingName),
	/// What became of the archive, when it was to be removed.
	SourceDisposition(SourceDisposition),
	/// The item was created but could not be added to one of the destination's public links or
	/// shares.
	PropagationFailed {
		/// The item's uuid.
		dest_uuid: Uuid,
		/// Shared because [`Error`] is not `Clone` and events are.
		error: Arc<Error>,
	},
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
pub struct ExtractUpdate {
	/// Where the job is.
	pub phase: ExtractPhase,
	/// Whether it runs, is paused or winds down.
	pub run_state: RunState,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Bytes of the archive read so far.
	pub bytes_read: u64,
	/// The files being extracted right now.
	pub active: Vec<ExtractActiveFile>,
	/// What happened since the last update, in order.
	pub events: Vec<ExtractEvent>,
	/// Bytes of files extracted per second, over the last 10 seconds of running time; `None`
	/// until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// The time left to read the rest of the archive, at the rate it was read over the last 10
	/// seconds of running time; `None` while there is no rate or the job winds down, zero once
	/// it ended.
	pub eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub active_time: Duration,
}

/// The outcome of an extraction, whether it completed, was cancelled or failed. Directories
/// and files it created stay, except the folders a late wrong password sends to the trash (see
/// `top_level`); a file it was writing when it stopped never becomes visible.
///
/// An extraction knows no totals up front: an entry is only known once it is read. What one
/// that ended early leaves [not attempted](ItemCounts::files_not_attempted) is what it had
/// started or planned: the files it was writing and the directories it had not created yet.
#[derive(Debug)]
pub struct ExtractReport {
	/// Items created directly in the destination (the new folder, or with
	/// [`ExtractRoot::Destination`](super::ExtractRoot::Destination) every item at the top of
	/// the archive), in creation order, up to [`MAX_REPORT_RECORDS`]; the callback receives all
	/// of them, as they are created. An extraction that failed with
	/// [`ErrorKind::ArchiveWrongPassword`](crate::ErrorKind) before extracting any file tried
	/// to move the folders it had created to the trash, for a retry to start clean: those it
	/// moved are left out here, though the callback received them, and `counts.dirs_created`
	/// still counts them, and the directories in them, as created.
	pub top_level: Vec<ExtractedTopLevel>,
	/// The entries that were not extracted because something went wrong, up to 1000.
	pub failures: Vec<ExtractFailure>,
	/// The entries left out on purpose, up to 1000.
	pub skipped: Vec<ExtractSkippedEntry>,
	/// The entries created under another name than the archive gives them, up to 1000.
	pub renamed: Vec<ExtractRenamedEntry>,
	/// The entries whose names read as something they are not, up to 1000.
	pub misleading_names: Vec<ExtractMisleadingName>,
	/// What the lists above only count.
	pub omitted: OmittedRecords,
	pub totals: ArchiveTotals,
	pub counts: ItemCounts,
	/// Bytes in the archive after its last entry that belong to none (another archive appended
	/// to it, say), counted from the first non-zero one; zero padding is not counted. For a zip,
	/// the bytes before its first entry (a self-extracting stub, say).
	pub unaccounted_bytes: u64,
	/// Names a zip lists more than once; the last entry of each was extracted.
	pub duplicates: Option<DuplicateEntries>,
	/// What became of the archive, when it was to be removed.
	pub dispositions: Vec<SourceDisposition>,
}

impl JobReport for ExtractReport {
	const NAME: &'static str = "extraction";
}

/// An extraction that ended early: cancelled, or stopped by an error that affects the whole job.
pub type ExtractFailed = JobFailed<ExtractReport>;

/// Receives an extraction's progress. All calls come from the one job, in order.
pub trait ExtractCallback: MaybeSendSync + 'static {
	/// Items created directly in the destination, delivered as soon as they exist and before
	/// any update counts them, so a caller can clean up even after an abrupt end.
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>);
	fn on_update(&self, update: ExtractUpdate);
}

impl<T: ExtractCallback + ?Sized> ExtractCallback for Arc<T> {
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>) {
		(**self).on_top_level_created(items);
	}

	fn on_update(&self, update: ExtractUpdate) {
		(**self).on_update(update);
	}
}

/// What an extraction counts, next to the job-agnostic [`RunCore`].
pub(crate) struct ExtractState {
	core: RunCore<ExtractEvent, ExtractPhase>,
	totals: ArchiveTotals,
	counts: ItemCounts,
	bytes_read: u64,
	active: Vec<ExtractActiveFile>,
	/// Top-level items created and not delivered yet: they go out in batches, each before the
	/// next update.
	pending_top_level: Vec<ExtractedTopLevel>,
	/// The job ended: whatever of the archive it did not read, it never will.
	ended: bool,
}

/// Most top-level items one `on_top_level_created` call carries.
const TOP_LEVEL_BATCH: usize = 256;

impl JobState for ExtractState {
	type Phase = ExtractPhase;
	type Event = ExtractEvent;
	type Callback = dyn ExtractCallback;

	fn core(&mut self) -> &mut RunCore<ExtractEvent, ExtractPhase> {
		&mut self.core
	}

	fn progress(&self) -> Progress {
		let ArchiveTotals::Streaming { archive_bytes } = self.totals;
		Progress {
			bytes_done: self.counts.bytes_done,
			units: Units {
				done: self.bytes_read,
				settled: if self.ended {
					archive_bytes
				} else {
					self.bytes_read
				},
				total: archive_bytes,
			},
		}
	}

	fn deliver(
		&mut self,
		callback: &dyn ExtractCallback,
		snapshot: Snapshot<ExtractEvent, ExtractPhase>,
	) {
		if !self.pending_top_level.is_empty() {
			callback.on_top_level_created(std::mem::take(&mut self.pending_top_level));
		}
		callback.on_update(ExtractUpdate {
			phase: snapshot.phase,
			run_state: snapshot.run_state,
			totals: self.totals,
			counts: self.counts,
			bytes_read: self.bytes_read,
			active: self.active.clone(),
			events: snapshot.events,
			bytes_per_second: snapshot.bytes_per_second,
			eta: snapshot.eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {
		self.ended = true;
		// a file still running now was never finished
		for file in std::mem::take(&mut self.active) {
			self.not_attempted(&file);
		}
	}
}

impl ExtractState {
	fn remove_active(&mut self, dest_uuid: Uuid) -> u64 {
		match self.active.iter().position(|f| f.dest_uuid == dest_uuid) {
			Some(index) => self.active.remove(index).bytes_done,
			None => 0,
		}
	}

	/// Counts a file the job started and dropped: what it uploaded is no longer done.
	fn not_attempted(&mut self, file: &ExtractActiveFile) {
		self.counts.bytes_done -= file.bytes_done;
		self.counts.files_not_attempted += 1;
		self.counts.bytes_not_attempted += file.size.unwrap_or(file.bytes_done);
	}
}

/// An extraction's reporter: the job-agnostic [`job::report::Reporter`] over an
/// [`ExtractState`].
pub(crate) type Reporter = job::report::Reporter<ExtractState>;

impl Reporter {
	pub(crate) fn new(callback: impl ExtractCallback, totals: ArchiveTotals) -> MaybeArc<Self> {
		Self::from_parts(
			ExtractState {
				core: RunCore::new(ExtractPhase::WaitingForWorker),
				totals,
				counts: ItemCounts::default(),
				bytes_read: 0,
				active: Vec::new(),
				pending_top_level: Vec::new(),
				ended: false,
			},
			Box::new(callback),
		)
	}

	pub(crate) fn set_bytes_read(&self, bytes_read: u64) {
		self.with_state(|state| {
			if state.bytes_read != bytes_read {
				state.bytes_read = bytes_read;
				state.core.mark_changed();
			}
		});
	}

	/// Delivered in a batch of up to [`TOP_LEVEL_BATCH`] items right before the next update
	/// (every job ends with one), so a caller holds every item the job created by the time the
	/// job returns.
	pub(crate) fn top_level_created(&self, item: ExtractedTopLevel) {
		let mut full = false;
		self.with_state(|state| {
			state.pending_top_level.push(item);
			state.core.mark_changed();
			full = state.pending_top_level.len() >= TOP_LEVEL_BATCH;
		});
		if full {
			self.flush_then_call(|_| {});
		}
	}

	pub(crate) fn dir_created(&self, dest_uuid: Uuid, dest_parent: Uuid, name: &str) {
		self.with_state(|state| {
			state.counts.dirs_created += 1;
			state.core.push(ExtractEvent::DirCreated {
				dest_uuid,
				dest_parent,
				name: name.to_owned(),
			});
		});
	}

	/// A failed directory; `event` is `None` once the report keeps no more records.
	pub(crate) fn dir_failed(&self, event: Option<ExtractFailure>) {
		self.with_state(|state| {
			state.counts.dirs_failed += 1;
			match event {
				Some(failure) => state.core.push(ExtractEvent::DirFailed(failure)),
				None => state.core.mark_changed(),
			}
		});
	}

	pub(crate) fn file_started(&self, file: ExtractActiveFile) {
		self.with_state(|state| {
			state.active.push(file.clone());
			state.core.push(ExtractEvent::FileStarted(file));
		});
	}

	pub(crate) fn chunk_uploaded(&self, dest_uuid: Uuid, bytes: u64) {
		self.with_state(|state| {
			state.counts.bytes_done += bytes;
			if let Some(file) = state.active.iter_mut().find(|f| f.dest_uuid == dest_uuid) {
				file.bytes_done += bytes;
			}
			state.core.mark_changed();
		});
	}

	pub(crate) fn file_done(&self, file: &ExtractActiveFile, size: u64) {
		self.with_state(|state| {
			let counted = state.remove_active(file.dest_uuid);
			state.counts.bytes_done = state.counts.bytes_done - counted + size;
			state.counts.files_done += 1;
			state.core.push(ExtractEvent::FileDone {
				entry: file.entry,
				dest_uuid: file.dest_uuid,
				dest_parent: file.dest_parent,
				name: file.name.clone(),
				size,
			});
		});
	}

	/// A failed file (`dest_uuid` is `None` if it never started); the bytes it uploaded move
	/// from done to failed. `event` is `None` once the report keeps no more records.
	pub(crate) fn file_failed(
		&self,
		dest_uuid: Option<Uuid>,
		bytes: u64,
		event: Option<ExtractFailure>,
	) {
		self.with_state(|state| {
			let counted = dest_uuid.map_or(0, |uuid| state.remove_active(uuid));
			state.counts.bytes_done -= counted;
			state.counts.bytes_failed =
				state.counts.bytes_failed.saturating_add(bytes.max(counted));
			state.counts.files_failed += 1;
			match event {
				Some(failure) => state.core.push(ExtractEvent::FileFailed(failure)),
				None => state.core.mark_changed(),
			}
		});
	}

	/// A file that was running when the job stopped, `bytes` in size: not attempted, neither done
	/// nor failed.
	pub(crate) fn file_abandoned(&self, dest_uuid: Uuid, bytes: u64) {
		self.with_state(|state| {
			let counted = state.remove_active(dest_uuid);
			state.counts.bytes_done -= counted;
			state.counts.files_not_attempted += 1;
			state.counts.bytes_not_attempted += bytes;
			state.core.mark_changed();
		});
	}

	/// Directories planned that a job which ended early never started.
	pub(crate) fn dirs_not_attempted(&self, count: u64) {
		self.with_state(|state| {
			state.counts.dirs_not_attempted += count;
			state.core.mark_changed();
		});
	}

	/// A skipped entry; `event` is `None` once the report keeps no more records.
	pub(crate) fn skipped(&self, bytes: u64, event: Option<ExtractSkippedEntry>) {
		self.with_state(|state| {
			state.counts.entries_skipped += 1;
			state.counts.bytes_skipped = state.counts.bytes_skipped.saturating_add(bytes);
			match event {
				Some(entry) => state.core.push(ExtractEvent::Skipped(entry)),
				None => state.core.mark_changed(),
			}
		});
	}

	pub(crate) fn event(&self, event: ExtractEvent) {
		self.with_state(|state| state.core.push(event));
	}

	pub(crate) fn counts(&self) -> ItemCounts {
		self.read(|state| state.counts)
	}
}

/// Adds `record` to `list` unless it holds [`MAX_REPORT_RECORDS`] already, then only counting
/// it in `omitted`; whether it was kept.
pub(crate) fn keep<T>(list: &mut Vec<T>, omitted: &mut u64, record: T) -> bool {
	if list.len() < MAX_REPORT_RECORDS {
		list.push(record);
		true
	} else {
		*omitted += 1;
		false
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use super::ExtractState;

	impl ExtractState {
		/// The names of the files running now.
		pub(crate) fn active_names(&self) -> Vec<String> {
			self.active.iter().map(|file| file.name.clone()).collect()
		}
	}
}
