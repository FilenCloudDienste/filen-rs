use std::{num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use futures::{Stream, StreamExt, future::BoxFuture};
use governor::{Quota, RateLimiter};
use tower::{Layer, Service};

use super::{BYTES_PER_KILOBYTE, BYTES_PER_KILOBYTE_USIZE, BandwidthLimiter};

pub(crate) fn new_download_bandwidth_limiter(kbps: NonZeroU32) -> BandwidthLimiter {
	RateLimiter::direct(Quota::per_second(kbps))
}

#[derive(Clone)]
pub(crate) struct DownloadBandwidthLimiterLayer {
	limiter: Option<Arc<BandwidthLimiter>>,
}

impl<S> Layer<S> for DownloadBandwidthLimiterLayer {
	type Service = DownloadBandwidthLimiterService<S>;

	fn layer(&self, service: S) -> Self::Service {
		DownloadBandwidthLimiterService {
			inner: service,
			limiter: self.limiter.clone(),
		}
	}
}

impl DownloadBandwidthLimiterLayer {
	pub(crate) fn new(limiter: Option<Arc<BandwidthLimiter>>) -> Self {
		Self { limiter }
	}
}

#[derive(Clone)]
pub(crate) struct DownloadBandwidthLimiterService<S> {
	inner: S,
	limiter: Option<Arc<BandwidthLimiter>>,
}

impl<S, Req> Service<Req> for DownloadBandwidthLimiterService<S>
where
	S: Service<Req, Response = reqwest::Response>,
	S::Future: Send + 'static,
{
	type Response = S::Response;
	type Error = S::Error;
	// can avoid the box here with TAITs https://github.com/rust-lang/rust/issues/63063
	// once they are stable
	type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

	fn poll_ready(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Req) -> Self::Future {
		let fut = self.inner.call(req);
		let limiter = self.limiter.clone();
		Box::pin(async move {
			let response = fut.await?;
			match limiter {
				Some(limiter) => Ok(map_download_response(limiter, response)),
				None => Ok(response),
			}
		})
	}
}

fn map_download_response(
	limiter: Arc<BandwidthLimiter>,
	response: reqwest::Response,
) -> reqwest::Response {
	limit_download_response(limiter, response)
}

pub fn limit_download_response(
	limiter: Arc<BandwidthLimiter>,
	response: reqwest::Response,
) -> reqwest::Response {
	let http_response: http::Response<reqwest::Body> = response.into();
	let (parts, body) = http_response.into_parts();
	if body.as_bytes().is_some() {
		return http::Response::from_parts(parts, body).into();
	}
	let body_stream = http_body_util::BodyDataStream::new(body);
	let limited_stream = limit_response_stream(body_stream, limiter);
	let limited_body = reqwest::Body::wrap_stream(limited_stream);
	let limited_response = http::Response::from_parts(parts, limited_body);
	limited_response.into()
}

fn limit_response_stream<S, E>(
	stream: S,
	limiter: Arc<BandwidthLimiter>,
) -> impl Stream<Item = Result<Bytes, E>>
where
	S: Stream<Item = Result<Bytes, E>>,
{
	async_stream::stream! {
		tokio::pin!(stream);

		while let Some(item) = stream.next().await {
			match item {
				Ok(mut bytes) => {
					while let Some(chunk_size)= NonZeroU32::new(bytes.len().min(u32::MAX as usize) as u32){
						match limiter.until_n_ready(chunk_size.div_ceil(BYTES_PER_KILOBYTE)).await {
							Ok(()) => {
								yield Ok(bytes);
								break;
							},
							Err(capacity_err) => {
								limiter.until_n_ready(NonZeroU32::new(capacity_err.0).expect("minimum allowed capacity should be non-zero")).await.expect("cannot be more than the error capacity");
								let bytes_capacity = (capacity_err.0 as usize) * BYTES_PER_KILOBYTE_USIZE;
								let bytes = bytes.split_to(bytes_capacity);
								yield Ok(bytes);
							},
						};
					}

				}
				Err(e) => yield Err(e),
			}
		}
	}
}
