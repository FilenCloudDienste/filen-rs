//! A map of at most a given number of entries that drops the least recently touched one past it:
//! what the hints remember of reads, verifications and damage is bounded, as each forgotten entry
//! costs only work done again (a walk finds a partly read file, a verified file is hashed again).

use std::{
	collections::{BTreeMap, HashMap},
	fmt::Debug,
	hash::Hash,
};

use tracing::debug;

/// The map. Keys are ids the drive minted, so the default randomly seeded hasher stays.
#[derive(Debug)]
pub(super) struct Recent<K, V> {
	entries: HashMap<K, (V, u64)>,
	/// Every key by when it was last touched, oldest first.
	order: BTreeMap<u64, K>,
	next: u64,
	cap: usize,
	/// What the map holds, for the log line of an eviction.
	what: &'static str,
}

impl<K: Copy + Eq + Hash + Debug, V> Recent<K, V> {
	/// An empty map of at most `cap` entries (at least one), naming its entries `what` in the log.
	pub(super) fn new(cap: usize, what: &'static str) -> Self {
		Self {
			entries: HashMap::new(),
			order: BTreeMap::new(),
			next: 0,
			cap: cap.max(1),
			what,
		}
	}

	/// Sets `key` to `value` as the most recently touched entry, dropping the least recently
	/// touched one past the bound.
	pub(super) fn insert(&mut self, key: K, value: V) {
		let tick = self.tick();
		if let Some((_, old)) = self.entries.insert(key, (value, tick)) {
			self.order.remove(&old);
		}
		self.order.insert(tick, key);
		if self.entries.len() > self.cap
			&& let Some((_, oldest)) = self.order.pop_first()
		{
			self.entries.remove(&oldest);
			debug!(what = self.what, key = ?oldest, "forgot the oldest entry past the bound");
		}
	}

	/// The value of `key`, if held, untouched.
	pub(super) fn get(&self, key: &K) -> Option<&V> {
		self.entries.get(key).map(|(value, _)| value)
	}

	/// Whether `key` is held, touching it so it is kept the longest.
	pub(super) fn touch(&mut self, key: &K) -> bool {
		let tick = self.tick();
		let Some((_, at)) = self.entries.get_mut(key) else {
			return false;
		};
		self.order.remove(at);
		*at = tick;
		self.order.insert(tick, *key);
		true
	}

	/// Drops `key`, returning its value.
	pub(super) fn remove(&mut self, key: &K) -> Option<V> {
		let (value, at) = self.entries.remove(key)?;
		self.order.remove(&at);
		Some(value)
	}

	fn tick(&mut self) -> u64 {
		self.next = self.next.wrapping_add(1);
		self.next
	}
}

#[cfg(test)]
mod tests {
	use super::Recent;

	#[test]
	fn the_activity_map_drops_its_oldest_past_its_bound() {
		let mut recent = Recent::new(2, "test entries");
		recent.insert(1, 'a');
		recent.insert(2, 'b');
		assert!(recent.touch(&1));
		recent.insert(3, 'c');
		assert_eq!(recent.entries.len(), 2);
		assert_eq!(recent.order.len(), 2);
		assert_eq!(recent.get(&2), None, "the least recently touched goes");
		assert_eq!(recent.get(&1), Some(&'a'));
		assert_eq!(recent.get(&3), Some(&'c'));
	}
}
