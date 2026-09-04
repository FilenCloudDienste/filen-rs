//! The mass-delete guard: a safety screen between the reconciler's plan and the apply layer.
//!
//! Two distinct failure modes warrant holding deletions back:
//! 1. an INCOMPLETE local scan (a missing/unreadable root, an unreadable subtree, a name
//!    collision) — a half-read source must never be read as "everything was deleted"; and
//! 2. a deletion VOLUME beyond a configurable threshold — a legitimately large delete is allowed
//!    up to a point, past which it is held for explicit approval.
//!
//! When the guard trips it holds the deletions (non-destructive creates/transfers still apply)
//! and reports them so the caller can approve the batch or adjust the threshold. The one exception
//! is a delete paired with a create at the SAME path — a file<->dir type flip — which is held as a
//! unit: executing half of it only produces a server rejection every pass. Pure and synchronous so
//! the policy is fully unit-testable.

use std::{collections::HashSet, fmt};

use super::plan::SyncAction;

/// Deletion-volume policy. The limit for a pass is `max(floor, ratio * tracked_items)`: small
/// deletes are always allowed (the floor), and beyond that up to a fraction of the tracked set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DeleteGuard {
	floor: usize,
	ratio: f64,
}

impl Default for DeleteGuard {
	fn default() -> Self {
		// Always allow up to `floor` deletions (routine small cleanups never prompt); beyond that,
		// hold once the deletion count exceeds `ratio` of the tracked set. The floor is small on
		// purpose: a larger floor (e.g. 100) makes the ratio inert for normal-sized folders —
		// deleting 100% of a 50-item sync would never trip — defeating the guard.
		Self {
			floor: 10,
			ratio: 0.5,
		}
	}
}

impl DeleteGuard {
	/// A guard that never trips on volume (a power-user opt-out); the scan-incomplete precondition
	/// still applies.
	// Exercised by the guard unit tests; retained as the "disable the volume floor" API surface.
	#[allow(dead_code)]
	pub(crate) fn unlimited() -> Self {
		Self {
			floor: usize::MAX,
			ratio: 1.0,
		}
	}

	fn limit(&self, tracked: usize) -> usize {
		let ratio_limit = (self.ratio * tracked as f64) as usize;
		self.floor.max(ratio_limit)
	}
}

/// Why the guard held this pass's deletions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardReason {
	/// The local scan did not complete, so apparent deletions may be phantom (an unmounted root,
	/// an unreadable subtree). All deletions are held unconditionally.
	ScanIncomplete,
	/// First sync of the pair (empty baseline): with no established sync relationship, a
	/// "deletion" is really a pre-existing destination item the source happens to lack — not a
	/// propagated removal. Held so a first sync against a non-empty destination cannot wipe it
	/// (the locked "start from an empty destination" expectation; surfaced rather than silently
	/// destroyed).
	FirstSyncWithDeletions { deletions: usize },
	/// The remote view has never converged (the cache snapshot carries no watermark), so its
	/// apparent emptiness is untrustworthy — a "remote deletion" may just be an un-observed item.
	/// Held until the cache converges at least once.
	RemoteUnconverged { deletions: usize },
	/// The remote view came back COMPLETELY empty while the baseline still tracks remote items,
	/// and this pass would delete MORE THAN ONE of them. A converged-but-empty listing is exactly
	/// what a transient backend/cache fault looks like (the cache logs "every sync root listed
	/// EMPTY but the cache holds N item(s)" and converges to that), and a small pair sails under
	/// the volume threshold, so the wipe would apply unchallenged. In effect the volume floor
	/// drops to one item while the remote lists nothing at all.
	///
	/// Deleting the last remaining item is deliberately NOT held: an empty remote is the normal,
	/// permanent end state of that deletion, so holding it would never release and the pair would
	/// keep the local copy forever.
	RemoteEmptied { deletions: usize },
	/// The deletion count exceeded the configured limit for the tracked-item count (see
	/// [`DeleteGuard`]).
	ExceededThreshold { deletions: usize, limit: usize },
}

impl fmt::Display for GuardReason {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::ScanIncomplete => f.write_str(
				"the local scan did not complete, so an apparently-deleted item may just be unread",
			),
			Self::FirstSyncWithDeletions { deletions } => write!(
				f,
				"first sync of this pair: {deletions} pre-existing destination item(s) would be deleted"
			),
			Self::RemoteUnconverged { deletions } => write!(
				f,
				"the remote view has never converged, so {deletions} apparent remote deletion(s) are untrustworthy"
			),
			Self::RemoteEmptied { deletions } => write!(
				f,
				"the remote listed NOTHING at all while {deletions} item(s) are still tracked"
			),
			Self::ExceededThreshold { deletions, limit } => {
				write!(f, "{deletions} deletion(s) exceed the limit of {limit}")
			}
		}
	}
}

/// The screened plan: the actions safe to apply now, plus any deletions held back (and why).
#[derive(Debug)]
pub(crate) struct GuardDecision {
	pub(crate) safe: Vec<SyncAction>,
	pub(crate) held: Vec<SyncAction>,
	pub(crate) reason: Option<GuardReason>,
}

/// Inputs to [`screen`] describing how trustworthy this pass's state is.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScreenState {
	/// The local scan finished without errors (a partial scan must not be read as deletions).
	pub(crate) scan_complete: bool,
	/// The remote view has converged at least once (the cache snapshot carried a watermark).
	pub(crate) remote_converged: bool,
	/// The remote view holds NO items at all while the baseline still tracks at least one item
	/// with a remote uuid — an all-empty listing that would delete the whole pair.
	pub(crate) remote_emptied: bool,
	// (see `GuardReason::RemoteEmptied`: a one-item pair's last deletion is still released)
	/// This is the pair's first sync (the baseline is empty — no established relationship).
	pub(crate) first_sync: bool,
	/// How many items the pair's baseline currently tracks (the ratio denominator).
	pub(crate) tracked: usize,
}

impl ScreenState {
	/// Whether this pass's evidence that something is ABSENT can be acted on: a complete scan, and
	/// a remote view that has converged at least once and is not wholly empty. A baseline row
	/// dropped on untrustworthy absence resurrects the item on the next healthy pass, which reads
	/// the surviving side as a fresh creation.
	///
	/// `remote_converged` only records that the view converged ONCE, so it stays true straight
	/// through a transient all-empty listing; `remote_emptied` is what catches that fault. The
	/// price is that a pair whose items all vanished from both sides keeps their rows until the
	/// remote lists something again — dead bookkeeping that costs nothing, where the row the fault
	/// would have dropped costs a resurrected file.
	pub(crate) fn absence_trusted(&self) -> bool {
		self.scan_complete && self.remote_converged && !self.remote_emptied
	}
}

/// Screen a plan: hold its deletions when the pass's state is not trustworthy enough to act on
/// them (incomplete scan, first sync against a non-empty destination, un-converged or wholly empty
/// remote) or the deletion volume exceeds the guard's limit; otherwise pass everything through. Non-destructive
/// actions (creates/transfers) always proceed.
pub(crate) fn screen(
	actions: Vec<SyncAction>,
	state: ScreenState,
	guard: DeleteGuard,
) -> GuardDecision {
	let deletions = actions.iter().filter(|a| a.is_delete()).count();
	if deletions == 0 {
		return GuardDecision {
			safe: actions,
			held: Vec::new(),
			reason: None,
		};
	}

	let reason = if !state.scan_complete {
		Some(GuardReason::ScanIncomplete)
	} else if state.first_sync {
		Some(GuardReason::FirstSyncWithDeletions { deletions })
	} else if !state.remote_converged {
		Some(GuardReason::RemoteUnconverged { deletions })
	} else if state.remote_emptied && deletions > 1 {
		Some(GuardReason::RemoteEmptied { deletions })
	} else {
		let limit = guard.limit(state.tracked);
		if deletions > limit {
			Some(GuardReason::ExceededThreshold { deletions, limit })
		} else {
			None
		}
	};

	match reason {
		None => GuardDecision {
			safe: actions,
			held: Vec::new(),
			reason: None,
		},
		Some(reason) => {
			// A delete whose path is also (re)created this pass is half of a type flip: the create
			// is rejected while the old item still exists, so releasing it alone would fail every
			// pass and never let the baseline advance. Hold both halves together.
			let deleted_paths: HashSet<&str> = actions
				.iter()
				.filter(|a| a.is_delete())
				.map(SyncAction::rel_path)
				.collect();
			let replaced: HashSet<String> = actions
				.iter()
				.filter(|a| a.is_create() && deleted_paths.contains(a.rel_path()))
				.map(|a| a.rel_path().to_string())
				.collect();
			let (held, safe): (Vec<_>, Vec<_>) = actions
				.into_iter()
				.partition(|a| a.is_delete() || (a.is_create() && replaced.contains(a.rel_path())));
			tracing::debug!(
				"guard: holding {} action(s) back this pass — {reason:?}",
				held.len()
			);
			GuardDecision {
				safe,
				held,
				reason: Some(reason),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync_engine::baseline::NodeKind;

	fn del(rel: &str) -> SyncAction {
		SyncAction::DeleteLocal {
			rel_path: rel.to_string(),
			kind: NodeKind::File,
		}
	}

	fn upload(rel: &str) -> SyncAction {
		SyncAction::UploadFile {
			rel_path: rel.to_string(),
		}
	}

	fn trash_dir(rel: &str) -> SyncAction {
		SyncAction::TrashRemote {
			rel_path: rel.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: uuid::Uuid::nil(),
		}
	}

	fn create_remote_dir(rel: &str) -> SyncAction {
		SyncAction::CreateRemoteDir {
			rel_path: rel.to_string(),
		}
	}

	/// An established, converged, completed-scan state with `tracked` baseline items — the normal
	/// steady-state in which the volume threshold is the only thing that can hold deletions.
	fn established(tracked: usize) -> ScreenState {
		ScreenState {
			scan_complete: true,
			remote_converged: true,
			remote_emptied: false,
			first_sync: false,
			tracked,
		}
	}

	#[test]
	fn passes_everything_when_under_the_limit_and_scan_complete() {
		let actions = vec![upload("a"), del("b")];
		let decision = screen(actions, established(100), DeleteGuard::default());
		assert_eq!(decision.safe.len(), 2);
		assert!(decision.held.is_empty());
		assert!(decision.reason.is_none());
	}

	#[test]
	fn holds_all_deletions_when_the_scan_is_incomplete() {
		// Even a single deletion is held if the scan could not be trusted — but non-destructive
		// actions still pass.
		let actions = vec![upload("keep"), del("a"), del("b")];
		let state = ScreenState {
			scan_complete: false,
			..established(10_000)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(decision.reason, Some(GuardReason::ScanIncomplete));
		assert_eq!(decision.safe, vec![upload("keep")]);
		assert_eq!(decision.held.len(), 2, "both deletions held");
	}

	#[test]
	fn holds_deletions_on_first_sync_so_a_populated_destination_is_not_wiped() {
		// Empty baseline (first sync): a "deletion" is a pre-existing destination item, not a
		// propagated removal. Held + surfaced; the upload still proceeds.
		let actions = vec![upload("mine"), del("theirs")];
		let state = ScreenState {
			first_sync: true,
			tracked: 0,
			..established(0)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(
			decision.reason,
			Some(GuardReason::FirstSyncWithDeletions { deletions: 1 })
		);
		assert_eq!(
			decision.safe,
			vec![upload("mine")],
			"additive action still applies"
		);
		assert_eq!(decision.held.len(), 1);
	}

	#[test]
	fn holds_deletions_when_the_remote_has_never_converged() {
		// A non-first sync (baseline exists) but the cache snapshot has no watermark: its emptiness
		// is untrustworthy, so deletions are held until it converges.
		let actions = vec![del("maybe_gone")];
		let state = ScreenState {
			remote_converged: false,
			..established(5)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(
			decision.reason,
			Some(GuardReason::RemoteUnconverged { deletions: 1 })
		);
		assert_eq!(decision.held.len(), 1);
	}

	#[test]
	fn holds_deletions_when_the_whole_remote_view_came_back_empty() {
		// A converged snapshot with zero items while the baseline still tracks remote ones: a
		// small pair's wipe (2 deletions) is under the volume floor, so only this reason stops it.
		let actions = vec![del("a"), del("b")];
		let state = ScreenState {
			remote_emptied: true,
			..established(2)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(
			decision.reason,
			Some(GuardReason::RemoteEmptied { deletions: 2 })
		);
		assert_eq!(decision.held.len(), 2, "the whole pair is held");
	}

	#[test]
	fn an_emptied_remote_still_releases_a_single_deletion() {
		// Deleting the last item legitimately empties the remote, and that emptiness is permanent:
		// holding it would never release and the local copy would survive the deletion forever.
		let actions = vec![del("only")];
		let state = ScreenState {
			remote_emptied: true,
			..established(1)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert!(decision.reason.is_none(), "{:?}", decision.reason);
		assert_eq!(decision.safe.len(), 1);
	}

	#[test]
	fn a_wholly_empty_remote_view_is_not_trusted_absence_evidence() {
		let mut state = established(4);
		assert!(state.absence_trusted(), "a converged, fully scanned pass");
		state.remote_emptied = true;
		assert!(
			!state.absence_trusted(),
			"a wholly empty remote view may be a transient fault; the baseline row is the only \
			 record that the item was ever synced"
		);
	}

	#[test]
	fn holds_deletions_past_the_volume_threshold() {
		// 30 deletions against 20 tracked exceeds max(floor=10, 0.5*20=10) = 10.
		let mut actions: Vec<_> = (0..30).map(|i| del(&format!("d{i}"))).collect();
		actions.push(upload("safe"));
		let decision = screen(actions, established(20), DeleteGuard::default());
		assert_eq!(
			decision.reason,
			Some(GuardReason::ExceededThreshold {
				deletions: 30,
				limit: 10,
			})
		);
		assert_eq!(decision.safe, vec![upload("safe")]);
		assert_eq!(decision.held.len(), 30);
	}

	#[test]
	fn holds_a_total_wipe_of_a_normal_sized_tree() {
		// The regression the old floor=100 missed: deleting ALL 20 files of a 20-item sync must
		// trip the guard (max(10, 0.5*20=10) = 10; 20 > 10), not silently trash everything.
		let actions: Vec<_> = (0..20).map(|i| del(&format!("f{i}"))).collect();
		let decision = screen(actions, established(20), DeleteGuard::default());
		assert_eq!(
			decision.reason,
			Some(GuardReason::ExceededThreshold {
				deletions: 20,
				limit: 10,
			})
		);
		assert_eq!(decision.held.len(), 20, "the whole wipe is held");
	}

	#[test]
	fn allows_a_small_routine_deletion() {
		// Deleting a handful (<= floor) never prompts, even if it is a high fraction of a tiny tree.
		let actions = vec![del("a"), del("b"), upload("c")];
		let decision = screen(actions, established(3), DeleteGuard::default());
		assert!(decision.reason.is_none(), "2 deletions is below the floor");
		assert_eq!(decision.safe.len(), 3);
	}

	#[test]
	fn ratio_raises_the_limit_for_large_trees() {
		// 200 deletions against 1000 tracked items: limit = max(10, 0.5*1000) = 500 -> allowed.
		let actions: Vec<_> = (0..200).map(|i| del(&format!("d{i}"))).collect();
		let decision = screen(actions, established(1000), DeleteGuard::default());
		assert!(decision.reason.is_none(), "within the ratio limit");
		assert_eq!(decision.safe.len(), 200);
	}

	#[test]
	fn holds_a_replace_create_together_with_its_paired_delete() {
		// A file<->dir type flip at "x": the delete and the create at the same path are one
		// operation. Holding the delete while releasing the create would just have the server
		// reject the create every pass, so both are held; unrelated actions still go through.
		let actions = vec![trash_dir("x"), create_remote_dir("x"), upload("unrelated")];
		let state = ScreenState {
			scan_complete: false,
			..established(100)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(decision.reason, Some(GuardReason::ScanIncomplete));
		assert_eq!(
			decision.safe,
			vec![upload("unrelated")],
			"unrelated safe actions still released"
		);
		assert_eq!(
			decision.held,
			vec![trash_dir("x"), create_remote_dir("x")],
			"the type flip is held as a unit"
		);
	}

	#[test]
	fn a_create_at_an_unrelated_path_is_never_held() {
		let actions = vec![del("gone"), create_remote_dir("other")];
		let state = ScreenState {
			scan_complete: false,
			..established(100)
		};
		let decision = screen(actions, state, DeleteGuard::default());
		assert_eq!(decision.safe, vec![create_remote_dir("other")]);
		assert_eq!(decision.held, vec![del("gone")]);
	}

	#[test]
	fn unlimited_guard_never_trips_on_volume_but_still_honors_an_incomplete_scan() {
		let actions: Vec<_> = (0..10_000).map(|i| del(&format!("d{i}"))).collect();
		assert!(
			screen(actions, established(1), DeleteGuard::unlimited())
				.reason
				.is_none()
		);

		let actions = vec![del("a")];
		let state = ScreenState {
			scan_complete: false,
			..established(1)
		};
		assert_eq!(
			screen(actions, state, DeleteGuard::unlimited()).reason,
			Some(GuardReason::ScanIncomplete),
			"unlimited still refuses deletions from an incomplete scan"
		);
	}
}
