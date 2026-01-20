use std::{
	marker::PhantomData,
	task::{Context, Poll},
};

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use futures::future::LocalBoxFuture;
use tower::{Layer, Service};

use crate::Error;

use super::super::retry::RetryError;

#[derive(Clone, Default)]
pub(crate) struct DownloadLayer<'a>(PhantomData<&'a ()>);

impl<'a> DownloadLayer<'a> {
	pub(crate) fn new() -> Self {
		Self(PhantomData)
	}
}

impl<'a, S> Layer<S> for DownloadLayer<'a> {
	type Service = DownloadService<'a, S>;

	fn layer(&self, inner: S) -> Self::Service {
		DownloadService {
			inner,
			_lifetime: PhantomData,
		}
	}
}

#[derive(Clone, Default)]
pub(crate) struct DownloadService<'a, S> {
	inner: S,
	_lifetime: PhantomData<&'a ()>,
}

impl<'a, S, Req> Service<Req> for DownloadService<'a, S>
where
	S: Service<Req, Response = reqwest::Response, Error = RetryError<Error>>,
	S::Future: 'a,
{
	type Response = Vec<u8>;
	type Error = RetryError<Error>;
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	type Future = DownloadBodyFuture<'a, S::Future>;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	type Future = LocalBoxFuture<'a, Result<Self::Response, Self::Error>>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let fut = self.inner.call(req);

		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			DownloadBodyFuture::new(fut)
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			Box::pin(async move {
				match fut.await?.bytes().await {
					Ok(body) => Ok(Vec::from(body)),
					Err(e) => {
						if e.is_timeout() {
							Err(RetryError::Retry(e.into()))
						} else {
							Err(RetryError::NoRetry(e.into()))
						}
					}
				}
			})
		}
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
mod native {
	use std::{
		marker::PhantomData,
		pin::Pin,
		task::{Context, Poll},
	};

	use http_body::Body;
	use std::future::Future;

	use crate::{Error, auth::http::retry::RetryError};

	#[pin_project::pin_project(project = DownloadBodyFutureProj)]
	pub(crate) enum DownloadBodyFuture<'a, S> {
		AwaitingInner {
			#[pin]
			fut: S,
			_lifetime: PhantomData<&'a ()>,
		},
		ReadingBody {
			#[pin]
			body: reqwest::Body,
			collected: Vec<u8>,
		},
	}

	impl<'a, S> DownloadBodyFuture<'a, S> {
		pub(super) fn new(inner: S) -> Self {
			Self::AwaitingInner {
				fut: inner,
				_lifetime: PhantomData,
			}
		}
	}

	impl<'a, S> Future for DownloadBodyFuture<'a, S>
	where
		S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
	{
		type Output = Result<Vec<u8>, RetryError<Error>>;

		fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
			loop {
				let this = self.as_mut().project();
				match this {
					DownloadBodyFutureProj::AwaitingInner { fut, .. } => match fut.poll(cx) {
						Poll::Ready(Ok(response)) => {
							let (_, body) = http::Response::from(response).into_parts();
							let sizes = body.size_hint();
							let size_to_alloc = sizes.exact().unwrap_or_else(|| {
								let upper = sizes.upper().unwrap_or(sizes.lower());
								if upper / 2 < sizes.lower() {
									sizes.lower()
								} else {
									upper
								}
							});
							self.set(DownloadBodyFuture::ReadingBody {
								body,
								collected: Vec::with_capacity(size_to_alloc as usize),
							});
						}
						Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
						Poll::Pending => return Poll::Pending,
					},
					DownloadBodyFutureProj::ReadingBody {
						mut body,
						collected,
					} => loop {
						match body.as_mut().poll_frame(cx) {
							Poll::Ready(Some(Ok(frame))) => {
								if let Some(chunk) = frame.data_ref() {
									collected.extend_from_slice(chunk);
								}
							}
							Poll::Ready(Some(Err(e))) => {
								if e.is_timeout() {
									return Poll::Ready(Err(RetryError::Retry(Error::from(e))));
								}
								return Poll::Ready(Err(RetryError::NoRetry(Error::from(e))));
							}
							Poll::Ready(None) => {
								return Poll::Ready(Ok(std::mem::take(collected)));
							}
							Poll::Pending => {
								return Poll::Pending;
							}
						}
					},
				}
			}
		}
	}
}

#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(crate) use native::*;
