pub use managed::ManagedFuture;
pub use pausable::{PauseSignal, PauseSignalRust};

mod pausable {
	// Heavily inspired by
	// https://github.com/xuxiaocheng0201/pausable_future
	use std::{cell::OnceCell, future::Future, task::Waker};

	use pin_project_lite::pin_project;
	use serde::Deserialize;
	use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
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

	#[derive(Debug, Default)]
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

	#[wasm_bindgen(js_name = "PauseSignal")]
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
			let ptr = ptr.as_f64().unwrap() as u32;
			if ptr == 0 {
				Err(value.clone())
			} else {
				let rc_ref =
					unsafe { <Self as wasm_bindgen::convert::RefFromWasmAbi>::ref_from_abi(ptr) };
				Ok(rc_ref)
			}
		}
	}

	#[wasm_bindgen(js_class = "PauseSignal")]
	impl PauseSignalRust {
		#[wasm_bindgen(constructor)]
		pub fn new() -> Self {
			Self::default()
		}

		#[wasm_bindgen(js_name = "isPaused")]
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
	use wasm_bindgen::prelude::wasm_bindgen;

	use crate::{Error, error::AbortedError, runtime::CommanderFutHandle};

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

	struct NewManagedFuture {
		abort_signal: AbortSignal,
		pause_signal: PauseSignal,
	}
}
