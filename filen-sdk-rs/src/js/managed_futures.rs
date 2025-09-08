pub use managed::ManagedFuture;
pub use pausable::{PauseSignal, PauseSignalRust};

mod pausable {
	// Heavily inspired by
	// https://github.com/xuxiaocheng0201/pausable_future
	use std::{
		cell::{OnceCell, RefCell},
		future::Future,
		rc::Rc,
		task::{Poll, Waker},
	};

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
	use web_sys::js_sys;

	pin_project! {
		pub(super) struct Pausable<F> where F: Future {
			#[pin]
			fut: F,
			signal: Option<PauseSignalRust>,
		}
	}

	#[derive(Debug)]
	struct Controller {
		paused: bool,
		wakers: Vec<Waker>,
	}

	#[derive(Deserialize, Default)]
	#[serde(transparent)]
	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	// unfortunately we have to call this PauseSignal, instead of PauseSignalJS,
	// because Tsify does not support renaming the struct
	// and we need to export the Rust PauseSignal struct as PauseSignal in JS
	pub struct PauseSignal(#[serde(with = "serde_wasm_bindgen::preserve")] JsValue);

	impl PauseSignal {
		pub(super) fn into_pausable<F>(self, fut: F) -> Result<Pausable<F>, JsValue>
		where
			F: Future,
		{
			let pausable = Pausable {
				fut: fut,
				signal: if self.0.is_undefined() {
					None
				} else {
					Some(PauseSignalRust::clone_from_js_value(self.0)?)
				},
			};
			Ok(pausable)
		}
	}

	thread_local! {
		static WBG_PTR: OnceCell<JsValue> = OnceCell::new();
	}

	impl PauseSignalRust {
		// This is made by using cargo expand on the #[wasm_bindgen] for PauseSignalRust
		// and then combining the wasm_bindgen::convert::TryFromJsValue for PauseSignalRust
		// and impl wasm_bindgen::convert::RefFromWasmAbi for PauseSignalRust
		// the default TryFromJsValue implementation calls into JS to delete the value JS side
		// this version clones the inner value instead
		fn clone_from_js_value(
			value: wasm_bindgen::JsValue,
		) -> wasm_bindgen::__rt::core::result::Result<Self, JsValue> {
			let ptr = {
				WBG_PTR.with(|v| {
					js_sys::Reflect::get(&value, v.get_or_init(|| JsValue::from_str("__wbg_ptr")))
				})
			}?;
			let ptr = ptr.as_f64().unwrap() as u32;
			if ptr == 0 {
				wasm_bindgen::__rt::core::result::Result::Err(value)
			} else {
				let rc_ref =
					unsafe { <Self as wasm_bindgen::convert::RefFromWasmAbi>::ref_from_abi(ptr) };
				Ok((&*rc_ref).clone())
			}
		}
	}

	// Might not work with tuple struct
	#[derive(Debug, Clone)]
	#[wasm_bindgen(js_name = "PauseSignal")]
	pub struct PauseSignalRust(Rc<RefCell<Controller>>);

	impl PauseSignalRust {
		fn inner(&self) -> std::cell::Ref<'_, Controller> {
			self.0.borrow()
		}

		fn inner_mut(&self) -> std::cell::RefMut<'_, Controller> {
			self.0.borrow_mut()
		}
	}

	#[wasm_bindgen(js_class = "PauseSignal")]
	impl PauseSignalRust {
		#[wasm_bindgen(constructor)]
		pub fn new() -> Self {
			Self(Rc::new(RefCell::new(Controller {
				paused: false,
				wakers: Vec::new(),
			})))
		}

		#[wasm_bindgen(js_name = "isPaused")]
		pub fn is_paused(&self) -> bool {
			self.inner().paused
		}

		#[wasm_bindgen]
		pub fn pause(&self) {
			let mut inner = self.inner_mut();
			inner.paused = true;
		}

		#[wasm_bindgen]
		pub fn resume(&self) {
			let mut inner = self.inner_mut();
			inner.paused = false;
			for waker in inner.wakers.drain(..) {
				waker.wake();
			}
		}
	}

	impl<F> Future for Pausable<F>
	where
		F: Future,
	{
		type Output = F::Output;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let this = self.project();
			if let Some(signal) = this.signal {
				let mut controller = signal.inner_mut();
				if !controller.paused {
					std::mem::drop(controller);
					this.fut.poll(cx)
				} else {
					controller.wakers.push(cx.waker().clone());
					Poll::Pending
				}
			} else {
				this.fut.poll(cx)
			}
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
				let closure = Closure::once(move || {
					let _ = sender.send(());
				});
				signal.set_onabort(Some(closure.as_ref().unchecked_ref()));
				Ok(AbortSignalFuture::Some {
					fut: async move {
						if signal.aborted() {
							log::debug!("AbortSignal already aborted, returning AbortedError");
							return AbortedError;
						}
						let _closure = closure; // keep the closure alive
						let _ = receiver.await;
						log::debug!("AbortSignal aborted, returning AbortedError");
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
}

mod managed {
	use std::task::Poll;

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use tsify::Tsify;
	use wasm_bindgen::{JsValue, prelude::wasm_bindgen};

	use crate::{Error, error::AbortedError};

	use super::{abortable::*, pausable::*};

	#[derive(Deserialize, Default, Tsify)]
	#[serde(rename_all = "camelCase")]
	pub struct ManagedFuture {
		#[serde(default)]
		abort_signal: AbortSignal,
		#[serde(default)]
		pause_signal: PauseSignal,
	}

	impl ManagedFuture {
		pub(crate) fn into_js_managed_future<F>(
			self,
			fut: F,
		) -> Result<JSManagedFuture<F, impl Future<Output = AbortedError>>, JsValue>
		where
			F: std::future::Future,
		{
			let abort_fut = self.abort_signal.into_future()?;
			let pausable = self.pause_signal.into_pausable(fut)?;
			Ok(JSManagedFuture {
				main_fut: pausable,
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
			main_fut: Pausable<F>,
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
			let this = self.project();
			if let Poll::Ready(aborted) = this.abort_fut.poll(cx) {
				Poll::Ready(Err(Error::from(aborted)))
			} else {
				this.main_fut.poll(cx)
			}
		}
	}
}
