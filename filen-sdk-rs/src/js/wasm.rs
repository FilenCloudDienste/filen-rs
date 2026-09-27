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
