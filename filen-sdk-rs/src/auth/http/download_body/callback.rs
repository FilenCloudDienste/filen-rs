use std::task::{Context, Poll};

use crate::Error;

use super::super::retry::RetryError;

pub(crate) struct DownloadWithCallbackLayer<'a, F> {
	callback: Option<&'a F>,
	max_body_len: Option<usize>,
}

impl<F> Clone for DownloadWithCallbackLayer<'_, F> {
	fn clone(&self) -> Self {
		Self {
			callback: self.callback,
			max_body_len: self.max_body_len,
		}
	}
}

impl<'a, F> DownloadWithCallbackLayer<'a, F>
where
	F: Fn(u64, Option<u64>),
{
	pub(crate) fn new(callback: Option<&'a F>) -> Self {
		Self {
			callback,
			max_body_len: None,
		}
	}

	/// Caps the collected response body at `max_body_len` bytes, failing the request if the
	/// streamed body grows past it. Used for file-chunk downloads to bound a single chunk's buffer:
	/// without the cap a misbehaving/compromised egest node could stream a multi-GiB body for one
	/// chunk and bypass the file-IO memory budget, OOMing the client. The caller picks a bound that
	/// clears the largest legitimate chunk body across encryption versions.
	pub(crate) fn with_max_body_len(mut self, max_body_len: usize) -> Self {
		self.max_body_len = Some(max_body_len);
		self
	}
}

impl<'a, S, F> Layer<S> for DownloadWithCallbackLayer<'a, F> {
	type Service = DownloadWithCallbackService<'a, S, F>;

	fn layer(&self, inner: S) -> Self::Service {
		DownloadWithCallbackService {
			inner,
			callback: self.callback,
			max_body_len: self.max_body_len,
		}
	}
}

pub(crate) struct DownloadWithCallbackService<'a, S, F> {
	inner: S,
	callback: Option<&'a F>,
	max_body_len: Option<usize>,
}

impl<'a, S, F> Clone for DownloadWithCallbackService<'a, S, F>
where
	S: Clone,
{
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			callback: self.callback,
			max_body_len: self.max_body_len,
		}
	}
}

impl<'a, S, Req, F> Service<Req> for DownloadWithCallbackService<'a, S, F>
where
	S: Service<Req, Response = reqwest::Response, Error = RetryError<Error>>,
	S::Future: 'a,
	F: Fn(u64, Option<u64>),
{
	type Response = Vec<u8>;
	type Error = RetryError<Error>;
	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	type Future = DownloadWithCallbackFuture<'a, S::Future, F>;
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	type Future = DownloadWithCallbackFuture<'a>;

	fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let fut = self.inner.call(req);

		DownloadWithCallbackFuture::new(fut, self.callback, self.max_body_len)
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
		pub(super) fn new<SFut, F>(
			inner: SFut,
			callback: Option<&'a F>,
			max_body_len: Option<usize>,
		) -> Self
		where
			SFut: Future<Output = Result<reqwest::Response, RetryError<Error>>> + 'a,
			F: Fn(u64, Option<u64>) + 'a,
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

				let mut collected = Vec::try_with_capacity(content_length).map_err(|e| {
					RetryError::NoRetry(Error::custom_with_source(
						ErrorKind::InsufficientMemory,
						e,
						Some("failed to allocate memory for response body"),
					))
				})?;
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
					if let Some(max_body_len) = max_body_len
						&& collected.len() > max_body_len
					{
						return Err(RetryError::NoRetry(Error::custom(
							ErrorKind::Response,
							"download body exceeded the maximum expected chunk size",
						)));
					}
					if last_update_time.elapsed() >= CALLBACK_INTERVAL
						&& let Some(callback) = &callback
					{
						callback(collected.len() as u64, real_content_length);
						last_update_time = Instant::now();
					}
				}
				// Fire a final callback with the full collected length, mirroring the native
				// path's `Poll::Ready(None)` branch. Without this, a chunk that arrives within
				// CALLBACK_INTERVAL reports no progress, so fast wasm downloads sit at 0% until
				// completion.
				if let Some(callback) = &callback {
					callback(collected.len() as u64, real_content_length);
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
	pub(crate) struct DownloadWithCallbackFuture<'a, S, F> {
		callback: Option<&'a F>,
		max_body_len: Option<usize>,
		#[pin]
		state: DownloadWithCallbackFutureState<S>,
	}

	impl<'a, S, F> DownloadWithCallbackFuture<'a, S, F>
	where
		S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
		F: Fn(u64, Option<u64>),
	{
		pub(crate) fn new(inner: S, callback: Option<&'a F>, max_body_len: Option<usize>) -> Self {
			Self {
				callback,
				max_body_len,
				state: DownloadWithCallbackFutureState::AwaitingInner(inner),
			}
		}
	}

	impl<S, F> Future for DownloadWithCallbackFuture<'_, S, F>
	where
		S: Future<Output = Result<reqwest::Response, RetryError<Error>>>,
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
							let collected =
								Vec::try_with_capacity(content_length).map_err(|e| {
									RetryError::NoRetry(Error::custom_with_source(
										ErrorKind::InsufficientMemory,
										e,
										Some("failed to allocate memory for response body"),
									))
								})?;
							this.state
								.set(DownloadWithCallbackFutureState::ReadingBody {
									body,
									collected,
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
									if let Some(max_body_len) = *this.max_body_len
										&& collected.len() > max_body_len
									{
										return Poll::Ready(Err(RetryError::NoRetry(
											Error::custom(
												ErrorKind::Response,
												"download body exceeded the maximum expected chunk size",
											),
										)));
									}
								}
								if last_update_time.elapsed() >= CALLBACK_INTERVAL
									&& let Some(callback) = &this.callback
								{
									callback(collected.len() as u64, *real_content_length);
									*last_update_time = Instant::now();
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
								if let Some(callback) = this.callback {
									callback(collected.len() as u64, *real_content_length);
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

#[cfg(all(test, not(all(target_family = "wasm", target_os = "unknown"))))]
mod tests {
	use futures::executor::block_on;

	use super::DownloadWithCallbackFuture;
	use crate::{Error, auth::http::retry::RetryError};

	// A concrete callback type so `None` can be typed without a live callback.
	type NoCallback = fn(u64, Option<u64>);

	fn response_with_body(body: Vec<u8>, x_content_length: &str) -> reqwest::Response {
		let http_resp = http::Response::builder()
			.header("X-Cl", x_content_length)
			.body(body)
			.expect("valid response");
		reqwest::Response::from(http_resp)
	}

	fn run(response: reqwest::Response, max_body_len: Option<usize>) -> Result<Vec<u8>, Error> {
		let fut = DownloadWithCallbackFuture::new(
			std::future::ready(Ok::<_, RetryError<Error>>(response)),
			None::<&NoCallback>,
			max_body_len,
		);
		block_on(fut).map_err(|e| match e {
			RetryError::Retry(e) | RetryError::NoRetry(e) => e,
		})
	}

	/// A streamed body larger than the cap must fail instead of being buffered unbounded — this is
	/// the memory-budget bypass a misbehaving egest node could otherwise exploit (X-Cl is
	/// server-controlled, here understating the real body).
	#[test]
	fn body_exceeding_cap_errors() {
		let cap = 100;
		let response = response_with_body(vec![7u8; cap + 50], "20");
		assert!(
			run(response, Some(cap)).is_err(),
			"a body over the cap must error"
		);
	}

	/// A body within the cap is collected normally.
	#[test]
	fn body_within_cap_succeeds() {
		let cap = 1000;
		let response = response_with_body(vec![7u8; 200], "200");
		let body = run(response, Some(cap)).expect("a body within the cap must succeed");
		assert_eq!(body.len(), 200);
	}

	/// With no cap the full body is collected, preserving the behavior of the non-chunk callers.
	#[test]
	fn uncapped_collects_full_body() {
		let response = response_with_body(vec![7u8; 5000], "5000");
		let body = run(response, None).expect("an uncapped download must succeed");
		assert_eq!(body.len(), 5000);
	}
}
