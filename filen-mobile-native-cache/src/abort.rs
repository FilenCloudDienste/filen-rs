//! Cooperative cancellation for the calls that move bytes.
//!
//! uniffi cannot cancel a Rust future from Swift — the generated `CALL_CANCELLED` arm is a
//! `fatalError` — so cancellation travels in-band instead: the caller keeps a controller, hands
//! its signal to the call, and the call selects on it. Modelled on the SDK's
//! `ManagedAbortController`/`ManagedAbortSignal`, which cannot be reused here: its feature would
//! drag the whole SDK uniffi surface into these bindings.

use std::sync::Arc;

use tokio::sync::watch;

use crate::CacheError;

/// The caller's end of a cancellation: one per call it might want to stop.
#[derive(uniffi::Object, Default)]
pub struct FfiAbortController {
	sender: watch::Sender<bool>,
}

#[uniffi::export]
impl FfiAbortController {
	#[uniffi::constructor]
	pub fn new() -> Self {
		Self::default()
	}

	/// The end handed to the call. Any number may be taken, and all of them see the abort.
	pub fn signal(&self) -> Arc<FfiAbortSignal> {
		Arc::new(FfiAbortSignal {
			receiver: self.sender.subscribe(),
		})
	}

	/// Asks whatever holds a signal to stop. Idempotent, never blocks, and says nothing about
	/// when the call actually gives up — that is the op's business.
	pub fn abort(&self) {
		// `send_replace` rather than `send`: the value must land even when no signal is alive
		// (nothing has been handed out yet, or the call has already returned), or `is_aborted`
		// would go on reporting `false` after an abort.
		self.sender.send_replace(true);
	}

	pub fn is_aborted(&self) -> bool {
		*self.sender.borrow()
	}
}

/// The op's end of a cancellation, taken from [`FfiAbortController::signal`].
#[derive(uniffi::Object)]
pub struct FfiAbortSignal {
	receiver: watch::Receiver<bool>,
}

#[uniffi::export]
impl FfiAbortSignal {
	pub fn is_aborted(&self) -> bool {
		*self.receiver.borrow()
	}
}

impl FfiAbortSignal {
	/// Resolves once aborted, and never otherwise.
	///
	/// A controller dropped without an explicit abort is NOT an abort: an app that fails to
	/// retain it — ARC finalising it mid-transfer — must not thereby cancel the transfer, so that
	/// branch parks forever rather than resolving.
	pub(crate) async fn aborted(&self) {
		let mut receiver = self.receiver.clone();
		loop {
			if *receiver.borrow_and_update() {
				return;
			}
			if receiver.changed().await.is_err() {
				std::future::pending::<()>().await;
			}
		}
	}
}

/// The early-out a cancellable op takes before it touches anything: a signal that is already
/// aborted means the call was cancelled before it began, and nothing of it may happen.
pub(crate) fn check_not_aborted(abort: Option<&Arc<FfiAbortSignal>>) -> Result<(), CacheError> {
	match abort {
		Some(signal) if signal.is_aborted() => Err(CacheError::Aborted(
			"the call was aborted before it started".into(),
		)),
		_ => Ok(()),
	}
}

/// Runs `fut`, giving up on it as soon as `abort` fires.
///
/// Biased towards the op: an abort that races completion loses, because cancelling work that has
/// already been delivered would be a lie. Giving up DROPS `fut`, so only work that is safe to drop
/// mid-flight belongs in here — never a database write or a marker transition, which have to run
/// to their own completion whatever the caller wants.
pub(crate) async fn with_abort<T, E>(
	abort: Option<&Arc<FfiAbortSignal>>,
	fut: impl Future<Output = Result<T, E>>,
) -> Result<T, CacheError>
where
	E: Into<CacheError>,
{
	let Some(signal) = abort else {
		return fut.await.map_err(Into::into);
	};
	tokio::select! {
		biased;
		res = fut => res.map_err(Into::into),
		() = signal.aborted() => Err(CacheError::Aborted("the call was aborted".into())),
	}
}

#[cfg(test)]
mod tests {
	use std::sync::atomic::{AtomicBool, Ordering};

	use super::*;

	#[tokio::test]
	async fn abort_resolves_every_waiter_and_shows_in_is_aborted() {
		let controller = FfiAbortController::new();
		let first = controller.signal();
		let second = controller.signal();
		assert!(!controller.is_aborted());
		assert!(!first.is_aborted());

		controller.abort();

		assert!(controller.is_aborted());
		assert!(first.is_aborted() && second.is_aborted());
		// Both waiters resolve, and so does one taken after the fact.
		tokio::time::timeout(std::time::Duration::from_secs(1), async {
			first.aborted().await;
			second.aborted().await;
			controller.signal().aborted().await;
		})
		.await
		.expect("an abort must resolve every waiter");
	}

	/// A signal outlives its controller, and what it reports is what the controller last said —
	/// an app that drops the controller mid-transfer must not thereby cancel the transfer.
	#[tokio::test]
	async fn a_dropped_controller_is_not_an_abort() {
		let controller = FfiAbortController::new();
		let signal = controller.signal();
		drop(controller);

		assert!(!signal.is_aborted());
		assert!(
			tokio::time::timeout(std::time::Duration::from_millis(50), signal.aborted())
				.await
				.is_err(),
			"a controller dropped without an abort must never resolve a waiter"
		);

		let controller = FfiAbortController::new();
		let signal = controller.signal();
		controller.abort();
		drop(controller);
		assert!(signal.is_aborted(), "the abort itself survives the drop");
		signal.aborted().await;
	}

	#[tokio::test]
	async fn with_abort_gives_up_on_a_pending_op() {
		let controller = FfiAbortController::new();
		let signal = controller.signal();
		controller.abort();
		let finished = AtomicBool::new(false);

		let err = with_abort::<(), CacheError>(Some(&signal), async {
			std::future::pending::<()>().await;
			finished.store(true, Ordering::Relaxed);
			Ok(())
		})
		.await
		.expect_err("an aborted call must fail");

		assert!(matches!(err, CacheError::Aborted(_)), "got {err:?}");
		assert!(
			!finished.load(Ordering::Relaxed),
			"the op future must have been dropped, not run to completion"
		);
	}

	/// The bias: work that completes in the same poll as the abort arrives is work that was
	/// delivered, and reporting it cancelled would be a lie about what reached the server.
	#[tokio::test]
	async fn completion_beats_a_racing_abort() {
		let controller = FfiAbortController::new();
		let signal = controller.signal();
		controller.abort();

		assert_eq!(
			with_abort::<u8, CacheError>(Some(&signal), async { Ok(7) })
				.await
				.unwrap(),
			7
		);
	}

	#[tokio::test]
	async fn no_signal_means_no_cancellation() {
		assert!(check_not_aborted(None).is_ok());
		assert_eq!(
			with_abort::<u8, CacheError>(None, async { Ok(1) })
				.await
				.unwrap(),
			1
		);

		let controller = FfiAbortController::new();
		let signal = controller.signal();
		assert!(check_not_aborted(Some(&signal)).is_ok());
		controller.abort();
		assert!(matches!(
			check_not_aborted(Some(&signal)),
			Err(CacheError::Aborted(_))
		));
	}
}
