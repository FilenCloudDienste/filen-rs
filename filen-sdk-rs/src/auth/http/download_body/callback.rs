use std::task::{Context, Poll};

use crate::Error;

use super::super::retry::RetryError;

pub(crate) struct DownloadWithCallbackLayer<'a, FRef, F>
where
	FRef: 'a,
{
	callback: Option<FRef>,
	_phantom: std::marker::PhantomData<F>,
	_lifetime: std::marker::PhantomData<&'a ()>,
}

impl<FRef, F> Clone for DownloadWithCallbackLayer<'_, FRef, F>
where
	FRef: Clone,
{
	fn clone(&self) -> Self {
		Self {
			callback: self.callback.clone(),
			_phantom: self._phantom,
			_lifetime: self._lifetime,
		}
	}
}

impl<'a, FRef, F> DownloadWithCallbackLayer<'a, FRef, F>
where
	F: Fn(u64, Option<u64>),
{
	pub(crate) fn new(callback: Option<FRef>) -> Self
	where
		FRef: 'a,
	{
		Self {
			callback,
			_phantom: std::marker::PhantomData,
			_lifetime: std::marker::PhantomData,
		}
	}
}

impl<'a, S, FRef, F> Layer<S> for DownloadWithCallbackLayer<'a, FRef, F>
where
	FRef: Clone,
{
	type Service = DownloadWithCallbackService<'a, S, FRef, F>;

	fn layer(&self, inner: S) -> Self::Service {
		DownloadWithCallbackService {
			inner,
			callback: self.callback.clone(),
			_phantom: self._phantom,
			_lifetime: self._lifetime,
		}
	}
}

pub(crate) struct DownloadWithCallbackService<'a, S, FRef, F> {
	inner: S,
	callback: Option<FRef>,
	_phantom: std::marker::PhantomData<F>,
	_lifetime: std::marker::PhantomData<&'a ()>,
}

impl<'a, S, FRef, F> Clone for DownloadWithCallbackService<'a, S, FRef, F>
where
	S: Clone,
	FRef: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			callback: self.callback.clone(),
			_phantom: self._phantom,
			_lifetime: self._lifetime,
		}
	}
}

impl<'a, S, Req, FRef, F> Service<Req> for DownloadWithCallbackService<'a, S, FRef, F>
where
	S: Service<Req, Response = reqwest::Response, Error = RetryError<Error>>,
	S::Future: 'a,
	FRef: AsRef<F> + Clone + 'a,
	F: Fn(u64, Option<u64>),
{
	type Response = Vec<u8>;
	type Error = RetryError<Error>;
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	type Future = DownloadWithCallbackFuture<S::Future, FRef, F>;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	type Future = DownloadWithCallbackFuture<'a>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let progress_callback = self.callback.clone();
		let fut = self.inner.call(req);

		DownloadWithCallbackFuture::new(fut, progress_callback)
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
mod boxed {
	use futures::{StreamExt, future::LocalBoxFuture};
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	use wasmtimer::std::Instant;

	use crate::{Error, ErrorKind, auth::http::retry::RetryError};

	use crate::consts::CALLBACK_INTERVAL;

	#[pin_project::pin_project]
	pub(crate) struct DownloadWithCallbackFuture<'a> {
		#[pin]
		fut: LocalBoxFuture<'a, Result<Vec<u8>, RetryError<Error>>>,
	}

	impl<'a> DownloadWithCallbackFuture<'a> {
		pub(super) fn new<SFut, FRef, F>(inner: SFut, callback: Option<FRef>) -> Self
		where
			SFut: Future<Output = Result<reqwest::Response, RetryError<Error>>> + 'a,
			FRef: AsRef<F> + Clone + 'a,
			F: Fn(u64, Option<u64>),
		{
			let fut = Box::pin(async move {
				let resp = inner.await?;

				let real_content_length = resp
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

				let mut collected = Vec::with_capacity(content_length);
				let mut stream = resp.bytes_stream();
				let mut last_update_time = Instant::now();
				while let Some(chunk_res) = stream.next().await {
					let chunk = match chunk_res {
						Ok(c) => c,
						Err(e) => {
							if e.is_timeout() {
								return Err(RetryError::Retry(e.into()));
							}
							return Err(RetryError::NoRetry(e.into()));
						}
					};
					collected.extend_from_slice(&chunk);
					if last_update_time.elapsed() >= CALLBACK_INTERVAL
						&& let Some(callback) = &callback
					{
						callback.as_ref()(collected.len() as u64, real_content_length);
						last_update_time = Instant::now();
					}
				}
				Ok(collected)
			});
			DownloadWithCallbackFuture { fut }
		}
	}

	impl Future for DownloadWithCallbackFuture<'_> {
		type Output = Result<Vec<u8>, RetryError<Error>>;

		fn poll(
			self: std::pin::Pin<&mut Self>,
			cx: &mut std::task::Context<'_>,
		) -> std::task::Poll<Self::Output> {
			self.project().fut.poll(cx)
		}
	}
}
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(crate) use boxed::*;

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod native {
	use std::{
		task::{Context, Poll},
		time::Instant,
	};

	use http_body::Body;

	use crate::{Error, ErrorKind, auth::http::retry::RetryError, consts::CALLBACK_INTERVAL};

	#[pin_project::pin_project(project = DownloadWithCallbackFutureStateProj)]
	enum DownloadWithCallbackFutureState<S> {
		AwaitingInner(#[pin] S),
		ReadingBody {
			#[pin]
			body: reqwest::Body,
			collected: Vec<u8>,
			last_update_time: Instant,
			real_content_length: Option<u64>,
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
		pub(crate) fn new(inner: S, callback: Option<FRef>) -> Self {
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
							let real_content_length =
								response.headers().get("X-Cl").and_then(|h| {
									h.to_str().ok().and_then(|h| str::parse::<u64>(h).ok())
								});
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
									real_content_length,
								});
						}
						Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
						Poll::Pending => return Poll::Pending,
					},
					DownloadWithCallbackFutureStateProj::ReadingBody {
						mut body,
						collected,
						last_update_time,
						real_content_length,
					} => loop {
						match body.as_mut().poll_frame(cx) {
							Poll::Ready(Some(Ok(frame))) => {
								if let Some(chunk) = frame.data_ref() {
									collected.extend_from_slice(chunk);
								}
								if last_update_time.elapsed() >= CALLBACK_INTERVAL
									&& let Some(callback) = &this.callback
								{
									callback.as_ref()(collected.len() as u64, *real_content_length);
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
									callback.as_ref()(collected.len() as u64, *real_content_length);
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
}
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use native::*;
use tower::{Layer, Service};
