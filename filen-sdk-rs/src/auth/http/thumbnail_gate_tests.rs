use super::{ClientConfig, ThumbnailConfig};

/// Both ends of the gate's permit count are hazards: 0 parks every decode forever, and
/// anything above `Semaphore::MAX_PERMITS` panics inside `Semaphore::new` — reachable from a
/// `u32` config field on wasm32, where `usize` is 32-bit and the ceiling is `u32::MAX >> 3`.
#[test]
fn decode_concurrency_is_clamped_at_both_ends() {
	assert_eq!(
		ThumbnailConfig::new(&ClientConfig::default().with_thumbnail_decode_concurrency(0))
			.gate
			.available_permits(),
		1,
		"concurrency 0 must be floored to 1, not left as a park-forever gate"
	);
	assert_eq!(
		ThumbnailConfig::new(
			&ClientConfig::default().with_thumbnail_decode_concurrency(usize::MAX)
		)
		.gate
		.available_permits(),
		tokio::sync::Semaphore::MAX_PERMITS,
		"an oversized concurrency must be clamped to Semaphore::MAX_PERMITS, not panic"
	);
}
