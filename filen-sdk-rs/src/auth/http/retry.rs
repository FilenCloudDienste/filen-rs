use std::sync::Arc;

use futures::future::{self, Map};
use tower::{
	Layer, Service,
	retry::{Policy, Retry, RetryLayer},
};

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

#[derive(Clone, Debug)]
pub(crate) struct RetryPolicy {
	budget: Arc<TpsBudget>,
	/// Retries left for *this* request session; decremented on each retry, reset per request by
	/// tower's per-request policy clone. Not shared (unlike `budget`), so it bounds a single call.
	retries_remaining: u32,
}

impl RetryPolicy {
	pub(crate) fn new(budget: TpsBudget) -> Self {
		Self {
			budget: Arc::new(budget),
			retries_remaining: MAX_RETRIES,
		}
	}
}

#[derive(Clone, Debug)]
pub(crate) enum RetryError<E> {
	Retry(E),
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
			RetryError::Retry(e) => e,
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
	type Future = future::Ready<()>;

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
					Some(future::ready(()))
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
mod tests {
	use std::{
		future::Future,
		pin::Pin,
		sync::{
			Arc,
			atomic::{AtomicUsize, Ordering},
		},
		task::{Context, Poll},
	};

	use tokio::time::Duration;
	use tower::{Layer, Service, ServiceExt, retry::Policy};

	use super::{MAX_RETRIES, RetryError, RetryMapLayer, RetryPolicy};
	use crate::auth::http::tower_wasm_time::tps_budget::TpsBudget;

	/// A single request that always fails with a retryable error must give up after `MAX_RETRIES`
	/// retries — even though the budget (with its constant `reserve` floor) would otherwise keep
	/// permitting them. This is the guard against the infinite-retry hang.
	#[test]
	fn caps_retries_per_request() {
		let mut policy = RetryPolicy::new(TpsBudget::default());
		let mut granted = 0u32;
		// Ask far more times than the cap; the default budget's reserve easily covers MAX_RETRIES
		// withdrawals, so the cap (not the budget) is what stops us.
		for _ in 0..(MAX_RETRIES + 50) {
			let mut result: Result<(), RetryError<()>> = Err(RetryError::Retry(()));
			match policy.retry(&mut (), &mut result) {
				Some(_) => granted += 1,
				None => break,
			}
		}
		assert_eq!(granted, MAX_RETRIES);
	}

	/// A successful result never asks to retry (and tops up the budget).
	#[test]
	fn success_does_not_retry() {
		let mut policy = RetryPolicy::new(TpsBudget::default());
		let mut result: Result<(), RetryError<()>> = Ok(());
		assert!(policy.retry(&mut (), &mut result).is_none());
	}

	/// A `NoRetry`-tagged error is never retried, regardless of remaining budget/attempts.
	#[test]
	fn no_retry_error_is_not_retried() {
		let mut policy = RetryPolicy::new(TpsBudget::default());
		let mut result: Result<(), RetryError<()>> = Err(RetryError::NoRetry(()));
		assert!(policy.retry(&mut (), &mut result).is_none());
	}

	// ---- Reproduction of the nightly hang (deterministic, via tokio's paused clock) ----
	//
	// The bug never reproduced locally because it is round-trip-time dependent: the OLD code's
	// ONLY stop-gate for a retryable error was `budget.withdraw()` (no per-request cap). The two
	// tests below drive the REAL, unchanged `TpsBudget` with simulated RTT (tokio time is paused;
	// `advance` moves the same `tokio::time::Instant` the budget reads) to show that gate's
	// behavior, and the third drives the REAL production retry stack to show the staged cap fixes
	// it. `TpsBudget::default()` = `new(10s, min_per_sec=10, retry_percent=0.2)`.

	/// THE HANG (old code): at a high RTT the budget NEVER refuses, so the pre-fix retry loop —
	/// gated solely on `budget.withdraw()` — would retry a permanent egest 404 forever. At
	/// ~200ms/attempt (5 req/s, below `min_per_sec`) the `reserve` floor is never breached; 500
	/// consecutive successes stand in for infinity.
	#[tokio::test(start_paused = true)]
	async fn budget_alone_never_refuses_at_high_rtt() {
		let budget = TpsBudget::default();
		for attempt in 0..500 {
			assert!(
				budget.withdraw(),
				"budget refused at attempt {attempt}; the pre-fix code would have stopped here, \
				 but at this RTT it never does — hence the infinite retry"
			);
			tokio::time::advance(Duration::from_millis(200)).await;
		}
	}

	/// WHY IT PASSED LOCALLY: at a fast RTT the budget DOES refuse within ~100 retries, so pre-fix
	/// the call returned the 404 in ~2s (matching the observed 2.40s local pass). At ~10ms/attempt
	/// (100 req/s, well above `min_per_sec`) withdrawals outrun the floor.
	#[tokio::test(start_paused = true)]
	async fn budget_alone_refuses_quickly_at_low_rtt() {
		let budget = TpsBudget::default();
		let mut granted = 0u32;
		for _ in 0..5000 {
			if !budget.withdraw() {
				break;
			}
			granted += 1;
			tokio::time::advance(Duration::from_millis(10)).await;
		}
		assert!(
			(90..=150).contains(&granted),
			"expected the budget to refuse after ~100 retries at fast RTT, got {granted}"
		);
	}

	/// A `tower::Service` that fails with a retryable error on every call after a simulated RTT,
	/// counting calls. The safety stop lets the OLD (uncapped) code's infinite loop TERMINATE the
	/// test (reaching it proves it retried far past any sane bound) instead of hanging forever.
	#[derive(Clone)]
	struct ForeverRetry {
		calls: Arc<AtomicUsize>,
		rtt: Duration,
		safety_stop: usize,
	}

	impl Service<()> for ForeverRetry {
		type Response = ();
		type Error = RetryError<&'static str>;
		type Future = Pin<Box<dyn Future<Output = Result<(), RetryError<&'static str>>> + Send>>;

		fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
			Poll::Ready(Ok(()))
		}

		fn call(&mut self, _req: ()) -> Self::Future {
			let calls = self.calls.clone();
			let rtt = self.rtt;
			let safety_stop = self.safety_stop;
			Box::pin(async move {
				let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
				// Paused-clock auto-advance fires this timer with no real delay, so the budget
				// sees `rtt` elapse per attempt — i.e. a high-RTT egest.
				tokio::time::sleep(rtt).await;
				if n >= safety_stop {
					Ok(())
				} else {
					Err(RetryError::Retry("egest 404"))
				}
			})
		}
	}

	/// THE FIX (staged code): the real production retry stack (`RetryMapLayer` → `RetryMapService`,
	/// the same wrapper `get_raw_bytes` builds) GIVES UP after `MAX_RETRIES` retries on a
	/// forever-retryable error at high RTT, instead of looping until the 1000-call safety stop.
	/// Run against the pre-fix code (`git stash`/worktree at HEAD) this same test instead reaches
	/// `calls == 1000`, demonstrating the unbounded retry.
	#[tokio::test(start_paused = true)]
	async fn real_retry_stack_gives_up_after_max_retries_at_high_rtt() {
		let calls = Arc::new(AtomicUsize::new(0));
		let service = ForeverRetry {
			calls: calls.clone(),
			rtt: Duration::from_millis(200), // 5 req/s — below min_per_sec, so the budget never refuses
			safety_stop: 1000,
		};
		let stack = RetryMapLayer::new(RetryPolicy::new(TpsBudget::default())).layer(service);

		// Must RESOLVE (not hang). Pre-fix this only terminated via the safety stop.
		let result: Result<(), &str> = stack.oneshot(()).await;

		assert_eq!(
			calls.load(Ordering::SeqCst),
			(MAX_RETRIES + 1) as usize,
			"expected 1 initial attempt + MAX_RETRIES retries, then give up"
		);
		assert_eq!(
			result,
			Err("egest 404"),
			"the real last error must propagate to the caller"
		);
	}
}
