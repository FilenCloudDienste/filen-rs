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

/// Only a cancel or a removal stops work that a suspension lets run on.
#[test]
fn only_a_cancel_or_a_removal_is_cancelled() {
	assert!(PassGate::default().not_cancelled());
	for (state, not_cancelled) in [
		(PassControl::Run, true),
		(PassControl::Suspended { escalate_at: None }, true),
		(PassControl::Cancelled, false),
		(PassControl::Retired, false),
	] {
		let (_tx, gate) = gate(state);
		assert_eq!(gate.not_cancelled(), not_cancelled, "{state:?}");
	}
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
