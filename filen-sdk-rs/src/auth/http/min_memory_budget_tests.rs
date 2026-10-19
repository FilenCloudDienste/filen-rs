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
