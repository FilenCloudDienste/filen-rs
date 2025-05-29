use std::sync::OnceLock;

use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

#[cfg(target_os = "android")]
pub fn init_logger() {
	android_log::init("filen-sdk-rs").unwrap();
}

#[cfg(not(target_os = "android"))]
pub fn init_logger() {
	env_logger::init();
}

pub(crate) fn get_runtime() -> &'static Runtime {
	RUNTIME.get_or_init(|| {
		init_logger();
		Runtime::new().expect("Failed to create Tokio runtime")
	})
}
