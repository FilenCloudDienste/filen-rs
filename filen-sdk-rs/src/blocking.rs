//! Synchronous work that has to park its thread (a decoder that reads remote bytes chunk by chunk
//! and waits for each), taken off the threads that drive the async runtime.

use std::{cell::Cell, rc::Rc};

use tokio::sync::oneshot;

use crate::util::panic_message;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use wasm_worker::WorkerSlot;

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

/// One long-lived dedicated wasm worker per purpose (thumbnail decodes, archive codecs), which
/// runs that purpose's jobs one after another.
///
/// Which thread the jobs run on matters, in three ways:
/// - **Not the commander.** It drives every async task in the SDK, the very chunk fetches a
///   blocked job waits for included.
/// - **Not a rayon worker.** `runtime::do_cpu_intensive` IS the rayon pool, and chunk decryption
///   runs there too: a job parked on a rayon worker would be waiting on the pool that has to run
///   to unblock it.
/// - **One worker at a time per purpose, not one per job.** Each `runtime::spawn` instantiates a
///   fresh wasm module and its thread state in the shared linear memory that is never returned to
///   the host, and retains the worker's JS wrapper for the life of the spawning thread. A worker
///   per job would grow exactly the memory these jobs are budgeted to bound. A replacement is
///   spawned only when the previous worker is retired for going silent (see
///   [`WorkerSlot::retire`]).
///
/// Parking there is legal: `wasm-full` builds with `+atomics`, where `std` selects the futex
/// parker, and the worker is a dedicated one rather than the JS main thread.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm_worker {
	use std::sync::{
		Mutex, PoisonError,
		atomic::{AtomicU64, Ordering},
		mpsc,
	};

	type Job = Box<dyn FnOnce() + Send>;

	/// One purpose's worker, and the liveness bookkeeping the drivers of its jobs share. Meant to
	/// be a `static`: the worker outlives every client.
	pub(crate) struct WorkerSlot {
		/// The live worker's job sender, tagged with the generation it belongs to.
		///
		/// Re-settable rather than a `OnceLock` because the worker CAN stop draining this channel
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
		/// drains, and every later job of the purpose queues behind a worker that is gone.
		/// [`retire`](Self::retire) is how a driver's stall deadline gets out of that.
		jobs: Mutex<Option<(u64, mpsc::Sender<Job>)>>,
		next_generation: AtomicU64,
		/// Bumped every time the worker is observed doing something: asking a driver for a chunk,
		/// taking that chunk back, or finishing a job.
		///
		/// Per purpose rather than per driver because a driver's own `select!` is NOT a liveness
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
				jobs: Mutex::new(None),
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
		/// [`retire`](Self::retire) if it stops answering.
		pub(crate) fn submit<T: Send + 'static>(
			&'static self,
			job: impl FnOnce() -> T + Send + 'static,
			on_panic: impl FnOnce(String) -> T + Send + 'static,
		) -> (u64, tokio::sync::oneshot::Receiver<T>) {
			let (result_tx, result_rx) = tokio::sync::oneshot::channel();
			// Poisoning cannot happen under `panic=abort`; taking the guard anyway keeps a
			// hypothetical one from wedging the very jobs this exists to un-wedge.
			let mut slot = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
			let (generation, jobs) = slot.get_or_insert_with(|| {
				let (tx, rx) = mpsc::channel::<Job>();
				crate::runtime::spawn(move || {
					// `recv` parks this worker between jobs; it fails only once `retire` has
					// dropped this generation's sender, which is the worker's cue to let its
					// thread go.
					while let Ok(job) = rx.recv() {
						job();
					}
				});
				(self.next_generation.fetch_add(1, Ordering::Relaxed), tx)
			});
			// Succeeds whether or not anything is still draining the channel (see `jobs`). A dead
			// worker is detected by the driver's deadline, never by this send.
			let generation = *generation;
			let _ = jobs.send(Box::new(move || {
				// a panic traps this worker: retired, the next job gets a fresh one instead of
				// waiting out a stall deadline behind it. Retiring is the outer report, so it runs
				// before the driver hears of the panic and submits again.
				super::catch_panic(
					move || super::send_catching_panic(job, result_tx, on_panic),
					move |_| self.retire(generation),
				);
				// Not about speed: chunk traffic already covers a job that is merely slow. This is
				// the only event a job that asks for no chunk at all produces, so bumping here is
				// what makes the invariant total: every job the worker takes and returns from
				// moves the stamp at least once.
				self.note_activity();
			}));
			(generation, result_rx)
		}

		/// Drops `generation`'s sender, so the next [`submit`](Self::submit) spawns a fresh worker
		/// and the old thread exits if it ever comes back.
		///
		/// Generation-checked because several callers can be queued behind the same dead worker
		/// and each times out on its own schedule: unchecked, the late ones would retire the
		/// healthy replacement, and every respawn costs a wasm module instantiation that is never
		/// returned to the host.
		pub(crate) fn retire(&self, generation: u64) {
			let mut slot = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
			if slot.as_ref().is_some_and(|(live, _)| *live == generation) {
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
			self.jobs
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.as_ref()
				.is_some_and(|(live, _)| *live == generation)
		}
	}
}
