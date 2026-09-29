//! The continuous engine: react to local and remote changes by running a debounced sync pass,
//! plus a periodic safety-net pass.
//!
//! Two trigger sources feed one "dirty" signal: a `notify` filesystem watcher on the local root,
//! and a cache sync-root subscription for remote changes. A burst is debounced (coalesced) into a
//! single [`sync_once`](super::SyncEngine::sync_once). A periodic tick re-syncs regardless, so
//! anything the watcher coalesces or drops (FSEvents/inotify are best-effort) is still caught; how
//! often it runs scales with what a whole-tree pass costs this pair (see
//! [`WatchConfig::safety_net`]), since that tick is the only pass an idle pair runs.
//!
//! Neither trigger source is required. A filesystem watcher that cannot start or cannot cover the
//! whole tree (on Linux, typically a tree with more directories than `fs.inotify.max_user_watches`
//! allows) and a cache subscription the cache refuses are logged, recorded on the
//! [`WatchStatus`], and leave that side on the safety net alone — a slower sync, not none. A
//! watcher error at runtime is recorded the same way, and wakes the loop for a pass.
//!
//! Self-induced changes (the engine's own writes/uploads) re-trigger the watcher and the cache
//! subscription; that costs at most one extra pass, which the baseline recognizes as already
//! synced and no-ops. The engine's own *staging* writes — download temp files and the quarantine
//! bin — are filtered out instead (see [`triggers_pass`]): they are never content the next pass
//! would act on, so a pass for them is pure waste.
//!
//! A pair that is ALREADY paused cannot be watched — [`watch`](super::SyncEngine::watch) refuses
//! it, so a handle whose loop runs nothing is never handed out. Pausing a pair whose watch is
//! mid-pass reaches into that pass exactly as it does for a one-shot
//! [`sync_once`](super::SyncEngine::sync_once) (see
//! [`pause_pair_with`](super::SyncEngine::pause_pair_with)): it parks, or it unwinds. While a pair is
//! [`paused`](super::SyncEngine::pause_pair) under a running watch the loop runs no passes and leaves
//! the dirty signal alone, so it is still pending when the pair resumes: whatever happened during
//! the pause is picked up by the first pass afterwards — including the remainder of a pass the
//! pause itself cut short, which re-arms that signal on its way out. The watcher and the cache
//! subscription are left registered throughout — tearing either down would cost a full relist to
//! rebuild.

use std::{
	ffi::OsStr,
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use notify::{EventHandler, RecursiveMode, Watcher};
use tokio::{sync::Notify, time::Instant};
use tracing::Instrument;

use super::{
	SyncEvent, SyncObserver, SyncReport,
	apply::{LOCK_FAILURE, STORE_FAILURE},
	baseline::PairId,
	changes::{FullPassReason, PairChanges},
	engine::SyncEngine,
	scan::QUARANTINE_DIR,
};
use crate::{
	Error, ErrorKind,
	cache::{SyncRootCallback, SyncRootHandle},
	io::DOWNLOAD_TMP_EXT,
};

/// Default quiet window a burst of change events is coalesced over before a sync pass runs.
const DEBOUNCE: Duration = Duration::from_millis(800);
/// How many debounce windows a burst of change events may hold a pass off for, whether or not it
/// ever goes quiet. A tree something writes into continuously — a build directory, a log — never
/// does, and the quiet window alone would hold the pass for as long as the writing lasts.
const MAX_DEBOUNCE_BURST: u32 = 8;
/// Default FLOOR for the periodic whole-tree pass — the backstop for anything the watchers miss or
/// coalesce. The interval actually used scales up from here with what such a pass costs this pair
/// (see [`net_interval`]).
const SAFETY_NET: Duration = Duration::from_secs(300);
/// How much wall time the safety net leaves between whole-tree passes per unit of time one COSTS:
/// a 1 % duty cycle, so a pair never spends more than about a hundredth of its life re-reading
/// itself for a change nothing announced.
const SAFETY_NET_DUTY: u32 = 100;
/// Ceiling on that scaled interval, however long a whole-tree pass takes: beyond this the backstop
/// is too rare to be one. A pair whose pass costs more than `MAX_SAFETY_NET / SAFETY_NET_DUTY`
/// (3.6 min) therefore runs a hotter duty cycle than 1 % — the deliberate trade, since a change no
/// notification carried must not wait most of a day to be found.
const MAX_SAFETY_NET: Duration = Duration::from_secs(6 * 60 * 60);
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
	/// The FLOOR for how long after the last whole-tree pass another runs regardless of any trigger
	/// — the backstop for what the FS watcher and the cache subscription miss or coalesce. An
	/// event-triggered pass pushes the next net pass out by a full interval instead of being
	/// trailed by a redundant one. Must exceed [`debounce`](Self::debounce): a safety net inside the
	/// coalescing window would keep firing before a burst could ever settle.
	///
	/// The interval the loop actually waits is derived from this and from what READING both sides
	/// whole last cost this pair (`SyncReport::read_cost` — the baseline, the scan, the snapshot
	/// and the view, never the transfers or a wait for the drive-write lock): `SAFETY_NET_DUTY`
	/// (100) times that duration, clamped to this floor and to `MAX_SAFETY_NET` (6 h). A small pair
	/// measures a read in milliseconds and so keeps
	/// exactly this cadence; a pair large enough for a pass to take seconds polls proportionally
	/// less often, because that tick is the only work an idle pair does and a fixed interval would
	/// spend hours of CPU a day on a tree nothing is changing. A value above the ceiling is
	/// honoured as the floor it is: the caller asked for a slower backstop.
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
///
/// The loop can also end on its own, when the pair is removed underneath it (see
/// [`SyncEngine::remove_pair`](super::SyncEngine::remove_pair)); the handle stays usable either
/// way, and [`status`](Self::status) says which of the two it was (see [`WatchState`]).
pub struct WatchHandle {
	// Dropping the sender closes the channel, which breaks the loop's shutdown select arm.
	shutdown: tokio::sync::oneshot::Sender<()>,
	// Resolves when the background loop has returned; `stop` awaits it.
	loop_done: tokio::task::JoinHandle<()>,
	status: tokio::sync::watch::Receiver<WatchStatus>,
	// Holds the FS watcher and the cache registration alive for the watch's lifetime. Either is
	// `None` when it could not be set up; the status says so (see [`WatchStatus`]).
	_watcher: Option<notify::RecommendedWatcher>,
	_sync_root: Option<SyncRootHandle>,
}

impl WatchHandle {
	/// Stop the watch and wait for the background loop to finish (up to one in-flight pass), so a
	/// caller can be sure nothing is still touching either side when this returns. Dropping the
	/// handle stops the loop too, but does not wait for it.
	///
	/// A pass PARKED on a [`pause`](super::SyncEngine::pause_pair) does not hold either of those
	/// open: stopping the watch gives that pass's transfers up (as
	/// [`cancel_paused_actions`](super::SyncEngine::cancel_paused_actions) would) so it can unwind
	/// and release the drive-write lock — including a pause that lands after this was called, which
	/// is the one nobody is left to reverse. The pair itself stays paused.
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

/// What a watch loop is doing right now — and, once it has ended, WHY, which is what a caller
/// cannot otherwise see coming.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum WatchState {
	/// Live: running passes, or waiting for the next trigger.
	#[default]
	Running,
	/// Live but idle: the pair is [`paused`](super::SyncEngine::pause_pair), so the loop runs no
	/// passes until it is resumed. It keeps its filesystem watcher and cache subscription.
	/// Published once the loop reaches its idle poll, so a pass SUSPENDED part-way through by that
	/// same pause still reads as [`Running`](Self::Running) until it ends.
	Paused,
	/// Terminal — the watch was stopped: its handle was dropped or [`stop`](WatchHandle::stop)ped.
	Stopped,
	/// Terminal — the pair was removed underneath the loop (see
	/// [`SyncEngine::remove_pair`](super::SyncEngine::remove_pair)), so there was nothing left to
	/// sync. Neither side was touched by the removal itself.
	PairRemoved,
}

/// The health of a running watch, published after every pass. A pass failing outright (as opposed
/// to a single action inside it, which surfaces as [`SyncEvent::ActionFailed`]) is otherwise
/// invisible: the loop logs it and retries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct WatchStatus {
	/// Passes that failed back to back; reset to zero by the next successful pass.
	pub consecutive_failures: u32,
	/// Why the last pass failed, or `None` while the watch is healthy.
	pub last_error: Option<String>,
	/// What the loop is doing — including the two ways it can END, which a caller has no other way
	/// to tell apart.
	pub state: WatchState,
	/// Why the watch no longer hears about every LOCAL change as it happens, or `None` while the
	/// filesystem watcher covers the whole tree. Set when the watcher could not start or could not
	/// watch the whole tree, or reported an error while running (a directory created past the OS
	/// watch limit goes unwatched). Local changes it misses are picked up by the safety-net pass.
	/// Holds the first error, and stays set for the life of the watch: the watcher does not recover
	/// the coverage it lost.
	pub local_events_degraded: Option<String>,
	/// Why the watch hears about no REMOTE changes as they happen, or `None` while its cache
	/// subscription is registered. Set when the cache refused the subscription; remote changes are
	/// then picked up by the safety-net pass. Stays set for the life of the watch.
	pub remote_events_degraded: Option<String>,
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
		// The pair AND the signal that ends the loop, in one step: everything below — the cache
		// registration especially — takes real time, and a removal that lands during it must not
		// find the loop unsubscribed (see [`SyncEngine::watchable_pair`]).
		let (record, removed) = self.watchable_pair(pair).await?;
		let local_root = PathBuf::from(&record.local_root);

		let dirty = Arc::new(Notify::new());
		// Created before either trigger source, so a source that fails to start can say so.
		let (status_tx, status_rx) = tokio::sync::watch::channel(WatchStatus::default());

		// Remote-change trigger: a cache sync-root subscription that pings on any committed batch.
		// Best-effort, like the engine's own subscription (see `SyncEngine::observe_pair`): without
		// it remote changes wait for the safety net, which is better than no watch at all.
		//
		// A doorbell only. WHAT changed remotely is recorded by the engine's own per-pair
		// subscription, which exists with or without a watch, so this one failing costs wake-ups,
		// never the remote changelist.
		let remote_dirty = Arc::clone(&dirty);
		let callback: SyncRootCallback = Box::new(move |_events| {
			remote_dirty.notify_one();
		});
		let sync_root = match self
			.client
			.clone()
			.add_sync_root(record.remote_root, callback)
			.await
		{
			Ok(handle) => Some(handle),
			Err(error) => {
				tracing::warn!(
					"sync pair {pair}: remote change notifications are unavailable ({error}); remote changes will sync on the safety net"
				);
				status_tx.send_modify(|status| {
					status.remote_events_degraded = Some(error.to_string());
				});
				None
			}
		};

		// Local-change trigger: a recursive filesystem watcher on the local root. The watcher
		// reports canonicalized paths, so the root it filters against must be canonical too.
		let watch_root = std::fs::canonicalize(&local_root).unwrap_or_else(|_| local_root.clone());
		// The pair's changelists. The handler below records what changed into them and a pass takes
		// them; the engine holds the same ones, so a one-shot `sync_once` reads the same set.
		let changes = self.pair_changes(pair).await;
		let handler = local_event_handler(
			pair,
			watch_root,
			Arc::clone(&dirty),
			status_tx.clone(),
			Arc::clone(&changes),
		);
		let watcher = start_local_watcher::<notify::RecommendedWatcher>(
			pair,
			&local_root,
			handler,
			&status_tx,
		);
		if status_tx.borrow().local_events_degraded.is_some() {
			// The watcher could not be created, or could not cover the whole tree: what it misses
			// is unknown, so no pass of this pair may narrow its local half down.
			changes.degrade(FullPassReason::LocalEventsDegraded);
		}
		// From here on something IS recording local changes, so a pass may narrow its local half
		// down to what the list holds. Before this — and after the loop below ends — an empty list
		// means "nobody was looking", which is not evidence of a quiet tree.
		changes.cover_local();
		// A platform whose watcher cannot report a dropped event (see `PairChanges::new`) has its
		// events permanently distrusted; say so on the status, or a caller sees a healthy watch
		// whose passes are all full and has nothing to point at.
		if let Some(reason) = changes.degraded() {
			status_tx.send_if_modified(|status| {
				if status.local_events_degraded.is_some() {
					return false;
				}
				status.local_events_degraded = Some(reason.to_string());
				true
			});
		}

		let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
		let stop = Stop {
			handle: shutdown_rx,
			removed,
			handle_gone: false,
		};
		let engine = Arc::clone(&self);
		// Subscribed before the loop starts, so a change made while it sets itself up is still
		// pending for its first wait rather than lost.
		let rules = self.user_ignore_changes();
		// In the caller's span: every pass the loop runs logs `pair N`, and pair ids are per engine
		// database, so two engines in one process are otherwise indistinguishable in the log.
		let loop_done = tokio::spawn(
			run_loop(
				engine, pair, config, dirty, rules, stop, observer, status_tx,
			)
			.in_current_span(),
		);

		Ok(WatchHandle {
			shutdown: shutdown_tx,
			loop_done,
			status: status_rx,
			_watcher: watcher,
			_sync_root: sync_root,
		})
	}
}

/// Everything that ends a watch: the handle going away (its shutdown sender dropped), and the pair
/// being removed underneath the loop. Both are checked wherever the loop waits, so neither is
/// delayed by a backoff or by a paused poll.
struct Stop {
	handle: tokio::sync::oneshot::Receiver<()>,
	removed: tokio::sync::watch::Receiver<bool>,
	/// Set once the handle's signal has been seen. A `oneshot::Receiver` PANICS if it is polled
	/// again after it completed, and the loop awaits [`ended`](Self::ended) more than once: the
	/// running pass races it, and whatever the loop waits on next races it again.
	handle_gone: bool,
}

impl Stop {
	/// Whether it was the PAIR going away that ended the loop, rather than the handle.
	fn pair_removed(&self) -> bool {
		*self.removed.borrow()
	}

	/// Resolves once the watch must end. Level-triggered on both signals, not edge-triggered: a
	/// stop that already happened ends every later wait too, however often this is called.
	async fn ended(&mut self) {
		if self.handle_gone || *self.removed.borrow() {
			return;
		}
		tokio::select! {
			biased;
			_ = &mut self.handle => self.handle_gone = true,
			// An error here means the engine dropped the signal — with the pair, or with itself.
			// Either way there is nothing left to sync.
			_ = self.removed.changed() => {}
		}
	}
}

/// The background loop: an initial pass, then debounced passes on `dirty`, plus a periodic pass,
/// until the watch is stopped or its pair removed (see [`Stop`]). A pass that fails outright backs
/// the loop off (see [`backoff`]) so a persistent error does not hot-loop at the debounce cadence;
/// that backoff is also the retry timer, since an outage over a quiescent tree produces no trigger
/// of its own.
#[allow(clippy::too_many_arguments)]
async fn run_loop(
	engine: Arc<SyncEngine>,
	pair: PairId,
	config: WatchConfig,
	dirty: Arc<Notify>,
	mut rules: tokio::sync::watch::Receiver<u64>,
	mut stop: Stop,
	mut observer: SyncObserver,
	status: tokio::sync::watch::Sender<WatchStatus>,
) {
	let mut failures: u32 = 0;
	// When a pass last read both sides whole — what the safety net measures from (see [`net_due`]).
	// The initial pass below is one: a pair's first pass of a process has no changelist to narrow
	// itself with.
	let mut last_whole_pass = Instant::now();

	// How long the net leaves between whole-tree passes. It starts at the configured floor and is
	// re-derived from every pass that reads both sides (see [`net_interval`]): a pair can only learn
	// what its tree costs by reading it once.
	let mut net_every = config.safety_net;

	let mut safety_net = tokio::time::interval(net_every);
	safety_net.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	safety_net.tick().await; // consume the immediate first tick (the initial pass below covers it)

	loop {
		if engine.is_paused(pair).await {
			// A closed channel just means nobody is watching the health any more.
			let _ = status.send_if_modified(|status| {
				let changed = status.state != WatchState::Paused;
				status.state = WatchState::Paused;
				changed
			});
			if !wait_while_paused(&mut stop, config.debounce).await {
				break;
			}
			continue;
		}

		// The backstop for anything both changelists silently missed: a pass that reads both sides
		// whole, at most `net_every` after the last one that did.
		if net_due(last_whole_pass, Instant::now(), net_every) {
			engine
				.force_full_pass(pair, FullPassReason::SafetyNet)
				.await;
		}
		let PassEnd {
			error,
			owed,
			read_whole,
			read_cost,
		} = tokio::select! {
			outcome = run_pass(&engine, pair, observer.as_mut()) => outcome,
			// Never resolves: it only makes sure a pass PARKED on a suspension gives up when the
			// watch is stopped, so the pass above can finish and this loop can end. Without it a
			// stop (or a dropped handle) waits out the suspension's escalation window — for ever if
			// it has none — while the pass sits on the drive-write lock.
			() = unpark_on_stop(&engine, pair, &mut stop, config.debounce) => unreachable!(),
		};
		if read_whole {
			last_whole_pass = Instant::now();
			// What that READ cost is the only measurement anyone has of what this pair's tree costs,
			// so the net re-scales from it — the pass's own wall time would fold in its transfers,
			// its wait for the drive-write lock and any stretch a suspension parked it in, and one
			// big upload on a ten-item pair would push the backstop out to the ceiling. A changed
			// interval needs a new timer — a tokio `Interval` has a fixed period — and recreating it
			// here starts the wait from now, which is the same phase reset a successful pass does
			// just below.
			let scaled = net_interval(config.safety_net, read_cost);
			if scaled != net_every {
				tracing::debug!(
					"sync pair {pair}: reading both sides took {read_cost:?}, so the safety net moves from {net_every:?} to {scaled:?}"
				);
				net_every = scaled;
				safety_net = tokio::time::interval_at(Instant::now() + net_every, net_every);
				safety_net.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
			}
		}
		if error.is_none() {
			failures = 0;
			// The net measures time since the last SUCCESSFUL pass, not since the last tick: a pass
			// that just succeeded has already done everything the net would do, so a tick that came
			// due during it buys nothing but a no-op pass right after. A FAILED pass deliberately
			// leaves the interval alone — its own retry timer is the backoff, and a run of failures
			// must not push the net out indefinitely. So does a pass a pause cut short: it did not
			// do what the net would have done either.
			if !owed {
				safety_net.reset();
			}
		} else {
			failures = failures.saturating_add(1);
		}
		if owed {
			// What that pass did not get to is still owed, and the trigger that started it went
			// with it. Re-arm the signal, so the wait below ends at the debounce and hands over to
			// the paused branch above — which runs the remainder the moment the pair resumes.
			// Without it the loop sits in that wait until the safety net fires, minutes after the
			// resume, with nothing else to wake it (the work it skipped changed neither side).
			dirty.notify_one();
		}
		publish_pass(&status, failures, error);
		let delay = backoff(failures);
		if let Some(delay) = delay {
			tracing::warn!(
				"sync pair {pair}: {failures} consecutive failure(s); waiting {delay:?} before the next pass"
			);
		}

		if !wait_for_next_pass(
			&mut stop,
			&dirty,
			&mut safety_net,
			&mut rules,
			config.debounce,
			delay,
		)
		.await
		{
			break;
		}
	}

	// The terminal state, and which of the two it is: a caller watching the health is otherwise
	// left waiting on a report that will never come, unable to tell a stopped loop from a quiet one
	// — or a watch it stopped itself from a pair that was taken out from under it.
	// The watcher goes away with this loop, so nothing records this pair's local changes any more:
	// until another watch starts, every pass reads the local side whole. Looked up without
	// creating one — the pair may be exactly what ended the loop.
	if let Some(changes) = engine.existing_pair_changes(pair).await {
		changes.uncover_local();
	}
	let ended = if stop.pair_removed() {
		WatchState::PairRemoved
	} else {
		WatchState::Stopped
	};
	status.send_modify(|status| status.state = ended);
}

/// Wait for the watch to be stopped, then keep cutting loose a pass PARKED on the pair's
/// suspension — and never resolve, so this only ever ends the pass it is racing, never the wait for
/// it.
///
/// A suspended pass comes back when the pair is resumed or its actions cancelled; a stopped watch
/// has nobody left to ask for either, so without this the loop (and [`WatchHandle::stop`], and the
/// drive-write lock the pass is holding) would wait out the suspension's escalation window, or for
/// ever if the pause was made with no window at all. Repeated on a `poll` timer rather than done
/// once, because a pause can land at any moment while the pass runs — including AFTER the stop,
/// which is exactly when there is nobody left to un-park it. Polled for the same reason
/// [`wait_while_paused`] is: the alternative is per-pair wakeup plumbing through the engine.
async fn unpark_on_stop(engine: &SyncEngine, pair: PairId, stop: &mut Stop, poll: Duration) {
	stop.ended().await;
	loop {
		engine.cancel_suspended_pass(pair).await;
		tokio::time::sleep(poll).await;
	}
}

/// Wait out one poll interval while the pair is paused. Returns `false` if the watch was stopped.
///
/// Deliberately does NOT touch `dirty`: a change that happens while the pair is paused leaves its
/// signal pending, so the first pass after the resume is the one that catches up on the lot. The
/// pause itself is polled rather than signalled — the alternative is per-pair wakeup plumbing
/// through the engine to save a timer that fires at the debounce cadence.
async fn wait_while_paused(stop: &mut Stop, poll: Duration) -> bool {
	tokio::select! {
		biased;
		_ = stop.ended() => false,
		_ = tokio::time::sleep(poll) => true,
	}
}

/// Wait for the next pass to be due. Returns `false` if the watch was stopped instead.
///
/// After a failed pass that `backoff` delay *is* the whole wait: it is the retry timer, so a
/// persistent failure retries on the backoff schedule rather than falling through to whatever the
/// safety net or a change event happens to offer next. While healthy the wait ends on the periodic
/// safety-net tick, on new device-wide user ignore patterns (`rules`), or on a change event once
/// the burst behind it has gone quiet for `debounce`. New patterns are not debounced: they arrive
/// one deliberate call at a time, and a burst of calls is already one wake-up (the signal carries
/// the latest version, not each one).
///
/// A burst that never goes quiet is coalesced for at most [`MAX_DEBOUNCE_BURST`] debounces, and a
/// safety-net tick that comes due while it is being coalesced ends the wait too: neither the quiet
/// window nor a continuous writer may postpone a pass indefinitely.
async fn wait_for_next_pass(
	stop: &mut Stop,
	dirty: &Notify,
	safety_net: &mut tokio::time::Interval,
	rules: &mut tokio::sync::watch::Receiver<u64>,
	debounce: Duration,
	backoff: Option<Duration>,
) -> bool {
	if let Some(delay) = backoff {
		return tokio::select! {
			biased;
			_ = stop.ended() => false,
			_ = tokio::time::sleep(delay) => true,
		};
	}

	tokio::select! {
		biased;
		_ = stop.ended() => return false,
		_ = safety_net.tick() => {}
		_ = user_ignore_changed(rules) => {}
		_ = dirty.notified() => {
			// Coalesce the burst: wait for `debounce` of quiet (each new event restarts it) — but
			// for no longer than `MAX_DEBOUNCE_BURST` debounces all told, and no longer than the
			// periodic pass was going to wait anyway. A tree something writes into continuously
			// never goes quiet, and with the quiet window as the only way out of this loop such a
			// tree never gets a pass at all.
			let cap = tokio::time::sleep(debounce * MAX_DEBOUNCE_BURST);
			tokio::pin!(cap);
			loop {
				tokio::select! {
					biased;
					_ = stop.ended() => return false,
					_ = &mut cap => break,
					_ = safety_net.tick() => break,
					_ = dirty.notified() => continue,
					_ = tokio::time::sleep(debounce) => break,
				}
			}
		}
	}
	true
}

/// Resolves once the device-wide user ignore patterns have changed since `rules` last saw them —
/// never, once the engine that owns the signal is gone. A closed channel means this loop is on its
/// way out too (it holds the engine), so parking is what lets its stop signal end the wait, where a
/// ready arm would spin.
async fn user_ignore_changed(rules: &mut tokio::sync::watch::Receiver<u64>) {
	if rules.changed().await.is_err() {
		std::future::pending().await
	}
}

/// How long to wait before the next pass after `failures` consecutive failed ones: nothing while
/// healthy, then [`BASE_BACKOFF`] doubling per failure, capped at [`MAX_BACKOFF`].
fn backoff(failures: u32) -> Option<Duration> {
	let doublings = failures.checked_sub(1)?.min(31);
	Some((BASE_BACKOFF * 2u32.pow(doublings)).min(MAX_BACKOFF))
}

/// Run one pass, logging (not propagating) any failure — the loop is best-effort and the next
/// trigger or the periodic tick retries. `observer` receives this pass's [`SyncEvent`]s.
///
/// Returns the pass's own error, if any (per-action errors inside a completed pass do not count on
/// their own; a drive lock it could not take does, so does a pass that found a side full — see
/// [`halted`](SyncReport::halted) — and so does one whose record of work already done could not be
/// written — `store_failed` — so the loop backs off rather than running into the same full disk or
/// wedged store at the debounce cadence), and whether it left work owed (see [`pass_outcome`]).
async fn run_pass(
	engine: &SyncEngine,
	pair: PairId,
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> PassEnd {
	match engine
		.sync_pass(pair, observer, super::engine::WhenIdle::Skip)
		.await
	{
		Ok(report) => {
			if !report.errors.is_empty() {
				tracing::warn!("sync pair {pair}: {} action error(s)", report.errors.len());
			}
			pass_outcome(report)
		}
		Err(e) => {
			tracing::warn!("sync pair {pair} failed: {e}");
			// A pass that failed outright read nothing it could finish with, so the net is still
			// owed a whole-tree read; the engine has already forced the next pass full.
			PassEnd {
				error: Some(e.to_string()),
				owed: false,
				read_whole: false,
				read_cost: Duration::ZERO,
			}
		}
	}
}

/// What one finished pass means for the loop.
struct PassEnd {
	/// The pass's own failure, if any.
	error: Option<String>,
	/// Whether a pause cut it short, leaving the loop owing a pass (see [`owes_a_pass`]).
	owed: bool,
	/// Whether it read both sides whole (see [`read_both_sides_whole`]).
	read_whole: bool,
	/// What READING both sides cost it (see [`SyncReport::read_cost`]) — what the net scales by.
	/// Zero on a pass that never finished reading, which is also one that never set `read_whole`.
	read_cost: Duration,
}

/// Whether this pass read both sides WHOLE — what the safety net measures from.
///
/// Which is exactly the pass that recorded WHY it had to (see
/// [`full_pass`](SyncReport::full_pass)): a pass with no reason narrowed its read to the paths its
/// changelists named, an idle wake ran no pass at all, and a pause can still cut either short. All
/// three leave the net owing the whole-tree read it exists to be the backstop for.
fn read_both_sides_whole(report: &SyncReport) -> bool {
	report.full_pass.is_some()
}

/// Whether the safety net is due: no pass has read both sides whole for a whole `every`.
///
/// Measured from the last such pass rather than taken from the interval's own tick, so a tree busy
/// enough to keep the loop woken by its own changes cannot starve the backstop, and so the net
/// counts what it exists for — a whole-tree read — rather than merely a pass having run.
fn net_due(last_whole_pass: Instant, now: Instant, every: Duration) -> bool {
	now.saturating_duration_since(last_whole_pass) >= every
}

/// How long the safety net waits between whole-tree passes, given the configured `floor` and what
/// the pair's last whole-tree READ `cost` (see [`SyncReport::read_cost`]): [`SAFETY_NET_DUTY`]
/// times that cost, never below the floor and never above [`MAX_SAFETY_NET`].
///
/// The net is the only pass an idle pair runs, so its interval is that pair's whole idle cost. A
/// fixed one cannot serve both ends: 300 s is free for a pair whose read is milliseconds and hours
/// of CPU a day for one whose read is tens of seconds. Scaling with the measured read cost makes
/// the idle duty cycle roughly constant instead — the floor keeps a small pair exactly as
/// responsive as it is today, and the ceiling keeps a huge one's backstop from drifting out to a
/// working day. It is the READ and not the pass: a pair with ten items whose one pass uploaded a
/// 5 GB file, or sat parked on a suspension for an hour, has learnt nothing about how expensive its
/// tree is to read, and must come back to the floor rather than to the ceiling.
///
/// A `floor` above the ceiling wins: a caller asking for a slower backstop than 6 h gets it, and
/// clamping to a max below the min would panic.
fn net_interval(floor: Duration, cost: Duration) -> Duration {
	cost.saturating_mul(SAFETY_NET_DUTY)
		.clamp(floor, floor.max(MAX_SAFETY_NET))
}

/// What a pass that returned a report means for the loop: its own failure, if any, and whether it
/// left work owed (see [`owes_a_pass`]).
///
/// A pass that could not take the drive lock returns a report but applied nothing, so it is a
/// failed pass. Its backoff is the retry timer, which is why it owes no re-armed trigger on top.
/// A pass [`halted`](SyncReport::halted) for want of space is a failed pass too, so it backs off.
/// So is one whose baseline or journal write did not land (`store_failed`): the act that write was
/// recording stands, and the next pass would run straight into the same wedged store.
fn pass_outcome(report: SyncReport) -> PassEnd {
	let read_whole = read_both_sides_whole(&report);
	let read_cost = report.read_cost;
	if !report.lock_failed {
		let error = report
			.halted
			.map(|reason| reason.held_line())
			.or_else(|| report.store_failed.then(|| STORE_FAILURE.to_string()));
		return PassEnd {
			error,
			owed: owes_a_pass(&report),
			read_whole,
			read_cost,
		};
	}
	let error = report
		.errors
		.into_iter()
		.find(|error| error.starts_with(LOCK_FAILURE))
		.unwrap_or_else(|| LOCK_FAILURE.to_string());
	// A pass that could not take the drive lock still READ both sides: the lock is only asked for
	// once there is a plan to apply.
	PassEnd {
		error: Some(error),
		owed: false,
		read_whole,
		read_cost,
	}
}

/// Whether a pause cut this pass short, leaving the loop owing a pass of its own.
///
/// Two shapes, one debt. A pass cancelled part-way through its plan reports what it did not do as
/// [`interrupted`](SyncReport::interrupted). A pass cancelled BEFORE it had a plan to count — a
/// pause landing while it read the two sides, or in the instant between the loop's own pause check
/// and the pass's — has nothing to count and reports itself
/// [`paused`](SyncReport::paused). Either way the pass did none of what the safety net would have
/// done and the trigger that started it is spent, so the loop must re-arm that trigger and leave
/// the net measuring from the last pass that actually ran.
fn owes_a_pass(report: &SyncReport) -> bool {
	report.interrupted > 0 || report.paused
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

/// Publish a finished pass's health. Only the pass's own fields change: a degraded trigger source
/// recorded earlier is still degraded after the pass.
fn publish_pass(
	status: &tokio::sync::watch::Sender<WatchStatus>,
	failures: u32,
	error: Option<String>,
) {
	status.send_modify(|status| {
		status.consecutive_failures = failures;
		status.last_error = error;
		status.state = WatchState::Running;
	});
}

/// Start the filesystem watcher on `root`. Never fails the watch: a watcher that cannot be created
/// is recorded (see [`degrade_local`]) and `None` leaves local changes to the safety net. One that
/// starts but cannot watch the whole tree — on Linux, a tree with more directories than
/// `fs.inotify.max_user_watches` allows — is recorded too, but kept: it still reports changes in
/// the part of the tree it did cover.
///
/// Generic over the watcher so a test can make it fail; the watch always uses
/// [`notify::RecommendedWatcher`].
fn start_local_watcher<W: Watcher>(
	pair: PairId,
	root: &Path,
	handler: impl EventHandler,
	status: &tokio::sync::watch::Sender<WatchStatus>,
) -> Option<W> {
	let mut watcher = W::new(handler, notify::Config::default())
		.inspect_err(|error| degrade_local(pair, status, error))
		.ok()?;
	if let Err(error) = watcher.watch(root, RecursiveMode::Recursive) {
		degrade_local(pair, status, &error);
	}
	Some(watcher)
}

/// The filesystem watcher's event handler: it records WHAT changed on the pair's local changelist
/// (see [`PairChanges::note_local_event`]) and then wakes the loop, unless the event is the
/// engine's own staging write (see [`triggers_pass`]).
///
/// An error wakes the loop too, and is recorded twice: on the watch's status for a caller to see,
/// and on the changelist, because it means the watcher may have missed something it cannot name —
/// a directory created past the OS watch limit goes unwatched, a failed read loses the events it
/// held — so no later pass may narrow its local half down.
///
/// `notify` delivers on its own thread, so everything here is a lock, a compare and a push.
fn local_event_handler(
	pair: PairId,
	root: PathBuf,
	dirty: Arc<Notify>,
	status: tokio::sync::watch::Sender<WatchStatus>,
	changes: Arc<PairChanges>,
) -> impl FnMut(notify::Result<notify::Event>) + Send + 'static {
	move |res| match res {
		Ok(event) => {
			changes.note_local_event(&root, &event, |path| is_engine_staging(&root, path));
			if triggers_pass(&root, &event) {
				dirty.notify_one();
			}
		}
		Err(error) => {
			changes.degrade(FullPassReason::LocalEventsDegraded);
			degrade_local(pair, &status, &error);
			dirty.notify_one();
		}
	}
}

/// Record on the status that the watch no longer hears about every local change. Keeps the FIRST
/// error, the one that cost the coverage (a later one is usually the same limit hit again), and
/// warns for that one only, so a watcher reporting a stream of errors cannot flood the log.
fn degrade_local(
	pair: PairId,
	status: &tokio::sync::watch::Sender<WatchStatus>,
	error: &notify::Error,
) {
	let first = status.send_if_modified(|status| {
		if status.local_events_degraded.is_some() {
			return false;
		}
		status.local_events_degraded = Some(error.to_string());
		true
	});
	if first {
		tracing::warn!(
			"sync pair {pair}: the filesystem watcher is not seeing every local change ({error}); what it misses will sync on the safety net"
		);
	} else {
		tracing::debug!("sync pair {pair}: filesystem watcher error: {error}");
	}
}

#[cfg(test)]
mod tests {
	use std::{
		path::{Path, PathBuf},
		sync::Arc,
		time::Duration,
	};

	use notify::{
		EventHandler, EventKind, RecursiveMode, Watcher, WatcherKind,
		event::{CreateKind, ModifyKind, RenameMode},
	};
	use tokio::sync::Notify;

	use super::{
		BASE_BACKOFF, DEBOUNCE, FullPassReason, MAX_BACKOFF, MAX_DEBOUNCE_BURST, MAX_SAFETY_NET,
		PairChanges, SAFETY_NET, SAFETY_NET_DUTY, Stop, SyncReport, WatchConfig, WatchState,
		WatchStatus, backoff, local_event_handler, net_due, net_interval, owes_a_pass,
		pass_outcome, publish_pass, read_both_sides_whole, start_local_watcher, triggers_pass,
		wait_for_next_pass, wait_while_paused,
	};
	use crate::{
		Error, ErrorKind,
		sync_engine::apply::{LOCK_FAILURE, STORE_FAILURE, note_lock_failure},
	};

	fn watch_limit() -> notify::Error {
		notify::Error::new(notify::ErrorKind::MaxFilesWatch)
	}

	/// A watcher the OS refuses to create at all.
	struct Unstartable;

	impl Watcher for Unstartable {
		fn new<F: EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
			Err(notify::Error::generic("no watcher backend"))
		}
		fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
			unreachable!("never created")
		}
		fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
			unreachable!("never created")
		}
		fn kind() -> WatcherKind {
			WatcherKind::NullWatcher
		}
	}

	/// A watcher that starts, then runs out of OS watches part-way through the tree.
	struct OverLimit;

	impl Watcher for OverLimit {
		fn new<F: EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
			Ok(Self)
		}
		fn watch(&mut self, _: &Path, _: RecursiveMode) -> notify::Result<()> {
			Err(watch_limit())
		}
		fn unwatch(&mut self, _: &Path) -> notify::Result<()> {
			Ok(())
		}
		fn kind() -> WatcherKind {
			WatcherKind::NullWatcher
		}
	}

	/// A watcher that cannot be set up must not take the watch down with it: the watch starts on
	/// the safety net alone, and says so on its status instead of failing.
	#[test]
	fn a_watcher_that_cannot_start_leaves_the_watch_degraded_not_failed() {
		let (status, health) = tokio::sync::watch::channel(WatchStatus::default());
		assert!(
			start_local_watcher::<Unstartable>(1, Path::new("/sync/root"), |_| {}, &status)
				.is_none()
		);
		let degraded = health.borrow().local_events_degraded.clone();
		assert_eq!(degraded.as_deref(), Some("no watcher backend"));
		assert_eq!(health.borrow().remote_events_degraded, None);
	}

	/// A watcher that covered only part of the tree is kept, since it still reports that part, and
	/// the watch is marked degraded.
	#[test]
	fn a_watcher_over_the_watch_limit_is_kept_and_reported() {
		let (status, health) = tokio::sync::watch::channel(WatchStatus::default());
		assert!(
			start_local_watcher::<OverLimit>(1, Path::new("/sync/root"), |_| {}, &status).is_some()
		);
		assert_eq!(
			health.borrow().local_events_degraded.as_deref(),
			Some("OS file watch limit reached.")
		);
	}

	/// A watcher error at runtime wakes the loop (whatever the watcher lost is caught one debounce
	/// later, not at the safety net), marks the watch degraded, and keeps the first error.
	#[tokio::test(start_paused = true)]
	async fn a_watcher_error_wakes_the_loop_and_marks_the_watch_degraded() {
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Arc::new(Notify::new());
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();
		let (status, health) = tokio::sync::watch::channel(WatchStatus::default());
		let changes = Arc::new(PairChanges::new());
		let mut handler = local_event_handler(
			1,
			PathBuf::from("/sync/root"),
			Arc::clone(&dirty),
			status.clone(),
			Arc::clone(&changes),
		);

		handler(Err(watch_limit()));
		assert_eq!(
			changes.take().full_pass_reason(10),
			Some(FullPassReason::LocalEventsDegraded),
			"a watcher error costs coverage nothing can narrow down again"
		);
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE,
			"a watcher error must wake the loop, not wait for the safety net"
		);
		assert_eq!(
			health.borrow().local_events_degraded.as_deref(),
			Some("OS file watch limit reached.")
		);

		handler(Err(notify::Error::generic("read failed")));
		assert_eq!(
			health.borrow().local_events_degraded.as_deref(),
			Some("OS file watch limit reached."),
			"the first error is the one that cost the coverage"
		);

		// A pass finishing afterwards does not clear it: the lost coverage does not come back.
		publish_pass(&status, 1, Some("boom".to_string()));
		let after = health.borrow().clone();
		assert_eq!(
			after,
			WatchStatus {
				consecutive_failures: 1,
				last_error: Some("boom".to_string()),
				state: WatchState::Running,
				local_events_degraded: Some("OS file watch limit reached.".to_string()),
				remote_events_degraded: None,
			}
		);
	}

	/// A safety-net interval as `run_loop` sets one up: the immediate first tick consumed, so the
	/// next one is a full [`SAFETY_NET`] away.
	async fn safety_net() -> tokio::time::Interval {
		let mut interval = tokio::time::interval(SAFETY_NET);
		interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
		interval.tick().await;
		interval
	}

	/// The device-wide user ignore signal as the loop holds one, plus the sender the engine's
	/// `set_user_ignore` fires.
	fn user_ignore() -> (
		tokio::sync::watch::Sender<u64>,
		tokio::sync::watch::Receiver<u64>,
	) {
		tokio::sync::watch::channel(0)
	}

	/// A [`Stop`] as the loop holds one, plus the two senders that can trip it: the handle's
	/// shutdown, and the engine's per-pair removal signal.
	fn stop() -> (
		tokio::sync::oneshot::Sender<()>,
		tokio::sync::watch::Sender<bool>,
		Stop,
	) {
		let (shutdown_tx, handle) = tokio::sync::oneshot::channel();
		let (removed_tx, removed) = tokio::sync::watch::channel(false);
		(
			shutdown_tx,
			removed_tx,
			Stop {
				handle,
				removed,
				handle_gone: false,
			},
		)
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
	/// and the backoff schedule is decoration. New device-wide patterns are no exception: a loop
	/// backing off is applying nothing for them to stop, and letting them cut the timer short would
	/// turn a setter call into an on-demand retry against whatever is failing.
	#[tokio::test(start_paused = true)]
	async fn a_failed_pass_retries_on_the_backoff_schedule() {
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (rules_tx, mut rules) = user_ignore();

		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
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

		rules_tx.send_modify(|version| *version += 1);
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				Some(BASE_BACKOFF)
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			BASE_BACKOFF,
			"new patterns must not turn the backoff into an on-demand retry"
		);
	}

	/// A healthy wait is unchanged: a change event, coalesced over [`DEBOUNCE`], or the safety net.
	#[tokio::test(start_paused = true)]
	async fn a_healthy_wait_ends_on_a_debounced_change_or_the_safety_net() {
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(start.elapsed(), DEBOUNCE, "a change waits out the debounce");

		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			SAFETY_NET - DEBOUNCE,
			"with nothing happening, the safety net is what ends the wait"
		);
	}

	/// A burst that never goes quiet must not starve the pass. Something writing into the tree
	/// continuously restarts the quiet window with every event, so the wait is capped: the pass
	/// runs after [`MAX_DEBOUNCE_BURST`] debounces whatever the stream does, and a safety-net tick
	/// coming due during a burst ends it too rather than being kept waiting behind it.
	#[tokio::test(start_paused = true)]
	async fn a_burst_that_never_goes_quiet_still_runs_a_pass() {
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Arc::new(Notify::new());
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		// An event every half debounce, for ever: the quiet window never elapses.
		let stream = tokio::spawn({
			let dirty = Arc::clone(&dirty);
			async move {
				loop {
					tokio::time::sleep(DEBOUNCE / 2).await;
					dirty.notify_one();
				}
			}
		});

		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(
			tokio::time::timeout(
				DEBOUNCE * (MAX_DEBOUNCE_BURST + 4),
				wait_for_next_pass(
					&mut stop,
					&dirty,
					&mut safety_net,
					&mut rules,
					DEBOUNCE,
					None
				)
			)
			.await
			.expect("a burst that never goes quiet must not hold the pass off for ever")
		);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE * MAX_DEBOUNCE_BURST,
			"the burst is coalesced for at most that long, and then the pass runs"
		);

		// And the net is an arm of that wait: a tick due mid-burst ends it at the tick.
		let mut net = tokio::time::interval(DEBOUNCE / 4);
		net.tick().await;
		let start = tokio::time::Instant::now();
		dirty.notify_one();
		assert!(
			tokio::time::timeout(
				DEBOUNCE * MAX_DEBOUNCE_BURST,
				wait_for_next_pass(&mut stop, &dirty, &mut net, &mut rules, DEBOUNCE, None)
			)
			.await
			.expect("a safety-net tick must not wait out the burst")
		);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE / 4,
			"the periodic pass is due, so it runs — the burst does not postpone it"
		);

		stream.abort();
	}

	/// Stopping the watch is never delayed by a backoff.
	#[tokio::test(start_paused = true)]
	async fn a_stop_interrupts_the_backoff() {
		let (shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		drop(shutdown_tx);
		let start = tokio::time::Instant::now();
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				Some(MAX_BACKOFF)
			)
			.await,
			"a stopped watch must not wait out the backoff"
		);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// A pair removed underneath the loop ends every wait at once, whichever one the loop is in —
	/// the failure backoff included, which is exactly where a watch on a removed pair ends up
	/// (every pass fails with "unknown sync pair") and where it used to sit forever.
	#[tokio::test(start_paused = true)]
	async fn a_removed_pair_ends_the_wait_wherever_the_loop_is() {
		let (_shutdown_tx, removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		removed_tx.send(true).unwrap();
		let start = tokio::time::Instant::now();
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				Some(MAX_BACKOFF)
			)
			.await,
			"a removed pair must not wait out the backoff"
		);
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await,
			"a removed pair must not wait for the next trigger either"
		);
		assert!(
			!wait_while_paused(&mut stop, Duration::from_secs(600)).await,
			"a removed pair must not keep polling its pause"
		);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// The engine dropping the signal (with the pair, or with itself) stops the loop just as a
	/// removal does: there is nothing left to sync either way.
	#[tokio::test(start_paused = true)]
	async fn a_dropped_removal_signal_stops_the_loop_too() {
		let (_shutdown_tx, removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		drop(removed_tx);
		let start = tokio::time::Instant::now();
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// A paused pair polls, and never consumes the change signal — nor the device-wide pattern one:
	/// what a paused loop walked past is still there for the first wait after the resume, so the
	/// backlog syncs then rather than waiting out a whole safety-net interval.
	#[tokio::test(start_paused = true)]
	async fn a_paused_loop_leaves_the_change_signal_pending() {
		let (shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (rules_tx, mut rules) = user_ignore();
		let poll = Duration::from_millis(200);

		// A change lands while the pair is paused; two paused polls walk past it.
		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(wait_while_paused(&mut stop, poll).await);
		assert!(wait_while_paused(&mut stop, poll).await);
		assert_eq!(
			start.elapsed(),
			poll * 2,
			"a paused poll waits its interval"
		);

		// Resumed: the pending notification is what ends the very next wait, one debounce later —
		// not the safety net, which is far away.
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE,
			"the change made during the pause was swallowed"
		);

		// New user patterns behave the same: the paused loop walks past them, and they end the first
		// wait after the resume — at once, since they are not debounced.
		rules_tx.send_modify(|version| *version += 1);
		let start = tokio::time::Instant::now();
		assert!(wait_while_paused(&mut stop, poll).await);
		assert_eq!(
			start.elapsed(),
			poll,
			"a paused loop must not run a pass for new patterns"
		);
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			Duration::ZERO,
			"the patterns set during the pause were swallowed"
		);

		// And stopping the watch is never delayed by a paused poll.
		drop(shutdown_tx);
		let start = tokio::time::Instant::now();
		assert!(!wait_while_paused(&mut stop, Duration::from_secs(600)).await);
		assert_eq!(start.elapsed(), Duration::ZERO);
	}

	/// A pause that lands on a pass BEFORE it has a plan to count leaves the same debt as one that
	/// cut a plan in half: the pass applied nothing, and the trigger that started it is spent. The
	/// loop must re-arm that trigger, or the first pass after the resume is whichever safety-net
	/// tick happens to come next — a pair the user just un-paused sitting idle for minutes.
	#[tokio::test(start_paused = true)]
	async fn a_pause_that_beat_the_plan_still_owes_a_pass() {
		// The loop's tail for such a pass: re-arm the trigger, leave the safety net alone.
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();
		let report = SyncReport {
			paused: true,
			..SyncReport::default()
		};
		if owes_a_pass(&report) {
			dirty.notify_one();
		}

		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			DEBOUNCE,
			"the loop waited out the safety net instead of owning the pass the pause cut short"
		);

		// A pass that ran to the end is the only one that owes nothing.
		assert!(!owes_a_pass(&SyncReport::default()));
		assert!(owes_a_pass(&SyncReport {
			interrupted: 1,
			..SyncReport::default()
		}));
	}

	/// A pass that could not take the drive lock still returns a report, but it applied nothing.
	/// The loop must count it as a failed pass — so the health shows it and the next pass waits out
	/// the backoff — instead of resetting its failure count as though the pass had run. The backoff
	/// is the retry timer, so no re-armed trigger is owed on top of it.
	#[test]
	fn a_pass_that_could_not_take_the_drive_lock_counts_as_a_failure() {
		let mut report = SyncReport {
			full_pass: Some(FullPassReason::SafetyNet),
			..SyncReport::default()
		};
		let cause = Error::custom(ErrorKind::RetryFailed, "held by another device");
		note_lock_failure(&mut report, 3, &cause, &mut |_| {});
		let end = pass_outcome(report);
		assert_eq!(
			end.error,
			Some(format!("{LOCK_FAILURE}: {cause}")),
			"a pass that could not take the drive lock must publish its cause"
		);
		assert!(
			!end.owed,
			"a failed pass is retried on the backoff, not re-armed at the debounce"
		);
		assert!(
			end.read_whole,
			"the lock is only asked for once the pass has read both sides and planned"
		);
	}

	/// A pass that applied its actions but could not RECORD one of them ran the act without its
	/// record. The loop must count it as a failed pass — the same way it counts a lock it could not
	/// take — so the next one waits out the backoff instead of losing the next record to the same
	/// wedged store.
	#[test]
	fn a_pass_whose_record_did_not_land_counts_as_a_failure() {
		let report = SyncReport {
			uploaded: 1,
			store_failed: true,
			..SyncReport::default()
		};
		let end = pass_outcome(report);
		assert_eq!(
			end.error.as_deref(),
			Some(STORE_FAILURE),
			"a pass that lost a record must publish a cause, not read as healthy"
		);
		assert!(
			!end.owed,
			"a failed pass is retried on the backoff, not re-armed at the debounce"
		);
	}

	/// The safety net measures from the last pass that READ both sides whole, not from the last
	/// pass to run: a pass a pause cut short before it read anything leaves the net owed, so a
	/// pair being paused and resumed repeatedly cannot starve the backstop.
	#[tokio::test(start_paused = true)]
	async fn the_safety_net_measures_from_the_last_whole_read() {
		let start = tokio::time::Instant::now();
		assert!(!net_due(start, start, SAFETY_NET));
		assert!(!net_due(start, start + SAFETY_NET - DEBOUNCE, SAFETY_NET));
		assert!(net_due(start, start + SAFETY_NET, SAFETY_NET));
		assert!(net_due(start, start + SAFETY_NET * 3, SAFETY_NET));

		// A pass that READ BOTH SIDES counts, whether or not its actions all landed — and it is
		// the one that recorded why it had to read them.
		assert!(read_both_sides_whole(&SyncReport {
			full_pass: Some(FullPassReason::SafetyNet),
			..SyncReport::default()
		}));
		assert!(read_both_sides_whole(&SyncReport {
			full_pass: Some(FullPassReason::FirstPass),
			interrupted: 2,
			..SyncReport::default()
		}));
		assert!(
			!read_both_sides_whole(&SyncReport::default()),
			"a change-scoped pass read one dirty set, not the tree the net is the backstop for"
		);
		assert!(
			!read_both_sides_whole(&SyncReport {
				paused: true,
				..SyncReport::default()
			}),
			"a pause that beat the read leaves the net owed a whole-tree pass"
		);
	}

	/// The net scales by what READING both sides cost, never by how long the pass took. A pass that
	/// read a ten-item tree in milliseconds and then spent twenty minutes uploading — or queueing
	/// for the drive-write lock, or parked on a suspension — has learnt nothing about the tree, so
	/// the backstop stays at the floor instead of being pushed out towards the ceiling.
	#[test]
	fn the_net_scales_by_the_read_not_by_the_whole_pass() {
		let after_a_long_upload = pass_outcome(SyncReport {
			full_pass: Some(FullPassReason::SafetyNet),
			read_cost: Duration::from_millis(10),
			uploaded: 1,
			..SyncReport::default()
		});
		assert!(after_a_long_upload.read_whole);
		assert_eq!(
			net_interval(SAFETY_NET, after_a_long_upload.read_cost),
			SAFETY_NET,
			"a 10 ms read keeps the floor however long the pass itself ran"
		);

		// A read that genuinely costs seconds is what scales the net out.
		let big_tree = pass_outcome(SyncReport {
			full_pass: Some(FullPassReason::SafetyNet),
			read_cost: Duration::from_secs(30),
			..SyncReport::default()
		});
		assert_eq!(
			net_interval(SAFETY_NET, big_tree.read_cost),
			Duration::from_secs(30) * SAFETY_NET_DUTY
		);

		// A pass that failed outright measured no read at all, so it cannot rescale anything.
		assert_eq!(
			net_interval(
				SAFETY_NET,
				pass_outcome(SyncReport {
					paused: true,
					..SyncReport::default()
				})
				.read_cost
			),
			SAFETY_NET
		);
	}

	/// The handler records what changed for the next pass to narrow itself with, keyed as the scan
	/// keys it — and the engine's own staging writes dirty nothing.
	#[test]
	fn the_handler_records_what_changed_for_the_next_pass() {
		let (status, _health) = tokio::sync::watch::channel(WatchStatus::default());
		let dirty = Arc::new(Notify::new());
		let changes = Arc::new(PairChanges::new());
		// What `watch_with` does once the watcher is running: only then is the list evidence.
		changes.cover_local();
		let root = PathBuf::from("/sync/root");
		let mut handler = local_event_handler(
			1,
			root.clone(),
			Arc::clone(&dirty),
			status,
			Arc::clone(&changes),
		);

		handler(Ok(event(
			EventKind::Create(CreateKind::File),
			&["/sync/root/sub/a.txt"],
		)));
		handler(Ok(event(
			EventKind::Create(CreateKind::File),
			&["/sync/root/sub/dee76e0e-0000-0000-0000-000000000000.filendl"],
		)));
		handler(Ok(event(
			EventKind::Create(CreateKind::File),
			&["/sync/root/.filen-sync-trash/gone.txt"],
		)));

		let scope = changes.take();
		assert_eq!(
			scope.sizes(),
			(1, 0),
			"only the real change is worth re-observing"
		);
		assert_eq!(
			scope.full_pass_reason(10),
			None,
			"one named path does not force a whole-tree read"
		);
	}

	/// A stop that already fired ends every later wait, and can be awaited any number of times: the
	/// loop races it against the running pass and then against whatever it waits on next, and a
	/// `oneshot::Receiver` polled again after it completed PANICS.
	#[tokio::test(start_paused = true)]
	async fn a_stop_that_already_fired_ends_every_later_wait() {
		let (shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();

		drop(shutdown_tx);
		stop.ended().await;
		stop.ended().await;
		assert!(!wait_while_paused(&mut stop, Duration::from_secs(600)).await);
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert!(
			!stop.pair_removed(),
			"the handle went away, not the pair — the loop must report the right end"
		);
	}

	/// A device-wide user ignore change wakes a running loop, so the pass that applies it runs now
	/// rather than at a safety-net tick up to minutes away. A burst of setter calls is ONE wake-up,
	/// not one per call, and a watch on its way out is not woken at all.
	#[tokio::test(start_paused = true)]
	async fn a_user_ignore_change_wakes_the_loop() {
		let (shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (rules_tx, mut rules) = user_ignore();

		rules_tx.send_modify(|version| *version += 1);
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			Duration::ZERO,
			"new user ignore patterns must wake the loop, not wait for the safety net"
		);

		// Two calls in a row wake the loop once — the pass that follows reads the latest text ...
		rules_tx.send_modify(|version| *version += 1);
		rules_tx.send_modify(|version| *version += 1);
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(start.elapsed(), Duration::ZERO);

		// ... and the wait after it is the ordinary one, rather than a spin on the second call.
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
		assert_eq!(
			start.elapsed(),
			SAFETY_NET,
			"a change the loop has already picked up must not wake it again"
		);

		// A watch being stopped or its pair removed ends the wait, change pending or not.
		drop(shutdown_tx);
		rules_tx.send_modify(|version| *version += 1);
		assert!(
			!wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				DEBOUNCE,
				None
			)
			.await
		);
	}

	/// The safety net's interval scales with what a whole-tree pass costs: the configured floor for
	/// a pair small enough that a pass is nearly free, a 1 % duty cycle above it, and a ceiling so
	/// the backstop never drifts out to most of a day.
	#[test]
	fn the_safety_net_scales_with_what_a_whole_tree_pass_costs() {
		// Floor: a 10-item pair measures its pass in microseconds and keeps today's cadence.
		assert_eq!(net_interval(SAFETY_NET, Duration::ZERO), SAFETY_NET);
		assert_eq!(
			net_interval(SAFETY_NET, Duration::from_millis(1)),
			SAFETY_NET
		);
		// The floor is exactly where the scaling takes over.
		assert_eq!(
			net_interval(SAFETY_NET, SAFETY_NET / SAFETY_NET_DUTY),
			SAFETY_NET
		);

		// Scaling: 100x the cost, so the pair spends about a hundredth of its life on the net.
		assert_eq!(
			net_interval(SAFETY_NET, Duration::from_secs(10)),
			Duration::from_secs(1_000)
		);
		assert_eq!(
			net_interval(SAFETY_NET, Duration::from_secs(31)),
			Duration::from_secs(3_100),
			"the measured 1M pass polls about every 52 minutes"
		);

		// Cap, including a cost absurd enough to overflow a naive multiply.
		assert_eq!(
			net_interval(SAFETY_NET, Duration::from_secs(600)),
			MAX_SAFETY_NET
		);
		assert_eq!(net_interval(SAFETY_NET, Duration::MAX), MAX_SAFETY_NET);

		// A floor above the ceiling is the caller's own choice of a slower backstop, and must not
		// panic the way a max-below-min clamp would.
		let slow = MAX_SAFETY_NET * 2;
		assert_eq!(net_interval(slow, Duration::from_secs(1)), slow);
		assert_eq!(net_interval(slow, Duration::MAX), slow);
	}

	/// A configured debounce, not the default one, is what a burst is coalesced over.
	#[tokio::test(start_paused = true)]
	async fn the_configured_debounce_is_what_a_burst_waits_out() {
		let (_shutdown_tx, _removed_tx, mut stop) = stop();
		let dirty = Notify::new();
		let mut safety_net = safety_net().await;
		let (_rules_tx, mut rules) = user_ignore();
		let debounce = Duration::from_millis(50);

		dirty.notify_one();
		let start = tokio::time::Instant::now();
		assert!(
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&mut safety_net,
				&mut rules,
				debounce,
				None
			)
			.await
		);
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
