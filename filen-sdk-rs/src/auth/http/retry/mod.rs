use std::{sync::Arc, time::Duration};

use futures::future::{self, Either, Map};
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use tokio::time::{Sleep, sleep};
use tower::{
	Layer, Service,
	retry::{Policy, Retry, RetryLayer},
};
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::{Sleep, sleep};

use super::tower_wasm_time::tps_budget::TpsBudget;

/// Hard per-request ceiling on retries, independent of the [`TpsBudget`].
///
/// The budget only throttles the aggregate retry *rate*: its `sum()` always includes a constant
/// `reserve` floor, so a single request that keeps failing with a retryable error never drives the
/// budget to zero once its retry rate falls to ~`min_per_sec` — at a high enough round-trip time it
/// would retry **forever**. (That is exactly how the nightly egest-download test hung on a
/// permanently-missing chunk.) This cap bounds any one request to a finite number of attempts
/// regardless of the budget. Each request session gets a fresh count because tower clones the
/// `Policy` per request (the shared budget lives behind an `Arc`, so only the rate state is shared).
const MAX_RETRIES: u32 = 10;

/// Per-request ceiling on retries of a *pre-send connect* failure — the class that pauses before
/// re-attempting (see [`RetryError::RetryAfterBackoff`]). It is separate from [`MAX_RETRIES`], and
/// much smaller, because these retries cost wall-clock time: a device that is genuinely offline
/// answers every lookup instantly, so this cap times [`CONNECT_BACKOFF_BASE`] is the latency such a
/// device pays before the error reaches the caller.
const MAX_CONNECT_RETRIES: u32 = 3;

/// Pause before the first re-attempt of a pre-send connect failure, doubled per attempt: the
/// schedule is 200ms, 400ms, 800ms, so the class rides out a resolver outage of ~1.4s and no more.
/// A longer outage still surfaces as an error, exactly as it does today; widen this only with
/// evidence that real outages sit above it, since every millisecond added here is latency an
/// offline device pays on every single request.
const CONNECT_BACKOFF_BASE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
	budget: Arc<TpsBudget>,
	/// Retries left for *this* request session; decremented on each retry, reset per request by
	/// tower's per-request policy clone. Not shared (unlike `budget`), so it bounds a single call.
	retries_remaining: u32,
	/// Retries left for the pre-send connect class specifically, counted separately because each
	/// one sleeps. Cloned per request like `retries_remaining`, so it too bounds a single call.
	connect_retries_remaining: u32,
}

impl RetryPolicy {
	pub(crate) fn new(budget: TpsBudget) -> Self {
		Self {
			budget: Arc::new(budget),
			retries_remaining: MAX_RETRIES,
			connect_retries_remaining: MAX_CONNECT_RETRIES,
		}
	}
}

#[derive(Clone, Debug)]
pub(crate) enum RetryError<E> {
	Retry(E),
	/// Retryable, but only after a pause. The transport failed before the request was sent and the
	/// cause is the local network stack rather than the peer — a resolver that briefly went away,
	/// a refused connect — so an immediate re-attempt would just re-ask the same broken resolver
	/// and burn the per-request cap in a few milliseconds. Paced by [`MAX_CONNECT_RETRIES`] and
	/// [`CONNECT_BACKOFF_BASE`] instead of the instant [`Self::Retry`] path.
	RetryAfterBackoff(E),
	NoRetry(E),
}

impl<E> RetryError<E> {
	/// Tag `error` as retryable-or-not from a `bool` — the natural inverse of
	/// [`into_inner`](Self::into_inner), so call sites read "classify, then tag" instead of
	/// open-coding the `if retryable { Retry } else { NoRetry }` branch.
	pub(crate) fn from_retryable(retryable: bool, error: E) -> Self {
		if retryable {
			RetryError::Retry(error)
		} else {
			RetryError::NoRetry(error)
		}
	}

	pub(crate) fn into_inner(self) -> E {
		match self {
			RetryError::Retry(e) | RetryError::RetryAfterBackoff(e) => e,
			RetryError::NoRetry(e) => e,
		}
	}
}

#[derive(Clone)]
pub(crate) struct RetryMapLayer {
	inner: RetryLayer<RetryPolicy>,
}

impl RetryMapLayer {
	pub(crate) fn new(policy: RetryPolicy) -> Self {
		Self {
			inner: RetryLayer::new(policy),
		}
	}
}

impl<S> Layer<S> for RetryMapLayer {
	type Service = RetryMapService<S>;

	fn layer(&self, service: S) -> Self::Service {
		RetryMapService {
			inner: self.inner.layer(service),
		}
	}
}

#[derive(Clone)]
pub(crate) struct RetryMapService<S> {
	inner: Retry<RetryPolicy, S>,
}

impl<S, Req, Res, E> Service<Req> for RetryMapService<S>
where
	S: Service<Req, Response = Res, Error = RetryError<E>> + Clone,
	Req: Clone,
	E: std::fmt::Debug,
{
	type Response = Res;
	type Error = E;
	type Future = Map<
		<Retry<RetryPolicy, S> as Service<Req>>::Future,
		fn(Result<Res, RetryError<E>>) -> Result<Res, E>,
	>;

	fn poll_ready(
		&mut self,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Result<(), Self::Error>> {
		self.inner.poll_ready(cx).map_err(|e| e.into_inner())
	}

	fn call(&mut self, req: Req) -> Self::Future {
		futures::FutureExt::map(self.inner.call(req), |res| res.map_err(|e| e.into_inner()))
	}
}

impl<Req, Res, E> Policy<Req, Res, RetryError<E>> for RetryPolicy
where
	E: std::fmt::Debug,
	Req: Clone,
{
	/// `Left` retries immediately, `Right` after a pause. Naming both branches keeps the delay off
	/// the hot path: an instant retry still resolves without touching a timer.
	type Future = Either<future::Ready<()>, Sleep>;

	fn retry(
		&mut self,
		_req: &mut Req,
		result: &mut Result<Res, RetryError<E>>,
	) -> Option<Self::Future> {
		match result {
			Ok(_) => {
				self.budget.deposit();
				None
			}
			Err(RetryError::Retry(_)) => {
				// Per-request cap first (short-circuits, so an exhausted request does not also
				// spend a shared budget token), then the rate budget. Either failing gives up and
				// lets the real last error propagate to the caller.
				if self.retries_remaining > 0 && self.budget.withdraw() {
					self.retries_remaining -= 1;
					Some(Either::Left(future::ready(())))
				} else {
					None
				}
			}
			Err(RetryError::RetryAfterBackoff(_)) => {
				// Same two gates as above, against this class's own much smaller cap: a retry that
				// sleeps must not be handed out `MAX_RETRIES` times.
				if self.connect_retries_remaining > 0 && self.budget.withdraw() {
					let attempt = MAX_CONNECT_RETRIES - self.connect_retries_remaining;
					self.connect_retries_remaining -= 1;
					Some(Either::Right(sleep(CONNECT_BACKOFF_BASE * (1 << attempt))))
				} else {
					None
				}
			}
			Err(RetryError::NoRetry(_)) => None,
		}
	}

	fn clone_request(&mut self, req: &Req) -> Option<Req> {
		Some(req.clone())
	}
}

#[cfg(test)]
mod tests;
