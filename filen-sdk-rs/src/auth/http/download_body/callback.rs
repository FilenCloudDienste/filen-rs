use std::task::{Context, Poll};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use std::time::Instant;

use http_body::Body;
use tower::{Layer, Service};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::std::Instant;

use crate::{Error, ErrorKind, consts::CALLBACK_INTERVAL};

use super::super::retry::RetryError;

pub(crate) struct DownloadWithCallbackLayer<FRef, F> {
	callback: Option<FRef>,
	_phantom: std::marker::PhantomData<F>,
}

impl<FRef, F> Clone for DownloadWithCallbackLayer<FRef, F>
where
	FRef: Clone,
{
	fn clone(&self) -> Self {
		Self {
			callback: self.callback.clone(),
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<FRef, F> DownloadWithCallbackLayer<FRef, F>
where
	F: Fn(u64, Option<u64>),
{
	pub(crate) fn new(callback: Option<FRef>) -> Self {
		Self {
			callback,
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, FRef, F> Layer<S> for DownloadWithCallbackLayer<FRef, F>
where
	FRef: Clone,
{
	type Service = DownloadWithCallbackService<S, FRef, F>;

	fn layer(&self, inner: S) -> Self::Service {
		DownloadWithCallbackService {
			inner,
			callback: self.callback.clone(),
			_phantom: std::marker::PhantomData,
		}
	}
}

pub(crate) struct DownloadWithCallbackService<S, FRef, F> {
	inner: S,
	callback: Option<FRef>,
	_phantom: std::marker::PhantomData<F>,
}

impl<S, FRef, F> Clone for DownloadWithCallbackService<S, FRef, F>
where
	S: Clone,
	FRef: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			callback: self.callback.clone(),
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, Req, FRef, F> Service<Req> for DownloadWithCallbackService<S, FRef, F>
where
	S: Service<Req, Response = reqwest::Response, Error = RetryError<Error>>,
	FRef: AsRef<F> + Clone,
	F: Fn(u64, Option<u64>),
{
	type Response = Vec<u8>;
	type Error = RetryError<Error>;
	type Future = DownloadWithCallbackFuture<S::Future, FRef, F>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let progress_callback = self.callback.clone();
		let fut = self.inner.call(req);

		DownloadWithCallbackFuture::new(fut, progress_callback)
	}
}

#[pin_project::pin_project(project = DownloadWithCallbackFutureStateProj)]
enum DownloadWithCallbackFutureState<S> {
	AwaitingInner(#[pin] S),
	ReadingBody {
		#[pin]
		body: reqwest::Body,
		collected: Vec<u8>,
		last_update_time: Instant,
	},
}

#[pin_project::pin_project(project = DownloadWithCallbackFutureProj)]
pub(crate) struct DownloadWithCallbackFuture<S, FRef, F> {
	callback: Option<FRef>,
	#[pin]
	state: DownloadWithCallbackFutureState<S>,
	_phantom: std::marker::PhantomData<F>,
}

impl<S, FRef, F> DownloadWithCallbackFuture<S, FRef, F>
where
	S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
	FRef: AsRef<F>,
	F: Fn(u64, Option<u64>),
{
	fn new(inner: S, callback: Option<FRef>) -> Self {
		Self {
			callback,
			state: DownloadWithCallbackFutureState::AwaitingInner(inner),
			_phantom: std::marker::PhantomData,
		}
	}
}

impl<S, FRef, F> Future for DownloadWithCallbackFuture<S, FRef, F>
where
	S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
	FRef: AsRef<F>,
	F: Fn(u64, Option<u64>),
{
	type Output = Result<Vec<u8>, RetryError<Error>>;

	fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		let mut this = self.project();
		loop {
			match this.state.as_mut().project() {
				DownloadWithCallbackFutureStateProj::AwaitingInner(fut) => match fut.poll(cx) {
					Poll::Ready(Ok(response)) => {
						let real_content_length = response
							.headers()
							.get("X-Cl")
							.and_then(|h| h.to_str().ok().and_then(|h| str::parse::<u64>(h).ok()));
						let content_length: usize = real_content_length
							.unwrap_or_default()
							.try_into()
							.map_err(|e| {
								RetryError::NoRetry(Error::custom_with_source(
									ErrorKind::InsufficientMemory,
									e,
									Some("content length too large"),
								))
							})?;
						let (_, body) = http::Response::from(response).into_parts();
						this.state
							.set(DownloadWithCallbackFutureState::ReadingBody {
								body,
								collected: Vec::with_capacity(content_length),
								last_update_time: Instant::now(),
							});
					}
					Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
					Poll::Pending => return Poll::Pending,
				},
				DownloadWithCallbackFutureStateProj::ReadingBody {
					mut body,
					collected,
					last_update_time,
				} => loop {
					match body.as_mut().poll_frame(cx) {
						Poll::Ready(Some(Ok(frame))) => {
							if let Some(chunk) = frame.data_ref() {
								collected.extend_from_slice(chunk);
							}
							if last_update_time.elapsed() >= CALLBACK_INTERVAL
								&& let Some(callback) = &this.callback
							{
								callback.as_ref()(collected.len() as u64, None);
								*last_update_time = Instant::now();
							}
						}
						Poll::Ready(Some(Err(e))) => {
							if e.is_timeout() {
								return Poll::Ready(Err(RetryError::Retry(Error::from(e))));
							}
							return Poll::Ready(Err(RetryError::NoRetry(Error::from(e))));
						}
						Poll::Ready(None) => {
							if let Some(callback) = this.callback {
								callback.as_ref()(collected.len() as u64, None);
							}
							return Poll::Ready(Ok(std::mem::take(collected)));
						}
						Poll::Pending => return Poll::Pending,
					}
				},
			}
		}
	}
}
