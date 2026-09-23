//! Cooperative control of a long-running job: pause, cancel, an internal stop, a cap on
//! concurrent operations, and background tasks that cannot outlive the job.
//!
//! Pause and cancel are observed by the job's own work instead of by stopping to poll it, so a
//! job can finish (or drop) its in-flight chunks and release their memory reservations before
//! it parks.

use std::{future::Future, sync::Arc};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::{
	consts::MAX_SMALL_PARALLEL_REQUESTS,
	runtime::{SpawnTaskHandle, spawn_task_maybe_send},
	util::MaybeSend,
};

/// Upper bound on a job's concurrent operations (creates, chunk transfers, finalizations).
///
/// Memory is bounded separately by the client's memory semaphore; this only stops thousands of
/// tiny files from fanning out into thousands of simultaneous operations. It mirrors the bound
/// the recursive upload puts on its in-flight entries, and every request still passes the
/// client-wide concurrency and rate limits.
pub(crate) const MAX_CONCURRENT_OPERATIONS: usize = MAX_SMALL_PARALLEL_REQUESTS;

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
}

/// Shared by all of a job's work. A dropped pause sender counts as "not paused" and a dropped
/// cancel sender as "not cancelled": losing a controller must never pause or cancel the job.
#[derive(Debug, Clone)]
pub(crate) struct JobControl {
	inner: Arc<Inner>,
}

impl Default for JobControl {
	fn default() -> Self {
		Self::new(None, None)
	}
}

impl JobControl {
	pub(crate) fn new(
		pause: Option<watch::Receiver<bool>>,
		cancel: Option<watch::Receiver<bool>>,
	) -> Self {
		let (stop, _) = watch::channel(false);
		Self {
			inner: Arc::new(Inner {
				pause,
				cancel,
				stop,
			}),
		}
	}

	pub(crate) fn is_cancelled(&self) -> bool {
		self.inner.cancel.as_ref().is_some_and(|c| *c.borrow())
	}

	/// Ends the job from within, e.g. when the account runs out of storage.
	pub(crate) fn stop(&self) {
		self.inner.stop.send_replace(true);
	}

	pub(crate) fn is_stopping(&self) -> bool {
		self.is_cancelled() || *self.inner.stop.borrow()
	}

	pub(crate) fn is_pause_requested(&self) -> bool {
		self.inner.pause.as_ref().is_some_and(|p| *p.borrow())
	}

	/// Resolves once the job is cancelled or stopped; never, if neither can happen.
	pub(crate) async fn stopping(&self) {
		let mut stop = self.inner.stop.subscribe();
		let stopped = async move {
			// the sender lives in `inner`, which outlives this future
			let _ = stop.wait_for(|stopped| *stopped).await;
		};
		match self.inner.cancel.clone() {
			Some(mut cancel) => {
				let cancelled = async move {
					if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
						std::future::pending::<()>().await;
					}
				};
				tokio::select! {
					() = cancelled => {},
					() = stopped => {},
				}
			}
			None => stopped.await,
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

/// Caps a job's concurrent operations. A permit is only handed out while the job is running.
#[derive(Debug, Clone)]
pub(crate) struct OperationLimiter {
	semaphore: Arc<Semaphore>,
}

impl OperationLimiter {
	pub(crate) fn new(max_operations: usize) -> Self {
		Self {
			semaphore: Arc::new(Semaphore::new(max_operations.max(1))),
		}
	}

	/// Waits for a free slot and for the job to be running. A pause that starts while waiting
	/// for a slot gives the slot back until the job resumes, so a paused job holds none.
	pub(crate) async fn acquire(
		&self,
		control: &JobControl,
	) -> Result<OwnedSemaphorePermit, Stopped> {
		loop {
			control.checkpoint().await?;
			let permit = control
				.until_stopping(self.semaphore.clone().acquire_owned())
				.await?
				.expect("the semaphore is never closed");
			if !control.is_pause_requested() {
				return Ok(permit);
			}
		}
	}

	#[cfg(test)]
	pub(crate) fn available(&self) -> usize {
		self.semaphore.available_permits()
	}
}

/// Background tasks owned by a job. Each runs on the SDK runtime (`spawn_task_maybe_send`:
/// a tokio task natively, a `spawn_local` task on the current worker on wasm), and is aborted
/// when the job aborts them or drops this set, so no task outlives the job.
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

	/// Aborts every running task and waits until all of them have dropped their work.
	pub(crate) async fn abort_all(&mut self) {
		self.kill.send_replace(true);
		while self.tasks.next().await.is_some() {}
	}
}

impl<T> Drop for JobTasks<T> {
	fn drop(&mut self) {
		self.kill.send_replace(true);
	}
}

#[cfg(test)]
mod tests {
	use std::{
		sync::atomic::{AtomicBool, AtomicUsize, Ordering},
		time::Duration,
	};

	use super::*;

	fn controlled() -> (watch::Sender<bool>, watch::Sender<bool>, JobControl) {
		let (pause, pause_rx) = watch::channel(false);
		let (cancel, cancel_rx) = watch::channel(false);
		(
			pause,
			cancel,
			JobControl::new(Some(pause_rx), Some(cancel_rx)),
		)
	}

	/// Sets its flag when dropped, which a task's future is when the task is aborted.
	struct SetOnDrop(Arc<AtomicBool>);
	impl Drop for SetOnDrop {
		fn drop(&mut self) {
			self.0.store(true, Ordering::SeqCst);
		}
	}

	#[tokio::test]
	async fn checkpoint_passes_while_running_and_fails_once_stopping() {
		let (_pause, cancel, control) = controlled();
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
		let (pause, _cancel, control) = controlled();
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
		let (pause, cancel, control) = controlled();
		pause.send_replace(true);
		let waiter = tokio::spawn({
			let control = control.clone();
			async move { control.checkpoint().await }
		});
		tokio::time::sleep(Duration::from_secs(1)).await;
		cancel.send_replace(true);
		assert_eq!(waiter.await.unwrap(), Err(Stopped));
	}

	#[tokio::test]
	async fn dropped_controllers_neither_pause_nor_cancel() {
		let (pause, cancel, control) = controlled();
		pause.send_replace(true);
		drop(pause);
		drop(cancel);
		assert_eq!(control.checkpoint().await, Ok(()));
		assert!(!control.is_stopping());
	}

	#[tokio::test(start_paused = true)]
	async fn until_stopping_drops_the_work_when_stopped() {
		let (_pause, cancel, control) = controlled();
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
	async fn limiter_caps_concurrent_operations() {
		let control = JobControl::default();
		let limiter = OperationLimiter::new(2);
		let first = limiter.acquire(&control).await.unwrap();
		let _second = limiter.acquire(&control).await.unwrap();
		assert_eq!(limiter.available(), 0);
		let third = tokio::spawn({
			let (limiter, control) = (limiter.clone(), control.clone());
			async move { limiter.acquire(&control).await.map(|_| ()) }
		});
		tokio::task::yield_now().await;
		assert!(!third.is_finished());
		drop(first);
		assert_eq!(third.await.unwrap(), Ok(()));
	}

	#[tokio::test(start_paused = true)]
	async fn a_paused_job_waiting_for_a_slot_holds_none() {
		let (pause, _cancel, control) = controlled();
		let limiter = OperationLimiter::new(1);
		let held = limiter.acquire(&control).await.unwrap();
		let waiter = tokio::spawn({
			let (limiter, control) = (limiter.clone(), control.clone());
			async move { limiter.acquire(&control).await.map(|_| ()) }
		});
		tokio::task::yield_now().await;
		pause.send_replace(true);
		drop(held);
		tokio::time::sleep(Duration::from_secs(1)).await;
		assert!(!waiter.is_finished(), "no slot is handed out while paused");
		assert_eq!(
			limiter.available(),
			1,
			"the paused waiter gave its slot back"
		);
		pause.send_replace(false);
		assert_eq!(waiter.await.unwrap(), Ok(()));
	}

	#[tokio::test]
	async fn limiter_gives_up_when_stopped() {
		let (_pause, cancel, control) = controlled();
		let limiter = OperationLimiter::new(1);
		let _held = limiter.acquire(&control).await.unwrap();
		let waiter = tokio::spawn({
			let (limiter, control) = (limiter.clone(), control.clone());
			async move { limiter.acquire(&control).await.map(|_| ()) }
		});
		tokio::task::yield_now().await;
		cancel.send_replace(true);
		assert_eq!(waiter.await.unwrap(), Err(Stopped));
	}

	#[test]
	fn max_concurrent_operations_mirrors_the_recursive_upload_bound() {
		assert_eq!(MAX_CONCURRENT_OPERATIONS, MAX_SMALL_PARALLEL_REQUESTS);
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
	async fn abort_all_drops_every_running_task() {
		let mut tasks = JobTasks::<()>::new();
		let dropped = Arc::new(AtomicUsize::new(0));
		struct CountOnDrop(Arc<AtomicUsize>);
		impl Drop for CountOnDrop {
			fn drop(&mut self) {
				self.0.fetch_add(1, Ordering::SeqCst);
			}
		}
		for _ in 0..5 {
			let marker = CountOnDrop(dropped.clone());
			tasks.spawn(async move {
				let _marker = marker;
				std::future::pending::<()>().await;
			});
		}
		tasks.abort_all().await;
		assert_eq!(dropped.load(Ordering::SeqCst), 5);
		assert!(tasks.next().await.is_none());
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
