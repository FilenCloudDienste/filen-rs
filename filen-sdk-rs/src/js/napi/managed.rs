//! The pause signal and the managed abort controller on napi, and the read of the
//! `ManagedFuture` a managed call takes.
//!
//! napi has no `ManagedFuture.create`: a call takes a plain object whose `abortSignal` and
//! `pauseSignal` are these classes or `undefined`, as ubrn's record has them.

use std::{any::type_name, sync::Arc};

use napi::{
	Env, JsValue, ValueType,
	bindgen_prelude::{
		ClassInstance, FromNapiValue, JavaScriptClassExt, JsObjectValue, Object, Unknown,
		ValidateNapiValue,
	},
};
use napi_derive::napi;

use super::{IntoWire, Wire};
use crate::{Error, ErrorKind, js};

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

/// Reads the `ManagedFuture` a managed call takes, on the JS thread; the signals' watch channels
/// are cloned out of it, so nothing of JS is held once it returns. A napi failure to read it is
/// thrown (`Err`); a signal that is neither `undefined` nor an instance of its class, `null`
/// included, is a conversion error for the call to reject with (`Ok(Err)`), as ubrn's converters
/// refuse it.
#[deny(unsafe_code)]
pub(crate) fn read_managed_future(
	record: &Object<'_>,
) -> napi::Result<Result<js::ManagedFuture, Error>> {
	let abort_signal = match read_signal::<ManagedAbortSignal>(record, "abortSignal")? {
		Ok(signal) => signal.map(|signal| Arc::new(signal.0.clone())),
		Err(e) => return Ok(Err(e)),
	};
	let pause_signal = match read_signal::<PauseSignal>(record, "pauseSignal")? {
		Ok(signal) => signal.map(|signal| Arc::new(signal.0.clone())),
		Err(e) => return Ok(Err(e)),
	};
	Ok(Ok(js::ManagedFuture {
		abort_signal,
		pause_signal,
	}))
}

/// `record[name]`, `undefined` or an instance of `T`'s class. Checked by hand: napi's own read
/// takes `null` for `undefined`, and without type tags (napi8) casts any wrapped object to `T`.
/// `instanceof` still trusts the prototype chain, which only code that rewires a signal's
/// prototype can mislead.
#[deny(unsafe_code)]
fn read_signal<'env, T>(
	record: &Object<'env>,
	name: &str,
) -> napi::Result<Result<Option<ClassInstance<'env, T>>, Error>>
where
	T: JavaScriptClassExt + 'env,
	ClassInstance<'env, T>: FromNapiValue + ValidateNapiValue,
{
	let value: Unknown = record.get_named_property(name)?;
	if value.get_type()? == ValueType::Undefined {
		return Ok(Ok(None));
	}
	let env = Env::from_raw(record.value().env);
	if value.get_type()? != ValueType::Object || !T::instance_of(&env, &value)? {
		return Ok(Err(Error::custom(
			ErrorKind::Conversion,
			format!("{name} is neither undefined nor a {}", type_name::<T>()),
		)));
	}
	Ok(Ok(record.get_named_property(name)?))
}
