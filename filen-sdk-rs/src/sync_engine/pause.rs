//! Pausing a pair, and what that does to the pass already in flight.
//!
//! [`pause_pair`](super::SyncEngine::pause_pair) stops the NEXT pass; on its own that leaves a pass
//! already running to finish — minutes of uploading for a big pass, all of it under the
//! drive-write lock. A pause therefore also reaches into the running pass, in one of two flavours
//! ([`PauseMode`]):
//!
//! * [`Suspend`](PauseMode::Suspend) — the pass PARKS. Its action loop awaits the resume before
//!   starting the next action, so nothing new begins; a transfer already running finishes (the
//!   native upload/download paths poll no pause signal of their own, so there is no mid-transfer
//!   park to take). Resuming continues the same pass exactly where it stopped — a Rust future that
//!   nobody polls simply waits, so the pass keeps its plan, its drive lock and everything it has
//!   already done.
//! * [`Cancel`](PauseMode::Cancel) — the transfer in flight is DROPPED, every remaining action is
//!   skipped, and the pass returns a report whose [`interrupted`](super::SyncReport::interrupted)
//!   says how many actions it did not carry out. Nothing an interrupted action did is recorded
//!   half-way: a download writes to a `.filendl` temp file that its drop guard removes and puts
//!   back any local copy it had stashed out of its way, an upload
//!   becomes a file on the server only at its final `upload/done`, and the cancel covers a
//!   transfer's NETWORK op only — one that got as far as finishing still writes its baseline row
//!   and counts as applied, so the remote is never ahead of the baseline describing it. The one
//!   thing a drop cannot take back is an upload the server committed while its response was still
//!   in flight: that file exists, recorded nowhere, and its action counts as interrupted — the next
//!   pass uploads it again, over the same name, which versions it rather than duplicating it. The
//!   next pass re-plans whatever was left.
//!
//! A [`Suspend`](PauseMode::Suspend) ESCALATES into a [`Cancel`](PauseMode::Cancel) after
//! [`PauseOptions::cancel_after`], counted from the PAUSE CALL, which is what makes the default safe
//! to leave in a UI: a short pause resumes its transfers where they were, a pause nobody comes back
//! to gives them up rather than sitting on the drive-write lock for as long as the app is open. The
//! deadline is stamped once, on the pause itself, so "transfers will be dropped in five minutes"
//! means five minutes from the button press — not from whenever an action next happens to park on
//! it, which for a big transfer can be minutes later still.

use std::{future::Future, time::Duration};

use tokio::{sync::watch, time::Instant};

/// How long a [`Suspend`](PauseMode::Suspend) pause parks before it converts itself into a
/// [`Cancel`](PauseMode::Cancel) — the default [`PauseOptions::cancel_after`]. Long enough that a
/// pause a user reverses within a few minutes keeps its transfers, short enough that a forgotten
/// pause does not hold the drive-write lock (and its open connections) indefinitely.
pub const DEFAULT_CANCEL_AFTER: Duration = Duration::from_secs(300);

/// What pausing a pair does to the pass that is already running (see the [module
/// docs](self)). Neither flavour changes what pausing does to the NEXT pass: it does not run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseMode {
	/// Park the pass between actions and keep it alive; resuming continues it where it stopped.
	Suspend,
	/// Drop the transfer in flight, skip the rest of the plan, and return an
	/// [`interrupted`](super::SyncReport::interrupted) report.
	Cancel,
}

/// How to pause a pair — see [`SyncEngine::pause_pair_with`](super::SyncEngine::pause_pair_with).
/// [`Default`] is what plain [`pause_pair`](super::SyncEngine::pause_pair) uses:
/// [`Suspend`](PauseMode::Suspend), escalating after [`DEFAULT_CANCEL_AFTER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PauseOptions {
	/// What happens to the pass in flight.
	pub mode: PauseMode,
	/// How long AFTER THE PAUSE CALL a [`Suspend`](PauseMode::Suspend) becomes a
	/// [`Cancel`](PauseMode::Cancel) — not how long an action must sit parked, so the countdown a UI
	/// shows is the one the pass keeps. `None` never escalates: the pass then parks until the pair
	/// is resumed or its actions are cancelled, holding the drive-write lock throughout. Ignored by
	/// [`Cancel`](PauseMode::Cancel), which has nothing left to escalate.
	pub cancel_after: Option<Duration>,
}

impl Default for PauseOptions {
	fn default() -> Self {
		Self {
			mode: PauseMode::Suspend,
			cancel_after: Some(DEFAULT_CANCEL_AFTER),
		}
	}
}

/// The live control state of one pair: what a pass of it may do right now. `Run` is the only
/// non-paused value, so it doubles as the in-memory mirror of the persisted `paused` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PassControl {
	/// Not paused: passes run.
	Run,
	/// Paused, parking the pass in flight (see [`PauseMode::Suspend`]).
	Suspended {
		/// When this suspension turns itself into a [`Cancelled`](Self::Cancelled) — stamped by
		/// [`pausing`](Self::pausing) from [`PauseOptions::cancel_after`], so every action of the
		/// pass shares ONE deadline set at the pause call. `None` never escalates.
		escalate_at: Option<Instant>,
	},
	/// Paused, unwinding the pass in flight (see [`PauseMode::Cancel`]).
	Cancelled,
	/// The pair is being REMOVED: like [`Cancelled`](Self::Cancelled) for the pass, and FINAL — a
	/// pause, a resume or a cancel of the paused actions landing while
	/// [`remove_pair`](super::SyncEngine::remove_pair) waits for that pass is refused rather than
	/// allowed to take the cancel back, or to report a conversion of it. Reviving the pass there would
	/// either park it on a state nothing left in the engine can change — its channel and its row go
	/// with the pair moments later — or let it apply the rest of its plan against a pair that is
	/// being taken away.
	Retired,
}

impl PassControl {
	/// The state a pause puts the pair in, with its escalation deadline stamped HERE — at the pause
	/// call — rather than measured from wherever the pass happens to park afterwards. Re-issuing a
	/// pause stamps a fresh deadline, so new options restart the window rather than inheriting the
	/// old one's remainder.
	///
	/// A window the clock cannot hold (`Duration::MAX`, and anything else past the end of it) reads
	/// as no deadline at all: it is a moment nothing reaches, which is what `None` already means.
	pub(super) fn pausing(options: PauseOptions) -> Self {
		match options.mode {
			PauseMode::Suspend => Self::Suspended {
				escalate_at: options
					.cancel_after
					.and_then(|after| Instant::now().checked_add(after)),
			},
			PauseMode::Cancel => Self::Cancelled,
		}
	}

	/// Whether this state means the pair is paused — every state but [`Run`](Self::Run).
	pub(super) fn is_paused(self) -> bool {
		!matches!(self, Self::Run)
	}
}

/// The pause checkpoint an apply pass consults: one per pass, shared (cloned) by its concurrent
/// transfers. It holds a SENDER as well as reading the state, because an escalation observed by one
/// parked action converts the pause for the whole pair — otherwise every other parked action would
/// wait out its own window and the pass would unwind one lonely action at a time.
#[derive(Clone, Default)]
pub(super) struct PassGate {
	/// `None` for a pass nothing can pause (unit tests, and any caller that has no pair state).
	control: Option<watch::Sender<PassControl>>,
	/// A receiver held for as long as the pass (and every transfer that cloned this gate) lives,
	/// and never read: it makes [`watch::Sender::closed`] on the pair's channel resolve exactly
	/// when the pass has ended, which is how
	/// [`remove_pair`](super::SyncEngine::remove_pair) waits for the pass it just cancelled.
	_alive: Option<watch::Receiver<PassControl>>,
}

impl PassGate {
	pub(super) fn new(control: watch::Sender<PassControl>) -> Self {
		let alive = control.subscribe();
		Self {
			control: Some(control),
			_alive: Some(alive),
		}
	}

	/// Wait until the next action may begin. `false` means the pass was CANCELLED: nothing further
	/// may start, and the caller reports what it did not do.
	///
	/// A suspended pass parks here — the future is simply not resolved — until the pair is resumed
	/// (`true`), its actions are cancelled (`false`), or the pause outlives its
	/// [`cancel_after`](PauseOptions::cancel_after) window and this converts it (`false`). The
	/// window runs from the PAUSE CALL, not from the moment this action reached the checkpoint: an
	/// action that ran on for most of it gives up on the pause's schedule, not on its own. A pause
	/// re-issued with new options carries a new deadline and so restarts it.
	pub(super) async fn wait_to_start(&self) -> bool {
		let Some(control) = &self.control else {
			return true;
		};
		let mut state = control.subscribe();
		loop {
			let current = *state.borrow_and_update();
			match current {
				PassControl::Run => return true,
				PassControl::Cancelled | PassControl::Retired => return false,
				PassControl::Suspended { escalate_at } => {
					await_change(control, &mut state, escalate_at).await;
				}
			}
		}
	}

	/// Whether the pass's pair is being REMOVED, as opposed to merely cancelled — the two ways
	/// [`guard`](Self::guard) can come back empty, which the pass answers differently: a cancel
	/// leaves a paused pair to report on, a removal leaves no pair at all.
	pub(super) fn retired(&self) -> bool {
		self.control
			.as_ref()
			.is_some_and(|control| *control.borrow() == PassControl::Retired)
	}

	/// Race `work` against a cancel of the pass. `None` means it was DROPPED where it stood, which
	/// is safe only for work that has left nothing behind when it is dropped part-way — so this
	/// wraps an action's NETWORK op alone, never the baseline/journal row that records it: a
	/// transfer that finished must get to write its row even if the pass is cancelled while it
	/// waits for the store, or the remote ends up ahead of the baseline that describes it.
	///
	/// Unlike [`wait_to_start`](Self::wait_to_start), this does not park a suspended pass — the
	/// work is already running and the suspension's job is to start nothing NEW — but it does run
	/// that suspension's escalation timer, so a pause that lands while every action is in flight
	/// still gives them up after its window instead of waiting for one to finish first.
	pub(super) async fn guard<F>(&self, work: F) -> Option<F::Output>
	where
		F: Future,
	{
		let Some(control) = &self.control else {
			return Some(work.await);
		};
		let cancelled = cancelled(control);
		tokio::select! {
			output = work => Some(output),
			() = cancelled => None,
		}
	}
}

/// Wait for the pair's control state to change, converting a suspension that outlives its
/// [`escalate_at`](PassControl::Suspended::escalate_at) deadline into a cancel — for the WHOLE pair,
/// so every action waiting on it unwinds at the same moment.
///
/// The deadline is absolute (it was stamped at the pause call), so an action that reaches this after
/// the window has already elapsed converts the pause immediately instead of waiting out a fresh one.
async fn await_change(
	control: &watch::Sender<PassControl>,
	state: &mut watch::Receiver<PassControl>,
	escalate_at: Option<Instant>,
) {
	let changed = async {
		if state.changed().await.is_err() {
			// Nobody can change this pass's state any more — impossible while the gate holds a
			// sender. Never resolve, rather than spin on a channel that has nothing left to say.
			std::future::pending::<()>().await;
		}
	};
	match escalate_at {
		Some(deadline) => {
			if tokio::time::timeout_at(deadline, changed).await.is_err() {
				cancel_suspension(control);
			}
		}
		None => changed.await,
	}
}

/// Turn a suspension into a cancel, leaving every other state alone: a pair that is running has no
/// parked pass to unwind, and one already cancelling is where this would put it anyway.
pub(super) fn cancel_suspension(control: &watch::Sender<PassControl>) {
	control.send_if_modified(|control| {
		let suspended = matches!(control, PassControl::Suspended { .. });
		if suspended {
			*control = PassControl::Cancelled;
		}
		suspended
	});
}

/// Resolves once the pass is cancelled, and never otherwise — running the escalation timer of any
/// suspension it sees on the way, so a pause lands on the work in flight on its own schedule.
async fn cancelled(control: &watch::Sender<PassControl>) {
	let mut state = control.subscribe();
	loop {
		let current = *state.borrow_and_update();
		match current {
			PassControl::Cancelled | PassControl::Retired => return,
			PassControl::Run => await_change(control, &mut state, None).await,
			PassControl::Suspended { escalate_at } => {
				await_change(control, &mut state, escalate_at).await;
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::{cell::RefCell, task::Poll};

	use super::*;

	fn gate(initial: PassControl) -> (watch::Sender<PassControl>, PassGate) {
		let (tx, _rx) = watch::channel(initial);
		let gate = PassGate::new(tx.clone());
		(tx, gate)
	}

	/// A suspension issued right now, escalating `after` from here — what `pause_pair_with` sends.
	fn suspended(after: Duration) -> PassControl {
		PassControl::pausing(PauseOptions {
			mode: PauseMode::Suspend,
			cancel_after: Some(after),
		})
	}

	/// A pass nothing can pause runs straight through — the shape every non-engine caller gets.
	#[tokio::test]
	async fn an_ungated_pass_never_parks() {
		let gate = PassGate::default();
		assert!(gate.wait_to_start().await);
		assert_eq!(gate.guard(async { 7 }).await, Some(7));
	}

	/// SUSPEND parks the action loop between actions, and a resume continues it — the action that
	/// was about to start still starts, nothing is re-done and nothing is skipped.
	#[tokio::test(start_paused = true)]
	async fn a_suspended_pass_parks_between_actions_and_resumes() {
		let (tx, gate) = gate(PassControl::Suspended { escalate_at: None });

		let mut next = std::pin::pin!(gate.wait_to_start());
		assert!(
			futures::poll!(next.as_mut()).is_pending(),
			"a suspended pass must not start its next action"
		);

		tx.send(PassControl::Run).unwrap();
		assert_eq!(
			futures::poll!(next.as_mut()),
			Poll::Ready(true),
			"the resume must let the parked action start"
		);
	}

	/// CANCEL drops the action in flight, and the action leaves NOTHING behind: its baseline row is
	/// written after the transfer, so a drop before that records nothing at all.
	#[tokio::test(start_paused = true)]
	async fn a_cancelled_action_is_dropped_and_records_no_row() {
		let (tx, gate) = gate(PassControl::Run);
		let rows: RefCell<Vec<&str>> = RefCell::new(Vec::new());

		// The transfer under the gate, then the row it produces — the shape `apply_transfer` uses.
		let mut action = std::pin::pin!(async {
			if gate
				.guard(tokio::time::sleep(Duration::from_secs(30)))
				.await
				.is_none()
			{
				return;
			}
			rows.borrow_mut().push("a.txt");
		});
		assert!(futures::poll!(action.as_mut()).is_pending());

		tx.send(PassControl::Cancelled).unwrap();
		assert_eq!(
			futures::poll!(action.as_mut()),
			Poll::Ready(()),
			"a cancel must drop the action in flight"
		);
		assert!(
			rows.borrow().is_empty(),
			"a cancelled action must leave no baseline row behind"
		);
	}

	/// And the actions BEHIND the cancelled one never start.
	#[tokio::test(start_paused = true)]
	async fn a_cancelled_pass_starts_no_further_action() {
		let (_tx, gate) = gate(PassControl::Cancelled);
		assert!(
			!gate.wait_to_start().await,
			"the action loop started anyway"
		);
		assert_eq!(
			gate.guard(std::future::pending::<()>()).await,
			None,
			"and work still running must be dropped"
		);
	}

	/// A transfer that FINISHED keeps what it did: the gate covers the network op alone, so the
	/// baseline row written after it — which waits on the store lock the pass's other transfers are
	/// contending for — is not dropped by a cancel that lands in that window. Without this the
	/// server holds a file no baseline row describes and no report counts.
	#[tokio::test(start_paused = true)]
	async fn a_finished_transfer_records_its_row_even_if_the_pass_is_cancelled() {
		let (tx, gate) = gate(PassControl::Run);
		let rows: RefCell<Vec<&str>> = RefCell::new(Vec::new());
		let store = tokio::sync::Mutex::new(());
		let busy = store.lock().await;

		// The shape `apply_transfer` uses: the network op under the gate, its row after it.
		let mut action = std::pin::pin!(async {
			if gate
				.guard(tokio::time::sleep(Duration::from_secs(30)))
				.await
				.is_none()
			{
				return;
			}
			let _store = store.lock().await;
			rows.borrow_mut().push("a.txt");
		});
		assert!(futures::poll!(action.as_mut()).is_pending());
		tokio::time::advance(Duration::from_secs(30)).await;
		// The transfer is done; its row is waiting for the store another transfer is holding.
		assert!(futures::poll!(action.as_mut()).is_pending());

		// `send_replace`, as the engine's own pause does: the guard is past its network op, so it
		// has no receiver left listening — which is the point.
		tx.send_replace(PassControl::Cancelled);
		assert!(
			futures::poll!(action.as_mut()).is_pending(),
			"a cancel must not drop the bookkeeping of a transfer that already finished"
		);
		drop(busy);
		assert_eq!(futures::poll!(action.as_mut()), Poll::Ready(()));
		assert_eq!(
			*rows.borrow(),
			["a.txt"],
			"the finished transfer never recorded what it did"
		);
	}

	/// A suspension that outlives its window converts itself into a cancel — for the whole pair, so
	/// the actions parked beside it unwind at the same moment instead of each waiting out its own.
	#[tokio::test(start_paused = true)]
	async fn a_suspension_escalates_into_a_cancel_after_its_window() {
		let (tx, gate) = gate(suspended(Duration::from_secs(2)));

		let mut first = std::pin::pin!(gate.wait_to_start());
		let mut second = std::pin::pin!(gate.wait_to_start());
		assert!(futures::poll!(first.as_mut()).is_pending());
		assert!(futures::poll!(second.as_mut()).is_pending());

		tokio::time::advance(Duration::from_secs(2)).await;
		assert_eq!(
			futures::poll!(first.as_mut()),
			Poll::Ready(false),
			"the parked action must give up once the window elapsed"
		);
		assert_eq!(
			*tx.borrow(),
			PassControl::Cancelled,
			"the escalation must convert the PAIR's pause, not just this one action"
		);
		assert_eq!(
			futures::poll!(second.as_mut()),
			Poll::Ready(false),
			"the action parked beside it must unwind at once, not on its own timer"
		);
	}

	/// The window is not conditional on an action being PARKED: a suspension that lands while every
	/// action is in flight (one big upload, or a full set of concurrent ones) escalates on its own
	/// schedule, rather than waiting for an action to finish so that the next one can park and
	/// start the timer.
	#[tokio::test(start_paused = true)]
	async fn a_suspension_escalates_while_an_action_is_running() {
		let (tx, gate) = gate(PassControl::Run);

		let mut action = std::pin::pin!(gate.guard(std::future::pending::<()>()));
		assert!(futures::poll!(action.as_mut()).is_pending());

		tx.send(suspended(Duration::from_secs(2))).unwrap();
		assert!(
			futures::poll!(action.as_mut()).is_pending(),
			"a suspension starts nothing new, but does not drop what is already running"
		);

		tokio::time::advance(Duration::from_secs(2)).await;
		assert_eq!(
			futures::poll!(action.as_mut()),
			Poll::Ready(None),
			"the running action must be given up once the window elapsed"
		);
		assert_eq!(
			*tx.borrow(),
			PassControl::Cancelled,
			"the escalation must convert the PAIR's pause"
		);
	}

	/// A pause reversed inside its window keeps everything: no escalation, and the pass runs on.
	#[tokio::test(start_paused = true)]
	async fn a_pause_resumed_inside_its_window_does_not_escalate() {
		let (tx, gate) = gate(suspended(Duration::from_secs(300)));

		let mut next = std::pin::pin!(gate.wait_to_start());
		assert!(futures::poll!(next.as_mut()).is_pending());
		tokio::time::advance(Duration::from_secs(60)).await;
		assert!(futures::poll!(next.as_mut()).is_pending());

		tx.send(PassControl::Run).unwrap();
		assert_eq!(futures::poll!(next.as_mut()), Poll::Ready(true));
		tokio::time::advance(Duration::from_secs(600)).await;
		assert_eq!(
			*tx.borrow(),
			PassControl::Run,
			"a resumed pause must not escalate afterwards"
		);
	}

	/// The default pause suspends, and stamps its deadline from the CALL: `cancel_after` after now,
	/// not after whatever the pass does next.
	#[tokio::test(start_paused = true)]
	async fn the_default_pause_suspends_and_escalates() {
		let options = PauseOptions::default();
		assert_eq!(options.mode, PauseMode::Suspend);
		assert_eq!(options.cancel_after, Some(DEFAULT_CANCEL_AFTER));
		assert_eq!(
			PassControl::pausing(options),
			PassControl::Suspended {
				escalate_at: Some(Instant::now() + DEFAULT_CANCEL_AFTER)
			}
		);
		assert_eq!(
			PassControl::pausing(PauseOptions {
				mode: PauseMode::Cancel,
				cancel_after: None,
			}),
			PassControl::Cancelled
		);
		assert!(!PassControl::Run.is_paused());
		assert!(PassControl::Cancelled.is_paused());
		assert!(PassControl::Suspended { escalate_at: None }.is_paused());
	}

	/// The window runs from the PAUSE, not from the moment an action reaches the checkpoint: an
	/// action that was still transferring for most of it gives up when the pause says so. Otherwise
	/// "transfers will be dropped in five minutes" means five minutes after the last one finishes,
	/// which for a big queue is not a number anyone can act on.
	#[tokio::test(start_paused = true)]
	async fn the_escalation_window_is_measured_from_the_pause_call() {
		let (tx, gate) = gate(PassControl::Run);
		tx.send(suspended(Duration::from_secs(10))).unwrap();

		// The action in flight runs on for most of the window before the next one can park.
		tokio::time::advance(Duration::from_secs(8)).await;
		let mut next = std::pin::pin!(gate.wait_to_start());
		assert!(futures::poll!(next.as_mut()).is_pending());

		tokio::time::advance(Duration::from_secs(2)).await;
		assert_eq!(
			futures::poll!(next.as_mut()),
			Poll::Ready(false),
			"the window restarted when the action parked instead of running from the pause"
		);
		assert_eq!(*tx.borrow(), PassControl::Cancelled);
	}

	/// A window the clock cannot represent — `Duration::MAX`, the ordinary way to spell "effectively
	/// never" — means the pause never escalates. It is a deadline nothing can reach, so the honest
	/// answer is the one `None` already has; panicking inside the pause call, on an option a caller
	/// is free to construct, is not.
	#[tokio::test(start_paused = true)]
	async fn a_window_no_deadline_can_hold_never_escalates() {
		assert_eq!(
			PassControl::pausing(PauseOptions {
				mode: PauseMode::Suspend,
				cancel_after: Some(Duration::MAX),
			}),
			PassControl::Suspended { escalate_at: None },
			"a window past the end of the clock must read as 'never', not fail the pause"
		);
	}

	/// A pause RE-ISSUED with new options is a new pause: its window starts again, so a caller that
	/// extends a pause gets the extension it asked for rather than the remainder of the old one.
	#[tokio::test(start_paused = true)]
	async fn a_re_issued_pause_restarts_the_escalation_window() {
		let (tx, gate) = gate(suspended(Duration::from_secs(10)));
		let mut next = std::pin::pin!(gate.wait_to_start());
		assert!(futures::poll!(next.as_mut()).is_pending());

		tokio::time::advance(Duration::from_secs(8)).await;
		tx.send(suspended(Duration::from_secs(10))).unwrap();
		assert!(futures::poll!(next.as_mut()).is_pending());

		// Past the FIRST pause's deadline, inside the second's.
		tokio::time::advance(Duration::from_secs(8)).await;
		assert!(
			futures::poll!(next.as_mut()).is_pending(),
			"the re-issued pause kept the first one's deadline"
		);

		tokio::time::advance(Duration::from_secs(2)).await;
		assert_eq!(futures::poll!(next.as_mut()), Poll::Ready(false));
	}
}
