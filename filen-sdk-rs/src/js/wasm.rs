use wasm_bindgen::JsValue;

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
