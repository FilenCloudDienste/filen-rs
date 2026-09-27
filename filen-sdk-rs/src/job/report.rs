//! The job-agnostic half of a job's progress reporting: pause and cancel state, the active-time
//! clock, the rate estimator and event batching, and the [`Reporter`] that turns state changes
//! into throttled, ordered callbacks. What a job counts and what its updates carry stays with the
//! job, behind [`JobState`].

use std::{
	fmt,
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

use crate::{
	Error,
	job::{
		JobControl, Stopped,
		progress::{ActiveClock, EventBatcher, RateEstimator},
	},
	util::{MaybeArc, MaybeSend, MaybeSendSync},
};

/// Whether a job is running, and how far a pause or cancel has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
	feature = "wasm-full",
	derive(serde::Serialize, tsify::Tsify),
	tsify(into_wasm_abi, large_number_types_as_bigints),
	serde(rename_all = "camelCase")
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RunState {
	/// Running, and also a job an error stopped while it winds down: that one ignores a pause,
	/// as there is nothing left to pause, and reports `Cancelling` only once it is cancelled too.
	Running,
	/// A pause was requested and in-flight work is still finishing.
	Pausing,
	/// Paused: nothing is running, and no drive lock and no reservation from the client's memory
	/// budget is held. What a job keeps resident while paused, if anything, is stated in that
	/// job's docs.
	Paused,
	/// Cancelled and winding down; a cancel overrides a pause.
	Cancelling,
}

/// A job's phase. A terminal phase ends the job: no time is left to estimate once it is reached.
pub(crate) trait JobPhase: Copy + PartialEq + MaybeSend + 'static {
	/// The terminal phases: every job ends in one of these three.
	const DONE: Self;
	const CANCELLED: Self;
	const FAILED: Self;

	fn is_terminal(self) -> bool {
		self == Self::DONE || self == Self::CANCELLED || self == Self::FAILED
	}
}

/// How much of a job's work is done, in the units its rate and time left are estimated in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Units {
	/// Work finished, successfully or not.
	pub(crate) done: u64,
	/// Work that will not be done any more: finished, or given up on when the job ended early.
	pub(crate) settled: u64,
	pub(crate) total: u64,
}

/// What a job reports its progress as: the bytes its transfer rate is measured by, and its work
/// in [`Units`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Progress {
	pub(crate) bytes_done: u64,
	pub(crate) units: Units,
}

/// The job-agnostic part of every update, handed to [`JobState::deliver`] to build the job's own.
#[derive(Debug)]
pub(crate) struct Snapshot<E, P> {
	pub(crate) phase: P,
	pub(crate) run_state: RunState,
	pub(crate) events: Vec<E>,
	pub(crate) bytes_per_second: Option<u64>,
	pub(crate) eta: Option<Duration>,
	/// Time spent running, paused time left out.
	pub(crate) active_time: Duration,
}

/// The state every job has regardless of what it does. Whether a pause was asked, whether the
/// job is paused and whether it winds down are inputs; the reported [`RunState`] is derived from
/// them.
#[derive(Debug)]
pub(crate) struct RunCore<E, P> {
	phase: P,
	pause_requested: bool,
	paused: bool,
	winding_down: WindingDown,
	/// Whether anything (counters included) moved since the last update.
	changed: bool,
	clock: ActiveClock,
	rate: RateEstimator,
	batcher: EventBatcher<E>,
}

/// Whether a job winds down, and why; it only ever goes further down this list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum WindingDown {
	No,
	/// For an error that ended the job: nothing new starts, so it neither pauses nor has time
	/// left to estimate.
	ForError,
	/// For a cancel, which it is reported as.
	ForCancel,
}

impl<E, P: JobPhase> RunCore<E, P> {
	pub(crate) fn new(phase: P) -> Self {
		let mut clock = ActiveClock::default();
		clock.resume(Duration::ZERO);
		Self {
			phase,
			pause_requested: false,
			paused: false,
			winding_down: WindingDown::No,
			changed: true,
			clock,
			rate: RateEstimator::default(),
			batcher: EventBatcher::default(),
		}
	}

	pub(crate) fn run_state(&self) -> RunState {
		if self.winding_down == WindingDown::ForCancel {
			RunState::Cancelling
		} else if self.paused {
			RunState::Paused
		} else if self.pause_requested && self.winding_down == WindingDown::No {
			// a job an error ended runs on to its end whatever was asked
			RunState::Pausing
		} else {
			RunState::Running
		}
	}

	/// Marks the next update as carrying a change.
	pub(crate) fn mark_changed(&mut self) {
		self.changed = true;
	}

	/// Queues an event for the next update.
	pub(crate) fn push(&mut self, event: E) {
		self.changed = true;
		self.batcher.push(event);
	}

	/// Sends the next update without waiting for the interval.
	pub(crate) fn mark_urgent(&mut self) {
		self.batcher.mark_urgent();
	}

	fn is_due(&self, now: Duration) -> bool {
		self.batcher.is_due(now, self.changed)
	}

	fn snapshot(&mut self, now: Duration, progress: Progress) -> Snapshot<E, P> {
		let active_time = self.clock.active(now);
		self.rate
			.record(active_time, progress.bytes_done, progress.units.done);
		let events = self.batcher.take(now);
		self.changed = false;
		Snapshot {
			phase: self.phase,
			run_state: self.run_state(),
			events,
			bytes_per_second: self.rate.bytes_per_second(),
			// a job winding down does nothing more, so there is no time left to estimate
			eta: if self.winding_down != WindingDown::No && !self.phase.is_terminal() {
				None
			} else {
				self.rate
					.eta(progress.units.total.saturating_sub(progress.units.settled))
			},
			active_time,
		}
	}
}

/// A job's reportable state: the shared [`RunCore`] plus whatever the job counts.
pub(crate) trait JobState: MaybeSend + 'static {
	type Phase: JobPhase;
	type Event;
	type Callback: ?Sized + MaybeSendSync;

	fn core(&mut self) -> &mut RunCore<Self::Event, Self::Phase>;

	fn progress(&self) -> Progress;

	/// Builds the job's update from `snapshot` and its own state, and hands it to `callback`.
	fn deliver(&mut self, callback: &Self::Callback, snapshot: Snapshot<Self::Event, Self::Phase>);

	/// Settles the job's counts once it has ended, before its last update.
	fn settle(&mut self);
}

/// What in-flight operations report to, independent of the job: the handle the shared helpers
/// (lock waits, reservations, listings) hold, so they need not be generic over the job.
pub(crate) trait JobTick: MaybeSendSync {
	/// Settles whether the job counts as paused, and sends an update when one is due.
	fn tick(&self);
	fn ops_in_flight(&self) -> &AtomicU64;
	/// Records whether a pause is requested; the job counts as paused once no operation is in
	/// flight.
	fn set_pause_requested(&self, requested: bool);
	/// A cancel overrides a pause: the job winds down instead of pausing.
	fn set_cancelling(&self);
	/// The job winds down, cancelled through `control` or ended by an error: a stop overrides
	/// a pause (see [`Reporter::wind_down`]).
	fn wind_down(&self, control: &JobControl);
}

/// A handle to a job's reporter for code that does not know which job it works for.
#[derive(Clone)]
pub(crate) struct Ops(MaybeArc<dyn JobTick>);

impl fmt::Debug for Ops {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("Ops").finish_non_exhaustive()
	}
}

impl Ops {
	/// Counts an operation as in flight; taken before the operation holds anything.
	pub(crate) fn op(&self) -> OpGuard {
		self.0.ops_in_flight().fetch_add(1, Ordering::SeqCst);
		// a job reported paused stops being paused once anything starts
		self.0.tick();
		OpGuard(self.clone())
	}

	pub(crate) fn set_pause_requested(&self, requested: bool) {
		self.0.set_pause_requested(requested);
	}

	pub(crate) fn set_cancelling(&self) {
		self.0.set_cancelling();
	}
}

/// Counts an operation (chunk transfer, create, finalize) as in flight until dropped; a pause is
/// only complete once none are.
pub(crate) struct OpGuard(Ops);

impl Drop for OpGuard {
	fn drop(&mut self) {
		(self.0).0.ops_in_flight().fetch_sub(1, Ordering::SeqCst);
		(self.0).0.tick();
	}
}

/// Turns job state changes into callbacks. Every callback is made while holding the state lock,
/// so the callback sees them in the order the job produced them.
pub(crate) struct Reporter<S: JobState> {
	state: Mutex<S>,
	callback: Box<S::Callback>,
	start: Instant,
	ops_in_flight: AtomicU64,
}

impl<S: JobState> Reporter<S> {
	pub(crate) fn from_parts(state: S, callback: Box<S::Callback>) -> MaybeArc<Self> {
		MaybeArc::new(Self {
			state: Mutex::new(state),
			callback,
			start: Instant::now(),
			ops_in_flight: AtomicU64::new(0),
		})
	}

	fn now(&self) -> Duration {
		self.start.elapsed()
	}

	fn lock(&self) -> std::sync::MutexGuard<'_, S> {
		self.state.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// Applies `change`, settles whether the job counts as paused, and sends an update when one
	/// is due.
	pub(crate) fn with_state(&self, change: impl FnOnce(&mut S)) {
		let now = self.now();
		let mut state = self.lock();
		change(&mut state);
		self.update_paused(&mut state, now);
		if state.core().is_due(now) {
			self.flush(&mut state, now);
		}
	}

	/// Reads the job's state.
	pub(crate) fn read<T>(&self, read: impl FnOnce(&S) -> T) -> T {
		read(&self.lock())
	}

	/// Calls into the callback while holding the state lock, so the call stays in order with
	/// the updates.
	pub(crate) fn call(&self, call: impl FnOnce(&S::Callback)) {
		let _state = self.lock();
		call(&self.callback);
	}

	/// Sends an update carrying everything so far, then calls into the callback, both under one
	/// hold of the state lock.
	pub(crate) fn flush_then_call(&self, call: impl FnOnce(&S::Callback)) {
		let now = self.now();
		let mut state = self.lock();
		self.flush(&mut state, now);
		call(&self.callback);
	}

	fn flush(&self, state: &mut S, now: Duration) {
		let progress = state.progress();
		let snapshot = state.core().snapshot(now, progress);
		state.deliver(&self.callback, snapshot);
	}

	/// Settles whether the job counts as paused, and sends an update when one is due (the
	/// throttle interval has passed, or the pause state changed).
	pub(crate) fn tick(&self) {
		self.with_state(|_| {});
	}

	/// A handle for shared helpers that count in-flight operations.
	pub(crate) fn ops(self: &MaybeArc<Self>) -> Ops {
		Ops(MaybeArc::clone(self) as MaybeArc<dyn JobTick>)
	}

	/// Counts an operation as in flight; taken before the operation holds anything.
	pub(crate) fn op(self: &MaybeArc<Self>) -> OpGuard {
		self.ops().op()
	}

	#[cfg(test)]
	pub(crate) fn ops_in_flight(&self) -> u64 {
		self.ops_in_flight.load(Ordering::SeqCst)
	}

	/// Waits out a pause, reporting it; `Err` once the job is stopping.
	pub(crate) async fn checkpoint(&self, control: &JobControl) -> Result<(), Stopped> {
		self.set_pause_requested(control.is_pause_requested());
		let result = control.checkpoint().await;
		self.set_pause_requested(control.is_pause_requested());
		if result.is_err() {
			self.wind_down(control);
		}
		result
	}

	/// Records whether a pause is requested; the job counts as paused once no operation is in
	/// flight.
	pub(crate) fn set_pause_requested(&self, requested: bool) {
		self.with_state(|state| {
			let core = state.core();
			if core.pause_requested != requested {
				core.pause_requested = requested;
				core.mark_urgent();
			}
		});
	}

	fn update_paused(&self, state: &mut S, now: Duration) {
		let in_flight = self.ops_in_flight.load(Ordering::SeqCst);
		let core = state.core();
		let paused = core.pause_requested && core.winding_down == WindingDown::No && in_flight == 0;
		if paused != core.paused {
			core.paused = paused;
			if paused {
				core.clock.pause(now);
			} else {
				core.clock.resume(now);
			}
			core.mark_urgent();
		}
	}

	#[cfg(test)]
	pub(crate) fn is_paused(&self) -> bool {
		self.lock().core().paused
	}

	pub(crate) fn set_phase(&self, phase: S::Phase) {
		self.with_state(|state| {
			let core = state.core();
			if core.phase != phase {
				core.phase = phase;
				core.mark_urgent();
			}
		});
	}

	/// A cancel overrides a pause: the job winds down instead of pausing, reported cancelling.
	pub(crate) fn set_cancelling(&self) {
		self.stop(true);
	}

	/// The job winds down, and stops overriding a pause: reported cancelling when `control`
	/// cancelled it, and running on to its end when an error did.
	pub(crate) fn wind_down(&self, control: &JobControl) {
		self.stop(control.is_cancelled());
	}

	fn stop(&self, cancelled: bool) {
		let why = if cancelled {
			WindingDown::ForCancel
		} else {
			WindingDown::ForError
		};
		self.with_state(|state| {
			let core = state.core();
			if why > core.winding_down {
				core.winding_down = why;
				core.mark_urgent();
			}
		});
	}

	/// The last update of a job, sent at once.
	pub(crate) fn finish(&self, phase: S::Phase) {
		self.finish_with(phase, |_| {});
	}

	/// The last update of a job, sent at once after `prepare` (e.g. setting the totals of a job
	/// that ends before it starts).
	pub(crate) fn finish_with(&self, phase: S::Phase, prepare: impl FnOnce(&mut S)) {
		let now = self.now();
		let mut state = self.lock();
		prepare(&mut state);
		state.core().phase = phase;
		state.settle();
		let core = state.core();
		// a finished job is neither pausing nor paused, whatever was last requested
		core.pause_requested = false;
		core.paused = false;
		core.clock.pause(now);
		self.flush(&mut state, now);
	}
}

impl<S: JobState> JobTick for Reporter<S> {
	fn tick(&self) {
		Reporter::tick(self);
	}

	fn ops_in_flight(&self) -> &AtomicU64 {
		&self.ops_in_flight
	}

	fn set_pause_requested(&self, requested: bool) {
		Reporter::set_pause_requested(self, requested);
	}

	fn set_cancelling(&self) {
		Reporter::set_cancelling(self);
	}

	fn wind_down(&self, control: &JobControl) {
		Reporter::wind_down(self, control);
	}
}

/// A job's report, named in the message of the [`JobFailed`] that carries it.
pub trait JobReport: fmt::Debug {
	/// What the job is called in messages, e.g. "copy".
	const NAME: &'static str;
}

/// A job that ended early: cancelled, or stopped by an error that affects the whole job.
#[derive(Debug)]
pub struct JobFailed<R> {
	/// What the job did before it ended.
	pub report: R,
	/// [`ErrorKind::Cancelled`](crate::ErrorKind::Cancelled) after a cancel, or the error that
	/// ended the job. Shared with the failure it came from when one item's error ended the whole
	/// job, since [`Error`] is not `Clone`.
	pub error: Arc<Error>,
}

impl<R: JobReport> fmt::Display for JobFailed<R> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "the {} ended early: {}", R::NAME, self.error)
	}
}

impl<R: JobReport> std::error::Error for JobFailed<R> {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		Some(&*self.error)
	}
}

impl<R> From<JobFailed<R>> for Error {
	/// The error that ended the job: the original once nothing else holds it, or else an error of
	/// the same kind wrapping the shared one.
	fn from(failed: JobFailed<R>) -> Self {
		let JobFailed { report, error } = failed;
		// the report may hold the error too, in the failure it came from
		drop(report);
		Error::unshared(error)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::job::test_support::controls;

	#[derive(Debug, Clone, Copy, PartialEq, Eq)]
	enum Phase {
		Working,
		Done,
		Cancelled,
		Failed,
	}

	impl JobPhase for Phase {
		const DONE: Self = Self::Done;
		const CANCELLED: Self = Self::Cancelled;
		const FAILED: Self = Self::Failed;
	}

	/// A job of 10 units, of which `done` are done.
	struct State {
		core: RunCore<(), Phase>,
		done: u64,
	}

	/// The run state and whether time left was estimated, of each update.
	type Seen = Mutex<Vec<(RunState, bool)>>;

	impl JobState for State {
		type Phase = Phase;
		type Event = ();
		type Callback = Seen;

		fn core(&mut self) -> &mut RunCore<(), Phase> {
			&mut self.core
		}

		fn progress(&self) -> Progress {
			Progress {
				bytes_done: self.done,
				units: Units {
					done: self.done,
					settled: self.done,
					total: 10,
				},
			}
		}

		fn deliver(&mut self, seen: &Seen, snapshot: Snapshot<(), Phase>) {
			seen.lock()
				.unwrap()
				.push((snapshot.run_state, snapshot.eta.is_some()));
		}

		fn settle(&mut self) {}
	}

	#[test]
	fn a_job_an_error_stopped_runs_on_until_a_cancel() {
		let reporter = Reporter::from_parts(
			State {
				core: RunCore::new(Phase::Working),
				done: 0,
			},
			Box::new(Seen::default()),
		);
		// two updates of progress: time left can be told
		for done in 1..=2 {
			reporter.with_state(|state| {
				state.done = done;
				state.core().mark_urgent();
			});
		}
		let (_pause, cancel, control) = controls();
		reporter.set_pause_requested(true);
		// an error stops the paused job: it winds down, running, and a pause asked again changes
		// nothing, with no time left to estimate
		reporter.wind_down(&control);
		reporter.set_pause_requested(false);
		reporter.set_pause_requested(true);
		// until it is cancelled
		cancel.send_replace(true);
		reporter.wind_down(&control);
		assert_eq!(
			*reporter.callback.lock().unwrap(),
			[
				(RunState::Running, false),
				(RunState::Running, true),
				(RunState::Paused, true),
				(RunState::Running, false),
				(RunState::Running, false),
				(RunState::Running, false),
				(RunState::Cancelling, false),
			]
		);
	}
}
