use serde::Serialize;
use tokio::sync::{
	mpsc::{self, UnboundedSender},
	oneshot,
};
use wasm_bindgen::{JsCast, JsValue};
use web_sys::js_sys;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(start)
)]
pub fn main_js() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	#[cfg(debug_assertions)]
	crate::obs::try_init(crate::auth::http::LogLevel::Debug);
	#[cfg(not(debug_assertions))]
	crate::obs::try_init(crate::auth::http::LogLevel::Info);
	Ok(())
}

/// Deserializes an optional JS callback, for `#[serde(default, deserialize_with = ...)]` fields of
/// `Option<js_sys::Function>`. `undefined` and `null` mean no callback. A plain `js_sys::Function`
/// field with `#[serde(default)]` cannot be used: `Function::default()` is `new Function("")`, which
/// is eval and throws under a Content-Security-Policy without `unsafe-eval`, so every call that
/// omitted the callback would fail before it started.
pub(crate) fn optional_function<'de, D: serde::Deserializer<'de>>(
	deserializer: D,
) -> Result<Option<js_sys::Function>, D::Error> {
	let value: JsValue = serde_wasm_bindgen::preserve::deserialize(deserializer)?;
	if value.is_undefined() || value.is_null() {
		return Ok(None);
	}
	value
		.dyn_into::<js_sys::Function>()
		.map(Some)
		.map_err(|_| serde::de::Error::custom("expected a function"))
}

/// Delivers items to `handler` on this thread in the order they were sent: the wasm twin of the
/// uniffi `spawn_ordered_dispatch`. A job runs on the commander and sends its callbacks here,
/// where the JS functions it calls live (they never leave the thread that got them). Dropping
/// every returned sender ends the task, which then resolves the returned receiver: every item
/// sent by then has been handled.
pub(crate) fn spawn_local_dispatch<T: 'static>(
	mut handler: impl FnMut(T) + 'static,
) -> (UnboundedSender<T>, oneshot::Receiver<()>) {
	let (sender, mut receiver) = mpsc::unbounded_channel::<T>();
	let (done, handled) = oneshot::channel();
	crate::runtime::spawn_local(async move {
		while let Some(item) = receiver.recv().await {
			handler(item);
		}
		let _ = done.send(());
	});
	(sender, handled)
}

/// Calls a job's `callback` with `value`, if the caller passed one, serialized as the SDK's
/// other records are: objects rather than maps, large numbers as BigInt.
pub(crate) fn call_callback(callback: Option<&js_sys::Function>, value: &impl Serialize) {
	let Some(callback) = callback else {
		return;
	};
	let serializer = serde_wasm_bindgen::Serializer::new()
		.serialize_maps_as_objects(true)
		.serialize_large_number_types_as_bigints(true);
	let value = value
		.serialize(&serializer)
		.expect("failed to serialize a job callback (should be impossible)");
	let _ = callback.call1(&JsValue::UNDEFINED, &value);
}
