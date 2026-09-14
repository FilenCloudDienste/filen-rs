//! Progress/events a sync pass reports to whoever initiated it.
//!
//! A [`SyncObserver`] is called SYNCHRONOUSLY, in happen-before order, as a pass plans and applies
//! its actions — so a caller (or UI) can show exactly what the engine is doing and why, live,
//! instead of only the post-hoc [`SyncReport`](super::SyncReport). Mirrors the cache's
//! `SyncRootCallback` shape. Register one via
//! [`SyncEngine::sync_once_observed`](super::SyncEngine::sync_once_observed) (one-shot) or
//! [`SyncEngine::watch_observed`](super::SyncEngine::watch_observed) (continuous).

use std::panic::{AssertUnwindSafe, catch_unwind};

use super::{SyncMode, apply::SyncReport};

/// A single event from a sync pass, delivered to a [`SyncObserver`] in the order it happens.
///
/// The lifecycle of one pass is: [`PassStarted`](Self::PassStarted) → (either
/// [`Refused`](Self::Refused), or any number of [`Conflict`](Self::Conflict) /
/// [`DeletionsHeld`](Self::DeletionsHeld) then [`Planned`](Self::Planned) followed by one
/// in-progress event per applied action, each possibly trailed by
/// [`ActionFailed`](Self::ActionFailed)) → [`PassCompleted`](Self::PassCompleted). A pass the pair's
/// pause cut short, or that could not take the drive lock, reports one
/// [`Interrupted`](Self::Interrupted) before it completes.
///
/// A pass that fails outright ends with [`PassFailed`](Self::PassFailed) instead of
/// `PassCompleted` — with or without a `PassStarted` before it, depending on how far it got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
	/// A pass began (after the read-only prepare), in this mode.
	PassStarted { mode: SyncMode },
	/// The pass refused to run (e.g. an unresolved name collision); no actions were applied.
	Refused { reason: String },
	/// How many executable actions this pass will apply (after guard screening) — the denominator
	/// for progress; the in-progress events below tick through it.
	Planned { actions: usize },
	/// A two-way path where both sides diverged; left untouched for the caller to resolve.
	Conflict { rel_path: String },
	/// The guard held some deletions back this pass (mass-delete volume, first sync, or an
	/// un-converged remote). `reason` is the guard's `Debug` rendering; `pass_token` identifies
	/// this exact batch for
	/// [`SyncEngine::approve_deletions`](super::SyncEngine::approve_deletions).
	DeletionsHeld {
		count: usize,
		reason: String,
		pass_token: String,
	},
	/// Uploading a local file to the remote.
	Uploading { rel_path: String },
	/// Downloading a remote file into the local tree.
	Downloading { rel_path: String },
	/// Creating a directory on the remote.
	CreatingRemoteDir { rel_path: String },
	/// Creating a directory in the local tree.
	CreatingLocalDir { rel_path: String },
	/// Trashing an item on the remote (a propagated local deletion).
	TrashingRemote { rel_path: String },
	/// Moving a locally-removed item into the pair's quarantine dir (a propagated remote deletion).
	DeletingLocal { rel_path: String },
	/// Re-parenting/renaming a file on the remote in place of a re-upload (a detected local move).
	MovingRemote { from: String, to: String },
	/// Renaming a local file in place of a re-download (a detected remote move).
	MovingLocal { from: String, to: String },
	/// An individual action failed; the pass continues past it (the failure is also in the report),
	/// and a failure no transfer to that side can get past — a full disk or a full account — also
	/// holds back the transfers that write there (see
	/// [`SyncReport::halted`](super::SyncReport::halted)).
	ActionFailed { rel_path: String, error: String },
	/// The pass was cut short: the pair was paused with
	/// [`PauseMode::Cancel`](super::PauseMode::Cancel) while it ran, so `actions` of its planned
	/// actions were not carried out — the transfer dropped in flight plus everything behind it — or
	/// the pass could not take the drive lock and carried out none of them.
	/// They left nothing behind and the next pass re-plans them (see
	/// [`SyncReport::interrupted`](super::SyncReport::interrupted)).
	Interrupted { actions: usize },
	/// A path that is already identical on both sides was adopted into the baseline (no transfer) —
	/// so a later one-sided change at that path is classified correctly rather than misread.
	AdoptedBaseline { rel_path: String },
	/// The pass finished; carries the full [`SyncReport`].
	PassCompleted { report: SyncReport },
	/// The pass failed outright, so there is no report: `error` is the rendering of the error
	/// [`sync_once_observed`](super::SyncEngine::sync_once_observed) returns (an unknown or
	/// removed pair, an unreadable baseline, a sync root the server no longer has). The last event
	/// of that pass; no [`PassCompleted`](Self::PassCompleted) follows. Failures of single actions
	/// are [`ActionFailed`](Self::ActionFailed) instead.
	PassFailed { error: String },
}

/// A sink for [`SyncEvent`]s, owned for the lifetime of a continuous
/// [`watch`](super::SyncEngine::watch_observed). Called SYNCHRONOUSLY between the pass's async
/// steps, in happen-before order, so keep it quick — offload heavy work onto a channel/queue.
///
/// One-shot callers use [`SyncEngine::sync_once_observed`](super::SyncEngine::sync_once_observed),
/// which takes any `&mut (dyn FnMut(SyncEvent) + Send)` (e.g. `&mut |event| { … }`).
///
/// An observer that panics does not take the pass down with it: the panic is caught at the call,
/// logged once, and that observer receives nothing more for the rest of the pass, which carries on
/// and still returns its report. The next pass calls it again. (The panic still reaches the process's
/// panic hook, and under `panic = "abort"` there is nothing to catch.)
pub type SyncObserver = Box<dyn FnMut(SyncEvent) + Send + 'static>;

/// Wrap `observer` so a panic inside it stays inside it: caught, logged once, and the observer is
/// skipped from then on. A pass calls its observer while holding the drive-write lock and between
/// the steps that keep the baseline and the two trees in step, so an unwind out of it could leave a
/// half-applied action behind; the pass is worth more than the observer's remaining events.
pub(super) fn contain_panics(
	observer: &mut (dyn FnMut(SyncEvent) + Send),
) -> impl FnMut(SyncEvent) + Send + '_ {
	let mut panicked = false;
	move |event| {
		if panicked {
			return;
		}
		if catch_unwind(AssertUnwindSafe(|| observer(event))).is_err() {
			panicked = true;
			tracing::error!(
				"a sync observer panicked; it receives no further events for the rest of this pass"
			);
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A panic inside the observer is caught, and the observer is never called again by that
	/// wrapper; an observer that did not panic sees every event.
	#[test]
	fn a_panicking_observer_is_contained_and_then_skipped() {
		let mut calls = 0usize;
		let mut observer = |_event: SyncEvent| {
			calls += 1;
			if calls == 2 {
				panic!("observer failure");
			}
		};
		{
			let mut contained = contain_panics(&mut observer);
			for actions in 0..5 {
				contained(SyncEvent::Planned { actions });
			}
		}
		assert_eq!(calls, 2, "the observer was called again after it panicked");

		let mut seen = Vec::new();
		let mut recorder = |event: SyncEvent| seen.push(event);
		{
			let mut contained = contain_panics(&mut recorder);
			for actions in 0..3 {
				contained(SyncEvent::Planned { actions });
			}
		}
		assert_eq!(seen.len(), 3);
	}
}
