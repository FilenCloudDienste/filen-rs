use super::{ClientConfig, FULL_CHUNK_BYTES, SharedClientState};

/// A budget below one full encrypted chunk can never satisfy `Chunk::acquire`'s `acquire_many`
/// (it would await more permits than the semaphore will ever hold), hanging every transfer.
/// `SharedClientState::new` must reject it in every build — not only when the `http-provider`
/// feature (with its stricter half-budget rule) is compiled in.
#[tokio::test]
async fn rejects_budget_below_one_chunk() {
	let one_chunk = FULL_CHUNK_BYTES;

	// Half a chunk: acquire_many(chunk_size) would await more permits than exist -> hang.
	assert!(
		SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk / 2)).is_err(),
		"a sub-chunk budget must be rejected at construction"
	);
	// One byte below a chunk is still rejected.
	assert!(
		SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk - 1)).is_err(),
		"a budget one byte below a chunk must be rejected"
	);
	// Exactly one chunk is the minimum that can satisfy a single acquire. With `http-provider`
	// compiled in the stricter half-budget rule rejects it, so only assert acceptance here
	// when that feature is absent.
	#[cfg(not(feature = "http-provider"))]
	SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk))
		.expect("a one-chunk budget must be valid without http-provider");
}

/// The smallest budget construction accepts in this build.
fn smallest_budget() -> usize {
	if cfg!(feature = "http-provider") {
		2 * FULL_CHUNK_BYTES
	} else {
		FULL_CHUNK_BYTES
	}
}

#[tokio::test]
async fn a_set_budget_is_read_back_and_sizes_the_semaphore() {
	let state =
		SharedClientState::new(ClientConfig::default().with_memory_budget(4 * FULL_CHUNK_BYTES))
			.expect("a valid budget");
	state
		.set_file_io_memory_budget(8 * FULL_CHUNK_BYTES)
		.expect("a larger budget is valid");
	assert_eq!(state.memory_budget(), 8 * FULL_CHUNK_BYTES);
	assert_eq!(
		state.memory_semaphore().available_permits(),
		8 * FULL_CHUNK_BYTES
	);
	state
		.set_file_io_memory_budget(smallest_budget())
		.expect("the smallest budget construction accepts is valid");
	assert_eq!(state.memory_budget(), smallest_budget());
	assert_eq!(
		state.memory_semaphore().available_permits(),
		smallest_budget()
	);
}

#[tokio::test]
async fn a_set_budget_construction_would_reject_is_rejected_and_changes_nothing() {
	let state =
		SharedClientState::new(ClientConfig::default().with_memory_budget(4 * FULL_CHUNK_BYTES))
			.expect("a valid budget");
	for budget in [0, FULL_CHUNK_BYTES - 1, smallest_budget() - 1] {
		let error = state
			.set_file_io_memory_budget(budget)
			.expect_err("a budget below the minimum is rejected");
		assert_eq!(error.kind(), crate::ErrorKind::InvalidState);
		assert_eq!(state.memory_budget(), 4 * FULL_CHUNK_BYTES);
		assert_eq!(
			state.memory_semaphore().available_permits(),
			4 * FULL_CHUNK_BYTES
		);
	}
}

#[tokio::test]
async fn a_set_budget_above_the_semaphores_ceiling_counts_as_the_ceiling() {
	let state = SharedClientState::new(ClientConfig::default()).expect("valid defaults");
	state
		.set_file_io_memory_budget(usize::MAX)
		.expect("an oversized budget is clamped, not rejected");
	assert_eq!(state.memory_budget(), tokio::sync::Semaphore::MAX_PERMITS);
}

#[tokio::test]
async fn a_budget_set_below_what_is_held_hands_out_nothing_until_under_it() {
	let state =
		SharedClientState::new(ClientConfig::default().with_memory_budget(4 * FULL_CHUNK_BYTES))
			.expect("a valid budget");
	let chunk = u32::try_from(FULL_CHUNK_BYTES).expect("a chunk fits a u32");
	let semaphore = state.memory_semaphore();
	let mut held = (0..3)
		.map(|_| semaphore.try_acquire_many(chunk).expect("free"))
		.collect::<Vec<_>>();
	state
		.set_file_io_memory_budget(smallest_budget())
		.expect("a valid budget");
	assert_eq!(state.memory_budget(), smallest_budget());
	assert_eq!(semaphore.available_permits(), 0);

	let mut waiter = Box::pin(semaphore.acquire_many(chunk));
	assert!(futures::poll!(waiter.as_mut()).is_pending());
	// given back down to exactly the new budget: still nothing to hand out
	held.truncate(smallest_budget() / FULL_CHUNK_BYTES);
	for _ in 0..8 {
		tokio::task::yield_now().await;
	}
	assert!(futures::poll!(waiter.as_mut()).is_pending());
	assert_eq!(semaphore.available_permits(), 0);

	held.pop();
	let permit = waiter.await.expect("never closed");
	assert_eq!(permit.num_permits(), FULL_CHUNK_BYTES);
	drop(permit);
	drop(held);
	assert_eq!(semaphore.available_permits(), smallest_budget());
}
