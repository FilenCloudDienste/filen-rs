use std::num::NonZeroU32;

use bytes::Bytes;
use futures::Stream;
use reqwest::Body;
use tower::{Layer, Service};

use crate::{Error, ErrorKind};

use super::{
	super::{Request, limit::RateLimiter},
	BYTES_PER_KILOBYTE_USIZE,
};

const BANDWIDTH_CHUNK_SIZE_KB: NonZeroU32 = NonZeroU32::new(16).unwrap();

pub(crate) fn new_upload_bandwidth_limiter(kbps: NonZeroU32) -> Result<RateLimiter, Error> {
	if kbps < BANDWIDTH_CHUNK_SIZE_KB {
		return Err(Error::custom(
			ErrorKind::InvalidState,
			format!(
				"upload bandwidth limit must be at least {} kilobytes per second",
				BANDWIDTH_CHUNK_SIZE_KB
			),
		));
	}

	Ok(RateLimiter::new(kbps))
}

pub(crate) async fn set_upload_bandwidth_limit(limiter: &RateLimiter, kbps: Option<NonZeroU32>) {
	limiter
		.change_rate_per_sec(kbps.map(clamp_upload_bandwidth_kbps))
		.await;
}

// the request body stream acquires up to BANDWIDTH_CHUNK_SIZE_KB per permit, so any live
// quota below that would exceed the governor's burst capacity on every acquisition
fn clamp_upload_bandwidth_kbps(kbps: NonZeroU32) -> NonZeroU32 {
	kbps.max(BANDWIDTH_CHUNK_SIZE_KB)
}

#[derive(Clone)]
pub(crate) struct UploadBandwidthLimiterLayer<'a> {
	limiter: &'a RateLimiter,
}

impl<'a, S> Layer<S> for UploadBandwidthLimiterLayer<'a> {
	type Service = UploadBandwidthLimiterService<'a, S>;

	fn layer(&self, service: S) -> Self::Service {
		UploadBandwidthLimiterService {
			inner: service,
			limiter: self.limiter,
		}
	}
}

impl<'a> UploadBandwidthLimiterLayer<'a> {
	pub(crate) fn new(limiter: &'a RateLimiter) -> Self {
		Self { limiter }
	}
}

#[derive(Clone)]
pub(crate) struct UploadBandwidthLimiterService<'a, S> {
	inner: S,
	limiter: &'a RateLimiter,
}

impl<'a, S> Service<Request<bytes::Bytes, reqwest::Url>> for UploadBandwidthLimiterService<'a, S>
where
	S: Service<reqwest::RequestBuilder>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = S::Future;

	fn poll_ready(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx)
	}

	fn call(&mut self, req: Request<bytes::Bytes, reqwest::Url>) -> Self::Future {
		let req = req.into_builder_map_body(|bytes| limit_upload(self.limiter, bytes));
		self.inner.call(req)
	}
}

pub fn limit_upload(limiter: &RateLimiter, bytes: Bytes) -> Body {
	match make_stream_from_body(bytes, limiter.clone()) {
		Ok(stream) => Body::wrap_stream(stream),
		Err(bytes) => Body::from(bytes),
	}
}

fn make_stream_from_body(
	buffer: Bytes,
	limiter: RateLimiter,
) -> Result<impl Stream<Item = Result<Bytes, Error>>, Bytes> {
	if buffer.is_empty() {
		return Err(buffer);
	}

	Ok(futures::stream::unfold(
		(buffer, limiter),
		move |(mut buffer, limiter)| async move {
			if let Some(chunk_size) = bytes_to_kilobytes(buffer.len()) {
				let chunk_size_kilobytes = chunk_size.min(BANDWIDTH_CHUNK_SIZE_KB);
				let chunk_size_bytes = ((chunk_size_kilobytes.get() as usize)
					* BYTES_PER_KILOBYTE_USIZE)
					.min(buffer.len());

				match limiter.acquire_amount(chunk_size_kilobytes).await {
					Ok(()) => {
						let chunk = buffer.split_to(chunk_size_bytes);
						Some((Ok(chunk), (buffer, limiter)))
					}
					Err(e) => Some((
						Err(Error::custom(
							ErrorKind::InvalidState,
							format!(
								"upload bandwidth limiter capacity is below the {} KiB chunk size: {}",
								chunk_size_kilobytes, e
							),
						)),
						(Bytes::new(), limiter),
					)),
				}
			} else {
				None
			}
		},
	))
}

fn bytes_to_kilobytes(bytes: usize) -> Option<NonZeroU32> {
	NonZeroU32::new(u32::try_from(bytes.div_ceil(BYTES_PER_KILOBYTE_USIZE)).unwrap_or(u32::MAX))
}

#[cfg(test)]
mod tests {
	use futures::StreamExt;

	use super::*;

	#[test]
	fn clamp_enforces_chunk_size_minimum() {
		for kbps in 1..BANDWIDTH_CHUNK_SIZE_KB.get() {
			assert_eq!(
				clamp_upload_bandwidth_kbps(NonZeroU32::new(kbps).unwrap()),
				BANDWIDTH_CHUNK_SIZE_KB
			);
		}
		assert_eq!(
			clamp_upload_bandwidth_kbps(BANDWIDTH_CHUNK_SIZE_KB),
			BANDWIDTH_CHUNK_SIZE_KB
		);
		let above_minimum = NonZeroU32::new(BANDWIDTH_CHUNK_SIZE_KB.get() + 1).unwrap();
		assert_eq!(clamp_upload_bandwidth_kbps(above_minimum), above_minimum);
	}

	#[tokio::test]
	async fn runtime_setter_keeps_chunk_acquisition_within_burst() {
		let limiter = RateLimiter::default();
		set_upload_bandwidth_limit(&limiter, NonZeroU32::new(1)).await;
		assert!(
			limiter
				.acquire_amount(BANDWIDTH_CHUNK_SIZE_KB)
				.await
				.is_ok()
		);
	}

	#[tokio::test]
	async fn undersized_limiter_yields_stream_error_not_panic() {
		let limiter = RateLimiter::new(NonZeroU32::new(1).unwrap());
		let buffer = Bytes::from(vec![
			0u8;
			BANDWIDTH_CHUNK_SIZE_KB.get() as usize
				* BYTES_PER_KILOBYTE_USIZE
		]);
		let mut stream = Box::pin(make_stream_from_body(buffer, limiter).unwrap());
		let first = stream.next().await.expect("stream should yield an item");
		assert!(first.is_err());
		assert!(stream.next().await.is_none());
	}
}
