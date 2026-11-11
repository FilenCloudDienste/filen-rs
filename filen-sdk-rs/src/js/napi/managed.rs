//! The pause signal and the managed abort controller on napi, and the read of the
//! `ManagedFuture` a managed call takes.
//!
//! napi has no `ManagedFuture.create`: a call takes a plain object whose `abortSignal` and
//! `pauseSignal` are these classes or `undefined`, as ubrn's record has them.

use std::sync::Arc;

use napi::bindgen_prelude::{ClassInstance, JsObjectValue, Object};
use napi_derive::napi;

use super::{IntoWire, Wire};
use crate::js;

#[napi(js_name = "PauseSignal")]
pub(crate) struct PauseSignal(pub(crate) js::PauseSignal);

#[napi]
impl PauseSignal {
	#[napi(constructor, ts_args_type = "")]
	pub fn new() -> Self {
		Self(js::PauseSignal::new())
	}
}

#[napi(js_name = "ManagedAbortController")]
pub(crate) struct ManagedAbortController(pub(crate) js::ManagedAbortController);

#[napi]
impl ManagedAbortController {
	#[napi(constructor, ts_args_type = "")]
	pub fn new() -> Self {
		Self(js::ManagedAbortController::new())
	}
}

#[napi(js_name = "ManagedAbortSignal")]
pub(crate) struct ManagedAbortSignal(pub(crate) js::ManagedAbortSignal);

#[deny(unsafe_code)]
impl Wire for js::ManagedAbortSignal {
	type Wire = ManagedAbortSignal;
}

#[deny(unsafe_code)]
impl IntoWire for js::ManagedAbortSignal {
	fn into_wire(self) -> ManagedAbortSignal {
		ManagedAbortSignal(self)
	}
}

/// Reads the `ManagedFuture` a managed call takes, on the JS thread: napi checks each signal to
/// be an instance of its class, and the signals' watch channels are cloned out of them, so
/// nothing of JS is held once it returns.
// No napi export runs a managed call yet.
#[allow(dead_code)]
#[deny(unsafe_code)]
pub(crate) fn read_managed_future(record: &Object<'_>) -> napi::Result<js::ManagedFuture> {
	let abort_signal: Option<ClassInstance<'_, ManagedAbortSignal>> =
		record.get_named_property("abortSignal")?;
	let pause_signal: Option<ClassInstance<'_, PauseSignal>> =
		record.get_named_property("pauseSignal")?;
	Ok(js::ManagedFuture {
		abort_signal: abort_signal.map(|signal| Arc::new(signal.0.clone())),
		pause_signal: pause_signal.map(|signal| Arc::new(signal.0.clone())),
	})
}
