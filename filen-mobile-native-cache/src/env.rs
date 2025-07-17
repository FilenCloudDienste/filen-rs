use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();
pub(crate) static INIT_LOGGER: OnceLock<()> = OnceLock::new();

#[cfg(target_os = "android")]
pub(crate) fn init_logger() {
	INIT_LOGGER.get_or_init(|| {
		android_log::init("filen-sdk-rs").unwrap();
	});
}

#[cfg(target_os = "ios")]
pub(crate) fn init_logger() {
	INIT_LOGGER.get_or_init(|| {
		let _ = oslog::OsLogger::new("io.filen.app.FilenFileProvider")
			.level_filter(log::LevelFilter::Debug)
			.init()
			.unwrap();
	});
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub(crate) fn init_logger() {
	println!("Initializing logger");
	INIT_LOGGER.get_or_init(|| {
		println!("Initializing env_logger");
		let _ =
			env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug"))
				.try_init();
	});
}

#[cfg(target_os = "ios")]
fn build_tokio_runtime() -> Runtime {
	Builder::new_multi_thread()
		.enable_all()
		.worker_threads(1)
		.thread_stack_size(1024 * 1024)
		.build()
		.expect("Failed to create Tokio runtime")
}

#[cfg(not(target_os = "ios"))]
fn build_tokio_runtime() -> Runtime {
	Builder::new_multi_thread()
		.enable_all()
		.thread_stack_size(1024 * 1024 * 2)
		.build()
		.expect("Failed to create Tokio runtime")
}

pub(crate) fn get_runtime() -> &'static Runtime {
	RUNTIME.get_or_init(|| {
		log::info!("Creating Tokio runtime");
		build_tokio_runtime()
	})
}
