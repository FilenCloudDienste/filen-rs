use wasm_bindgen::JsValue;

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(start)
)]
pub fn main_js() -> Result<(), JsValue> {
	console_error_panic_hook::set_once();
	#[cfg(debug_assertions)]
	wasm_logger::init(wasm_logger::Config::new(log::Level::Debug));
	#[cfg(not(debug_assertions))]
	wasm_logger::init(wasm_logger::Config::new(log::Level::Info));
	Ok(())
}
