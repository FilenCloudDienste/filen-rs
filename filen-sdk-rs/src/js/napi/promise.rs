//! Async methods on napi: a promise per call, with ubrn's trailing `asyncOpts_?: { signal }`.

use std::{cell::Cell, future::Future};

use napi::{
	Env, JsValue,
	bindgen_prelude::{
		FnArgs, Function, FunctionCallContext, JsObjectValue, Object, PromiseRaw, ToNapiValue,
		Unknown,
	},
};
use tokio::sync::oneshot;

use super::{
	error::{plain_error, throw},
	wire::IntoWire,
};
use crate::Error;

/// Runs `fut` on napi's Tokio runtime and returns a promise of its output.
///
/// The promise resolves with the value's wire form or rejects with the error in ubrn's shape
/// (`throw`). As under ubrn, a `signal` that is already aborted rejects without running `fut`,
/// and aborting it later drops `fut` (which cancels a commander task it awaits) and rejects. The
/// rejection is an `Error` named `AbortError`, with ubrn's message.
pub(crate) fn spawn_promise<'env, T>(
	env: &'env Env,
	async_opts: Option<Object<'env>>,
	fut: impl Future<Output = Result<T, Error>> + Send + 'static,
) -> napi::Result<PromiseRaw<'env, T::Wire>>
where
	T: IntoWire + Send + 'static,
	T::Wire: ToNapiValue,
{
	let signal = match async_opts {
		Some(opts) => opts.get::<Object>("signal")?,
		None => None,
	};
	let abort = match signal {
		Some(signal) => Some(on_abort(env, &signal)?),
		None => None,
	};
	env.spawn_future_with_callback(
		async move {
			let aborted = async {
				if let Some(abort) = abort
					&& abort.await.is_ok()
				{
					return;
				}
				// No signal, or one collected before it fired: never aborts.
				std::future::pending().await
			};
			// An already aborted signal is ready on the first poll, ahead of `fut`.
			Ok(tokio::select! {
				biased;
				() = aborted => None,
				output = fut => Some(output),
			})
		},
		|env, output| match output {
			Some(Ok(value)) => Ok(value.into_wire()),
			Some(Err(e)) => Err(throw(env, e)),
			None => Err(abort_error(env)),
		},
	)
}

/// `EventTarget.addEventListener(type, listener)`.
type AddEventListener<'env> =
	Function<'env, FnArgs<(&'env str, Function<'env, Unknown<'env>, ()>)>, Unknown<'env>>;

/// A receiver that completes when `signal` aborts, at once if it already has.
///
/// The `abort` listener stays on the signal after the call settles (ubrn removes it): it holds
/// only a spent sender, so a long-lived signal reused for many calls keeps one small closure per
/// call until it is collected.
fn on_abort(env: &Env, signal: &Object) -> napi::Result<oneshot::Receiver<()>> {
	let (sender, receiver) = oneshot::channel();
	if signal.get::<bool>("aborted")?.unwrap_or(false) {
		// The receiver is still here, so the send cannot fail.
		let _ = sender.send(());
		return Ok(receiver);
	}
	let sender = Cell::new(Some(sender));
	let listener = env.create_function_from_closure("abort", move |_: FunctionCallContext| {
		if let Some(sender) = sender.take() {
			// The call may have settled and dropped the receiver; then there is nothing to abort.
			let _ = sender.send(());
		}
		Ok(())
	})?;
	let add_event_listener: AddEventListener = signal.get_named_property("addEventListener")?;
	add_event_listener.apply(signal, FnArgs::from(("abort", listener)))?;
	Ok(receiver)
}

/// ubrn's `UniffiInternalError.AbortError`.
fn abort_error(env: &Env) -> napi::Error {
	let error = plain_error(env, "A Rust future was aborted").and_then(|mut error| {
		error.set_named_property("name", "AbortError")?;
		Ok(error)
	});
	match error {
		Ok(error) => napi::Error::from(error.to_unknown()),
		Err(e) => e,
	}
}
