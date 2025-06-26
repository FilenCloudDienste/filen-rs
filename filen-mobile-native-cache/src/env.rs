use std::sync::OnceLock;

use tokio::runtime::Runtime;

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
pub(crate) static INIT_LOGGER: OnceLock<()> = OnceLock::new();

#[cfg(target_os = "android")]
pub(crate) fn init_logger() {
	INIT_LOGGER.get_or_init(|| {
		android_log::init("filen-sdk-rs").unwrap();
	});
}

#[cfg(not(target_os = "android"))]
pub(crate) fn init_logger() {
	println!("Initializing logger");
	INIT_LOGGER.get_or_init(|| {
		println!("Initializing env_logger");
		let _ = env_logger::builder()
			.filter_level(log::LevelFilter::Debug)
			.parse_default_env()
			.try_init();
		// env_logger::init();
	});
}

pub(crate) fn get_runtime() -> &'static Runtime {
	init_logger();
	RUNTIME.get_or_init(|| Runtime::new().expect("Failed to create Tokio runtime"))
}
