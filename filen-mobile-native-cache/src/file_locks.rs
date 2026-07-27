//! Per-item locks serialising mutations of a single file's local cache copy.
//!
//! A download writes `cache_dir/<uuid>` while a clear removes it. Without serialisation the two
//! interleave: the provider stops providing an item and immediately re-requests it, and the clear
//! lands *after* the fresh download and evicts it, so the next open re-downloads. Keyed per uuid
//! because one global lock would serialise every download in the app.

use std::{collections::HashMap, sync::Arc};

use filen_types::fs::Uuid;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[derive(Default)]
pub(crate) struct FileLocks {
	locks: Mutex<HashMap<Uuid, Arc<Mutex<()>>>>,
}

impl FileLocks {
	/// Takes the lock for `uuid`, waiting for any in-flight operation on the same item.
	///
	/// Unheld entries are pruned on the way in, so the map stays sized to the operations actually
	/// in flight rather than to every uuid the app has ever touched.
	pub(crate) async fn lock(&self, uuid: Uuid) -> OwnedMutexGuard<()> {
		let lock = {
			let mut locks = self.locks.lock().await;
			// An entry owned only by the map is held by nobody and awaited by nobody. The entry we
			// are about to take survives this: it is cloned below, while the map still holds a
			// copy, and the clone keeps the count above one for as long as we need it.
			locks.retain(|_, lock| Arc::strong_count(lock) > 1);
			Arc::clone(locks.entry(uuid).or_default())
		};
		lock.lock_owned().await
	}

	#[cfg(test)]
	async fn tracked(&self) -> usize {
		self.locks.lock().await.len()
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	// The timeouts below are assertions about blocking, not about speed: under `start_paused`
	// tokio only advances the clock once every task is stalled, so a blocked acquisition reaches
	// the deadline deterministically and a free one resolves without the clock moving at all.
	// On the real clock the `is_ok()` cases would be a bet on being scheduled within 50 ms.

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	/// Two operations on the SAME item must not run at once — this is the whole point.
	#[tokio::test(start_paused = true)]
	async fn a_second_lock_on_one_item_waits_for_the_first() {
		let locks = FileLocks::default();
		let guard = locks.lock(uuid(1)).await;

		let waited = tokio::time::timeout(Duration::from_millis(50), locks.lock(uuid(1))).await;
		assert!(
			waited.is_err(),
			"the second acquisition must block while the first is held"
		);

		drop(guard);
		assert!(
			tokio::time::timeout(Duration::from_millis(50), locks.lock(uuid(1)))
				.await
				.is_ok(),
			"releasing the first must let the second through"
		);
	}

	/// Different items must not contend, or one slow download would stall every other file.
	#[tokio::test(start_paused = true)]
	async fn locks_on_different_items_are_independent() {
		let locks = FileLocks::default();
		let _held = locks.lock(uuid(1)).await;

		assert!(
			tokio::time::timeout(Duration::from_millis(50), locks.lock(uuid(2)))
				.await
				.is_ok(),
			"a different uuid must not wait on an unrelated item"
		);
	}

	/// The map must not accumulate an entry per uuid ever touched.
	#[tokio::test]
	async fn released_entries_are_pruned() {
		let locks = FileLocks::default();
		for byte in 0..8 {
			drop(locks.lock(uuid(byte)).await);
		}

		// Acquiring once more prunes everything released above, leaving only the live entry.
		let _held = locks.lock(uuid(200)).await;
		assert_eq!(locks.tracked().await, 1);
	}

	/// A held entry must survive another caller's prune, or the two would end up with different
	/// mutexes for the same item and stop excluding each other.
	#[tokio::test(start_paused = true)]
	async fn a_held_entry_survives_a_prune() {
		let locks = FileLocks::default();
		let guard = locks.lock(uuid(1)).await;

		// Traffic on other uuids drives the prune while uuid(1) is still held.
		for byte in 10..14 {
			drop(locks.lock(uuid(byte)).await);
		}

		assert!(
			tokio::time::timeout(Duration::from_millis(50), locks.lock(uuid(1)))
				.await
				.is_err(),
			"the surviving entry must still exclude a second acquisition"
		);
		drop(guard);
	}
}
