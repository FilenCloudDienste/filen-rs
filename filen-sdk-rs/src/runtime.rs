use futures::stream::FuturesUnordered;
use std::mem::ManuallyDrop;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasm_bindgen::prelude::*;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use wasm_bindgen_rayon::init_thread_pool;
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use web_sys::js_sys::Int32Array;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod atomics {
	use super::*;
	#[wasm_bindgen]
	extern "C" {
		#[wasm_bindgen(js_namespace = Atomics)]
		pub(super) fn wait(
			typed_array: &Int32Array,
			index: u32,
			value: i32,
		) -> wasm_bindgen::JsValue;

		#[wasm_bindgen(js_namespace = Atomics)]
		pub(super) fn store(typed_array: &Int32Array, index: u32, value: i32) -> i32;

		#[wasm_bindgen(js_namespace = Atomics)]
		pub(super) fn notify(typed_array: &Int32Array, index: u32) -> u32;
	}
}

/// Runs a CPU intensive function on a separate thread, returning a future that resolves to the result.
///
/// IMPORTANT: Requires that this future is NEVER forgotten, or it can cause UB.
/// Will block the current thread if dropped before completion.
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

/// Runs an async function on a separate worker thread, returning the result.
///
/// meant to be used for wasm so that we can use do_cpu_intensive on this thread.
/// This is because wasm doesn't allow blocking the main thread
/// which we might need to do to prevent UB if a do_cpu_intensive future is dropped before completion.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
async fn do_on_worker<F, R>(f: F) -> R
where
	F: AsyncFnOnce() -> R + Send + 'static,
	R: Send + 'static,
{
	let (result_sender, result_receiver) = tokio::sync::oneshot::channel::<R>();

	struct WorkerTerminator {
		worker: web_sys::Worker,
	}

	impl Drop for WorkerTerminator {
		fn drop(&mut self) {
			self.worker.terminate();
		}
	}
	// bad practice but works for now
	// in the future I want to have a dedicated single worker thread for this
	let worker = spawn(move || {
		wasm_bindgen_futures::spawn_local(async {
			let res = f().await;
			let _ = result_sender.send(res);
		});
	})
	.unwrap_throw();

	let _worker_terminator = WorkerTerminator { worker };

	result_receiver.await.expect_throw("Worker thread panicked")
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
/// Spawns a web worker to run the given closure.
///
/// Currently hangs around forever unless manually terminated.
fn spawn(f: impl FnOnce() + Send + 'static) -> Result<web_sys::Worker, JsValue> {
	let options = web_sys::WorkerOptions::new();
	options.set_type(web_sys::WorkerType::Module);
	let worker = web_sys::Worker::new_with_options("./worker.js", &options)?;
	// Double-boxing because `dyn FnOnce` is unsized and so `Box<dyn FnOnce()>` is a fat pointer.
	// But `Box<Box<dyn FnOnce()>>` is just a plain pointer, and since wasm has 32-bit pointers,
	// we can cast it to a `u32` and back.
	let ptr = Box::into_raw(Box::new(Box::new(f) as Box<dyn FnOnce()>));
	let msg = web_sys::js_sys::Array::new();
	// Send the worker a reference to our memory chunk, so it can initialize a wasm module
	// using the same memory.
	msg.push(&wasm_bindgen::memory());
	// Also send the worker the address of the closure we want to execute.
	msg.push(&JsValue::from(ptr as usize));
	worker.post_message(&msg)?;

	Ok(worker)
}

#[wasm_bindgen]
// This function is here for `worker.js` to call.
pub fn worker_entry_point(ptr: usize) {
	// Interpret the address we were given as a pointer to a closure to call.
	let closure = unsafe { Box::from_raw(ptr as *mut Box<dyn FnOnce()>) };
	(*closure)();
}

pub(crate) fn spawn_thread<F>(f: F)
where
	F: FnOnce() + Send + 'static,
{
	spawn(f);
}

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

pub(crate) use blocking_join;

use crate::crypto::shared::{CreateRandom, DataCrypter};

// this does bad things
struct AsyncTaskHandle<T> {
	async_receiver: ManuallyDrop<tokio::sync::oneshot::Receiver<T>>,
}

impl<T> Drop for AsyncTaskHandle<T> {
	fn drop(&mut self) {
		let mut async_receiver = unsafe { ManuallyDrop::take(&mut self.async_receiver) };

		// we close first on wasm so that there's no time of check time of use issues
		// with try_recv and the worker trying to send after we've checked but before we wait
		// since we don't use this channel in the blocking wait on wasm
		// #[cfg(all(target_family = "wasm", target_os = "unknown"))]
		// async_receiver.close();

		match async_receiver.try_recv() {
			Ok(_) => {
				log::info!("AsyncTaskHandle dropped after task completion, didn't get result");
			}
			Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
				log::info!("AsyncTaskHandle dropped after task completion");
			}
			Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
				#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
				{
					log::debug!(
						"AsyncTaskHandle being dropped before completion, blocking current thread to avoid UB"
					);
					tokio::task::block_in_place(|| {
						let _ = async_receiver.blocking_recv();
					})
				}
				#[cfg(all(target_family = "wasm", target_os = "unknown"))]
				{
					log::warn!(
						"AsyncTaskHandle being dropped before completion, blocking current thread to avoid UB",
					);
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

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
pub async fn test_async_drop() {
	do_on_worker(async || {
		let (sender, receiver) = std::sync::mpsc::channel::<String>();
		{
			log::info!("Creating future");

			let fut = do_cpu_intensive(|| {
				log::info!("Starting async drop test");
				let key = crate::crypto::v3::EncryptionKey::generate();
				let mut data = vec![0u8; 1024 * 100];
				key.blocking_encrypt_data(&mut data).unwrap();
				sender
					.send("Hello from async drop test".to_string())
					.unwrap();

				log::info!("Finished async drop test");
				42
			});
			log::info!("Created future, awaiting");
			let res = fut.await;
			log::info!("Future completed with result: {}", res);
			log::info!("Future completed : {})", receiver.recv().unwrap());
		}

		let (sender, receiver) = std::sync::mpsc::channel::<String>();
		{
			log::info!("Creating future");

			let _fut = do_cpu_intensive(|| {
				log::info!("Starting async drop test");
				let key = crate::crypto::v3::EncryptionKey::generate();
				let mut data = vec![0u8; 1024 * 100];
				key.blocking_encrypt_data(&mut data).unwrap();
				sender
					.send("Hello from async drop test".to_string())
					.unwrap();

				log::info!("Finished async drop test");
				42
			});
			log::info!("Created future, dropping");
			// fut.await;
		}

		log::info!("Dropped future, waiting for message");

		log::info!("Received message: {}", receiver.recv().unwrap());

		log::info!("Dropped future, did we panic?");
	})
	.await;

	for i in 0..20 {
		do_on_worker(async move || {
			log::info!("Spawned worker sequential {}", i);
		})
		.await
	}

	let handles: FuturesUnordered<_> = (0..20)
		.map(|i| {
			do_on_worker(async move || {
				log::info!("Spawned worker parallel {}", i);
			})
		})
		.collect();
	futures::StreamExt::collect::<Vec<()>>(handles).await;
}
