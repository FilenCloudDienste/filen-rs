//! Cooperative control of a long-running job: pause, cancel, an internal stop, and background
//! tasks that cannot outlive the job.
//!
//! Pause and cancel are observed by the job's own work instead of by stopping to poll it, so a
//! job can finish (or drop) its in-flight chunks and release their memory reservations before
//! it parks.

use std::{
	future::Future,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::watch;

use crate::{
	runtime::{SpawnTaskHandle, spawn_task_maybe_send},
	util::MaybeSend,
};

/// Returned when a job was cancelled or stopped while waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stopped;

#[derive(Debug)]
struct Inner {
	/// External pause request; `None` when the caller cannot pause.
	pause: Option<watch::Receiver<bool>>,
	/// External cancel request; `None` when the caller cannot cancel.
	cancel: Option<watch::Receiver<bool>>,
	/// Internal stop, for failures that end the whole job (e.g. no storage left).
	stop: watch::Sender<bool>,
	/// Set once a cancel has been seen: a cancel cannot be taken back.
	cancelled: AtomicBool,
}

/// Shared by all of a job's work. A dropped pause sender counts as "not paused" and a dropped
/// cancel sender as "not cancelled": losing a controller must never pause or cancel the job.
/// Once seen, a cancel stays in effect even if the sender goes back to `false`.
#[derive(Debug, Clone)]
pub struct JobControl {
	inner: Arc<Inner>,
}

impl Default for JobControl {
	/// A job that cannot be paused or cancelled.
	fn default() -> Self {
		Self::from_receivers(None, None)
	}
}

/// Pauses, resumes and cancels the job of the [`JobControl`] it came with. Once every clone is
/// dropped the job runs unpaused, and can no longer be paused or cancelled.
#[derive(Debug, Clone)]
pub struct JobController {
	pause: watch::Sender<bool>,
	cancel: watch::Sender<bool>,
}

impl JobController {
	/// The job finishes what is in flight and then waits, holding nothing, until resumed.
	pub fn pause(&self) {
		self.pause.send_replace(true);
	}

	pub fn resume(&self) {
		self.pause.send_replace(false);
	}

	/// The job winds down and reports what it did. A cancel cannot be taken back.
	pub fn cancel(&self) {
		self.cancel.send_replace(true);
	}
}

impl JobControl {
	/// A job that the returned [`JobController`] pauses, resumes and cancels.
	pub fn new() -> (Self, JobController) {
		let (pause, pause_rx) = watch::channel(false);
		let (cancel, cancel_rx) = watch::channel(false);
		(
			Self::from_receivers(Some(pause_rx), Some(cancel_rx)),
			JobController { pause, cancel },
		)
	}

	/// A job paused while `pause` holds `true` and cancelled once `cancel` does. Either may be
	/// `None` when the caller cannot pause or cancel.
	pub(crate) fn from_receivers(
		pause: Option<watch::Receiver<bool>>,
		cancel: Option<watch::Receiver<bool>>,
	) -> Self {
		let (stop, _) = watch::channel(false);
		Self {
			inner: Arc::new(Inner {
				pause,
				cancel,
				stop,
				cancelled: AtomicBool::new(false),
			}),
		}
	}

	pub(crate) fn is_cancelled(&self) -> bool {
		if self.inner.cancelled.load(Ordering::SeqCst) {
			return true;
		}
		let cancelled = self.inner.cancel.as_ref().is_some_and(|c| *c.borrow());
		if cancelled {
			self.inner.cancelled.store(true, Ordering::SeqCst);
		}
		cancelled
	}

	/// Ends the job from within, e.g. when the account runs out of storage.
	pub(crate) fn stop(&self) {
		self.inner.stop.send_replace(true);
	}

	pub(crate) fn is_stopping(&self) -> bool {
		self.is_cancelled() || *self.inner.stop.borrow()
	}

	pub(crate) fn is_pause_requested(&self) -> bool {
		// the last value outlives a dropped sender, which must count as not paused
		self.inner
			.pause
			.as_ref()
			.is_some_and(|p| p.has_changed().is_ok() && *p.borrow())
	}

	/// Resolves once the job is cancelled or stopped; never, if neither can happen.
	pub(crate) async fn stopping(&self) {
		if self.is_stopping() {
			return;
		}
		let mut stop = self.inner.stop.subscribe();
		let stopped = async move {
			// the sender lives in `inner`, which outlives this future
			let _ = stop.wait_for(|stopped| *stopped).await;
		};
		match self.inner.cancel.clone() {
			Some(mut cancel) => {
				let latch = &self.inner.cancelled;
				let cancelled = async move {
					if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
						std::future::pending::<()>().await;
					}
					latch.store(true, Ordering::SeqCst);
				};
				tokio::select! {
					() = cancelled => {},
					() = stopped => {},
				}
			}
			None => stopped.await,
		}
	}

	/// Resolves once the pause request is no longer `paused`; never, if it cannot change.
	pub(crate) async fn pause_changed(&self, paused: bool) {
		match self.inner.pause.clone() {
			Some(mut pause) => {
				let _ = pause.wait_for(|current| *current != paused).await;
				// a dropped sender leaves the job unpaused for good, whatever its last value
				if pause.has_changed().is_err() && !paused {
					std::future::pending::<()>().await;
				}
			}
			None => std::future::pending::<()>().await,
		}
	}

	/// Resolves once no pause is requested; never, while paused forever.
	async fn resumed(&self) {
		if let Some(mut pause) = self.inner.pause.clone() {
			// a dropped sender can no longer pause: treat it as resumed
			let _ = pause.wait_for(|paused| !*paused).await;
		}
	}

	/// The checkpoint before starting new work: `Err` when the job is stopping, otherwise waits
	/// out a pause (returning `Err` if the job stops meanwhile).
	pub(crate) async fn checkpoint(&self) -> Result<(), Stopped> {
		if self.is_stopping() {
			return Err(Stopped);
		}
		if !self.is_pause_requested() {
			return Ok(());
		}
		tokio::select! {
			biased;
			() = self.stopping() => Err(Stopped),
			() = self.resumed() => if self.is_stopping() { Err(Stopped) } else { Ok(()) },
		}
	}

	/// Runs `fut` unless the job stops first, in which case `fut` is dropped.
	pub(crate) async fn until_stopping<F: Future>(&self, fut: F) -> Result<F::Output, Stopped> {
		tokio::select! {
			biased;
			() = self.stopping() => Err(Stopped),
			out = fut => Ok(out),
		}
	}
}

/// Adapts the bindings' abort signal to a job's cooperative cancel; only builds with bindings
/// (and the tests) use it.
#[cfg(any(feature = "uniffi", feature = "wasm-full", test))]
pub(crate) mod cancel_grace {
	use std::{
		future::Future,
		pin::Pin,
		task::{Context, Poll},
		time::Duration,
	};

	use pin_project_lite::pin_project;
	use tokio::sync::watch;

	use crate::{Error, ErrorKind, util::sleep};

	/// How long a cancelled job may take to finish what cannot be interrupted (a directory create or
	/// file registration already sent) and report it, before it is dropped as is.
	#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
	pub(crate) const CANCEL_GRACE: Duration = Duration::from_secs(5);

	/// Runs `job`, ending it with [`ErrorKind::Cancelled`] if it has not finished `grace` after
	/// `cancel` turned `true`. The job sees the same cancel through its
	/// [`JobControl`](super::JobControl) and normally ends on its own well within the grace period.
	pub(crate) async fn with_cancel_grace<T>(
		job: impl Future<Output = Result<T, Error>>,
		mut cancel: watch::Receiver<bool>,
		grace: Duration,
	) -> Result<T, Error> {
		let deadline = async move {
			if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
				std::future::pending::<()>().await;
			}
			sleep(grace).await;
		};
		tokio::select! {
			biased;
			result = job => result,
			() = deadline => Err(Error::custom(
				ErrorKind::Cancelled,
				"cancelled job did not stop within its grace period",
			)),
		}
	}

	pin_project! {
		/// Resolves with `job`, turning `abort` resolving into a cooperative cancel: it sets `cancel`
		/// and keeps waiting for the job, which then winds down (see [`with_cancel_grace`]).
		pub(crate) struct CancelOnAbort<J, A> {
			#[pin]
			job: J,
			#[pin]
			abort: A,
			cancel: watch::Sender<bool>,
			aborted: bool,
		}
	}

	impl<J, A> CancelOnAbort<J, A> {
		pub(crate) fn new(job: J, abort: A, cancel: watch::Sender<bool>) -> Self {
			Self {
				job,
				abort,
				cancel,
				aborted: false,
			}
		}
	}

	impl<J: Future, A: Future> Future for CancelOnAbort<J, A> {
		type Output = J::Output;

		fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
			let this = self.project();
			// an abort future must not be polled again once it has resolved
			if !*this.aborted && this.abort.poll(cx).is_ready() {
				*this.aborted = true;
				this.cancel.send_replace(true);
			}
			this.job.poll(cx)
		}
	}

	#[cfg(test)]
	mod tests {
		use std::sync::{
			Arc,
			atomic::{AtomicBool, Ordering},
		};

		use super::*;
		use crate::job::{JobControl, test_support::SetOnDrop};

		#[tokio::test(start_paused = true)]
		async fn a_cancelled_job_that_finishes_in_time_keeps_its_result() {
			let (cancel, cancel_rx) = watch::channel(false);
			let job = async {
				tokio::time::sleep(Duration::from_secs(2)).await;
				Ok::<_, Error>("finished")
			};
			cancel.send_replace(true);
			let result = with_cancel_grace(job, cancel_rx, Duration::from_secs(5)).await;
			assert_eq!(result.unwrap(), "finished");
		}

		#[tokio::test(start_paused = true)]
		async fn a_cancelled_job_is_dropped_after_its_grace_period() {
			let (cancel, cancel_rx) = watch::channel(false);
			let dropped = Arc::new(AtomicBool::new(false));
			let marker = SetOnDrop(dropped.clone());
			let job = async move {
				let _marker = marker;
				std::future::pending::<Result<(), Error>>().await
			};
			let run = tokio::spawn(with_cancel_grace(job, cancel_rx, Duration::from_secs(5)));
			tokio::time::sleep(Duration::from_secs(60)).await;
			assert!(!run.is_finished(), "no deadline without a cancel");
			cancel.send_replace(true);
			tokio::time::sleep(Duration::from_secs(4)).await;
			assert!(!run.is_finished(), "the job gets its grace period");
			let result = run.await.unwrap();
			assert_eq!(result.unwrap_err().kind(), ErrorKind::Cancelled);
			assert!(dropped.load(Ordering::SeqCst));
		}

		#[tokio::test(start_paused = true)]
		async fn an_abort_becomes_a_cancel_the_job_sees() {
			let (cancel, cancel_rx) = watch::channel(false);
			let control = JobControl::from_receivers(None, Some(cancel_rx));
			let (abort, abort_rx) = tokio::sync::oneshot::channel::<()>();
			let job = {
				let control = control.clone();
				async move {
					control.stopping().await;
					Ok::<_, Error>("wound down")
				}
			};
			let run = tokio::spawn(CancelOnAbort::new(job, abort_rx, cancel));
			tokio::time::sleep(Duration::from_secs(1)).await;
			assert!(!run.is_finished());
			abort.send(()).unwrap();
			assert_eq!(run.await.unwrap().unwrap(), "wound down");
			assert!(control.is_cancelled());
		}
	}
}

/// Background tasks owned by a job. Each runs on the SDK runtime (`spawn_task_maybe_send`:
/// a tokio task natively, a `spawn_local` task on the current worker on wasm), and is stopped
/// when this set is dropped, so no task outlives the job.
pub(crate) struct JobTasks<T> {
	kill: watch::Sender<bool>,
	tasks: FuturesUnordered<SpawnTaskHandle<Option<T>>>,
}

impl<T: MaybeSend + 'static> JobTasks<T> {
	pub(crate) fn new() -> Self {
		let (kill, _) = watch::channel(false);
		Self {
			kill,
			tasks: FuturesUnordered::new(),
		}
	}

	pub(crate) fn spawn(&self, fut: impl Future<Output = T> + MaybeSend + 'static) {
		let mut kill = self.kill.subscribe();
		self.tasks.push(spawn_task_maybe_send(async move {
			tokio::select! {
				biased;
				// Also resolves (with an error) once the set is dropped with the sender.
				_ = kill.wait_for(|killed| *killed) => None,
				out = fut => Some(out),
			}
		}));
	}

	pub(crate) fn len(&self) -> usize {
		self.tasks.len()
	}

	pub(crate) fn is_empty(&self) -> bool {
		self.tasks.is_empty()
	}

	/// The next finished task's output; `None` once no task is left. Aborted tasks are skipped.
	pub(crate) async fn next(&mut self) -> Option<T> {
		while let Some(out) = self.tasks.next().await {
			if let Some(out) = out {
				return Some(out);
			}
		}
		None
	}
}

impl<T> Drop for JobTasks<T> {
	fn drop(&mut self) {
		self.kill.send_replace(true);
	}
}

#[cfg(test)]
pub(crate) mod test_support {
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};

	use tokio::sync::watch;

	use super::JobControl;

	/// A job control with its pause and cancel senders, so a test can drop one without the other
	/// (a [`JobController`](super::JobController) only drops both).
	pub(crate) fn controls() -> (watch::Sender<bool>, watch::Sender<bool>, JobControl) {
		let (pause, pause_rx) = watch::channel(false);
		let (cancel, cancel_rx) = watch::channel(false);
		(
			pause,
			cancel,
			JobControl::from_receivers(Some(pause_rx), Some(cancel_rx)),
		)
	}

	/// Sets its flag when dropped, which a future is when the task running it is aborted.
	pub(crate) struct SetOnDrop(pub(crate) Arc<AtomicBool>);

	impl Drop for SetOnDrop {
		fn drop(&mut self) {
			self.0.store(true, Ordering::SeqCst);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::{
		test_support::{SetOnDrop, controls},
		*,
	};

	#[tokio::test]
	async fn a_controller_pauses_resumes_and_cancels_its_job() {
		let (control, controller) = JobControl::new();
		assert!(!control.is_pause_requested());
		controller.pause();
		assert!(control.is_pause_requested());
		controller.resume();
		assert!(!control.is_pause_requested());
		assert_eq!(control.checkpoint().await, Ok(()));
		controller.cancel();
		assert!(control.is_cancelled());
		assert_eq!(control.checkpoint().await, Err(Stopped));
	}

	#[tokio::test]
	async fn a_dropped_controller_leaves_its_job_running() {
		let (control, controller) = JobControl::new();
		controller.pause();
		drop(controller);
		assert!(!control.is_pause_requested());
		assert!(!control.is_stopping());
		assert_eq!(control.checkpoint().await, Ok(()));
	}

	#[tokio::test]
	async fn checkpoint_passes_while_running_and_fails_once_stopping() {
		let (_pause, cancel, control) = controls();
		assert_eq!(control.checkpoint().await, Ok(()));
		cancel.send_replace(true);
		assert!(control.is_cancelled());
		assert_eq!(control.checkpoint().await, Err(Stopped));

		let internal = JobControl::default();
		internal.stop();
		assert!(internal.is_stopping());
		assert!(!internal.is_cancelled());
		assert_eq!(internal.checkpoint().await, Err(Stopped));
	}

	#[tokio::test(start_paused = true)]
	async fn checkpoint_waits_out_a_pause() {
		let (pause, _cancel, control) = controls();
		pause.send_replace(true);
		let waiter = tokio::spawn({
			let control = control.clone();
			async move { control.checkpoint().await }
		});
		tokio::time::sleep(Duration::from_secs(60)).await;
		assert!(!waiter.is_finished(), "a paused checkpoint must wait");
		pause.send_replace(false);
		assert_eq!(waiter.await.unwrap(), Ok(()));
	}

	#[tokio::test(start_paused = true)]
	async fn cancel_ends_a_paused_checkpoint() {
		let (pause, cancel, control) = controls();
		pause.send_replace(true);
		let waiter = tokio::spawn({
			let control = control.clone();
			async move { control.checkpoint().await }
		});
		tokio::time::sleep(Duration::from_secs(1)).await;
		cancel.send_replace(true);
		assert_eq!(waiter.await.unwrap(), Err(Stopped));
	}

	#[tokio::test(start_paused = true)]
	async fn pause_changes_are_observed() {
		let (pause, _cancel, control) = controls();
		let changed = tokio::spawn({
			let control = control.clone();
			async move { control.pause_changed(false).await }
		});
		tokio::time::sleep(Duration::from_secs(1)).await;
		assert!(!changed.is_finished());
		pause.send_replace(true);
		changed.await.unwrap();

		let resumed = tokio::spawn({
			let control = control.clone();
			async move { control.pause_changed(true).await }
		});
		drop(pause);
		resumed.await.unwrap();
	}

	#[tokio::test]
	async fn a_pause_sender_dropped_while_paused_resumes_the_job() {
		let (pause, _cancel, control) = controls();
		pause.send_replace(true);
		assert!(control.is_pause_requested());
		drop(pause);
		assert!(
			!control.is_pause_requested(),
			"a dropped pause sender is not a pause"
		);
		assert_eq!(control.checkpoint().await, Ok(()));
		// a driver that last saw "not paused" must not be woken by the stale `true`
		assert!(
			futures::poll!(std::pin::pin!(control.pause_changed(false))).is_pending(),
			"a dropped pause sender is no pause change"
		);
	}

	#[tokio::test]
	async fn a_cancel_cannot_be_taken_back() {
		let (_pause, cancel, control) = controls();
		cancel.send_replace(true);
		assert!(control.is_cancelled());
		cancel.send_replace(false);
		assert!(control.is_cancelled());
		assert!(control.is_stopping());
		assert_eq!(control.checkpoint().await, Err(Stopped));
		control.stopping().await;
	}

	#[tokio::test]
	async fn dropped_controllers_neither_pause_nor_cancel() {
		let (pause, cancel, control) = controls();
		pause.send_replace(true);
		drop(pause);
		drop(cancel);
		assert_eq!(control.checkpoint().await, Ok(()));
		assert!(!control.is_stopping());
	}

	#[tokio::test(start_paused = true)]
	async fn until_stopping_drops_the_work_when_stopped() {
		let (_pause, cancel, control) = controls();
		let dropped = Arc::new(AtomicBool::new(false));
		let work = {
			let marker = SetOnDrop(dropped.clone());
			async move {
				let _marker = marker;
				std::future::pending::<()>().await;
			}
		};
		let run = tokio::spawn({
			let control = control.clone();
			async move { control.until_stopping(work).await }
		});
		tokio::time::sleep(Duration::from_secs(1)).await;
		cancel.send_replace(true);
		assert_eq!(run.await.unwrap(), Err(Stopped));
		assert!(dropped.load(Ordering::SeqCst));
	}

	#[tokio::test]
	async fn tasks_run_and_report_their_outputs() {
		let mut tasks = JobTasks::new();
		for i in 0..10usize {
			tasks.spawn(async move { i * 2 });
		}
		assert_eq!(tasks.len(), 10);
		let mut outputs = Vec::new();
		while let Some(out) = tasks.next().await {
			outputs.push(out);
		}
		outputs.sort();
		assert_eq!(outputs, (0..10).map(|i| i * 2).collect::<Vec<_>>());
		assert!(tasks.is_empty());
	}

	#[tokio::test]
	async fn dropping_the_set_aborts_its_tasks() {
		let dropped = Arc::new(AtomicBool::new(false));
		{
			let tasks = JobTasks::<()>::new();
			let marker = SetOnDrop(dropped.clone());
			tasks.spawn(async move {
				let _marker = marker;
				std::future::pending::<()>().await;
			});
			tokio::task::yield_now().await;
		}
		for _ in 0..1000 {
			if dropped.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(
			dropped.load(Ordering::SeqCst),
			"the task must not outlive the set"
		);
	}
}
