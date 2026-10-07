/// Aborts the wrapped task on drop unless it was [`join`](Self::join)ed first.
///
/// A bare `tokio::spawn` task is detached: dropping the future that holds its
/// `JoinHandle` does not stop it. Wrapping the handle here re-ties the task to
/// the owner's lifetime, so cancelling the owning future aborts the task.
pub(super) struct AbortOnDropHandle {
	handle: Option<tokio::task::JoinHandle<()>>,
}

impl AbortOnDropHandle {
	pub(super) fn new(handle: tokio::task::JoinHandle<()>) -> Self {
		Self {
			handle: Some(handle),
		}
	}

	/// Await the task's normal completion, disarming the abort-on-drop.
	pub(super) async fn join(mut self) -> Result<(), tokio::task::JoinError> {
		match self.handle.take() {
			Some(handle) => handle.await,
			None => Ok(()),
		}
	}
}

impl Drop for AbortOnDropHandle {
	fn drop(&mut self) {
		if let Some(handle) = &self.handle {
			handle.abort();
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	};

	use super::AbortOnDropHandle;

	/// Sets a flag when its future is dropped — a spawned task's future is
	/// dropped when the task is aborted, so this observes the abort.
	struct SetOnDrop(Arc<AtomicBool>);
	impl Drop for SetOnDrop {
		fn drop(&mut self) {
			self.0.store(true, Ordering::SeqCst);
		}
	}

	#[tokio::test]
	async fn dropping_guard_aborts_the_task() {
		let aborted = Arc::new(AtomicBool::new(false));
		let marker = aborted.clone();
		let guard = AbortOnDropHandle::new(tokio::spawn(async move {
			let _marker = SetOnDrop(marker);
			// Never completes on its own; only an abort ends it.
			std::future::pending::<()>().await;
		}));
		// Let the task start and park.
		tokio::task::yield_now().await;
		drop(guard);
		// The abort drops the task's future, running SetOnDrop.
		for _ in 0..1000 {
			if aborted.load(Ordering::SeqCst) {
				break;
			}
			tokio::task::yield_now().await;
		}
		assert!(
			aborted.load(Ordering::SeqCst),
			"dropping the guard must abort the task"
		);
	}

	#[tokio::test]
	async fn joining_guard_runs_task_to_completion() {
		let done = Arc::new(AtomicBool::new(false));
		let marker = done.clone();
		let guard = AbortOnDropHandle::new(tokio::spawn(async move {
			marker.store(true, Ordering::SeqCst);
		}));
		guard.join().await.unwrap();
		assert!(
			done.load(Ordering::SeqCst),
			"joined task must run to completion, not be aborted"
		);
	}
}
