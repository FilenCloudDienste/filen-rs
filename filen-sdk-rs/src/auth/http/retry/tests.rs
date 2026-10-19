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

use super::{MAX_CONNECT_RETRIES, MAX_RETRIES, RetryError, RetryMapLayer, RetryPolicy};
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

/// The pre-send connect class pays an escalating pause and is capped far below `MAX_RETRIES`,
/// because each of its retries sleeps: retryability alone would buy ten instant re-lookups —
/// tens of milliseconds — against a resolver that is out for longer than that, while a device
/// that is genuinely offline must not spend ten backoffs before the error surfaces.
#[tokio::test(start_paused = true)]
async fn connect_backoff_escalates_and_is_capped() {
	let mut policy = RetryPolicy::new(TpsBudget::default());
	let start = tokio::time::Instant::now();
	let mut granted = 0u32;
	for _ in 0..(MAX_CONNECT_RETRIES + 5) {
		let mut result: Result<(), RetryError<()>> = Err(RetryError::RetryAfterBackoff(()));
		match policy.retry(&mut (), &mut result) {
			Some(delay) => {
				delay.await;
				granted += 1;
			}
			None => break,
		}
	}
	assert_eq!(granted, MAX_CONNECT_RETRIES);
	assert_eq!(
		start.elapsed(),
		Duration::from_millis(1400),
		"200ms + 400ms + 800ms: the class rides out ~1.4s of outage and no more"
	);
}

/// The instant classes keep their zero delay: a stale-pool retry must re-issue immediately, so
/// the `Left` branch resolves without ever touching a timer.
#[tokio::test(start_paused = true)]
async fn immediate_retry_does_not_sleep() {
	let mut policy = RetryPolicy::new(TpsBudget::default());
	let start = tokio::time::Instant::now();
	let mut result: Result<(), RetryError<()>> = Err(RetryError::Retry(()));
	policy
		.retry(&mut (), &mut result)
		.expect("a retryable error must be retried")
		.await;
	assert_eq!(start.elapsed(), Duration::ZERO);
}

/// The two caps are independent, so a request that burns the slow class still has its instant
/// attempts left (a DNS blip followed by a stale pooled connection is one request, two causes).
#[tokio::test(start_paused = true)]
async fn connect_cap_does_not_consume_the_immediate_cap() {
	let mut policy = RetryPolicy::new(TpsBudget::default());
	for _ in 0..MAX_CONNECT_RETRIES {
		let mut result: Result<(), RetryError<()>> = Err(RetryError::RetryAfterBackoff(()));
		policy
			.retry(&mut (), &mut result)
			.expect("within the connect cap")
			.await;
	}

	let mut exhausted: Result<(), RetryError<()>> = Err(RetryError::RetryAfterBackoff(()));
	assert!(
		policy.retry(&mut (), &mut exhausted).is_none(),
		"the connect cap is spent"
	);

	let mut immediate: Result<(), RetryError<()>> = Err(RetryError::Retry(()));
	assert!(
		policy.retry(&mut (), &mut immediate).is_some(),
		"but the instant cap is untouched"
	);
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

/// Fails with the pre-send connect class until its `succeed_after`th call. Unlike
/// [`ForeverRetry`] it takes no RTT: a resolver that is down answers instantly, which is the
/// whole reason this class needs a pause rather than an immediate re-attempt.
#[derive(Clone)]
struct ConnectFailsUntil {
	calls: Arc<AtomicUsize>,
	succeed_after: usize,
}

impl Service<()> for ConnectFailsUntil {
	type Response = ();
	type Error = RetryError<&'static str>;
	type Future = Pin<Box<dyn Future<Output = Result<(), RetryError<&'static str>>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Poll::Ready(Ok(()))
	}

	fn call(&mut self, _req: ()) -> Self::Future {
		let calls = self.calls.clone();
		let succeed_after = self.succeed_after;
		Box::pin(async move {
			let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
			if n >= succeed_after {
				Ok(())
			} else {
				Err(RetryError::RetryAfterBackoff("dns error"))
			}
		})
	}
}

/// THE FIX, end to end: a resolver that comes back during the backoff is absorbed by the real
/// production stack and the caller never sees an error. This is the nightly case — three tests
/// panicked on an `EAI_NONAME` that had cleared long before this budget would run out — and it
/// is what classification alone does NOT buy: without the pause the retries all land inside the
/// same outage, milliseconds apart.
#[tokio::test(start_paused = true)]
async fn real_retry_stack_absorbs_a_connect_failure_that_clears() {
	let calls = Arc::new(AtomicUsize::new(0));
	let service = ConnectFailsUntil {
		calls: calls.clone(),
		succeed_after: 3,
	};
	let stack = RetryMapLayer::new(RetryPolicy::new(TpsBudget::default())).layer(service);
	let start = tokio::time::Instant::now();

	let result: Result<(), &str> = stack.oneshot(()).await;

	assert_eq!(result, Ok(()), "the caller must never see the blip");
	assert_eq!(calls.load(Ordering::SeqCst), 3);
	assert_eq!(
		start.elapsed(),
		Duration::from_millis(600),
		"two retries, paused 200ms then 400ms"
	);
}

/// The other half of the bargain: an outage that does NOT clear still surfaces, capped, so a
/// device that is simply offline pays a bounded latency instead of retrying into the void.
#[tokio::test(start_paused = true)]
async fn real_retry_stack_gives_up_on_a_persistent_connect_failure() {
	let calls = Arc::new(AtomicUsize::new(0));
	let service = ConnectFailsUntil {
		calls: calls.clone(),
		succeed_after: usize::MAX,
	};
	let stack = RetryMapLayer::new(RetryPolicy::new(TpsBudget::default())).layer(service);
	let start = tokio::time::Instant::now();

	let result: Result<(), &str> = stack.oneshot(()).await;

	assert_eq!(result, Err("dns error"), "the real error still propagates");
	assert_eq!(
		calls.load(Ordering::SeqCst),
		(MAX_CONNECT_RETRIES + 1) as usize,
		"1 initial attempt + MAX_CONNECT_RETRIES"
	);
	assert_eq!(
		start.elapsed(),
		Duration::from_millis(1400),
		"the whole latency an offline device pays before the error reaches it"
	);
}
