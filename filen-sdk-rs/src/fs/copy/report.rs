//! What a copy reports while it runs and when it ends, and the [`Reporter`] that turns job
//! state changes into throttled, ordered callbacks.

use std::{
	sync::{
		Arc, Mutex,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use filen_macros::js_type;
use filen_types::fs::Uuid;

use crate::{
	Error,
	fs::{
		categories::{DirType, NonRootItemType, Normal},
		file::enums::RemoteFileType,
	},
	util::{MaybeArc, MaybeSendSync},
};

use super::{
	plan::{PlanTotals, RenameReason, RenamedEntry, SkipReason, SkippedEntry},
	progress::{ActiveClock, EventBatcher, RateEstimator, work_units},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"),
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

/// Whether a copy is running, and how far a pause or cancel has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"),
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RunState {
	Running,
	/// A pause was requested and in-flight work is still finishing.
	Pausing,
	/// Paused: nothing is running, and no memory or drive lock is held.
	Paused,
	/// Cancelled and winding down; a cancel overrides a pause.
	Cancelling,
}

/// Running counts. Once the job is over, everything planned is done, failed or not attempted:
/// `created + failed + not_attempted == totals` for directories, and likewise for files and
/// bytes. Skipped entries are not part of the totals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct CopyCounts {
	pub dirs_created: u64,
	/// Includes the directories below a failed one, which are never attempted.
	pub dirs_failed: u64,
	pub files_done: u64,
	/// Includes the files below a failed directory, which are never attempted.
	pub files_failed: u64,
	pub bytes_done: u64,
	pub bytes_failed: u64,
	/// What a job that ended early (cancelled, or stopped by an error) never copied, including
	/// files it had started: their partial uploads never become visible. Zero while running.
	pub dirs_not_attempted: u64,
	pub files_not_attempted: u64,
	pub bytes_not_attempted: u64,
	pub entries_skipped: u64,
	pub bytes_skipped: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct ScanProgress {
	pub sources_done: u64,
	pub sources_total: u64,
	/// Bytes of listing responses received so far, and the expected total when known.
	pub listing_bytes: u64,
	pub listing_total_bytes: Option<u64>,
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
	all(target_family = "wasm", target_os = "unknown", feature = "wasm-full"),
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
	pub error: Arc<Error>,
	/// Files and bytes not copied because of this failure (a directory's whole subtree).
	pub affected_files: u64,
	pub affected_bytes: u64,
}

/// The source of a failed item: a file can be copied again as is; a directory is addressed
/// through the handle the caller attached to it.
#[derive(Debug, Clone)]
pub enum FailedSource<D> {
	File(Box<RemoteFileType<'static>>),
	Dir(D),
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
	Skipped {
		source_path: String,
		bytes: u64,
		reason: SkipReason,
	},
	Renamed {
		source_uuid: Uuid,
		source_path: String,
		name: String,
		reason: RenameReason,
	},
	/// The item was created but could not be added to one of the destination's public links
	/// or shares.
	PropagationFailed {
		dest_uuid: Uuid,
		error: Arc<Error>,
	},
	/// A created directory's color could not be set.
	ColorFailed {
		dest_uuid: Uuid,
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
	pub counts: CopyCounts,
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
	pub counts: CopyCounts,
}

impl<D> Default for CopyReport<D> {
	fn default() -> Self {
		Self {
			top_level: Vec::new(),
			failures: Vec::new(),
			skipped: Vec::new(),
			renamed: Vec::new(),
			totals: PlanTotals::default(),
			counts: CopyCounts::default(),
		}
	}
}

/// Receives a copy's progress. All calls come from one [`Reporter`], in order.
pub trait CopyCallback: MaybeSendSync + 'static {
	fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>);
	fn top_level_created(&self, item: CopiedTopLevel);
	fn update(&self, update: CopyUpdate);
}

impl<T: CopyCallback + ?Sized> CopyCallback for std::sync::Arc<T> {
	fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		(**self).top_level_planned(items);
	}

	fn top_level_created(&self, item: CopiedTopLevel) {
		(**self).top_level_created(item);
	}

	fn update(&self, update: CopyUpdate) {
		(**self).update(update);
	}
}

struct State {
	phase: CopyPhase,
	pause_requested: bool,
	paused: bool,
	cancelling: bool,
	scan: ScanProgress,
	totals: PlanTotals,
	counts: CopyCounts,
	active: Vec<ActiveFile>,
	batcher: EventBatcher<CopyEvent>,
	changed: bool,
	clock: ActiveClock,
	rate: RateEstimator,
}

impl State {
	fn run_state(&self) -> RunState {
		if self.cancelling {
			RunState::Cancelling
		} else if self.paused {
			RunState::Paused
		} else if self.pause_requested {
			RunState::Pausing
		} else {
			RunState::Running
		}
	}
}

/// Turns job state changes into callbacks. Every callback is made while holding the state lock,
/// so the callback sees them in the order the job produced them.
pub(crate) struct Reporter {
	state: Mutex<State>,
	callback: Box<dyn CopyCallback>,
	start: Instant,
	ops_in_flight: AtomicU64,
}

/// Counts an operation (chunk transfer, create, finalize) as in flight until dropped; a pause is
/// only complete once none are.
pub(crate) struct OpGuard(MaybeArc<Reporter>);

impl Drop for OpGuard {
	fn drop(&mut self) {
		self.0.ops_in_flight.fetch_sub(1, Ordering::SeqCst);
		self.0.refresh_pause();
	}
}

impl Reporter {
	pub(crate) fn new(callback: impl CopyCallback) -> MaybeArc<Self> {
		let start = Instant::now();
		let mut clock = ActiveClock::default();
		clock.resume(Duration::ZERO);
		MaybeArc::new(Self {
			state: Mutex::new(State {
				phase: CopyPhase::Scanning,
				pause_requested: false,
				paused: false,
				cancelling: false,
				scan: ScanProgress::default(),
				totals: PlanTotals::default(),
				counts: CopyCounts::default(),
				active: Vec::new(),
				batcher: EventBatcher::default(),
				changed: true,
				clock,
				rate: RateEstimator::default(),
			}),
			callback: Box::new(callback),
			start,
			ops_in_flight: AtomicU64::new(0),
		})
	}

	fn now(&self) -> Duration {
		self.start.elapsed()
	}

	/// Applies `change` and sends an update when one is due.
	fn with_state(&self, change: impl FnOnce(&mut State)) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		change(&mut state);
		if state.batcher.is_due(now, state.changed) {
			self.flush(&mut state, now);
		}
	}

	fn flush(&self, state: &mut State, now: Duration) {
		let active_time = state.clock.active(now);
		let counts = state.counts;
		let done_units = work_units(
			counts.files_done + counts.files_failed,
			counts.bytes_done + counts.bytes_failed,
		);
		let settled_units = work_units(
			counts.files_done + counts.files_failed + counts.files_not_attempted,
			counts.bytes_done + counts.bytes_failed + counts.bytes_not_attempted,
		);
		let total_units = work_units(state.totals.files, state.totals.bytes);
		let finished = matches!(
			state.phase,
			CopyPhase::Done | CopyPhase::Cancelled | CopyPhase::Failed
		);
		state
			.rate
			.record(active_time, state.counts.bytes_done, done_units);
		let events = state.batcher.take(now);
		state.changed = false;
		self.callback.update(CopyUpdate {
			phase: state.phase,
			run_state: state.run_state(),
			scan: state.scan,
			totals: state.totals,
			counts: state.counts,
			active: state.active.clone(),
			events,
			bytes_per_second: state.rate.bytes_per_second(),
			// a job winding down copies nothing more, so there is no time left to estimate
			eta: if state.cancelling && !finished {
				None
			} else {
				state.rate.eta(total_units.saturating_sub(settled_units))
			},
			active_time,
		});
	}

	/// Sends an update when the throttle interval has passed.
	pub(crate) fn tick(&self) {
		self.with_state(|_| {});
	}

	/// Counts an operation as in flight; taken before the operation holds anything.
	pub(crate) fn op(self: &MaybeArc<Self>) -> OpGuard {
		self.ops_in_flight.fetch_add(1, Ordering::SeqCst);
		// a job reported paused stops being paused once anything starts
		self.refresh_pause();
		OpGuard(MaybeArc::clone(self))
	}

	#[cfg(test)]
	pub(crate) fn ops_in_flight(&self) -> u64 {
		self.ops_in_flight.load(Ordering::SeqCst)
	}

	/// Records whether a pause is requested; the job counts as paused once no operation is in
	/// flight.
	pub(crate) fn set_pause_requested(&self, requested: bool) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		if state.pause_requested != requested {
			state.pause_requested = requested;
			state.batcher.mark_urgent();
		}
		self.update_paused(&mut state, now);
		if state.batcher.is_due(now, state.changed) {
			self.flush(&mut state, now);
		}
	}

	fn refresh_pause(&self) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		self.update_paused(&mut state, now);
		if state.batcher.is_due(now, state.changed) {
			self.flush(&mut state, now);
		}
	}

	fn update_paused(&self, state: &mut State, now: Duration) {
		let paused = state.pause_requested
			&& !state.cancelling
			&& self.ops_in_flight.load(Ordering::SeqCst) == 0;
		if paused != state.paused {
			state.paused = paused;
			if paused {
				state.clock.pause(now);
			} else {
				state.clock.resume(now);
			}
			state.batcher.mark_urgent();
		}
	}

	#[cfg(test)]
	pub(crate) fn is_paused(&self) -> bool {
		self.state.lock().unwrap_or_else(|e| e.into_inner()).paused
	}

	pub(crate) fn set_phase(&self, phase: CopyPhase) {
		self.with_state(|state| {
			if state.phase != phase {
				state.phase = phase;
				state.batcher.mark_urgent();
			}
		});
	}

	/// A cancel overrides a pause: the job winds down instead of pausing.
	pub(crate) fn set_cancelling(&self) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		if !state.cancelling {
			state.cancelling = true;
			state.batcher.mark_urgent();
		}
		self.update_paused(&mut state, now);
		if state.batcher.is_due(now, state.changed) {
			self.flush(&mut state, now);
		}
	}

	pub(crate) fn set_scan(&self, scan: ScanProgress) {
		self.with_state(|state| {
			state.changed |= state.scan != scan;
			state.scan = scan;
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
				state.batcher.push(CopyEvent::Skipped {
					source_path: entry.source_path.clone(),
					bytes: entry.bytes,
					reason: entry.reason,
				});
			}
			for entry in renamed {
				state.batcher.push(CopyEvent::Renamed {
					source_uuid: entry.source_uuid,
					source_path: entry.source_path.clone(),
					name: entry.name.as_ref().to_owned(),
					reason: entry.reason,
				});
			}
			state.changed = true;
			state.batcher.mark_urgent();
		});
	}

	pub(crate) fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		let _state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		self.callback.top_level_planned(items);
	}

	/// Delivered at once, after an update carrying everything that happened before it.
	pub(crate) fn top_level_created(&self, item: CopiedTopLevel) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		self.flush(&mut state, now);
		self.callback.top_level_created(item);
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
			state.changed = true;
			state.batcher.push(CopyEvent::DirCreated {
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
			state.changed = true;
			state.batcher.push(CopyEvent::DirFailed(info));
		});
	}

	pub(crate) fn file_started(&self, file: ActiveFile) {
		self.with_state(|state| {
			state.active.push(file.clone());
			state.changed = true;
			state.batcher.push(CopyEvent::FileStarted(file));
		});
	}

	pub(crate) fn chunk_uploaded(&self, dest_uuid: Uuid, bytes: u64) {
		self.with_state(|state| {
			state.counts.bytes_done += bytes;
			if let Some(file) = state.active.iter_mut().find(|f| f.dest_uuid == dest_uuid) {
				file.bytes_done += bytes;
			}
			state.changed = true;
		});
	}

	fn remove_active(state: &mut State, dest_uuid: Uuid) -> u64 {
		match state.active.iter().position(|f| f.dest_uuid == dest_uuid) {
			Some(index) => state.active.remove(index).bytes_done,
			None => 0,
		}
	}

	pub(crate) fn file_done(&self, file: &ActiveFile) {
		self.with_state(|state| {
			let counted = Self::remove_active(state, file.dest_uuid);
			// a file whose chunks were counted as they uploaded ends with exactly its size
			state.counts.bytes_done = state.counts.bytes_done - counted + file.size;
			state.counts.files_done += 1;
			state.changed = true;
			state.batcher.push(CopyEvent::FileDone {
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
			let counted = Self::remove_active(state, dest_uuid);
			state.counts.bytes_done -= counted;
			state.counts.bytes_failed += info.affected_bytes;
			state.counts.files_failed += info.affected_files;
			state.changed = true;
			state.batcher.push(CopyEvent::FileFailed(info));
		});
	}

	/// A file that was running when the job stopped: it is neither done nor failed.
	pub(crate) fn file_abandoned(&self, dest_uuid: Uuid) {
		self.with_state(|state| {
			let counted = Self::remove_active(state, dest_uuid);
			state.counts.bytes_done -= counted;
			state.changed = true;
		});
	}

	pub(crate) fn event(&self, event: CopyEvent) {
		self.with_state(|state| {
			state.changed = true;
			state.batcher.push(event);
		});
	}

	pub(crate) fn counts(&self) -> CopyCounts {
		self.state.lock().unwrap_or_else(|e| e.into_inner()).counts
	}

	/// The last update of a job, sent at once.
	pub(crate) fn finish(&self, phase: CopyPhase) {
		let now = self.now();
		let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
		state.phase = phase;
		for file in std::mem::take(&mut state.active) {
			// normally already settled; a file still running now was never finished
			state.counts.bytes_done -= file.bytes_done;
		}
		let totals = state.totals;
		let counts = &mut state.counts;
		counts.dirs_not_attempted = totals
			.dirs
			.saturating_sub(counts.dirs_created + counts.dirs_failed);
		counts.files_not_attempted = totals
			.files
			.saturating_sub(counts.files_done + counts.files_failed);
		counts.bytes_not_attempted = totals
			.bytes
			.saturating_sub(counts.bytes_done + counts.bytes_failed);
		// a finished job is neither pausing nor paused, whatever was last requested
		state.pause_requested = false;
		state.paused = false;
		state.clock.pause(now);
		self.flush(&mut state, now);
	}
}
