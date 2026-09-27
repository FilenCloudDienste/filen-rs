//! What compressing reports while it runs and when it ends, and the [`Reporter`] that turns job
//! state changes into throttled, ordered callbacks.

use std::{sync::Arc, time::Duration};

use filen_macros::js_type;
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

/// Where a compression is. The last three are where it ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub enum CompressPhase {
	/// Listing the sources. A job paused before it starts waits here, having listed nothing.
	Scanning,
	/// Waiting for another archive job to finish; nothing is held meanwhile, and the destination
	/// is only checked once the wait is over.
	WaitingForWorker,
	/// Reading the sources into the archive and uploading it.
	Compressing,
	/// Registering the archive in the destination.
	Finishing,
	/// Reading the archive back, to check it holds the sources before they are deleted for
	/// good.
	Verifying,
	/// Removing the sources, once the archive is verified.
	DisposingSources,
	/// Ran to its end.
	Done,
	/// Ended early by a cancel.
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
#[js_type(export, no_deser, no_default)]
pub struct CompressCounts {
	/// Files written into the archive.
	pub files_done: u64,
	pub entries_skipped: u64,
	/// Bytes of the files left out.
	pub bytes_skipped: u64,
	/// Bytes of the sources read.
	pub bytes_read: u64,
	/// Bytes of the archive uploaded so far.
	pub bytes_written: u64,
	/// The archive's size once it is registered; 0 before.
	pub bytes_done: u64,
	/// Bytes of the archive read back to check it before the sources are deleted for good (see
	/// [`CompressPhase::Verifying`]); up to `bytes_done`, and 0 when nothing is read back.
	pub bytes_verified: u64,
}

/// The source file being read into the archive right now, shaped like the copy and extract
/// jobs' active files.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default)]
pub struct CompressActiveFile {
	/// The source file's uuid.
	pub source_uuid: Uuid,
	/// Its name in the archive.
	pub name: String,
	/// Its whole path in the archive.
	pub path: String,
	/// Its size in bytes.
	pub size: u64,
	/// Bytes of it read so far.
	pub bytes_done: u64,
}

/// Something that happened to one item, reported in the next update.
#[derive(Debug, Clone)]
pub enum CompressEvent {
	/// An item was left out of the archive.
	Skipped(SkippedEntry),
	/// An item got another name in the archive.
	Renamed(RenamedEntry),
	/// A source's data does not match the hash in its metadata. It is in the archive as it was
	/// read; the source itself may be damaged.
	SourceHashMismatch { source_uuid: Uuid, path: String },
	/// What became of a source, when the sources were to be removed.
	SourceDisposition(SourceDisposition),
	/// The archive was registered but could not be added to one of the destination's public
	/// links or shares.
	PropagationFailed {
		/// The archive's uuid.
		dest_uuid: Uuid,
		/// Shared because [`Error`] is not `Clone` and events are.
		error: Arc<Error>,
	},
}

/// One progress callback: the complete current state plus the events since the last one.
#[derive(Debug, Clone)]
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
	/// The source being read, as the copy and extract updates list theirs. An archive is
	/// written one file at a time, in its order, so this holds at most one: none between files,
	/// or for an empty one.
	pub active: Vec<CompressActiveFile>,
	/// What happened since the last update, in order.
	pub events: Vec<CompressEvent>,
	/// Bytes of the sources read (and, in `Verifying`, of the archive read back) per second,
	/// over the last 10 seconds of running time; `None` until there is a rate.
	pub bytes_per_second: Option<u64>,
	/// The time left at that rate; `None` while there is no rate or the job winds down, zero
	/// once it ended.
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
	/// The items that got another name in the archive.
	pub renamed: Vec<RenamedEntry>,
	/// What the plan held; zero when the job ended before its sources were listed.
	pub totals: PlanTotals,
	/// What was done.
	pub counts: CompressCounts,
	/// For a job refused up front for `max_bytes`: the archive's exact size.
	pub needed_bytes: Option<u64>,
	/// What became of each source, when the sources were to be removed.
	pub dispositions: Vec<SourceDisposition>,
	/// Source files whose data did not match the hash in their metadata: they went into the
	/// archive as they were read (up to 1000 are listed).
	pub hash_mismatches: Vec<HashMismatch>,
}

/// A source file whose data did not match the hash in its metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashMismatch {
	/// The source file's uuid.
	pub source_uuid: Uuid,
	/// Its path in the archive.
	pub path: String,
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
	/// The job's progress, throttled; the last one comes once the job ended.
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
	active: Option<CompressActiveFile>,
	/// Bytes of the archive to read back, once reading it back started.
	verify_total: u64,
	/// The job ended: whatever it did not read, it never will.
	ended: bool,
}

impl JobState for CompressState {
	type Phase = CompressPhase;
	type Event = CompressEvent;
	type Callback = dyn CompressCallback;

	fn core(&mut self) -> &mut RunCore<CompressEvent, CompressPhase> {
		&mut self.core
	}

	fn progress(&self) -> Progress {
		// the sources read, then the archive read back if it is
		let done = self.counts.bytes_read + self.counts.bytes_verified;
		let total = self.totals.bytes + self.verify_total;
		Progress {
			bytes_done: done,
			units: Units {
				done,
				settled: if self.ended { total } else { done },
				total,
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
			active: self.active.iter().cloned().collect(),
			events: snapshot.events,
			bytes_per_second: snapshot.bytes_per_second,
			eta: snapshot.eta,
			active_time: snapshot.active_time,
		});
	}

	fn settle(&mut self) {
		self.ended = true;
		self.active = None;
	}
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
				active: None,
				verify_total: 0,
				ended: false,
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

	/// `bytes` more of the source `file` were read; `file` builds it when it starts.
	pub(crate) fn source_read(
		&self,
		source_uuid: Uuid,
		bytes: u64,
		file: impl FnOnce() -> CompressActiveFile,
	) {
		self.with_state(|state| {
			state.counts.bytes_read += bytes;
			match &mut state.active {
				Some(active) if active.source_uuid == source_uuid => active.bytes_done += bytes,
				active => {
					*active = Some(CompressActiveFile {
						bytes_done: bytes,
						..file()
					});
				}
			}
			state.core.mark_changed();
		});
	}

	pub(crate) fn file_done(&self) {
		self.with_state(|state| {
			state.counts.files_done += 1;
			state.active = None;
			state.core.mark_changed();
		});
	}

	/// Reading the `len` bytes of the archive back started.
	pub(crate) fn verifying(&self, len: u64) {
		self.with_state(|state| {
			state.verify_total = len;
			state.core.mark_changed();
		});
	}

	pub(crate) fn archive_verified(&self, bytes: u64) {
		self.with_state(|state| {
			state.counts.bytes_verified += bytes;
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

	/// Tells of what became of sources in an update sent at once: a job dropped before its last
	/// update (the bindings drop one that outlives its cancel grace) has still told of every
	/// source it removed.
	pub(crate) fn dispositions(&self, dispositions: &[SourceDisposition]) {
		self.with_state(|state| {
			for disposition in dispositions {
				state
					.core
					.push(CompressEvent::SourceDisposition(disposition.clone()));
			}
			state.core.mark_urgent();
		});
	}

	pub(crate) fn counts(&self) -> CompressCounts {
		self.read(|state| state.counts)
	}

	/// The last update of a job that ends early, carrying the `totals` it would have compressed
	/// (a job refused before it planned was never told them).
	pub(crate) fn finish_early(&self, phase: CompressPhase, totals: PlanTotals) {
		self.finish_with(phase, |state| state.totals = totals);
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Mutex;

	use super::*;

	#[derive(Default)]
	struct Updates(Mutex<Vec<CompressUpdate>>);

	impl CompressCallback for Updates {
		fn on_archive_created(&self, _: RemoteFile) {}

		fn on_update(&self, update: CompressUpdate) {
			self.0.lock().unwrap().push(update);
		}
	}

	fn starting(source_uuid: Uuid, path: &str) -> CompressActiveFile {
		CompressActiveFile {
			source_uuid,
			name: path.to_owned(),
			path: path.to_owned(),
			size: 5,
			bytes_done: 0,
		}
	}

	#[test]
	fn the_source_being_read_is_active_until_it_is_in() {
		let reporter = Reporter::new(Updates::default());
		let [a, b] = [Uuid::from_u128(1), Uuid::from_u128(2)];
		let active = || reporter.read(|state| state.active.clone());
		reporter.source_read(a, 2, || starting(a, "a"));
		reporter.source_read(a, 3, || starting(a, "a"));
		assert_eq!(
			active(),
			Some(CompressActiveFile {
				bytes_done: 5,
				..starting(a, "a")
			})
		);
		reporter.file_done();
		assert_eq!(active(), None);
		reporter.source_read(b, 1, || starting(b, "b"));
		assert_eq!(
			active(),
			Some(CompressActiveFile {
				bytes_done: 1,
				..starting(b, "b")
			})
		);
		reporter.finish(CompressPhase::Cancelled);
		assert_eq!(active(), None, "an ended job reads nothing");
		assert_eq!(reporter.counts().bytes_read, 6);
	}

	#[test]
	fn a_job_that_ended_has_no_time_left() {
		for phase in [
			CompressPhase::Done,
			CompressPhase::Cancelled,
			CompressPhase::Failed,
		] {
			let updates = Arc::new(Updates::default());
			let reporter = Reporter::new(Arc::clone(&updates));
			let totals = PlanTotals {
				dirs: 0,
				files: 2,
				bytes: 100,
			};
			reporter.set_plan(totals, &[], &[]);
			let source = Uuid::from_u128(1);
			reporter.source_read(source, 30, || starting(source, "a"));
			reporter.finish(phase);
			let last = updates.0.lock().unwrap().last().cloned().unwrap();
			assert_eq!(last.phase, phase);
			assert_eq!(last.eta, Some(Duration::ZERO), "{phase:?}");
		}
	}
}
