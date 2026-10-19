use super::{ClientConfig, FULL_CHUNK_BYTES, SharedClientState};

/// `SharedClientState::new` rejects a memory budget whose half cannot hold one chunk, because
/// the HTTP provider caps a stream's read-ahead window at half the budget.
#[tokio::test]
async fn rejects_budget_whose_half_cannot_hold_one_chunk() {
	let one_chunk = FULL_CHUNK_BYTES;

	// budget == 1 chunk -> half is half a chunk -> rejected.
	assert!(
		SharedClientState::new(ClientConfig::default().with_memory_budget(one_chunk)).is_err(),
		"a 1-chunk budget (half < one chunk) must be rejected at construction"
	);

	// budget == 2 chunks -> half is exactly one chunk -> accepted (the iOS default boundary).
	SharedClientState::new(ClientConfig::default().with_memory_budget(2 * one_chunk))
		.expect("a 2-chunk budget (half == one chunk) must be valid");

	// the platform defaults must remain valid.
	SharedClientState::new(ClientConfig::default()).expect("the default budget must be valid");
}
