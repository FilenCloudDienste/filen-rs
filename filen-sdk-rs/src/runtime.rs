#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use wasm_bindgen_rayon::init_thread_pool;

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
		let (async_sender, async_receiver) = tokio::sync::oneshot::channel::<R>();

		let handle = AsyncTaskHandle {
			async_receiver: ManuallyDrop::new(async_receiver),
		};

		unsafe {
			rayon::spawn_unchecked(move || {
				let res = f();
				let _ = async_sender.send(res);
			});
		};

		handle
	}
}
pub(crate) use async_scoped_task::do_cpu_intensive;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod worker_handle {
	use wasm_bindgen::prelude::*;
	use web_sys::{DedicatedWorkerGlobalScope, js_sys::Object};

	#[wasm_bindgen]
	extern "C" {
		#[wasm_bindgen(thread_local_v2, js_name = self)]
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

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod commander_thread {
	use std::{pin::Pin, sync::OnceLock};

	use futures::stream::FuturesUnordered;
	use tokio::sync::mpsc::UnboundedSender;
	use wasm_bindgen::UnwrapThrowExt;

	use crate::auth::JsClient;

	use super::{spawn_local, wasm_threading::spawn_worker};

	// Sender for commander worker tasks
	static COMMANDER_WORKER_SENDER: OnceLock<UnboundedSender<Box<dyn FnOnceBox>>> = OnceLock::new();

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

	fn get_or_init_commander_sender()
	-> &'static tokio::sync::mpsc::UnboundedSender<Box<dyn FnOnceBox>> {
		COMMANDER_WORKER_SENDER.get_or_init(|| {
			let (sender, mut receiver) =
				tokio::sync::mpsc::unbounded_channel::<Box<dyn FnOnceBox>>();

			spawn_worker(move || {
				spawn_local(async move {
					let mut futures = FuturesUnordered::new();

					loop {
						use futures::StreamExt;

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
										break;
									},
								}
							},
							_ = futures.next() => {}
						}
					}
				});
			})
			.expect_throw("Failed to spawn commander worker");

			sender
		})
	}
	/// Runs an async function on a dedicated 'commander' worker thread, returning the result.
	///
	/// meant to be used for wasm so that we can use do_cpu_intensive on this thread.
	/// This is because wasm doesn't allow blocking the main thread
	/// which we might need to do to prevent UB if a do_cpu_intensive future is dropped before completion.
	pub(crate) async fn do_on_commander<F, Fut, R>(f: F) -> R
	where
		F: FnOnce() -> Fut + Send + 'static,
		Fut: Future<Output = R> + 'static,
		R: Send + 'static,
	{
		let sender = get_or_init_commander_sender();

		let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<R>();

		sender
			.send(Box::new(move || {
				Box::pin(async move {
					let res = f().await;
					let _ = result_sender.send(res);
				}) as Pin<Box<dyn Future<Output = ()> + 'static>>
			}))
			.expect_throw("Failed to send task to commander worker");

		result_receiver
			.await
			.expect_throw("Worker thread dropped task result")
	}

	impl JsClient {}
}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use commander_thread::do_on_commander;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod wasm_threading {
	use serde::Serialize;
	use wasm_bindgen::prelude::*;

	use super::worker_handle::{WORKER_HANDLE, WorkerHandle};

	#[derive(Serialize, tsify::Tsify)]
	#[tsify(into_wasm_abi)]
	#[serde(rename_all = "camelCase")]
	pub struct WorkerInitEvent {
		#[serde(with = "serde_wasm_bindgen::preserve")]
		#[tsify(type = "WebAssembly.Memory")]
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

	pub(super) fn spawn_local_on_worker(f: impl Future<Output = ()> + 'static) {
		let maybe_handle = WORKER_HANDLE.with_borrow(|weak_handle| weak_handle.upgrade());
		wasm_bindgen_futures::spawn_local(async move {
			f.await;
			std::mem::drop(maybe_handle);
		});
	}
}

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

pub fn spawn_local<F>(f: F)
where
	F: Future<Output = ()> + 'static,
{
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	{
		wasm_threading::spawn_local_on_worker(f);
	}
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	{
		#[allow(clippy::let_underscore_future)]
		let _ = f;
		panic!("spawn_local is only currently supported on wasm targets");
	}
}

/// A macro to run blocking code in parallel using rayon's thread pool by nesting [rayon::join].
///
/// I want to make a generic version of this but I want to expand the left side before the right side
/// which I'm not sure how to do in macros while keeping the order of the returned tuple the same
/// so for now this only supports up to 4 expressions.
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
	}}; // ($($rest:expr),+ $e:expr) => {{
	    // 	let (left, right) = rayon::join(|| blocking_join!($($rest),+), $e);
	    // 	(left, right)
	    // }};
}

pub(crate) use blocking_join;
