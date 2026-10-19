use super::{ClientConfig, SharedClientState};

/// `GlobalConcurrencyLimitLayer::new` is a `tokio::sync::Semaphore`, so 0 permits parks every
/// request forever and anything above `MAX_PERMITS` panics inside the constructor — reachable
/// from `JsClientConfig`'s `u32` on wasm32, where the ceiling is `u32::MAX >> 3`.
#[tokio::test]
async fn concurrency_is_clamped_at_both_ends() {
	assert_eq!(
		SharedClientState::new(ClientConfig::default().with_concurrency(0))
			.expect("a zero concurrency is clamped, not rejected")
			.max_concurrency(),
		1,
		"concurrency 0 must be floored to 1, not left as a park-forever limiter"
	);
	assert_eq!(
		SharedClientState::new(ClientConfig::default().with_concurrency(usize::MAX))
			.expect("an oversized concurrency is clamped, not rejected")
			.max_concurrency(),
		tokio::sync::Semaphore::MAX_PERMITS,
		"an oversized concurrency must be clamped to Semaphore::MAX_PERMITS, not panic"
	);
}
