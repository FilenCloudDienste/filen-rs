use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::Error;

use super::super::retry::RetryError;

#[derive(Clone, Default)]
pub(crate) struct DownloadLayer;

impl<S> Layer<S> for DownloadLayer {
	type Service = DownloadService<S>;

	fn layer(&self, inner: S) -> Self::Service {
		DownloadService { inner }
	}
}

#[derive(Clone, Default)]
pub(crate) struct DownloadService<S> {
	inner: S,
}

impl<S, Req> Service<Req> for DownloadService<S>
where
	S: Service<Req, Response = reqwest::Response, Error = RetryError<Error>>,
	S::Future: Send + 'static,
{
	type Response = Vec<u8>;
	type Error = RetryError<Error>;
	type Future = DownloadBodyFuture<S::Future>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let fut = self.inner.call(req);

		DownloadBodyFuture::new(fut)
	}
}

use http_body::Body;
use std::future::Future;
use std::pin::Pin;

#[pin_project::pin_project(project = DownloadBodyFutureProj)]
pub(crate) enum DownloadBodyFuture<S> {
	AwaitingInner(#[pin] S),
	ReadingBody {
		#[pin]
		body: reqwest::Body,
		collected: Vec<u8>,
	},
}

impl<S> DownloadBodyFuture<S> {
	fn new(inner: S) -> Self {
		Self::AwaitingInner(inner)
	}
}

impl<S> Future for DownloadBodyFuture<S>
where
	S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
{
	type Output = Result<Vec<u8>, RetryError<Error>>;

	fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
		loop {
			let this = self.as_mut().project();
			match this {
				DownloadBodyFutureProj::AwaitingInner(fut) => match fut.poll(cx) {
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
