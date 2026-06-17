//! The mass-delete guard: a safety screen between the reconciler's plan and the apply layer.
//!
//! Two distinct failure modes warrant holding deletions back:
//! 1. an INCOMPLETE local scan (a missing/unreadable root, an unreadable subtree, a name
//!    collision) — a half-read source must never be read as "everything was deleted"; and
//! 2. a deletion VOLUME beyond a configurable threshold — a legitimately large delete is allowed
//!    up to a point, past which it is held for explicit approval.
//!
//! When the guard trips it holds ONLY the deletions (non-destructive creates/transfers still
//! apply) and reports them so the caller can approve the batch or adjust the threshold. Pure and
//! synchronous so the policy is fully unit-testable.

use super::plan::SyncAction;

/// Deletion-volume policy. The limit for a pass is `max(floor, ratio * tracked_items)`: small
/// deletes are always allowed (the floor), and beyond that up to a fraction of the tracked set.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DeleteGuard {
	/// Always allow at least this many deletions, regardless of the ratio.
	pub(crate) floor: usize,
	/// Allow deletions up to this fraction of the tracked-item count (beyond the floor).
	pub(crate) ratio: f64,
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
pub(crate) enum GuardReason {
	/// The local scan did not complete, so apparent deletions may be phantom (an unmounted root,
	/// an unreadable subtree). All deletions are held unconditionally.
	ScanIncomplete,
	/// The deletion count exceeded the configured limit for the tracked-item count.
	ExceededThreshold { deletions: usize, limit: usize },
}

/// The screened plan: the actions safe to apply now, plus any deletions held back (and why).
#[derive(Debug)]
pub(crate) struct GuardDecision {
	pub(crate) safe: Vec<SyncAction>,
	pub(crate) held: Vec<SyncAction>,
	pub(crate) reason: Option<GuardReason>,
}

/// Screen a plan: hold its deletions if the scan was incomplete or the deletion volume exceeds the
/// guard's limit; otherwise pass everything through. `tracked` is how many items the pair's
/// baseline currently holds (the denominator for the ratio).
pub(crate) fn screen(
	actions: Vec<SyncAction>,
	scan_complete: bool,
	tracked: usize,
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

	let reason = if !scan_complete {
		Some(GuardReason::ScanIncomplete)
	} else {
		let limit = guard.limit(tracked);
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
			let (held, safe) = actions.into_iter().partition(SyncAction::is_delete);
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

	#[test]
	fn passes_everything_when_under_the_limit_and_scan_complete() {
		let actions = vec![upload("a"), del("b")];
		let decision = screen(actions, true, 100, DeleteGuard::default());
		assert_eq!(decision.safe.len(), 2);
		assert!(decision.held.is_empty());
		assert!(decision.reason.is_none());
	}

	#[test]
	fn holds_all_deletions_when_the_scan_is_incomplete() {
		// Even a single deletion is held if the scan could not be trusted — but non-destructive
		// actions still pass.
		let actions = vec![upload("keep"), del("a"), del("b")];
		let decision = screen(actions, false, 10_000, DeleteGuard::default());
		assert_eq!(decision.reason, Some(GuardReason::ScanIncomplete));
		assert_eq!(decision.safe, vec![upload("keep")]);
		assert_eq!(decision.held.len(), 2, "both deletions held");
	}

	#[test]
	fn holds_deletions_past_the_volume_threshold() {
		// 30 deletions against 20 tracked exceeds max(floor=10, 0.5*20=10) = 10.
		let mut actions: Vec<_> = (0..30).map(|i| del(&format!("d{i}"))).collect();
		actions.push(upload("safe"));
		let decision = screen(actions, true, 20, DeleteGuard::default());
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
		let decision = screen(actions, true, 20, DeleteGuard::default());
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
		let decision = screen(actions, true, 3, DeleteGuard::default());
		assert!(decision.reason.is_none(), "2 deletions is below the floor");
		assert_eq!(decision.safe.len(), 3);
	}

	#[test]
	fn ratio_raises_the_limit_for_large_trees() {
		// 200 deletions against 1000 tracked items: limit = max(10, 0.5*1000) = 500 -> allowed.
		let actions: Vec<_> = (0..200).map(|i| del(&format!("d{i}"))).collect();
		let decision = screen(actions, true, 1000, DeleteGuard::default());
		assert!(decision.reason.is_none(), "within the ratio limit");
		assert_eq!(decision.safe.len(), 200);
	}

	#[test]
	fn unlimited_guard_never_trips_on_volume_but_still_honors_an_incomplete_scan() {
		let actions: Vec<_> = (0..10_000).map(|i| del(&format!("d{i}"))).collect();
		assert!(
			screen(actions, true, 1, DeleteGuard::unlimited())
				.reason
				.is_none()
		);

		let actions = vec![del("a")];
		assert_eq!(
			screen(actions, false, 1, DeleteGuard::unlimited()).reason,
			Some(GuardReason::ScanIncomplete),
			"unlimited still refuses deletions from an incomplete scan"
		);
	}
}
