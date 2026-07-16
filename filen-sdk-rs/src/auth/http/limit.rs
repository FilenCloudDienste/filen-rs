use std::{num::NonZeroU32, sync::Arc, task::Poll};

use futures::{FutureExt, future::BoxFuture};
use tokio::sync::RwLock;
use tower::{Layer, Service};

#[derive(Clone)]
pub struct RateLimiter(Arc<RwLock<Option<Arc<governor::DefaultDirectRateLimiter>>>>);

impl Default for RateLimiter {
	fn default() -> Self {
		Self(Arc::new(RwLock::new(None)))
	}
}

impl RateLimiter {
	pub(crate) fn new(per_second: NonZeroU32) -> Self {
		let quota = governor::Quota::per_second(per_second);
		let limiter = governor::DefaultDirectRateLimiter::direct(quota);
		let _ = limiter.check_n(per_second); // make sure we start empty and don't go over the limit in the first second
		Self(Arc::new(RwLock::new(Some(Arc::new(limiter)))))
	}

	pub(crate) async fn acquire(&self) {
		// Clone the Arc'd limiter and drop the read guard before awaiting, so a concurrent set_*
		// (which queues a writer on the fair RwLock) is not blocked behind in-flight acquires that
		// are still waiting out the old rate.
		let limiter = self.0.read().await.clone();
		if let Some(rate_limiter) = limiter {
			rate_limiter.until_ready().await;
		}
	}

	#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
	pub(crate) async fn acquire_amount(
		&self,
		amount: NonZeroU32,
	) -> Result<(), governor::InsufficientCapacity> {
		let limiter = self.0.read().await.clone();
		if let Some(rate_limiter) = limiter {
			rate_limiter.until_n_ready(amount).await
		} else {
			Ok(())
		}
	}

	pub(crate) async fn change_rate_per_sec(&self, per_second: Option<NonZeroU32>) {
		let mut lock = self.0.write().await;
		if let Some(per_second) = per_second {
			let quota = governor::Quota::per_second(per_second);
			*lock = Some(Arc::new(governor::DefaultDirectRateLimiter::direct(quota)));
		} else {
			*lock = None;
		}
	}
}

enum RateLimiterServiceState {
	Reset,
	InnerPollSuceeded,
	AwaitingPermit(BoxFuture<'static, ()>),
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
			limiter: self.limiter.clone(),
			state: RateLimiterServiceState::Reset,
		}
	}
}

#[derive(Clone)]
pub(crate) struct GlobalRateLimitLayer {
	pub(crate) limiter: RateLimiter,
}

impl GlobalRateLimitLayer {
	pub fn new(per_second: NonZeroU32) -> Self {
		Self {
			limiter: RateLimiter::new(per_second),
		}
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

		if let RateLimiterServiceState::AwaitingPermit(fut) = &mut self.state {
			fut.poll_unpin(cx).map(|()| Ok(()))
		} else {
			let limiter = self.limiter.clone();
			let mut fut = Box::pin(async move {
				limiter.acquire().await;
			});
			match fut.poll_unpin(cx) {
				Poll::Ready(()) => {
					self.state = RateLimiterServiceState::AllocatedPermit;
					Poll::Ready(Ok(()))
				}
				Poll::Pending => {
					self.state = RateLimiterServiceState::AwaitingPermit(fut);
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
	use std::{
		num::NonZeroU32,
		time::{Duration, Instant},
	};

	use super::RateLimiter;

	/// Changing the rate must not be blocked by an in-flight `acquire` that is waiting out the old
	/// rate: the read guard has to be dropped before awaiting the limiter, or a `set_*` writer
	/// queued on the fair RwLock freezes every new request behind the existing waiters.
	#[tokio::test]
	async fn set_rate_is_not_blocked_by_in_flight_acquire() {
		// 1 req/s, with the initial burst drained by `new`, so the next acquire must wait ~1s.
		let limiter = RateLimiter::new(NonZeroU32::new(1).unwrap());
		let waiting = limiter.clone();
		let acquire = tokio::spawn(async move { waiting.acquire().await });
		// Let the spawned acquire take the read guard and start awaiting the limiter.
		tokio::time::sleep(Duration::from_millis(50)).await;

		let start = Instant::now();
		limiter
			.change_rate_per_sec(Some(NonZeroU32::new(1000).unwrap()))
			.await;
		let elapsed = start.elapsed();
		acquire.abort();

		assert!(
			elapsed < Duration::from_millis(500),
			"changing the rate blocked behind an in-flight acquire ({elapsed:?}); the read guard \
			 is held across the limiter await"
		);
	}
}
