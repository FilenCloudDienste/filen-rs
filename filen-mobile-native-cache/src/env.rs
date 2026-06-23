use std::sync::OnceLock;

use tokio::runtime::{Builder, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Idempotent; installs the per-target tracing subscriber (logcat on Android, os_log on iOS,
/// fmt on desktop) unless a host already installed one.
pub(crate) fn init_logger() {
	filen_sdk_rs::obs::try_init(filen_sdk_rs::auth::http::LogLevel::Debug);
}

#[cfg(target_os = "android")]
static VM: OnceLock<jni::JavaVM> = OnceLock::new();

#[cfg(target_os = "android")]
#[unsafe(export_name = "Java_io_filen_app_FilenDocumentsProvider_initJavaVM")]
pub extern "system" fn java_init(env: jni::JNIEnv, _class: jni::objects::JClass) {
	let vm = env.get_java_vm().unwrap();
	_ = VM.set(vm);
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

#[cfg(target_os = "android")]
fn build_tokio_runtime() -> Runtime {
	Builder::new_multi_thread()
		.enable_all()
		.thread_stack_size(1024 * 1024 * 2)
		.on_thread_start(|| {
			let vm = VM.get().expect("init java vm");
			vm.attach_current_thread_permanently().unwrap();
		})
		.build()
		.expect("Failed to create Tokio runtime")
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
fn build_tokio_runtime() -> Runtime {
	Builder::new_multi_thread()
		.enable_all()
		.thread_stack_size(1024 * 1024 * 2)
		.build()
		.expect("Failed to create Tokio runtime")
}

pub(crate) fn get_runtime() -> &'static Runtime {
	RUNTIME.get_or_init(|| {
		tracing::info!("Creating Tokio runtime");
		let rt = build_tokio_runtime();
		// Start the hang watchdog on the runtime we just built (enter it so the spawn sees a
		// current handle). Runs at most once per process.
		let _guard = rt.enter();
		filen_sdk_rs::obs::spawn_inflight_watchdog();
		rt
	})
}
