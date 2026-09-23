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
	baseline::PathFailure,
	ignore::IgnoreDecision,
	observe::{LocalObservation, LocalObservations},
	outcome::{PlannedConflict, UnsyncablePath, UnsyncableReason},
	plan::{self, RemoteView, SyncAction},
	scan::LocalScan,
	tree::Baseline,
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
	) {
		for at in touched {
			// A whole read answers for every path, so dropping `at`'s whole subtree is the whole of
			// the truth. A change-scoped one answers only for the paths it DECIDED — `filter_changed`
			// records a root among those and no other — while the prune still takes the subtree, so
			// a carried root strictly under `at` would go on the strength of a reading that never
			// looked there. What goes with it is the withholding that keeps a deletion off ignored
			// content (`withhold_deletions_over_unreachable`) and the root's place in `last_ignored`.
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
		let (unknown, never_synced) = plan::unknown_remote_paths(baseline, &view.skipped);
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
/// optimization plan's section 3.6.
///
/// Built at the end of every pass out of that pass's own plan, and consumed as an INPUT to the next
/// pass's dirty set. What is in it:
///
/// - every path this pass's plan named, both endpoints of a move included, whether the action was
///   applied, failed, held by the guard or never reached. A superset of the work left owing is the
///   safe direction — re-observing a path that was applied costs a stat and plans nothing — and it
///   is what lets an interrupted pass find its remainder, a failed action be re-planned, and a held
///   deletion batch reproduce the token the caller was handed. An APPLIED path is in it for a
///   reason of its own: a pass whose baseline write did not land (`store_failed`) carried the act
///   out and has no row saying so, and only a fresh reading of that path corrects it.
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
/// # How it must be consumed
///
/// It is not a changelist entry. Feeding it through the capped local list is the mistake the first
/// attempt at this made: the cap is a quarter of the tree size as the pair last RECORDED it, which
/// is zero before a pair's first sync, so a plan of any size collapsed the next pass to a
/// whole-tree read and reported that pass as owing work it had in fact applied. The set is also not
/// a reason on its own: the full-pass triggers of section 3.5 are unchanged by it, and an
/// interrupted pass still forces the next one full in this round.
pub(super) fn carry_over<'a>(
	planned: impl IntoIterator<Item = &'a SyncAction>,
	conflicts: &[PlannedConflict],
	held_remote: &BTreeSet<String>,
	failures: &HashMap<String, PathFailure>,
) -> BTreeSet<String> {
	let mut owed: BTreeSet<String> = BTreeSet::new();
	for action in planned {
		let (from, to) = action.endpoints();
		owed.insert(from.to_owned());
		owed.insert(to.to_owned());
	}
	owed.extend(
		conflicts
			.iter()
			.map(|conflict| conflict.rel_path.clone())
			.chain(held_remote.iter().cloned())
			.chain(failures.keys().cloned()),
	);
	owed
}

#[cfg(test)]
mod tests {
	use std::{fs, path::Path};

	use super::super::side::Side;

	use uuid::Uuid;

	use super::*;
	use crate::sync_engine::{
		baseline::{BaselineEntry, BaselineState, NodeKind},
		ignore::{FILENIGNORE, IgnoreLevel, IgnoreRules},
		observe::observe_local,
		outcome::PlannedNodeKind,
		scan::{RuleFiles, scan_local},
	};

	fn temp_root() -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_facts_test_{}", Uuid::new_v4()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// A complete walk of an empty directory: every fact map empty, so what a merge with it leaves
	/// behind is exactly what the prune spared.
	fn empty_scan() -> LocalScan {
		let root = temp_root();
		let scan = scan_local(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		)
		.0;
		assert!(scan.complete);
		fs::remove_dir_all(&root).ok();
		scan
	}

	fn whole_tree_facts(root: &Path) -> PairFacts {
		let scan = scan_local(
			root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		)
		.0;
		assert!(scan.complete, "{:?}", scan.errors);
		let mut facts = PairFacts::default();
		facts.merge_local_scan("", &scan);
		facts
	}

	/// Re-observe `dirty` under `root` the way a change-scoped pass does, with no baseline.
	fn observe(root: &Path, dirty: &[&str]) -> LocalObservations {
		let dirty: BTreeSet<String> = dirty.iter().map(|path| (*path).to_owned()).collect();
		observe_local(
			root,
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			&dirty,
		)
		.0
	}

	fn local_facts_at(paths: &[&str]) -> PairFacts {
		PairFacts {
			invalid_names: paths
				.iter()
				.map(|path| ((*path).to_owned(), "rejected".to_owned()))
				.collect(),
			ignore_blocked: paths.iter().map(|path| (*path).to_owned()).collect(),
			..PairFacts::default()
		}
	}

	fn keys<V>(map: &BTreeMap<String, V>) -> Vec<&str> {
		map.keys().map(String::as_str).collect()
	}

	fn listed(set: &BTreeSet<String>) -> Vec<&str> {
		set.iter().map(String::as_str).collect()
	}

	fn decision(pattern: &str) -> IgnoreDecision {
		IgnoreDecision {
			level: IgnoreLevel::User,
			pattern: pattern.to_owned(),
		}
	}

	/// A prune is a SUBTREE operation, not a string-prefix one. `d/` is where the subtree starts and
	/// `d0` is where it ends (`/` is 0x2F, `0` is 0x30), so the siblings a prefix comparison would
	/// swallow — `d.txt` below the range, `d0` and `d2/x.txt` above it, `dd/x.txt` further still —
	/// all survive a walk of `d`.
	#[test]
	fn a_prune_takes_the_subtree_and_not_the_siblings_around_it() {
		let mut facts = local_facts_at(&[
			"d",
			"d/x.txt",
			"d/deep/y.txt",
			"d.txt",
			"d0",
			"d2/x.txt",
			"dd/x.txt",
			"e",
		]);

		facts.merge_local_scan("d", &empty_scan());

		assert_eq!(
			keys(&facts.invalid_names),
			vec!["d.txt", "d0", "d2/x.txt", "dd/x.txt", "e"],
			"only the walked directory and what is under it is re-answered for"
		);
		assert_eq!(
			listed(&facts.ignore_blocked),
			vec!["d.txt", "d0", "d2/x.txt", "dd/x.txt", "e"],
			"a set of paths prunes by the same bounds as a map of them"
		);
	}

	/// The pair root is the one path with no subtree bounds of its own: a walk of it answers for
	/// everything, so it replaces every local fact there is.
	#[test]
	fn a_whole_tree_walk_replaces_every_local_fact() {
		let mut facts = local_facts_at(&["d", "d/x.txt", "e"]);

		facts.merge_local_scan("", &empty_scan());

		assert!(facts.invalid_names.is_empty(), "{:?}", facts.invalid_names);
		assert!(
			facts.ignore_blocked.is_empty(),
			"{:?}",
			facts.ignore_blocked
		);
	}

	/// An INCOMPLETE walk may have missed the very entry a carried fact is about, so it prunes
	/// nothing: the facts it did find are recorded over the carried ones, and the rest stand until a
	/// pass can see the subtree whole.
	#[test]
	fn an_incomplete_walk_drops_no_fact() {
		let mut facts = local_facts_at(&["d/gone.txt"]);
		let mut scan = empty_scan();
		scan.complete = false;

		facts.merge_local_scan("d", &scan);

		assert_eq!(keys(&facts.invalid_names), vec!["d/gone.txt"]);
	}

	/// The recompute is the whole-tree computation applied to a subtree: carrying the untouched
	/// facts and re-walking the dirty directory has to land on exactly what a whole-tree walk of the
	/// changed tree would have recorded.
	#[test]
	fn a_subtree_recompute_agrees_with_the_whole_tree_computation() {
		fn build(root: &Path) {
			fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
			fs::write(root.join("keep.txt"), b"k").unwrap();
			fs::create_dir(root.join("d")).unwrap();
			fs::write(root.join("d").join("ok.txt"), b"o").unwrap();
			// Reserved device name: creatable here, refused by the SDK's own name validator.
			fs::write(root.join("d").join("CON"), b"c").unwrap();
			fs::write(root.join("d").join("x.log"), b"l").unwrap();
			fs::create_dir(root.join("e")).unwrap();
			fs::write(root.join("e").join("CON"), b"c").unwrap();
			fs::write(root.join("e").join("y.log"), b"l").unwrap();
		}
		// Every shape of change to one directory: a blocked path that goes away, a new one that
		// appears, and a newly hidden entry.
		fn change(root: &Path) {
			fs::remove_file(root.join("d").join("CON")).unwrap();
			fs::create_dir(root.join("d").join("bad.")).unwrap();
			fs::write(root.join("d").join("bad.").join("inner.txt"), b"i").unwrap();
			fs::write(root.join("d").join("z.log"), b"l").unwrap();
		}

		let carried_root = temp_root();
		build(&carried_root);
		let mut carried = whole_tree_facts(&carried_root);
		change(&carried_root);
		carried.observe_local(&observe(&carried_root, &["d"]));

		let whole_root = temp_root();
		build(&whole_root);
		change(&whole_root);
		let whole = whole_tree_facts(&whole_root);

		assert_eq!(
			keys(&whole.invalid_names),
			vec!["d/bad.", "e/CON"],
			"the tree under test has to carry a fact on each side of the dirty directory"
		);
		assert_eq!(carried, whole);

		fs::remove_dir_all(&carried_root).ok();
		fs::remove_dir_all(&whole_root).ok();
	}

	/// A re-observed FILE answers for its own path: a name the remote rejects is recorded there —
	/// without which a rename INTO such a name is planned as a push that can only fail — and a path
	/// that is gone takes its fact with it.
	#[test]
	fn a_re_observed_file_records_a_rejected_name_and_an_absent_one_drops_it() {
		let root = temp_root();
		fs::create_dir(root.join("d")).unwrap();
		fs::write(root.join("d").join("CON"), b"c").unwrap();

		let mut facts = PairFacts::default();
		facts.observe_local(&observe(&root, &["d/CON"]));
		assert!(
			facts.invalid_names["d/CON"].contains("reserved"),
			"the validator's own reason: {:?}",
			facts.invalid_names
		);

		fs::rename(root.join("d").join("CON"), root.join("d").join("ok.txt")).unwrap();
		facts.observe_local(&observe(&root, &["d/CON", "d/ok.txt"]));
		assert!(
			facts.invalid_names.is_empty(),
			"the path the file left is absent, and the one it took is syncable: {:?}",
			facts.invalid_names
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A path the rules hide is recorded as an ignored root by whichever read reached it — the
	/// observation of the path itself as much as a walk of the directory above it.
	#[test]
	fn a_hidden_path_is_recorded_as_an_ignored_root() {
		let root = temp_root();
		fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
		fs::create_dir(root.join("d")).unwrap();
		fs::write(root.join("d").join("x.log"), b"l").unwrap();

		let mut facts = PairFacts::default();
		facts.observe_local(&observe(&root, &["d/x.log"]));

		assert_eq!(keys(&facts.ignored_local), vec!["d/x.log"]);
		assert_eq!(facts.ignored_local["d/x.log"].pattern, "*.log");

		fs::remove_dir_all(&root).ok();
	}

	/// The remote facts are pruned by what the DELTA touched and by nothing else: a path the cache
	/// announced is re-answered for by the view, and the local side's evidence — which says nothing
	/// about the remote item at a path — leaves them all standing.
	#[test]
	fn remote_facts_are_pruned_by_the_paths_the_delta_touched() {
		let mut facts = PairFacts {
			invalid_names: BTreeMap::from([("a/x.txt".to_owned(), "rejected".to_owned())]),
			ignored_remote: BTreeMap::from([
				("a".to_owned(), decision("a")),
				("a/b".to_owned(), decision("b")),
				("a2".to_owned(), decision("a2")),
			]),
			unknown_remote: BTreeMap::from([
				("a/b".to_owned(), UnsyncableReason::RemoteUndecodable),
				("b".to_owned(), UnsyncableReason::RemoteBrokenParent),
			]),
			never_synced_remote: vec![
				UnsyncablePath::new("a/c", UnsyncableReason::RemoteBrokenParent),
				UnsyncablePath::new("a2", UnsyncableReason::RemoteUndecodable),
			],
			..PairFacts::default()
		};
		let view = RemoteView {
			nodes: Side::default(),
			has_collisions: false,
			held_paths: BTreeSet::from(["c".to_owned()]),
			skipped: Vec::new(),
			ignored: BTreeMap::from([("c".to_owned(), decision("c"))]),
			ignored_default_untracked: 0,
		};

		facts.merge_remote_view(
			&BTreeSet::from(["a".to_owned()]),
			&view,
			&Baseline::default(),
		);

		assert_eq!(keys(&facts.ignored_remote), vec!["a2", "c"]);
		assert_eq!(keys(&facts.unknown_remote), vec!["b"]);
		assert_eq!(
			facts
				.never_synced_remote
				.iter()
				.map(|report| report.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["a2"]
		);
		assert_eq!(
			keys(&facts.invalid_names),
			vec!["a/x.txt"],
			"a remote read is no evidence about the local path"
		);
	}

	/// A carried ignored root strictly UNDER a touched path survives the prune for as long as the
	/// baseline names a row at or under it, and goes with the prune once none is left.
	///
	/// The prune takes `at`'s whole subtree, but a change-scoped view re-derives the roots among the
	/// paths that pass DECIDED and no others (`RemoteView::filter_changed`), so a root it never
	/// looked at would be dropped on the strength of a reading that never covered it — and with it
	/// the withholding that keeps a deletion off ignored content, and the root's place in
	/// `last_ignored`. Rows at or under the root are the evidence that what the rule hides is still
	/// there: they are what an `untrack_ignored` failed to delete, and what it retries.
	#[test]
	fn a_carried_ignored_root_under_a_touched_path_lives_while_rows_sit_under_it() {
		let row = |rel_path: &str| BaselineEntry {
			rel_path: rel_path.to_owned(),
			kind: NodeKind::File,
			remote_uuid: Some(Uuid::from_u128(11)),
			content_hash: None,
			size: None,
			local_mtime: None,
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		};
		// A derived view that re-derived NOTHING: the pass decided only `a`, so `filter_changed`
		// recorded no root, and whatever survives here survives as a carried fact.
		let derived_view = || RemoteView {
			nodes: Side::default(),
			has_collisions: false,
			held_paths: BTreeSet::new(),
			skipped: Vec::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		};
		let carried = || PairFacts {
			ignored_remote: BTreeMap::from([
				("a/keep".to_owned(), decision("keep")),
				("a/gone".to_owned(), decision("gone")),
			]),
			..PairFacts::default()
		};

		// `a/keep` still has a row under it; `a/gone` has none.
		let rows = Baseline::from_rows([row("a/keep/hidden.txt")]);
		let mut scoped = carried();
		scoped.merge_remote_view(&BTreeSet::from(["a".to_owned()]), &derived_view(), &rows);
		assert_eq!(
			keys(&scoped.ignored_remote),
			vec!["a/keep"],
			"the root with rows under it is re-supplied; the one with none is pruned as before"
		);

		// No rows at all: the prune stands exactly as it did before this was re-supplied at all.
		let mut untracked = carried();
		untracked.merge_remote_view(
			&BTreeSet::from(["a".to_owned()]),
			&derived_view(),
			&Baseline::default(),
		);
		assert!(
			untracked.ignored_remote.is_empty(),
			"nothing here says ignored content with no row is still there: {:?}",
			untracked.ignored_remote
		);

		// A whole read answers for every path, so its view REPLACES the carried roots, rows or not.
		let mut whole = carried();
		whole.merge_remote_view(&BTreeSet::from([String::new()]), &derived_view(), &rows);
		assert!(
			whole.ignored_remote.is_empty(),
			"a whole read that found no ignored root means there is none: {:?}",
			whole.ignored_remote
		);
	}

	/// The carry-over set names every disposition a plan can leave behind, both endpoints of a move
	/// included, plus the paths the pass deferred and every path carrying a failure streak.
	#[test]
	fn the_carry_over_set_names_every_disposition_of_a_plan() {
		let applied = [
			SyncAction::UploadFile {
				rel_path: "up.txt".to_owned(),
			},
			SyncAction::MoveRemote {
				from_path: "old/name.txt".to_owned(),
				to_path: "new/name.txt".to_owned(),
				kind: NodeKind::File,
				remote_uuid: Uuid::from_u128(7),
			},
		];
		let held = [SyncAction::TrashRemote {
			rel_path: "gone.txt".to_owned(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(8),
		}];
		let conflicts = [PlannedConflict::new(
			"clash.txt",
			Some(PlannedNodeKind::File),
			Some(PlannedNodeKind::File),
		)];
		let failures = HashMap::from([
			(
				"stuck.txt".to_owned(),
				PathFailure {
					attempts: 3,
					last_error: "no".to_owned(),
					last_failure_at: 0,
				},
			),
			(
				"flaky.txt".to_owned(),
				PathFailure {
					attempts: 1,
					last_error: "no".to_owned(),
					last_failure_at: 0,
				},
			),
		]);

		let owed = carry_over(
			applied.iter().chain(&held),
			&conflicts,
			&BTreeSet::from(["midflight.txt".to_owned()]),
			&failures,
		);

		assert_eq!(
			listed(&owed),
			vec![
				"clash.txt",
				"flaky.txt",
				"gone.txt",
				"midflight.txt",
				"new/name.txt",
				"old/name.txt",
				"stuck.txt",
				"up.txt",
			],
			"a move owes BOTH its endpoints, and a streak below the block threshold is owed too — \
			 nothing else would ever look at it again"
		);
	}
}
