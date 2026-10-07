//! Synchronous work that has to park its thread (a decoder that reads remote bytes chunk by chunk
//! and waits for each), taken off the threads that drive the async runtime.

use std::{cell::Cell, rc::Rc};

use tokio::sync::oneshot;

use crate::util::panic_message;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use wasm_worker::{WorkerPool, WorkerSlot};

/// Runs `job` and sends what it returns through `result`; should it panic, sends what
/// `on_panic` makes of the panic's message instead, so the waiting side learns why the job
/// ended rather than only that it is gone.
pub(crate) fn send_catching_panic<T: 'static>(
	job: impl FnOnce() -> T,
	result: oneshot::Sender<T>,
	on_panic: impl FnOnce(String) -> T + 'static,
) {
	// shared with the report, which on wasm sends from inside the panic hook, never returning here
	let result = Rc::new(Cell::new(Some(result)));
	let report = {
		let result = Rc::clone(&result);
		move |message| {
			if let Some(result) = result.take() {
				let _ = result.send(on_panic(message));
			}
		}
	};
	if let Some(value) = catch_panic(job, report)
		&& let Some(result) = result.take()
	{
		let _ = result.send(value);
	}
}

/// Runs `job`; should it panic, hands the panic's message to `report` and returns `None`.
///
/// Natively the panic is caught once it unwound out of `job`, whose state went with it, so
/// nothing it left half-done is looked at again. The wasm build is `panic=abort`: nothing
/// unwinds, and the thread traps right after the panic hook ran, so the hook calls `report`
/// (see `panic_reports`, wasm only) and this never returns `None` there.
fn catch_panic<T>(job: impl FnOnce() -> T, report: impl FnOnce(String) + 'static) -> Option<T> {
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
			.map_err(|payload| report(panic_message(payload.as_ref())))
			.ok()
	}
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		panic_reports::push(Box::new(report));
		let value = job();
		panic_reports::pop();
		Some(value)
	}
}

/// The reports [`catch_panic`] leaves for the panic hook on wasm, where a panic traps its thread
/// instead of unwinding to a caller that could catch it.
///
/// No test runs this module. The native tests catch a codec's panic through `catch_unwind`
/// (`fs::archive::worker`'s tests), and the browser suite never makes a worker panic, so the
/// hook, the outermost-first order of the reports and the retire they drive are unproven on wasm.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod panic_reports {
	use std::{cell::RefCell, sync::Once};

	type Report = Box<dyn FnOnce(String)>;

	thread_local! {
		/// The reports of the calls running on this thread, outermost first.
		static REPORTS: RefCell<Vec<Report>> = const { RefCell::new(Vec::new()) };
	}

	static HOOK: Once = Once::new();

	pub(super) fn push(report: Report) {
		// chained in front of the hook in place (the console logger `main_js` installs), which
		// still runs after the reports
		HOOK.call_once(|| {
			let previous = std::panic::take_hook();
			std::panic::set_hook(Box::new(move |info| {
				let reports = REPORTS
					.try_with(|reports| {
						reports
							.try_borrow_mut()
							.map(|mut r| std::mem::take(&mut *r))
					})
					.ok()
					.and_then(Result::ok)
					.unwrap_or_default();
				if !reports.is_empty() {
					let message = super::panic_message(info.payload());
					// outermost first: a worker is retired before its job's driver hears of the
					// panic, so a job the driver submits next never reaches the dying worker
					for report in reports {
						report(message.clone());
					}
				}
				previous(info);
			}));
		});
		REPORTS.with(|reports| reports.borrow_mut().push(report));
	}

	pub(super) fn pop() {
		REPORTS.with(|reports| reports.borrow_mut().pop());
	}
}

/// Long-lived dedicated wasm workers, a few per purpose, which run that purpose's jobs: one for
/// archive codecs ([`WorkerSlot`]), a small pool for thumbnail decodes ([`WorkerPool`]).
///
/// Which thread the jobs run on matters, in three ways:
/// - **Not the commander.** It drives every async task in the SDK, the very chunk fetches a
///   blocked job waits for included.
/// - **Not a rayon worker.** `runtime::do_cpu_intensive` IS the rayon pool, and chunk decryption
///   runs there too: a job parked on a rayon worker would be waiting on the pool that has to run
///   to unblock it.
/// - **A fixed few workers per purpose, never one per job.** Each `runtime::spawn` instantiates a
///   fresh wasm module and its thread state in the shared linear memory that is never returned to
///   the host, and retains the worker's JS wrapper for the life of the spawning thread. A worker
///   per job would grow exactly the memory these jobs are budgeted to bound. A slot spawns its
///   worker when a job first needs one, and a replacement only once that worker is retired: for a
///   job that panicked (which traps the worker), or for going silent (see
///   [`WorkerSlot::retire`]).
///
/// Parking there is legal: `wasm-full` builds with `+atomics`, where `std` selects the futex
/// parker, and the worker is a dedicated one rather than the JS main thread.
///
/// Natively no job runs here (each gets a thread of its own); the module is compiled for the
/// native unit tests only, where `runtime::spawn` starts a plain thread.
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), test))]
mod wasm_worker {
	use std::sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicU64, AtomicUsize, Ordering},
		mpsc,
	};

	use tokio::sync::oneshot;

	type Job = Box<dyn FnOnce() + Send>;

	/// A spawned worker: where its jobs go, and how many of them it has yet to finish.
	struct Live {
		generation: u64,
		jobs: mpsc::Sender<Job>,
		/// Jobs handed to this worker and not finished: what a [`WorkerPool`] picks a worker by.
		///
		/// `Arc`: the jobs count themselves off on the worker thread. Per generation rather than
		/// per slot, so a retired worker that does come back counts off its own jobs, never its
		/// replacement's. `Relaxed` throughout: it guards no data, and a stale read only sends a
		/// job behind a busy worker or spawns one more within the pool's bound, never a wrong
		/// result. A driver that has its answer reads it fresh: the job counts itself off before
		/// it sends the result, whose channel orders the two.
		pending: Arc<AtomicUsize>,
	}

	/// One worker, and the liveness bookkeeping the drivers of its jobs share. Meant to be a
	/// `static`, or a slot of one: the worker outlives every client.
	pub(crate) struct WorkerSlot {
		/// The live worker, tagged with the generation it belongs to.
		///
		/// Re-settable rather than a `OnceLock` because the worker CAN stop draining its channel
		/// without ever exiting. Two ways:
		///
		/// - a trap. The wasm build is `panic=abort` and `heif-decoder` stubs `__cxa_throw` as
		///   `unreachable`, so a malformed HEIC turns libheif's `length_error`/`bad_alloc` into a
		///   trap on this worker. A trap abandons the thread's stack without running a single
		///   destructor, so the `Receiver` is leaked rather than closed;
		/// - a park, in an earlier shape of the thumbnail source that shared one reply channel
		///   across a decode: a driver dropped between taking a request and answering it left the
		///   worker waiting for a reply that never came. Reply channels are per request now and
		///   close with the driver, so that shape is gone; the machinery below stays for the trap.
		///
		/// Either way `send` in [`submit`](Self::submit) keeps SUCCEEDING into a channel nothing
		/// drains, and every later job of the slot queues behind a worker that is gone.
		/// [`retire`](Self::retire) is how a driver's stall deadline gets out of that.
		live: Mutex<Option<Live>>,
		next_generation: AtomicU64,
		/// Bumped every time the worker is observed doing something: asking a driver for a chunk,
		/// taking that chunk back, or finishing a job.
		///
		/// Per slot rather than per driver because a driver's own `select!` is NOT a liveness
		/// signal for the worker. Several drivers can wait on the one worker at once while it runs
		/// their jobs strictly in turn, and a driver queued behind someone else's job never sees a
		/// reply of its own, however healthy that job is. Without this it would time out on the
		/// other caller's wall clock and retire a live worker, which costs a wasm instantiation
		/// that is never given back.
		///
		/// `Relaxed` at both ends, deliberately: nothing is published through this counter (it
		/// guards no data), and a driver reads it only to choose between re-arming a timer and
		/// returning an error. All it needs is that a bump eventually becomes visible, which every
		/// ordering gives. A check-read that did miss one would retire a live worker, costing an
		/// instantiation, never a wrong result and never a missed hang.
		activity: AtomicU64,
	}

	impl WorkerSlot {
		pub(crate) const fn new() -> Self {
			Self {
				live: Mutex::new(None),
				next_generation: AtomicU64::new(0),
				activity: AtomicU64::new(0),
			}
		}

		/// Records progress, returning the stamp to compare against when the caller's deadline
		/// expires.
		pub(crate) fn note_activity(&self) -> u64 {
			self.activity
				.fetch_add(1, Ordering::Relaxed)
				.wrapping_add(1)
		}

		/// The current stamp. Equal to what a driver last recorded means nothing anywhere has
		/// heard from the worker since.
		pub(crate) fn activity(&self) -> u64 {
			self.activity.load(Ordering::Relaxed)
		}

		/// Queues `job`, spawning a worker if there is none. The receiver gets what `job`
		/// returns, or, should it panic, what `on_panic` makes of the panic's message; `on_panic`
		/// runs inside the panic hook, so it must not log (the panic may have struck while the
		/// log's lock was held).
		///
		/// The returned generation names the worker that took the job; hand it to
		/// [`retire`](Self::retire) if it stops answering. A job whose receiver is gone by the
		/// time the worker reaches it is skipped.
		pub(crate) fn submit<T: Send + 'static>(
			&'static self,
			job: impl FnOnce() -> T + Send + 'static,
			on_panic: impl FnOnce(String) -> T + Send + 'static,
		) -> (u64, oneshot::Receiver<T>) {
			let (result_tx, result_rx) = oneshot::channel();
			// Poisoning cannot happen under `panic=abort`; taking the guard anyway keeps a
			// hypothetical one from wedging the very jobs this exists to un-wedge.
			let mut slot = self.live.lock().unwrap_or_else(PoisonError::into_inner);
			let live = slot.get_or_insert_with(|| {
				let (jobs, rx) = mpsc::channel::<Job>();
				crate::runtime::spawn(move || {
					// `recv` parks this worker between jobs; it fails only once `retire` has
					// dropped this generation's sender, which is the worker's cue to let its
					// thread go.
					while let Ok(job) = rx.recv() {
						job();
					}
				});
				Live {
					generation: self.next_generation.fetch_add(1, Ordering::Relaxed),
					jobs,
					pending: Arc::new(AtomicUsize::new(0)),
				}
			});
			let generation = live.generation;
			live.pending.fetch_add(1, Ordering::Relaxed);
			// counted off however the job ends, short of a trap, which retires the worker anyway
			let (returned, panicked, skipped) = (
				Arc::clone(&live.pending),
				Arc::clone(&live.pending),
				Arc::clone(&live.pending),
			);
			let job = move || {
				let value = job();
				returned.fetch_sub(1, Ordering::Relaxed);
				value
			};
			let on_panic = move |message| {
				panicked.fetch_sub(1, Ordering::Relaxed);
				on_panic(message)
			};
			// Succeeds whether or not anything is still draining the channel (see `live`). A dead
			// worker is detected by the driver's deadline, never by this send.
			let _ = live.jobs.send(Box::new(move || {
				if result_tx.is_closed() {
					// Its driver went away while it queued: nothing waits for what it would
					// make, and it would only hold the worker from the jobs behind it.
					skipped.fetch_sub(1, Ordering::Relaxed);
				} else {
					// a panic traps this worker: retired, the next job gets a fresh one instead
					// of waiting out a stall deadline behind it. Retiring is the outer report, so
					// it runs before the driver hears of the panic and submits again.
					super::catch_panic(
						move || super::send_catching_panic(job, result_tx, on_panic),
						move |_| self.retire(generation),
					);
				}
				// Not about speed: chunk traffic already covers a job that is merely slow. This is
				// the only event a job that asks for no chunk at all produces, so bumping here is
				// what makes the invariant total: every job the worker takes and returns from
				// moves the stamp at least once.
				self.note_activity();
			}));
			(generation, result_rx)
		}

		/// How many jobs the live worker has yet to finish; `None` when there is no live worker,
		/// so the next [`submit`](Self::submit) spawns one.
		fn load(&self) -> Option<usize> {
			self.live
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.as_ref()
				.map(|live| live.pending.load(Ordering::Relaxed))
		}

		/// Drops `generation`'s sender, so the next [`submit`](Self::submit) spawns a fresh worker
		/// and the old thread exits if it ever comes back.
		///
		/// Generation-checked because several callers can be queued behind the same dead worker
		/// and each times out on its own schedule: unchecked, the late ones would retire the
		/// healthy replacement, and every respawn costs a wasm module instantiation that is never
		/// returned to the host.
		pub(crate) fn retire(&self, generation: u64) {
			let mut slot = self.live.lock().unwrap_or_else(PoisonError::into_inner);
			if slot
				.as_ref()
				.is_some_and(|live| live.generation == generation)
			{
				*slot = None;
			}
		}

		/// Whether `generation` is still the worker [`submit`](Self::submit) queues to.
		///
		/// A driver whose generation has been retired will never be served (its job is stranded
		/// in the dead worker's leaked channel), so activity from the REPLACEMENT worker must not
		/// keep re-arming its deadline. Without this the stamp would read as movement forever and
		/// that driver would hang holding its permit.
		pub(crate) fn is_live(&self, generation: u64) -> bool {
			self.live
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.as_ref()
				.is_some_and(|live| live.generation == generation)
		}
	}

	/// Up to `N` workers of one purpose, each a [`WorkerSlot`] with liveness bookkeeping of its
	/// own, spawned only as jobs running at once need them.
	///
	/// A job goes to any live worker with nothing to do, whichever submitter spawned it;
	/// failing that, to one of the first `width` slots without a worker, which spawns one;
	/// failing that, behind the worker with the fewest jobs among the first `width`. `width`
	/// bounds where a job may make the pool grow, not which idle worker it may use, so the pool
	/// never holds more workers than jobs ever ran at once, nor more than the widest `width` a
	/// submitter asked for, nor more than `N`. A job queued behind another starts when that one
	/// ends, not when some other worker frees up first; a submitter that admits no more jobs at
	/// once than its `width` (a gate of that size) only ever queues behind other submitters'
	/// jobs, or behind jobs whose drivers went away without waiting for them.
	pub(crate) struct WorkerPool<const N: usize> {
		slots: [WorkerSlot; N],
		/// Held while a job picks its worker, so two submitters never both take the one idle
		/// worker, or both spawn into the one empty slot.
		dispatch: Mutex<()>,
	}

	impl<const N: usize> WorkerPool<N> {
		pub(crate) const fn new() -> Self {
			const { assert!(N > 0, "a pool needs a worker to run its jobs") };
			Self {
				slots: [const { WorkerSlot::new() }; N],
				dispatch: Mutex::new(()),
			}
		}

		/// Queues `job` on an idle worker, or else on one of the first `width` slots (at least
		/// one, at most `N`), spawning that slot's worker if it has none; see
		/// [`WorkerSlot::submit`] for the result and `on_panic`. The ticket names the worker that
		/// took the job.
		pub(crate) fn submit<T: Send + 'static>(
			&'static self,
			width: usize,
			job: impl FnOnce() -> T + Send + 'static,
			on_panic: impl FnOnce(String) -> T + Send + 'static,
		) -> (WorkerTicket, oneshot::Receiver<T>) {
			let _dispatch = self.dispatch.lock().unwrap_or_else(PoisonError::into_inner);
			let width = width.max(1);
			let mut empty = None;
			let mut least: Option<(&'static WorkerSlot, usize)> = None;
			let mut idle = None;
			for (index, slot) in self.slots.iter().enumerate() {
				let within = index < width;
				match slot.load() {
					Some(0) => {
						idle = Some(slot);
						break;
					}
					None if within => {
						empty.get_or_insert(slot);
					}
					Some(pending) if within => {
						if least.is_none_or(|(_, fewest)| pending < fewest) {
							least = Some((slot, pending));
						}
					}
					None | Some(_) => {}
				}
			}
			let slot = idle
				.or(empty)
				.or(least.map(|(slot, _)| slot))
				.expect("a pool has a slot, and every submit looks at one (should be impossible)");
			let (generation, done) = slot.submit(job, on_panic);
			(WorkerTicket { slot, generation }, done)
		}
	}

	/// The worker a [`WorkerPool`] gave a job to: what the job's driver watches, and retires
	/// should it stop answering. The [`WorkerSlot`] calls of the same names, bound to that worker.
	pub(crate) struct WorkerTicket {
		slot: &'static WorkerSlot,
		generation: u64,
	}

	impl WorkerTicket {
		pub(crate) fn note_activity(&self) -> u64 {
			self.slot.note_activity()
		}

		pub(crate) fn activity(&self) -> u64 {
			self.slot.activity()
		}

		pub(crate) fn is_live(&self) -> bool {
			self.slot.is_live(self.generation)
		}

		pub(crate) fn retire(&self) {
			self.slot.retire(self.generation);
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{
		collections::HashSet,
		sync::{
			Arc, Condvar, Mutex, PoisonError,
			atomic::{AtomicBool, Ordering},
		},
		thread::{self, ThreadId},
		time::Duration,
	};

	use super::wasm_worker::{WorkerPool, WorkerTicket};

	/// How long a job waits for the others it expects to run beside it, or for its release.
	/// Only ever spent in full when the pool is broken: a working one wakes it at once.
	const PATIENCE: Duration = Duration::from_secs(30);

	/// Jobs that hold their worker until the test lets them go, and count how many got that far.
	#[derive(Default)]
	struct Hold {
		state: Mutex<(usize, bool)>,
		changed: Condvar,
	}

	impl Hold {
		/// Run on a worker: counts this job in, then holds it until released. Whether it was
		/// released rather than given up on.
		fn arrive_and_wait(&self) -> bool {
			let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
			state.0 += 1;
			self.changed.notify_all();
			let (state, _) = self
				.changed
				.wait_timeout_while(state, PATIENCE, |(_, released)| !*released)
				.unwrap_or_else(PoisonError::into_inner);
			state.1
		}

		/// Waits until `count` jobs are holding their workers at once. Whether they all were.
		fn wait_for(&self, count: usize) -> bool {
			let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
			let (state, _) = self
				.changed
				.wait_timeout_while(state, PATIENCE, |(arrived, _)| *arrived < count)
				.unwrap_or_else(PoisonError::into_inner);
			state.0 >= count
		}

		fn release(&self) {
			self.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
			self.changed.notify_all();
		}
	}

	type Answer = (ThreadId, bool);

	fn no_panic(message: String) -> Answer {
		panic!("a test job panicked: {message}")
	}

	/// A job that holds its worker until `hold` is released, answering with the worker's thread.
	fn held(
		pool: &'static WorkerPool<4>,
		width: usize,
		hold: &Arc<Hold>,
	) -> (WorkerTicket, tokio::sync::oneshot::Receiver<Answer>) {
		let hold = Arc::clone(hold);
		pool.submit(
			width,
			move || (thread::current().id(), hold.arrive_and_wait()),
			no_panic,
		)
	}

	fn quick(
		pool: &'static WorkerPool<4>,
		width: usize,
	) -> (WorkerTicket, tokio::sync::oneshot::Receiver<Answer>) {
		pool.submit(width, || (thread::current().id(), true), no_panic)
	}

	#[test]
	fn a_pool_runs_as_many_jobs_at_once_as_its_width() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let hold = Arc::new(Hold::default());

		let jobs: Vec<_> = (0..3).map(|_| held(&POOL, 3, &hold)).collect();
		assert!(
			hold.wait_for(3),
			"three jobs on a pool three wide must all be running at once"
		);
		hold.release();

		let threads: HashSet<ThreadId> = jobs
			.into_iter()
			.map(|(_, done)| done.blocking_recv().expect("the job answers"))
			.map(|(thread, released)| {
				assert!(released);
				thread
			})
			.collect();
		assert_eq!(threads.len(), 3, "each ran on a worker of its own");
	}

	#[test]
	fn a_pool_spawns_no_more_workers_than_its_width() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let hold = Arc::new(Hold::default());

		let jobs: Vec<_> = (0..6).map(|_| held(&POOL, 2, &hold)).collect();
		assert!(hold.wait_for(2));
		hold.release();

		let threads: HashSet<ThreadId> = jobs
			.into_iter()
			.map(|(_, done)| done.blocking_recv().expect("the job answers").0)
			.collect();
		assert_eq!(
			threads.len(),
			2,
			"the four jobs past the width waited their turn on the two workers"
		);
	}

	#[test]
	fn an_idle_worker_takes_a_narrower_submitters_job() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let (busy, done_soon) = (Arc::new(Hold::default()), Arc::new(Hold::default()));

		// a wider submitter's two jobs spawn two workers
		let (_, busy_done) = held(&POOL, 2, &busy);
		let (_, soon_done) = held(&POOL, 2, &done_soon);
		assert!(busy.wait_for(1) && done_soon.wait_for(1));
		done_soon.release();
		let (idle_thread, _) = soon_done.blocking_recv().expect("the job answers");

		// one wide only, yet it runs on the idle second worker, not behind the busy first
		let (_, done) = quick(&POOL, 1);
		let (thread, _) = done.blocking_recv().expect("the job answers");
		assert_eq!(thread, idle_thread);

		busy.release();
		let (busy_thread, released) = busy_done.blocking_recv().expect("the job answers");
		assert!(
			released,
			"the narrow job finished while the busy worker still held its own"
		);
		assert_ne!(busy_thread, idle_thread);
	}

	#[test]
	fn a_job_whose_driver_went_away_is_skipped() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let hold = Arc::new(Hold::default());
		let ran = Arc::new(AtomicBool::new(false));

		let (_, first_done) = held(&POOL, 1, &hold);
		assert!(hold.wait_for(1));
		let (_, abandoned) = {
			let ran = Arc::clone(&ran);
			POOL.submit(
				1,
				move || {
					ran.store(true, Ordering::Relaxed);
					(thread::current().id(), true)
				},
				no_panic,
			)
		};
		drop(abandoned);
		// queued behind the abandoned one, so it runs once that one was skipped
		let (_, after) = quick(&POOL, 1);
		hold.release();
		let (first_thread, _) = first_done.blocking_recv().expect("the job answers");
		after.blocking_recv().expect("the job answers");
		assert!(!ran.load(Ordering::Relaxed));

		// and the skipped job was counted off: the worker is idle again, so the next job,
		// however wide, reuses it rather than spawning another
		let (_, next) = quick(&POOL, 4);
		assert_eq!(
			next.blocking_recv().expect("the job answers").0,
			first_thread
		);
	}

	#[test]
	fn each_worker_keeps_its_own_activity_stamp() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let hold = Arc::new(Hold::default());

		let (first, first_done) = held(&POOL, 2, &hold);
		let (second, second_done) = held(&POOL, 2, &hold);
		assert!(hold.wait_for(2));
		let (first_before, second_before) = (first.activity(), second.activity());
		// a driver queued on the second worker is not kept waiting by the first one's progress
		assert_ne!(first.note_activity(), first_before);
		assert_eq!(second.activity(), second_before);
		hold.release();

		let (first_thread, _) = first_done.blocking_recv().expect("the job answers");
		let (second_thread, _) = second_done.blocking_recv().expect("the job answers");
		assert_ne!(first_thread, second_thread);
	}

	#[test]
	fn a_pool_reuses_an_idle_worker_before_spawning_another() {
		static POOL: WorkerPool<4> = WorkerPool::new();

		let threads: HashSet<ThreadId> = (0..5)
			.map(|_| {
				let (_, done) = quick(&POOL, 4);
				done.blocking_recv().expect("the job answers").0
			})
			.collect();
		assert_eq!(
			threads.len(),
			1,
			"jobs one after another all ran on the first worker"
		);
	}

	#[test]
	fn a_retired_worker_is_replaced_while_its_job_is_stranded() {
		static POOL: WorkerPool<4> = WorkerPool::new();
		let hold = Arc::new(Hold::default());

		let (stuck, stranded) = held(&POOL, 1, &hold);
		assert!(hold.wait_for(1));
		stuck.retire();
		assert!(!stuck.is_live());

		// the only slot spawns a replacement, which runs the next job at once
		let (fresh, done) = quick(&POOL, 1);
		assert!(fresh.is_live());
		let (thread, _) = done.blocking_recv().expect("the replacement answers");
		// a late retire for the old generation leaves the replacement alone
		stuck.retire();
		assert!(fresh.is_live());

		hold.release();
		let (old_thread, released) = stranded.blocking_recv().expect("the old worker answers");
		assert!(released);
		assert_ne!(old_thread, thread);
	}
}
