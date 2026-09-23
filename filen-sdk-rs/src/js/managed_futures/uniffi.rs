pub use managed::ManagedFuture;
pub use pausable::PauseSignal;

mod pausable {
	use pin_project_lite::pin_project;
	use std::future::Future;

	use crate::runtime::{self, CommanderFutHandle};

	pin_project! {
		pub(super) struct Pausable<F> where F: Future {
			#[pin]
			fut: F,
			signal: Option<PauseSignal>,
		}
	}

	/// A signal that can pause and resume async operations
	#[derive(Clone, uniffi::Object)]
	pub struct PauseSignal {
		sender: tokio::sync::watch::Sender<bool>,
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	impl Default for PauseSignal {
		fn default() -> Self {
			Self::new()
		}
	}

	#[uniffi::export]
	impl PauseSignal {
		#[uniffi::constructor]
		pub fn new() -> Self {
			let (sender, receiver) = tokio::sync::watch::channel(false);
			Self { sender, receiver }
		}

		pub fn is_paused(&self) -> bool {
			*self.receiver.borrow()
		}

		pub fn pause(&self) {
			let _ = self.sender.send(true);
		}

		pub fn resume(&self) {
			let _ = self.sender.send(false);
		}
	}

	impl PauseSignal {
		pub(super) fn into_pausable_on_commander<F, Fut>(
			self,
			fut_builder: F,
		) -> CommanderFutHandle<Fut::Output>
		where
			F: FnOnce() -> Fut + Send + 'static,
			Fut: Future + Send + 'static,
			Fut::Output: Send + 'static,
		{
			runtime::do_with_pause_channel_on_commander((self.sender, self.receiver), fut_builder)
		}

		/// The pause requests, for a job that pauses itself instead of being stopped from polling.
		pub(super) fn receiver(&self) -> tokio::sync::watch::Receiver<bool> {
			self.receiver.clone()
		}
	}
}

mod abortable {
	use pin_project_lite::pin_project;
	use std::task::Poll;

	use crate::error::AbortedError;

	#[derive(uniffi::Object)]
	pub struct ManagedAbortController {
		sender: tokio::sync::watch::Sender<bool>,
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	#[uniffi::export]
	impl ManagedAbortController {
		#[uniffi::constructor]
		pub fn new() -> Self {
			let (sender, receiver) = tokio::sync::watch::channel(false);
			Self { sender, receiver }
		}

		pub fn signal(&self) -> ManagedAbortSignal {
			ManagedAbortSignal {
				receiver: self.receiver.clone(),
			}
		}

		pub fn abort(&self) {
			let _ = self.sender.send(true);
		}
	}

	#[derive(Clone, uniffi::Object)]
	pub struct ManagedAbortSignal {
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	#[uniffi::export]
	impl ManagedAbortSignal {
		pub fn aborted(&self) -> bool {
			*self.receiver.borrow()
		}
	}

	impl ManagedAbortSignal {
		pub(super) fn into_future(self) -> AbortSignalFuture<impl Future<Output = AbortedError>> {
			AbortSignalFuture::Some {
				fut: async move {
					let mut receiver = self.receiver;
					loop {
						if *receiver.borrow() {
							return AbortedError;
						}
						if receiver.changed().await.is_err() {
							// The controller (sender) was dropped without an explicit abort. Treat
							// this as "not aborted": a dropped controller means no further abort
							// signals can arrive, NOT that the in-flight operation should be
							// cancelled. Returning AbortedError here would spuriously cancel an
							// upload/download whenever the app failed to retain the controller
							// (GC/ARC finalization mid-transfer). Never resolve instead, so this
							// branch cannot cancel the operation.
							return std::future::pending::<AbortedError>().await;
						}
					}
				},
			}
		}
	}

	pin_project! {
		#[project = AbortSignalFutureProj]
		pub(super) enum AbortSignalFuture<F> {
			None,
			Some{#[pin] fut: F},
		}
	}

	impl<F> Future for AbortSignalFuture<F>
	where
		F: Future<Output = AbortedError>,
	{
		type Output = AbortedError;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let this = self.project();
			match this {
				AbortSignalFutureProj::None => Poll::Pending,
				AbortSignalFutureProj::Some { fut: pinned } => pinned.poll(cx),
			}
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn dropped_controller_does_not_abort() {
			futures::executor::block_on(async {
				let controller = ManagedAbortController::new();
				let mut fut = std::pin::pin!(controller.signal().into_future());
				// Drop the controller (the only watch sender) without an explicit abort.
				drop(controller);
				// The sender is gone, so `changed()` resolves to Err on the first poll. The
				// future must stay Pending rather than resolve to AbortedError: a dropped
				// controller must never cancel the in-flight operation.
				assert!(futures::poll!(fut.as_mut()).is_pending());
			});
		}
	}
}

mod managed {
	use pin_project_lite::pin_project;
	use std::{sync::Arc, task::Poll};

	use crate::{
		Error,
		error::AbortedError,
		fs::copy::{
			JobControl,
			control::cancel_grace::{CANCEL_GRACE, CancelOnAbort, with_cancel_grace},
		},
		runtime::{self, CommanderFutHandle},
	};

	use super::{abortable::*, pausable::*};

	#[derive(uniffi::Record)]
	pub struct ManagedFuture {
		pub abort_signal: Option<Arc<ManagedAbortSignal>>,
		pub pause_signal: Option<Arc<PauseSignal>>,
	}

	impl ManagedFuture {
		pub(crate) fn into_js_managed_commander_future<F, Fut>(
			self,
			fut_builder: F,
		) -> JSManagedFuture<CommanderFutHandle<Fut::Output>, impl Future<Output = AbortedError>>
		where
			F: FnOnce() -> Fut + Send + 'static,
			Fut: Future + Send + 'static,
			Fut::Output: Send + 'static,
		{
			let abort_fut = match self.abort_signal {
				Some(signal_arc) => Arc::unwrap_or_clone(signal_arc).into_future(),
				None => AbortSignalFuture::None,
			};
			let pausable = match self.pause_signal {
				Some(signal_arc) => {
					Arc::unwrap_or_clone(signal_arc).into_pausable_on_commander(fut_builder)
				}
				None => crate::runtime::do_on_commander(fut_builder),
			};
			JSManagedFuture {
				main_fut: Some(pausable),
				abort_fut,
			}
		}

		/// Runs a job that observes pause and cancel itself through its [`JobControl`], so a
		/// paused job can release what it holds and a cancelled one can report what it did.
		/// An abort becomes a cancel; a job still running [`CANCEL_GRACE`] after that is dropped.
		#[cfg_attr(not(test), expect(dead_code, reason = "used by the copy bindings"))]
		pub(crate) fn into_js_managed_commander_job<F, Fut, T>(
			self,
			job: F,
		) -> CancelOnAbort<CommanderFutHandle<Result<T, Error>>, impl Future<Output = AbortedError>>
		where
			F: FnOnce(JobControl) -> Fut + Send + 'static,
			Fut: Future<Output = Result<T, Error>> + Send + 'static,
			T: Send + 'static,
		{
			let abort_fut = match self.abort_signal {
				Some(signal_arc) => Arc::unwrap_or_clone(signal_arc).into_future(),
				None => AbortSignalFuture::None,
			};
			let pause = self.pause_signal.map(|signal| signal.receiver());
			let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
			let handle = runtime::do_on_commander(move || {
				let control = JobControl::new(pause, Some(cancel_rx.clone()));
				with_cancel_grace(job(control), cancel_rx, CANCEL_GRACE)
			});
			CancelOnAbort::new(handle, abort_fut, cancel)
		}
	}

	pin_project! {
		pub(crate) struct JSManagedFuture<F, F1>
		where
			F: std::future::Future,
			F1: std::future::Future<Output = AbortedError>,
		{
			#[pin]
			main_fut: Option<CommanderFutHandle<F::Output>>,
			#[pin]
			abort_fut: AbortSignalFuture<F1>,
		}
	}

	impl<T, F, F1> std::future::Future for JSManagedFuture<F, F1>
	where
		F: std::future::Future<Output = Result<T, Error>>,
		F1: std::future::Future<Output = AbortedError>,
	{
		type Output = Result<T, Error>;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			let mut this = self.project();
			if let Poll::Ready(aborted) = this.abort_fut.poll(cx) {
				// drop the main future, which cancels it on the commander thread
				this.main_fut.take();
				Poll::Ready(Err(Error::from(aborted)))
			} else if let Some(main_fut) = this.main_fut.as_mut().as_pin_mut() {
				if let Poll::Ready(res) = main_fut.poll(cx) {
					this.main_fut.take();
					Poll::Ready(res)
				} else {
					Poll::Pending
				}
			} else {
				Poll::Pending
			}
		}
	}

	#[cfg(test)]
	mod tests {
		use std::time::Duration;

		use super::*;

		// The outer future is driven by a plain executor, as the foreign one would drive it.
		#[test]
		fn pause_and_abort_reach_the_job_as_requests_it_observes() {
			let controller = ManagedAbortController::new();
			let pause = Arc::new(PauseSignal::new());
			let managed = ManagedFuture {
				abort_signal: Some(Arc::new(controller.signal())),
				pause_signal: Some(pause.clone()),
			};
			let (paused_tx, paused_rx) = std::sync::mpsc::channel();
			let job = managed.into_js_managed_commander_job(move |control| async move {
				control.pause_changed(false).await;
				paused_tx.send(()).unwrap();
				control.stopping().await;
				Ok::<_, Error>(control.is_cancelled())
			});

			pause.pause();
			paused_rx
				.recv_timeout(Duration::from_secs(10))
				.expect("the job saw the pause request");
			controller.abort();

			let cancelled =
				futures::executor::block_on(job).expect("the job ends with its own result");
			assert!(cancelled, "the abort reached the job as a cancel");
		}

		#[test]
		fn a_job_without_signals_runs_to_completion() {
			let managed = ManagedFuture {
				abort_signal: None,
				pause_signal: None,
			};
			let job = managed.into_js_managed_commander_job(|control| async move {
				control
					.checkpoint()
					.await
					.map_err(|_| Error::custom(crate::ErrorKind::Cancelled, "stopped"))?;
				Ok::<_, Error>(7)
			});
			assert_eq!(futures::executor::block_on(job).unwrap(), 7);
		}
	}
}
