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
//!
//! A pair that is ALREADY paused cannot be watched — [`watch`](super::SyncEngine::watch) refuses
//! it, so a handle whose loop runs nothing is never handed out. While a pair is
//! [`paused`](super::SyncEngine::pause_pair) under a running watch the loop runs no passes and leaves
//! the dirty signal alone, so it is still pending when the pair resumes: whatever happened during
//! the pause is picked up by the first pass afterwards. The watcher and the cache subscription are
//! left registered throughout — tearing either down would cost a full relist to rebuild.

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

/// Default quiet window a burst of change events is coalesced over before a sync pass runs.
const DEBOUNCE: Duration = Duration::from_millis(800);
/// Default periodic full pass — the backstop for anything the watchers miss or coalesce.
const SAFETY_NET: Duration = Duration::from_secs(300);
/// Delay after the first failed pass; doubles per consecutive failure up to [`MAX_BACKOFF`].
const BASE_BACKOFF: Duration = Duration::from_secs(2);
/// Ceiling on that backoff, so a persistent failure still retries about as often as the safety net.
const MAX_BACKOFF: Duration = Duration::from_secs(300);

/// The timings a continuous watch runs on. [`Default`] is what [`SyncEngine::watch`] and
/// [`SyncEngine::watch_observed`] use; [`SyncEngine::watch_with`] takes an explicit one — mainly so
/// a test can drive the loop faster than the production cadence, but also for a caller that wants a
/// tighter safety net (a shared folder several devices write to) or a looser one (a metered link).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchConfig {
	/// Quiet window a burst of change events is coalesced over before one pass runs. Every event
	/// restarts it, so a continuous stream of writes syncs once it stops, not repeatedly during.
	/// Must be non-zero: a zero debounce turns every filesystem event into its own pass.
	pub debounce: Duration,
	/// How long after the last SUCCESSFUL pass one runs regardless of any trigger — the backstop
	/// for what the FS watcher and the cache subscription miss or coalesce. An event-triggered pass
	/// pushes the next net pass out by a full interval instead of being trailed by a redundant one.
	/// Must exceed [`debounce`](Self::debounce): a safety net inside the coalescing window would
	/// keep firing before a burst could ever settle.
	pub safety_net: Duration,
}

impl Default for WatchConfig {
	fn default() -> Self {
		Self {
			debounce: DEBOUNCE,
			safety_net: SAFETY_NET,
		}
	}
}

impl WatchConfig {
	/// Reject a configuration the loop cannot honour, at `watch_with` time rather than as a
	/// misbehaving background task nobody is watching.
	fn validate(&self) -> Result<(), Error> {
		if self.debounce.is_zero() {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				"watch debounce must be greater than zero",
			));
		}
		if self.safety_net <= self.debounce {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!(
					"watch safety net ({:?}) must be longer than the debounce ({:?})",
					self.safety_net, self.debounce
				),
			));
		}
		Ok(())
	}
}

/// An active continuous sync. Dropping it stops the background loop, the FS watcher, and the
/// cache subscription; [`stop`](Self::stop) does the same but waits for the loop to finish.
pub struct WatchHandle {
	// Dropping the sender closes the channel, which breaks the loop's shutdown select arm.
	shutdown: tokio::sync::oneshot::Sender<()>,
	// Resolves when the background loop has returned; `stop` awaits it.
	loop_done: tokio::task::JoinHandle<()>,
	status: tokio::sync::watch::Receiver<WatchStatus>,
	// Holds the FS watcher and the cache registration alive for the watch's lifetime.
	_watcher: notify::RecommendedWatcher,
	_sync_root: SyncRootHandle,
}

impl WatchHandle {
	/// Stop the watch and wait for the background loop to finish (up to one in-flight pass), so a
	/// caller can be sure nothing is still touching either side when this returns. Dropping the
	/// handle stops the loop too, but does not wait for it.
	pub async fn stop(self) {
		let Self {
			shutdown,
			loop_done,
			status: _status,
			_watcher,
			_sync_root,
		} = self;
		drop(shutdown);
		// The loop only fails to join if it panicked; nothing left to wait for either way.
		let _ = loop_done.await;
	}

	/// Watch the loop's health: every completed pass publishes a [`WatchStatus`], so a caller can
	/// react to a watch that is failing (its passes error out) instead of only seeing it go quiet.
	pub fn status(&self) -> tokio::sync::watch::Receiver<WatchStatus> {
		self.status.clone()
	}
}

/// The health of a running watch, published after every pass. A pass failing outright (as opposed
/// to a single action inside it, which surfaces as [`SyncEvent::ActionFailed`]) is otherwise
/// invisible: the loop logs it and retries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchStatus {
	/// Passes that failed back to back; reset to zero by the next successful pass.
	pub consecutive_failures: u32,
	/// Why the last pass failed, or `None` while the watch is healthy.
	pub last_error: Option<String>,
}

impl SyncEngine {
	/// Start a continuous sync of `pair`: an immediate pass, then a debounced pass on every local
	/// or remote change, plus a periodic safety-net pass. Returns a [`WatchHandle`] that stops
	/// everything when dropped. Requires a multi-threaded runtime (it spawns a background task).
	///
	/// A [`paused`](Self::pause_pair) pair cannot be watched: starting a watch on one is an error,
	/// because a handle whose loop is doing nothing (and says nothing about why) is indistinguishable
	/// from a broken watch. Resume the pair first, then watch it.
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
		self.watch_with(pair, WatchConfig::default(), observer)
			.await
	}

	/// Like [`watch_observed`](Self::watch_observed), but on `config`'s timings instead of the
	/// defaults. Errors if the configuration is not one the loop can honour (see [`WatchConfig`]),
	/// and — like the other two — if the pair is paused.
	pub async fn watch_with(
		self: Arc<Self>,
		pair: PairId,
		config: WatchConfig,
		observer: SyncObserver,
	) -> Result<WatchHandle, Error> {
		config.validate()?;
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
		if record.paused {
			return Err(Error::custom(
				ErrorKind::InvalidState,
				format!("sync pair {pair} is paused: resume it before starting a watch on it"),
			));
		}
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
		let (status_tx, status_rx) = tokio::sync::watch::channel(WatchStatus::default());
		let engine = Arc::clone(&self);
		let loop_done = tokio::spawn(run_loop(
			engine,
			pair,
			config,
			dirty,
			shutdown_rx,
			observer,
			status_tx,
		));

		Ok(WatchHandle {
			shutdown: shutdown_tx,
			loop_done,
			status: status_rx,
			_watcher: watcher,
			_sync_root: sync_root,
		})
	}
}

/// The background loop: an initial pass, then debounced passes on `dirty`, plus a periodic pass,
/// until `shutdown` fires (its sender dropped). A pass that fails outright backs the loop off (see
/// [`backoff`]) so a persistent error does not hot-loop at the debounce cadence; that backoff is
/// also the retry timer, since an outage over a quiescent tree produces no trigger of its own.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
	engine: Arc<SyncEngine>,
	pair: PairId,
	config: WatchConfig,
	dirty: Arc<Notify>,
	mut shutdown: tokio::sync::oneshot::Receiver<()>,
	mut observer: SyncObserver,
	status: tokio::sync::watch::Sender<WatchStatus>,
) {
	let mut failures: u32 = 0;

	let mut safety_net = tokio::time::interval(config.safety_net);
	safety_net.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	safety_net.tick().await; // consume the immediate first tick (the initial pass below covers it)

	loop {
		if engine.is_paused(pair).await {
			if !wait_while_paused(&mut shutdown, config.debounce).await {
				return;
			}
			continue;
		}

		let error = run_pass(&engine, pair, observer.as_mut()).await;
		if error.is_none() {
			failures = 0;
			// The net measures time since the last SUCCESSFUL pass, not since the last tick: a pass
			// that just succeeded has already done everything the net would do, so a tick that came
			// due during it buys nothing but a no-op pass right after. A FAILED pass deliberately
			// leaves the interval alone — its own retry timer is the backoff, and a run of failures
			// must not push the net out indefinitely.
			safety_net.reset();
		} else {
			failures = failures.saturating_add(1);
		}
		// A closed channel just means nobody is watching the health any more.
		let _ = status.send(WatchStatus {
			consecutive_failures: failures,
			last_error: error,
		});
		let delay = backoff(failures);
		if let Some(delay) = delay {
			tracing::warn!(
				"sync pair {pair}: {failures} consecutive failure(s); waiting {delay:?} before the next pass"
			);
		}

		if !wait_for_next_pass(
			&mut shutdown,
			&dirty,
			&mut safety_net,
			config.debounce,
			delay,
		)
		.await
		{
			return;
		}
	}
}

/// Wait out one poll interval while the pair is paused. Returns `false` if the watch was stopped.
///
/// Deliberately does NOT touch `dirty`: a change that happens while the pair is paused leaves its
/// signal pending, so the first pass after the resume is the one that catches up on the lot. The
/// pause itself is polled rather than signalled — the alternative is per-pair wakeup plumbing
/// through the engine to save a timer that fires at the debounce cadence.
async fn wait_while_paused(
	shutdown: &mut tokio::sync::oneshot::Receiver<()>,
	poll: Duration,
) -> bool {
	tokio::select! {
		biased;
		_ = &mut *shutdown => false,
		_ = tokio::time::sleep(poll) => true,
	}
}

/// Wait for the next pass to be due. Returns `false` if the watch was stopped instead.
///
/// After a failed pass that `backoff` delay *is* the whole wait: it is the retry timer, so a
/// persistent failure retries on the backoff schedule rather than falling through to whatever the
/// safety net or a change event happens to offer next. While healthy the wait ends on the periodic
/// safety-net tick, or on a change event once the burst behind it has gone quiet for `debounce`.
async fn wait_for_next_pass(
	shutdown: &mut tokio::sync::oneshot::Receiver<()>,
	dirty: &Notify,
	safety_net: &mut tokio::time::Interval,
	debounce: Duration,
	backoff: Option<Duration>,
) -> bool {
	if let Some(delay) = backoff {
		return tokio::select! {
			biased;
			_ = &mut *shutdown => false,
			_ = tokio::time::sleep(delay) => true,
		};
	}

	tokio::select! {
		biased;
		_ = &mut *shutdown => return false,
		_ = safety_net.tick() => {}
		_ = dirty.notified() => {
			// Coalesce the burst: wait for `debounce` of quiet (each new event restarts it).
			loop {
				tokio::select! {
					biased;
					_ = &mut *shutdown => return false,
					_ = dirty.notified() => continue,
					_ = tokio::time::sleep(debounce) => break,
				}
			}
		}
	}
	true
}

/// How long to wait before the next pass after `failures` consecutive failed ones: nothing while
/// healthy, then [`BASE_BACKOFF`] doubling per failure, capped at [`MAX_BACKOFF`].
fn backoff(failures: u32) -> Option<Duration> {
	let doublings = failures.checked_sub(1)?.min(31);
	Some((BASE_BACKOFF * 2u32.pow(doublings)).min(MAX_BACKOFF))
}

/// Run one pass, logging (not propagating) any failure — the loop is best-effort and the next
/// trigger or the periodic tick retries. `observer` receives this pass's [`SyncEvent`]s. Returns
/// the pass's own error, if any (per-action errors inside a completed pass do not count).
async fn run_pass(
	engine: &SyncEngine,
	pair: PairId,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> Option<String> {
	match engine.sync_once_observed(pair, observer).await {
		Ok(report) => {
			if !report.errors.is_empty() {
				tracing::warn!("sync pair {pair}: {} action error(s)", report.errors.len());
			}
			None
		}
		Err(e) => {
			tracing::warn!("sync pair {pair} failed: {e}");
			Some(e.to_string())
		}
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
	use std::{
		path::{Path, PathBuf},
		time::Duration,
	};

	use notify::{
		EventKind,
		event::{CreateKind, ModifyKind, RenameMode},
	};
	use tokio::sync::Notify;

	use super::{
		BASE_BACKOFF, DEBOUNCE, MAX_BACKOFF, SAFETY_NET, WatchConfig, backoff, triggers_pass,
		wait_for_next_pass, wait_while_paused,
	};

	/// A safety-net interval as `run_loop` sets one up: the immediate first tick consumed, so the
	/// next one is a full [`SAFETY_NET`] away.
	async fn safety_net() -> tokio::time::Interval {
		let mut interval = tokio::time::interval(SAFETY_NET);
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		interval.tick().await;
		interval
	}

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

	/// The retry after a failed pass must be governed by the backoff alone. Nothing wakes the loop
	/// during an outage that leaves the tree quiescent — no local writes, no remote batches — so if
	/// the backoff is only a *prefix* to the usual wait, the retry lands on the safety-net cadence
	/// and the backoff schedule is decoration.
	#[tokio::test(start_paused = true)]
	async fn a_failed_pass_retries_on_the_backoff_schedule() {
		let (_shutdown_tx, mut shutdown) = tokio::sync::oneshot::channel();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;

		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut shutdown,
				&dirty,
				&mut safety_net,
				DEBOUNCE,
				Some(BASE_BACKOFF)
			)
			.await,
			"a backoff that elapsed means retry, not stop"
		);
		assert_eq!(
			start.elapsed(),
			BASE_BACKOFF,
			"the backoff is the whole wait before the retry"
		);
	}

	/// A healthy wait is unchanged: a change event, coalesced over [`DEBOUNCE`], or the safety net.
	#[tokio::test(start_paused = true)]
	async fn a_healthy_wait_ends_on_a_debounced_change_or_the_safety_net() {
		let (_shutdown_tx, mut shutdown) = tokio::sync::oneshot::channel();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;

		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(wait_for_next_pass(&mut shutdown, &dirty, &mut safety_net, DEBOUNCE, None).await);
		assert_eq!(start.elapsed(), DEBOUNCE, "a change waits out the debounce");

		let start = tokio::time::Instant::now();
		assert!(wait_for_next_pass(&mut shutdown, &dirty, &mut safety_net, DEBOUNCE, None).await);
		assert_eq!(
			start.elapsed(),
			SAFETY_NET - DEBOUNCE,
			"with nothing happening, the safety net is what ends the wait"
		);
	}

	/// Stopping the watch is never delayed by a backoff.
	#[tokio::test(start_paused = true)]
	async fn a_stop_interrupts_the_backoff() {
		let (shutdown_tx, mut shutdown) = tokio::sync::oneshot::channel();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;

		drop(shutdown_tx);
		let start = tokio::time::Instant::now();
		assert!(
			!wait_for_next_pass(
				&mut shutdown,
				&dirty,
				&mut safety_net,
				DEBOUNCE,
				Some(MAX_BACKOFF)
			)
			.await,
			"a stopped watch must not wait out the backoff"
		);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// A paused pair polls, and never consumes the change signal: the notification a paused loop
	/// walked past is still there for the first wait after the resume, so the backlog syncs then
	/// rather than waiting out a whole safety-net interval.
	#[tokio::test(start_paused = true)]
	async fn a_paused_loop_leaves_the_change_signal_pending() {
		let (shutdown_tx, mut shutdown) = tokio::sync::oneshot::channel();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let poll = Duration::from_millis(200);

		// A change lands while the pair is paused; two paused polls walk past it.
		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(wait_while_paused(&mut shutdown, poll).await);
		assert!(wait_while_paused(&mut shutdown, poll).await);
		assert_eq!(
			start.elapsed(),
			poll * 2,
			"a paused poll waits its interval"
		);

		// Resumed: the pending notification is what ends the very next wait, one debounce later —
		// not the safety net, which is far away.
		let start = tokio::time::Instant::now();
		assert!(wait_for_next_pass(&mut shutdown, &dirty, &mut safety_net, DEBOUNCE, None).await);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE,
			"the change made during the pause was swallowed"
		);

		// And stopping the watch is never delayed by a paused poll.
		drop(shutdown_tx);
		let start = tokio::time::Instant::now();
		assert!(!wait_while_paused(&mut shutdown, Duration::from_secs(600)).await);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// A configured debounce, not the default one, is what a burst is coalesced over.
	#[tokio::test(start_paused = true)]
	async fn the_configured_debounce_is_what_a_burst_waits_out() {
		let (_shutdown_tx, mut shutdown) = tokio::sync::oneshot::channel();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let debounce = Duration::from_millis(50);

		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(wait_for_next_pass(&mut shutdown, &dirty, &mut safety_net, debounce, None).await);
		assert_eq!(start.elapsed(), debounce);
	}

	#[test]
	fn a_watch_config_the_loop_cannot_honour_is_refused() {
		let default = WatchConfig::default();
		assert_eq!(default.debounce, DEBOUNCE);
		assert_eq!(default.safety_net, SAFETY_NET);
		default.validate().expect("the defaults must be valid");

		// A zero debounce turns every filesystem event into its own pass.
		assert!(
			WatchConfig {
				debounce: Duration::ZERO,
				..default
			}
			.validate()
			.is_err()
		);
		// A safety net inside the coalescing window fires before a burst can ever settle — equal
		// counts, since the tick would land exactly as the debounce expires.
		assert!(
			WatchConfig {
				debounce: Duration::from_secs(10),
				safety_net: Duration::from_secs(5),
			}
			.validate()
			.is_err()
		);
		assert!(
			WatchConfig {
				debounce: Duration::from_secs(5),
				safety_net: Duration::from_secs(5),
			}
			.validate()
			.is_err()
		);
		WatchConfig {
			debounce: Duration::from_millis(1),
			safety_net: Duration::from_millis(2),
		}
		.validate()
		.expect("a tight but ordered pair is fine");
	}

	#[test]
	fn backoff_grows_then_caps_and_resets() {
		assert_eq!(backoff(0), None, "a healthy loop must not wait");
		assert_eq!(backoff(1), Some(BASE_BACKOFF));
		assert_eq!(backoff(2), Some(BASE_BACKOFF * 2));
		assert_eq!(backoff(3), Some(BASE_BACKOFF * 4));
		// Capped, and never panicking on an absurd failure count.
		assert_eq!(backoff(40), Some(MAX_BACKOFF));
		assert_eq!(backoff(u32::MAX), Some(MAX_BACKOFF));
	}
}
