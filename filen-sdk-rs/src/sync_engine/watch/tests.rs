use std::{
	path::{Path, PathBuf},
	sync::Arc,
	time::Duration,
};

use futures::FutureExt;
use notify::{
	EventHandler, EventKind, RecursiveMode, Watcher, WatcherKind,
	event::{CreateKind, ModifyKind, RenameMode},
};
use tokio::sync::Notify;

use super::{
	BASE_BACKOFF, DEBOUNCE, FullPassReason, MAX_BACKOFF, MAX_DEBOUNCE_BURST, MAX_SAFETY_NET,
	PairChanges, SAFETY_NET, SAFETY_NET_DUTY, Stop, SyncReport, WatchConfig, WatchState,
	WatchStatus, backoff, local_event_handler, net_due, net_interval, owes_a_pass, pass_outcome,
	publish_pass, read_both_sides_whole, start_local_watcher, triggers_pass, wait_for_next_pass,
	wait_while_paused,
};
use crate::{
	Error, ErrorKind,
	sync_engine::apply::{
		LOCK_FAILURE, STORE_FAILURE, StoreFailure, UNREAD_PLAN, note_lock_failure,
	},
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
		start_local_watcher::<Unstartable>(1, Path::new("/sync/root"), |_| {}, &status).is_none()
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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

/// A caller's request for a pass ends a healthy wait at the debounce, like a change, and cuts a
/// failure backoff short where a change cannot: a "sync now" pressed after fixing what failed
/// must not wait out up to [`MAX_BACKOFF`] doing nothing.
#[tokio::test(start_paused = true)]
async fn a_requested_pass_runs_at_the_debounce_and_through_a_backoff() {
	let (_shutdown_tx, _removed_tx, mut stop) = stop();
	let dirty = Notify::new();
	let requested = Notify::new();
	let mut safety_net = safety_net().await;
	let (_rules_tx, mut rules) = user_ignore();

	requested.notify_one();
	let start = tokio::time::Instant::now();
	assert!(
		wait_for_next_pass(
			&mut stop,
			&dirty,
			&requested,
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
		"a request waits out the debounce"
	);

	requested.notify_one();
	dirty.notify_one();
	let start = tokio::time::Instant::now();
	assert!(
		wait_for_next_pass(
			&mut stop,
			&dirty,
			&requested,
			&mut safety_net,
			&mut rules,
			DEBOUNCE,
			Some(MAX_BACKOFF)
		)
		.await
	);
	assert_eq!(
		start.elapsed(),
		Duration::ZERO,
		"a request must cut the backoff short"
	);
	assert!(
		dirty.notified().now_or_never().is_some(),
		"the change event must still be pending, not consumed by the backoff"
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
			&Notify::new(),
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
			&Notify::new(),
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
				&Notify::new(),
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
			wait_for_next_pass(
				&mut stop,
				&dirty,
				&Notify::new(),
				&mut net,
				&mut rules,
				DEBOUNCE,
				None
			)
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
	let end = pass_outcome(&report);
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
		store_failed: Some(StoreFailure::RecordNotWritten),
		..SyncReport::default()
	};
	let end = pass_outcome(&report);
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

/// A pass that could not read its staged plan is a failed pass too, and publishes that it could
/// not READ its plan: no record of work it did went missing.
#[test]
fn a_pass_whose_plan_could_not_be_read_counts_as_a_failure_of_its_own() {
	let report = SyncReport {
		uploaded: 1,
		store_failed: Some(StoreFailure::UnreadPlan),
		..SyncReport::default()
	};
	let end = pass_outcome(&report);
	assert_eq!(end.error.as_deref(), Some(UNREAD_PLAN));
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
	let after_a_long_upload = pass_outcome(&SyncReport {
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
	let big_tree = pass_outcome(&SyncReport {
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
			pass_outcome(&SyncReport {
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
			&Notify::new(),
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
