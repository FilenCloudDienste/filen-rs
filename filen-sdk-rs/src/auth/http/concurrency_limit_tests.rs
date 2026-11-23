use super::{ClientConfig, SharedClientState};

/// The limits are `tokio::sync::Semaphore`s, so 0 permits parks every request forever and
/// anything above `MAX_PERMITS` panics inside the constructor — reachable from
/// `JsClientConfig`'s `u32` on wasm32, where the ceiling is `u32::MAX >> 3`. The request pool is
/// the transfers' limit plus the control headroom, so the ceiling sits that far below.
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
		tokio::sync::Semaphore::MAX_PERMITS - 4,
		"an oversized concurrency must be clamped below Semaphore::MAX_PERMITS by the control \
		 headroom, not panic"
	);
}

#[tokio::test]
async fn set_concurrency_is_read_back_by_max_concurrency_with_the_same_clamp() {
	let state = SharedClientState::new(ClientConfig::default().with_concurrency(16))
		.expect("a valid config");
	state.set_concurrency(4);
	assert_eq!(state.max_concurrency(), 4);
	state.set_concurrency(32);
	assert_eq!(state.max_concurrency(), 32);
	state.set_concurrency(0);
	assert_eq!(state.max_concurrency(), 1, "0 must be floored to 1");
	state.set_concurrency(usize::MAX);
	assert_eq!(
		state.max_concurrency(),
		tokio::sync::Semaphore::MAX_PERMITS - 4
	);
}

#[tokio::test]
async fn set_concurrency_reaches_every_clone_of_the_state() {
	let state = SharedClientState::new(ClientConfig::default().with_concurrency(16))
		.expect("a valid config");
	let clone = state.clone();
	state.set_concurrency(3);
	assert_eq!(clone.max_concurrency(), 3);
}
