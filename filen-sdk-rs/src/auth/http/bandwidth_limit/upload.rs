use std::{num::NonZeroU32, sync::Arc};

use bytes::Bytes;
use futures::Stream;
use governor::{Quota, RateLimiter};
use reqwest::Body;
use tower::{Layer, Service};

use crate::{Error, ErrorKind};

use super::{super::Request, BYTES_PER_KILOBYTE_USIZE, BandwidthLimiter};

const BANDWIDTH_CHUNK_SIZE_KB: NonZeroU32 = NonZeroU32::new(16).unwrap();
const BANDWIDTH_CHUNK_USIZE_KB: usize = BANDWIDTH_CHUNK_SIZE_KB.get() as usize;

pub(crate) fn new_upload_bandwidth_limiter(kbps: NonZeroU32) -> Result<BandwidthLimiter, Error> {
	if kbps < BANDWIDTH_CHUNK_SIZE_KB {
		return Err(Error::custom(
			ErrorKind::InvalidState,
			format!(
				"upload bandwidth limit must be at least {} kilobytes per second",
				BANDWIDTH_CHUNK_SIZE_KB
			),
		));
	}

	Ok(RateLimiter::direct(Quota::per_second(kbps)))
}

#[derive(Clone)]
pub(crate) struct UploadBandwidthLimiterLayer<'a> {
	limiter: Option<&'a Arc<BandwidthLimiter>>,
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
	pub(crate) fn new(limiter: Option<&'a Arc<BandwidthLimiter>>) -> Self {
		Self { limiter }
	}
}

#[derive(Clone)]
pub(crate) struct UploadBandwidthLimiterService<'a, S> {
	inner: S,
	limiter: Option<&'a Arc<BandwidthLimiter>>,
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
		let req = req.into_builder_map_body(|bytes| match self.limiter {
			Some(limiter) => limit_upload(limiter, bytes),
			None => bytes.into(),
		});
		self.inner.call(req)
	}
}

pub fn limit_upload(limiter: &Arc<BandwidthLimiter>, bytes: Bytes) -> Body {
	match make_stream_from_body(bytes, limiter.clone()) {
		Ok(stream) => Body::wrap_stream(stream),
		Err(bytes) => Body::from(bytes),
	}
}

fn make_stream_from_body(
	buffer: Bytes,
	limiter: Arc<BandwidthLimiter>,
) -> Result<impl Stream<Item = Result<Bytes, std::convert::Infallible>>, Bytes> {
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

				limiter
					.until_n_ready(chunk_size_kilobytes)
					.await
					.expect("BANDWIDTH_CHUNK_SIZE should be < BandwidthManager limit");

				let chunk = buffer.split_to(chunk_size_bytes);
				Some((Ok(chunk), (buffer, limiter)))
			} else {
				None
			}
		},
	))
}

fn bytes_to_kilobytes(bytes: usize) -> Option<NonZeroU32> {
	NonZeroU32::new(u32::try_from(bytes.div_ceil(BYTES_PER_KILOBYTE_USIZE)).unwrap_or(u32::MAX))
}
