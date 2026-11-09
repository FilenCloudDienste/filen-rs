//! A seam the store-hold tests watch: [`off_store`](super::off_store) reports here, on its blocking
//! thread, right before it takes a store's lock, so a test can tell that a read has reached a lock
//! it holds rather than inferring it from the read being unfinished.

use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

use super::SharedStore;

/// The stores a test is waiting on, each with the sender its next lock attempt fires. Keyed by the
/// store's address, which stays unique for as long as the test that registered it holds the store.
static WATCHED: Mutex<Vec<(usize, oneshot::Sender<()>)>> = Mutex::new(Vec::new());

fn key(store: &SharedStore) -> usize {
	Arc::as_ptr(store).addr()
}

/// Answers once a blocking hop is about to take `store`'s lock.
pub(super) fn next_store_lock(store: &SharedStore) -> oneshot::Receiver<()> {
	let (tx, rx) = oneshot::channel();
	WATCHED
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
		.push((key(store), tx));
	rx
}

/// Fire every waiter registered for `store`; a no-op for a store nobody watches.
pub(super) fn reaching_store_lock(store: &SharedStore) {
	let key = key(store);
	let mut watched = WATCHED.lock().unwrap_or_else(PoisonError::into_inner);
	for (_, tx) in watched.extract_if(.., |(watched, _)| *watched == key) {
		// The waiting test may have given up already; nothing else listens.
		let _ = tx.send(());
	}
}
