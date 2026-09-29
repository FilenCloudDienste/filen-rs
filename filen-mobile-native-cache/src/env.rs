use std::{
	future::Future,
	sync::{Arc, Mutex, MutexGuard, OnceLock},
};

use tokio::{
	runtime::{Builder, Runtime},
	task::{AbortHandle, JoinSet},
};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Idempotent; installs the per-target tracing subscriber (logcat on Android, os_log on iOS,
/// fmt on desktop) unless a host already installed one.
pub(crate) fn init_logger() {
	filen_sdk_rs::obs::try_init(filen_sdk_rs::auth::http::LogLevel::Debug);
}

#[cfg(target_os = "android")]
static VM: OnceLock<jni::JavaVM> = OnceLock::new();

#[cfg(target_os = "android")]
// JNI entry point: export_name is an unsafe attribute.
#[allow(unsafe_code)]
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

/// The fire-and-forget tasks one `FilenMobileCacheState` has spawned: the cleanup sweeps, the
/// auth-file re-check, the live-path start and its drainer. Every one of them captures the
/// state's `Arc<RwLock<CacheState>>` — the sweeps hold a read guard across their whole run — so
/// it is these tasks, not the `FilenMobileCacheState` handle, that decide when the authenticated
/// state and the SQLite connection inside it actually go away. One `JoinSet` holds them so that
/// dropping the state aborts them all, and so [`shutdown`](Self::shutdown) can wait for those
/// aborts to land — which a drop cannot: an abort only takes effect at the task's next yield, on
/// a runtime thread, and a caller reopening the same directory in the meantime found the DB
/// still open (a sharing violation on Windows; elsewhere a task working on a file it no longer
/// owns).
///
/// Cloneable so a task can file a follow-up under the same state (the live start spawns its
/// drainer).
#[derive(Clone, Default)]
pub(crate) struct BackgroundTasks(Arc<Mutex<JoinSet<()>>>);

impl BackgroundTasks {
	/// Spawns `task` on the shared runtime and tracks it. The handle only aborts.
	pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) -> AbortHandle {
		let mut set = self.lock();
		// Reap what has finished, or the set keeps a handle per FFI call for the state's
		// lifetime (every unauthenticated call launches a cleanup).
		while set.try_join_next().is_some() {}
		set.spawn_on(task, get_runtime().handle())
	}

	/// Aborts every task and returns once each has actually stopped — its future, and every
	/// guard and `Arc` inside it, dropped. Repeats for anything a stopping task managed to
	/// spawn on its way out. Terminal for the live path: a start aborted mid-way keeps its
	/// claim, so a later auth path does not bring the socket back up.
	pub(crate) async fn shutdown(&self) {
		loop {
			let mut set = std::mem::take(&mut *self.lock());
			if set.is_empty() {
				return;
			}
			set.shutdown().await;
		}
	}

	fn lock(&self) -> MutexGuard<'_, JoinSet<()>> {
		self.0
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}
}

#[cfg(test)]
mod background_tasks_tests {
	use std::sync::Arc;

	use super::BackgroundTasks;

	/// The point of `shutdown` over a drop: when it returns, the task's captures are gone.
	#[tokio::test]
	async fn shutdown_returns_only_once_an_aborted_task_has_let_go() {
		let tasks = BackgroundTasks::default();
		let held = Arc::new(());
		let pinned = held.clone();
		tasks.spawn(async move {
			let _pinned = pinned;
			std::future::pending::<()>().await;
		});
		assert_eq!(
			Arc::strong_count(&held),
			2,
			"the task owns its capture while it lives"
		);

		tasks.shutdown().await;

		assert_eq!(Arc::strong_count(&held), 1);
	}

	/// A follow-up that a tracked task spawned is stopped by the same shutdown.
	#[tokio::test]
	async fn shutdown_also_stops_what_a_task_spawned() {
		let tasks = BackgroundTasks::default();
		let held = Arc::new(());
		let pinned = held.clone();
		let inner = tasks.clone();
		let (spawned, has_spawned) = tokio::sync::oneshot::channel();
		tasks.spawn(async move {
			inner.spawn(async move {
				let _pinned = pinned;
				std::future::pending::<()>().await;
			});
			let _ = spawned.send(());
			std::future::pending::<()>().await;
		});
		has_spawned.await.unwrap();
		assert_eq!(Arc::strong_count(&held), 2);

		tasks.shutdown().await;

		assert_eq!(Arc::strong_count(&held), 1);
	}

	/// Finished tasks are reaped on the next spawn, so a long-lived state does not keep a handle
	/// per FFI call.
	#[tokio::test]
	async fn finished_tasks_are_reaped_on_the_next_spawn() {
		let tasks = BackgroundTasks::default();
		let done = tasks.spawn(async {});
		while !done.is_finished() {
			tokio::time::sleep(std::time::Duration::from_millis(5)).await;
		}
		tasks.spawn(std::future::pending());
		assert_eq!(tasks.lock().len(), 1);
		tasks.shutdown().await;
	}
}
