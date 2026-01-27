#[cfg(feature = "wasm-full")]
pub use wasm_bindgen_rayon::init_thread_pool;

use crate::util::MaybeSend;

mod async_scoped_task {
	use std::mem::ManuallyDrop;

	struct AsyncTaskHandle<T> {
		async_receiver: ManuallyDrop<tokio::sync::oneshot::Receiver<T>>,
	}

	impl<T> Drop for AsyncTaskHandle<T> {
		fn drop(&mut self) {
			// SAFETY: we are taking the receiver out of the ManuallyDrop
			// we do this exactly once, in the drop impl, so it's safe
			let mut async_receiver = unsafe { ManuallyDrop::take(&mut self.async_receiver) };

			match async_receiver.try_recv() {
				Ok(_) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {}
				Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
					log::debug!(
						"AsyncTaskHandle being dropped before completion, blocking current thread to avoid UB"
					);
					#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
					{
						tokio::task::block_in_place(|| {
							let _ = async_receiver.blocking_recv();
						})
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

	/// Spawns a closure on the rayon threadpool without requiring 'static lifetime.
	///
	/// # Safety
	/// The caller must guarantee that the closure does not outlive any references it captures.
	#[cfg(feature = "multi-threaded-crypto")]
	unsafe fn spawn_unchecked<F>(f: F)
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
			let (async_sender, async_receiver) = tokio::sync::oneshot::channel::<R>();

			let handle = AsyncTaskHandle {
				async_receiver: ManuallyDrop::new(async_receiver),
			};
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

#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
mod commander_thread {
	use std::{mem::ManuallyDrop, pin::Pin, sync::OnceLock};

	use futures::Stream;

	use pin_project_lite::pin_project;

	use crate::util::{MaybeSend, WasmResultExt};

	// Sender for commander worker tasks
	static COMMANDER_RUNTIME_HANDLE: OnceLock<RuntimeHandle> = OnceLock::new();

	trait FnOnceBox: Send + 'static {
		fn call_box(self: Box<Self>) -> Pin<Box<dyn Future<Output = ()> + 'static>>;
	}

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
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		tokio_handle: tokio::runtime::Handle,
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		close_sender: tokio::sync::oneshot::Sender<()>,
	}

	struct JoinHandle {}

	impl RuntimeHandle {
		fn new() -> Self {
			#[cfg(all(target_family = "wasm", target_os = "unknown"))]
			{
				use super::{spawn, spawn_local};
				use futures::{StreamExt, stream::FuturesUnordered};

				let (sender, mut receiver) =
					tokio::sync::mpsc::unbounded_channel::<Box<dyn FnOnceBox>>();

				spawn(move || {
					spawn_local(async move {
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
											log::debug!("Commander worker shutting down");
											break;
										},
									}
								},
								_ = futures.next() => {}
							}
						}
					});
				});

				Self { sender }
			}
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				let runtime = tokio::runtime::Builder::new_multi_thread()
					.enable_all()
					.build()
					.expect_or_throw("Failed to create commander runtime");
				let handle = runtime.handle().clone();

				let (close_sender, close_receiver) = tokio::sync::oneshot::channel::<()>();

				std::thread::spawn(move || {
					runtime.block_on(async {
						let _ = close_receiver.await;
						log::debug!("Commander runtime shutting down");
					});
				});

				Self {
					tokio_handle: handle,
					close_sender,
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
				self.sender
					.send(Box::new(move || {
						Box::pin(async move {
							fut_builder.build().await;
						}) as Pin<Box<dyn Future<Output = ()> + 'static>>
					}))
					.expect_or_throw("Failed to send task to commander worker");
			}
			#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
			{
				self.tokio_handle
					.spawn(async move { fut_builder.build().await });
			}
		}
	}

	fn get_or_init_async_runtime() -> &'static RuntimeHandle {
		COMMANDER_RUNTIME_HANDLE.get_or_init(RuntimeHandle::new)
	}

	pin_project! {
		pub(crate) struct CommanderFutHandle<T> {
			paused: bool,
			pause_signal: tokio::sync::watch::Sender<bool>,
			cancel_signal: ManuallyDrop<tokio::sync::oneshot::Sender<()>>,
			#[pin]
			result_receiver: tokio::sync::oneshot::Receiver<T>,
		}

		impl<T> PinnedDrop for CommanderFutHandle<T> {
			fn drop(mut this: Pin<&mut Self>) {
				// SAFETY: this is the only place we take the cancel signal out of the ManuallyDrop
				// drop is only ever called once, so this is safe
				let cancel_signal = unsafe { ManuallyDrop::take(&mut this.cancel_signal) };
				// don't care if it errors, just means the task was already completed/dropped
				let _ = cancel_signal.send(());
			}
		}
	}

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
		let runtime = get_or_init_async_runtime();

		let (fut_builder, handle) = make_future_builder_with_handle(pause_signal, f);

		runtime.build_and_spawn(fut_builder);

		handle
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
	use serde::Serialize;
	use wasm_bindgen::prelude::*;

	use super::worker_handle::{WORKER_HANDLE, WorkerHandle};

	#[derive(Serialize, tsify::Tsify)]
	#[tsify(into_wasm_abi)]
	#[serde(rename_all = "camelCase")]
	pub struct WorkerInitEvent {
		#[serde(with = "serde_wasm_bindgen::preserve")]
		#[cfg_attr(
			all(target_family = "wasm", target_os = "unknown"),
			tsify(type = "WebAssembly.Memory")
		)]
		memory: JsValue,
		closure_ptr: usize,
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	/// Spawns a web worker to run the given closure.
	///
	/// Currently hangs around forever unless manually terminated.
	pub(super) fn spawn_worker(f: impl FnOnce() + Send + 'static) -> Result<(), JsValue> {
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
		};

		let event = serde_wasm_bindgen::to_value(&event)?;
		worker.post_message(&event)?;

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
		wasm_threading::spawn_worker(f).expect_throw("Failed to spawn worker");
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

	pub(crate) fn is_finished(&self) -> bool {
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			!self.receiver.is_empty()
		}
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			self.handle.is_finished()
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
			if sender.send(f.await).is_err() {
				panic!("receiver closed");
			}
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
		wasm_threading::spawn_worker(|| {
			spawn_local_on_worker(f());
		})
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

#[cfg(not(all(
	target_family = "wasm",
	target_os = "unknown",
	not(feature = "wasm-full")
)))]
pub fn spawn_local<F>(f: F)
where
	F: Future<Output = ()> + 'static,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		spawn_local_on_worker(f);
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		std::mem::drop(f);
		panic!("spawn_local is only currently supported on wasm targets");
	}
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
