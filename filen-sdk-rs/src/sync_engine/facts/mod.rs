//! The path-keyed facts a pass carries from one pass to the next, and the set of paths the next
//! pass owes a second look at.
//!
//! A whole-tree pass learns what it blocks by walking everything: a name the remote would reject, a
//! symlink read under its real path, an ignored root, a remote item the view cannot place. A
//! change-scoped pass reads the paths its changelists name and nothing else, so those facts have to
//! be CARRIED — and carried facts go stale. [`PairFacts`] is the one place that state lives, with
//! one rule holding it together:
//!
//! > a fact is dropped only where this pass has evidence that covers it, and the evidence that
//! > drops it is the same read that records what replaces it.
//!
//! So every prune here happens inside a merge. There is no way to drop a subtree's facts without
//! handing over the walk, the observation or the view that re-answers for it, which is what keeps a
//! path from silently losing its block. The recompute itself is not new logic: a dirty subtree's
//! facts come out of [`scan_subtree`](super::scan::scan_subtree) — the whole-tree walker started at
//! that directory — and the remote's out of [`RemoteView`] and
//! [`plan::unknown_remote_paths`](super::plan::unknown_remote_paths), the very functions a whole
//! pass uses. This module only says where they apply and what they replace.
//!
//! # Which way a mistake here goes
//!
//! Facts BLOCK; they never produce a node. A fact kept too long defers an action (the path stays
//! blocked until a full pass clears it); a fact dropped too early allows an action the previous
//! pass refused. Neither invents an absence — that is the local observation's and the remote
//! delta's business, and invariant I1 lives there — but the second direction is the one that
//! surprises a user, so pruning is deliberately the narrower half: a walk that came back INCOMPLETE
//! prunes nothing, and a dirty path this pass got no reading for keeps every fact it had.
//!
//! # What is NOT carried, and why
//!
//! - `has_collisions`: a pass that finds one REFUSES, and a refusal forces the next pass full
//!   (`next_pass_scope`), which recomputes it. Carrying the bool would be state nothing ever reads.
//! - the remote half of `ignore_blocked` (`RemoteRules::blocked`) and `remote_rule_errors`: both
//!   come from `SyncEngine::remote_rules`, which reads the rule files the BASELINE names and so
//!   costs the same on a change-scoped pass as on a whole one. Recomputed, not carried; the caller
//!   unions them over [`PairFacts::ignore_blocked`], which holds only what a local walk found.
//! - `failures`: `BaselineStore::failures` is one indexed read of a table the size of the pair's
//!   failing paths, not a whole-tree read. It stays a per-pass read — and it is an INPUT to
//!   [`carry_over`] rather than carried state.
//! - `ignored_default_untracked`: a count, logged and never reported.
//!
//! # Directory moves
//!
//! A pass re-keys everything it blocks along with the directory moves it folds
//! (`Prepared::fold_dir_moves`). Carried facts are that same state one pass later, so they follow
//! the same re-keying: the fields here are `pub(super)` exactly so the fold can re-key them in
//! place, as it already does for the loose fields they replace.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use super::{
	baseline::{PathFailure, SyncedPaths},
	ignore::IgnoreDecision,
	observe::{LocalObservation, LocalObservations},
	outcome::{PlannedConflict, UnsyncablePath, UnsyncableReason},
	plan::{self, RemoteView},
	rows::Baseline,
	scan::LocalScan,
};

/// Everything a pass blocks, reports or withholds, keyed by path and carried between passes.
///
/// Each field is the one `Prepared` holds today under the same name, so a pass reads them exactly
/// as it does now; what changes is that a change-scoped pass no longer re-derives them all from a
/// whole-tree read.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct PairFacts {
	/// Paths whose name the remote would reject, with the validator's message
	/// ([`LocalScan::invalid_names`]).
	pub(super) invalid_names: BTreeMap<String, String>,
	/// Symlinked directories whose target lies inside the root, to that target's own path
	/// ([`LocalScan::aliased_dirs`]).
	pub(super) aliased_dirs: BTreeMap<String, String>,
	/// The top-most LOCAL paths the ignore rules hide, with the deciding rule.
	pub(super) ignored_local: BTreeMap<String, IgnoreDecision>,
	/// Directories whose own `.filenignore` a local walk could not use. The REMOTE rules' blocked
	/// set is recomputed every pass and unioned over this by the caller (see the module docs).
	pub(super) ignore_blocked: BTreeSet<String>,
	/// The top-most REMOTE paths the ignore rules hide, with the deciding rule.
	pub(super) ignored_remote: BTreeMap<String, IgnoreDecision>,
	/// Synced paths whose remote item exists but is out of the view, with why. Blocked and
	/// reported, never read as a deletion.
	pub(super) unknown_remote: BTreeMap<String, UnsyncableReason>,
	/// Remote items out of the view that no baseline row names: reported only.
	pub(super) never_synced_remote: Vec<UnsyncablePath>,
}

impl PairFacts {
	/// Record what one local walk found, dropping what it replaces: the facts at or under `at`,
	/// which is `""` for a whole-tree scan and the dirty directory for a subtree walk.
	///
	/// An INCOMPLETE walk drops nothing. It may have missed the very entry a carried fact is about,
	/// and re-allowing a path on the strength of a read that admits it could not see everything is
	/// the one direction this module does not take (see the module docs).
	pub(super) fn merge_local_scan(&mut self, at: &str, scan: &LocalScan) {
		if scan.complete {
			self.prune_local(at);
		}
		clone_into(&mut self.invalid_names, &scan.invalid_names);
		clone_into(&mut self.aliased_dirs, &scan.aliased_dirs);
		clone_into(&mut self.ignored_local, &scan.ignored);
		self.ignore_blocked
			.extend(scan.ignore_blocked.iter().cloned());
	}

	/// Record what a change-scoped pass re-observed locally: one merge per observation, each
	/// replacing the facts of the subtree its evidence covers.
	///
	/// A dirty path with NO observation is not touched here. This pass got no local reading for it
	/// (see [`observe_local`](super::observe::observe_local)), so its facts stand and the next pass
	/// looks again.
	pub(super) fn observe_local(&mut self, observed: &LocalObservations) {
		for (at, observation) in &observed.observed {
			match observation {
				LocalObservation::Dir(scan) => self.merge_local_scan(at, scan),
				// A file answers for its own path alone — and for the subtree a directory that USED
				// to stand there left behind, which is why it prunes like the rest.
				LocalObservation::File {
					node,
					name_rejection,
				} => {
					self.prune_local(at);
					// Load-bearing: without it a rename of a synced file INTO a rejected name is
					// planned as a push that can only fail, every pass, forever.
					if let Some(detail) = name_rejection {
						self.invalid_names
							.insert(node.rel_path.clone(), detail.clone());
					}
				}
				// Nothing is there, so nothing at or under it is blocked any more.
				LocalObservation::Absent(absence) => self.prune_local(absence.path()),
				LocalObservation::Hidden(decision) => {
					self.prune_local(at);
					self.ignored_local.insert(at.clone(), decision.clone());
				}
			}
		}
		// The ancestors of the dirty paths, which no single observation answers for: a rule file
		// that could not be read blocks its directory whoever walked past it.
		self.ignore_blocked
			.extend(observed.ignore_blocked.iter().cloned());
	}

	/// Record what the remote side of a pass found, dropping what it replaces: the facts at or
	/// under each path in `touched`.
	///
	/// `touched` is [`RemoteObservation::touched`](super::remote::RemoteObservation::touched) for a
	/// change-scoped pass and `[""]` for a whole-tree one, whose view answers for everything. The
	/// view must already be FILTERED — `ignored`/`held_paths`/`skipped` are what it says after
	/// `RemoteView::filter`, which is where a pass reads them today.
	pub(super) fn merge_remote_view(
		&mut self,
		touched: &BTreeSet<String>,
		view: &RemoteView,
		baseline: &Baseline,
		synced: &SyncedPaths,
	) {
		for at in touched {
			// A whole read answers for every path, so dropping `at`'s whole subtree is the whole of
			// the truth. A change-scoped one answers only for the paths it DECIDED — `filter_changed`
			// records a root among those and no other — while the prune still takes the subtree, so
			// a carried root strictly under `at` would go on the strength of a reading that never
			// looked there. What goes with it is the withholding that keeps a deletion off ignored
			// content (`engine::Withholding`) and the root's place in `last_ignored`.
			//
			// Re-supplied where the baseline still names a row at or under the root: those rows are
			// the evidence that what the rule hides is still there and still untracked — the very
			// rows `untrack_ignored` retries its `delete_subtrees` on. A root with NO row under it
			// is dropped as before: nothing here says it still exists, and carrying it on would
			// withhold its directory's deletion for as long as the pair lasted.
			let carried: Vec<(String, IgnoreDecision)> = if at.is_empty() {
				Vec::new()
			} else {
				plan::under_dir(&self.ignored_remote, at)
					.filter(|(root, _)| baseline.tracked(root, true))
					.map(|(root, decision)| (root.clone(), decision.clone()))
					.collect()
			};
			self.prune_remote(at);
			self.ignored_remote.extend(carried);
		}
		clone_into(&mut self.ignored_remote, &view.ignored);
		// The same read a whole pass makes, over whatever this one skipped: a derived view skips
		// nothing (it asks for a full pass instead), so on a change-scoped pass this adds nothing
		// and the prune above is the whole of the update.
		let (unknown, never_synced) = plan::unknown_remote_paths(synced, &view.skipped);
		self.unknown_remote.extend(unknown);
		for report in never_synced {
			if !self.never_synced_remote.contains(&report) {
				self.never_synced_remote.push(report);
			}
		}
	}

	/// Drop every LOCAL fact at or under `at`.
	fn prune_local(&mut self, at: &str) {
		prune_map(&mut self.invalid_names, at);
		prune_map(&mut self.aliased_dirs, at);
		prune_map(&mut self.ignored_local, at);
		prune_set(&mut self.ignore_blocked, at);
	}

	/// Drop every REMOTE fact at or under `at`. Kept apart from the local half because the two
	/// sides' evidence is: a path that is gone from disk says nothing about the remote item there,
	/// and an item the cache moved says nothing about what sits at its old path locally.
	fn prune_remote(&mut self, at: &str) {
		prune_map(&mut self.ignored_remote, at);
		prune_map(&mut self.unknown_remote, at);
		prune_reports(&mut self.never_synced_remote, at);
	}
}

/// Copy `from` over `into`, key by key.
fn clone_into<V: Clone>(into: &mut BTreeMap<String, V>, from: &BTreeMap<String, V>) {
	into.extend(
		from.iter()
			.map(|(path, value)| (path.clone(), value.clone())),
	);
}

/// Drop every entry at or under `at`, found by seeking to that subtree's key range rather than by
/// testing every key ([`plan::subtree_bounds`]). The pair root takes the whole map with it: `""`
/// names no subtree by that rule, and a walk of the root answers for everything.
fn prune_map<V>(map: &mut BTreeMap<String, V>, at: &str) {
	if at.is_empty() {
		map.clear();
		return;
	}
	map.remove(at);
	let under: Vec<String> = plan::under_dir(map, at)
		.map(|(path, _)| path.clone())
		.collect();
	for path in under {
		map.remove(&path);
	}
}

/// [`prune_map`] for a set of paths.
fn prune_set(set: &mut BTreeSet<String>, at: &str) {
	if at.is_empty() {
		set.clear();
		return;
	}
	set.remove(at);
	let under: Vec<String> = set.range(plan::subtree_bounds(at)).cloned().collect();
	for path in under {
		set.remove(&path);
	}
}

/// [`prune_map`] for the report lines, which are a list rather than a map: one item can put a line
/// at a path another item already named, so they are not keyed.
fn prune_reports(reports: &mut Vec<UnsyncablePath>, at: &str) {
	if at.is_empty() {
		reports.clear();
		return;
	}
	reports.retain(|report| report.rel_path != at && !plan::is_under(&report.rel_path, at));
}

/// The paths the NEXT pass owes a look at whatever its changelists say — the carry-over set of the
/// optimization plan's section 3.6 — as a list that may name a path more than once (an action that
/// is not a move names its one path as both endpoints).
/// [`PairChanges::note_owed`](super::changes::PairChanges::note_owed) makes the set, and decides
/// whether to keep one at all.
///
/// Drawn at the end of every pass out of that pass's own plan, and consumed as an INPUT to the next
/// pass's dirty set. What is in it:
///
/// - every path this pass's plan released past the guard (`released`: both endpoints of each
///   action, a move's two included), whether the action was applied, failed or never reached. The
///   pass reads them out of its staged plan only until there are more than
///   [`PairChanges::note_owed`](super::changes::PairChanges::note_owed) keeps, so a plan of any
///   size costs that set and no more. A superset of the work left owing is the safe direction —
///   re-observing a path that was applied costs a stat and plans nothing — and it is what lets an
///   interrupted pass find its remainder and a failed action be re-planned. An APPLIED path is in
///   it for a reason of its own: a pass whose baseline write did not land (`store_failed`) carried
///   the act out and has no row saying so, and only a fresh reading of that path corrects it.
/// - every path this pass DEFERRED because the cache was mid-transition there (`held_remote`).
/// - every path with a failure streak, expired or not. Not only the ones whose retry interval runs
///   out next: the set is rebuilt from each pass's own read, so a path filtered out here is one no
///   later pass would look at either, and its retry would never come. The set is bounded by the
///   failing paths, which is why keeping them all is cheap.
///
/// What it deliberately does NOT walk is the baseline. A non-`Synced` row, and a row that records
/// one side only, are already put in the dirty set by `derive::from_baseline`, in the same pass
/// that cannot derive them — a second O(tree) walk here would only ask the same question again.
///
/// Nor does it carry the batch the guard HELD. A hold sends the next pass that runs to a whole
/// read ([`FullPassReason::DeletionHold`](super::FullPassReason::DeletionHold)), which looks at
/// those paths and reproduces the batch from both trees; owing them as well would only keep a pair
/// whose one piece of business is the hold from ever being idle, so every wake would re-read both
/// trees to hold the identical batch again.
///
/// # How it must be consumed
///
/// It is not a changelist entry. Feeding it through the capped local list is the mistake the first
/// attempt at this made: the cap is a quarter of the tree size as the pair last RECORDED it, which
/// is zero before a pair's first sync, so a plan of any size collapsed the next pass to a
/// whole-tree read and reported that pass as owing work it had in fact applied. It is bounded
/// instead by the absolute cap the changelists' own cap tops out at: a plan that names more paths
/// than that leaves no set behind, and the next pass reads both sides whole
/// ([`FullPassReason::CarriedOver`](super::FullPassReason::CarriedOver)). Within that bound the set
/// is not a reason on its own: the full-pass triggers of section 3.5 are unchanged by it, and an
/// interrupted pass still forces the next one full in this round.
pub(super) fn carry_over<'a>(
	released: impl IntoIterator<Item = &'a str>,
	conflicts: &'a [PlannedConflict],
	held_remote: &'a BTreeSet<String>,
	failures: &'a HashMap<String, PathFailure>,
) -> impl Iterator<Item = &'a str> {
	released
		.into_iter()
		.chain(conflicts.iter().map(|conflict| conflict.rel_path.as_str()))
		.chain(held_remote.iter().map(String::as_str))
		.chain(failures.keys().map(String::as_str))
}

#[cfg(test)]
mod tests;
