//! What compressing reports while it runs and when it ends, and the [`Reporter`] that turns job
//! state changes into throttled, ordered callbacks.

use std::{sync::Arc, time::Duration};

use filen_types::fs::Uuid;

use crate::{
	Error,
	fs::{
		archive::dispose::SourceDisposition,
		drive_job::{
			listing::ScanProgress,
			plan::{PlanTotals, RenamedEntry, SkippedEntry},
		},
		file::RemoteFile,
	},
	job::{
		self,
		report::{JobFailed, JobPhase, JobReport, JobState, Progress, RunCore, Snapshot, Units},
	},
	util::{MaybeArc, MaybeSendSync},
};

pub use crate::job::report::RunState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressPhase {
	/// Listing the sources.
	Scanning,
	/// Waiting for another archive job to finish; nothing is held meanwhile.
	WaitingForWorker,
	Compressing,
	/// Registering the archive in the destination.
	Finishing,
	/// Removing the sources, once the archive is verified.
	DisposingSources,
	Done,
	Cancelled,
	/// Ended early by an error that affects the whole job.
	Failed,
}

impl JobPhase for CompressPhase {
	fn is_terminal(self) -> bool {
		matches!(self, Self::Done | Self::Cancelled | Self::Failed)
	}
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompressCounts {
	/// Files written into the archive.
	pub files_done: u64,
	pub entries_skipped: u64,
	pub bytes_skipped: u64,
	/// Bytes of the sources read.
	pub bytes_read: u64,
	/// Bytes of the archive written (uploaded or uploading).
	pub bytes_written: u64,
	/// The archive's size once it is registered; 0 before.
	pub bytes_done: u64,
}

#[derive(Debug, Clone)]
pub enum CompressEvent {
	Skipped(SkippedEntry),
	Renamed(RenamedEntry),
	/// A source's data does not match the hash in its metadata. It is in the archive as it was
	/// read; the source itself may be damaged.
	SourceHashMismatch {
		source_uuid: Uuid,
		path: String,
	},
	/// What became of a source, when the sources were to be removed.
	SourceDisposition(SourceDisposition),
	/// The archive was registered but could not be added to one of the destination's public
	/// links or shares.
	PropagationFailed {
		dest_uuid: Uuid,
		/// Shared because [`Error`] is not `Clone` and events are.
		error: Arc<Error>,
	},
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
pub struct CompressUpdate {
	pub phase: CompressPhase,
	pub run_state: RunState,
	pub scan: ScanProgress,
	pub totals: PlanTotals,
	pub counts: CompressCounts,
	pub events: Vec<CompressEvent>,
	pub bytes_per_second: Option<u64>,
	pub eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub active_time: Duration,
}

/// The outcome of compressing. Nothing is visible in the drive unless the archive was
/// registered.
#[derive(Debug, Default)]
pub struct CompressReport {
	pub archive: Option<RemoteFile>,
	pub skipped: Vec<SkippedEntry>,
	pub renamed: Vec<RenamedEntry>,
	pub totals: PlanTotals,
	pub counts: CompressCounts,
	/// For a job refused up front for `max_bytes`: the archive's exact size.
	pub needed_bytes: Option<u64>,
	/// What became of each source, when the sources were to be removed.
	pub dispositions: Vec<SourceDisposition>,
}

impl JobReport for CompressReport {
	const NAME: &'static str = "compression";
}

/// A compression that ended early: cancelled, or stopped by an error.
pub type CompressFailed = JobFailed<CompressReport>;

/// Receives a compression's progress. All calls come from the one job, in order.
pub trait CompressCallback: MaybeSendSync + 'static {
	/// The archive, as soon as it is registered.
	fn on_archive_created(&self, archive: RemoteFile);
	fn on_update(&self, update: CompressUpdate);
}

impl<T: CompressCallback + ?Sized> CompressCallback for Arc<T> {
	fn on_archive_created(&self, archive: RemoteFile) {
		(**self).on_archive_created(archive);
	}

	fn on_update(&self, update: CompressUpdate) {
		(**self).on_update(update);
	}
}

pub(crate) struct CompressState {
	core: RunCore<CompressEvent, CompressPhase>,
	scan: ScanProgress,
	totals: PlanTotals,
	counts: CompressCounts,
}

impl JobState for CompressState {
	type Phase = CompressPhase;
	type Event = CompressEvent;
	type Callback = dyn CompressCallback;

	fn core(&mut self) -> &mut RunCore<CompressEvent, CompressPhase> {
		&mut self.core
	}

	fn progress(&self) -> Progress {
		Progress {
			bytes_done: self.counts.bytes_read,
			units: Units {
				done: self.counts.bytes_read,
				settled: self.counts.bytes_read,
				total: self.totals.bytes,
			},
		}
	}

	fn deliver(
		&mut self,
		callback: &dyn CompressCallback,
		snapshot: Snapshot<CompressEvent, CompressPhase>,
	) {
		callback.on_update(CompressUpdate {
			phase: snapshot.phase,
			run_state: snapshot.run_state,
			scan: self.scan,
			totals: self.totals,
			counts: self.counts,
			events: snapshot.events,
			bytes_per_second: snapshot.bytes_per_second,
			eta: snapshot.eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {}
}

pub(crate) type Reporter = job::report::Reporter<CompressState>;

impl Reporter {
	pub(crate) fn new(callback: impl CompressCallback) -> MaybeArc<Self> {
		Self::from_parts(
			CompressState {
				core: RunCore::new(CompressPhase::Scanning),
				scan: ScanProgress::default(),
				totals: PlanTotals::default(),
				counts: CompressCounts::default(),
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
				state.counts.entries_skipped += 1;
				state.counts.bytes_skipped += entry.bytes;
				state.core.push(CompressEvent::Skipped(entry.clone()));
			}
			for entry in renamed {
				state.core.push(CompressEvent::Renamed(entry.clone()));
			}
			state.core.mark_changed();
			state.core.mark_urgent();
		});
	}

	pub(crate) fn source_read(&self, bytes: u64) {
		self.with_state(|state| {
			state.counts.bytes_read += bytes;
			state.core.mark_changed();
		});
	}

	pub(crate) fn file_done(&self) {
		self.with_state(|state| {
			state.counts.files_done += 1;
			state.core.mark_changed();
		});
	}

	pub(crate) fn archive_written(&self, bytes: u64) {
		self.with_state(|state| {
			state.counts.bytes_written += bytes;
			state.core.mark_changed();
		});
	}

	/// Delivered at once, after an update carrying everything that happened before it.
	pub(crate) fn archive_created(&self, archive: RemoteFile, size: u64) {
		self.with_state(|state| state.counts.bytes_done = size);
		self.flush_then_call(|callback| callback.on_archive_created(archive));
	}

	pub(crate) fn event(&self, event: CompressEvent) {
		self.with_state(|state| state.core.push(event));
	}

	pub(crate) fn counts(&self) -> CompressCounts {
		self.read(|state| state.counts)
	}

	/// The last update of a job that ends before it starts, carrying the `totals` it would have
	/// compressed.
	pub(crate) fn finish_unstarted(&self, phase: CompressPhase, totals: PlanTotals) {
		self.finish_with(phase, |state| state.totals = totals);
	}
}
