use std::collections::HashMap;

use filen_types::fs::Uuid;

use super::stalest_first;

fn uuid(byte: u8) -> Uuid {
	Uuid::from_bytes([byte; 16])
}

/// The pass is routinely killed before it finishes, so the order is what a truncated pass
/// actually delivered. Oldest-listed first, and anything with no stamp at all — never listed,
/// or the account root, which carries no `last_listed` row — ahead of everything.
#[test]
fn the_pass_walks_the_stalest_containers_first() {
	let stamps = HashMap::from([(uuid(1), 300), (uuid(2), 100), (uuid(3), 200)]);
	assert_eq!(
		stalest_first(&stamps, vec![uuid(1), uuid(2), uuid(3), uuid(4)]),
		vec![uuid(4), uuid(2), uuid(3), uuid(1)],
		"unstamped first, then oldest listing to newest"
	);
}

/// Stable across passes: two containers listed in the same millisecond must not reshuffle on
/// every reconnect, or a pass that is always cut short keeps starting somewhere new.
#[test]
fn ties_keep_their_order() {
	let stamps = HashMap::from([(uuid(1), 100), (uuid(2), 100), (uuid(3), 100)]);
	let order = vec![uuid(3), uuid(1), uuid(2)];
	assert_eq!(stalest_first(&stamps, order.clone()), order);
}
