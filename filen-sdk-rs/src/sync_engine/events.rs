//! Progress/events a sync pass reports to whoever initiated it.
//!
//! A [`SyncObserver`] is called SYNCHRONOUSLY, in happen-before order, as a pass plans and applies
//! its actions — so a caller (or UI) can show exactly what the engine is doing and why, live,
//! instead of only the post-hoc [`SyncReport`](super::SyncReport). Mirrors the cache's
//! `SyncRootCallback` shape. Register one via
//! [`SyncEngine::sync_once_observed`](super::SyncEngine::sync_once_observed) (one-shot) or
//! [`SyncEngine::watch_observed`](super::SyncEngine::watch_observed) (continuous).

use super::{SyncMode, apply::SyncReport};

/// A single event from a sync pass, delivered to a [`SyncObserver`] in the order it happens.
///
/// The lifecycle of one pass is: [`PassStarted`](Self::PassStarted) → (either
/// [`Refused`](Self::Refused), or any number of [`Conflict`](Self::Conflict) /
/// [`DeletionsHeld`](Self::DeletionsHeld) then [`Planned`](Self::Planned) followed by one
/// in-progress event per applied action, each possibly trailed by
/// [`ActionFailed`](Self::ActionFailed)) → [`PassCompleted`](Self::PassCompleted).
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
	/// An individual action failed; the pass continues past it (the failure is also in the report).
	ActionFailed { rel_path: String, error: String },
	/// A path that is already identical on both sides was adopted into the baseline (no transfer) —
	/// so a later one-sided change at that path is classified correctly rather than misread.
	AdoptedBaseline { rel_path: String },
	/// The pass finished; carries the full [`SyncReport`].
	PassCompleted { report: SyncReport },
}

/// A sink for [`SyncEvent`]s, owned for the lifetime of a continuous
/// [`watch`](super::SyncEngine::watch_observed). Called SYNCHRONOUSLY between the pass's async
/// steps, in happen-before order, so keep it quick — offload heavy work onto a channel/queue.
///
/// One-shot callers use [`SyncEngine::sync_once_observed`](super::SyncEngine::sync_once_observed),
/// which takes any `&mut (dyn FnMut(SyncEvent) + Send)` (e.g. `&mut |event| { … }`).
pub type SyncObserver = Box<dyn FnMut(SyncEvent) + Send + 'static>;
