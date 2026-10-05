use futures::FutureExt;
#[cfg(feature = "wasm-full")]
pub use wasm_bindgen_rayon::init_thread_pool;

use crate::util::MaybeSend;

// Scoped tasks: rayon runs closures that borrow the caller's stack, and the handle blocks on
// drop until they finish.
#[allow(unsafe_code)]
mod async_scoped_task {
	// The mt-crypto-only scoped-task machinery, gated once at the module rather than per item.
	#[cfg(feature = "multi-threaded-crypto")]
	mod multi_threaded {
		use std::mem::ManuallyDrop;

		pub(super) struct AsyncTaskHandle<T> {
			async_receiver: ManuallyDrop<tokio::sync::oneshot::Receiver<T>>,
		}

		impl<T> AsyncTaskHandle<T> {
			pub(super) fn new(receiver: tokio::sync::oneshot::Receiver<T>) -> Self {
				Self {
					async_receiver: ManuallyDrop::new(receiver),
				}
			}
		}

		impl<T> Drop for AsyncTaskHandle<T> {
			fn drop(&mut self) {
				// SAFETY: we are taking the receiver out of the ManuallyDrop
				// we do this exactly once, in the drop impl, so it's safe
				let mut async_receiver = unsafe { ManuallyDrop::take(&mut self.async_receiver) };

				match async_receiver.try_recv() {
					Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
					Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
						tracing::debug!(
							"AsyncTaskHandle being dropped before completion, blocking current thread to avoid UB"
						);
						// This wait is mandatory for soundness: the rayon task may still be
						// using data borrowed from the caller's stack. Panicking here would
						// skip the wait and free those borrows early. Every ready-made
						// blocking primitive can panic in some host context —
						// `block_in_place` outside a blocking-allowed multi-threaded-runtime
						// thread (including inside a current-thread `block_on`, whatever
						// handle is entered), `blocking_recv` on any runtime-driving thread,
						// `futures::executor::block_on` inside another futures executor (its
						// `enter()` re-entrancy guard) — so this parks the thread manually
						// instead, consulting no executor thread-locals at all.
						#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
						{
							let _ = park_until_received(async_receiver);
						}
						#[cfg(all(target_family = "wasm", target_os = "unknown"))]
						{
							let _ = async_receiver.blocking_recv();
						}
					}
				}
			}
		}

		impl<T> Future for AsyncTaskHandle<T> {
			type Output = T;

			fn poll(
				self: std::pin::Pin<&mut Self>,
				cx: &mut std::task::Context<'_>,
			) -> std::task::Poll<Self::Output> {
				let this = self.get_mut();
				std::pin::Pin::new(&mut *this.async_receiver)
					.poll(cx)
					.map(|res| res.expect("Thread panicked"))
			}
		}

		/// Blocks the current thread until the receiver resolves by polling it with a
		/// `thread::park`-based waker. The oneshot's waker is fired directly by the rayon-side
		/// `send`/sender-drop, cross-thread, so completion needs no runtime; polling a tokio
		/// oneshot consults no executor state. This cannot panic in any host context (unlike
		/// `block_in_place`, `blocking_recv`, or `futures::executor::block_on`), which the
		/// soundness of [`AsyncTaskHandle`]'s drop depends on. Parking a runtime worker here is
		/// acceptable: the wait is bounded by the rayon task's pure-CPU duration and only happens
		/// on the rare drop-before-completion path.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		fn park_until_received<T>(
			mut receiver: tokio::sync::oneshot::Receiver<T>,
		) -> Result<T, tokio::sync::oneshot::error::RecvError> {
			use std::{
				pin::Pin,
				sync::Arc,
				task::{Context, Poll, Wake, Waker},
				thread,
			};

			struct ThreadUnparker(thread::Thread);

			impl Wake for ThreadUnparker {
				fn wake(self: Arc<Self>) {
					self.0.unpark();
				}

				fn wake_by_ref(self: &Arc<Self>) {
					self.0.unpark();
				}
			}

			let waker = Waker::from(Arc::new(ThreadUnparker(thread::current())));
			let mut cx = Context::from_waker(&waker);
			loop {
				match Pin::new(&mut receiver).poll(&mut cx) {
					Poll::Ready(res) => return res,
					// A lost unpark token cannot deadlock: `unpark` called between our poll
					// and `park` makes `park` return immediately, and spurious wakeups just
					// re-poll.
					Poll::Pending => thread::park(),
				}
			}
		}

		/// Spawns a closure on the rayon threadpool without requiring 'static lifetime.
		///
		/// # Safety
		/// The caller must guarantee that the closure does not outlive any references it captures.
		pub(super) unsafe fn spawn_unchecked<F>(f: F)
		where
			F: FnOnce() + Send,
		{
			let f = Box::into_raw(Box::new(f));
			struct SendPtr(*mut ());

			unsafe impl Send for SendPtr {}
			let ptr = SendPtr(f as *mut ());

			rayon::spawn(move || {
				let ptr = ptr;
				// SAFETY: we are doing this to bypass the 'static requirement of rayon::spawn
				// this function is unsafe because the caller must guarantee that the closure
				// does not outlive any references it captures
				let f = unsafe { Box::from_raw(ptr.0 as *mut F) };
				f();
			});
		}
	}

	/// Runs a CPU intensive intensive function on the rayon threadpool, returning a future that resolves to the result.
	///
	/// # Important
	/// Requires that this future is ***NEVER*** forgotten, or it can cause UB.
	/// Will block the current thread if dropped before completion.
	///
	/// # Safety
	/// This function should technically be unsafe, however it would be annoying to use unsafe everywhere it is used in this crate
	/// and as a general principle within this crate we should never be leaking any futures anywhere
	/// and with its pub(crate) visibility it should be safe enough.
	///
	/// # Futures Notes
	/// This is a 'naive' implementation of a Scoped Task system with an async interface.
	/// Hopefully, one day it will be possible for such an implementation to exist in a safe manner without the need for blocking on drop.
	/// This is blocked on 2 things
	/// 1) A functional AsyncDrop trait in Rust https://github.com/rust-lang/rust/issues/126482.
	///    I tried to use the existing nightly version behind the feature flag,
	///    but it was giving me a bunch of memory segfaults and other issues,
	///    so I would consider that to be not production ready yet.
	/// 2) A Linear Type system/!Forget trait/!Leak trait/drop guarantee in Rust.
	///    This would allow us to guarantee at compile time that the futures we create here are never leaked or forgotten.
	///    This is a longer term goal without a tracking issue that I could find.
	pub(crate) fn do_cpu_intensive<F, R>(f: F) -> impl Future<Output = R>
	where
		F: FnOnce() -> R + Send,
		R: Send,
	{
		#[cfg(feature = "multi-threaded-crypto")]
		{
			use multi_threaded::{AsyncTaskHandle, spawn_unchecked};

			let (async_sender, async_receiver) = tokio::sync::oneshot::channel::<R>();

			let handle = AsyncTaskHandle::new(async_receiver);
			unsafe {
				spawn_unchecked(move || {
					let res = f();
					let _ = async_sender.send(res);
				});
			};
			handle
		}
		#[cfg(not(feature = "multi-threaded-crypto"))]
		{
			// without being able to spawn on a threadpool, we just run the function asynchronously
			async move { f() }
		}
	}
}
pub(crate) use async_scoped_task::do_cpu_intensive;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod worker_handle {
	use wasm_bindgen::prelude::*;
	use web_sys::{DedicatedWorkerGlobalScope, js_sys::Object};

	#[wasm_bindgen::prelude::wasm_bindgen]
	extern "C" {
		#[wasm_bindgen::prelude::wasm_bindgen(thread_local_v2, js_name = self)]
		static SELF: Option<Object>;
	}

	/// Handle to close the worker when dropped
	/// this is needed because wasm workers don't close automatically when all tasks are done
	/// so we hold a Weak reference to this handle in the worker thread
	/// and a strong reference for any task running on the worker
	/// when all the strong references are dropped the handle will be dropped and close the worker
	pub(super) struct WorkerHandle;

	impl Drop for WorkerHandle {
		fn drop(&mut self) {
			SELF.with(|s| {
				s.clone()
					.unwrap_throw()
					.dyn_into::<DedicatedWorkerGlobalScope>()
					.unwrap_throw()
					.close();
			})
		}
	}

	thread_local! {
		pub(super) static WORKER_HANDLE: std::cell::RefCell<std::rc::Weak<WorkerHandle>>  = const { std::cell::RefCell::new(std::rc::Weak::new()) };
	}
}

/// Why a worker spawned by `spawn_async_claiming` never started, for whatever waits on it to fail
/// with. Never set on native, where a spawned thread always starts.
#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
#[derive(Clone, Default)]
pub(crate) struct WorkerStart {
	#[cfg(any(
		test,
		all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
	))]
	failure: std::sync::Arc<std::sync::OnceLock<crate::error::WorkerStartupError>>,
}

#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
impl WorkerStart {
	#[cfg(any(
		test,
		all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
	))]
	fn reason(&self) -> Option<&crate::error::WorkerStartupError> {
		self.failure.get()
	}

	/// The error to fail a waiter with, once the spawner has given up on the worker.
	pub(crate) fn failure(&self) -> Option<crate::Error> {
		cfg_select! {
			any(
				test,
				all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
			) => {
				self.reason().cloned().map(Into::into)
			}
			_ => {
				None
			}
		}
	}
}

#[cfg(test)]
pub(crate) use worker_start::WorkerInbox;

// A web worker that never starts (its script fails to load, or it never gets as far as running its
// task) drops nothing: the closure it was handed, and every channel end inside, simply leaks. So
// whoever waits on such a worker hears silence, not an error, unless the spawner gives up on it.
#[cfg(any(
	test,
	all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
))]
mod worker_start {
	use std::{
		sync::{Mutex, PoisonError, TryLockError},
		time::Duration,
	};

	use super::WorkerStart;
	use crate::{Error, ErrorKind, error::WorkerStartupError};

	/// How long a spawned worker may take to start before it is presumed dead. Starting means
	/// fetching and compiling the SDK's wasm module again in the new worker, which takes seconds
	/// on a slow phone, never this long.
	pub(super) const WORKER_START_TIMEOUT: Duration = Duration::from_secs(30);

	/// The error for a worker `error` event, given the message it carried, if it was an
	/// `ErrorEvent` at all.
	pub(super) fn load_error(message: Option<String>) -> WorkerStartupError {
		match message {
			Some(message) if !message.is_empty() => WorkerStartupError::Load(message),
			_ => WorkerStartupError::LoadUnreported,
		}
	}

	/// A spawned worker's receiving end, held until the worker claims it. Exactly one side gets
	/// it: the worker, which then runs, or the spawner, which drops it so every sender sees the
	/// channel close and everything queued on it is dropped. A worker that starts after that
	/// finds nothing to claim and exits.
	pub(crate) struct WorkerInbox<R> {
		receiver: Mutex<Option<R>>,
		start: WorkerStart,
	}

	impl<R> WorkerInbox<R> {
		pub(crate) fn new(receiver: R) -> Self {
			Self {
				receiver: Mutex::new(Some(receiver)),
				start: WorkerStart::default(),
			}
		}

		/// Called by the worker as it starts; `None` once the spawner has given up on it.
		pub(super) fn claim(&self) -> Option<R> {
			self.receiver
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.take()
		}

		/// Gives up on a worker that has not claimed the inbox yet, recording why; `false` if it
		/// already has, or is claiming it right now. Never blocks: the spawning thread can be a
		/// page's main thread, which may not wait on a lock.
		pub(crate) fn abandon(&self, reason: WorkerStartupError) -> bool {
			let mut slot = match self.receiver.try_lock() {
				Ok(slot) => slot,
				Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
				Err(TryLockError::WouldBlock) => return false,
			};
			let Some(receiver) = slot.take() else {
				return false;
			};
			// Recorded before the receiver drops, so a sender that sees the channel close can
			// read why.
			let _ = self.start.failure.set(reason);
			drop(slot);
			drop(receiver);
			true
		}

		/// What whoever waits on the worker reads why it never started from.
		pub(crate) fn start(&self) -> &WorkerStart {
			&self.start
		}
	}

	/// The value a commander call settles with when the commander worker could not run it.
	pub(crate) trait CommanderOutput {
		fn commander_unavailable(error: Error) -> Self;
	}

	impl<T, E: From<Error>> CommanderOutput for Result<T, E> {
		fn commander_unavailable(error: Error) -> Self {
			Err(error.into())
		}
	}

	/// The error for a call whose result sender was dropped unsent, given the commander's start.
	pub(super) fn commander_unavailable_error(start: &WorkerStart) -> Error {
		start.failure().unwrap_or_else(|| {
			Error::custom(
				ErrorKind::Internal,
				"commander worker dropped a call without a result",
			)
		})
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn an_error_event_message_becomes_the_load_error() {
			assert_eq!(
				load_error(Some("SyntaxError: unexpected token".to_owned())),
				WorkerStartupError::Load("SyntaxError: unexpected token".to_owned())
			);
		}

		#[test]
		fn an_event_without_a_message_is_still_a_load_error() {
			assert_eq!(load_error(None), WorkerStartupError::LoadUnreported);
			assert_eq!(
				load_error(Some(String::new())),
				WorkerStartupError::LoadUnreported
			);
		}

		#[test]
		fn a_startup_error_says_why_the_worker_never_ran() {
			assert_eq!(
				WorkerStartupError::Spawn("SecurityError".to_owned()).to_string(),
				"worker could not be created: SecurityError"
			);
			assert_eq!(
				WorkerStartupError::Load("NetworkError".to_owned()).to_string(),
				"worker failed to load: NetworkError"
			);
			assert_eq!(
				WorkerStartupError::LoadUnreported.to_string(),
				"worker failed to load (no error details)"
			);
			assert_eq!(
				WorkerStartupError::TimedOut(WORKER_START_TIMEOUT).to_string(),
				"worker did not start within 30s"
			);
		}

		#[test]
		fn abandoning_an_unclaimed_inbox_fails_everything_queued_on_it() {
			let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
			let (result_sender, mut result_receiver) = tokio::sync::oneshot::channel::<()>();
			sender.send(result_sender).unwrap();
			let inbox = WorkerInbox::new(receiver);

			assert!(inbox.abandon(WorkerStartupError::TimedOut(WORKER_START_TIMEOUT)));

			assert!(sender.is_closed());
			assert_eq!(
				result_receiver.try_recv(),
				Err(tokio::sync::oneshot::error::TryRecvError::Closed)
			);
			assert_eq!(
				inbox.start().reason(),
				Some(&WorkerStartupError::TimedOut(Duration::from_secs(30)))
			);
		}

		#[test]
		fn a_worker_starting_after_it_was_abandoned_claims_nothing() {
			let inbox = WorkerInbox::new(());
			assert!(inbox.abandon(WorkerStartupError::LoadUnreported));
			assert_eq!(inbox.claim(), None);
		}

		#[test]
		fn a_claimed_inbox_cannot_be_abandoned() {
			let inbox = WorkerInbox::new(());
			assert_eq!(inbox.claim(), Some(()));
			assert!(!inbox.abandon(WorkerStartupError::LoadUnreported));
			assert_eq!(inbox.start().reason(), None);
		}

		#[test]
		fn an_inbox_being_claimed_is_left_to_the_worker() {
			let inbox = WorkerInbox::new(());
			let claiming = inbox.receiver.lock().unwrap();
			assert!(!inbox.abandon(WorkerStartupError::LoadUnreported));
			drop(claiming);
			assert_eq!(inbox.claim(), Some(()));
		}

		#[test]
		fn the_first_reason_to_abandon_an_inbox_is_kept() {
			let inbox = WorkerInbox::new(());
			assert!(inbox.abandon(WorkerStartupError::LoadUnreported));
			assert!(!inbox.abandon(WorkerStartupError::TimedOut(WORKER_START_TIMEOUT)));
			assert_eq!(
				inbox.start().reason(),
				Some(&WorkerStartupError::LoadUnreported)
			);
		}

		#[test]
		fn a_call_on_an_abandoned_commander_fails_with_the_startup_error() {
			let inbox = WorkerInbox::new(());
			inbox.abandon(WorkerStartupError::Load("NetworkError".to_owned()));

			let result = Result::<(), Error>::commander_unavailable(commander_unavailable_error(
				inbox.start(),
			));

			let error = result.unwrap_err();
			assert_eq!(error.kind(), ErrorKind::Internal);
			assert_eq!(
				error.downcast_ref::<WorkerStartupError>(),
				Some(&WorkerStartupError::Load("NetworkError".to_owned()))
			);
		}
	}
}

#[cfg(any(feature = "uniffi", feature = "wasm-full"))]
mod commander_thread {
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	use std::sync::OnceLock;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use std::sync::{Mutex, PoisonError};
	use std::{mem::ManuallyDrop, pin::Pin};

	use futures::Stream;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use futures::{StreamExt, stream::FuturesUnordered};

	use pin_project_lite::pin_project;

	use super::WorkerStart;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use crate::runtime::{
		spawn_async_claiming,
		worker_start::{CommanderOutput, commander_unavailable_error},
	};
	use crate::util::MaybeSend;
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	use crate::util::WasmResultExt;

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	static COMMANDER_RUNTIME_HANDLE: OnceLock<RuntimeHandle> = OnceLock::new();
	// Replaceable on wasm: a commander worker that never started is given up on, and the next
	// call spawns a fresh one.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	static COMMANDER_RUNTIME_HANDLE: Mutex<Option<RuntimeHandle>> = Mutex::new(None);

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	type TaskReceiver = tokio::sync::mpsc::UnboundedReceiver<Box<dyn FnOnceBox>>;

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	trait FnOnceBox: Send + 'static {
		fn call_box(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + 'static>>;
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	impl<F, Fut> FnOnceBox for F
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = ()> + 'static,
	{
		fn call_box(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + 'static>> {
			Box::pin((*self)())
		}
	}

	struct RuntimeHandle {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		sender: tokio::sync::mpsc::UnboundedSender<Box<dyn FnOnceBox>>,
		// Handed to every call, which reads from it why it failed if the worker never started.
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		start: WorkerStart,
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		tokio_handle: tokio::runtime::Handle,
		// Held only for its Drop: closing this oneshot signals the commander runtime
		// thread to stop (`close_receiver.await` resolves), shutting down the runtime.
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		_close_sender: tokio::sync::oneshot::Sender<()>,
	}

	impl RuntimeHandle {
		fn new() -> Self {
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				let (sender, receiver) =
					tokio::sync::mpsc::unbounded_channel::<Box<dyn FnOnceBox>>();
				let start = spawn_async_claiming("commander", receiver, run_commander);
				Self { sender, start }
			}
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				let runtime = tokio::runtime::Builder::new_multi_thread()
					.enable_all()
					.build()
					.expect_or_throw("Failed to create commander runtime");
				let handle = runtime.handle().clone();

				// Drive the in-flight hang watchdog on this runtime too. The standalone SDK
				// artifact (uniffi/wasm-full: the Notes/Chats/transfers path) installs the
				// InflightLayer but, unlike the cache, had no runtime spawning the watchdog that
				// consumes it — so its per-span bookkeeping was dead weight and the "still running
				// after Ns" hang signal never fired. `spawn_inflight_watchdog` is idempotent via a
				// process-wide guard, so this is a no-op if the cache runtime already claimed it.
				{
					let _guard = handle.enter();
					crate::obs::spawn_inflight_watchdog();
				}

				let (close_sender, close_receiver) = tokio::sync::oneshot::channel::<()>();

				std::thread::spawn(move || {
					runtime.block_on(async {
						let _ = close_receiver.await;
						tracing::debug!("Commander runtime shutting down");
					});
				});

				Self {
					tokio_handle: handle,
					_close_sender: close_sender,
				}
			}
		}

		fn build_and_spawn<F, Fut>(&self, fut_builder: CommanderFutBuilder<F, Fut, Fut::Output>)
		where
			F: FnOnce() -> Fut + Send + 'static,
			Fut: Future + MaybeSend + 'static,
			Fut::Output: Send + 'static,
		{
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				// Fails only once the worker was given up on: the task drops here, and its
				// handle reports why.
				let _ = self.sender.send(Box::new(move || {
					Box::pin(async move {
						fut_builder.build().await;
					}) as Pin<Box<dyn Future<Output = ()> + 'static>>
				}));
			}
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				self.tokio_handle
					.spawn(async move { fut_builder.build().await });
			}
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	fn get_or_init_async_runtime() -> &'static RuntimeHandle {
		COMMANDER_RUNTIME_HANDLE.get_or_init(RuntimeHandle::new)
	}

	/// Runs the commander worker's tasks until every sender is gone.
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	async fn run_commander(mut receiver: TaskReceiver) {
		let mut futures = FuturesUnordered::new();

		loop {
			tokio::select! {
				val = receiver.recv() => {
					match val {
						Some(task_constructor) => {
							// Construct the future on THIS thread
							let fut = task_constructor.call_box();
							futures.push(fut);
						},
						None => {
							while (futures.next().await).is_some() {}
							tracing::debug!("Commander worker shutting down");
							break;
						},
					}
				},
				// The guard is load-bearing: `FuturesUnordered::next()` on an EMPTY set
				// is `Ready(None)` IMMEDIATELY, so without it this select never returns
				// `Pending` once the last resident future completes — the commander then
				// busy-spins and never yields to its JS event loop, starving every
				// `spawn_local` task on this thread (and nested-worker startup, which the
				// browser runs as parent-thread event-loop tasks).
				_ = futures.next(), if !futures.is_empty() => {}
			}
		}
	}

	pin_project! {
		// `paused`/`pause_signal` back the unused-for-now pause/resume/is_paused API below; kept
		// for potential future use.
		#[allow(dead_code)]
		pub(crate) struct CommanderFutHandle<T> {
			paused: bool,
			pause_signal: tokio::sync::watch::Sender<bool>,
			cancel_signal: ManuallyDrop<tokio::sync::oneshot::Sender<()>>,
			start: WorkerStart,
			#[pin]
			result_receiver: tokio::sync::oneshot::Receiver<T>,
		}

		impl<T> PinnedDrop for CommanderFutHandle<T> {
			fn drop(mut this: Pin<&mut Self>) {
				// SAFETY: this is the only place we take the cancel signal out of the ManuallyDrop
				// drop is only ever called once, so this is safe
				#[allow(unsafe_code)]
				let cancel_signal = unsafe { ManuallyDrop::take(&mut this.cancel_signal) };
				// don't care if it errors, just means the task was already completed/dropped
				let _ = cancel_signal.send(());
			}
		}
	}

	// Unused for now: a task-pause API kept for potential future use.
	#[allow(dead_code)]
	impl<T> CommanderFutHandle<T> {
		pub(crate) fn pause(&mut self) {
			if !self.paused {
				// don't care if it errors, just means the task was already completed/dropped
				let _ = self.pause_signal.send(true);
				self.paused = true;
			}
		}

		pub(crate) fn resume(&mut self) {
			if self.paused {
				// don't care if it errors, just means the task was already completed/dropped
				let _ = self.pause_signal.send(false);
				self.paused = false;
			}
		}

		pub(crate) fn is_paused(&self) -> bool {
			self.paused
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	impl<T> Future for CommanderFutHandle<T> {
		type Output = T;

		fn poll(
			self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let this = self.project();
			this.result_receiver
				.poll(cx)
				.map(|res| res.expect_or_throw("CommanderFuture panicked"))
		}
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	impl<T: CommanderOutput> Future for CommanderFutHandle<T> {
		type Output = T;

		fn poll(
			self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let this = self.project();
			// wasm panics abort rather than unwind, so the commander drops a result sender unsent
			// only when its worker was given up on before it started.
			this.result_receiver.poll(cx).map(|res| {
				res.unwrap_or_else(|_| {
					T::commander_unavailable(commander_unavailable_error(this.start))
				})
			})
		}
	}

	pin_project! {
		struct CommanderFut<F, T>
		where
			F: Future<Output = T>,
		{
			#[pin]
			inner: F,
			#[pin]
			pause_stream: Option<tokio_stream::wrappers::WatchStream<bool>>,
			#[pin]
			cancel_signal: tokio::sync::oneshot::Receiver<()>,
			result_sender: Option<tokio::sync::oneshot::Sender<T>>,
			paused: bool,
		}
	}

	struct CommanderFutBuilder<F, Fut, T>
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = T> + 'static,
		T: Send + 'static,
	{
		future_builder: F,
		pause_stream: tokio_stream::wrappers::WatchStream<bool>,
		cancel_signal: tokio::sync::oneshot::Receiver<()>,
		result_sender: tokio::sync::oneshot::Sender<T>,
	}

	impl<F, Fut, T> CommanderFutBuilder<F, Fut, T>
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = T> + 'static,
		T: Send + 'static,
	{
		fn build(self) -> CommanderFut<Fut, T> {
			CommanderFut {
				inner: (self.future_builder)(),
				pause_stream: Some(self.pause_stream),
				cancel_signal: self.cancel_signal,
				result_sender: Some(self.result_sender),
				paused: false,
			}
		}
	}

	impl<F, T> Future for CommanderFut<F, T>
	where
		F: Future<Output = T>,
	{
		type Output = bool;

		fn poll(
			self: Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let mut this = self.project();

			// check for cancellation
			match this.cancel_signal.poll(cx) {
				std::task::Poll::Ready(_) => {
					return std::task::Poll::Ready(false);
				}
				std::task::Poll::Pending => {}
			}

			// check for pause
			if let Some(mut pause_stream) = this.pause_stream.as_mut().as_pin_mut() {
				loop {
					match pause_stream.as_mut().poll_next(cx) {
						std::task::Poll::Ready(Some(paused)) => {
							*this.paused = paused;
						}
						std::task::Poll::Ready(None) => {
							// pause signal closed, treat as unpaused
							this.pause_stream.set(None);
							*this.paused = false;
							break;
						}
						std::task::Poll::Pending => {
							break;
						}
					}
				}
			}

			if *this.paused {
				std::task::Poll::Pending
			} else {
				match this.inner.as_mut().poll(cx) {
					std::task::Poll::Ready(v) => {
						if let Some(sender) = this.result_sender.take() {
							let _ = sender.send(v);
						}
						std::task::Poll::Ready(true)
					}
					std::task::Poll::Pending => std::task::Poll::Pending,
				}
			}
		}
	}

	fn make_future_builder_with_handle<F, Fut>(
		pause_signal: Option<(
			tokio::sync::watch::Sender<bool>,
			tokio::sync::watch::Receiver<bool>,
		)>,
		fut_builder: F,
		start: WorkerStart,
	) -> (
		CommanderFutBuilder<F, Fut, Fut::Output>,
		CommanderFutHandle<Fut::Output>,
	)
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future + 'static,
		Fut::Output: Send + 'static,
	{
		let (pause_signal_tx, pause_signal_rx) =
			pause_signal.unwrap_or_else(|| tokio::sync::watch::channel(false));
		let (cancel_signal_tx, cancel_signal_rx) = tokio::sync::oneshot::channel();
		let (result_sender, result_receiver) = tokio::sync::oneshot::channel();

		let fut_builder = CommanderFutBuilder {
			future_builder: fut_builder,
			pause_stream: tokio_stream::wrappers::WatchStream::new(pause_signal_rx),
			cancel_signal: cancel_signal_rx,
			result_sender,
		};

		let handle = CommanderFutHandle {
			paused: false,
			pause_signal: pause_signal_tx,
			cancel_signal: ManuallyDrop::new(cancel_signal_tx),
			start,
			result_receiver,
		};

		(fut_builder, handle)
	}

	fn inner_do_on_commander<F, Fut>(
		pause_signal: Option<(
			tokio::sync::watch::Sender<bool>,
			tokio::sync::watch::Receiver<bool>,
		)>,
		f: F,
	) -> CommanderFutHandle<Fut::Output>
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future + MaybeSend + 'static,
		Fut::Output: Send + 'static,
	{
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			let runtime = get_or_init_async_runtime();

			let (fut_builder, handle) =
				make_future_builder_with_handle(pause_signal, f, WorkerStart::default());

			runtime.build_and_spawn(fut_builder);

			handle
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let mut slot = COMMANDER_RUNTIME_HANDLE
				.lock()
				.unwrap_or_else(PoisonError::into_inner);
			// A worker given up on dropped its inbox, closing the channel.
			if slot
				.as_ref()
				.is_some_and(|runtime| runtime.sender.is_closed())
			{
				*slot = None;
			}
			let runtime = slot.get_or_insert_with(RuntimeHandle::new);

			let (fut_builder, handle) =
				make_future_builder_with_handle(pause_signal, f, runtime.start.clone());

			runtime.build_and_spawn(fut_builder);

			handle
		}
	}

	/// Runs an async function on a dedicated 'commander' worker thread, returning the result.
	///
	/// meant to be used for wasm so that we can use do_cpu_intensive on this thread.
	/// This is because wasm doesn't allow blocking the main thread
	/// which we might need to do to prevent UB if a do_cpu_intensive future is dropped before completion.
	pub(crate) fn do_on_commander<F, Fut>(f: F) -> CommanderFutHandle<Fut::Output>
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future + MaybeSend + 'static,
		Fut::Output: Send + 'static,
	{
		inner_do_on_commander(None, f)
	}

	pub(crate) fn do_with_pause_channel_on_commander<F, Fut>(
		channel: (
			tokio::sync::watch::Sender<bool>,
			tokio::sync::watch::Receiver<bool>,
		),
		f: F,
	) -> CommanderFutHandle<Fut::Output>
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future + MaybeSend + 'static,
		Fut::Output: Send + 'static,
	{
		inner_do_on_commander(Some(channel), f)
	}
}
#[cfg(feature = "uniffi")]
pub(crate) use commander_thread::{
	CommanderFutHandle, do_on_commander, do_with_pause_channel_on_commander,
};
#[cfg(feature = "wasm-full")]
pub(crate) use commander_thread::{
	CommanderFutHandle, do_on_commander, do_with_pause_channel_on_commander,
};

#[cfg(feature = "wasm-full")]
mod wasm_threading {
	use filen_macros::js_type;
	use wasm_bindgen::prelude::*;

	use super::{
		worker_handle::{WORKER_HANDLE, WorkerHandle},
		worker_start::load_error,
	};
	use crate::error::WorkerStartupError;

	#[js_type(export, no_deser, no_default)]
	pub struct WorkerInitEvent {
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			tsify(type = "WebAssembly.Memory"),
			serde(with = "serde_wasm_bindgen::preserve")
		)]
		memory: JsValue,
		closure_ptr: usize,
		main_time_origin: f64,
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	/// The `performance.timeOrigin` of the spawning PAGE, in epoch milliseconds. Every worker this
	/// SDK spawns rebases its `performance.now()` onto this origin (see
	/// `filen-sdk-worker-thread.js`): raw `performance.now()` is relative to each context's OWN
	/// creation time, but timer and rate-limiter state (`wasmtimer`'s global wheel, `governor`'s
	/// limiter) lives in shared wasm memory and is read from every thread — mixing per-context
	/// clock domains there skews readings by the page-to-worker startup gap and stalls timers and
	/// rate limits for exactly that long.
	fn reference_time_origin() -> f64 {
		use web_sys::js_sys;

		let global = js_sys::global();
		// A worker we spawned earlier stored the page's origin at boot; hand the SAME reference
		// down so nested spawns keep normalizing to one domain.
		if let Ok(value) =
			js_sys::Reflect::get(&global, &JsValue::from_str("__filenMainTimeOrigin"))
			&& let Some(value) = value.as_f64()
		{
			return value;
		}
		js_sys::Reflect::get(&global, &JsValue::from_str("performance"))
			.ok()
			.and_then(|performance| {
				js_sys::Reflect::get(&performance, &JsValue::from_str("timeOrigin")).ok()
			})
			.and_then(|value| value.as_f64())
			.unwrap_or(0.0)
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	thread_local! {
		/// Worker wrappers retained for the life of the spawning thread: Chromium may terminate
		/// a dedicated worker whose JS wrapper is garbage-collected before the worker's script
		/// begins executing, so dropping the wrapper right after `new Worker()` races startup
		/// against the next GC. Retaining the wrapper pins only the JS object — workers still
		/// close themselves via [`WorkerHandle`] when their work ends.
		static SPAWNED_WORKERS: std::cell::RefCell<Vec<web_sys::Worker>> =
			const { std::cell::RefCell::new(Vec::new()) };
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	/// Spawns a web worker to run the given closure.
	///
	/// Currently hangs around forever unless manually terminated.
	///
	/// `on_error` gets every `error` event the worker raises, including the one for a script that
	/// failed to load.
	pub(super) fn spawn_worker(
		f: impl FnOnce() + Send + 'static,
		on_error: impl Fn(WorkerStartupError) + 'static,
	) -> Result<(), JsValue> {
		let options = web_sys::WorkerOptions::new();
		options.set_type(web_sys::WorkerType::Module);
		let worker = web_sys::Worker::new_with_options("./filen-sdk-worker-thread.js", &options)?;
		// Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` is a fat pointer.
		// But `Box<Box<dyn FnOnce()>>` is just a plain pointer, and since wasm has 32-bit pointers,
		// we can cast it to a `u32` and back.
		let ptr = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce()>));

		// Send the worker a reference to our memory chunk, so it can initialize a wasm module
		// using the same memory.
		let event = WorkerInitEvent {
			memory: wasm_bindgen::memory(),
			closure_ptr: ptr as usize,
			main_time_origin: reference_time_origin(),
		};

		let event = serde_wasm_bindgen::to_value(&event)?;
		worker.post_message(&event)?;

		// A worker that dies without unwinding (panic=abort traps, script-fetch failures) leaks
		// every channel sender it owns, so its consumers see SILENCE, not errors — this log is
		// the only direct evidence of the death (it pairs with the cache's init-ack timeout).
		// Not always an ErrorEvent: a worker whose script fails to load fires a plain Event (Firefox), and
		// reading `message` off that hands undefined to wasm, throwing inside the handler and losing the log.
		let onerror = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
			let error = load_error(
				e.dyn_ref::<web_sys::ErrorEvent>()
					.map(web_sys::ErrorEvent::message),
			);
			tracing::error!("worker startup/runtime error: {error}");
			on_error(error);
		});
		worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));
		// The handler must outlive the worker; one small leaked closure per spawn is acceptable.
		onerror.forget();
		SPAWNED_WORKERS.with_borrow_mut(|workers| workers.push(worker));

		Ok(())
	}

	#[wasm_bindgen]
	// Called by `./filen-sdk-worker-thread.js` with the closure pointer from spawn_worker.
	pub fn worker_entry_point(ptr: usize) {
		let worker_handle = std::rc::Rc::new(WorkerHandle);

		WORKER_HANDLE.with_borrow_mut(|weak_handle| {
			*weak_handle = std::rc::Rc::downgrade(&worker_handle);
		});
		// Interpret the address we were given as a pointer to a closure to call.
		#[allow(unsafe_code)]
		let closure = unsafe { Box::from_raw(ptr as *mut Box<dyn FnOnce()>) };
		(*closure)();
		std::mem::drop(worker_handle);
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn spawn_local_on_worker(f: impl Future<Output = ()> + 'static) {
	let maybe_handle =
		worker_handle::WORKER_HANDLE.with_borrow(|weak_handle| weak_handle.upgrade());
	wasm_bindgen_futures::spawn_local(async move {
		f.await;
		std::mem::drop(maybe_handle);
	});
}

#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
pub fn spawn<F>(f: F)
where
	F: FnOnce() + Send + 'static,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		use wasm_bindgen::UnwrapThrowExt;
		wasm_threading::spawn_worker(f, |_| {}).expect_throw("Failed to spawn worker");
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		std::thread::spawn(f);
	}
}

#[derive(Debug)]
pub(crate) struct SpawnTaskHandle<T> {
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	receiver: tokio::sync::oneshot::Receiver<T>,
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	handle: tokio::task::JoinHandle<T>,
}

impl<T> SpawnTaskHandle<T> {
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) fn new(handle: tokio::task::JoinHandle<T>) -> Self {
		Self { handle }
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	pub(crate) fn new(receiver: tokio::sync::oneshot::Receiver<T>) -> Self {
		Self { receiver }
	}
}

impl<T> std::future::Future for SpawnTaskHandle<T> {
	type Output = T;

	fn poll(
		mut self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Self::Output> {
		cfg_select! {
			all(target_family = "wasm", target_os = "unknown") => {
				self.as_mut().receiver
					.poll_unpin(cx)
					.map(|res| res.expect("Spawned task panicked"))
			}
			_ => {
				self.as_mut().handle.poll_unpin(cx)
					.map(|res| res.expect("Spawned task panicked"))
			}
		}
	}
}

pub(crate) fn spawn_task_maybe_send<F, T>(f: F) -> SpawnTaskHandle<T>
where
	F: Future<Output = T> + MaybeSend + 'static,
	T: 'static + MaybeSend,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		let (sender, receiver) = tokio::sync::oneshot::channel();
		spawn_local_on_worker(async move {
			let _ = sender.send(f.await);
		});

		SpawnTaskHandle::new(receiver)
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		SpawnTaskHandle::new(tokio::spawn(f))
	}
}

#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
pub fn spawn_async<F, Fut>(f: F)
where
	F: FnOnce() -> Fut + Send + 'static,
	Fut: Future<Output = ()> + 'static,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		use wasm_bindgen::UnwrapThrowExt;
		wasm_threading::spawn_worker(
			|| {
				spawn_local_on_worker(f());
			},
			|_| {},
		)
		.expect_throw("Failed to spawn worker");
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		std::thread::spawn(move || {
			let runtime = tokio::runtime::Builder::new_current_thread()
				.enable_all()
				.build()
				.expect("failed to create websocket runtime");

			runtime.block_on(f());
		});
	}
}

/// [`spawn_async`] for a worker that is waited on through `input`, typically the receiving end of
/// its request channel: `f` gets `input` only once the worker runs. On wasm a worker that cannot
/// be created, fails to load or has not started within `WORKER_START_TIMEOUT` is given up on
/// instead, and `input` dropped, so its senders see the channel close; the returned
/// [`WorkerStart`] says why. `name` labels the worker in wasm logs.
#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
pub(crate) fn spawn_async_claiming<R, F, Fut>(name: &'static str, input: R, f: F) -> WorkerStart
where
	R: Send + 'static,
	F: FnOnce(R) -> Fut + Send + 'static,
	Fut: Future<Output = ()> + 'static,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		use std::sync::Arc;

		use worker_start::{WORKER_START_TIMEOUT, WorkerInbox};

		use crate::error::WorkerStartupError;

		fn give_up<R>(name: &str, inbox: &WorkerInbox<R>, reason: WorkerStartupError) {
			if inbox.abandon(reason)
				&& let Some(reason) = inbox.start().reason()
			{
				tracing::error!(
					"giving up on the {name} worker, failing what waits on it: {reason}"
				);
			}
		}

		let inbox = Arc::new(WorkerInbox::new(input));
		let start = inbox.start().clone();
		let worker_inbox = Arc::clone(&inbox);
		let error_inbox = Arc::clone(&inbox);

		let spawned = wasm_threading::spawn_worker(
			move || {
				let Some(input) = worker_inbox.claim() else {
					tracing::warn!("{name} worker started after it was given up on; exiting");
					return;
				};
				spawn_local_on_worker(f(input));
			},
			move |error| give_up(name, &error_inbox, error),
		);
		match spawned {
			Ok(()) => spawn_local_on_worker(async move {
				// Not `util::sleep`: this runs on the spawning thread, the page's main thread for
				// the commander, and wasmtimer's timer callback locks a mutex every worker shares,
				// which is an `Atomics.wait` the page forbids. This is one `setTimeout`.
				futures_timer::Delay::new(WORKER_START_TIMEOUT).await;
				give_up(
					name,
					&inbox,
					WorkerStartupError::TimedOut(WORKER_START_TIMEOUT),
				);
			}),
			Err(e) => give_up(name, &inbox, WorkerStartupError::Spawn(format!("{e:?}"))),
		}
		start
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		let _ = name;
		spawn_async(move || f(input));
		WorkerStart::default()
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub fn spawn_local<F>(f: F)
where
	F: Future<Output = ()> + 'static,
{
	spawn_local_on_worker(f);
}

/// A macro to run blocking code in parallel using rayon's thread pool by nesting [rayon::join].
///
/// I want to make a generic version of this but I want to expand the left side before the right side
/// which I'm not sure how to do in macros while keeping the order of the returned tuple the same
/// so for now this only supports up to 4 expressions.
#[cfg(feature = "multi-threaded-crypto")]
macro_rules! blocking_join {
	($e:expr) => {
		$e
	};

	($e1:expr, $e2:expr) => {
		rayon::join($e1, $e2)
	};

	($e1:expr, $e2:expr, $e3:expr) => {{
		let ((a, b), c) = rayon::join(|| rayon::join($e1, $e2), $e3);
		(a, b, c)
	}};

	($e1:expr, $e2:expr, $e3:expr, $e4:expr) => {{
		let (((a, b), c), d) = rayon::join(|| rayon::join(|| rayon::join($e1, $e2), $e3), $e4);
		(a, b, c, d)
	}};
}

// Fallback implementation that just runs the expressions sequentially
#[cfg(not(feature = "multi-threaded-crypto"))]
macro_rules! blocking_join {
	($e:expr) => {
		$e
	};

	($e1:expr, $e2:expr) => {
		($e1(), $e2())
	};

	($e1:expr, $e2:expr, $e3:expr) => {
		($e1(), $e2(), $e3())
	};

	($e1:expr, $e2:expr, $e3:expr, $e4:expr) => {
		($e1(), $e2(), $e3(), $e4())
	};
}

pub(crate) use blocking_join;

#[cfg(all(
	test,
	feature = "multi-threaded-crypto",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
mod tests {
	use std::{
		sync::atomic::{AtomicBool, Ordering},
		time::Duration,
	};

	use super::do_cpu_intensive;

	// The dropped handle must block until the rayon task is done with the
	// caller-stack data it borrows (`data`/`finished`) — on every runtime flavor
	// and outside any runtime.
	fn assert_drop_in_flight_blocks_until_task_finishes() {
		let started = AtomicBool::new(false);
		let finished = AtomicBool::new(false);
		let data = vec![1u8, 2, 3];
		let data_ref = &data;
		let (started_ref, finished_ref) = (&started, &finished);
		let handle = do_cpu_intensive(move || {
			started_ref.store(true, Ordering::SeqCst);
			std::thread::sleep(Duration::from_millis(200));
			let sum: u32 = data_ref.iter().map(|&b| u32::from(b)).sum();
			finished_ref.store(true, Ordering::SeqCst);
			sum
		});
		while !started.load(Ordering::SeqCst) {
			std::thread::sleep(Duration::from_millis(1));
		}
		drop(handle);
		assert!(finished.load(Ordering::SeqCst));
	}

	#[test]
	fn drop_in_flight_on_current_thread_runtime_waits_without_panic() {
		let runtime = tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap();
		runtime.block_on(async {
			assert_drop_in_flight_blocks_until_task_finishes();
		});
	}

	#[test]
	fn drop_in_flight_on_multi_thread_runtime_waits_without_panic() {
		let runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(1)
			.build()
			.unwrap();
		runtime.block_on(async {
			assert_drop_in_flight_blocks_until_task_finishes();
		});
	}

	#[test]
	fn drop_in_flight_outside_runtime_waits_without_panic() {
		assert_drop_in_flight_blocks_until_task_finishes();
	}

	// A host application may drive SDK futures with `futures::executor::block_on`; a drop
	// mid-poll then runs while that executor's thread-local re-entrancy guard is set, where a
	// nested `futures::executor::block_on` would panic and skip the mandatory wait.
	#[test]
	fn drop_in_flight_inside_futures_executor_waits_without_panic() {
		futures::executor::block_on(async {
			assert_drop_in_flight_blocks_until_task_finishes();
		});
	}

	// A multi-thread handle entered on a current-thread `block_on` thread: the innermost
	// registered handle reports `MultiThread`, but blocking via `block_in_place` still panics
	// in this context — the drop wait must not rely on the registered handle's flavor.
	#[test]
	fn drop_in_flight_with_foreign_multi_thread_handle_entered_waits_without_panic() {
		let multi_thread_runtime = tokio::runtime::Builder::new_multi_thread()
			.worker_threads(1)
			.build()
			.unwrap();
		let multi_thread_handle = multi_thread_runtime.handle().clone();
		let current_thread_runtime = tokio::runtime::Builder::new_current_thread()
			.build()
			.unwrap();
		current_thread_runtime.block_on(async {
			let _guard = multi_thread_handle.enter();
			assert_drop_in_flight_blocks_until_task_finishes();
		});
	}
}
