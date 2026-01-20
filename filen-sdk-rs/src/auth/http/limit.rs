use std::{num::NonZeroU32, sync::Arc, task::Poll};

use futures::task::AtomicWaker;
use governor::{
	clock::DefaultClock,
	state::{InMemoryState, NotKeyed},
};
use tower::{Layer, Service};

use crate::runtime::{self, SpawnTaskHandle};

type RateLimiter = Arc<governor::RateLimiter<NotKeyed, InMemoryState, DefaultClock>>;

pub(crate) struct RateConfig {
	num: NonZeroU32,
	per: std::time::Duration,
}

impl RateConfig {
	pub fn new(num: NonZeroU32, per: std::time::Duration) -> Self {
		Self { num, per }
	}

	fn to_rate_limiter(&self) -> Option<RateLimiter> {
		let quota = governor::Quota::per_second(self.num);
		Some(Arc::new(governor::RateLimiter::direct(quota)))
	}
}

#[derive(Debug)]
enum RateLimiterServiceState {
	Reset,
	InnerPollSuceeded,
	AwaitingPermit {
		handle: SpawnTaskHandle<()>,
		waker: Arc<AtomicWaker>,
	},
	AllocatedPermit,
}

impl PartialEq for RateLimiterServiceState {
	fn eq(&self, other: &Self) -> bool {
		matches!(
			(self, other),
			(
				RateLimiterServiceState::Reset,
				RateLimiterServiceState::Reset
			) | (
				RateLimiterServiceState::InnerPollSuceeded,
				RateLimiterServiceState::InnerPollSuceeded,
			) | (
				RateLimiterServiceState::AllocatedPermit,
				RateLimiterServiceState::AllocatedPermit,
			) | (
				RateLimiterServiceState::AwaitingPermit { .. },
				RateLimiterServiceState::AwaitingPermit { .. },
			)
		)
	}
}

pub(crate) struct GlobalRateLimiterService<S> {
	inner: S,
	limiter: RateLimiter,
	state: RateLimiterServiceState,
}

impl<S: Clone> Clone for GlobalRateLimiterService<S> {
	fn clone(&self) -> Self {
		Self {
			inner: self.inner.clone(),
			limiter: Arc::clone(&self.limiter),
			state: RateLimiterServiceState::Reset,
		}
	}
}

#[derive(Clone)]
pub(crate) struct GlobalRateLimitLayer {
	limiter: RateLimiter,
}

impl GlobalRateLimitLayer {
	pub fn new(config: RateConfig) -> Option<Self> {
		Some(Self {
			limiter: config.to_rate_limiter()?,
		})
	}
}

impl<S> Layer<S> for GlobalRateLimitLayer {
	type Service = GlobalRateLimiterService<S>;

	fn layer(&self, service: S) -> Self::Service {
		GlobalRateLimiterService {
			inner: service,
			limiter: self.limiter.clone(),
			state: RateLimiterServiceState::Reset,
		}
	}
}

impl<S, Req> Service<Req> for GlobalRateLimiterService<S>
where
	S: Service<Req>,
{
	type Response = S::Response;
	type Error = S::Error;
	type Future = S::Future;

	fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
		if self.state == RateLimiterServiceState::AllocatedPermit {
			return Poll::Ready(Ok(()));
		}

		if self.state == RateLimiterServiceState::Reset {
			match self.inner.poll_ready(cx) {
				Poll::Ready(Ok(())) => {
					self.state = RateLimiterServiceState::InnerPollSuceeded;
				}
				other => return other,
			}
		}

		if let RateLimiterServiceState::AwaitingPermit { handle, waker } = &self.state {
			if handle.is_finished() {
				self.state = RateLimiterServiceState::AllocatedPermit;
				Poll::Ready(Ok(()))
			} else {
				waker.register(cx.waker());
				Poll::Pending
			}
		} else {
			match self.limiter.check() {
				Ok(_) => {
					self.state = RateLimiterServiceState::AllocatedPermit;
					Poll::Ready(Ok(()))
				}
				Err(_) => {
					let waker = Arc::new(AtomicWaker::new());
					waker.register(cx.waker());
					let limiter = self.limiter.clone();
					let waker_clone = waker.clone();
					self.state = RateLimiterServiceState::AwaitingPermit {
						handle: runtime::spawn_task_maybe_send(async move {
							limiter.until_ready().await;
							waker_clone.wake();
						}),
						waker: waker.clone(),
					};
					Poll::Pending
				}
			}
		}
	}

	fn call(&mut self, req: Req) -> Self::Future {
		self.state = RateLimiterServiceState::Reset;
		self.inner.call(req)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::num::NonZeroU32;
	use std::sync::Arc;
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::task::{Context, Poll};
	use std::time::{Duration, Instant};
	use tower::{Layer, Service, ServiceExt};

	// Mock service that tracks call count
	#[derive(Clone)]
	struct MockService {
		call_count: Arc<AtomicUsize>,
		ready_count: Arc<AtomicUsize>,
	}

	impl MockService {
		fn new() -> Self {
			Self {
				call_count: Arc::new(AtomicUsize::new(0)),
				ready_count: Arc::new(AtomicUsize::new(0)),
			}
		}

		fn call_count(&self) -> usize {
			self.call_count.load(Ordering::SeqCst)
		}

		fn ready_count(&self) -> usize {
			self.ready_count.load(Ordering::SeqCst)
		}
	}

	impl Service<()> for MockService {
		type Response = ();
		type Error = std::io::Error;
		type Future = std::pin::Pin<
			Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
		>;

		fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
			self.ready_count.fetch_add(1, Ordering::SeqCst);
			Poll::Ready(Ok(()))
		}

		fn call(&mut self, _req: ()) -> Self::Future {
			self.call_count.fetch_add(1, Ordering::SeqCst);
			Box::pin(async { Ok(()) })
		}
	}

	#[test]
	fn test_rate_config_creation() {
		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		assert_eq!(config.num.get(), 10);
		assert_eq!(config.per, Duration::from_secs(1));
	}

	#[test]
	fn test_rate_config_to_rate_limiter() {
		let config = RateConfig::new(NonZeroU32::new(5).unwrap(), Duration::from_secs(1));
		let limiter = config.to_rate_limiter();
		assert!(limiter.is_some());
	}

	#[test]
	fn test_global_rate_limit_layer_creation() {
		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config);
		assert!(layer.is_some());
	}

	#[test]
	fn test_layer_wraps_service() {
		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();
		let _wrapped = layer.layer(mock_service);
		// If this compiles and runs, the layer successfully wraps the service
	}

	#[tokio::test]
	async fn test_service_allows_requests_under_limit() {
		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();
		let call_counter = mock_service.call_count.clone();

		let mut service = layer.layer(mock_service);

		let start = Instant::now();

		// Make 5 requests under the limit of 10
		for _ in 0..5 {
			service.ready().await.unwrap();
			let _ = service.call(()).await;
		}

		let elapsed = start.elapsed();

		// All 5 requests should complete quickly without rate limiting delay
		assert!(
			elapsed < Duration::from_millis(100),
			"Requests under limit should not be delayed"
		);
		assert_eq!(
			call_counter.load(Ordering::SeqCst),
			5,
			"All 5 requests should have been called"
		);
	}

	#[tokio::test]
	async fn test_service_cloning() {
		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();

		let service1 = layer.layer(mock_service);
		let service2 = service1.clone();

		// Both services should share the same rate limiter
		assert!(Arc::ptr_eq(&service1.limiter, &service2.limiter));
	}

	#[tokio::test]
	async fn test_rate_limiter_enforces_limit() {
		// Very restrictive limit: 2 requests per second
		let config = RateConfig::new(NonZeroU32::new(2).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();

		let mut service = layer.layer(mock_service);

		let start = Instant::now();
		// First 2 requests should succeed immediately
		let _ = service.ready().await;
		let _ = service.call(()).await;
		let _ = service.ready().await;
		let _ = service.call(()).await;
		let first_two = start.elapsed();

		// Should be nearly instant
		assert!(first_two < Duration::from_millis(100));

		// Third request should be delayed
		let start = Instant::now();
		let _ = service.ready().await;
		let _ = service.call(()).await;
		let third = start.elapsed();

		// Should take close to 1 second
		assert!(third > Duration::from_millis(800));
	}

	#[tokio::test]
	async fn test_state_transitions() {
		let config = RateConfig::new(NonZeroU32::new(1).unwrap(), Duration::from_millis(100));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();

		let mut service = layer.layer(mock_service);

		// Initial state should be Reset
		assert_eq!(service.state, RateLimiterServiceState::Reset);

		// After successful poll_ready
		let _ = service.ready().await;
		assert_eq!(service.state, RateLimiterServiceState::AllocatedPermit);

		// After call, state should reset
		let _future = service.call(()).await;
		assert_eq!(service.state, RateLimiterServiceState::Reset);
	}

	#[tokio::test]
	async fn test_shared_limiter_across_clones() {
		// 3 requests per second
		let config = RateConfig::new(NonZeroU32::new(3).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let mock_service = MockService::new();

		let mut service1 = layer.layer(mock_service.clone());
		let mut service2 = service1.clone();

		// Use up the quota with service1
		let _ = service1.ready().await;
		let _ = service1.call(()).await;
		let _ = service1.ready().await;
		let _ = service1.call(()).await;
		let _ = service1.ready().await;

		// service2 should now be rate limited
		let start = Instant::now();
		let _ = service2.ready().await;
		let elapsed = start.elapsed();

		assert!(elapsed > Duration::from_millis(800));
	}

	#[tokio::test]
	async fn test_poll_ready_with_inner_service_not_ready() {
		use std::future::Future;
		use std::pin::Pin;

		// Mock service that's not immediately ready
		#[derive(Clone)]
		struct SlowMockService {
			ready_calls: Arc<AtomicUsize>,
		}

		impl Service<super::super::Request<bytes::Bytes, reqwest::Url>> for SlowMockService {
			type Response = reqwest::Response;
			type Error = std::io::Error;
			type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

			fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
				let calls = self.ready_calls.fetch_add(1, Ordering::SeqCst);
				if calls < 2 {
					cx.waker().wake_by_ref();
					Poll::Pending
				} else {
					Poll::Ready(Ok(()))
				}
			}

			fn call(
				&mut self,
				_req: super::super::Request<bytes::Bytes, reqwest::Url>,
			) -> Self::Future {
				Box::pin(async { Err(std::io::Error::other("mock")) })
			}
		}

		let config = RateConfig::new(NonZeroU32::new(10).unwrap(), Duration::from_secs(1));
		let layer = GlobalRateLimitLayer::new(config).unwrap();
		let slow_service = SlowMockService {
			ready_calls: Arc::new(AtomicUsize::new(0)),
		};

		let mut service = layer.layer(slow_service);
		let _ = service.ready().await;

		// Should eventually become ready after inner service is ready
		assert!(service.state == RateLimiterServiceState::AllocatedPermit);
	}

	#[test]
	fn test_rate_limiter_state_equality() {
		assert_eq!(
			RateLimiterServiceState::Reset,
			RateLimiterServiceState::Reset
		);
		assert_eq!(
			RateLimiterServiceState::InnerPollSuceeded,
			RateLimiterServiceState::InnerPollSuceeded
		);
		assert_eq!(
			RateLimiterServiceState::AllocatedPermit,
			RateLimiterServiceState::AllocatedPermit
		);

		// Different states should not be equal
		assert_ne!(
			RateLimiterServiceState::Reset,
			RateLimiterServiceState::InnerPollSuceeded
		);
	}
}
