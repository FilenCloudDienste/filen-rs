//! What a copy reports while it runs and when it ends, and the [`Reporter`] that turns job
//! state changes into throttled, ordered callbacks.

use std::{sync::Arc, time::Duration};

use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	Error,
	fs::{
		categories::{DirType, NonRootItemType, Normal},
		drive_job::counts::ItemCounts,
	},
	job::{
		self,
		progress::work_units,
		report::{JobFailed, JobPhase, JobReport, JobState, Progress, RunCore, Snapshot, Units},
	},
	util::{MaybeArc, MaybeSendSync},
};

pub(crate) use crate::job::report::OpGuard;
pub use crate::job::report::RunState;

use crate::fs::drive_job::{
	listing::{FailedSource, ScanProgress},
	plan::{PlanTotals, RenamedEntry, SkipReason, SkippedEntry},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum CopyPhase {
	/// Listing the sources and destinations.
	Scanning,
	CreatingDirectories,
	CopyingFiles,
	/// Checking whether the destinations became shared or linked during the copy.
	Finishing,
	Done,
	Cancelled,
	/// Ended early by an error that affects the whole job (e.g. no storage left).
	Failed,
}

/// A file being copied right now.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ActiveFile {
	pub source_uuid: Uuid,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
	pub size: u64,
	pub bytes_done: u64,
}

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
pub enum CopyStage {
	CreateDirectory,
	Download,
	Upload,
	Finalize,
	/// The file was registered, but the server made it a new version of an existing file with
	/// the same name instead of a new file (possible only if a client writing without the
	/// drive lock took the name at the last moment). The existing file keeps its previous
	/// content as a version; the copy itself does not exist as its own file.
	RegisteredAsVersion {
		/// The stable uuid of the file the copy became a version of.
		existing_file: Uuid,
	},
}

/// Why an item was not copied, with what is needed to show and retry it.
#[derive(Debug, Clone)]
pub struct FailureInfo {
	pub source_uuid: Uuid,
	pub source_path: String,
	/// The directory the item was to be created in, to retry it in with
	/// [`Client::copy_items_to`](crate::auth::Client::copy_items_to).
	pub dest_parent_dir: DirType<'static, Normal>,
	pub dest_name: String,
	pub stage: CopyStage,
	/// Shared because [`Error`] is not `Clone`, and one failure goes both into an event and
	/// into the report.
	pub error: Arc<Error>,
	/// Files and bytes not copied because of this failure (a directory's whole subtree).
	pub affected_files: u64,
	pub affected_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct CopyFailure<D> {
	pub source: FailedSource<D>,
	pub info: FailureInfo,
}

#[derive(Debug, Clone)]
pub enum CopyEvent {
	DirCreated {
		source_uuid: Uuid,
		dest_uuid: Uuid,
		dest_parent: Uuid,
		name: String,
	},
	DirFailed(FailureInfo),
	FileStarted(ActiveFile),
	FileDone {
		source_uuid: Uuid,
		dest_uuid: Uuid,
		dest_parent: Uuid,
		name: String,
		size: u64,
	},
	FileFailed(FailureInfo),
	Skipped(SkippedEntry),
	Renamed(RenamedEntry),
	/// The item was created but could not be added to one of the destination's public links
	/// or shares.
	PropagationFailed {
		dest_uuid: Uuid,
		/// Shared because [`Error`] is not `Clone` and events are.
		error: Arc<Error>,
	},
	/// A created directory's color could not be set.
	ColorFailed {
		dest_uuid: Uuid,
		/// Shared because [`Error`] is not `Clone` and events are.
		error: Arc<Error>,
	},
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
pub struct CopyUpdate {
	pub phase: CopyPhase,
	pub run_state: RunState,
	pub scan: ScanProgress,
	pub totals: PlanTotals,
	pub counts: ItemCounts,
	pub active: Vec<ActiveFile>,
	pub events: Vec<CopyEvent>,
	pub bytes_per_second: Option<u64>,
	pub eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub active_time: Duration,
}

/// A top-level item as planned, announced before anything is created so a caller can clean up
/// even after an abrupt end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTopLevelItem {
	pub request: usize,
	pub source_uuid: Uuid,
	pub dest_uuid: Uuid,
	pub dest_parent: Uuid,
	pub name: String,
	pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct CopiedTopLevel {
	pub request: usize,
	pub source_uuid: Uuid,
	pub item: NonRootItemType<'static, Normal>,
}

/// The outcome of a copy, whether it completed, was cancelled or failed.
#[derive(Debug)]
pub struct CopyReport<D> {
	/// Top-level items created, in creation order.
	pub top_level: Vec<CopiedTopLevel>,
	pub failures: Vec<CopyFailure<D>>,
	pub skipped: Vec<SkippedEntry>,
	/// Items created under a different name than their source's. A top-level item's planned
	/// keep-both name is not listed; a later one, because the planned name was taken after the
	/// destination was listed, is.
	pub renamed: Vec<RenamedEntry>,
	pub totals: PlanTotals,
	pub counts: ItemCounts,
}

impl<D> Default for CopyReport<D> {
	fn default() -> Self {
		Self {
			top_level: Vec::new(),
			failures: Vec::new(),
			skipped: Vec::new(),
			renamed: Vec::new(),
			totals: PlanTotals::default(),
			counts: ItemCounts::default(),
		}
	}
}

impl<D: std::fmt::Debug> JobReport for CopyReport<D> {
	const NAME: &'static str = "copy";
}

/// A copy that ended early: cancelled, or stopped by an error that affects the whole job.
pub type CopyFailed<D> = JobFailed<CopyReport<D>>;

/// Receives a copy's progress. All calls come from the one job, in order.
pub trait CopyCallback: MaybeSendSync + 'static {
	fn on_top_level_planned(&self, items: Vec<PlannedTopLevelItem>);
	fn on_top_level_created(&self, item: CopiedTopLevel);
	fn on_update(&self, update: CopyUpdate);
}

impl<T: CopyCallback + ?Sized> CopyCallback for Arc<T> {
	fn on_top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		(**self).on_top_level_planned(items);
	}

	fn on_top_level_created(&self, item: CopiedTopLevel) {
		(**self).on_top_level_created(item);
	}

	fn on_update(&self, update: CopyUpdate) {
		(**self).on_update(update);
	}
}

/// What a copy counts, next to the job-agnostic [`RunCore`].
pub(crate) struct CopyState {
	core: RunCore<CopyEvent, CopyPhase>,
	scan: ScanProgress,
	totals: PlanTotals,
	counts: ItemCounts,
	active: Vec<ActiveFile>,
}

impl JobPhase for CopyPhase {
	const DONE: Self = Self::Done;
	const CANCELLED: Self = Self::Cancelled;
	const FAILED: Self = Self::Failed;
}

impl JobState for CopyState {
	type Phase = CopyPhase;
	type Event = CopyEvent;
	type Callback = dyn CopyCallback;

	fn core(&mut self) -> &mut RunCore<CopyEvent, CopyPhase> {
		&mut self.core
	}

	fn progress(&self) -> Progress {
		let counts = self.counts;
		Progress {
			bytes_done: counts.bytes_done,
			units: Units {
				done: work_units(
					counts.files_done + counts.files_failed,
					counts.bytes_done + counts.bytes_failed,
				),
				settled: work_units(
					counts.files_done + counts.files_failed + counts.files_not_attempted,
					counts.bytes_done + counts.bytes_failed + counts.bytes_not_attempted,
				),
				total: work_units(self.totals.files, self.totals.bytes),
			},
		}
	}

	fn deliver(&mut self, callback: &dyn CopyCallback, snapshot: Snapshot<CopyEvent, CopyPhase>) {
		callback.on_update(CopyUpdate {
			phase: snapshot.phase,
			run_state: snapshot.run_state,
			scan: self.scan,
			totals: self.totals,
			counts: self.counts,
			active: self.active.clone(),
			events: snapshot.events,
			bytes_per_second: snapshot.bytes_per_second,
			eta: snapshot.eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {
		for file in std::mem::take(&mut self.active) {
			// normally already settled; a file still running now was never finished
			self.counts.bytes_done -= file.bytes_done;
		}
		let totals = self.totals;
		let counts = &mut self.counts;
		counts.dirs_not_attempted = totals
			.dirs
			.saturating_sub(counts.dirs_created + counts.dirs_failed);
		counts.files_not_attempted = totals
			.files
			.saturating_sub(counts.files_done + counts.files_failed);
		counts.bytes_not_attempted = totals
			.bytes
			.saturating_sub(counts.bytes_done + counts.bytes_failed);
	}
}

impl CopyState {
	fn remove_active(&mut self, dest_uuid: Uuid) -> u64 {
		match self.active.iter().position(|f| f.dest_uuid == dest_uuid) {
			Some(index) => self.active.remove(index).bytes_done,
			None => 0,
		}
	}
}

/// A copy's reporter: the job-agnostic [`job::report::Reporter`] over a [`CopyState`].
pub(crate) type Reporter = job::report::Reporter<CopyState>;

impl Reporter {
	pub(crate) fn new(callback: impl CopyCallback) -> MaybeArc<Self> {
		Self::from_parts(
			CopyState {
				core: RunCore::new(CopyPhase::Scanning),
				scan: ScanProgress::default(),
				totals: PlanTotals::default(),
				counts: ItemCounts::default(),
				active: Vec::new(),
			},
			Box::new(callback),
		)
	}

	pub(crate) fn set_scan(&self, scan: ScanProgress) {
		self.with_state(|state| {
			if state.scan != scan {
				state.scan = scan;
				state.core.mark_changed();
			}
		});
	}

	/// Takes the plan's totals and reports what it skipped or renamed.
	pub(crate) fn set_plan(
		&self,
		totals: PlanTotals,
		skipped: &[SkippedEntry],
		renamed: &[RenamedEntry],
	) {
		self.with_state(|state| {
			state.totals = totals;
			for entry in skipped {
				state.counts.entries_skipped += match entry.reason {
					SkipReason::UndecryptableFile { .. } => 1,
					SkipReason::Unreachable { count } => count,
				};
				state.counts.bytes_skipped += entry.bytes;
				// the report keeps the entry too
				state.core.push(CopyEvent::Skipped(entry.clone()));
			}
			for entry in renamed {
				state.core.push(CopyEvent::Renamed(entry.clone()));
			}
			state.core.mark_changed();
			state.core.mark_urgent();
		});
	}

	pub(crate) fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.call(|callback| callback.on_top_level_planned(items));
	}

	/// Delivered at once, after an update carrying everything that happened before it.
	pub(crate) fn top_level_created(&self, item: CopiedTopLevel) {
		self.flush_then_call(|callback| callback.on_top_level_created(item));
	}

	pub(crate) fn dir_created(
		&self,
		source_uuid: Uuid,
		dest_uuid: Uuid,
		dest_parent: Uuid,
		name: &str,
	) {
		self.with_state(|state| {
			state.counts.dirs_created += 1;
			state.core.push(CopyEvent::DirCreated {
				source_uuid,
				dest_uuid,
				dest_parent,
				name: name.to_owned(),
			});
		});
	}

	/// A failed directory; its whole subtree (`affected_*`, plus `descendant_dirs`) counts as
	/// failed too.
	pub(crate) fn dir_failed(&self, info: FailureInfo, descendant_dirs: u64) {
		self.with_state(|state| {
			state.counts.dirs_failed += 1 + descendant_dirs;
			state.counts.files_failed += info.affected_files;
			state.counts.bytes_failed += info.affected_bytes;
			state.core.push(CopyEvent::DirFailed(info));
		});
	}

	pub(crate) fn file_started(&self, file: ActiveFile) {
		self.with_state(|state| {
			state.active.push(file.clone());
			state.core.push(CopyEvent::FileStarted(file));
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

	pub(crate) fn file_done(&self, file: &ActiveFile) {
		self.with_state(|state| {
			let counted = state.remove_active(file.dest_uuid);
			// a file whose chunks were counted as they uploaded ends with exactly its size
			state.counts.bytes_done = state.counts.bytes_done - counted + file.size;
			state.counts.files_done += 1;
			state.core.push(CopyEvent::FileDone {
				source_uuid: file.source_uuid,
				dest_uuid: file.dest_uuid,
				dest_parent: file.dest_parent,
				name: file.name.clone(),
				size: file.size,
			});
		});
	}

	/// A failed file; the bytes it already uploaded move from done to failed.
	pub(crate) fn file_failed(&self, dest_uuid: Uuid, info: FailureInfo) {
		self.with_state(|state| {
			let counted = state.remove_active(dest_uuid);
			state.counts.bytes_done -= counted;
			state.counts.bytes_failed += info.affected_bytes;
			state.counts.files_failed += info.affected_files;
			state.core.push(CopyEvent::FileFailed(info));
		});
	}

	/// A file that was running when the job stopped: it is neither done nor failed.
	pub(crate) fn file_abandoned(&self, dest_uuid: Uuid) {
		self.with_state(|state| {
			let counted = state.remove_active(dest_uuid);
			state.counts.bytes_done -= counted;
			state.core.mark_changed();
		});
	}

	pub(crate) fn event(&self, event: CopyEvent) {
		self.with_state(|state| state.core.push(event));
	}

	pub(crate) fn counts(&self) -> ItemCounts {
		self.read(|state| state.counts)
	}

	/// The last update of a job that ends before it starts: it carries the `totals` the job
	/// would have copied, none of them attempted.
	pub(crate) fn finish_unstarted(&self, phase: CopyPhase, totals: PlanTotals) {
		self.finish_with(phase, |state| state.totals = totals);
	}
}
