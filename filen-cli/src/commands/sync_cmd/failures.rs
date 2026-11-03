//! Failures a watch has printed already, so that a path that fails the same way on every pass is
//! named once rather than on every pass.

use std::collections::{HashMap, HashSet};

use filen_sdk_rs::sync_engine::{PairId, SyncEvent};

/// How many distinct failures of a pair are remembered. A failure past them is printed every time,
/// so a watch whose failures keep changing, an error naming a time for one, does not grow without
/// bound.
const MAX_REMEMBERED: usize = 10_000;

/// The failures each pair's passes have printed, and how many the pass in flight left out.
#[derive(Default)]
pub(super) struct Failures {
	/// `<path>: <error>` of each failure printed, by pair.
	printed: HashMap<PairId, HashSet<String>>,
	/// How many failures of the pass in flight were left out, by pair.
	repeated: HashMap<PairId, usize>,
}

impl Failures {
	/// Notes `event` of `pair`, and answers whether its line is left out: a failure of the same
	/// path with the same error as one printed before.
	pub(super) fn is_repeat(&mut self, pair: PairId, event: &SyncEvent) -> bool {
		match event {
			SyncEvent::PassStarted { .. } => {
				self.repeated.remove(&pair);
				false
			}
			SyncEvent::ActionFailed { rel_path, error } => {
				let printed = self.printed.entry(pair).or_default();
				let failure = format!("{rel_path}: {error}");
				if printed.contains(&failure) {
					let repeated = self.repeated.entry(pair).or_default();
					*repeated = repeated.saturating_add(1);
					return true;
				}
				if printed.len() < MAX_REMEMBERED {
					printed.insert(failure);
				}
				false
			}
			// no other event is a failure of a path
			_ => false,
		}
	}

	/// How many failures of `pair`'s pass in flight were left out.
	pub(super) fn repeated(&self, pair: PairId) -> usize {
		self.repeated.get(&pair).copied().unwrap_or(0)
	}
}

#[cfg(test)]
mod tests {
	use filen_sdk_rs::sync_engine::{SyncEvent, SyncMode};

	use super::{Failures, MAX_REMEMBERED};

	fn failed(rel_path: &str, error: &str) -> SyncEvent {
		SyncEvent::ActionFailed {
			rel_path: rel_path.to_owned(),
			error: error.to_owned(),
		}
	}

	fn started() -> SyncEvent {
		SyncEvent::PassStarted {
			mode: SyncMode::TwoWay,
		}
	}

	#[test]
	fn a_failure_is_printed_once_per_watch_and_its_repeats_counted_per_pass() {
		let mut failures = Failures::default();

		assert!(!failures.is_repeat(1, &started()));
		assert!(!failures.is_repeat(1, &failed("a", "denied")));
		assert_eq!(failures.repeated(1), 0);
		assert!(!failures.is_repeat(1, &started()));
		assert!(failures.is_repeat(1, &failed("a", "denied")));
		assert!(failures.is_repeat(1, &failed("a", "denied")));
		assert_eq!(failures.repeated(1), 2);
		assert!(!failures.is_repeat(1, &started()));
		assert_eq!(failures.repeated(1), 0);
	}

	#[test]
	fn another_error_or_another_pair_is_no_repeat() {
		let mut failures = Failures::default();

		assert!(!failures.is_repeat(1, &failed("a", "denied")));
		assert!(!failures.is_repeat(1, &failed("a", "too large")));
		assert!(!failures.is_repeat(1, &failed("b", "denied")));
		assert!(!failures.is_repeat(2, &failed("a", "denied")));
	}

	#[test]
	fn failures_past_the_remembered_ones_are_always_printed() {
		let mut failures = Failures::default();
		for i in 0..MAX_REMEMBERED {
			assert!(!failures.is_repeat(1, &failed(&i.to_string(), "e")));
		}

		assert!(failures.is_repeat(1, &failed(&(MAX_REMEMBERED - 1).to_string(), "e")));
		assert!(!failures.is_repeat(1, &failed("one more", "e")));
		assert!(!failures.is_repeat(1, &failed("one more", "e")));
	}
}
