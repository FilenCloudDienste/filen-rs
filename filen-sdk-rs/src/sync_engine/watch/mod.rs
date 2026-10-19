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

use filen_macros::js_type;
use notify::{EventHandler, RecursiveMode, Watcher};
use tokio::{sync::Notify, time::Instant};
use tracing::Instrument;

use super::{
	SyncEvent, SyncObserver, SyncReport,
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
#[js_type(export, no_deser, no_default, napi)]
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
#[js_type(export, no_deser, no_default, napi)]
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

		// The pair's changelists. The local handler below records what changed into them and a pass
		// takes them; the engine holds the same ones, so a one-shot `sync_once` reads the same set.
		let changes = self.pair_changes(pair).await;
		// The pair's own doorbell rather than one of this watch's: the engine rings it too, when a
		// caller approves held deletions (`approve_deletions`).
		let dirty = changes.doorbell();
		// Rung by `request_pass` alone, and heard even through a failure backoff (see
		// `wait_for_next_pass`).
		let requested = changes.request_signal();
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
		// `notify`'s ReadDirectoryChangesW backend (8.2.0 `windows.rs`) sets no `Flag::Rescan` and
		// no info string on any event, so a dropped event there is indistinguishable from a quiet
		// tree and a local changelist can never be known to be complete. Treat the watch as
		// degraded from its start rather than narrow a pass down on it. A pair no watch covers
		// reads its local side whole anyway, and says so with its own reason.
		#[cfg(target_os = "windows")]
		changes.degrade(FullPassReason::LocalEventsDegraded);
		// From here on something IS recording local changes, so a pass may narrow its local half
		// down to what the list holds. Before this — and after the loop below ends — an empty list
		// means "nobody was looking", which is not evidence of a quiet tree.
		changes.cover_local();
		// A platform whose watcher cannot report a dropped event (see above) has its
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
			run_loop(WatchLoop {
				engine,
				pair,
				config,
				dirty,
				requested,
				rules,
				stop,
				observer,
				status: status_tx,
			})
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

/// Everything the background loop of one watch owns (see [`run_loop`]).
struct WatchLoop {
	engine: Arc<SyncEngine>,
	pair: PairId,
	config: WatchConfig,
	/// The one signal both trigger sources ring — the local watcher and the remote sync root — and
	/// the engine rings on an approval of held deletions: the pair's doorbell
	/// (`PairChanges::doorbell`).
	dirty: Arc<Notify>,
	/// Rung when a caller asks for a pass (`SyncEngine::request_pass`): the pair's request signal
	/// (`PairChanges::request_signal`).
	requested: Arc<Notify>,
	/// Changes whenever the engine's user ignore rules do.
	rules: tokio::sync::watch::Receiver<u64>,
	stop: Stop,
	observer: SyncObserver,
	status: tokio::sync::watch::Sender<WatchStatus>,
}

/// The background loop: an initial pass, then debounced passes on `dirty`, plus a periodic pass,
/// until the watch is stopped or its pair removed (see [`Stop`]). A pass that fails outright backs
/// the loop off (see [`backoff`]) so a persistent error does not hot-loop at the debounce cadence;
/// that backoff is also the retry timer, since an outage over a quiescent tree produces no trigger
/// of its own.
async fn run_loop(watch: WatchLoop) {
	let WatchLoop {
		engine,
		pair,
		config,
		dirty,
		requested,
		mut rules,
		mut stop,
		mut observer,
		status,
	} = watch;
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
			&requested,
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
/// safety net or a change event happens to offer next. Only a caller's request (`requested`) cuts
/// it short: someone asked for this retry, typically after fixing what failed. While healthy the
/// wait ends on the periodic safety-net tick, on new device-wide user ignore patterns (`rules`), or
/// on a change event or a request once the burst behind it has gone quiet for `debounce`. New
/// patterns are not debounced: they arrive one deliberate call at a time, and a burst of calls is
/// already one wake-up (the signal carries the latest version, not each one).
///
/// A burst that never goes quiet is coalesced for at most [`MAX_DEBOUNCE_BURST`] debounces, and a
/// safety-net tick that comes due while it is being coalesced ends the wait too: neither the quiet
/// window nor a continuous writer may postpone a pass indefinitely.
async fn wait_for_next_pass(
	stop: &mut Stop,
	dirty: &Notify,
	requested: &Notify,
	safety_net: &mut tokio::time::Interval,
	rules: &mut tokio::sync::watch::Receiver<u64>,
	debounce: Duration,
	backoff: Option<Duration>,
) -> bool {
	if let Some(delay) = backoff {
		return tokio::select! {
			biased;
			_ = stop.ended() => false,
			_ = requested.notified() => true,
			_ = tokio::time::sleep(delay) => true,
		};
	}

	tokio::select! {
		biased;
		_ = stop.ended() => return false,
		_ = safety_net.tick() => {}
		_ = user_ignore_changed(rules) => {}
		_ = async {
			tokio::select! {
				_ = dirty.notified() => {}
				_ = requested.notified() => {}
			}
		} => {
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
			pass_outcome(&report)
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
/// So is one whose baseline or journal write did not land, or whose staged plan could not be read
/// (`store_failed`): the act that write was recording stands, and the next pass would run straight
/// into the same wedged store, or stage its plan beside it on the same disk. Each publishes a line
/// of its own ([`StoreFailure::line`](super::apply::StoreFailure::line)).
fn pass_outcome(report: &SyncReport) -> PassEnd {
	PassEnd {
		error: report.pass_error(),
		owed: !report.lock_failed && owes_a_pass(report),
		// A pass that could not take the drive lock still READ both sides: the lock is only asked
		// for once there is a plan to apply.
		read_whole: read_both_sides_whole(report),
		read_cost: report.read_cost,
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
mod tests;
