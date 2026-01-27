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
	}
}

mod abortable {
	use pin_project_lite::pin_project;
	use std::task::Poll;

	use crate::error::AbortedError;

	#[derive(uniffi::Object)]
	pub struct AbortController {
		sender: tokio::sync::watch::Sender<bool>,
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	#[uniffi::export]
	impl AbortController {
		#[uniffi::constructor]
		pub fn new() -> Self {
			let (sender, receiver) = tokio::sync::watch::channel(false);
			Self { sender, receiver }
		}

		pub fn signal(&self) -> AbortSignal {
			AbortSignal {
				receiver: self.receiver.clone(),
			}
		}

		pub fn abort(&self) {
			let _ = self.sender.send(true);
		}
	}

	#[derive(Clone, uniffi::Object)]
	pub struct AbortSignal {
		receiver: tokio::sync::watch::Receiver<bool>,
	}

	#[uniffi::export]
	impl AbortSignal {
		pub fn aborted(&self) -> bool {
			*self.receiver.borrow()
		}
	}

	impl AbortSignal {
		pub(super) fn into_future(self) -> AbortSignalFuture<impl Future<Output = AbortedError>> {
			AbortSignalFuture::Some {
				fut: async move {
					let mut receiver = self.receiver;
					loop {
						if *receiver.borrow() {
							return AbortedError;
						}
						if receiver.changed().await.is_err() {
							// sender dropped, treat as not aborted
							return AbortedError;
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
}

mod managed {
	use pin_project_lite::pin_project;
	use std::{sync::Arc, task::Poll};

	use crate::{Error, error::AbortedError, runtime::CommanderFutHandle};

	use super::{abortable::*, pausable::*};

	#[derive(uniffi::Record)]
	pub struct ManagedFuture {
		pub abort_signal: Option<Arc<AbortSignal>>,
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
}
