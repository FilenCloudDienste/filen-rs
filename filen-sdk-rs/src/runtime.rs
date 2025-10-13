#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub use wasm_bindgen_rayon::init_thread_pool;
