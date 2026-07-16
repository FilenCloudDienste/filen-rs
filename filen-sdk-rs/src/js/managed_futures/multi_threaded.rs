pub use managed::ManagedFuture;
pub use pausable::{PauseSignal, PauseSignalRust};

mod pausable {
	// Heavily inspired by
	// https://github.com/xuxiaocheng0201/pausable_future
	use std::{cell::OnceCell, future::Future};

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use wasm_bindgen::{JsCast, JsValue};
	use web_sys::js_sys;

	use crate::{
		Error, ErrorKind,
		runtime::{self, CommanderFutHandle},
	};

	pin_project! {
		pub(super) struct Pausable<F> where F: Future {
			#[pin]
			fut: F,
			signal: Option<PauseSignalRust>,
		}
	}

	#[derive(Deserialize, Default)]
	#[serde(transparent)]
	// unfortunately we have to call this PauseSignal, instead of PauseSignalJS,
	// because Tsify does not support renaming the struct
	// and we need to export the Rust PauseSignal struct as PauseSignal in JS
	pub struct PauseSignal(#[serde(with = "serde_wasm_bindgen::preserve")] JsValue);

	impl PauseSignal {
		pub(super) fn into_pausable_on_commander<F, Fut>(
			self,
			fut_builder: F,
		) -> Result<CommanderFutHandle<Fut::Output>, Error>
		where
			F: FnOnce() -> Fut + Send + 'static,
			Fut: Future + 'static,
			Fut::Output: Send + 'static,
		{
			if self.0.is_undefined() {
				Ok(runtime::do_on_commander(fut_builder))
			} else {
				let pause_signal =
					PauseSignalRust::get_ref_from_js_value(&self.0).map_err(|e| {
						let ty = JsValue::dyn_ref::<js_sys::JsString>(&e.js_typeof())
							.map(|s| format!("{}", s))
							.unwrap_or_else(|| "unknown".to_string());
						Error::custom(
							ErrorKind::Conversion,
							format!("expected PauseSignal, got {}", ty),
						)
					})?;
				Ok(runtime::do_with_pause_channel_on_commander(
					(pause_signal.sender.clone(), pause_signal.receiver.clone()),
					fut_builder,
				))
			}
		}
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "PauseSignal")]
	#[derive(Clone)]
	pub struct PauseSignalRust {
		sender: tokio::sync::watch::Sender<bool>,
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	impl Default for PauseSignalRust {
		fn default() -> Self {
			let (sender, receiver) = tokio::sync::watch::channel(false);
			Self { sender, receiver }
		}
	}

	thread_local! {
		static WBG_PTR: OnceCell<JsValue> = const { OnceCell::new() };
	}

	impl PauseSignalRust {
		fn get_ref_from_js_value(
			value: &wasm_bindgen::JsValue,
		) -> Result<wasm_bindgen::__rt::RcRef<Self>, JsValue> {
			let ptr = {
				WBG_PTR.with(|v| {
					js_sys::Reflect::get(value, v.get_or_init(|| JsValue::from_str("__wbg_ptr")))
				})
			}?;
			let ptr = ptr.as_f64().map_or(0, |ptr| ptr as u32);
			if ptr == 0 {
				Err(value.clone())
			} else {
				let rc_ref =
					unsafe { <Self as wasm_bindgen::convert::RefFromWasmAbi>::ref_from_abi(ptr) };
				Ok(rc_ref)
			}
		}
	}

	#[wasm_bindgen::prelude::wasm_bindgen(js_class = "PauseSignal")]
	impl PauseSignalRust {
		#[wasm_bindgen::prelude::wasm_bindgen(constructor)]
		pub fn new() -> Self {
			Self::default()
		}

		#[wasm_bindgen::prelude::wasm_bindgen(js_name = "isPaused")]
		pub fn is_paused(&self) -> bool {
			*self.receiver.borrow()
		}

		#[wasm_bindgen]
		pub fn pause(&self) {
			let _ = self.sender.send(true);
		}

		#[wasm_bindgen]
		pub fn resume(&self) {
			let _ = self.sender.send(false);
		}
	}
}

mod abortable {
	use std::{borrow::Cow, task::Poll};

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use wasm_bindgen::{JsCast, JsValue, prelude::Closure};
	use web_sys::{AbortSignal as WasmAbortSignal, js_sys::JsString};

	use crate::{Error, ErrorKind, error::AbortedError};

	#[derive(Deserialize, Default)]
	#[serde(transparent)]
	pub struct AbortSignal(#[serde(with = "serde_wasm_bindgen::preserve")] JsValue);

	/// Removes the registered `abort` listener when the operation's future ends (whether it
	/// completed, was cancelled, or the abort fired). Removing the listener before the `Closure`
	/// backing it is freed prevents a later `abort()` from invoking a dropped closure, and keeps
	/// a shared signal usable by other concurrent operations.
	struct AbortListenerGuard {
		signal: WasmAbortSignal,
		closure: Closure<dyn FnMut()>,
	}

	impl Drop for AbortListenerGuard {
		fn drop(&mut self) {
			let _ = self.signal.remove_event_listener_with_callback(
				"abort",
				self.closure.as_ref().unchecked_ref(),
			);
		}
	}

	impl AbortSignal {
		pub(super) fn into_future(
			self,
		) -> Result<AbortSignalFuture<impl Future<Output = AbortedError>>, Error> {
			if self.0.is_undefined() {
				Ok(AbortSignalFuture::None)
			} else {
				let signal: WasmAbortSignal = self.0.dyn_into().map_err(|e| {
					let ty = JsValue::dyn_ref::<JsString>(&e.js_typeof())
						.map(|s| Cow::Owned(String::from(s)))
						.unwrap_or(Cow::Borrowed("unknown"));
					Error::custom(
						ErrorKind::Conversion,
						format!("expected AbortSignal, got {}", ty),
					)
				})?;

				let (sender, receiver) = tokio::sync::oneshot::channel::<()>();
				let mut sender = Some(sender);
				// Register via `add_event_listener` rather than `set_onabort`: `set_onabort`
				// replaces any existing handler, so fanning one AbortSignal out to two concurrent
				// SDK operations (or a user-set `signal.onabort`) would clobber the others and
				// leave them uncancellable. The guard removes this exact listener when the
				// operation ends, so a late `abort()` can never call into a freed closure.
				let closure = Closure::<dyn FnMut()>::new(move || {
					if let Some(sender) = sender.take() {
						let _ = sender.send(());
					}
				});
				signal
					.add_event_listener_with_callback("abort", closure.as_ref().unchecked_ref())
					.map_err(|e| {
						Error::custom(
							ErrorKind::Conversion,
							format!("failed to register AbortSignal listener: {e:?}"),
						)
					})?;
				let guard = AbortListenerGuard { signal, closure };
				Ok(AbortSignalFuture::Some {
					fut: async move {
						if guard.signal.aborted() {
							tracing::debug!("AbortSignal already aborted, returning AbortedError");
							return AbortedError;
						}
						// Hold the guard for the operation's lifetime so the listener stays live
						// and is then cleanly removed on drop.
						let _guard = guard;
						let _ = receiver.await;
						tracing::debug!("AbortSignal aborted, returning AbortedError");
						AbortedError
					},
				})
			}
		}
	}

	pin_project! {
		#[project = AbortSignalFutureProj]
		pub(super) enum AbortSignalFuture<F> {
			None,
			Some{#[pin] fut: F},
		}
	}

	impl<F> Future for AbortSignalFuture<F>
	where
		F: Future<Output = AbortedError>,
	{
		type Output = AbortedError;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let this = self.project();
			match this {
				AbortSignalFutureProj::None => Poll::Pending,
				AbortSignalFutureProj::Some { fut: pinned } => pinned.poll(cx),
			}
		}
	}

	// Runs under `wasm-bindgen-test` on wasm32 (e.g. `wasm-pack test --node`), against the real
	// browser/Node `AbortController`/`AbortSignal`.
	#[cfg(all(test, target_family = "wasm", target_os = "unknown"))]
	mod tests {
		use std::{future::Future, time::Duration};

		use futures::future::{Either, select};
		use wasm_bindgen::JsValue;
		use wasm_bindgen_test::wasm_bindgen_test;

		use super::AbortSignal;
		use crate::error::AbortedError;

		/// Yields control once, so futures polled alongside this one reach their first `.await`
		/// point before the next statement runs.
		async fn yield_once() {
			let mut yielded = false;
			std::future::poll_fn(move |cx| {
				if yielded {
					std::task::Poll::Ready(())
				} else {
					yielded = true;
					cx.waker().wake_by_ref();
					std::task::Poll::Pending
				}
			})
			.await
		}

		/// `true` if `fut` resolves within `ms`, `false` if it is still pending. The bound turns a
		/// handler that never fires into a deterministic failure instead of an infinite hang.
		async fn aborts_within<F: Future<Output = AbortedError>>(fut: F, ms: u64) -> bool {
			let fut = std::pin::pin!(fut);
			let timeout = std::pin::pin!(futures_timer::Delay::new(Duration::from_millis(ms)));
			matches!(select(fut, timeout).await, Either::Left(_))
		}

		// Two operations sharing ONE AbortSignal must BOTH observe `abort()`. The old `set_onabort`
		// design let the second registration replace the first handler, so the first operation was
		// left uncancellable. Both futures are driven to their `receiver.await` point BEFORE
		// `abort()` fires, so this exercises event delivery — not the synchronous `aborted()`
		// fast path, which would mask the clobbering.
		#[wasm_bindgen_test]
		async fn both_operations_sharing_a_signal_are_aborted() {
			let controller = web_sys::AbortController::new().unwrap();
			let signal = controller.signal();
			let first = AbortSignal(JsValue::from(signal.clone()))
				.into_future()
				.unwrap();
			let second = AbortSignal(JsValue::from(signal)).into_future().unwrap();

			let aborter = async {
				yield_once().await;
				controller.abort();
			};
			let (first_aborted, second_aborted, ()) = futures::join!(
				aborts_within(first, 2000),
				aborts_within(second, 2000),
				aborter
			);

			assert!(
				first_aborted,
				"first operation was not aborted — its handler was clobbered by the second"
			);
			assert!(second_aborted, "second operation was not aborted");
		}

		// When an operation ends before any abort, its future (and the Drop guard) is dropped. The
		// fix registers via `add_event_listener` and removes that listener on drop, so it NEVER
		// touches `onabort`. The old `set_onabort` design left `onabort` pointing at the operation's
		// now-freed `Closure::once`, so a late `abort()` invoked a freed closure.
		#[wasm_bindgen_test]
		async fn ending_an_operation_leaves_onabort_untouched_and_the_signal_usable() {
			let controller = web_sys::AbortController::new().unwrap();
			let signal = controller.signal();
			assert!(
				signal.onabort().is_none(),
				"precondition: a fresh signal has no onabort handler"
			);

			// The operation registers, then ends (future + guard dropped).
			drop(
				AbortSignal(JsValue::from(signal.clone()))
					.into_future()
					.unwrap(),
			);

			// The fix never sets `onabort`; the old design would leave it referencing the freed
			// closure here.
			assert!(
				signal.onabort().is_none(),
				"onabort must stay unset — the fix registers via add_event_listener, not set_onabort"
			);

			// A late abort is a safe no-op and leaves the signal usable: a fresh operation
			// registered afterwards still resolves (via the already-aborted fast path).
			controller.abort();
			let late = AbortSignal(JsValue::from(signal)).into_future().unwrap();
			assert!(
				aborts_within(late, 2000).await,
				"signal was left unusable after the guarded drop + late abort"
			);
		}
	}
}

mod managed {
	use std::task::Poll;

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use tsify::Tsify;
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{Error, error::AbortedError, runtime::CommanderFutHandle};

	use super::{abortable::*, pausable::*};

	#[derive(Deserialize, Default, Tsify)]
	#[serde(rename_all = "camelCase")]
	#[tsify(from_wasm_abi)]
	pub struct ManagedFuture {
		#[serde(default)]
		abort_signal: AbortSignal,
		#[serde(default)]
		pause_signal: PauseSignal,
	}

	impl ManagedFuture {
		pub(crate) fn into_js_managed_commander_future<F, Fut>(
			self,
			f: F,
		) -> Result<
			JSManagedFuture<CommanderFutHandle<Fut::Output>, impl Future<Output = AbortedError>>,
			Error,
		>
		where
			F: FnOnce() -> Fut + Send + 'static,
			Fut: Future + 'static,
			Fut::Output: Send + 'static,
		{
			let abort_fut = self.abort_signal.into_future()?;
			let pausable = self.pause_signal.into_pausable_on_commander(f)?;
			Ok(JSManagedFuture {
				main_fut: Some(pausable),
				abort_fut,
			})
		}
	}

	pin_project! {
		pub(crate) struct JSManagedFuture<F, F1>
		where
			F: std::future::Future,
			F1: std::future::Future<Output = AbortedError>,
		{
			#[pin]
			main_fut: Option<CommanderFutHandle<F::Output>>,
			#[pin]
			abort_fut: AbortSignalFuture<F1>,
		}
	}

	impl<T, F, F1> std::future::Future for JSManagedFuture<F, F1>
	where
		F: std::future::Future<Output = Result<T, Error>>,
		F1: std::future::Future<Output = AbortedError>,
	{
		type Output = Result<T, Error>;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let mut this = self.project();
			if let Poll::Ready(aborted) = this.abort_fut.poll(cx) {
				// drop the main future, which cancels it on the commander thread
				this.main_fut.take();
				Poll::Ready(Err(Error::from(aborted)))
			} else if let Some(main_fut) = this.main_fut.as_mut().as_pin_mut() {
				if let Poll::Ready(res) = main_fut.poll(cx) {
					this.main_fut.take();
					Poll::Ready(res)
				} else {
					Poll::Pending
				}
			} else {
				Poll::Pending
			}
		}
	}
}
