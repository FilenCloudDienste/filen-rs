use std::num::NonZeroU32;

use bytes::Bytes;
use futures::{Stream, StreamExt, future::BoxFuture};
use tower::{Layer, Service};

use crate::auth::http::limit::RateLimiter;

use super::{BYTES_PER_KILOBYTE, BYTES_PER_KILOBYTE_USIZE};

pub(crate) fn new_download_bandwidth_limiter(kbps: NonZeroU32) -> RateLimiter {
	RateLimiter::new(kbps)
}

#[derive(Clone)]
pub(crate) struct DownloadBandwidthLimiterLayer {
	pub(crate) limiter: RateLimiter,
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
	pub(crate) fn new(limiter: RateLimiter) -> Self {
		Self { limiter }
	}
}

#[derive(Clone)]
pub(crate) struct DownloadBandwidthLimiterService<S> {
	inner: S,
	limiter: RateLimiter,
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
			Ok(map_download_response(limiter, response))
		})
	}
}

fn map_download_response(limiter: RateLimiter, response: reqwest::Response) -> reqwest::Response {
	limit_download_response(limiter, response)
}

pub fn limit_download_response(
	limiter: RateLimiter,
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
	limiter: RateLimiter,
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
						match limiter.acquire_amount(chunk_size.div_ceil(BYTES_PER_KILOBYTE)).await {
							Ok(()) => {
								yield Ok(bytes);
								break;
							},
							Err(capacity_err) => {
								let acquired = acquire_reported_capacity(&limiter, capacity_err).await;
								let bytes_capacity = (acquired.get() as usize) * BYTES_PER_KILOBYTE_USIZE;
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

// A concurrent rate change can swap in a governor with a smaller burst between the
// failed acquisition that produced `err` and the follow-up acquisition here, so the
// reported capacity cannot be assumed to still fit; retry with each newly reported
// capacity instead of panicking. Every reported capacity is at least 1 and strictly
// below the amount that was just requested, so the retries terminate.
// Returns the amount of kilobytes actually acquired.
async fn acquire_reported_capacity(
	limiter: &RateLimiter,
	mut err: governor::InsufficientCapacity,
) -> NonZeroU32 {
	loop {
		let amount = NonZeroU32::new(err.0).expect("minimum allowed capacity should be non-zero");
		match limiter.acquire_amount(amount).await {
			Ok(()) => return amount,
			Err(new_err) => err = new_err,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn reported_capacity_acquired_without_rate_change() {
		let limiter = RateLimiter::default();
		limiter.change_rate_per_sec(NonZeroU32::new(10)).await;
		let err = limiter
			.acquire_amount(NonZeroU32::new(1000).unwrap())
			.await
			.expect_err("request should exceed burst capacity");
		let acquired = acquire_reported_capacity(&limiter, err).await;
		assert_eq!(acquired.get(), err.0);
	}

	#[tokio::test]
	async fn retry_acquires_shrunken_capacity_after_rate_change() {
		let limiter = RateLimiter::default();
		limiter.change_rate_per_sec(NonZeroU32::new(10)).await;
		let err = limiter
			.acquire_amount(NonZeroU32::new(1000).unwrap())
			.await
			.expect_err("request should exceed burst capacity");
		// mimic a concurrent set_bandwidth_limits lowering the rate between the
		// failed acquisition and the follow-up one
		limiter.change_rate_per_sec(NonZeroU32::new(2)).await;
		let acquired = acquire_reported_capacity(&limiter, err).await;
		assert!(acquired.get() < err.0);
		assert_eq!(acquired.get(), 2);
	}

	#[tokio::test]
	async fn retry_succeeds_when_limit_removed_after_failed_acquisition() {
		let limiter = RateLimiter::default();
		limiter.change_rate_per_sec(NonZeroU32::new(10)).await;
		let err = limiter
			.acquire_amount(NonZeroU32::new(1000).unwrap())
			.await
			.expect_err("request should exceed burst capacity");
		limiter.change_rate_per_sec(None).await;
		let acquired = acquire_reported_capacity(&limiter, err).await;
		assert_eq!(acquired.get(), err.0);
	}
}
