use std::{fs, path::Path};

use super::super::side::Side;

use uuid::Uuid;

use super::*;
use crate::sync_engine::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
	ignore::{FILENIGNORE, IgnoreLevel, IgnoreRules},
	observe::observe_local,
	outcome::PlannedNodeKind,
	plan::SyncAction,
	scan::{PairRoot, RuleFiles, ScanRoot, scan_local},
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
		ScanRoot {
			path: &root,
			pair_root: PairRoot::Directory,
		},
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
		ScanRoot {
			path: root,
			pair_root: PairRoot::Directory,
		},
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
		PairRoot::Directory,
		&Baseline::default(),
		IgnoreRules::default(),
		&RuleFiles::Read,
		&dirty,
	)
	.unwrap()
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
		// A leading space: stored as written on every platform, refused by the SDK's own name
		// validator.
		fs::write(root.join("d").join(" c.txt"), b"c").unwrap();
		fs::write(root.join("d").join("x.log"), b"l").unwrap();
		fs::create_dir(root.join("e")).unwrap();
		fs::write(root.join("e").join(" c.txt"), b"c").unwrap();
		fs::write(root.join("e").join("y.log"), b"l").unwrap();
	}
	// Every shape of change to one directory: a blocked path that goes away, a new one that
	// appears, and a newly hidden entry.
	fn change(root: &Path) {
		fs::remove_file(root.join("d").join(" c.txt")).unwrap();
		fs::create_dir(root.join("d").join(" bad")).unwrap();
		fs::write(root.join("d").join(" bad").join("inner.txt"), b"i").unwrap();
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
		vec!["d/ bad", "e/ c.txt"],
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
	fs::write(root.join("d").join(" c.txt"), b"c").unwrap();

	let mut facts = PairFacts::default();
	facts.observe_local(&observe(&root, &["d/ c.txt"]));
	assert!(
		facts.invalid_names["d/ c.txt"].contains("start with a space"),
		"the validator's own reason: {:?}",
		facts.invalid_names
	);

	fs::rename(root.join("d").join(" c.txt"), root.join("d").join("ok.txt")).unwrap();
	facts.observe_local(&observe(&root, &["d/ c.txt", "d/ok.txt"]));
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
			UnsyncablePath {
				rel_path: "a/c".to_owned(),
				reason: UnsyncableReason::RemoteBrokenParent,
			},
			UnsyncablePath {
				rel_path: "a2".to_owned(),
				reason: UnsyncableReason::RemoteUndecodable,
			},
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
		&SyncedPaths::default(),
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
	scoped.merge_remote_view(
		&BTreeSet::from(["a".to_owned()]),
		&derived_view(),
		&rows,
		&SyncedPaths::default(),
	);
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
		&SyncedPaths::default(),
	);
	assert!(
		untracked.ignored_remote.is_empty(),
		"nothing here says ignored content with no row is still there: {:?}",
		untracked.ignored_remote
	);

	// A whole read answers for every path, so its view REPLACES the carried roots, rows or not.
	let mut whole = carried();
	whole.merge_remote_view(
		&BTreeSet::from([String::new()]),
		&derived_view(),
		&rows,
		&SyncedPaths::default(),
	);
	assert!(
		whole.ignored_remote.is_empty(),
		"a whole read that found no ignored root means there is none: {:?}",
		whole.ignored_remote
	);
}

/// The carry-over set names every action a plan released past the guard, whatever became of
/// it, both endpoints of a move included, plus the paths the pass deferred and every path
/// carrying a failure streak.
#[test]
fn the_carry_over_set_names_every_released_action_and_every_streak() {
	let released = [
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
	let conflicts = [PlannedConflict {
		rel_path: "clash.txt".to_owned(),
		local: Some(PlannedNodeKind::File),
		remote: Some(PlannedNodeKind::File),
	}];
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

	let midflight = BTreeSet::from(["midflight.txt".to_owned()]);
	let ends = released.iter().flat_map(|action| {
		let (from, to) = action.endpoints();
		[from, to]
	});
	let owed: BTreeSet<&str> = carry_over(ends, &conflicts, &midflight, &failures).collect();

	assert_eq!(
		Vec::from_iter(owed),
		vec![
			"clash.txt",
			"flaky.txt",
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
