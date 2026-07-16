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
					Err(e) => Err(RetryError::from_retryable(e.is_timeout(), e.into())),
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

	use crate::{Error, ErrorKind, auth::http::retry::RetryError};

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
								collected: Vec::try_with_capacity(
									size_to_alloc.try_into().map_err(|e| {
										RetryError::NoRetry(Error::custom_with_source(
											ErrorKind::InsufficientMemory,
											e,
											Some(
												"Could not convert size hint to usize for body allocation"
													.to_string(),
											),
										))
									})?,
								)
								.map_err(|e| {
									RetryError::NoRetry(Error::custom_with_source(
										ErrorKind::InsufficientMemory,
										e,
										Some("Failed to allocate memory for body".to_string()),
									))
								})?,
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
								// A mid-body read timeout must fail fast, not retry: the request
								// timeout classification (see execute_request) treats timeouts as
								// non-retryable, and retrying a stalled stream would burn another
								// full read timeout per attempt.
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

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests {
	use std::time::Duration;

	use tokio::{
		io::{AsyncReadExt, AsyncWriteExt},
		net::TcpListener,
	};

	use super::native::DownloadBodyFuture;
	use crate::auth::http::{ClientConfig, retry::RetryError};

	/// A host that streams part of a body then stalls trips the read timeout mid-body. The
	/// documented HTTP policy is to fail fast on timeouts, so this must be classified NoRetry —
	/// not retried for another full read timeout (which, over MAX_RETRIES at 300s spacing, would
	/// let a single stalled chunk hang for ~55 minutes on mobile).
	#[tokio::test]
	async fn mid_body_read_timeout_is_not_retryable() {
		let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = listener.local_addr().unwrap();
		tokio::spawn(async move {
			let (mut socket, _) = listener.accept().await.unwrap();
			let mut buf = [0u8; 1024];
			let _ = socket.read(&mut buf).await;
			// Promise 100 bytes, send only a few, then stall forever so the client's body read
			// (not the time-to-first-byte) is what trips the read timeout.
			let head = "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\npart";
			socket.write_all(head.as_bytes()).await.unwrap();
			socket.flush().await.unwrap();
			tokio::time::sleep(Duration::from_secs(30)).await;
		});

		let client = ClientConfig::default()
			.with_read_timeout(Some(Duration::from_millis(300)))
			.build_reqwest_client()
			.unwrap();
		let response = client.get(format!("http://{addr}/")).send().await.unwrap();

		let fut =
			DownloadBodyFuture::new(async move { Ok::<_, RetryError<crate::Error>>(response) });
		match fut.await {
			Err(RetryError::NoRetry(_)) => {}
			Err(RetryError::Retry(e)) => {
				panic!("a mid-body read timeout must be NoRetry (fail-fast), but was Retry: {e}")
			}
			Ok(_) => panic!("the body read must not complete while the server stalls mid-body"),
		}
	}
}
