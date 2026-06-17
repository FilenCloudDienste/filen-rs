//! The continuous engine: react to local and remote changes by running a debounced sync pass,
//! plus a periodic safety-net pass.
//!
//! Two trigger sources feed one "dirty" signal: a `notify` filesystem watcher on the local root,
//! and a cache sync-root subscription for remote changes. A burst is debounced (coalesced) into a
//! single [`sync_once`](super::SyncEngine::sync_once). A periodic tick re-syncs regardless, so
//! anything the watcher coalesces or drops (FSEvents/inotify are best-effort) is still caught.
//!
//! Self-induced changes (the engine's own writes/uploads) re-trigger the watcher and the cache
//! subscription; that costs at most one extra pass, which the baseline recognizes as already
//! synced and no-ops. (A tighter in-flight suppression is a future optimization.)

use std::{path::PathBuf, sync::Arc, time::Duration};

use notify::{RecursiveMode, Watcher};
use tokio::sync::Notify;

use super::{baseline::PairId, engine::SyncEngine};
use crate::{
	Error, ErrorKind,
	cache::{SyncRootCallback, SyncRootHandle},
};

/// Quiet window a burst of change events is coalesced over before a sync pass runs.
const DEBOUNCE: Duration = Duration::from_millis(800);
/// Periodic full pass — the backstop for anything the watchers miss or coalesce.
const SAFETY_NET: Duration = Duration::from_secs(300);

/// An active continuous sync. Dropping it stops the background loop, the FS watcher, and the
/// cache subscription.
pub struct WatchHandle {
	// Dropping the sender closes the channel, which breaks the loop's shutdown select arm.
	_shutdown: tokio::sync::oneshot::Sender<()>,
	// Holds the FS watcher and the cache registration alive for the watch's lifetime.
	_watcher: notify::RecommendedWatcher,
	_sync_root: SyncRootHandle,
}

impl SyncEngine {
	/// Start a continuous sync of `pair`: an immediate pass, then a debounced pass on every local
	/// or remote change, plus a periodic safety-net pass. Returns a [`WatchHandle`] that stops
	/// everything when dropped. Requires a multi-threaded runtime (it spawns a background task).
	pub async fn watch(self: Arc<Self>, pair: PairId) -> Result<WatchHandle, Error> {
		let record = {
			let store = self.store.lock().await;
			store
				.pair(pair)
				.map_err(|e| {
					Error::custom_with_source(
						ErrorKind::Internal,
						e,
						Some("loading pair".to_string()),
					)
				})?
				.ok_or_else(|| Error::custom(ErrorKind::InvalidState, "unknown sync pair"))?
		};
		let local_root = PathBuf::from(&record.local_root);

		let dirty = Arc::new(Notify::new());

		// Remote-change trigger: a cache sync-root subscription that pings on any committed batch.
		let remote_dirty = Arc::clone(&dirty);
		let callback: SyncRootCallback = Box::new(move |_events| {
			remote_dirty.notify_one();
		});
		let sync_root = self
			.client
			.clone()
			.add_sync_root(record.remote_root, callback)
			.await?;

		// Local-change trigger: a recursive filesystem watcher on the local root.
		let local_dirty = Arc::clone(&dirty);
		let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
			if res.is_ok() {
				local_dirty.notify_one();
			}
		})
		.map_err(watch_error)?;
		watcher
			.watch(&local_root, RecursiveMode::Recursive)
			.map_err(watch_error)?;

		let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
		let engine = Arc::clone(&self);
		tokio::spawn(run_loop(engine, pair, dirty, shutdown_rx));

		Ok(WatchHandle {
			_shutdown: shutdown_tx,
			_watcher: watcher,
			_sync_root: sync_root,
		})
	}
}

/// The background loop: an initial pass, then debounced passes on `dirty`, plus a periodic pass,
/// until `shutdown` fires (its sender dropped).
async fn run_loop(
	engine: Arc<SyncEngine>,
	pair: PairId,
	dirty: Arc<Notify>,
	mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
	run_pass(&engine, pair).await;

	let mut safety_net = tokio::time::interval(SAFETY_NET);
	safety_net.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	safety_net.tick().await; // consume the immediate first tick (the initial pass covered it)

	loop {
		tokio::select! {
			biased;
			_ = &mut shutdown => break,
			_ = safety_net.tick() => run_pass(&engine, pair).await,
			_ = dirty.notified() => {
				// Coalesce the burst: wait for DEBOUNCE of quiet (each new event restarts it).
				loop {
					tokio::select! {
						biased;
						_ = &mut shutdown => return,
						_ = dirty.notified() => continue,
						_ = tokio::time::sleep(DEBOUNCE) => break,
					}
				}
				run_pass(&engine, pair).await;
			}
		}
	}
}

/// Run one pass, logging (not propagating) any failure — the loop is best-effort and the next
/// trigger or the periodic tick retries.
async fn run_pass(engine: &SyncEngine, pair: PairId) {
	match engine.sync_once(pair).await {
		Ok(report) => {
			if !report.errors.is_empty() {
				tracing::warn!("sync pair {pair}: {} action error(s)", report.errors.len());
			}
		}
		Err(e) => tracing::warn!("sync pair {pair} failed: {e}"),
	}
}

fn watch_error(error: notify::Error) -> Error {
	Error::custom_with_source(ErrorKind::IO, error, Some("filesystem watcher".to_string()))
}
