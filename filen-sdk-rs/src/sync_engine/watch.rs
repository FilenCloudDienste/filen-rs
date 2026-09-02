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
//! synced and no-ops. The engine's own *staging* writes — download temp files and the quarantine
//! bin — are filtered out instead (see [`triggers_pass`]): they are never content the next pass
//! would act on, so a pass for them is pure waste.

use std::{
	ffi::OsStr,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use notify::{RecursiveMode, Watcher};
use tokio::sync::Notify;

use super::{SyncEvent, SyncObserver, baseline::PairId, engine::SyncEngine, scan::QUARANTINE_DIR};
use crate::{
	Error, ErrorKind,
	cache::{SyncRootCallback, SyncRootHandle},
	io::DOWNLOAD_TMP_EXT,
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
		self.watch_observed(pair, Box::new(|_| {})).await
	}

	/// Like [`watch`](Self::watch), but `observer` receives a [`SyncEvent`] for everything every
	/// pass does (see [`SyncEvent`] for the order) — the only way to observe a continuous sync,
	/// which otherwise discards each pass's report. The observer is moved into the background task
	/// and called synchronously between async steps, so keep it quick (offload to a channel).
	pub async fn watch_observed(
		self: Arc<Self>,
		pair: PairId,
		observer: SyncObserver,
	) -> Result<WatchHandle, Error> {
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

		// Local-change trigger: a recursive filesystem watcher on the local root. The watcher
		// reports canonicalized paths, so the root it filters against must be canonical too.
		let local_dirty = Arc::clone(&dirty);
		let watch_root = std::fs::canonicalize(&local_root).unwrap_or_else(|_| local_root.clone());
		let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
			if let Ok(event) = &res
				&& triggers_pass(&watch_root, event)
			{
				local_dirty.notify_one();
			}
		})
		.map_err(watch_error)?;
		watcher
			.watch(&local_root, RecursiveMode::Recursive)
			.map_err(watch_error)?;

		let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
		let engine = Arc::clone(&self);
		tokio::spawn(run_loop(engine, pair, dirty, shutdown_rx, observer));

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
	mut observer: SyncObserver,
) {
	run_pass(&engine, pair, observer.as_mut()).await;

	let mut safety_net = tokio::time::interval(SAFETY_NET);
	safety_net.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	safety_net.tick().await; // consume the immediate first tick (the initial pass covered it)

	loop {
		tokio::select! {
			biased;
			_ = &mut shutdown => break,
			_ = safety_net.tick() => run_pass(&engine, pair, observer.as_mut()).await,
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
				run_pass(&engine, pair, observer.as_mut()).await;
			}
		}
	}
}

/// Run one pass, logging (not propagating) any failure — the loop is best-effort and the next
/// trigger or the periodic tick retries. `observer` receives this pass's [`SyncEvent`]s.
async fn run_pass(engine: &SyncEngine, pair: PairId, observer: &mut (dyn FnMut(SyncEvent) + Send)) {
	match engine.sync_once_observed(pair, observer).await {
		Ok(report) => {
			if !report.errors.is_empty() {
				tracing::warn!("sync pair {pair}: {} action error(s)", report.errors.len());
			}
		}
		Err(e) => tracing::warn!("sync pair {pair} failed: {e}"),
	}
}

/// Whether a filesystem event under `root` is worth waking the loop for.
///
/// Everything is, except the engine's own staging writes: the `<uuid>.filendl` temp file a
/// download writes before renaming it into place (the rename is itself an event), and the
/// quarantine bin, which the scan never descends into. A user file that happens to end in
/// `.filendl` is not lost, only delayed to the next safety-net pass. A pathless event (a watcher
/// rescan notice) always counts.
fn triggers_pass(root: &Path, event: &notify::Event) -> bool {
	event.paths.is_empty()
		|| event
			.paths
			.iter()
			.any(|path| !is_engine_staging(root, path))
}

/// Whether `path` is one of the engine's own staging locations under `root` (see [`triggers_pass`]).
fn is_engine_staging(root: &Path, path: &Path) -> bool {
	let Ok(rel) = path.strip_prefix(root) else {
		return false;
	};
	rel.extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT))
		|| rel.components().next().map(|c| c.as_os_str()) == Some(OsStr::new(QUARANTINE_DIR))
}

fn watch_error(error: notify::Error) -> Error {
	Error::custom_with_source(ErrorKind::IO, error, Some("filesystem watcher".to_string()))
}

#[cfg(test)]
mod tests {
	use std::path::{Path, PathBuf};

	use notify::{
		EventKind,
		event::{CreateKind, ModifyKind, RenameMode},
	};

	use super::triggers_pass;

	fn event(kind: EventKind, paths: &[&str]) -> notify::Event {
		notify::Event {
			kind,
			paths: paths.iter().map(PathBuf::from).collect(),
			attrs: Default::default(),
		}
	}

	#[test]
	fn only_the_engines_own_staging_writes_are_filtered_out() {
		let root = Path::new("/sync/root");
		// Real changes wake the loop.
		assert!(triggers_pass(
			root,
			&event(EventKind::Create(CreateKind::File), &["/sync/root/a.txt"])
		));
		// The rename that commits a download does too — it is the real file appearing.
		assert!(triggers_pass(
			root,
			&event(
				EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
				&["/sync/root/a.txt"]
			)
		));
		// A pathless rescan notice always counts.
		assert!(triggers_pass(root, &event(EventKind::Other, &[])));
		// A path outside the root is not ours to classify.
		assert!(triggers_pass(
			root,
			&event(
				EventKind::Create(CreateKind::File),
				&["/elsewhere/x.filendl"]
			)
		));
		// The engine's own staging writes do not.
		assert!(!triggers_pass(
			root,
			&event(
				EventKind::Create(CreateKind::File),
				&["/sync/root/sub/dee76e0e-0000-0000-0000-000000000000.filendl"]
			)
		));
		assert!(!triggers_pass(
			root,
			&event(
				EventKind::Create(CreateKind::File),
				&["/sync/root/.filen-sync-trash/gone.txt"]
			)
		));
		// A batch that mentions both still wakes the loop, for the real path.
		assert!(triggers_pass(
			root,
			&event(
				EventKind::Create(CreateKind::File),
				&["/sync/root/.filen-sync-trash/gone.txt", "/sync/root/a.txt"]
			)
		));
	}
}
