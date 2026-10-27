use std::{
	collections::HashSet,
	sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rand::{Rng, SeedableRng, rngs::StdRng};

use super::*;
use crate::{
	alloc_meter,
	auth::test_support::offline_client,
	cache::{
		CacheState, open_sqlite, open_sqlite_in_memory, open_sqlite_with_flags,
		test_support::rewrite_behind_sqlite,
	},
	sync::lock::MAX_SLEEP_TIME_DEFAULT,
	sync_engine::{
		PauseMode,
		apply::PassStart,
		baseline::{BaselineChange, LegacyItem, LegacySide, NodeKind},
		bin::{
			PruneSummary,
			test_support::{bucket, dated_bin, day},
		},
		changes::tests::{cacheable_dir, cacheable_file},
		derive::Derived,
		ignore::IgnoreRules,
		plan::{
			RemoteNode,
			test_support::{first_dir_deletion, folded},
		},
		scan::LocalNode,
		side::test_support::copied,
	},
};

/// A pass reconciled and screened as a list, in memory: what [`reconcile_and_screen`] screens in
/// the stage is held to this.
struct ResidentScreened {
	conflicts: Vec<PlannedConflict>,
	/// Every executable action in apply order — what an APPROVED pass runs, held deletions and
	/// all. Empty when the plan deletes nothing, which no approval can release.
	all: Vec<SyncAction>,
	decision: guard::test_support::GuardDecision,
	pass_token: Option<String>,
	deferred_paths: usize,
	dropped: usize,
}

/// The prepared inputs reconciled in memory and screened as a list, as a pass screened its plan
/// before it screened it in its stage (see [`ResidentScreened`]).
fn reconcile_and_screen_resident(prep: &Prepared, state: guard::ScreenState) -> ResidentScreened {
	let plan = plan::reconcile(
		prep.record.mode,
		&prep.baseline,
		&prep.local_scan.nodes.of(&prep.baseline),
		&prep.remote_view.nodes.of(&prep.baseline),
		&prep.holds,
		prep.read.paths(),
	);
	let mut actions = prep.dir_moves.clone();
	actions.extend(plan.actions);
	let planned = actions.len();
	let actions = drop_blocked(actions, &prep.blocked_paths(), &prep.ignored_roots());
	let (actions, over_ignored) = withhold_deletions_over_unreachable(
		actions,
		&prep.facts.unknown_remote,
		&prep.holds.held_remote,
		[&prep.facts.ignored_local, &prep.facts.ignored_remote],
		&prep.blocked_rules(),
	);
	let dropped = planned - actions.len();
	let deferred_paths = plan.deferred_paths + over_ignored;
	let actions = creates_before_dir_moves(actions);
	let (conflict_actions, executable): (Vec<_>, Vec<_>) = actions
		.into_iter()
		.partition(|a| matches!(a, SyncAction::Conflict { .. }));
	let conflicts = conflict_actions
		.iter()
		.map(|a| {
			planned_conflict(
				a.rel_path(),
				&prep.local_scan.nodes.of(&prep.baseline),
				&prep.remote_view.nodes.of(&prep.baseline),
			)
		})
		.collect();
	let all = if executable.iter().any(SyncAction::is_delete) {
		executable.clone()
	} else {
		Vec::new()
	};
	let unignored = prep.unignored();
	let first_sync = state.first_sync
		|| (!unignored.is_empty()
			&& executable.iter().any(|action| {
				let path = action.rel_path();
				action.is_delete()
					&& plan::ancestors(path)
						.chain([path])
						.any(|at| unignored.contains(at))
			}));
	let decision = guard::test_support::screen(
		executable,
		guard::ScreenState {
			first_sync,
			..state
		},
		prep.record.delete_guard,
	);
	let pass_token = decision
		.reason
		.as_ref()
		.map(|reason| deletion_batch_token(&decision.held, reason));
	ResidentScreened {
		conflicts,
		all,
		decision,
		pass_token,
		deferred_paths,
		dropped,
	}
}

/// [`Blocking`] over a list.
fn drop_blocked(
	actions: Vec<SyncAction>,
	blocked: &BTreeSet<String>,
	ignored: &BTreeSet<String>,
) -> Vec<SyncAction> {
	let Some(mut blocking) = Blocking::new(blocked, ignored) else {
		return actions;
	};
	for action in &actions {
		blocking.note(action);
	}
	actions
		.into_iter()
		.filter(|action| blocking.keeps(action))
		.collect()
}

/// [`Withholding`] over a list, with the deletions it counts as deferred.
fn withhold_deletions_over_unreachable(
	actions: Vec<SyncAction>,
	unknown: &BTreeMap<String, UnsyncableReason>,
	held: &BTreeSet<String>,
	ignored: [&BTreeMap<String, IgnoreDecision>; 2],
	ignore_blocked: &BTreeSet<String>,
) -> (Vec<SyncAction>, usize) {
	let mut withholding = Withholding::new(unknown, held, ignored, ignore_blocked);
	for action in &actions {
		withholding.note(action);
	}
	if !withholding.withholds() {
		return (actions, withholding.over_ignored);
	}
	let kept = actions
		.into_iter()
		.filter(|action| withholding.keeps(action))
		.collect();
	(kept, withholding.over_ignored)
}

/// Run each directory move right after the creates of the new directories above its
/// destination, on the side it moves (see `plan::parents_ready`); everything else keeps its
/// order. Those creates are planned with the rest of the pass, which runs after the moves. Over
/// a list: what [`Screen::lead_creates`] does in the stage.
fn creates_before_dir_moves(actions: Vec<SyncAction>) -> Vec<SyncAction> {
	let (moves, mut rest): (Vec<_>, Vec<_>) = actions.into_iter().partition(|action| {
		matches!(
			action,
			SyncAction::MoveLocal {
				kind: NodeKind::Dir,
				..
			} | SyncAction::MoveRemote {
				kind: NodeKind::Dir,
				..
			}
		)
	});
	let mut ordered = Vec::with_capacity(moves.len() + rest.len());
	for action in moves {
		let local = matches!(action, SyncAction::MoveLocal { .. });
		let to = action.rel_path();
		ordered.extend(rest.extract_if(.., |create| match create {
			SyncAction::CreateLocalDir { rel_path } => local && plan::is_under(to, rel_path),
			SyncAction::CreateRemoteDir { rel_path } => !local && plan::is_under(to, rel_path),
			_ => false,
		}));
		ordered.push(action);
	}
	ordered.extend(rest);
	ordered
}

/// A held batch's token over a list of it, its lines sorted in memory: what [`BatchToken`]
/// hashes as a staged plan streams them.
fn deletion_batch_token(held: &[SyncAction], reason: &GuardReason) -> String {
	let mut lines: Vec<String> = held
		.iter()
		.map(|action| match action {
			SyncAction::DeleteLocal { rel_path, .. } => format!("local\t{rel_path}"),
			SyncAction::TrashRemote { rel_path, .. } => format!("remote\t{rel_path}"),
			other => format!("other\t{}", other.rel_path()),
		})
		.collect();
	lines.sort_unstable();
	let mut hasher = blake3::Hasher::new();
	hasher.update(token_kind(reason).as_bytes());
	hasher.update(b"\n");
	for line in &lines {
		hasher.update(line.as_bytes());
		hasher.update(b"\n");
	}
	hasher.finalize().to_hex().chars().take(16).collect()
}

/// The pair whose writes the pending-write tests record.
const PAIR: PairId = 1;

fn upload(rel: &str) -> SyncAction {
	SyncAction::UploadFile {
		rel_path: rel.to_string(),
	}
}

/// The rows of the full-pass trigger table that are facts about the PREVIOUS pass: a refusal is
/// whole-tree state, a cut-short pass that read both sides whole planned from evidence no
/// changelist holds — a change-scoped one hands its two lists back instead — so did a pass
/// that planned work it could not carry out, and so did every deletion hold.
#[test]
fn a_passs_outcome_decides_whether_the_next_one_reads_everything() {
	assert_eq!(
		next_pass_scope(&SyncReport::default()),
		None,
		"a pass that ran to the end leaves the next one to its changelists"
	);
	for reason in [
		FullPassReason::LocalOverflow,
		FullPassReason::RemoteOverflow,
	] {
		assert_eq!(
			next_pass_scope(&SyncReport {
				full_pass: Some(reason),
				..SyncReport::default()
			}),
			None,
			"{reason:?}: a pass that read whole for a change too large to narrow it to read \
				 everything that change named, so the next one narrows again"
		);
	}
	assert_eq!(
		next_pass_scope(&SyncReport {
			dropped_actions: 1,
			..SyncReport::default()
		}),
		Some(FullPassReason::UnappliedWork),
		"an action the pass planned and dropped is named in no changelist any more"
	);
	assert_eq!(
		next_pass_scope(&SyncReport {
			refused: Some(RefuseReason::LocalCollision),
			..SyncReport::default()
		}),
		Some(FullPassReason::PreviousRefusal)
	);
	assert_eq!(
		next_pass_scope(&SyncReport {
			paused: true,
			..SyncReport::default()
		}),
		Some(FullPassReason::InterruptedPass),
		"a cancel that beat the plan still consumed the changelists"
	);
	assert_eq!(
		next_pass_scope(&SyncReport {
			interrupted: 3,
			full_pass: Some(FullPassReason::SafetyNet),
			..SyncReport::default()
		}),
		Some(FullPassReason::InterruptedPass),
		"a whole read's evidence is in no changelist, so its remainder needs another one"
	);
	assert_eq!(
		next_pass_scope(&SyncReport {
			interrupted: 3,
			..SyncReport::default()
		}),
		None,
		"a change-scoped pass hands both its lists back, so the next one re-plans from them"
	);
	assert_eq!(
		next_pass_scope(&SyncReport {
			interrupted: 3,
			failed_paths: vec![("a.txt".to_string(), "boom".to_string())],
			..SyncReport::default()
		}),
		Some(FullPassReason::UnappliedWork),
		"an action that FAILED is a debt of its own, and still wants a whole read"
	);

	// EVERY deletion hold: three want evidence only a whole-tree read supplies, and the other
	// two have to be able to offer the caller the very same batch again under the same token,
	// which means reproducing absences the holding pass consumed.
	for reason in [
		GuardReason::ScanIncomplete,
		GuardReason::RemoteUnconverged { deletions: 2 },
		GuardReason::RemoteEmptied { deletions: 2 },
		GuardReason::ExceededThreshold {
			deletions: 40,
			limit: 10,
		},
		GuardReason::FirstSyncWithDeletions { deletions: 4 },
	] {
		assert_eq!(
			next_pass_scope(&SyncReport {
				guard: Some(reason.clone()),
				..SyncReport::default()
			}),
			Some(FullPassReason::DeletionHold),
			"{reason:?}"
		);
	}

	// A pass that made a plan and did not apply it, in each of the four shapes. Every one of
	// them drained the changelists first, so the paths are owed and nothing else holds them.
	for (label, report) in [
		(
			"a drive lock it could not take",
			SyncReport {
				lock_failed: Some(ErrorKind::RetryFailed),
				..SyncReport::default()
			},
		),
		(
			"a side that filled up",
			SyncReport {
				halted: Some(crate::sync_engine::HaltReason::LocalStorageFull),
				..SyncReport::default()
			},
		),
		(
			"a record that did not land",
			SyncReport {
				store_failed: Some(apply::StoreFailure::RecordNotWritten(ErrorKind::IO)),
				..SyncReport::default()
			},
		),
		(
			"an action that failed",
			SyncReport {
				failed_paths: vec![("a.txt".to_string(), "boom".to_string())],
				..SyncReport::default()
			},
		),
	] {
		assert_eq!(
			next_pass_scope(&report),
			Some(FullPassReason::UnappliedWork),
			"{label}"
		);
	}
}

/// An approval names one batch AND the kind of reason it was held for: the same deletions held
/// for another reason are a hold the caller was never shown, so they get a token of their own.
/// The order the plan listed them in, and the counts a reason carries, are not part of it.
#[test]
fn a_held_batchs_token_names_its_deletions_and_the_kind_of_hold() {
	let trash = |rel: &str| SyncAction::TrashRemote {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: Uuid::from_u128(1),
	};
	let batch = vec![
		trash("a.txt"),
		trash("b/c.txt"),
		SyncAction::DeleteLocal {
			rel_path: "d.txt".to_string(),
			kind: NodeKind::File,
		},
	];
	let over = GuardReason::ExceededThreshold {
		deletions: 3,
		limit: 2,
	};
	let token = deletion_batch_token(&batch, &over);
	assert_eq!(token.len(), 16);

	let reordered: Vec<SyncAction> = batch.iter().rev().cloned().collect();
	assert_eq!(
		deletion_batch_token(&reordered, &over),
		token,
		"the same batch in another order"
	);
	assert_eq!(
		deletion_batch_token(
			&batch,
			&GuardReason::ExceededThreshold {
				deletions: 3,
				limit: 1,
			}
		),
		token,
		"a limit that moved with the tracked set is still the same hold"
	);

	for other in [
		GuardReason::ScanIncomplete,
		GuardReason::FirstSyncWithDeletions { deletions: 3 },
		GuardReason::RemoteUnconverged { deletions: 3 },
		GuardReason::RemoteEmptied { deletions: 3 },
	] {
		assert_ne!(
			deletion_batch_token(&batch, &other),
			token,
			"the same deletions held for {other:?} must not take an approval of the volume hold"
		);
	}
	assert_ne!(
		deletion_batch_token(&batch[..2], &over),
		token,
		"a batch with one deletion fewer is another batch"
	);
}

#[test]
fn a_blocked_path_takes_its_subtree_but_not_its_lookalikes_out_of_the_plan() {
	let actions = vec![
		upload("ok.txt"),
		upload("bad"),
		upload("bad/inner.txt"),
		upload("bad/deep/x.txt"),
		// A sibling whose name merely STARTS with the blocked one: a different path entirely.
		upload("badly.txt"),
		upload("badge/y.txt"),
	];
	let blocked = BTreeSet::from(["bad".to_string()]);
	let kept: Vec<String> = drop_blocked(actions.clone(), &blocked, &BTreeSet::new())
		.iter()
		.map(|a| a.rel_path().to_string())
		.collect();
	assert_eq!(kept, vec!["ok.txt", "badly.txt", "badge/y.txt"]);

	// Nothing blocked -> the plan is untouched.
	assert_eq!(
		drop_blocked(actions.clone(), &BTreeSet::new(), &BTreeSet::new()),
		actions
	);
}

/// A synced file whose remote item dropped out of the view (its metadata stopped decoding) reads
/// to the reconciler exactly like a remote deletion, and one deletion is far under the guard's
/// floor, so the guard lets it through. The block on the unknown path is what keeps the local
/// copy.
#[test]
fn a_synced_path_whose_remote_item_left_the_view_is_blocked_not_deleted() {
	let root = Uuid::new_v4();
	let uuid = Uuid::new_v4();
	let hash = Blake3Hash::from([3; 32]);
	let baseline = HashMap::from([(
		"doc.txt".to_string(),
		BaselineEntry {
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: Some(hash),
			..synced_file("doc.txt", uuid, hash, 4)
		},
	)]);
	let local = HashMap::from([(
		"doc.txt".to_string(),
		LocalNode {
			rel_path: "doc.txt".to_string(),
			kind: NodeKind::File,
			size: 4,
			mtime_millis: 0,
			content_hash: Some(hash),
		},
	)]);
	let view = plan::build_remote_view(
		root,
		PairRoot::Directory,
		&[],
		&[],
		&[crate::cache::UndecodableItem {
			uuid,
			parent: root,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
		}],
		None,
	);
	let reconciled = plan::reconcile(
		SyncMode::TwoWay,
		&tree(&baseline),
		&local,
		&view.nodes.whole(),
		&plan::PassHolds::default(),
		plan::PassPaths::Whole,
	);
	assert_eq!(
		reconciled.actions,
		vec![SyncAction::DeleteLocal {
			rel_path: "doc.txt".to_string(),
			kind: NodeKind::File,
		}],
		"unblocked, the vanished item plans a local delete"
	);
	let healthy = guard::ScreenState {
		scan_complete: true,
		remote: guard::RemoteEvidence::Listed,
		first_sync: false,
		tracked: 1,
	};
	assert!(
		guard::test_support::screen(reconciled.actions.clone(), healthy, DeleteGuard::default())
			.held
			.is_empty(),
		"the guard alone lets that one delete through"
	);

	let rows = tree(&baseline);
	let (unknown, never_synced) =
		plan::unknown_remote_paths(&synced_for(&rows, &view.skipped), &view.skipped);
	assert_eq!(
		unknown,
		BTreeMap::from([("doc.txt".to_string(), UnsyncableReason::RemoteUndecodable)])
	);
	assert!(never_synced.is_empty(), "{never_synced:?}");
	assert!(
		drop_blocked(
			reconciled.actions,
			&unknown.keys().cloned().collect(),
			&BTreeSet::new()
		)
		.is_empty(),
		"the unknown path plans nothing"
	);
}

/// What a pass plans and reports once both sides are read: folded and screened exactly as a
/// pass folds and screens them, with a guard that holds nothing back.
fn folded_pass(
	mode: SyncMode,
	baseline: HashMap<String, BaselineEntry>,
	local: Vec<LocalNode>,
	remote_view: RemoteView,
	failures: HashMap<String, PathFailure>,
) -> (Vec<SyncAction>, Vec<UnsyncablePath>) {
	run_prepared(prepared(mode, baseline, local, remote_view, failures))
}

/// Fold and screen `prep` as a pass does.
fn run_prepared(mut prep: Prepared) -> (Vec<SyncAction>, Vec<UnsyncablePath>) {
	prep.fold_dir_moves().unwrap();
	let screened = screened_both(&mut prep);
	let mut actions = screened.decision.safe;
	actions.extend(screened.decision.held);
	(actions, prep.unsyncable())
}

/// What a pass makes of `prep`, whose two sides are whole maps, reconciled and screened as a list
/// in memory ([`reconcile_and_screen_resident`]) — and held to what a pass screens in its stage
/// from the same inputs: reconciled in memory, as a change-scoped pass reconciles, and window by
/// window over the same two sides staged, as a whole pass does. Each must come out with the same
/// actions, safe and held, in the same order, the same conflicts, hold, token and counts, and the
/// same folded plan, approved or not (see [`assert_screened_as`]). `prep` is left with its maps.
fn screened_both(prep: &mut Prepared) -> ResidentScreened {
	let resident = reconcile_and_screen_resident(prep, screen_state(prep));
	let (_dir, stage) = Stage::in_temp_dir();
	let in_memory = reconcile_and_screen(prep, screen_state(prep), || Ok(stage)).unwrap();
	assert_screened_as(in_memory, &resident, "reconciled in memory");
	let (whole_local, whole_remote) = (
		copied(&prep.local_scan.nodes),
		copied(&prep.remote_view.nodes),
	);
	let (_stages, local, remote) = stage::staged_sides(
		copied(&whole_local).into_whole().into_values(),
		copied(&whole_remote).into_whole().into_values(),
		2,
	);
	prep.local_scan.nodes = local;
	prep.remote_view.nodes = remote;
	let (_plan_dir, plan_stage) = Stage::in_temp_dir();
	let windowed = reconcile_and_screen(prep, screen_state(prep), || Ok(plan_stage)).unwrap();
	prep.local_scan.nodes = whole_local;
	prep.remote_view.nodes = whole_remote;
	assert_screened_as(windowed, &resident, "reconciled window by window");
	resident
}

/// Every action of `rows` a screened plan holds, in plan order.
fn read(screened: &Screened, rows: Rows) -> Vec<SyncAction> {
	screened
		.plan
		.as_ref()
		.map_or_else(Vec::new, |plan| plan.read(rows).unwrap())
}

/// What a plan screened in its stage folded into its directory deletions, by directory, sorted.
fn carried(screened: &Screened) -> Vec<(String, String)> {
	let mut carried = Vec::new();
	if let Some(plan) = &screened.plan {
		plan.each_carried(|dir, path| carried.push((dir.to_owned(), path.to_owned())))
			.unwrap();
	}
	carried.sort();
	carried
}

/// `staged`, a plan screened in its stage, is `resident`, the same plan screened as a list:
/// the same actions, safe and held, in the same order, the same conflicts, hold, token and counts
/// — and, folded as a pass folds it once the guard has let it through, the same plan to apply and
/// the same deletions carried. A held plan is folded as an approval releases it.
fn assert_screened_as(mut staged: Screened, resident: &ResidentScreened, how: &str) {
	assert_eq!(
		read(&staged, Rows::Safe),
		resident.decision.safe,
		"{how}: the actions safe to apply"
	);
	assert_eq!(
		read(&staged, Rows::Held),
		resident.decision.held,
		"{how}: the held ones"
	);
	assert_eq!(
		staged.reason, resident.decision.reason,
		"{how}: why they are held"
	);
	assert_eq!(
		staged.pass_token, resident.pass_token,
		"{how}: the held batch's token"
	);
	assert_eq!(staged.conflicts, resident.conflicts, "{how}: the conflicts");
	assert_eq!(
		staged.deferred_paths, resident.deferred_paths,
		"{how}: the deferred paths"
	);
	assert_eq!(
		staged.dropped, resident.dropped,
		"{how}: the dropped actions"
	);
	let released = if staged.reason.is_some() {
		staged.approve().unwrap();
		assert_eq!(
			read(&staged, Rows::Safe),
			resident.all,
			"{how}: what an approval releases"
		);
		resident.all.clone()
	} else {
		resident.decision.safe.clone()
	};
	staged.fold_subsumed().unwrap();
	assert_eq!(
		(read(&staged, Rows::Safe), carried(&staged)),
		folded(released),
		"{how}: what the apply runs, and what its directory deletions carry"
	);
}

/// A random action a reconcile plans — any but a directory move, which only the fold plans —
/// over a namespace small enough that plans flip types, nest, conflict and share prefixes often.
fn random_planned(rng: &mut StdRng) -> SyncAction {
	let path = random_path(rng);
	let kind = if rng.random_bool(0.5) {
		NodeKind::Dir
	} else {
		NodeKind::File
	};
	let remote_uuid = Uuid::from_u128(rng.random_range(0..4));
	match rng.random_range(0..10) {
		0 => SyncAction::CreateLocalDir { rel_path: path },
		1 => SyncAction::DownloadFile {
			rel_path: path,
			remote_uuid,
		},
		2 => SyncAction::DeleteLocal {
			rel_path: path,
			kind,
		},
		3 => SyncAction::CreateRemoteDir { rel_path: path },
		4 => upload(&path),
		5 => SyncAction::TrashRemote {
			rel_path: path,
			kind,
			remote_uuid,
		},
		6 => SyncAction::Conflict { rel_path: path },
		7 => SyncAction::AdoptBaseline { rel_path: path },
		8 => SyncAction::MoveRemote {
			from_path: random_path(rng),
			to_path: path,
			kind: NodeKind::File,
			remote_uuid,
		},
		_ => SyncAction::MoveLocal {
			from_path: random_path(rng),
			to_path: path,
			kind: NodeKind::File,
		},
	}
}

fn random_path(rng: &mut StdRng) -> String {
	(0..rng.random_range(1..=3))
		.map(|_| ["a", "a-b", "b", "e"][rng.random_range(0..4)])
		.collect::<Vec<_>>()
		.join("/")
}

/// A random directory move, as the fold plans one.
fn random_dir_move(rng: &mut StdRng) -> SyncAction {
	let (from_path, to_path) = (random_path(rng), random_path(rng));
	if rng.random_bool(0.5) {
		SyncAction::MoveLocal {
			from_path,
			to_path,
			kind: NodeKind::Dir,
		}
	} else {
		SyncAction::MoveRemote {
			from_path,
			to_path,
			kind: NodeKind::Dir,
			remote_uuid: Uuid::from_u128(9),
		}
	}
}

/// A staged plan as the apply is handed it: each action in apply order, with its run and whether
/// the lock may be let go before it; and what each directory deletion carries, sorted.
fn apply_order(plan: StagedPlan) -> (Vec<(SyncAction, stage::Run, bool)>, Vec<String>) {
	let mut pager = plan.pager();
	let mut order = Vec::new();
	for run in [stage::Run::Before, stage::Run::Transfers, stage::Run::After] {
		while let Some(staged) = pager.next_in(run).unwrap() {
			order.push((staged.entry.action, staged.run, staged.releasable));
		}
	}
	let mut carried = Vec::new();
	for (action, _, _) in &order {
		if let Some(rows) = pager.carried(action) {
			rows.each(&mut |path| {
				carried.push(format!("{} carries {path}", action.rel_path()));
				Ok(())
			})
			.unwrap();
		}
	}
	carried.sort();
	(order, carried)
}

/// The screen a pass runs in its stage — the plan finished and ordered, led by the directory
/// moves and the creates above their destinations, the conflicts split out, the deletions counted
/// and held, the held batch's token streamed, an approval, the fold, and the plan staged for the
/// apply — is the one a pass ran over a list, step for step, over plans of every shape: the same
/// actions in the same order, the same token, and the same order the apply runs them in.
#[test]
fn the_staged_screen_is_the_resident_one_over_generated_plans() {
	for seed in 0..400u64 {
		let mut rng = StdRng::seed_from_u64(seed);
		let actions: Vec<SyncAction> = (0..rng.random_range(0..=40))
			.map(|_| random_planned(&mut rng))
			.collect();
		let dir_moves: Vec<SyncAction> = (0..rng.random_range(0..=3))
			.map(|_| random_dir_move(&mut rng))
			.collect();
		let mut trashed = HashSet::new();
		for action in &actions {
			if let SyncAction::TrashRemote { remote_uuid, .. } = action
				&& rng.random_bool(0.3)
			{
				trashed.insert(*remote_uuid);
			}
		}
		let holds = plan::PassHolds {
			trashed,
			held_remote: BTreeSet::new(),
		};
		// An incomplete scan holds every deletion; a complete one under no limit holds none.
		let state = guard::ScreenState {
			scan_complete: rng.random_bool(0.5),
			remote: guard::RemoteEvidence::Listed,
			first_sync: false,
			tracked: 100,
		};
		let guard = DeleteGuard::unlimited();
		let context = format!("seed {seed}: {dir_moves:?} then {actions:?}");

		// As a list, the way a pass screened its plan before it screened it in its stage.
		let finished = plan::finish_plan(actions.clone(), 0, &holds);
		let mut listed = dir_moves.clone();
		listed.extend(finished.actions);
		let (conflicts, executable): (Vec<_>, Vec<_>) = creates_before_dir_moves(listed)
			.into_iter()
			.partition(|action| matches!(action, SyncAction::Conflict { .. }));
		let decision = guard::test_support::screen(executable.clone(), state, guard);
		let token = decision
			.reason
			.as_ref()
			.map(|reason| deletion_batch_token(&decision.held, reason));

		// In the stage, from the plan as a whole pass's windows write it.
		let (_dir, stage) = Stage::in_temp_dir();
		let (mut screen, deferred) = stage::windows_of(stage, &actions)
			.screen(&holds, &dir_moves)
			.unwrap();
		assert_eq!(deferred, finished.deferred_paths, "{context}");
		screen.lead_creates().unwrap();
		let mut staged_conflicts = Vec::new();
		screen
			.take_conflicts(|path| {
				staged_conflicts.push(SyncAction::Conflict {
					rel_path: path.to_owned(),
				});
			})
			.unwrap();
		assert_eq!(staged_conflicts, conflicts, "{context}");
		assert_eq!(screen.read(Rows::All).unwrap(), executable, "{context}");
		let reason = guard::reason_for(screen.count(Rows::Deletes).unwrap(), state, guard);
		assert_eq!(reason, decision.reason, "{context}");
		if let Some(reason) = &reason {
			screen.hold().unwrap();
			let mut streamed = BatchToken::new(reason);
			screen
				.each_held_by_line(|action| streamed.line(action))
				.unwrap();
			assert_eq!(Some(streamed.finish()), token, "{context}");
		}
		assert_eq!(screen.read(Rows::Held).unwrap(), decision.held, "{context}");
		assert_eq!(screen.read(Rows::Safe).unwrap(), decision.safe, "{context}");

		// Approved, folded and staged for the apply.
		screen.release_held().unwrap();
		screen.fold_subsumed().unwrap();
		let mut released = executable;
		let subsumed = plan::test_support::fold_subsumed_deletes(&mut released);
		assert_eq!(screen.read(Rows::Safe).unwrap(), released, "{context}");
		let staged = screen.stage_plan(PassStart::bare).unwrap();
		let (_dir, stage) = Stage::in_temp_dir();
		let mut writer = stage::PlanWriter::new(stage).unwrap();
		let mut filed = HashSet::new();
		for action in &released {
			writer.push(&PlannedEntry::bare(action.clone())).unwrap();
			if first_dir_deletion(&mut filed, action) {
				writer.carry(action, subsumed.under(action)).unwrap();
			}
		}
		assert_eq!(
			apply_order(staged),
			apply_order(writer.finish().unwrap()),
			"{context}"
		);
	}
}

/// A held batch's token, hashed line by line as the stage reads the held actions out in sorted
/// order, is the token of the same lines sorted in memory: byte for byte, over paths whose bytes
/// sort otherwise than their characters seem to and the create half of a type flip.
#[test]
fn a_streamed_token_is_the_token_of_the_sorted_lines() {
	let mut actions: Vec<SyncAction> = ["Zeta", "zeta", "Äpfel", "a b", "a-b", "a/b", "é", "a"]
		.into_iter()
		.flat_map(|path| {
			[
				SyncAction::DeleteLocal {
					rel_path: path.to_owned(),
					kind: NodeKind::File,
				},
				SyncAction::TrashRemote {
					rel_path: format!("r/{path}"),
					kind: NodeKind::Dir,
					remote_uuid: Uuid::from_u128(1),
				},
			]
		})
		.collect();
	actions.push(SyncAction::CreateRemoteDir {
		rel_path: "r/a".to_owned(),
	});
	actions.push(upload("kept.txt"));
	let reason = GuardReason::ExceededThreshold {
		deletions: 16,
		limit: 10,
	};
	let held: Vec<SyncAction> = actions[..17].to_vec();
	let (_dir, stage) = Stage::in_temp_dir();
	let mut screen = Screen::new(stage, &[], &actions).unwrap();
	assert_eq!(
		screen.hold().unwrap(),
		17,
		"16 deletions and the create a flip pairs"
	);
	assert_eq!(screen.read(Rows::Held).unwrap(), held);
	let mut streamed = BatchToken::new(&reason);
	screen
		.each_held_by_line(|action| streamed.line(action))
		.unwrap();
	let streamed = streamed.finish();
	assert_eq!(streamed, deletion_batch_token(&held, &reason));
	assert_eq!(streamed.len(), 16);
}

/// A batch the guard held is not owed: the hold sends the next pass that runs to a whole read,
/// which reproduces it. Owing its paths as well would keep a pair whose only business is the
/// hold from ever being idle, and every wake would re-read both trees to hold it again.
#[test]
fn a_held_batch_is_not_carried_over() {
	let actions = [
		upload("up.txt"),
		SyncAction::TrashRemote {
			rel_path: "gone.txt".to_owned(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(8),
		},
	];
	let (_dir, stage) = Stage::in_temp_dir();
	let mut plan = Screen::new(stage, &[], &actions).unwrap();
	assert_eq!(plan.hold().unwrap(), 1);
	let screened = Screened {
		conflicts: Vec::new(),
		plan: Some(plan),
		reason: Some(GuardReason::ExceededThreshold {
			deletions: 1,
			limit: 0,
		}),
		pass_token: None,
		deferred_paths: 0,
		dropped: 0,
	};
	assert_eq!(
		released_paths(&screened).unwrap(),
		BTreeSet::from(["up.txt".to_owned()]),
		"the upload is owed; the held trash is not"
	);
}

/// What the next pass owes is read out of the plan only until it is more than the changelists
/// keep, past which they keep nothing: a first sync's plan of the whole tree costs that many
/// paths, not one per item.
#[test]
fn the_released_paths_are_read_only_up_to_the_cap() {
	let actions: Vec<SyncAction> = (0..DIRTY_CAP * 3)
		.map(|n| upload(&format!("d{:02}/f{n:06}.txt", n % 100)))
		.collect();
	let (_dir, stage) = Stage::in_temp_dir();
	let screened = Screened {
		conflicts: Vec::new(),
		plan: Some(Screen::new(stage, &[], &actions).unwrap()),
		reason: None,
		pass_token: None,
		deferred_paths: 0,
		dropped: 0,
	};
	assert_eq!(released_paths(&screened).unwrap().len(), DIRTY_CAP + 1);
}

/// A local deletion of 100,000 synced files, which the guard holds for the volume, screened as a
/// whole pass screens it: what the pass holds while it plans, counts, holds and names the batch
/// is a page of the plan, not the plan, a copy of it, or a line per deletion for the token.
#[test]
fn a_held_mass_deletion_is_screened_holding_a_page_not_the_plan() {
	const FILES: u128 = 100_000;
	let hash = Blake3Hash::from([1; 32]);
	let files: Vec<(String, Uuid)> = (0..FILES)
		.map(|n| {
			(
				format!("d{:02}/f{n:06}.txt", n % 100),
				Uuid::from_u128(n + 1),
			)
		})
		.collect();
	let baseline = files
		.iter()
		.map(|(path, uuid)| (path.clone(), synced_file(path, *uuid, hash, 4)))
		.collect();
	let remote: Vec<RemoteNode> = files
		.iter()
		.map(|(path, uuid)| remote_file(path, *uuid, hash, 4))
		.collect();
	let mut prep = prepared(
		SyncMode::TwoWay,
		baseline,
		Vec::new(),
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	prep.record.delete_guard = DeleteGuard::default();
	let (_stages, local, remote) =
		stage::staged_sides(std::iter::empty(), remote, stage::WINDOW.get());
	prep.local_scan.nodes = local;
	prep.remote_view.nodes = remote;
	let (_dir, stage) = Stage::in_temp_dir();
	let (screened, peak) = alloc_meter::peak_bytes(|| {
		reconcile_and_screen(&prep, screen_state(&prep), || Ok(stage)).unwrap()
	});
	assert!(screened.pass_token.is_some(), "the batch is held");
	assert_eq!(
		screened.reason,
		Some(GuardReason::ExceededThreshold {
			deletions: 100_000,
			limit: 50_000,
		})
	);
	assert_eq!(screened.count(Rows::Held).unwrap(), 100_000);
	assert_eq!(screened.count(Rows::Safe).unwrap(), 0);
	// Screened as a list, the same pass held 29,075,796 bytes: the plan, the copy an approval
	// would release, the guard's partition and a line per deletion for the token.
	assert!(
		peak < 4 << 20,
		"screening {FILES} deletions held {peak} bytes at once"
	);
}

/// A pass's inputs before the fold, with a complete scan that found nothing it could not sync.
fn prepared(
	mode: SyncMode,
	baseline: HashMap<String, BaselineEntry>,
	local: Vec<LocalNode>,
	remote_view: RemoteView,
	failures: HashMap<String, PathFailure>,
) -> Prepared {
	let baseline = Baseline::from_rows(baseline.into_values());
	let (unknown_remote, never_synced_remote) = plan::unknown_remote_paths(
		&synced_for(&baseline, &remote_view.skipped),
		&remote_view.skipped,
	);
	Prepared {
		read: PassRead::Whole(Some(FullPassReason::SafetyNet)),
		remote_delta: Vec::new(),
		record: PairRecord {
			id: PAIR,
			local_root: String::new(),
			remote_root: Uuid::nil(),
			mode,
			delete_guard: DeleteGuard::unlimited(),
			paused: false,
		},
		tracked_at_start: baseline.len(),
		baseline,
		dir_moves: Vec::new(),
		local_scan: LocalScan {
			nodes: local
				.into_iter()
				.map(|node| (node.rel_path.clone(), node))
				.collect(),
			complete: true,
			errors: Vec::new(),
			invalid_names: BTreeMap::new(),
			aliased_dirs: BTreeMap::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
			ignore_blocked: BTreeSet::new(),
		},
		holds: plan::PassHolds {
			trashed: HashSet::new(),
			held_remote: remote_view.held_paths.clone(),
		},
		remote_view,
		remote_converged: true,
		remote_emptied: false,
		failures,
		facts: PairFacts {
			unknown_remote,
			never_synced_remote,
			..PairFacts::default()
		},
		remote_blocked: BTreeSet::new(),
		remote_rule_errors: Vec::new(),
		last_ignored: BTreeSet::new(),
		confirmed: Confirmed::default(),
	}
}

/// Plan 3.6's self-check as the pass runs it — [`assembly_bounds`], held against the length of
/// the map that was assembled — for a test holding the observations and the map at once.
fn assembly_accounted(
	baseline: &Baseline,
	derived: &Derived,
	observed: &LocalObservations,
	held: &BTreeSet<String>,
) -> bool {
	assembly_bounds(baseline, observed, held).contains(&derived.local.of(baseline).len())
}

/// The assembly self-check of plan 3.6: a map that accounts for the rows it carried and the
/// nodes its observations found is accepted, and one that lost a path NO observation covered —
/// the bookkeeping error that reads downstream as a deletion — is refused, which sends the
/// pass to a whole read instead.
#[test]
fn the_assembly_check_catches_a_path_no_observation_dropped() {
	let hash = Blake3Hash::from([7u8; 32]);
	let baseline = Baseline::from_rows([
		dir_row("docs", Uuid::new_v4()),
		file_row("docs/a.txt", Uuid::new_v4(), hash),
		file_row("keep.txt", Uuid::new_v4(), hash),
	]);
	let file_node = |rel: &str| LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		size: 4,
		mtime_millis: 1,
		content_hash: Some(hash),
	};
	// One observation: a COMPLETE walk of `docs`, which found both of its rows again.
	let scan = LocalScan {
		nodes: [local_dir("docs"), file_node("docs/a.txt")]
			.into_iter()
			.map(|node| (node.rel_path.clone(), node))
			.collect(),
		complete: true,
		errors: Vec::new(),
		invalid_names: BTreeMap::new(),
		aliased_dirs: BTreeMap::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
		ignore_blocked: BTreeSet::new(),
	};
	let observed = LocalObservations {
		observed: BTreeMap::from([("docs".to_string(), LocalObservation::Dir(Box::new(scan)))]),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};
	let none = BTreeSet::new();
	let held = BTreeSet::from(["keep.txt".to_string()]);
	let assembled = |paths: &[&str]| Derived {
		local: paths
			.iter()
			.map(|rel| ((*rel).to_string(), file_node(rel)))
			.collect(),
		remote: Side::default(),
		dirty: BTreeSet::new(),
		decided: BTreeSet::new(),
		held: BTreeSet::new(),
	};

	assert!(
		assembly_accounted(
			&baseline,
			&assembled(&["docs", "docs/a.txt", "keep.txt"]),
			&observed,
			&none
		),
		"the walk replaced its own two rows and the third was carried: that is the whole tree"
	);
	assert!(
		!assembly_accounted(
			&baseline,
			&assembled(&["docs", "docs/a.txt"]),
			&observed,
			&none
		),
		"a row outside every observation went missing and the check passed it — that is the \
			 shape that reads as a deletion"
	);
	// The same map, with `keep.txt` HELD instead: a held row is in no map on purpose, and the
	// count of them is what tells the two apart. `keep.txt` gets no observation at all — the
	// shape a hidden path or a path with no reading takes — so nothing else accounts for it.
	assert!(
		assembly_accounted(
			&baseline,
			&assembled(&["docs", "docs/a.txt"]),
			&observed,
			&held
		),
		"a row this pass holds is in neither map by design"
	);
	assert!(
		!assembly_accounted(
			&baseline,
			&assembled(&["docs", "docs/a.txt", "keep.txt"]),
			&observed,
			&held
		),
		"a held row that is in the map anyway means the two do not describe the same tree"
	);
	assert!(
		!assembly_accounted(
			&baseline,
			&assembled(&["docs", "docs/a.txt", "keep.txt", "invented.txt"]),
			&observed,
			&none
		),
		"a path no row and no observation named appeared in the map"
	);
}

/// A hidden observation takes its rows out of the local map (`derive::merge_local`), so the
/// check has to expect them gone. Counting a `Hidden` as "replaces nothing" would send every
/// pass that re-observes a hidden path with rows still behind it to a whole read — and would
/// pass a map that kept them, which is the pair of maps that plans a delete there.
#[test]
fn the_assembly_check_expects_a_hidden_path_to_take_its_rows() {
	let hash = Blake3Hash::from([7u8; 32]);
	let baseline = Baseline::from_rows([
		dir_row("logs", Uuid::new_v4()),
		file_row("logs/a.txt", Uuid::new_v4(), hash),
		file_row("keep.txt", Uuid::new_v4(), hash),
	]);
	let observed = LocalObservations {
		observed: BTreeMap::from([(
			"logs".to_string(),
			LocalObservation::Hidden(IgnoreDecision {
				level: IgnoreLevel::User,
				pattern: "logs/".to_string(),
			}),
		)]),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};
	let none = BTreeSet::new();
	let assembled = |paths: &[&str]| Derived {
		local: paths
			.iter()
			.map(|rel| ((*rel).to_string(), local_file(rel, hash)))
			.collect(),
		remote: Side::default(),
		dirty: BTreeSet::new(),
		decided: BTreeSet::new(),
		held: BTreeSet::new(),
	};

	assert!(
		assembly_accounted(&baseline, &assembled(&["keep.txt"]), &observed, &none),
		"the hidden root took its own row and the one under it"
	);
	assert!(
		!assembly_accounted(
			&baseline,
			&assembled(&["keep.txt", "logs/a.txt"]),
			&observed,
			&none
		),
		"a row under a hidden root that stayed in the map is a map the view no longer agrees \
			 with"
	);
}

/// A row this pass HOLDS under an observed path is not a row that observation replaced: it was
/// never in the map to be taken out of it. Counting it as both — held, and replaced — puts the
/// bound below the map the derivation legitimately assembled, and the pass falls back to a whole
/// read at every hidden root, dirty directory or absence with a half-written row under it.
#[test]
fn the_assembly_check_does_not_subtract_a_held_row_twice() {
	let hash = Blake3Hash::from([7u8; 32]);
	let baseline = Baseline::from_rows([
		dir_row("logs", Uuid::new_v4()),
		file_row("logs/a.txt", Uuid::new_v4(), hash),
		file_row("keep.txt", Uuid::new_v4(), hash),
	]);
	// `logs/a.txt` records one side only, so `derive::carried` refused it: it is in neither map,
	// it is held — and it sits under the root the observation below covers.
	let held = BTreeSet::from(["logs/a.txt".to_string()]);
	let observed = LocalObservations {
		observed: BTreeMap::from([(
			"logs".to_string(),
			LocalObservation::Hidden(IgnoreDecision {
				level: IgnoreLevel::User,
				pattern: "logs/".to_string(),
			}),
		)]),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};
	let assembled = Derived {
		local: [("keep.txt".to_string(), local_file("keep.txt", hash))]
			.into_iter()
			.collect(),
		remote: Side::default(),
		dirty: BTreeSet::new(),
		decided: BTreeSet::new(),
		held: BTreeSet::new(),
	};

	assert!(
		assembly_accounted(&baseline, &assembled, &observed, &held),
		"the hidden root took the one row it had in the map, and the held row was never in one"
	);
}

/// The rows a complete walk replaces are the table's count under it less what it pruned and
/// less the held rows, and the subtractions have to land exactly once each: a root pruned
/// INSIDE another pruned root is in that root's count already, a held row under a pruned root
/// went with the root, and a held row outside every root is taken on its own. A sibling that
/// is a string prefix and no path prefix (`docs b`) is under none of them.
#[test]
fn the_assembly_check_subtracts_nested_pruned_roots_and_held_rows_once() {
	let hash = Blake3Hash::from([7u8; 32]);
	let file = |rel: &str| file_row(rel, Uuid::new_v4(), hash);
	let dir = |rel: &str| dir_row(rel, Uuid::new_v4());
	let baseline = Baseline::from_rows([
		dir("docs"),
		file("docs/a.txt"),
		file("docs/half.txt"),
		dir("docs/build"),
		file("docs/build/out.o"),
		dir("docs/build/cache"),
		file("docs/build/cache/c.bin"),
		file("docs/build/held.o"),
		dir("docs/link"),
		file("docs/link/l.txt"),
		file("docs b"),
		file("keep.txt"),
	]);
	// Rows that record one side only (`derive::carried`): in neither map, one of them under a
	// root the walk pruned.
	let held = BTreeSet::from(["docs/half.txt".to_string(), "docs/build/held.o".to_string()]);
	let ignored = |pattern: &str| IgnoreDecision {
		level: IgnoreLevel::User,
		pattern: pattern.to_string(),
	};
	let scan = LocalScan {
		nodes: [
			local_dir("docs"),
			local_file("docs/a.txt", hash),
			local_file("docs/new.txt", hash),
		]
		.into_iter()
		.map(|node| (node.rel_path.clone(), node))
		.collect(),
		complete: true,
		errors: Vec::new(),
		invalid_names: BTreeMap::new(),
		aliased_dirs: BTreeMap::from([("docs/link".to_string(), "elsewhere".to_string())]),
		ignored: BTreeMap::from([
			("docs/build".to_string(), ignored("build/")),
			("docs/build/cache".to_string(), ignored("cache/")),
		]),
		ignored_default_untracked: 0,
		ignore_blocked: BTreeSet::new(),
	};
	let observed = LocalObservations {
		observed: BTreeMap::from([("docs".to_string(), LocalObservation::Dir(Box::new(scan)))]),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};

	// Twelve rows, two held: ten carried. The walk replaces `docs` and `docs/a.txt` — every
	// other row under it is pruned or held — and brings three nodes back. Subtracting the
	// nested root or the held row under `docs/build` a second time, or `docs/half.txt` not at
	// all, moves the one size the map may have.
	assert_eq!(assembly_bounds(&baseline, &observed, &held), 11..=11);
}

/// The check COUNTS the rows an observation replaces rather than reading them. After a
/// directory is renamed away those are every row it held, which `merge_local` walks right
/// after: reading them here as well was a second sweep of the subtree, for a number.
#[test]
fn the_assembly_check_counts_the_rows_it_replaces_without_reading_them() {
	const FILES: usize = 2_000;
	let hash = Blake3Hash::from([7u8; 32]);
	let baseline = Baseline::from_rows(
		std::iter::once(dir_row("logs", Uuid::new_v4()))
			.chain((0..FILES).map(|n| file_row(&format!("logs/{n:05}.txt"), Uuid::new_v4(), hash)))
			.chain([file_row("keep.txt", Uuid::new_v4(), hash)]),
	);
	// A hidden root takes its rows as an absence does, and unlike one it can be written here.
	let observed = LocalObservations {
		observed: BTreeMap::from([(
			"logs".to_string(),
			LocalObservation::Hidden(IgnoreDecision {
				level: IgnoreLevel::User,
				pattern: "logs/".to_string(),
			}),
		)]),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};
	let before = baseline.reads_for_test();
	let bounds = assembly_bounds(&baseline, &observed, &BTreeSet::new());
	let after = baseline.reads_for_test();
	assert_eq!(
		bounds,
		1..=1,
		"everything but `keep.txt` went with the hidden root"
	);
	let (statements, rows) = (after.0 - before.0, after.1 - before.1);
	assert!(
		statements <= 2 && rows <= 2,
		"the check read {rows} row(s) in {statements} statement(s) to count the {FILES} \
			 file(s) under the hidden root"
	);
}

/// The decided set's own check. A key in either map that no baseline row put there came from a
/// producer, and a producer that did not record it leaves a key the narrowed reconcile never
/// visits — no action at it, and nothing downstream that can tell that from agreement.
#[test]
fn the_decided_check_catches_a_key_no_producer_recorded() {
	let hash = Blake3Hash::from([9u8; 32]);
	let baseline = Baseline::from_rows([file_row("docs/a.txt", Uuid::new_v4(), hash)]);
	// A local producer's key: the node the observation found, off the path its row records.
	let moved: Side<LocalNode> = Side::from(HashMap::from([(
		"docs/moved.txt".to_string(),
		local_file("docs/moved.txt", hash),
	)]));
	// And a remote one: a path the delta placed that no row names.
	let placed: Side<RemoteNode> = Side::from(HashMap::from([(
		"docs/new".to_string(),
		remote_dir("docs/new", Uuid::new_v4()),
	)]));
	let empty_local: Side<LocalNode> = Side::default();
	let empty_remote: Side<RemoteNode> = Side::default();

	assert_eq!(
		unaccounted_key(&baseline, &moved, &empty_remote, &BTreeSet::new()).as_deref(),
		Some("docs/moved.txt"),
		"a local key nothing recorded"
	);
	assert_eq!(
		unaccounted_key(&baseline, &empty_local, &placed, &BTreeSet::new()).as_deref(),
		Some("docs/new"),
		"and the same on the remote side"
	);
	assert_eq!(
		unaccounted_key(
			&baseline,
			&moved,
			&placed,
			&BTreeSet::from(["docs/moved.txt".to_string(), "docs/new".to_string()])
		),
		None,
		"a producer that records what it moved is what the check is written for"
	);
	assert_eq!(
		unaccounted_key(
			&baseline,
			&Side::from(HashMap::from([(
				"docs/a.txt".to_string(),
				local_file("docs/a.txt", hash)
			)])),
			&empty_remote,
			&BTreeSet::new()
		),
		None,
		"a carried row needs no record: `from_baseline` only ever inserts at a row's path"
	);

	// And the same asked of a CARRIED side, which is where this check now runs. Only the
	// overlay is walked: the rows such a side derives answer for themselves and are no
	// producer's doing, which is what makes the check cost the change and not the tree.
	let mut produced: Side<LocalNode> = Side::carried();
	produced.insert(
		"docs/moved.txt".to_string(),
		local_file("docs/moved.txt", hash),
	);
	assert_eq!(
		unaccounted_key(&baseline, &produced, &empty_remote, &BTreeSet::new()).as_deref(),
		Some("docs/moved.txt"),
		"the overlay is where every producer writes, so that is where an unrecorded key is"
	);
	let mut at_a_row: Side<LocalNode> = Side::carried();
	at_a_row.insert("docs/a.txt".to_string(), local_file("docs/a.txt", hash));
	assert_eq!(
		unaccounted_key(&baseline, &at_a_row, &empty_remote, &BTreeSet::new()),
		None,
		"a key at a row's own path is no invention, whichever side holds it"
	);
}

fn dir_row(rel: &str, uuid: Uuid) -> BaselineEntry {
	BaselineEntry {
		kind: NodeKind::Dir,
		remote_uuid: Some(uuid),
		..synced_shell(rel)
	}
}

/// A synced file in its steady state: both sides agreed on `hash`, lineage id = uuid.
fn file_row(rel: &str, uuid: Uuid, hash: Blake3Hash) -> BaselineEntry {
	BaselineEntry {
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: Some(hash),
		..synced_file(rel, uuid, hash, 4)
	}
}

fn remote_dir(rel: &str, uuid: Uuid) -> RemoteNode {
	RemoteNode {
		rel_path: rel.to_string(),
		kind: NodeKind::Dir,
		remote_uuid: uuid,
		stable_uuid: None,
		content_hash: None,
		size: 0,
		modified_millis: 0,
	}
}

fn local_dir(rel: &str) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::Dir,
		size: 0,
		mtime_millis: 0,
		content_hash: None,
	}
}

fn local_file(rel: &str, hash: Blake3Hash) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		size: 4,
		mtime_millis: 0,
		content_hash: Some(hash),
	}
}

fn view(nodes: Vec<RemoteNode>, skipped: Vec<plan::SkippedRemote>, held: &[&str]) -> RemoteView {
	RemoteView {
		nodes: nodes
			.into_iter()
			.map(|node| (node.rel_path.clone(), node))
			.collect(),
		has_collisions: false,
		held_paths: held.iter().map(|path| path.to_string()).collect(),
		skipped,
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
	}
}

/// Whether `action` deletes `path`, directly or with a directory above it.
fn deletes(action: &SyncAction, path: &str) -> bool {
	action.is_delete() && (action.rel_path() == path || plan::is_under(path, action.rel_path()))
}

/// The uuids of a synced `docs/` holding `a.txt` and `x.bin`, and their contents.
struct Docs {
	dir: Uuid,
	a: Uuid,
	x: Uuid,
	a_hash: Blake3Hash,
	x_hash: Blake3Hash,
}

impl Docs {
	fn new() -> Self {
		Self {
			dir: Uuid::new_v4(),
			a: Uuid::new_v4(),
			x: Uuid::new_v4(),
			a_hash: Blake3Hash::from([1; 32]),
			x_hash: Blake3Hash::from([2; 32]),
		}
	}

	fn baseline(&self) -> HashMap<String, BaselineEntry> {
		HashMap::from([
			("docs".to_string(), dir_row("docs", self.dir)),
			(
				"docs/a.txt".to_string(),
				file_row("docs/a.txt", self.a, self.a_hash),
			),
			(
				"docs/x.bin".to_string(),
				file_row("docs/x.bin", self.x, self.x_hash),
			),
		])
	}

	fn local(&self, at: &str) -> Vec<LocalNode> {
		vec![
			local_dir(at),
			local_file(&format!("{at}/a.txt"), self.a_hash),
			local_file(&format!("{at}/x.bin"), self.x_hash),
		]
	}

	/// `x.bin` undecodable, so only the directory and `a.txt` are placed.
	fn remote_with_undecodable_x(&self, at: &str) -> RemoteView {
		view(
			vec![
				remote_dir(at, self.dir),
				remote_file(&format!("{at}/a.txt"), self.a, self.a_hash, 4),
			],
			vec![plan::SkippedRemote {
				remote_uuid: self.x,
				stable_uuid: Some(StableUuid::new_for_test(self.x)),
				rel_path: at.to_string(),
				path_is_dir: true,
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			&[],
		)
	}
}

/// A synced directory the remote renamed, holding a child whose remote item stopped decoding, is
/// still one local rename: the block on the child follows it into the renamed directory, so the
/// child is neither quarantined as a remote deletion nor pushed back over the item, and it is
/// reported where it now is on both sides.
#[test]
fn a_remote_dir_rename_carries_the_block_on_an_undecodable_child() {
	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		let docs = Docs::new();
		let (actions, unsyncable) = folded_pass(
			mode,
			docs.baseline(),
			docs.local("docs"),
			docs.remote_with_undecodable_x("documents"),
			HashMap::new(),
		);
		assert_eq!(
			actions,
			vec![SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			}],
			"{mode:?}"
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "documents/x.bin".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			"{mode:?}"
		);
	}
}

/// A remote move into a directory the local side does not have yet is still one move: the pass
/// creates the new parent first, and the undecodable child moves with its directory.
#[test]
fn a_remote_dir_move_into_a_new_parent_creates_it_first() {
	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		let docs = Docs::new();
		let mut remote = docs.remote_with_undecodable_x("fresh/documents");
		remote
			.nodes
			.insert("fresh".to_string(), remote_dir("fresh", Uuid::new_v4()));
		let (actions, unsyncable) = folded_pass(
			mode,
			docs.baseline(),
			docs.local("docs"),
			remote,
			HashMap::new(),
		);
		assert_eq!(
			actions,
			vec![
				SyncAction::CreateLocalDir {
					rel_path: "fresh".to_string(),
				},
				SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "fresh/documents".to_string(),
					kind: NodeKind::Dir,
				},
			],
			"{mode:?}"
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "fresh/documents/x.bin".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			"{mode:?}"
		);
	}
}

/// The push-side mirror: `mkdir fresh; mv docs fresh/documents` with an undecodable child is one
/// remote move after the create of its new parent, so the child's local copy is not uploaded as a
/// second, readable item.
#[test]
fn a_local_dir_move_into_a_new_parent_creates_it_first() {
	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		let docs = Docs::new();
		let mut local = docs.local("fresh/documents");
		local.push(local_dir("fresh"));
		let (actions, unsyncable) = folded_pass(
			mode,
			docs.baseline(),
			local,
			docs.remote_with_undecodable_x("docs"),
			HashMap::new(),
		);
		assert_eq!(
			actions,
			vec![
				SyncAction::CreateRemoteDir {
					rel_path: "fresh".to_string(),
				},
				SyncAction::MoveRemote {
					from_path: "docs".to_string(),
					to_path: "fresh/documents".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: docs.dir,
				},
			],
			"{mode:?}"
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "fresh/documents/x.bin".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			"{mode:?}"
		);
	}
}

/// The same remote move where the fold cannot run (a new local directory already sits at the
/// destination): the per-path plan quarantines the old directory, and that must not take the
/// undecodable child's local copy with it.
#[test]
fn a_directory_delete_never_takes_a_blocked_child_with_it() {
	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		let docs = Docs::new();
		let mut local = docs.local("docs");
		local.push(local_dir("documents"));
		let (actions, unsyncable) = folded_pass(
			mode,
			docs.baseline(),
			local,
			docs.remote_with_undecodable_x("documents"),
			HashMap::new(),
		);
		assert!(
			!actions.iter().any(|action| deletes(action, "docs/x.bin")),
			"{mode:?}: {actions:?}"
		);
		assert_eq!(
			unsyncable,
			vec![UnsyncablePath {
				rel_path: "docs/x.bin".to_string(),
				reason: UnsyncableReason::RemoteUndecodable,
			}],
			"{mode:?}"
		);
	}
}

/// A local rename of a directory holding a parked child is one remote move, and for the pass that
/// makes it the child stays parked and reported at its new path. The streak itself stays at the
/// old path in the store, so from the next pass the child is planned again, as after any rename.
#[test]
fn a_local_dir_rename_carries_the_park_on_a_failing_child() {
	let docs = Docs::new();
	let remote = view(
		vec![
			remote_dir("docs", docs.dir),
			remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
			// A later version of x.bin, the download that keeps failing.
			RemoteNode {
				stable_uuid: Some(StableUuid::new_for_test(docs.x)),
				..remote_file("docs/x.bin", Uuid::new_v4(), Blake3Hash::from([3; 32]), 4)
			},
		],
		Vec::new(),
		&[],
	);
	let now = Utc::now().timestamp_millis();
	let (actions, unsyncable) = folded_pass(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("documents"),
		remote,
		HashMap::from([("docs/x.bin".to_string(), failure(MAX_PATH_FAILURES, now))]),
	);
	assert_eq!(
		actions,
		vec![SyncAction::MoveRemote {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: docs.dir,
		}]
	);
	assert_eq!(
		unsyncable,
		vec![UnsyncablePath {
			rel_path: "documents/x.bin".to_string(),
			reason: UnsyncableReason::RepeatedFailure {
				attempts: MAX_PATH_FAILURES,
				last_error: "boom".to_string(),
			},
		}]
	);

	// The next pass: the move committed the rows at the new path, but the stored streak is still
	// keyed at the old one, so the child is planned again instead of being reported as parked. That
	// stale streak is still reported at the old path until `retry_path` clears it.
	let baseline = docs
		.baseline()
		.into_values()
		.map(|mut row| {
			row.rel_path = row.rel_path.replacen("docs", "documents", 1);
			(row.rel_path.clone(), row)
		})
		.collect();
	let remote = view(
		vec![
			remote_dir("documents", docs.dir),
			remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
			RemoteNode {
				stable_uuid: Some(StableUuid::new_for_test(docs.x)),
				..remote_file(
					"documents/x.bin",
					Uuid::new_v4(),
					Blake3Hash::from([3; 32]),
					4,
				)
			},
		],
		Vec::new(),
		&[],
	);
	let (actions, unsyncable) = folded_pass(
		SyncMode::TwoWay,
		baseline,
		docs.local("documents"),
		remote,
		HashMap::from([("docs/x.bin".to_string(), failure(MAX_PATH_FAILURES, now))]),
	);
	assert!(
		actions
			.iter()
			.any(|action| action.rel_path() == "documents/x.bin"),
		"{actions:?}"
	);
	assert_eq!(
		unsyncable,
		vec![UnsyncablePath {
			rel_path: "docs/x.bin".to_string(),
			reason: UnsyncableReason::RepeatedFailure {
				attempts: MAX_PATH_FAILURES,
				last_error: "boom".to_string(),
			},
		}],
		"the streak stays reported where the store keeps it"
	);
}

/// A remote rename of `docs/` to `new/` with `b/` moved into it, while a child of `docs/` stopped
/// decoding, folds into both moves in order, so the child follows its directory instead of
/// holding back a deletion of `docs/` the user never made. The same holds for the local mirror.
#[test]
fn a_dir_moved_into_another_moved_dir_carries_an_undecodable_child() {
	let b = Uuid::new_v4();
	let b_file = Uuid::new_v4();
	let b_hash = Blake3Hash::from([7; 32]);
	let b_rows = |at: &str| {
		[
			(at.to_string(), dir_row(at, b)),
			(
				format!("{at}/1.txt"),
				file_row(&format!("{at}/1.txt"), b_file, b_hash),
			),
		]
	};
	let expected_unsyncable = vec![UnsyncablePath {
		rel_path: "new/x.bin".to_string(),
		reason: UnsyncableReason::RemoteUndecodable,
	}];

	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		let docs = Docs::new();
		let mut baseline = docs.baseline();
		baseline.extend(b_rows("b"));
		let mut local = docs.local("docs");
		local.extend([local_dir("b"), local_file("b/1.txt", b_hash)]);
		let mut remote = docs.remote_with_undecodable_x("new");
		remote.nodes.extend([
			("new/b".to_string(), remote_dir("new/b", b)),
			(
				"new/b/1.txt".to_string(),
				remote_file("new/b/1.txt", b_file, b_hash, 4),
			),
		]);
		let (actions, unsyncable) = folded_pass(mode, baseline, local, remote, HashMap::new());
		assert_eq!(
			actions,
			vec![
				SyncAction::MoveLocal {
					from_path: "docs".to_string(),
					to_path: "new".to_string(),
					kind: NodeKind::Dir,
				},
				SyncAction::MoveLocal {
					from_path: "b".to_string(),
					to_path: "new/b".to_string(),
					kind: NodeKind::Dir,
				},
			],
			"pull, {mode:?}"
		);
		assert_eq!(unsyncable, expected_unsyncable, "pull, {mode:?}");
	}

	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		let docs = Docs::new();
		let mut baseline = docs.baseline();
		baseline.extend(b_rows("b"));
		let mut local = docs.local("new");
		local.extend([local_dir("new/b"), local_file("new/b/1.txt", b_hash)]);
		let mut remote = docs.remote_with_undecodable_x("docs");
		remote.nodes.extend([
			("b".to_string(), remote_dir("b", b)),
			(
				"b/1.txt".to_string(),
				remote_file("b/1.txt", b_file, b_hash, 4),
			),
		]);
		let (actions, unsyncable) = folded_pass(mode, baseline, local, remote, HashMap::new());
		assert_eq!(
			actions,
			vec![
				SyncAction::MoveRemote {
					from_path: "docs".to_string(),
					to_path: "new".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: docs.dir,
				},
				SyncAction::MoveRemote {
					from_path: "b".to_string(),
					to_path: "new/b".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: b,
				},
			],
			"push, {mode:?}"
		);
		assert_eq!(unsyncable, expected_unsyncable, "push, {mode:?}");
	}
}

/// A directory renamed only by case while the cache lists a child of it twice is not renamed
/// this pass, as a move to another path is not, and the directory at the old spelling is not
/// trashed with the held child in it.
#[test]
fn a_case_only_dir_rename_waits_out_a_held_child() {
	let docs = Docs::new();
	let mut baseline = docs.baseline();
	baseline.remove("docs/x.bin");
	let baseline = baseline
		.into_values()
		.map(|mut row| {
			row.rel_path = row.rel_path.replacen("docs", "Docs", 1);
			(row.rel_path.clone(), row)
		})
		.collect();
	let (actions, _) = folded_pass(
		SyncMode::TwoWay,
		baseline,
		vec![local_dir("docs"), local_file("docs/a.txt", docs.a_hash)],
		view(
			vec![
				remote_dir("Docs", docs.dir),
				remote_file("Docs/a.txt", docs.a, docs.a_hash, 4),
			],
			Vec::new(),
			&["Docs/a.txt"],
		),
		HashMap::new(),
	);
	assert!(
		!actions.iter().any(|action| matches!(
			action,
			SyncAction::MoveRemote {
				kind: NodeKind::Dir,
				..
			} | SyncAction::MoveLocal {
				kind: NodeKind::Dir,
				..
			}
		)),
		"{actions:?}"
	);
	assert!(
		!actions.iter().any(|action| deletes(action, "Docs/a.txt")),
		"{actions:?}"
	);
}

/// Only a LOCAL deletion waits for a child whose remote item is out of reach, because only there
/// is the local copy the one readable copy left. A local `rm -r` still trashes the remote
/// directory (recoverably) around an undecodable child, and a remote deletion around a rejected
/// local name still quarantines the local directory (recoverably) as before.
#[test]
fn only_a_local_deletion_waits_for_an_unreachable_child() {
	let docs = Docs::new();
	let (actions, _) = folded_pass(
		SyncMode::TwoWay,
		docs.baseline(),
		Vec::new(),
		docs.remote_with_undecodable_x("docs"),
		HashMap::new(),
	);
	assert!(
		actions.iter().any(|action| matches!(
			action,
			SyncAction::TrashRemote { rel_path, .. } if rel_path == "docs"
		)),
		"{actions:?}"
	);

	let mut baseline = docs.baseline();
	baseline.remove("docs/x.bin");
	let mut prep = prepared(
		SyncMode::TwoWay,
		baseline,
		vec![local_dir("docs"), local_file("docs/a.txt", docs.a_hash)],
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	prep.local_scan
		.invalid_names
		.insert("docs/CON".to_string(), "reserved".to_string());
	let (actions, _) = run_prepared(prep);
	assert!(
		actions.iter().any(|action| matches!(
			action,
			SyncAction::DeleteLocal { rel_path, .. } if rel_path == "docs"
		)),
		"{actions:?}"
	);
}

/// The remote replaced a synced directory with a file while an undecodable child of it is still
/// on the server. Withholding the local deletion alone would let the download's own stash take the
/// directory, child and all, so the download waits with it.
#[test]
fn a_type_flip_waits_with_the_deletion_it_replaces() {
	let docs = Docs::new();
	let remote = view(
		vec![remote_file(
			"docs",
			Uuid::new_v4(),
			Blake3Hash::from([9; 32]),
			4,
		)],
		vec![plan::SkippedRemote {
			remote_uuid: docs.x,
			stable_uuid: Some(StableUuid::new_for_test(docs.x)),
			rel_path: "elsewhere".to_string(),
			path_is_dir: false,
			reason: UnsyncableReason::RemoteBrokenParent,
		}],
		&[],
	);
	let (actions, _) = folded_pass(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("docs"),
		remote,
		HashMap::new(),
	);
	assert!(
		!actions.iter().any(|action| action.rel_path() == "docs"),
		"{actions:?}"
	);
}

/// A park on the renamed directory itself is cleared by the rename, as any rename clears one: it
/// does not follow the directory and hold back the move that carries it.
#[test]
fn a_park_on_a_renamed_directory_does_not_hold_back_its_move() {
	let docs = Docs::new();
	let remote = view(
		vec![
			remote_dir("documents", docs.dir),
			remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
			remote_file("documents/x.bin", docs.x, docs.x_hash, 4),
		],
		Vec::new(),
		&[],
	);
	let now = Utc::now().timestamp_millis();
	let (actions, unsyncable) = folded_pass(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("docs"),
		remote,
		HashMap::from([("docs".to_string(), failure(MAX_PATH_FAILURES, now))]),
	);
	assert_eq!(
		actions,
		vec![SyncAction::MoveLocal {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
		}]
	);
	assert!(unsyncable.is_empty(), "{unsyncable:?}");
}

/// A remote rename folded into a local move carries what the local scan could not sync under the
/// directory — a rejected name, a symlink alias and its target — to the new path.
#[test]
fn a_local_move_carries_rejected_names_and_aliases() {
	let docs = Docs::new();
	let sub = Uuid::new_v4();
	let mut baseline = docs.baseline();
	baseline.insert("docs/sub".to_string(), dir_row("docs/sub", sub));
	let mut local = docs.local("docs");
	local.push(local_dir("docs/sub"));
	let remote = view(
		vec![
			remote_dir("documents", docs.dir),
			remote_dir("documents/sub", sub),
			remote_file("documents/a.txt", docs.a, docs.a_hash, 4),
			remote_file("documents/x.bin", docs.x, docs.x_hash, 4),
		],
		Vec::new(),
		&[],
	);
	let mut prep = prepared(SyncMode::TwoWay, baseline, local, remote, HashMap::new());
	prep.facts
		.invalid_names
		.insert("docs/CON".to_string(), "reserved".to_string());
	prep.facts
		.aliased_dirs
		.insert("docs/link".to_string(), "docs/sub".to_string());
	let (actions, unsyncable) = run_prepared(prep);
	assert_eq!(
		actions,
		vec![SyncAction::MoveLocal {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
		}]
	);
	assert_eq!(
		unsyncable,
		vec![
			UnsyncablePath {
				rel_path: "documents/CON".to_string(),
				reason: UnsyncableReason::InvalidName {
					detail: "reserved".to_string(),
				},
			},
			UnsyncablePath {
				rel_path: "documents/link".to_string(),
				reason: UnsyncableReason::LocalAlias {
					target: "documents/sub".to_string(),
				},
			},
		]
	);
}

/// A local rename folded into a remote move carries the report of a never-synced remote item
/// under the directory to the new path.
#[test]
fn a_remote_move_carries_never_synced_remote_reports() {
	let docs = Docs::new();
	let remote = view(
		vec![
			remote_dir("docs", docs.dir),
			remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
			remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
		],
		vec![plan::SkippedRemote {
			remote_uuid: Uuid::new_v4(),
			stable_uuid: None,
			rel_path: "docs".to_string(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteUndecodable,
		}],
		&[],
	);
	let (actions, unsyncable) = folded_pass(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("documents"),
		remote,
		HashMap::new(),
	);
	assert_eq!(
		actions,
		vec![SyncAction::MoveRemote {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: docs.dir,
		}]
	);
	assert_eq!(
		unsyncable,
		vec![UnsyncablePath {
			rel_path: "documents".to_string(),
			reason: UnsyncableReason::RemoteUndecodable,
		}]
	);
}

const MODES: [SyncMode; 5] = [
	SyncMode::TwoWay,
	SyncMode::LocalToRemote,
	SyncMode::RemoteToLocal,
	SyncMode::LocalBackup,
	SyncMode::RemoteBackup,
];

/// An ignore decision at `level` whose deciding line is `pattern`.
fn hidden_by(level: IgnoreLevel, pattern: &str) -> IgnoreDecision {
	IgnoreDecision {
		level,
		pattern: pattern.to_string(),
	}
}

/// A decision by the `pattern` line of the root `.filenignore`.
fn by_root_file(pattern: &str) -> IgnoreDecision {
	hidden_by(IgnoreLevel::File { dir: String::new() }, pattern)
}

/// `docs/` synced and now ignored by the root `.filenignore`, on both sides.
fn ignoring_docs(prep: &mut Prepared) {
	let decision = by_root_file("docs/");
	prep.facts
		.ignored_local
		.insert("docs".to_string(), decision.clone());
	prep.facts
		.ignored_remote
		.insert("docs".to_string(), decision);
}

/// The baseline rows a pass that found `roots` ignored leaves behind.
fn untracked(
	mut baseline: HashMap<String, BaselineEntry>,
	roots: &BTreeSet<String>,
) -> HashMap<String, BaselineEntry> {
	baseline.retain(|path, _| {
		!roots
			.iter()
			.any(|root| path == root || plan::is_under(path, root))
	});
	baseline
}

/// A synced directory that becomes ignored plans nothing in any mode, whether the two sides hide
/// it or still show a change under it: a local deletion and a remote edit are both left alone,
/// and the pass untracks exactly that directory.
#[test]
fn a_synced_directory_that_becomes_ignored_plans_nothing_in_any_mode() {
	let docs = Docs::new();
	let edited_x = remote_file("docs/x.bin", Uuid::new_v4(), Blake3Hash::from([9; 32]), 4);
	for mode in MODES {
		let hidden = prepared(
			mode,
			docs.baseline(),
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		let changed = prepared(
			mode,
			docs.baseline(),
			vec![local_dir("docs"), local_file("docs/x.bin", docs.x_hash)],
			view(
				vec![
					remote_dir("docs", docs.dir),
					remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
					edited_x.clone(),
				],
				Vec::new(),
				&[],
			),
			HashMap::new(),
		);
		for (what, mut prep) in [("hidden", hidden), ("changed", changed)] {
			ignoring_docs(&mut prep);
			assert_eq!(
				prep.ignored_roots(),
				BTreeSet::from(["docs".to_string()]),
				"{mode:?} {what}"
			);
			let (actions, unsyncable) = run_prepared(prep);
			assert!(actions.is_empty(), "{mode:?} {what}: {actions:?}");
			assert!(unsyncable.is_empty(), "{mode:?} {what}: {unsyncable:?}");
		}
		// Without the rules the same hidden tree reads as deleted from both sides.
		let (actions, _) = run_prepared(prepared(
			mode,
			docs.baseline(),
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		));
		assert!(!actions.is_empty(), "{mode:?}");
	}
}

/// Once a pass has untracked an ignored directory, removing the rule reads its copies like a first
/// sync: identical ones are adopted with no transfer in every mode, and differing ones are a
/// conflict in a two-way pair.
#[test]
fn an_unignored_directory_reads_like_a_first_sync() {
	let docs = Docs::new();
	let mut ignored = prepared(
		SyncMode::TwoWay,
		docs.baseline(),
		Vec::new(),
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	ignoring_docs(&mut ignored);
	let baseline = untracked(docs.baseline(), &ignored.ignored_roots());
	assert!(baseline.is_empty(), "{baseline:?}");

	let remote = |x: RemoteNode| {
		view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				x,
			],
			Vec::new(),
			&[],
		)
	};
	for mode in MODES {
		let (actions, _) = folded_pass(
			mode,
			baseline.clone(),
			docs.local("docs"),
			remote(remote_file("docs/x.bin", docs.x, docs.x_hash, 4)),
			HashMap::new(),
		);
		assert!(
			!actions.is_empty()
				&& actions
					.iter()
					.all(|action| matches!(action, SyncAction::AdoptBaseline { .. })),
			"{mode:?}: {actions:?}"
		);
	}

	let mut prep = prepared(
		SyncMode::TwoWay,
		baseline,
		docs.local("docs"),
		remote(remote_file(
			"docs/x.bin",
			Uuid::new_v4(),
			Blake3Hash::from([9; 32]),
			4,
		)),
		HashMap::new(),
	);
	let screened = screened_both(&mut prep);
	assert_eq!(
		screened
			.conflicts
			.iter()
			.map(|conflict| conflict.rel_path.as_str())
			.collect::<Vec<_>>(),
		vec!["docs/x.bin"]
	);
}

/// The report names the top of each ignored subtree once across both sides, with the line that
/// hid it, says whether a baseline row sat at or under it, and leaves out what only the defaults
/// hide unless it was tracked. A sibling that sorts between a directory and its children is still
/// reported. Where the two sides hide one path with different lines, the local one is reported.
#[test]
fn the_report_lists_each_ignored_top_once_and_skips_untracked_defaults() {
	let docs = Docs::new();
	let mut baseline = docs.baseline();
	baseline.insert(
		"old.swp".to_string(),
		file_row("old.swp", Uuid::new_v4(), Blake3Hash::from([3; 32])),
	);
	let mut prep = prepared(
		SyncMode::TwoWay,
		baseline,
		Vec::new(),
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	prep.facts.ignored_local.extend([
		("docs".to_string(), by_root_file("docs/")),
		("docs b".to_string(), hidden_by(IgnoreLevel::User, "docs ?")),
		(
			"sub/.DS_Store".to_string(),
			hidden_by(IgnoreLevel::Default, ".DS_Store"),
		),
	]);
	prep.facts.ignored_remote.extend([
		// The same path, hidden there by another line: the local side's is the one reported.
		("docs".to_string(), hidden_by(IgnoreLevel::User, "*ocs")),
		(
			"docs/x.bin".to_string(),
			hidden_by(IgnoreLevel::User, "*.bin"),
		),
		("docs b/c".to_string(), hidden_by(IgnoreLevel::User, "c")),
		(
			"old.swp".to_string(),
			hidden_by(IgnoreLevel::Default, "*.swp"),
		),
	]);
	let ignored = |rel_path: &str, decision: IgnoreDecision, tracked: bool| IgnoredPath {
		rel_path: rel_path.to_string(),
		level: decision.level,
		pattern: decision.pattern,
		tracked,
	};
	assert_eq!(
		prep.ignored(),
		vec![
			ignored("docs", by_root_file("docs/"), true),
			ignored("docs b", hidden_by(IgnoreLevel::User, "docs ?"), false),
			ignored("old.swp", hidden_by(IgnoreLevel::Default, "*.swp"), true),
		]
	);
	assert_eq!(
		ignored("docs", by_root_file("docs/"), true).to_string(),
		r#"ignored "docs" (by .filenignore: docs/), no longer synced"#
	);
}

/// A directory deleted on one side keeps its own copy on the other while a user or `.filenignore`
/// rule hides something in it there, and counts that as deferred; its visible children are still
/// deleted. Content only the built-in defaults hide goes with the directory.
#[test]
fn a_deleted_directory_keeps_ignored_children_unless_only_defaults_hide_them() {
	let docs = Docs::new();
	for (level, kept) in [
		(IgnoreLevel::User, true),
		(
			IgnoreLevel::File {
				dir: "docs".to_string(),
			},
			true,
		),
		(IgnoreLevel::Default, false),
	] {
		let remote_side = view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		);
		// Trashed on the remote, ignored content left locally; then deleted locally, ignored
		// content left on the remote.
		let mut pulled = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			docs.local("docs"),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		pulled.facts.ignored_local.insert(
			"docs/node_modules".to_string(),
			hidden_by(level.clone(), "node_modules/"),
		);
		let mut pushed = prepared(
			SyncMode::TwoWay,
			docs.baseline(),
			Vec::new(),
			remote_side,
			HashMap::new(),
		);
		pushed.facts.ignored_remote.insert(
			"docs/node_modules".to_string(),
			hidden_by(level.clone(), "node_modules/"),
		);

		for (side, mut prep) in [("pulled", pulled), ("pushed", pushed)] {
			let screened = screened_both(&mut prep);
			let actions: Vec<&SyncAction> = screened
				.decision
				.safe
				.iter()
				.chain(&screened.decision.held)
				.collect();
			let deletes = |path: &str| {
				actions
					.iter()
					.any(|action| action.is_delete() && action.rel_path() == path)
			};
			assert_eq!(!deletes("docs"), kept, "{level:?} {side}: {actions:?}");
			assert!(
				deletes("docs/a.txt") && deletes("docs/x.bin"),
				"{level:?} {side}: {actions:?}"
			);
			assert_eq!(
				screened.deferred_paths,
				usize::from(kept),
				"{level:?} {side}"
			);
		}
	}
}

/// A path that stops being ignored syncs like a first sync in the one-way modes too: what only the
/// destination holds there has no row, and its deletion is held for approval as a first sync's
/// is, although the pair tracks other rows. A path the previous pass did not ignore gets no such
/// hold, and one a rule still hides plans no deletion at all.
#[test]
fn a_deletion_under_a_path_that_stops_being_ignored_is_held_like_a_first_sync() {
	let docs = Docs::new();
	let psd = Blake3Hash::from([7; 32]);
	let docs_remote = |extra: Option<RemoteNode>| {
		let mut nodes = vec![
			remote_dir("docs", docs.dir),
			remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
			remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
		];
		nodes.extend(extra);
		view(nodes, Vec::new(), &[])
	};
	let pushed = || {
		prepared(
			SyncMode::LocalToRemote,
			docs.baseline(),
			docs.local("docs"),
			docs_remote(Some(remote_file("a.psd", Uuid::new_v4(), psd, 4))),
			HashMap::new(),
		)
	};
	let pulled = || {
		let mut local = docs.local("docs");
		local.push(local_file("a.psd", psd));
		prepared(
			SyncMode::RemoteToLocal,
			docs.baseline(),
			local,
			docs_remote(None),
			HashMap::new(),
		)
	};
	let deletes_psd = |screened: &ResidentScreened| {
		screened
			.decision
			.safe
			.iter()
			.chain(&screened.decision.held)
			.any(|action| action.is_delete() && action.rel_path() == "a.psd")
	};
	for (side, build) in [
		("pushed", &pushed as &dyn Fn() -> Prepared),
		("pulled", &pulled),
	] {
		let mut plain = build();
		let screened = screened_both(&mut plain);
		assert!(
			screened.decision.reason.is_none() && deletes_psd(&screened),
			"{side}: {:?}",
			screened.decision
		);

		let mut unignored = build();
		unignored.last_ignored.insert("a.psd".to_string());
		let screened = screened_both(&mut unignored);
		assert_eq!(
			screened.decision.reason,
			Some(GuardReason::FirstSyncWithDeletions { deletions: 1 }),
			"{side}: {:?}",
			screened.decision
		);
		assert!(
			screened
				.decision
				.held
				.iter()
				.any(|action| action.rel_path() == "a.psd"),
			"{side}"
		);

		let mut still = build();
		still.last_ignored.insert("a.psd".to_string());
		still
			.facts
			.ignored_local
			.insert("a.psd".to_string(), hidden_by(IgnoreLevel::User, "*.psd"));
		still
			.facts
			.ignored_remote
			.insert("a.psd".to_string(), hidden_by(IgnoreLevel::User, "*.psd"));
		let screened = screened_both(&mut still);
		assert!(
			screened.decision.reason.is_none() && !deletes_psd(&screened),
			"{side}: {:?}",
			screened.decision
		);
	}
}

/// A pass records for the next one every root it ignores, a recorded root it still hides (under a
/// hidden root, or under rules it could not read), and an un-ignored root only while a deletion
/// under it is held.
#[test]
fn a_pass_records_the_ignored_roots_the_next_pass_needs() {
	let docs = Docs::new();
	let mut prep = prepared(
		SyncMode::LocalToRemote,
		docs.baseline(),
		Vec::new(),
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	prep.facts
		.ignored_local
		.insert("build".to_string(), hidden_by(IgnoreLevel::User, "build/"));
	prep.facts.ignore_blocked.insert("locked".to_string());
	prep.last_ignored = ["build/cache", "locked/tmp", "held", "released"]
		.map(String::from)
		.into();
	assert_eq!(prep.unignored(), BTreeSet::from(["held", "released"]));
	assert_eq!(
		prep.ignored_roots_to_record(["held/x.psd"]),
		["build", "build/cache", "locked/tmp", "held"]
			.map(String::from)
			.into()
	);
}

/// The `.filenignore` whose rules keep a withheld directory stays with it, or the next pass would
/// have no rule and take the ignored content down with the directory. A rule file above the
/// directory is not the directory's to keep.
#[test]
fn a_withheld_directory_keeps_the_rule_files_that_keep_it() {
	let file_level = |dir: &str, pattern: &str| {
		hidden_by(
			IgnoreLevel::File {
				dir: dir.to_string(),
			},
			pattern,
		)
	};
	let ignored = BTreeMap::from([
		(
			"proj/node_modules".to_string(),
			file_level("proj", "node_modules/"),
		),
		(
			"proj/sub/cache".to_string(),
			file_level("proj/sub", "cache/"),
		),
		("proj/build".to_string(), file_level("", "proj/build/")),
	]);
	let none = BTreeMap::new();
	let paths = [
		("proj/.filenignore", NodeKind::File),
		("proj/sub/.filenignore", NodeKind::File),
		("proj/a.txt", NodeKind::File),
		("proj/sub", NodeKind::Dir),
		("proj", NodeKind::Dir),
		(".filenignore", NodeKind::File),
	];
	for local in [true, false] {
		let actions = paths
			.iter()
			.map(|&(rel_path, kind)| {
				let rel_path = rel_path.to_string();
				if local {
					SyncAction::DeleteLocal { rel_path, kind }
				} else {
					SyncAction::TrashRemote {
						rel_path,
						kind,
						remote_uuid: Uuid::nil(),
					}
				}
			})
			.collect();
		let sides = if local {
			[&ignored, &none]
		} else {
			[&none, &ignored]
		};
		let (kept, deferred) = withhold_deletions_over_unreachable(
			actions,
			&BTreeMap::new(),
			&BTreeSet::new(),
			sides,
			&BTreeSet::new(),
		);
		let kept: Vec<&str> = kept.iter().map(SyncAction::rel_path).collect();
		assert_eq!(kept, vec!["proj/a.txt", ".filenignore"], "local: {local}");
		assert_eq!(deferred, 2, "local: {local}");
	}
}

/// A directory deleted on one side keeps its copy on the other while a subtree in it has rules
/// this pass could not read: what those rules hide cannot be told apart from what they do not.
#[test]
fn a_deleted_directory_keeps_a_subtree_whose_rules_could_not_be_read() {
	let docs = Docs::new();
	let mut pulled = prepared(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("docs"),
		view(Vec::new(), Vec::new(), &[]),
		HashMap::new(),
	);
	pulled.facts.ignore_blocked.insert("docs/sub".to_string());
	let mut pushed = prepared(
		SyncMode::TwoWay,
		docs.baseline(),
		Vec::new(),
		view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		),
		HashMap::new(),
	);
	pushed.facts.ignore_blocked.insert("docs/sub".to_string());

	for (side, mut prep) in [("pulled", pulled), ("pushed", pushed)] {
		let screened = screened_both(&mut prep);
		let actions: Vec<&SyncAction> = screened
			.decision
			.safe
			.iter()
			.chain(&screened.decision.held)
			.collect();
		let deletes = |path: &str| {
			actions
				.iter()
				.any(|action| action.is_delete() && action.rel_path() == path)
		};
		assert!(!deletes("docs"), "{side}: {actions:?}");
		assert!(deletes("docs/a.txt"), "{side}: {actions:?}");
		assert_eq!(screened.deferred_paths, 1, "{side}");
		assert!(
			screened.dropped > 0,
			"{side}: the withheld deletion was planned and dropped, so the next pass has to \
				 read everything to plan it again"
		);
	}
}

/// A file moved out of an ignored directory is not paired with its tracked copy there: that copy
/// is neither moved nor deleted, and once the pass has untracked the directory the file at its new
/// path is an ordinary new item.
#[test]
fn a_move_out_of_an_ignored_directory_leaves_the_ignored_copy_alone() {
	let docs = Docs::new();
	let docs_remote = || {
		view(
			vec![
				remote_dir("docs", docs.dir),
				remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
				remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		)
	};
	for mode in [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::LocalBackup,
	] {
		// Moved locally from docs/a.txt to a.txt; the local scan no longer shows docs/.
		let local = vec![local_file("a.txt", docs.a_hash)];
		let mut prep = prepared(
			mode,
			docs.baseline(),
			local.clone(),
			docs_remote(),
			HashMap::new(),
		);
		prep.facts
			.ignored_local
			.insert("docs".to_string(), hidden_by(IgnoreLevel::User, "docs/"));
		let roots = prep.ignored_roots();
		let (actions, _) = run_prepared(prep);
		assert!(actions.is_empty(), "{mode:?}: {actions:?}");

		let (actions, _) = folded_pass(
			mode,
			untracked(docs.baseline(), &roots),
			local,
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		assert_eq!(actions, vec![upload("a.txt")], "{mode:?}");
	}
	for mode in [
		SyncMode::TwoWay,
		SyncMode::RemoteToLocal,
		SyncMode::RemoteBackup,
	] {
		// Moved on the remote from docs/a.txt to a.txt; the view no longer shows docs/.
		let mut prep = prepared(
			mode,
			docs.baseline(),
			docs.local("docs"),
			view(
				vec![remote_file("a.txt", docs.a, docs.a_hash, 4)],
				Vec::new(),
				&[],
			),
			HashMap::new(),
		);
		prep.facts
			.ignored_remote
			.insert("docs".to_string(), hidden_by(IgnoreLevel::User, "docs/"));
		let (actions, _) = run_prepared(prep);
		assert!(actions.is_empty(), "{mode:?}: {actions:?}");
	}
}

/// A directory move carries the ignored roots under it to the new path, each set keyed like the
/// side it was read from, and the directory of the `.filenignore` that decided one with it. The
/// deciding line moves unchanged: it is reported as it matched, so a path-shaped line keeps
/// naming the pre-move path until the next pass decides again.
#[test]
fn a_dir_move_carries_the_ignored_roots_under_it() {
	let docs = Docs::new();
	let remote_at = |at: &str| {
		view(
			vec![
				remote_dir(at, docs.dir),
				remote_file(&format!("{at}/a.txt"), docs.a, docs.a_hash, 4),
				remote_file(&format!("{at}/x.bin"), docs.x, docs.x_hash, 4),
			],
			Vec::new(),
			&[],
		)
	};
	let in_docs = |dir: &str| {
		hidden_by(
			IgnoreLevel::File {
				dir: dir.to_string(),
			},
			"build/",
		)
	};
	let by_user = || hidden_by(IgnoreLevel::User, "cache/");

	// Renamed on the remote: the local scan is re-keyed, the view already reads the new path.
	let mut prep = prepared(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("docs"),
		remote_at("documents"),
		HashMap::new(),
	);
	prep.facts
		.ignored_local
		.insert("docs/build".to_string(), in_docs("docs"));
	prep.facts
		.ignored_remote
		.insert("documents/cache".to_string(), by_user());
	prep.facts
		.ignore_blocked
		.insert("docs/unreadable".to_string());
	prep.fold_dir_moves().unwrap();
	assert_eq!(
		prep.dir_moves,
		vec![SyncAction::MoveLocal {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
		}]
	);
	assert_eq!(
		prep.facts.ignored_local,
		BTreeMap::from([("documents/build".to_string(), in_docs("documents"))])
	);
	assert_eq!(
		prep.facts.ignored_remote,
		BTreeMap::from([("documents/cache".to_string(), by_user())])
	);
	assert_eq!(
		prep.facts.ignore_blocked,
		BTreeSet::from(["documents/unreadable".to_string()])
	);

	// Renamed locally: the view is re-keyed, the local scan already reads the new path.
	let mut prep = prepared(
		SyncMode::TwoWay,
		docs.baseline(),
		docs.local("documents"),
		remote_at("docs"),
		HashMap::new(),
	);
	prep.facts
		.ignored_local
		.insert("documents/build".to_string(), by_user());
	prep.facts
		.ignored_remote
		.insert("docs/cache".to_string(), in_docs("docs"));
	prep.facts
		.ignore_blocked
		.insert("docs/unreadable".to_string());
	prep.fold_dir_moves().unwrap();
	assert!(
		matches!(&prep.dir_moves[..], [SyncAction::MoveRemote { to_path, .. }] if to_path == "documents"),
		"{:?}",
		prep.dir_moves
	);
	assert_eq!(
		prep.facts.ignored_local,
		BTreeMap::from([("documents/build".to_string(), by_user())])
	);
	assert_eq!(
		prep.facts.ignored_remote,
		BTreeMap::from([("documents/cache".to_string(), in_docs("documents"))])
	);
	assert_eq!(
		prep.facts.ignore_blocked,
		BTreeSet::from(["documents/unreadable".to_string()])
	);
}

/// A root `.filenignore` of `*` hides every item on both sides. The remote is not read as emptied,
/// because that is decided on the unfiltered view, and no deletion is planned in any mode.
#[test]
fn ignoring_everything_neither_reads_as_an_emptied_remote_nor_deletes() {
	let docs = Docs::new();
	let raw = view(
		vec![
			remote_dir("docs", docs.dir),
			remote_file("docs/a.txt", docs.a, docs.a_hash, 4),
			remote_file("docs/x.bin", docs.x, docs.x_hash, 4),
		],
		Vec::new(),
		&[],
	);
	assert!(!remote_emptied(&raw.nodes.whole(), &tree(&docs.baseline())));
	assert!(
		remote_emptied(&HashMap::new(), &tree(&docs.baseline())),
		"the filtered view alone would read as a vanished remote"
	);
	for mode in MODES {
		let mut prep = prepared(
			mode,
			docs.baseline(),
			Vec::new(),
			view(Vec::new(), Vec::new(), &[]),
			HashMap::new(),
		);
		prep.remote_emptied = remote_emptied(&raw.nodes.whole(), &prep.baseline);
		ignoring_docs(&mut prep);
		assert!(screen_state(&prep).absence_trusted(), "{mode:?}");
		let (actions, _) = run_prepared(prep);
		assert!(actions.is_empty(), "{mode:?}: {actions:?}");
	}
}

/// A mode switch that adopts the destination's standing copies rewrites baseline rows for every
/// path it reads as gone on the source side, so it may only run on evidence a pass would act on.
/// Untrusted, the switch is worse than useless: it adopts nothing and then propagates the whole
/// backlog the caller asked to keep, or adopts a whole pair from whichever side a transient fault
/// left listing something.
#[test]
fn adopting_the_destination_is_refused_on_evidence_a_pass_would_not_act_on() {
	let healthy = guard::ScreenState {
		scan_complete: true,
		remote: guard::RemoteEvidence::Listed,
		first_sync: false,
		tracked: 5,
	};
	assert_eq!(adoption_refusal(None, &healthy), None);

	for (state, expected) in [
		(
			guard::ScreenState {
				scan_complete: false,
				..healthy
			},
			"the local scan is incomplete",
		),
		(
			guard::ScreenState {
				remote: guard::RemoteEvidence::Unconverged,
				..healthy
			},
			"the remote view has never converged",
		),
		(
			guard::ScreenState {
				remote: guard::RemoteEvidence::Emptied,
				..healthy
			},
			"the remote view came back wholly empty",
		),
	] {
		assert_eq!(adoption_refusal(None, &state).as_deref(), Some(expected));
	}

	// A name collision makes a 1:1 mapping impossible, so it refuses the switch for the same
	// reason it refuses a pass — even with everything else healthy.
	assert_eq!(
		adoption_refusal(Some(RefuseReason::LocalCollision), &healthy).as_deref(),
		Some(RefuseReason::LocalCollision.to_string().as_str())
	);
}

fn scan_root(tag: &str) -> PathBuf {
	let root = std::env::temp_dir().join(format!("filen_engine_{tag}_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	root
}

/// Reconcile a two-way pass over a real local scan, screened exactly as a pass screens it.
fn planned_over_scan(
	root: &Path,
	baseline: &HashMap<String, BaselineEntry>,
	remote: &impl Nodes<Node = RemoteNode>,
) -> Vec<SyncAction> {
	let (scan, _) = scan::scan_local(
		ScanRoot {
			path: root,
			pair_root: PairRoot::Directory,
		},
		&Baseline::default(),
		IgnoreRules::default(),
		RuleFiles::Read,
	);
	let plan = plan::reconcile(
		SyncMode::TwoWay,
		&tree(baseline),
		&scan.nodes.whole(),
		remote,
		&plan::PassHolds::default(),
		plan::PassPaths::Whole,
	);
	let ignored: BTreeSet<String> = scan.ignored.keys().cloned().collect();
	let blocked = scan
		.invalid_names
		.keys()
		.chain(scan.aliased_dirs.keys())
		.chain(&ignored)
		.chain(&scan.ignore_blocked)
		.cloned()
		.collect();
	drop_blocked(plan.actions, &blocked, &ignored)
}

/// A root `.filenignore` that cannot be read blocks the whole tree, not only the items under
/// some directory.
#[cfg(unix)]
#[test]
fn an_unreadable_root_filenignore_blocks_every_action() {
	use std::os::unix::fs::PermissionsExt;

	let root = scan_root("unreadable_rules");
	std::fs::write(root.join("a.txt"), b"a").unwrap();
	std::fs::create_dir(root.join("dir")).unwrap();
	std::fs::write(root.join("dir").join("b.txt"), b"b").unwrap();
	std::fs::write(root.join(".filenignore"), "*.tmp\n").unwrap();
	let readable = planned_over_scan(&root, &HashMap::new(), &HashMap::new());
	assert!(
		!readable.is_empty(),
		"the tree uploads while its rules read"
	);

	let rules = root.join(".filenignore");
	std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o000)).unwrap();
	let unreadable = planned_over_scan(&root, &HashMap::new(), &HashMap::new());
	std::fs::set_permissions(&rules, std::fs::Permissions::from_mode(0o644)).unwrap();
	assert!(
		unreadable.is_empty(),
		"nothing may upload past rules that could not be read: {unreadable:?}"
	);

	std::fs::remove_dir_all(&root).ok();
}

fn synced_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> BaselineEntry {
	BaselineEntry {
		remote_uuid: Some(uuid),
		content_hash: Some(hash),
		size: Some(size),
		local_mtime: Some(0),
		..synced_shell(rel)
	}
}

fn remote_file(rel: &str, uuid: Uuid, hash: Blake3Hash, size: u64) -> RemoteNode {
	RemoteNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: uuid,
		stable_uuid: Some(StableUuid::new_for_test(uuid)),
		content_hash: Some(hash),
		size,
		modified_millis: 0,
	}
}

/// A link to a directory inside the root that an earlier pass synced as a directory of its own:
/// its copy is not trashed, and nothing the remote holds under the link's name is downloaded
/// through the link into the real directory.
#[cfg(unix)]
#[test]
fn a_symlink_to_a_directory_inside_the_root_plans_nothing_under_the_link() {
	let root = scan_root("alias");
	std::fs::create_dir(root.join("real")).unwrap();
	std::fs::write(root.join("real").join("a.txt"), b"payload").unwrap();
	std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
	let hash: Blake3Hash = blake3::hash(b"payload").into();
	let other: Blake3Hash = blake3::hash(b"remote only").into();

	let mut baseline = HashMap::new();
	let mut remote = Side::default();
	for dir in ["real", "link"] {
		let uuid = Uuid::new_v4();
		baseline.insert(
			dir.to_string(),
			BaselineEntry {
				kind: NodeKind::Dir,
				remote_uuid: Some(uuid),
				..synced_shell(dir)
			},
		);
		remote.insert(
			dir.to_string(),
			RemoteNode {
				rel_path: dir.to_string(),
				kind: NodeKind::Dir,
				remote_uuid: uuid,
				stable_uuid: None,
				content_hash: None,
				size: 0,
				modified_millis: 0,
			},
		);
		let file = format!("{dir}/a.txt");
		let uuid = Uuid::new_v4();
		baseline.insert(file.clone(), synced_file(&file, uuid, hash, 7));
		remote.insert(file.clone(), remote_file(&file, uuid, hash, 7));
	}
	// Something the remote gained under the link's name since.
	remote.insert(
		"link/new.txt".to_string(),
		remote_file("link/new.txt", Uuid::new_v4(), other, 11),
	);

	let actions = planned_over_scan(&root, &baseline, &remote.whole());
	assert!(
		actions.is_empty(),
		"no deletion of the link's synced copy, no download through the link: {actions:?}"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// Renaming an already-SYNCED file to a name the remote would reject must not read as a local
/// deletion. Nothing can be pushed under the new name — but the copy the remote already holds
/// is the user's data, and a rename is not a request to destroy it.
#[test]
fn renaming_a_synced_file_into_a_rejected_name_leaves_the_remote_copy_alone() {
	let root = scan_root("rename_file");
	// `report.txt` was synced; the user has just renamed it to ` report.txt`, whose leading
	// space the remote rejects.
	std::fs::write(root.join(" report.txt"), b"payload").unwrap();
	let hash: Blake3Hash = blake3::hash(b"payload").into();
	let uuid = Uuid::new_v4();

	let actions = planned_over_scan(
		&root,
		&HashMap::from([(
			"report.txt".to_string(),
			synced_file("report.txt", uuid, hash, 7),
		)]),
		&HashMap::from([(
			"report.txt".to_string(),
			remote_file("report.txt", uuid, hash, 7),
		)]),
	);
	assert!(
		actions.is_empty(),
		"a rename into a rejected name plans nothing at all — and above all no deletion of the \
			 remote copy: {actions:?}"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// The same for a whole synced DIRECTORY: a remote directory deletion is recursive, so reading
/// the rename as a deletion would take the entire remote subtree with it.
#[test]
fn renaming_a_synced_dir_into_a_rejected_name_leaves_its_remote_subtree_alone() {
	let root = scan_root("rename_dir");
	// `docs/` was synced; the user has just renamed it to ` docs` (a leading space the remote
	// rejects), carrying its contents along.
	std::fs::create_dir_all(root.join(" docs")).unwrap();
	std::fs::write(root.join(" docs").join("a.txt"), b"payload").unwrap();
	let hash: Blake3Hash = blake3::hash(b"payload").into();
	let (dir_uuid, file_uuid) = (Uuid::new_v4(), Uuid::new_v4());

	let actions = planned_over_scan(
		&root,
		&HashMap::from([
			(
				"docs".to_string(),
				BaselineEntry {
					kind: NodeKind::Dir,
					remote_uuid: Some(dir_uuid),
					..synced_shell("docs")
				},
			),
			(
				"docs/a.txt".to_string(),
				synced_file("docs/a.txt", file_uuid, hash, 7),
			),
		]),
		&HashMap::from([
			(
				"docs".to_string(),
				RemoteNode {
					rel_path: "docs".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: dir_uuid,
					stable_uuid: None,
					content_hash: None,
					size: 0,
					modified_millis: 0,
				},
			),
			(
				"docs/a.txt".to_string(),
				remote_file("docs/a.txt", file_uuid, hash, 7),
			),
		]),
	);
	assert!(
		!actions.iter().any(SyncAction::is_delete),
		"the remote subtree of a locally-renamed directory must survive: {actions:?}"
	);

	std::fs::remove_dir_all(&root).ok();
}

/// The streak only blocks once it reaches the threshold. Below the threshold the path is still
/// planned, so a transient failure is retried rather than parked.
#[test]
fn a_failure_streak_blocks_only_at_the_threshold() {
	const NOW: i64 = 1_800_000_000_000;
	for attempts in 0..MAX_PATH_FAILURES {
		assert!(
			!streak_blocks(&failure(attempts, NOW), NOW),
			"{attempts} failure(s) is under the threshold, so the path is still planned"
		);
	}
	assert!(streak_blocks(&failure(MAX_PATH_FAILURES, NOW), NOW));
}

/// An exhausted streak does not park its path for good: once its last failure is a retry
/// interval old the path is planned again, and the failure that attempt records dates the
/// streak anew, so it waits a whole interval more.
#[test]
fn an_exhausted_streak_is_retried_once_its_last_failure_is_an_interval_old() {
	const NOW: i64 = 1_800_000_000_000;
	let interval = i64::try_from(PATH_FAILURE_RETRY_INTERVAL.as_millis()).unwrap();
	let exhausted_at = NOW - interval;

	assert!(
		streak_blocks(&failure(MAX_PATH_FAILURES, exhausted_at), NOW - 1),
		"a millisecond short of the interval, the path is still parked"
	);
	assert!(
		!streak_blocks(&failure(MAX_PATH_FAILURES, exhausted_at), NOW),
		"a whole interval after the last failure, the path is tried again"
	);
	assert!(
		!streak_blocks(&failure(MAX_PATH_FAILURES + 5, exhausted_at - 1), NOW),
		"however long the streak, it is the age of its last failure that releases it"
	);

	// That retry failed: one more attempt, dated now, parks it for another interval.
	let rearmed = failure(MAX_PATH_FAILURES + 1, NOW);
	assert!(streak_blocks(&rearmed, NOW));
	assert!(streak_blocks(&rearmed, NOW + interval - 1));
	assert!(!streak_blocks(&rearmed, NOW + interval));

	assert!(
		streak_blocks(&failure(MAX_PATH_FAILURES, NOW + interval * 10), NOW),
		"a last failure dated after now (the clock stepped back) must not release the path"
	);
}

fn failure(attempts: u32, last_failure_at: i64) -> PathFailure {
	PathFailure {
		attempts,
		last_error: "boom".to_string(),
		last_failure_at,
	}
}

/// `a.txt` and `b.txt` uploaded, and `docs` trashed with the two files under it folded in,
/// screened in a stage of their own.
fn uploads_and_a_folded_trash() -> (tempfile::TempDir, Screen) {
	let trash = |rel_path: &str, kind| SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: Uuid::from_u128(1),
	};
	let actions = [
		upload("a.txt"),
		upload("b.txt"),
		trash("docs", NodeKind::Dir),
		trash("docs/x.txt", NodeKind::File),
		trash("docs/y.txt", NodeKind::File),
	];
	let (dir, stage) = Stage::in_temp_dir();
	let mut screen = Screen::new(stage, &[], &actions).unwrap();
	assert_eq!(screen.fold_subsumed().unwrap(), 2);
	(dir, screen)
}

fn pairs(ends: &[(&str, &str)]) -> Vec<(String, String)> {
	ends.iter()
		.map(|(by, ended)| ((*by).to_string(), (*ended).to_string()))
		.collect()
}

/// Only a path with a streak has one to end: its own action ends it, and so does the directory
/// deletion that carries it out.
#[test]
fn a_pass_ends_only_the_streaks_the_store_has() {
	let (_dir, screen) = uploads_and_a_folded_trash();
	let streaks = HashMap::from([
		("a.txt".to_string(), failure(1, 0)),
		("docs/x.txt".to_string(), failure(2, 0)),
	]);
	assert_eq!(
		endable_streaks(&screen, Some(&streaks)).unwrap(),
		pairs(&[("a.txt", "a.txt"), ("docs", "docs/x.txt")])
	);
}

/// A store that could not say which paths have a streak leaves every path's to be ended, as if
/// each had one.
#[test]
fn a_pass_that_could_not_read_the_streaks_treats_every_path_as_having_one() {
	let (_dir, screen) = uploads_and_a_folded_trash();
	assert_eq!(
		endable_streaks(&screen, None).unwrap(),
		pairs(&[
			("a.txt", "a.txt"),
			("b.txt", "b.txt"),
			("docs", "docs"),
			("docs", "docs/x.txt"),
			("docs", "docs/y.txt"),
		])
	);
}

/// A first sync plans the whole tree and no path of it has failed: what the pass keeps to end
/// failure streaks with is nothing, not an owned path per planned item held until it ends.
#[test]
fn a_pass_with_no_failure_streak_keeps_no_path_to_end_one() {
	let actions: Vec<SyncAction> = (0..10_000)
		.map(|n| upload(&format!("d{:02}/f{n:05}.txt", n % 100)))
		.collect();
	let (_dir, stage) = Stage::in_temp_dir();
	let screen = Screen::new(stage, &[], &actions).unwrap();
	let streaks = HashMap::new();
	let (ends, peak) =
		alloc_meter::peak_bytes(|| endable_streaks(&screen, Some(&streaks)).unwrap());
	assert_eq!(peak, 0, "10_000 successful actions and no streak");
	assert_eq!(ends, Vec::new());
}

/// Once the apply has what its actions read, the pass keeps none of the trees it planned from:
/// a first push of 20_000 files into 100 directories the remote already has hands the apply an
/// entry per upload, and what the pass still holds beside them is a few small sets, not the
/// local scan, the remote view or a row per planned path.
#[tokio::test]
async fn the_apply_inputs_leave_the_pass_none_of_its_trees() {
	const CREATES: u32 = 20_000;
	let hash = Blake3Hash::from([1; 32]);
	let dirs: Vec<String> = (0..100u32).map(|d| format!("d{d:02}")).collect();
	let files: Vec<String> = (0..CREATES)
		.map(|n| format!("d{:02}/f{n:05}.txt", n % 100))
		.collect();
	let local: Vec<LocalNode> = dirs
		.iter()
		.map(|dir| local_dir(dir))
		.chain(files.iter().map(|file| local_file(file, hash)))
		.collect();
	let remote = view(
		(0..100u32)
			.zip(&dirs)
			.map(|(d, dir)| remote_dir(dir, Uuid::from_u128(u128::from(d) + 1)))
			.collect(),
		Vec::new(),
		&[],
	);
	let mut prep = prepared(
		SyncMode::TwoWay,
		HashMap::new(),
		local,
		remote,
		HashMap::new(),
	);
	let actions: Vec<SyncAction> = files.iter().map(|file| upload(file)).collect();
	let (_dir, stage) = Stage::in_temp_dir();
	let screen = Screen::new(stage, &[], &actions).unwrap();
	drop(actions);
	let plan = prep.stage_plan(screen).await.unwrap();
	assert_eq!(plan.pager().len(), 20_000);
	let ((), net) = alloc_meter::net_bytes(|| drop(prep));
	assert!(
		net > -4096,
		"the pass still held {} bytes beside the apply's inputs",
		-net
	);
}

/// Staging the plan ends the pass's read transaction: once the apply has its entries, neither
/// the plan nor what the pass keeps beside it holds a clone of the baseline's snapshot, which
/// would keep that transaction open through the apply and pin the WAL (see `Baseline::close`).
#[tokio::test]
async fn staging_the_plan_leaves_no_clone_of_the_pass_snapshot() {
	let (old, new) = (Blake3Hash::from([1; 32]), Blake3Hash::from([2; 32]));
	let uuid = Uuid::from_u128(1);
	let mut prep = prepared(
		SyncMode::TwoWay,
		HashMap::from([("a.txt".to_string(), synced_file("a.txt", uuid, old, 4))]),
		vec![local_file("a.txt", old), local_file("b.txt", new)],
		view(vec![remote_file("a.txt", uuid, new, 4)], Vec::new(), &[]),
		HashMap::new(),
	);
	let snapshot = prep
		.baseline
		.snapshot_for_test()
		.expect("a stored baseline reads through a snapshot");
	// Each entry takes what the apply reads for it out of the snapshot: the download its row,
	// the upload the absence of one.
	let actions = [
		SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: uuid,
		},
		upload("b.txt"),
	];
	let (_dir, stage) = Stage::in_temp_dir();
	let screen = Screen::new(stage, &[], &actions).unwrap();
	let plan = prep.stage_plan(screen).await.unwrap();
	assert_eq!(plan.pager().len(), 2);
	assert_eq!(
		snapshot.strong_count(),
		0,
		"a clone of the pass's snapshot outlives its plan"
	);
	drop(prep);
}

#[test]
fn local_roots_overlap_only_when_one_actually_contains_the_other() {
	let existing = Path::new("/sync/data");
	assert!(matches!(
		local_overlap(Path::new("/sync/data"), existing, 1),
		Some(PairOverlap::LocalRootInUse { pair: 1, .. })
	));
	assert!(matches!(
		local_overlap(Path::new("/sync/data/sub/deep"), existing, 1),
		Some(PairOverlap::LocalRootNested { pair: 1, .. })
	));
	assert!(matches!(
		local_overlap(Path::new("/sync"), existing, 1),
		Some(PairOverlap::LocalRootContains { pair: 1, .. })
	));
	// A sibling is disjoint, and so is a name that merely SHARES a prefix with the root: the
	// comparison is per path component, not per byte.
	assert!(local_overlap(Path::new("/sync/other"), existing, 1).is_none());
	assert!(local_overlap(Path::new("/sync/database"), existing, 1).is_none());
	assert!(local_overlap(Path::new("/elsewhere"), existing, 1).is_none());
}

/// The same relation between two REMOTE roots, decided from their ancestor chains. The chains
/// are what a partial cache view used to get wrong: `docs` is an ancestor of `reports` whether
/// or not anything has enumerated it, so the answer must come from a chain that reaches the
/// account root and not from what happens to be cached.
#[test]
fn remote_roots_overlap_only_when_one_chain_carries_the_other_root() {
	let (root, docs, reports, photos) = (
		Uuid::new_v4(),
		Uuid::new_v4(),
		Uuid::new_v4(),
		Uuid::new_v4(),
	);
	// Each chain is the root itself, then every ancestor up to the account root.
	let docs_chain = [docs, root];
	let reports_chain = [reports, docs, root];
	let photos_chain = [photos, root];

	assert!(matches!(
		remote_overlap(docs, &docs_chain, docs, &docs_chain, 1),
		Some(PairOverlap::RemoteRootInUse { pair: 1, .. })
	));
	assert!(matches!(
		remote_overlap(reports, &reports_chain, docs, &docs_chain, 1),
		Some(PairOverlap::RemoteRootNested { pair: 1, .. })
	));
	assert!(matches!(
		remote_overlap(docs, &docs_chain, reports, &reports_chain, 1),
		Some(PairOverlap::RemoteRootContains { pair: 1, .. })
	));
	// Siblings under one parent are disjoint, and sharing an ancestor is not a relation.
	assert!(remote_overlap(photos, &photos_chain, docs, &docs_chain, 1).is_none());
	assert!(remote_overlap(photos, &photos_chain, reports, &reports_chain, 1).is_none());
}

/// The reconciler's own one-line rendering of an action — what these tests assert plans by.
/// (The public [`PlannedAction`] rendering is pinned in `outcome.rs`.)
fn describe(action: &SyncAction) -> String {
	action.describe()
}

fn created(path: &str, replaced: Option<Uuid>) -> PendingKind {
	PendingKind::Created {
		path: path.to_string(),
		replaced,
	}
}

fn moved(from: &str, to: &str) -> PendingKind {
	PendingKind::Moved {
		from: from.to_string(),
		to: to.to_string(),
	}
}

/// A directory this engine renamed in place, which the cache still lists under its old
/// spelling, is shown at the new one — children and all. Leaving the children behind would read
/// them as untracked remote items beside tracked ones gone missing.
#[test]
fn folding_a_directory_rename_carries_its_subtree() {
	let (dir, child) = (Uuid::new_v4(), Uuid::new_v4());
	let node = |path: &str, uuid: Uuid, kind: NodeKind| RemoteNode {
		rel_path: path.to_string(),
		kind,
		remote_uuid: uuid,
		stable_uuid: None,
		content_hash: None,
		size: 0,
		modified_millis: 0,
	};
	let mut nodes: Side<RemoteNode> = [
		("Docs".to_string(), node("Docs", dir, NodeKind::Dir)),
		(
			"Docs/a.txt".to_string(),
			node("Docs/a.txt", child, NodeKind::File),
		),
	]
	.into_iter()
	.collect();
	// The move is the journal's one record, so the index holds the directory at its old path.
	let journal = HashMap::from([(
		dir,
		PendingWrite {
			at: Instant::now(),
			pair: PAIR,
			seq: 0,
			kind: moved("Docs", "docs"),
		},
	)]);
	let mut path_of = ViewIndex::new(&journal);
	// Built BEFORE the fold, as it is for a fold that has already answered one record the two
	// paths it names could not: the assertions below are then about the index being kept in
	// step, which is what they were about when it was built unconditionally.
	assert!(
		path_of
			.at(&nodes, &Baseline::default(), Uuid::new_v4())
			.is_none()
	);
	let mut changed = BTreeSet::new();
	assert!(fold_move(
		&mut nodes,
		&mut path_of,
		&Baseline::default(),
		dir,
		"Docs",
		"docs",
		&mut changed,
	));
	let mut paths: Vec<String> = nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort_unstable();
	assert_eq!(paths, ["docs", "docs/a.txt"]);
	assert_eq!(
		nodes.whole().at("docs/a.txt").unwrap().rel_path,
		"docs/a.txt"
	);
	assert_eq!(
		path_of.at(&nodes, &Baseline::default(), child).as_deref(),
		Some("docs/a.txt")
	);
	assert_eq!(
		path_of.at(&nodes, &Baseline::default(), dir).as_deref(),
		Some("docs")
	);
	// And every key it moved, BY NAME: both ends for the directory and both for each child it
	// carried. A key the fold leaves out here is a key a change-scoped reconcile never visits,
	// so the re-key would be invisible to the very pass that made it.
	assert_eq!(
		changed.iter().map(String::as_str).collect::<Vec<_>>(),
		["Docs", "Docs/a.txt", "docs", "docs/a.txt"],
		"the fold must record both ends of every key it re-keyed"
	);
}

fn hash(byte: u8) -> Blake3Hash {
	Blake3Hash::from([byte; 32])
}

/// A held conflict row for `a.txt` whose two sides carry the same content evidence — what
/// `record_conflict` writes once both sides have converged while the conflict was held.
fn converged_conflict(uuid: Uuid) -> BaselineEntry {
	BaselineEntry {
		rel_path: "a.txt".to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid),
		content_hash: Some(hash(3)),
		size: Some(5),
		local_mtime: Some(111),
		remote_modified: Some(222),
		state: BaselineState::Conflicted,
		local_kind: Some(NodeKind::File),
		remote_kind: Some(NodeKind::File),
		remote_hash: Some(hash(3)),
		remote_size: Some(5),
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: None,
	}
}

fn local_map(hash: Blake3Hash) -> HashMap<String, LocalNode> {
	HashMap::from([(
		"a.txt".to_string(),
		LocalNode {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			size: 5,
			mtime_millis: 111,
			content_hash: Some(hash),
		},
	)])
}

fn remote_map(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, RemoteNode> {
	HashMap::from([(
		"a.txt".to_string(),
		RemoteNode {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: Some(hash),
			size: 5,
			modified_millis: 222,
		},
	)])
}

/// Both sides converged while the conflict was held: keeping the local copy has nothing left
/// to push, so the resolution must record the whole synced state rather than a half row the
/// next pass reads as a local modification.
#[test]
fn keeping_local_on_a_converged_conflict_records_a_clean_synced_row() {
	let uuid = Uuid::new_v4();
	let entry = resolution_entry(
		"a.txt",
		&converged_conflict(uuid),
		ConflictResolution::KeepLocal,
	)
	.expect("the remote side had a kind, so a row is written");

	assert_eq!(entry.state, BaselineState::Synced);
	assert_eq!(
		entry.content_hash,
		Some(hash(3)),
		"the converged content is recorded"
	);
	assert_eq!(entry.size, Some(5));
	assert_eq!(entry.local_mtime, Some(111));
	assert_eq!(entry.remote_uuid, Some(uuid));
	assert_eq!(
		entry.agreed_hash, entry.content_hash,
		"resolving records what the two sides now agree on"
	);

	let baseline = HashMap::from([("a.txt".to_string(), entry)]);
	assert!(
		plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local_map(hash(3)),
			&remote_map(uuid, hash(3)),
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"the resolved path is settled: the next pass plans nothing at all"
	);
}

/// The mirror image: keeping the REMOTE copy of a converged conflict also has nothing left to
/// transfer, so it too must record the whole synced state instead of a half row.
#[test]
fn keeping_remote_on_a_converged_conflict_records_a_clean_synced_row() {
	let uuid = Uuid::new_v4();
	let entry = resolution_entry(
		"a.txt",
		&converged_conflict(uuid),
		ConflictResolution::KeepRemote,
	)
	.expect("the local side had a kind, so a row is written");

	assert_eq!(entry.state, BaselineState::Synced);
	assert_eq!(
		entry.remote_uuid,
		Some(uuid),
		"the converged remote version is recorded"
	);
	assert_eq!(entry.remote_modified, Some(222));
	assert_eq!(entry.content_hash, Some(hash(3)));
	assert_eq!(
		entry.agreed_hash, entry.content_hash,
		"resolving records what the two sides now agree on"
	);

	let baseline = HashMap::from([("a.txt".to_string(), entry)]);
	assert!(
		plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local_map(hash(3)),
			&remote_map(uuid, hash(3)),
			&plan::PassHolds::default(),
			plan::PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"the resolved path is settled: the next pass plans nothing at all"
	);
}

/// And the diverged case still pulls: with the sides genuinely different the row keeps no
/// remote anchor, so the remote copy reads as the change.
#[test]
fn keeping_remote_on_a_diverged_conflict_still_pulls_the_remote_copy() {
	let uuid = Uuid::new_v4();
	let held = BaselineEntry {
		remote_hash: Some(hash(9)),
		..converged_conflict(uuid)
	};
	let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepRemote)
		.expect("the local side had a kind, so a row is written");
	assert_eq!(
		entry.remote_uuid, None,
		"a diverged remote side must still read as changed"
	);
	assert_eq!(
		entry.agreed_hash,
		Some(hash(3)),
		"the resolution settles on the row's content, so the pull below is not re-read as a \
			 concurrent edit and the path does not conflict forever"
	);

	let baseline = HashMap::from([("a.txt".to_string(), entry)]);
	let actions = plan::reconcile(
		SyncMode::TwoWay,
		&tree(&baseline),
		&local_map(hash(3)),
		&remote_map(uuid, hash(9)),
		&plan::PassHolds::default(),
		plan::PassPaths::Whole,
	)
	.actions;
	assert_eq!(
		actions.iter().map(describe).collect::<Vec<_>>(),
		vec!["download file \"a.txt\"".to_string()],
	);
}

/// The ordinary case is unchanged: the sides really do differ, so the row is anchored to the
/// remote alone and the local copy is pushed on the next pass.
#[test]
fn keeping_local_on_a_diverged_conflict_still_pushes_the_local_copy() {
	let uuid = Uuid::new_v4();
	let held = BaselineEntry {
		remote_hash: Some(hash(9)),
		..converged_conflict(uuid)
	};
	let entry = resolution_entry("a.txt", &held, ConflictResolution::KeepLocal)
		.expect("the remote side had a kind, so a row is written");
	assert_eq!(
		entry.content_hash, None,
		"a diverged local side must still read as changed"
	);
	assert_eq!(
		entry.agreed_hash, None,
		"there is nothing agreed yet: the local copy has still to be pushed"
	);

	let baseline = HashMap::from([("a.txt".to_string(), entry)]);
	let actions = plan::reconcile(
		SyncMode::TwoWay,
		&tree(&baseline),
		&local_map(hash(3)),
		&remote_map(uuid, hash(9)),
		&plan::PassHolds::default(),
		plan::PassPaths::Whole,
	)
	.actions;
	assert_eq!(
		actions.iter().map(describe).collect::<Vec<_>>(),
		vec!["upload file \"a.txt\"".to_string()],
	);
}
/// A remote node the snapshot holds at `path`.
fn node_at(path: &str, uuid: Uuid) -> Side<RemoteNode> {
	Side::from(HashMap::from([(
		path.to_string(),
		RemoteNode {
			rel_path: path.to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: None,
			size: 0,
			modified_millis: 0,
		},
	)]))
}

/// A baseline row as the apply layer writes it straight after a remote write.
fn written_at(path: &str, uuid: Uuid, kind: NodeKind, hash: Option<Blake3Hash>) -> BaselineEntry {
	BaselineEntry {
		rel_path: path.to_string(),
		kind,
		remote_uuid: Some(uuid),
		content_hash: hash,
		size: Some(5),
		local_mtime: Some(111),
		remote_modified: Some(222),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: None,
	}
}

/// The baseline an upload of `a.txt` leaves behind.
fn written_row(uuid: Uuid, hash: Blake3Hash) -> HashMap<String, BaselineEntry> {
	HashMap::from([(
		"a.txt".to_string(),
		written_at("a.txt", uuid, NodeKind::File, Some(hash)),
	)])
}

/// A pass that panics — a baseline read that failed panics on purpose — comes back as the
/// pass's error, which the watch loop backs off on, instead of unwinding into the loop's task
/// and ending it without a word.
#[tokio::test]
async fn a_panicking_pass_fails_as_a_pass() {
	let failed = pass_panics_as_errors::<()>(7, "the pass", async {
		panic!("reading pair 7's baseline rows failed: disk I/O error")
	})
	.await
	.expect_err("a pass that panicked is a failed pass");
	assert!(
		failed
			.to_string()
			.contains("the pass panicked: reading pair 7's baseline rows failed"),
		"{failed}"
	);
	let owned =
		pass_panics_as_errors::<()>(7, "the pass", async { panic!("{}", String::from("owned")) })
			.await
			.expect_err("a pass that panicked is a failed pass");
	assert!(
		owned.to_string().contains("the pass panicked: owned"),
		"{owned}"
	);
	assert!(
		pass_panics_as_errors(7, "the pass", async { Ok(3) })
			.await
			.is_ok_and(|n| n == 3)
	);
}

/// The restart this journal exists for: the engine that made the write is gone, and the one
/// that reopens its baseline DB seconds later has nothing in memory to fold. Loading the
/// persisted journal is what keeps it from reading the just-uploaded file as absent and
/// uploading it a second time.
#[tokio::test]
async fn a_create_a_previous_engine_journalled_is_folded_after_a_reopen() {
	let path = std::env::temp_dir().join(format!("filen_sync_journal_{}.db", Uuid::new_v4()));
	let uuid = Uuid::new_v4();
	let baseline = written_row(uuid, hash(1));
	let pair = {
		let store = BaselineStore::open(&path).unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::LocalToRemote)
			.unwrap();
		store
			.record_pending(
				&PendingRow {
					pair,
					uuid,
					kind: created("a.txt", None),
					recorded_at: Utc::now().timestamp_millis(),
				},
				&[BaselineChange::Upsert(&baseline["a.txt"])],
				&[],
			)
			.unwrap();
		pair
		// The engine "exits" here, taking its in-memory journal with it.
	};

	// The restart itself: a real `open` on the same DB, so what is under test is the wiring
	// the reopened engine actually runs — loading the journal and aging it — not a
	// hand-assembled stand-in for it.
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();

	// The cache is still behind: its snapshot has nothing at all under the root yet.
	let mut remote = Side::default();
	let holds = engine
		.pending
		.settle(pair, &engine.observed.snapshot(), &remote.whole());
	assert_eq!(
		engine
			.pending
			.fold_into(pair, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1,
		"the reopened engine folds the write its predecessor made"
	);

	let actions = plan::reconcile(
		SyncMode::LocalToRemote,
		&tree(&baseline),
		&local_map(hash(1)),
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	)
	.actions;
	assert!(
		actions.is_empty(),
		"the restart re-does the pass's writes: {:?}",
		actions.iter().map(describe).collect::<Vec<_>>()
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// Watching a paused pair is refused rather than started idle: a handle whose loop runs nothing
/// looks exactly like a broken watch, and the caller who paused the pair is the one who can say
/// whether it should be running.
#[tokio::test]
async fn a_watch_on_a_paused_pair_is_refused() {
	let path = std::env::temp_dir().join(format!("filen_sync_watch_paused_{}.db", Uuid::new_v4()));
	let engine = Arc::new(
		SyncEngine::open(Arc::new(offline_client()), path.clone())
			.await
			.unwrap(),
	);
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	engine.pause_pair(pair).await.unwrap();

	let error = match Arc::clone(&engine).watch(pair).await {
		Ok(_) => panic!("a paused pair must not be watchable"),
		Err(error) => error.to_string(),
	};
	assert!(
		error.contains("paused") && error.contains("resume"),
		"the refusal must say what to do about it: {error}"
	);
	// An id nobody registered is still reported as unknown, not as paused.
	let unknown = match Arc::clone(&engine).watch(pair + 9_999).await {
		Ok(_) => panic!("an unknown pair must not be watchable"),
		Err(error) => error.to_string(),
	};
	assert!(unknown.contains("unknown sync pair"), "{unknown}");

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A second engine on a baseline another engine has open is refused, naming the baseline, and
/// opens once the first is dropped; an engine on another baseline opens beside it.
#[tokio::test]
async fn a_second_engine_on_an_open_baseline_is_refused_until_the_first_is_dropped() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	let first = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();

	let refused = match SyncEngine::open(Arc::new(offline_client()), path.clone()).await {
		Ok(_) => panic!("a second engine opened a baseline the first one has open"),
		Err(error) => error,
	};
	assert_eq!(refused.kind(), ErrorKind::IO, "{refused}");
	assert_eq!(
		refused
			.downcast_ref::<BaselineInUse>()
			.map(|in_use| in_use.path.as_path()),
		Some(path.as_path()),
		"{refused}"
	);

	let other = SyncEngine::open(Arc::new(offline_client()), dir.path().join("other.db"))
		.await
		.unwrap();
	drop(first);
	SyncEngine::open(Arc::new(offline_client()), path)
		.await
		.unwrap();
	drop(other);
}

/// An engine refused a baseline sweeps nothing beside it: what would pass for a crash's leftover
/// stage there is a live pass of the engine that has the baseline open.
#[tokio::test]
async fn an_engine_refused_a_baseline_leaves_the_stages_beside_it_alone() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	let first = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let live = dir.path().join("baseline.db-stage-1-0");
	std::fs::write(&live, b"a stage of the first engine's pass").unwrap();

	match SyncEngine::open(Arc::new(offline_client()), path).await {
		Ok(_) => panic!("a second engine opened a baseline the first one has open"),
		Err(error) => assert!(error.downcast_ref::<BaselineInUse>().is_some(), "{error}"),
	}
	assert!(
		live.exists(),
		"a refused open swept a stage of the engine that has the baseline open"
	);
	drop(first);
}

/// Where the platform has no file locks (Android's std has no `try_lock`), the engine opens
/// without one rather than refusing every open.
#[test]
fn an_unsupported_file_lock_opens_the_baseline_unlocked() {
	let settled = BaselineLock::settle(
		Err(std::fs::TryLockError::Error(
			std::io::ErrorKind::Unsupported.into(),
		)),
		tempfile::tempfile().unwrap(),
		Path::new("baseline.db"),
		Path::new("baseline.db-lock"),
	);
	assert!(matches!(settled, Ok(None)), "{:?}", settled.err());
}

/// Any other failure to take the lock refuses the open, naming the lock file.
#[test]
fn a_lock_that_fails_otherwise_refuses_the_open() {
	let refused = match BaselineLock::settle(
		Err(std::fs::TryLockError::Error(
			std::io::ErrorKind::PermissionDenied.into(),
		)),
		tempfile::tempfile().unwrap(),
		Path::new("baseline.db"),
		Path::new("baseline.db-lock"),
	) {
		Ok(_) => panic!("a lock that failed opened the baseline"),
		Err(error) => error,
	};
	assert_eq!(refused.kind(), ErrorKind::IO, "{refused}");
	assert!(
		refused
			.to_string()
			.contains("locking the sync engine's lock file baseline.db-lock"),
		"{refused}"
	);
}

/// A pass leaves its pair's writing connection holding the pages it wrote through, and a watch
/// then idles on them; every pass that reads gives that cache back on its way out, so the
/// writer's next read of a page goes to the file again.
#[tokio::test]
async fn a_finished_pass_gives_back_the_pair_writers_page_cache() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let store = cache_a_rewritten_page(&engine, pair, &path).await;

	// Offline, so the pass reads the baseline and then fails at the remote side: the way out is
	// what is under test, and every pass that reads takes the same one.
	engine
		.sync_once(pair)
		.await
		.expect_err("an offline pass reads no remote side");

	assert_eq!(
		writers_page(&store),
		"gone-page",
		"the page cache was given back, so the read went to the file"
	);
}

/// A watch wake with nothing announced runs no pass, so it gives back no page cache: it read
/// nothing, and a release reaches every connection in the process.
///
/// A page STILL cached is evicted by a release from any test running alongside as well (see
/// `rewrite_behind_sqlite`), so the scenario runs again in a process of its own (see
/// [`answer_alone`]) and answers with what the writer read.
#[tokio::test]
async fn an_idle_wake_keeps_the_pair_writers_page_cache() {
	let Some(answer) = std::env::var_os(ALONE) else {
		assert_eq!(
			answer_alone("an_idle_wake_keeps_the_pair_writers_page_cache"),
			"kept-page",
			"the idle wake gave the page cache back"
		);
		return;
	};
	let (engine, pair, path) = engine_with_pair("idle_wake_release").await;
	// Watched, with nothing announced on either side: the wake a quiet tree's watch makes.
	let changes = engine.pair_changes(pair).await;
	changes.cover_local();
	assert!(changes.is_idle());
	let store = cache_a_rewritten_page(&engine, pair, &path).await;

	// Offline, so a pass that read would fail at the remote side: `Ok` is a pass never started.
	engine
		.sync_pass(pair, &mut |_| {}, WhenIdle::Skip)
		.await
		.expect("an idle wake runs no pass");

	std::fs::write(answer, writers_page(&store)).unwrap();
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// Where [`answer_alone`] tells the test it runs to write its answer.
const ALONE: &str = "FILEN_SYNC_ENGINE_TEST_ALONE";

/// Run this module's test `name` again, alone in a process of its own with [`ALONE`] naming a
/// file, and return what it wrote there: for an assertion no other test may disturb, such as a
/// page still cached in the page cache every connection in a process shares.
fn answer_alone(name: &str) -> String {
	let dir = tempfile::tempdir().unwrap();
	let answer = dir.path().join("answer");
	let (_, module) = module_path!()
		.split_once("::")
		.expect("a module path starts with its crate");
	let test = format!("{module}::{name}");
	let alone = std::process::Command::new(
		std::env::current_exe().expect("a test binary knows its own path"),
	)
	.args([test.as_str(), "--exact", "--test-threads=1"])
	.env(ALONE, &answer)
	.output()
	.expect("running the test alone");
	assert!(
		alone.status.success(),
		"{test}, run alone, exited {}:\n{}",
		alone.status,
		String::from_utf8_lossy(&alone.stdout)
	);
	// No file at all when the name matched no test: libtest then runs none and exits 0.
	std::fs::read_to_string(&answer).expect("the test run alone wrote no answer")
}

/// A dry run reads what a pass reads and gives back what that cached when it ends, as a pass
/// does, whatever its read came to.
#[tokio::test]
async fn a_dry_run_gives_back_the_pair_writers_page_cache() {
	let (engine, pair, path) = engine_with_pair("plan_release").await;
	let store = cache_a_rewritten_page(&engine, pair, &path).await;

	// Offline, so the read fails before anything is planned: the way out is what is under test.
	engine
		.plan_pair(pair)
		.await
		.expect_err("an offline dry run reads no remote side");

	assert_eq!(
		writers_page(&store),
		"gone-page",
		"the dry run kept the page cache"
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// So does an adoption, which reads both sides as a dry run does before it writes the rows it
/// adopted.
#[tokio::test]
async fn an_adoption_gives_back_the_pair_writers_page_cache() {
	let (engine, pair, path) = engine_with_pair("adopt_release").await;
	let store = cache_a_rewritten_page(&engine, pair, &path).await;

	// Offline, as the dry run above.
	engine
		.reconfigure_pair(pair, SyncMode::LocalToRemote, Backlog::AdoptDestination)
		.await
		.expect_err("an offline adoption reads no remote side");

	assert_eq!(
		writers_page(&store),
		"gone-page",
		"the adoption kept the page cache"
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// And so does the confirmation sweep a resume runs, which reads every unconfirmed row.
#[tokio::test]
async fn a_confirmation_sweep_gives_back_the_pair_writers_page_cache() {
	let (engine, pair, path) = engine_with_pair("sweep_release").await;
	// A push no announcement has confirmed, so the sweep reads the rows instead of returning at
	// its first look.
	let pushed = synced_file(
		"pushed.txt",
		Uuid::from_u128(1),
		Blake3Hash::from([1; 32]),
		4,
	);
	locked(&engine.pair_store(pair).await.unwrap())
		.upsert_entry(pair, &pushed)
		.unwrap();
	let store = cache_a_rewritten_page(&engine, pair, &path).await;

	engine.sweep_confirmations(pair).await.unwrap();

	assert_eq!(
		writers_page(&store),
		"gone-page",
		"the sweep kept the page cache"
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// Leave `pair`'s writing connection holding a page, then change that page's bytes on disk
/// behind SQLite's back: [`writers_page`] answers `"kept-page"` from the writer's cache until
/// that cache is given back, and `"gone-page"` from the file after.
async fn cache_a_rewritten_page(engine: &SyncEngine, pair: PairId, path: &Path) -> SharedStore {
	let store = engine.pair_store(pair).await.unwrap();
	locked(&store)
		.writer()
		.execute_batch(
			"CREATE TABLE probe (v TEXT); INSERT INTO probe VALUES ('kept-page'); \
				 PRAGMA wal_checkpoint(TRUNCATE);",
		)
		.unwrap();
	assert_eq!(writers_page(&store), "kept-page");
	rewrite_behind_sqlite(path, b"kept-page", b"gone-page");
	store
}

/// The page [`cache_a_rewritten_page`] left, read through the pair's writing connection.
fn writers_page(store: &SharedStore) -> String {
	locked(store)
		.writer()
		.query_row("SELECT v FROM probe", [], |row| row.get(0))
		.unwrap()
}

/// Every stage an engine makes for a pair is a file of its own beside the baseline, so a second
/// one — a dry run's, or the next pass's — never opens or truncates a stage that is still being
/// read; each goes when it is dropped, and an engine opening the baseline removes those a crash
/// left behind.
#[tokio::test]
async fn each_stage_of_a_pair_is_a_file_of_its_own() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	let leftover = dir.path().join("baseline.db-stage-1-0");
	std::fs::write(&leftover, b"a crashed pass's stage").unwrap();
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	assert!(!leftover.exists(), "the open left a stage behind");
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	let live = engine.stage(pair).unwrap();
	let live_path = live.path().to_owned();
	assert_eq!(
		live_path,
		dir.path().join(format!("baseline.db-stage-{pair}-0"))
	);
	let mut live = stage::PlanWriter::new(live).unwrap();
	live.push(&PlannedEntry::bare(upload("kept.txt"))).unwrap();

	let other = engine.stage(pair).unwrap();
	let other_path = other.path().to_owned();
	assert_eq!(
		other_path,
		dir.path().join(format!("baseline.db-stage-{pair}-1"))
	);
	stage::PlanWriter::new(other).unwrap();
	assert!(!other_path.exists(), "a dropped stage left its file");

	let mut read = Vec::new();
	live.finish()
		.unwrap()
		.pager()
		.sweep(None, |entry| read.push(entry.action))
		.unwrap();
	assert_eq!(read, [upload("kept.txt")]);
	assert!(!live_path.exists(), "a dropped stage left its file");
}

/// A hold leaves a watched pair idle, so an approval has to give the pair its next pass itself:
/// due, whole, and announced on the doorbell its watch waits on — or the approved batch waits
/// for whatever wakes the watch next, often the safety net minutes later.
#[tokio::test]
async fn an_approval_makes_the_next_pass_due_and_wakes_the_watch() {
	let path = std::env::temp_dir().join(format!("filen_sync_approve_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let changes = engine.pair_changes(pair).await;
	changes.cover_local();
	changes.note_deletion_hold();
	let doorbell = changes.doorbell();
	assert!(changes.is_idle(), "a hold alone wakes nothing");
	assert!(
		doorbell.notified().now_or_never().is_none(),
		"nothing has rung yet"
	);

	engine.approve_deletions(pair, "0123456789abcdef").await;

	assert!(
		doorbell.notified().now_or_never().is_some(),
		"the approval must wake the pair's watch"
	);
	assert!(!changes.is_idle(), "the approved pass is due");
	assert_eq!(
		changes.take().full_pass_reason(10),
		Some(FullPassReason::DeletionHold),
		"and reads whole, the only read that reproduces the batch"
	);
	assert!(
		engine.take_approval(pair, "0123456789abcdef").await,
		"the approval itself is stored for that pass"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A requested pass is due, whole, and announced on the request signal the pair's watch waits
/// on; an unknown pair, a paused one and one being removed are refused, and the paused one is
/// left exactly as it was.
#[tokio::test]
async fn a_requested_pass_is_due_whole_and_wakes_the_watch() {
	let path = std::env::temp_dir().join(format!("filen_sync_request_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let changes = engine.pair_changes(pair).await;
	changes.cover_local();
	let requested = changes.request_signal();

	let unknown = engine.request_pass(pair + 9_999).await.unwrap_err();
	assert_eq!(unknown.kind(), ErrorKind::InvalidState);
	assert!(
		unknown.to_string().contains("unknown sync pair"),
		"{unknown}"
	);

	engine.pause_pair(pair).await.unwrap();
	let paused = engine.request_pass(pair).await.unwrap_err();
	assert_eq!(paused.kind(), ErrorKind::InvalidState);
	assert!(
		paused.to_string().contains("paused") && paused.to_string().contains("resume"),
		"the refusal must say what to do about it: {paused}"
	);
	assert!(changes.is_idle(), "a refused request forces nothing");
	assert!(
		requested.notified().now_or_never().is_none(),
		"a refused request rings nothing"
	);

	engine.set_control(pair, PassControl::Run).await.unwrap();
	engine.request_pass(pair).await.unwrap();
	assert!(
		requested.notified().now_or_never().is_some(),
		"the request must wake the pair's watch"
	);
	assert_eq!(
		changes.take().full_pass_reason(10),
		Some(FullPassReason::Requested)
	);

	// A pair whose removal is under way reads as paused, but is refused as what it is.
	engine
		.paused
		.lock()
		.await
		.get(&pair)
		.expect("the pause made the pair's control")
		.send_replace(PassControl::Retired);
	let retired = engine.request_pass(pair).await.unwrap_err();
	assert_eq!(
		retired.to_string(),
		being_removed(pair).to_string(),
		"a pair being removed must not be sent to resume_pair"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A watch starting on a pair while a removal of that same pair is in flight. Registering the
/// watch's stop signal after the existence check — with a cache registration and a filesystem
/// watcher in between — would let the removal land in the gap: it would find no signal to trip,
/// delete the row, and the watch would then subscribe to a fresh signal nobody will ever trip,
/// leaving a loop that fails every pass against a pair that is gone and never stops.
#[tokio::test]
async fn a_removal_racing_a_watchs_setup_is_never_lost() {
	let path = std::env::temp_dir().join(format!("filen_sync_watch_race_{}.db", Uuid::new_v4()));
	let engine = Arc::new(
		SyncEngine::open(Arc::new(offline_client()), path.clone())
			.await
			.unwrap(),
	);
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	// The registry lock fixes the interleaving, as in the pause/removal race above: both verbs
	// queue on it while this guard is held, and tokio hands the mutex out in request order, so
	// the watch's setup runs first and the removal second — the order in which the removal has
	// nothing registered to trip yet.
	let guard = engine.registry.lock().await;
	let watching = tokio::spawn({
		let engine = Arc::clone(&engine);
		async move { engine.watchable_pair(pair).await }
	});
	tokio::task::yield_now().await;
	let removing = tokio::spawn({
		let engine = Arc::clone(&engine);
		async move { engine.remove_pair(pair).await }
	});
	tokio::task::yield_now().await;
	drop(guard);

	removing.await.unwrap().unwrap();
	// The watch either never starts (the removal won the lock) or starts holding a signal the
	// removal has already tripped. What must not happen is a live signal for a dead pair.
	if let Ok((_, removed)) = watching.await.unwrap() {
		assert!(
			*removed.borrow(),
			"the watch subscribed after the removal had already passed by: its loop would \
				 retry a pair that no longer exists forever"
		);
	}

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pause and a removal of the same pair, in flight at once from two callers of one engine. An
/// id left behind in the paused set describes a pair that no longer exists: it makes
/// [`SyncEngine::is_paused`] report a phantom, and the set grows for as long as the engine is
/// open.
#[tokio::test]
async fn a_pause_racing_a_removal_leaves_no_paused_id_behind() {
	let path = std::env::temp_dir().join(format!("filen_sync_pause_race_{}.db", Uuid::new_v4()));
	let engine = Arc::new(
		SyncEngine::open(Arc::new(offline_client()), path.clone())
			.await
			.unwrap(),
	);
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	// The registry lock fixes the interleaving: both verbs queue on it while this guard is held,
	// and tokio's mutex hands it out in request order, so the pause runs first and the removal
	// second — the order in which a pause that persisted its flag can land after the row it
	// describes is already gone.
	let guard = engine.registry.lock().await;
	let pausing = tokio::spawn({
		let engine = Arc::clone(&engine);
		async move { engine.pause_pair(pair).await }
	});
	tokio::task::yield_now().await;
	let removing = tokio::spawn({
		let engine = Arc::clone(&engine);
		async move { engine.remove_pair(pair).await }
	});
	tokio::task::yield_now().await;
	drop(guard);

	// The pause itself may legitimately succeed or be refused, depending on which verb the
	// registry lock served first; what must hold either way is that nothing stays paused.
	let _ = pausing.await.unwrap();
	removing.await.unwrap().unwrap();
	assert!(
		!engine.is_paused(pair).await,
		"the removed pair left its id in the paused set; the next pair sqlite gives that id \
			 would never sync"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// One pair's store, held for as long as that pair's whole-tree read holds it. Answers once the
/// lock is actually in hand, and says so for as long as it keeps it, so a test can prove what
/// ran DURING the hold rather than timing anything.
fn store_held_for(
	store: SharedStore,
	how_long: Duration,
) -> (
	tokio::sync::oneshot::Receiver<()>,
	Arc<AtomicBool>,
	tokio::task::JoinHandle<()>,
) {
	let (acquired_tx, acquired_rx) = tokio::sync::oneshot::channel();
	let holding = Arc::new(AtomicBool::new(true));
	let task = {
		let holding = Arc::clone(&holding);
		tokio::task::spawn_blocking(move || {
			let _guard = locked(&store);
			let _ = acquired_tx.send(());
			std::thread::sleep(how_long);
			holding.store(false, Ordering::SeqCst);
		})
	};
	(acquired_rx, holding, task)
}

/// A pass on one pair must not hold up the control verbs of another. The pass's widest step is
/// its baseline read, which holds that pair's store for as long as the tree takes; before the
/// split there was one store for the whole engine, so `list_pairs`, `pause_pair` and
/// `resolve_conflict` on ANY pair queued behind it.
///
/// Asserted as "they finished while the hold was still in force" rather than as a deadline: the
/// property is that they never wait for that lock at all, and that reads the same on a busy
/// machine as on an idle one.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_pairs_store_hold_does_not_delay_another_pairs_control_verbs() {
	let (engine, pair_a, path) = engine_with_pair("store_split_control").await;
	let (pair_b, _) = locked(&engine.control)
		.create_pair("/root-b", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let store_b = engine.pair_store(pair_b).await.unwrap();
	locked(&store_b)
		.upsert_entry(pair_b, &converged_conflict(Uuid::new_v4()))
		.unwrap();

	let store_a = engine.pair_store(pair_a).await.unwrap();
	let (acquired, holding, wedge) = store_held_for(store_a, Duration::from_millis(750));
	acquired.await.unwrap();

	let started = std::time::Instant::now();
	engine.list_pairs().await.unwrap();
	engine.pause_pair(pair_b).await.unwrap();
	assert_eq!(
		engine
			.resolve_conflict(pair_b, "a.txt", ConflictResolution::KeepLocal)
			.await
			.unwrap(),
		None,
		"keeping the local side moves no copy into the bin"
	);
	let elapsed = started.elapsed();
	assert!(
		holding.load(Ordering::SeqCst),
		"pair B's control verbs waited out pair A's store hold: they took {elapsed:?}, by \
			 which time the hold had already been released"
	);

	wedge.await.unwrap();
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// And when a verb DOES have to wait for its own pair's store, it waits on a blocking thread:
/// the runtime's worker keeps running other tasks meanwhile. One worker thread here, so a task
/// that still makes progress proves the wait is not on it — which is what stops a pass's
/// whole-tree SQLite work from stalling a runtime this engine shares with everything else.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_pairs_whole_tree_store_work_waits_off_the_runtime_thread() {
	let (engine, pair, path) = engine_with_pair("store_off_runtime").await;
	let store = engine.pair_store(pair).await.unwrap();
	let (acquired, holding, wedge) = store_held_for(store, Duration::from_millis(300));
	acquired.await.unwrap();

	let ticks = Arc::new(AtomicUsize::new(0));
	let ticking = {
		let ticks = Arc::clone(&ticks);
		tokio::spawn(async move {
			loop {
				tokio::time::sleep(Duration::from_millis(10)).await;
				ticks.fetch_add(1, Ordering::SeqCst);
			}
		})
	};

	// This reads the pair's conflicts, so it wants the very lock the wedge is holding.
	let started = std::time::Instant::now();
	engine.list_conflicts(pair).await.unwrap();
	let waited = started.elapsed();
	ticking.abort();

	assert!(
		!holding.load(Ordering::SeqCst) && waited >= Duration::from_millis(150),
		"the read did not actually contend for the store it shares with the hold: {waited:?}"
	);
	assert!(
		ticks.load(Ordering::SeqCst) >= 5,
		"the runtime's only worker made almost no progress ({} ticks) while a store read \
			 waited: the wait is on the runtime thread, not on a blocking one",
		ticks.load(Ordering::SeqCst)
	);

	wedge.await.unwrap();
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A fresh engine on a throwaway baseline DB, with one pair registered.
///
/// An offline client is enough to open an engine, here and in every test that opens one:
/// [`SyncEngine::open`] only hands it to the cache to subscribe each pair's remote root, and a
/// subscription the cache refuses leaves that pair on the grace window instead of failing the
/// open (see [`SyncEngine::observe_pair`]) — which is exactly what an unconfigured cache
/// answers.
async fn engine_with_pair(tag: &str) -> (SyncEngine, PairId, PathBuf) {
	let path = std::env::temp_dir().join(format!("filen_sync_{tag}_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	(engine, pair, path)
}

/// The user ignore patterns round-trip, and a text with a bad line is refused whole, naming that
/// line and leaving the stored text alone.
#[tokio::test]
async fn user_ignore_patterns_round_trip_and_a_bad_text_is_refused_whole() {
	let (engine, _, path) = engine_with_pair("user_ignore").await;
	assert_eq!(engine.user_ignore().await.unwrap(), "");
	engine.set_user_ignore("*.psd\nbuild/").await.unwrap();
	assert_eq!(engine.user_ignore().await.unwrap(), "*.psd\nbuild/");

	let error = engine
		.set_user_ignore("*.tmp\n{a\n*.log")
		.await
		.unwrap_err()
		.to_string();
	assert!(
		error.contains("user ignore patterns:2:"),
		"the refusal must name the bad line: {error}"
	);
	assert_eq!(
		engine.user_ignore().await.unwrap(),
		"*.psd\nbuild/",
		"a refused text must not replace the stored one"
	);

	// The signal every watch loop waits on fires for a text that was STORED, and not for one
	// that was refused — a wake-up for patterns nothing can read is a pass for nothing.
	let rules = engine.user_ignore_changes();
	assert!(!rules.has_changed().unwrap());
	engine.set_user_ignore("*.tmp\n{a").await.unwrap_err();
	assert!(!rules.has_changed().unwrap());
	engine.set_user_ignore("*.tmp").await.unwrap();
	assert!(rules.has_changed().unwrap());

	// And the setter does not depend on anyone listening: the last watch going away must not
	// turn the next call into an error.
	drop(rules);
	engine.set_user_ignore("*.log").await.unwrap();
	assert_eq!(engine.user_ignore().await.unwrap(), "*.log");

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// `list_conflicts` returns exactly the rows held for resolution — both flavours, with what each
/// side held — and nothing from a synced row; an unknown pair is an error, not an empty list.
#[tokio::test]
async fn list_conflicts_reads_the_held_rows_only() {
	let (engine, pair, path) = engine_with_pair("list_conflicts").await;
	assert!(engine.list_conflicts(pair).await.unwrap().is_empty());
	let hash = Blake3Hash::from([7; 32]);
	{
		let pair_store = engine.pair_store(pair).await.unwrap();
		let store = locked(&pair_store);
		store
			.upsert_entry(pair, &synced_file("synced.txt", Uuid::new_v4(), hash, 1))
			.unwrap();
		store
			.upsert_entry(
				pair,
				&BaselineEntry {
					state: BaselineState::Conflicted,
					local_kind: Some(NodeKind::File),
					remote_kind: None,
					..synced_file("b/edited.txt", Uuid::new_v4(), hash, 2)
				},
			)
			.unwrap();
		store
			.upsert_entry(
				pair,
				&BaselineEntry {
					state: BaselineState::Overwritten,
					local_kind: Some(NodeKind::File),
					remote_kind: Some(NodeKind::File),
					..synced_file("a.txt", Uuid::new_v4(), hash, 3)
				},
			)
			.unwrap();
	}
	assert_eq!(
		engine.list_conflicts(pair).await.unwrap(),
		vec![
			PlannedConflict {
				rel_path: "a.txt".to_string(),
				local: Some(PlannedNodeKind::File),
				remote: Some(PlannedNodeKind::File),
			},
			PlannedConflict {
				rel_path: "b/edited.txt".to_string(),
				local: Some(PlannedNodeKind::File),
				remote: None,
			},
		]
	);
	assert!(engine.list_conflicts(pair + 1000).await.is_err());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pass that fails outright tells its observer, with the error the call returns. Here the pair
/// is gone, so the pass fails before it reads either side and that is its only event.
#[tokio::test]
async fn a_pass_that_fails_outright_reports_pass_failed() {
	let (engine, pair, path) = engine_with_pair("pass_failed").await;
	engine.remove_pair(pair).await.unwrap();

	let mut events = Vec::new();
	let error = engine
		.sync_once_observed(pair, &mut |event| events.push(event))
		.await
		.unwrap_err();
	assert_eq!(
		events,
		vec![SyncEvent::PassFailed {
			error: error.to_string(),
			kind: ErrorKind::InvalidState,
		}]
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A stand-in for the pass in flight, shaped like the action loop in [`apply`]: the checkpoint
/// before an action, a transfer that never finishes on its own, the row it would record once it
/// did, and the checkpoint of the action behind it. It resolves to what it got done — which,
/// once the pair it syncs is removed, must be nothing at all.
fn pass_in_flight(gate: PassGate) -> tokio::task::JoinHandle<Vec<&'static str>> {
	tokio::spawn(async move {
		let mut done = Vec::new();
		if !gate.wait_to_start().await {
			return done;
		}
		if gate.guard(std::future::pending::<()>()).await.is_some() {
			done.push("the transfer's baseline row");
		}
		if gate.wait_to_start().await {
			done.push("the action behind it");
		}
		done
	})
}

/// Removing a pair stops the pass in flight instead of letting it run on: the transfer is
/// dropped where it stands, it records nothing, the actions queued behind it never start — and
/// the removal does not return until that has happened, so a completed action is not left
/// racing the delete of the rows it would write into.
#[tokio::test]
async fn removing_a_pair_drops_the_transfer_in_flight_and_waits_for_the_pass() {
	let (engine, pair, path) = engine_with_pair("remove_mid_pass").await;
	let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
	tokio::task::yield_now().await;
	assert!(
		!pass.is_finished(),
		"the stand-in pass was supposed to be stuck in its transfer"
	);

	engine.remove_pair(pair).await.unwrap();
	assert!(
		pass.is_finished(),
		"remove_pair returned with the pass it retired the pair under still running"
	);
	assert_eq!(
		pass.await.unwrap(),
		Vec::<&str>::new(),
		"the pass kept working after its pair was removed"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The same, for a pass PARKED on the pair's pause: the removal converts that suspension into a
/// cancel, so the parked action never starts. Nobody is left to resume it — the pair it would
/// sync is gone.
#[tokio::test]
async fn removing_a_paused_pair_never_lets_its_parked_action_start() {
	let (engine, pair, path) = engine_with_pair("remove_parked").await;
	engine.pause_pair(pair).await.unwrap();
	let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
	tokio::task::yield_now().await;
	assert!(!pass.is_finished(), "a suspended pass must park, not end");

	engine.remove_pair(pair).await.unwrap();
	assert!(
		pass.is_finished(),
		"remove_pair returned with a pass still parked on the pair it removed"
	);
	assert_eq!(
		pass.await.unwrap(),
		Vec::<&str>::new(),
		"the parked action started after its pair was removed"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The two things that cut a pass loose — a watch being stopped (whose engine-side half is
/// [`SyncEngine::cancel_suspended_pass`], polled by the loop's unparking for as long as it
/// lives) and the pair being removed — both wait for something, and neither may end up waiting
/// on the other. Nor may a pause that never escalates park the removal.
#[tokio::test]
async fn a_stop_and_a_removal_of_the_same_pair_never_wait_on_each_other() {
	// STOP first: the watch is already gone and has cut the parked pass loose; the removal that
	// follows finds a pass that is cancelled but has not unwound yet.
	let (engine, pair, path) = engine_with_pair("stop_then_remove").await;
	engine.pause_pair(pair).await.unwrap();
	let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
	tokio::task::yield_now().await;
	engine.cancel_suspended_pass(pair).await;
	engine.remove_pair(pair).await.unwrap();
	assert!(
		pass.is_finished(),
		"the removal left the stopped pass behind"
	);
	assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
	drop(engine);
	std::fs::remove_file(&path).ok();

	// REMOVE first: the stopped watch keeps unparking a pair the engine has already forgotten,
	// which must neither block nor resurrect the control state the removal took away.
	let (engine, pair, path) = engine_with_pair("remove_then_stop").await;
	engine.pause_pair(pair).await.unwrap();
	let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
	tokio::task::yield_now().await;
	engine.remove_pair(pair).await.unwrap();
	engine.cancel_suspended_pass(pair).await;
	assert!(
		pass.is_finished(),
		"the removal did not end the parked pass"
	);
	assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
	assert!(
		!engine.is_paused(pair).await,
		"the removed pair kept its control state"
	);
	drop(engine);
	std::fs::remove_file(&path).ok();

	// PAUSE first, with no escalation window at all: nothing but the removal can end this pass,
	// so a removal that waited for the pause to be reversed would wait for ever.
	let (engine, pair, path) = engine_with_pair("pause_then_remove").await;
	let pass = pass_in_flight(engine.pass_gate(pair).await.unwrap());
	tokio::task::yield_now().await;
	engine
		.pause_pair_with(
			pair,
			PauseOptions {
				mode: PauseMode::Suspend,
				cancel_after: None,
			},
		)
		.await
		.unwrap();
	engine.remove_pair(pair).await.unwrap();
	assert!(
		pass.is_finished(),
		"the removal returned while a pass sat on a pause nothing else can end"
	);
	assert_eq!(pass.await.unwrap(), Vec::<&str>::new());
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The window the tests below aim at: the removal has cancelled the pass and is waiting for
/// it to end, and a control call lands in the middle of that wait. Reports what the pass got
/// done and how long the removal took.
async fn a_removal_raced_by<F, Fut>(
	engine: &Arc<SyncEngine>,
	pair: PairId,
	interfere: F,
) -> (Vec<&'static str>, Duration)
where
	F: FnOnce() -> Fut,
	Fut: std::future::Future<Output = ()>,
{
	let gate = engine.pass_gate(pair).await.unwrap();
	let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
	let (release_tx, release_rx) = tokio::sync::oneshot::channel();
	// The pass in flight, with its bookkeeping held up the way a sibling transfer contending for
	// its pair's store lock holds it up: that is the window the removal spends waiting for it.
	let pass = tokio::spawn(async move {
		let mut done = Vec::new();
		if gate.guard(std::future::pending::<()>()).await.is_some() {
			done.push("the transfer's baseline row");
		}
		dropped_tx.send(()).unwrap();
		release_rx.await.unwrap();
		if gate.wait_to_start().await {
			done.push("the action behind it");
		}
		done
	});

	let started = tokio::time::Instant::now();
	let removing = tokio::spawn({
		let engine = Arc::clone(engine);
		async move { engine.remove_pair(pair).await }
	});
	// The transfer was dropped, so the removal has cancelled the pass and is now waiting for it.
	dropped_rx.await.unwrap();
	interfere().await;
	release_tx.send(()).unwrap();

	removing.await.unwrap().unwrap();
	let elapsed = started.elapsed();
	let done = tokio::time::timeout(Duration::from_secs(60), pass)
		.await
		.expect("the removal left the pass parked on a pair that is gone")
		.unwrap();
	(done, elapsed)
}

/// A pause landing while the removal waits for the pass it cancelled must not put that pass back
/// to sleep. Nothing left in the engine could ever wake it again — the persisted flag, the
/// control channel and the map entry all go with the pair row — so it would park for ever,
/// holding the drive-write lock every other pair's pass queues on. The caller is told so, rather
/// than handed an `Ok` for a pause that took on nothing.
#[tokio::test(start_paused = true)]
async fn a_pause_landing_while_a_removal_waits_cannot_re_park_the_pass() {
	let (engine, pair, path) = engine_with_pair("pause_during_removal").await;
	let engine = Arc::new(engine);
	let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
		let refused = engine
			.pause_pair_with(
				pair,
				PauseOptions {
					mode: PauseMode::Suspend,
					// Nothing but the removal can end this pass: a suspension that never
					// escalates.
					cancel_after: None,
				},
			)
			.await
			.expect_err("a pause during a removal must not report success");
		assert!(
			refused.to_string().contains("is being removed"),
			"the refusal must say why: {refused}"
		);
	})
	.await;
	assert_eq!(
		done,
		Vec::<&str>::new(),
		"the pass kept working after its pair was removed"
	);
	// The cancel has to keep the last word THROUGHOUT the wait, not merely be re-sent once the
	// grace elapsed: a pass re-parked for those ten seconds sits on the drive-write lock every
	// other pair queues on, and the removal blocks the caller for as long.
	assert!(
		elapsed < REMOVE_CANCEL_GRACE,
		"the removal took {elapsed:?}: the pause re-parked the pass and only the grace \
			 timeout got it out"
	);
	assert!(
		!engine.is_paused(pair).await,
		"the removed pair kept its control state"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The other half: a RESUME landing in the same window must not restart the pass the removal
/// cancelled. It would apply the rest of its plan — uploads, remote trashes, local deletions —
/// against a pair whose rows are deleted moments later. It is refused, for the same reason the
/// pause above is.
#[tokio::test(start_paused = true)]
async fn a_resume_landing_while_a_removal_waits_cannot_restart_the_pass() {
	let (engine, pair, path) = engine_with_pair("resume_during_removal").await;
	let engine = Arc::new(engine);
	let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
		let refused = engine
			.resume_pair(pair)
			.await
			.expect_err("a resume during a removal must not report success");
		assert!(
			refused.to_string().contains("is being removed"),
			"the refusal must say why: {refused}"
		);
	})
	.await;
	assert_eq!(
		done,
		Vec::<&str>::new(),
		"the resume let the cancelled pass carry on against a pair being removed"
	);
	assert!(
		elapsed < REMOVE_CANCEL_GRACE,
		"the removal took {elapsed:?} instead of ending with the pass it cancelled"
	);
	assert!(
		!engine.is_paused(pair).await,
		"the removed pair kept its control state"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// And the third verb: cancelling the paused actions of a pair whose removal is waiting for the
/// pass it cancelled. The pair reads as paused — `Retired` is not `Run` — but there is no pause
/// to convert and no pair to leave paused, so an `Ok` here tells the caller its suspension was
/// given up when nothing of the kind happened. It is refused exactly as the pause and the resume
/// above are.
#[tokio::test(start_paused = true)]
async fn cancelling_the_paused_actions_while_a_removal_waits_is_refused() {
	let (engine, pair, path) = engine_with_pair("cancel_during_removal").await;
	let engine = Arc::new(engine);
	let (done, elapsed) = a_removal_raced_by(&engine, pair, || async {
		let refused = engine
			.cancel_paused_actions(pair)
			.await
			.expect_err("a cancel during a removal must not report success");
		assert!(
			refused.to_string().contains("is being removed"),
			"the refusal must say why, not read as an ordinary pause: {refused}"
		);
	})
	.await;
	assert_eq!(
		done,
		Vec::<&str>::new(),
		"the pass kept working after its pair was removed"
	);
	assert!(
		elapsed < REMOVE_CANCEL_GRACE,
		"the removal took {elapsed:?} instead of ending with the pass it cancelled"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A real pass registers its control channel BEFORE it reads either side: a pass that has read
/// its pair row but holds no gate is one a cancel can neither reach nor wait for, and reading
/// two trees takes as long as writing them, so that window is not a corner case.
#[tokio::test]
async fn a_pass_takes_its_gate_before_it_reads_either_side() {
	let (engine, pair, path) = engine_with_pair("gate_before_read").await;
	// Offline, so the pass fails in its remote enumeration. That it registered a control channel
	// before getting that far is what says the gate came first.
	assert!(
		engine.sync_once(pair).await.is_err(),
		"the offline stand-in was supposed to fail in its remote enumeration"
	);
	assert!(
		engine.paused.lock().await.contains_key(&pair),
		"the pass read the two sides before taking its gate: a cancel landing there would \
			 neither reach it nor wait for it"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pass that could not take the drive lock ran none of its plan, so it proves nothing about any
/// path it planned: every failure streak must come through it untouched. Clearing them would
/// hand a permanently broken path a fresh set of retries each time the lock is contended.
#[tokio::test]
async fn a_pass_that_could_not_take_the_drive_lock_leaves_every_failure_streak_alone() {
	let (engine, pair, path) = engine_with_pair("lock_failure_streaks").await;
	{
		let pair_store = engine.pair_store(pair).await.unwrap();
		let store = locked(&pair_store);
		let now = Utc::now().timestamp_millis();
		store
			.record_failure(pair, "broken.txt", "boom", now)
			.unwrap();
		store
			.record_failure(pair, "broken.txt", "boom", now)
			.unwrap();
	}
	let before = locked(&engine.pair_store(pair).await.unwrap())
		.failures(pair)
		.unwrap();

	let attempted = [upload("broken.txt"), upload("fine.txt")];
	let mut report = SyncReport::default();
	let mut events = Vec::new();
	apply::note_lock_failure(
		&mut report,
		attempted.len(),
		&Error::custom(ErrorKind::RetryFailed, "the drive lock is held elsewhere"),
		&mut |event| events.push(event),
	);
	let (_dir, stage) = Stage::in_temp_dir();
	let screen = Screen::new(stage, &[], &attempted).unwrap();
	let ends = endable_streaks(&screen, Some(&before)).unwrap();
	engine.note_path_outcomes(pair, ends, &mut report).await;

	assert_eq!(
		locked(&engine.pair_store(pair).await.unwrap())
			.failures(pair)
			.unwrap(),
		before,
		"a pass that applied nothing touched a path's failure streak"
	);
	assert_eq!(
		report.interrupted,
		attempted.len(),
		"the pass owes every action it planned"
	);
	assert_eq!(
		events,
		vec![SyncEvent::Interrupted {
			actions: attempted.len()
		}]
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pass with `docs/a.txt`'s earlier failed trash on record, whose plan folded that trash into
/// the trash of `docs`: the engine, the pair, the store's file, and the streaks the folded plan
/// can end.
async fn streak_under_a_folded_directory(
	tag: &str,
) -> (SyncEngine, PairId, PathBuf, Vec<(String, String)>) {
	let (engine, pair, path) = engine_with_pair(tag).await;
	let streaks = {
		let pair_store = engine.pair_store(pair).await.unwrap();
		let store = locked(&pair_store);
		store
			.record_failure(pair, "docs/a.txt", "boom", Utc::now().timestamp_millis())
			.unwrap();
		store.failures(pair).unwrap()
	};
	let trash = |rel_path: &str, kind| SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: Uuid::from_u128(1),
	};
	let actions = [
		trash("docs/a.txt", NodeKind::File),
		trash("docs", NodeKind::Dir),
	];
	let (_dir, stage) = Stage::in_temp_dir();
	let mut screen = Screen::new(stage, &[], &actions).unwrap();
	screen.fold_subsumed().unwrap();
	let ends = endable_streaks(&screen, Some(&streaks)).unwrap();
	(engine, pair, path, ends)
}

/// The deletions a directory deletion carried out happened with it: when it succeeds, their
/// failure streaks end with its own, or a later item at one of those paths would inherit a
/// streak it never earned.
#[tokio::test]
async fn a_directory_deletion_that_succeeds_ends_the_streaks_of_the_deletions_it_carried() {
	let (engine, pair, path, ends) = streak_under_a_folded_directory("folded_streak_cleared").await;

	let mut report = SyncReport::default();
	engine.note_path_outcomes(pair, ends, &mut report).await;

	assert!(report.errors.is_empty(), "{:?}", report.errors);
	assert_eq!(
		locked(&engine.pair_store(pair).await.unwrap())
			.failures(pair)
			.unwrap(),
		HashMap::new()
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A directory deletion that failed attempted none of the deletions it carried on their own, so
/// their streaks stay exactly as they were, and only the directory's grows.
#[tokio::test]
async fn a_directory_deletion_that_fails_leaves_the_streaks_of_the_deletions_it_carried() {
	let (engine, pair, path, ends) = streak_under_a_folded_directory("folded_streak_kept").await;
	let before = locked(&engine.pair_store(pair).await.unwrap())
		.failures(pair)
		.unwrap();

	let mut report = SyncReport {
		failed_paths: vec![("docs".to_string(), "boom".to_string())],
		..SyncReport::default()
	};
	engine.note_path_outcomes(pair, ends, &mut report).await;

	let after = locked(&engine.pair_store(pair).await.unwrap())
		.failures(pair)
		.unwrap();
	assert_eq!(after.get("docs/a.txt"), before.get("docs/a.txt"));
	assert_eq!(after.get("docs").map(|failure| failure.attempts), Some(1));

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pass whose actions end no streak still starts one for each path that failed: a first
/// failure has no streak to end, and is the one that begins it.
#[tokio::test]
async fn a_pass_that_ends_no_streak_still_records_its_failures() {
	let (engine, pair, path) = engine_with_pair("first_failure_recorded").await;

	let mut report = SyncReport {
		failed_paths: vec![("broken.txt".to_string(), "boom".to_string())],
		..SyncReport::default()
	};
	engine
		.note_path_outcomes(pair, Vec::new(), &mut report)
		.await;

	assert!(report.errors.is_empty(), "{:?}", report.errors);
	let after = locked(&engine.pair_store(pair).await.unwrap())
		.failures(pair)
		.unwrap();
	assert_eq!(
		after.get("broken.txt").map(|failure| failure.attempts),
		Some(1)
	);
	assert_eq!(after.len(), 1);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A REAL pass, parked in its read half: the remote enumeration is the first thing that wants
/// the client's cache slot, so holding that slot stops `sync_once` inside `prepare` — under the
/// gate it took first, which is the window the two tests below land a control call in.
fn a_pass_parked_in_its_read(
	engine: &Arc<SyncEngine>,
	pair: PairId,
) -> tokio::task::JoinHandle<Result<SyncReport, Error>> {
	let engine = Arc::clone(engine);
	tokio::spawn(async move { engine.sync_once(pair).await })
}

/// How long a test waits on the paused clock for every task to get as far as it can. The clock
/// stands still while any task can run and while a `spawn_blocking` hop is in flight, so the
/// wait ends only once every task, and every blocking hop it awaits, is waiting on something
/// that will not come. Counting yields instead races those hops, which run on the blocking
/// pool. Short, so that no timer of the engine's fires inside it.
const SETTLED: Duration = Duration::from_millis(1);

/// Wait for that pass to reach its gate — it registers the pair's control channel before it
/// reads either side — and then to park in the read behind it.
async fn parked(engine: &SyncEngine, pair: PairId) {
	tokio::time::sleep(SETTLED).await;
	assert!(
		engine.paused.lock().await.contains_key(&pair),
		"the pass never took its gate"
	);
}

/// What a task a control call just woke ended with, or `None` if it is still waiting once
/// everything has settled (see [`SETTLED`]).
async fn ended<T>(task: &mut tokio::task::JoinHandle<T>) -> Option<T> {
	tokio::time::timeout(SETTLED, task)
		.await
		.ok()
		.map(|joined| joined.unwrap())
}

/// And a removal landing in that window reaches it: the read is dropped where it stands, the
/// pass ends without planning anything, and the removal waits for its gate to close instead of
/// deleting the rows out from under a pass it never saw.
#[tokio::test(start_paused = true)]
async fn a_removal_during_the_read_half_ends_the_pass_before_it_plans() {
	let (engine, pair, path) = engine_with_pair("remove_during_prepare").await;
	let engine = Arc::new(engine);
	let slot = engine.client.cache_slot.lock().await;
	let pass = a_pass_parked_in_its_read(&engine, pair);
	parked(&engine, pair).await;
	assert!(
		!pass.is_finished(),
		"the pass was supposed to be stuck in its read"
	);

	let started = tokio::time::Instant::now();
	engine.remove_pair(pair).await.unwrap();
	let elapsed = started.elapsed();
	// The removal returns once the pass's gate has closed, while its task may still be giving
	// back its page cache on the blocking pool and reporting how it ended; let that tail run.
	tokio::time::sleep(SETTLED).await;
	assert!(
		pass.is_finished(),
		"remove_pair returned with a pass still reading the pair it removed"
	);
	// Waited for the pass rather than giving up on a read it could not reach.
	assert!(
		elapsed < REMOVE_CANCEL_GRACE,
		"the removal took {elapsed:?}: the cancel never reached the pass in its read"
	);

	// Only now, and there is nothing left to let through: the read was dropped where it stood.
	drop(slot);
	let refused = pass.await.unwrap().expect_err(
		"the read survived the removal, so the pass planned against rows that are gone",
	);
	assert!(
		refused.to_string().contains("is being removed"),
		"a pass dropped by a removal must say so, not report a read that failed: {refused}"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The same window, CANCELLED rather than removed: the pair is still there, so the pass reports
/// the pause instead of an error — the very report a pause landing a moment earlier would have
/// produced, with nothing planned, applied or recorded.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_read_half_ends_the_pass_with_the_paused_report() {
	let (engine, pair, path) = engine_with_pair("cancel_during_prepare").await;
	let engine = Arc::new(engine);
	let slot = engine.client.cache_slot.lock().await;
	let mut pass = a_pass_parked_in_its_read(&engine, pair);
	parked(&engine, pair).await;
	assert!(
		!pass.is_finished(),
		"the pass was supposed to be stuck in its read"
	);

	engine
		.pause_pair_with(
			pair,
			PauseOptions {
				mode: PauseMode::Cancel,
				cancel_after: None,
			},
		)
		.await
		.unwrap();
	let report = ended(&mut pass)
		.await
		.expect("the cancel never reached the pass reading the two sides")
		.expect("a cancelled read is not a failure: the pair is still there to be reported on");
	drop(slot);
	assert_eq!(
		report,
		SyncReport {
			paused: true,
			..SyncReport::default()
		},
		"a pass dropped in its read must report the pause and nothing else"
	);
	assert!(
		locked(&engine.pair_store(pair).await.unwrap())
			.entries(pair)
			.unwrap()
			.is_empty(),
		"a pass that never had a plan wrote a baseline row"
	);
	assert!(
		engine.is_paused(pair).await,
		"the cancel left the pair running"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A resolution landing while a pass reads the pair waits for that pass: the pass records its
/// conflict rows from what it read, so a resolution written in between would be put back to
/// `Conflicted` with nobody told — and, for `KeepBoth`, with the local copy already renamed.
#[tokio::test(start_paused = true)]
async fn a_resolution_waits_for_the_pass_reading_the_pair() {
	let (engine, pair, path) = engine_with_pair("resolve_during_prepare").await;
	let engine = Arc::new(engine);
	locked(&engine.pair_store(pair).await.unwrap())
		.upsert_entry(pair, &converged_conflict(Uuid::new_v4()))
		.unwrap();
	let slot = engine.client.cache_slot.lock().await;
	let pass = a_pass_parked_in_its_read(&engine, pair);
	parked(&engine, pair).await;

	let mut resolving = tokio::spawn({
		let engine = Arc::clone(&engine);
		async move {
			engine
				.resolve_conflict(pair, "a.txt", ConflictResolution::KeepLocal)
				.await
		}
	});
	assert!(
		ended(&mut resolving).await.is_none(),
		"the resolution landed while a pass was still reading the pair it would re-record"
	);
	assert!(
		locked(&engine.pair_store(pair).await.unwrap())
			.entry(pair, "a.txt")
			.unwrap()
			.is_some_and(|row| row.state == BaselineState::Conflicted),
		"the resolution wrote its row under the pass"
	);

	// The offline pass fails in its read once it gets the slot, and lets the resolution through.
	drop(slot);
	assert!(pass.await.unwrap().is_err());
	assert_eq!(
		resolving.await.unwrap().unwrap(),
		None,
		"keeping the local side moves no copy into the bin"
	);
	assert_eq!(
		locked(&engine.pair_store(pair).await.unwrap())
			.entry(pair, "a.txt")
			.unwrap()
			.map(|row| row.state),
		Some(BaselineState::Synced)
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A paused pair holds its conflicts like any other, and the caller must be able to settle them
/// before resuming: the pause does not make the resolution wait.
#[tokio::test]
async fn a_paused_pair_is_still_resolvable() {
	let (engine, pair, path) = engine_with_pair("resolve_paused").await;
	engine
		.pause_pair_with(
			pair,
			PauseOptions {
				mode: PauseMode::Suspend,
				cancel_after: None,
			},
		)
		.await
		.unwrap();
	locked(&engine.pair_store(pair).await.unwrap())
		.upsert_entry(pair, &converged_conflict(Uuid::new_v4()))
		.unwrap();
	assert_eq!(
		engine
			.resolve_conflict(pair, "a.txt", ConflictResolution::KeepLocal)
			.await
			.expect("a paused pair must stay resolvable"),
		None
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// And the last control verb: a resolution landing while a removal waits for its pass is refused
/// with the words the others use, rather than writing a row the removal is about to take away.
#[tokio::test(start_paused = true)]
async fn resolving_a_conflict_while_a_removal_waits_is_refused() {
	let (engine, pair, path) = engine_with_pair("resolve_during_removal").await;
	let engine = Arc::new(engine);
	let (done, _) = a_removal_raced_by(&engine, pair, || async {
		let refused = engine
			.resolve_conflict(pair, "a.txt", ConflictResolution::KeepBoth)
			.await
			.expect_err("a resolution during a removal must not report success");
		assert!(
			refused.to_string().contains("is being removed"),
			"the refusal must say why: {refused}"
		);
	})
	.await;
	assert_eq!(done, Vec::<&str>::new());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// Keeping the remote side of an ordinary conflict moves the losing local edit to the bin, and
/// the call NAMES where it put it: that edit was never uploaded, so the download that follows
/// would otherwise destroy the only copy, and a caller offering it back cannot be left to
/// guess among the bin's ` (N)` siblings. A local copy that already holds the remote's content
/// has nothing to lose, stays put, and there is no path to name.
#[tokio::test]
async fn keeping_remote_quarantines_a_local_edit_the_remote_never_had() {
	let path = std::env::temp_dir().join(format!("filen_sync_keep_remote_{}.db", Uuid::new_v4()));
	let root = std::env::temp_dir().join(format!("filen_sync_keep_remote_{}", Uuid::new_v4()));
	std::fs::create_dir_all(&root).unwrap();
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(root.to_str().unwrap(), Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();

	// Diverged: the local copy's content is not the remote head's.
	std::fs::write(root.join("a.txt"), b"LOCAL").unwrap();
	let diverged = BaselineEntry {
		remote_hash: Some(hash(4)),
		..converged_conflict(Uuid::new_v4())
	};
	locked(&engine.pair_store(pair).await.unwrap())
		.upsert_entry(pair, &diverged)
		.unwrap();
	// The resolution takes its day from the clock: either side of the call brackets it, so the
	// assertion holds across a midnight.
	let before = Utc::now().date_naive();
	let quarantined = engine
		.resolve_conflict(pair, "a.txt", ConflictResolution::KeepRemote)
		.await
		.unwrap()
		.expect("the resolution has to say where it put the copy it moved aside");
	let after = Utc::now().date_naive();
	let in_bucket = |day| bucket(&root, day).join("a.txt");
	assert!(
		quarantined == in_bucket(before) || quarantined == in_bucket(after),
		"the path it named is not the day's bucket of the bin: {quarantined:?}"
	);
	assert_eq!(
		std::fs::read(&quarantined).unwrap(),
		b"LOCAL",
		"the losing local edit must be recoverable from the bin"
	);
	assert!(!root.join("a.txt").exists());
	assert_eq!(
		locked(&engine.pair_store(pair).await.unwrap())
			.entry(pair, "a.txt")
			.unwrap(),
		None,
		"with the local side moved away, the remote copy must read as a fresh create"
	);

	// Converged: nothing to lose, nothing moved.
	std::fs::write(root.join("b.txt"), b"SAME").unwrap();
	let converged = BaselineEntry {
		rel_path: "b.txt".to_string(),
		..converged_conflict(Uuid::new_v4())
	};
	locked(&engine.pair_store(pair).await.unwrap())
		.upsert_entry(pair, &converged)
		.unwrap();
	assert_eq!(
		engine
			.resolve_conflict(pair, "b.txt", ConflictResolution::KeepRemote)
			.await
			.unwrap(),
		None,
		"nothing was moved aside, so there is no path to name"
	);
	assert!(root.join("b.txt").exists());
	for bucket in std::fs::read_dir(root.join(scan::QUARANTINE_DIR)).unwrap() {
		assert!(!bucket.unwrap().path().join("b.txt").exists());
	}

	drop(engine);
	std::fs::remove_file(&path).ok();
	std::fs::remove_dir_all(&root).ok();
}

/// A pass prunes its pair's bin at most once a UTC day: the first pass of a day deletes the
/// expired buckets, a second one that day does not look again, and the next day's does.
#[tokio::test]
async fn a_pairs_bin_is_pruned_once_a_day() {
	let (engine, pair, path) = engine_with_pair("bin_prune_daily").await;
	let root = tempfile::tempdir().unwrap();
	dated_bin(root.path());
	let old = bucket(root.path(), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
	std::fs::create_dir_all(&old).unwrap();
	let gate = PassGate::default();

	assert_eq!(
		engine
			.prune_bin_daily(pair, root.path(), day(), &gate)
			.await,
		Some(PruneSummary {
			removed: 1,
			adopted: 0,
			errors: 0,
			stopped: false,
		})
	);
	assert!(!old.exists());

	std::fs::create_dir_all(&old).unwrap();
	assert_eq!(
		engine
			.prune_bin_daily(pair, root.path(), day(), &gate)
			.await,
		None,
		"the second pass of the day pruned again"
	);
	assert!(old.exists());

	let next_day = day().succ_opt().unwrap();
	assert_eq!(
		engine
			.prune_bin_daily(pair, root.path(), next_day, &gate)
			.await
			.map(|pruned| pruned.removed),
		Some(1)
	);
	assert!(!old.exists());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A prune a pause stopped does not count as the day's: the pair's next pass prunes again.
#[tokio::test]
async fn a_bin_prune_stopped_by_a_pause_runs_again_on_the_next_pass() {
	let (engine, pair, path) = engine_with_pair("bin_prune_paused").await;
	let root = tempfile::tempdir().unwrap();
	dated_bin(root.path());
	let old = bucket(root.path(), NaiveDate::from_ymd_opt(2026, 9, 1).unwrap());
	std::fs::create_dir_all(&old).unwrap();
	let paused = PassGate::new(tokio::sync::watch::Sender::new(PassControl::Cancelled));

	assert_eq!(
		engine
			.prune_bin_daily(pair, root.path(), day(), &paused)
			.await
			.map(|pruned| pruned.stopped),
		Some(true)
	);
	assert!(old.exists(), "a stopped prune deleted a bucket");

	assert_eq!(
		engine
			.prune_bin_daily(pair, root.path(), day(), &PassGate::default())
			.await
			.map(|pruned| pruned.removed),
		Some(1)
	);
	assert!(!old.exists());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pair with no baseline row (its first pass) has filen-sync's trash moved into its bin, under
/// the day of the pass.
#[tokio::test]
async fn filen_syncs_trash_is_binned_while_the_pair_has_no_row() {
	let path = std::env::temp_dir().join(format!("filen_sync_legacy_trash_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let root = tempfile::tempdir().unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(
			root.path().to_str().unwrap(),
			Uuid::new_v4(),
			SyncMode::TwoWay,
		)
		.unwrap();
	let trash = root.path().join(scan::LEGACY_TRASH_DIR);
	std::fs::create_dir(&trash).unwrap();
	std::fs::write(trash.join("old.txt"), b"old").unwrap();

	let before = Utc::now().date_naive();
	engine.migrate_legacy_trash(pair, PassGate::default()).await;
	let after = Utc::now().date_naive();

	assert!(!trash.exists());
	let binned = [before, after]
		.into_iter()
		.map(|day| {
			bucket(root.path(), day)
				.join(scan::LEGACY_TRASH_DIR)
				.join("old.txt")
		})
		.find(|binned| binned.exists())
		.expect("the trash was binned under the day of the pass");
	assert_eq!(std::fs::read(binned).unwrap(), b"old");

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A suspension does not stop the migration: the read after it runs on through one and can write
/// rows, after which no pass would look again. Only a cancel stops it.
#[tokio::test]
async fn filen_syncs_trash_is_binned_through_a_suspension_but_not_after_a_cancel() {
	let path = std::env::temp_dir().join(format!("filen_sync_legacy_trash_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let root = tempfile::tempdir().unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(
			root.path().to_str().unwrap(),
			Uuid::new_v4(),
			SyncMode::TwoWay,
		)
		.unwrap();
	let trash = root.path().join(scan::LEGACY_TRASH_DIR);
	std::fs::create_dir(&trash).unwrap();

	let (control, _rx) = tokio::sync::watch::channel(PassControl::Cancelled);
	engine
		.migrate_legacy_trash(pair, PassGate::new(control.clone()))
		.await;
	assert!(trash.exists(), "a cancelled pass moved filen-sync's trash");

	control.send_replace(PassControl::Suspended { escalate_at: None });
	engine
		.migrate_legacy_trash(pair, PassGate::new(control))
		.await;
	assert!(
		!trash.exists(),
		"a suspended pass left filen-sync's trash behind"
	);
	assert!(root.path().join(scan::QUARANTINE_DIR).exists());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The walk runs once per pair and engine: a first pass refused or cancelled after it does not walk
/// the tree again, so a folder that appears after the walk stays where it is (and still never
/// synced).
#[tokio::test]
async fn filen_syncs_trash_is_looked_for_once_per_engine() {
	let path = std::env::temp_dir().join(format!("filen_sync_legacy_trash_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let root = tempfile::tempdir().unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(
			root.path().to_str().unwrap(),
			Uuid::new_v4(),
			SyncMode::TwoWay,
		)
		.unwrap();

	engine.migrate_legacy_trash(pair, PassGate::default()).await;
	let trash = root.path().join(scan::LEGACY_TRASH_DIR);
	std::fs::create_dir(&trash).unwrap();
	std::fs::write(trash.join("late.txt"), b"late").unwrap();
	engine.migrate_legacy_trash(pair, PassGate::default()).await;

	assert_eq!(std::fs::read(trash.join("late.txt")).unwrap(), b"late");
	assert!(!root.path().join(scan::QUARANTINE_DIR).exists());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// Once the pair has a baseline row it has synced, and filen-sync's trash is not looked for: a
/// folder of that name is left where it is (and still never synced).
#[tokio::test]
async fn filen_syncs_trash_is_left_alone_once_the_pair_has_a_row() {
	let path = std::env::temp_dir().join(format!("filen_sync_legacy_trash_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let root = tempfile::tempdir().unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(
			root.path().to_str().unwrap(),
			Uuid::new_v4(),
			SyncMode::TwoWay,
		)
		.unwrap();
	locked(&engine.control)
		.upsert_entry(
			pair,
			&file_row("b.txt", Uuid::from_u128(5), Blake3Hash::from([1; 32])),
		)
		.unwrap();
	let trash = root.path().join(scan::LEGACY_TRASH_DIR);
	std::fs::create_dir(&trash).unwrap();
	std::fs::write(trash.join("new.txt"), b"new").unwrap();

	engine.migrate_legacy_trash(pair, PassGate::default()).await;

	assert_eq!(std::fs::read(trash.join("new.txt")).unwrap(), b"new");
	assert!(!root.path().join(scan::QUARANTINE_DIR).exists());

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A dry run stays a pure read: it takes NO pass gate, so it neither leaves control state behind
/// for a pair nobody paused nor makes a removal wait for a call that writes nothing.
#[tokio::test]
async fn a_dry_run_takes_no_pass_gate() {
	let (engine, pair, path) = engine_with_pair("plan_no_gate").await;
	// Offline, so the remote enumeration inside fails; the gate would have been taken before
	// that, which is what this is about.
	let _ = engine.plan_pair(pair).await;
	assert!(
		!engine.paused.lock().await.contains_key(&pair),
		"a dry run took a pass gate"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// An engine whose one pair's read gets as far as the baseline — the cache it reads the remote
/// from holds nothing under the pair's root, so no server is asked — and fails there: the index
/// the read asks first, whether a push of ours awaits confirmation, is gone. A baseline read
/// that fails panics on purpose, rather than read as an absent row.
async fn engine_whose_baseline_read_fails() -> (SyncEngine, PairId, tempfile::TempDir) {
	let dir = tempfile::tempdir().unwrap();
	let cache = dir.path().join("cache.db");
	drop(CacheState::new_on_path(&cache, Uuid::from_u128(1)));
	let client = Arc::new(offline_client());
	client.configure_cache(cache, |_| {}).await.unwrap();
	let db = dir.path().join("sync.db");
	let engine = SyncEngine::open(client, db.clone()).await.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::from_u128(2), SyncMode::TwoWay)
		.unwrap();
	// Once the pair's store is open, which creates the index.
	engine.pair_store(pair).await.unwrap();
	open_sqlite(&db)
		.unwrap()
		.execute_batch("DROP INDEX baseline_unconfirmed")
		.unwrap();
	(engine, pair, dir)
}

/// A dry run whose read fails comes back as the call's error, as a pass's does, instead of
/// unwinding into the caller.
#[tokio::test]
async fn a_dry_run_whose_read_fails_returns_the_failure() {
	let (engine, pair, _dir) = engine_whose_baseline_read_fails().await;
	let failed = engine
		.plan_pair(pair)
		.await
		.expect_err("the dry run read no baseline to plan from");
	assert!(
		failed
			.to_string()
			.contains("the dry run panicked: reading pair"),
		"{failed}"
	);
	drop(engine);
}

/// So does the read an adoption makes before it switches the mode, and the mode is left as it
/// was.
#[tokio::test]
async fn an_adoption_whose_read_fails_returns_the_failure_and_keeps_the_mode() {
	let (engine, pair, _dir) = engine_whose_baseline_read_fails().await;
	let failed = engine
		.reconfigure_pair(pair, SyncMode::LocalToRemote, Backlog::AdoptDestination)
		.await
		.expect_err("the adoption read no baseline to adopt into");
	assert!(
		failed
			.to_string()
			.contains("the adoption read panicked: reading pair"),
		"{failed}"
	);
	assert_eq!(
		locked(&engine.control).pair(pair).unwrap().unwrap().mode,
		SyncMode::TwoWay
	);
	drop(engine);
}

/// A control verb refuses the state it cannot act on rather than returning `Ok` for nothing:
/// cancelling the transfers of a pair nobody paused has no parked pass to unwind, and an `Ok`
/// there reads as "the transfers are being given up" while they run happily on.
#[tokio::test]
async fn cancelling_the_actions_of_a_pair_that_is_not_paused_is_refused() {
	let (engine, pair, path) = engine_with_pair("cancel_not_paused").await;

	let running = engine
		.cancel_paused_actions(pair)
		.await
		.expect_err("a pair nobody paused has no actions to cancel");
	assert!(
		running.to_string().contains("is not paused"),
		"the refusal must say what state the pair is in: {running}"
	);
	let unknown = engine
		.cancel_paused_actions(pair + 9_999)
		.await
		.expect_err("an unknown pair must not be cancellable");
	assert!(
		unknown.to_string().contains("unknown sync pair"),
		"an unknown id must still read as unknown, not as unpaused: {unknown}"
	);

	// Paused, it is exactly what the verb is for — and it leaves the pair paused, so a repeat is
	// idempotent rather than a second refusal.
	engine.pause_pair(pair).await.unwrap();
	engine.cancel_paused_actions(pair).await.unwrap();
	assert!(
		engine.is_paused(pair).await,
		"cancelling the paused actions un-paused the pair"
	);
	engine.cancel_paused_actions(pair).await.unwrap();

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A pass that was still SCANNING when its pair was removed holds no control channel, so the
/// removal neither reaches it nor waits for it. Asking for its gate afterwards is where it has
/// to learn the pair is gone — otherwise it is handed a fresh RUNNING gate and applies its whole
/// plan (uploads, remote trashes, local deletions) against rows the removal has taken away.
#[tokio::test]
async fn a_pass_that_asks_for_its_gate_after_the_removal_is_refused() {
	let (engine, pair, path) = engine_with_pair("gate_after_removal").await;
	engine.remove_pair(pair).await.unwrap();

	assert!(
		engine.pass_gate(pair).await.is_err(),
		"the pass got a gate for a pair that no longer exists"
	);
	assert!(
		!engine.paused.lock().await.contains_key(&pair),
		"asking for the gate left a control entry behind for a pair that is gone"
	);

	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// The everyday complaint: a second edit within a few seconds of the first. The cache has not
/// listed our upload yet, but the engine knows exactly what it wrote there — so the pass must
/// push the new content rather than leave the path alone until the cache agrees.
#[test]
fn a_local_edit_at_a_path_whose_create_is_pending_is_pushed() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("a.txt", None));

	let baseline = written_row(uuid, hash(1));
	// The cache is behind: its snapshot has nothing at all under the root yet.
	let mut remote = Side::default();
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new());

	let actions = plan::reconcile(
		SyncMode::LocalToRemote,
		&tree(&baseline),
		&local_map(hash(2)),
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	)
	.actions;
	assert_eq!(
		actions.iter().map(describe).collect::<Vec<_>>(),
		vec!["upload file \"a.txt\"".to_string()],
		"the edit is pushed against what we wrote, not deferred until the cache catches up"
	);
}

/// The everyday case the snapshot alone cannot settle: the file we uploaded is gone again —
/// trashed by the user, or superseded by another client's re-upload — so its uuid will NEVER
/// appear in a snapshot. Waiting for it to appear froze the path for the whole grace window
/// and every pass in between reported an all-zero no-op. The cache announcing the uuid is the
/// evidence that it has caught up.
#[test]
fn a_write_the_cache_has_announced_retires_even_though_the_item_is_gone() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("a.txt", None));

	observations.note_uuids([uuid]);
	let observed = observations.snapshot();

	let mut remote = Side::default();
	pending.settle(PAIR, &observed, &remote.whole());
	assert_eq!(
		pending.fold_into(
			PAIR,
			&tree(&written_row(uuid, hash(1))),
			&mut remote,
			&mut BTreeSet::new()
		),
		0,
		"an announced uuid retires the write even with nothing left at the path"
	);
	assert!(
		remote.whole().is_empty(),
		"and the snapshot is believed again: the item really is gone"
	);
}

/// The race the pre-snapshot copy exists for: an event committed AFTER the snapshot was read
/// describes a state that snapshot predates. Retiring on it would let this very pass go on to
/// read the just-written item as a remote-side deletion.
#[test]
fn an_announcement_made_after_the_snapshot_was_read_does_not_retire_the_write() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("a.txt", None));

	// The pass copies the observations, THEN reads the snapshot; the event lands in between.
	let observed = observations.snapshot();
	observations.note_uuids([uuid]);

	let mut remote = Side::default();
	pending.settle(PAIR, &observed, &remote.whole());
	assert_eq!(
		pending.fold_into(
			PAIR,
			&tree(&written_row(uuid, hash(1))),
			&mut remote,
			&mut BTreeSet::new()
		),
		1,
		"only what was known before the snapshot may retire a write against it"
	);
	assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, uuid);
}

/// A move's uuid was necessarily announced earlier, when the item was created. Only an
/// announcement made after the move itself proves the cache applied the move.
#[test]
fn an_announcement_predating_a_move_does_not_retire_it() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();

	observations.note_uuids([uuid]);
	pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

	let baseline = HashMap::from([(
		"b.txt".to_string(),
		written_at("b.txt", uuid, NodeKind::File, Some(hash(1))),
	)]);
	let mut remote = node_at("a.txt", uuid);
	pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1,
		"the snapshot still shows the pre-move path and nothing new has been announced"
	);

	observations.note_uuids([uuid]);
	let mut remote = node_at("a.txt", uuid);
	pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		0,
		"an announcement made after the move retires it"
	);
}

/// The pass moved the file on; the cache still lists it where it was. The view has to show it
/// where we put it, or the pass reads the move as a remote-side delete plus a local create.
#[test]
fn a_pending_move_is_folded_to_its_destination() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

	let baseline = HashMap::from([(
		"b.txt".to_string(),
		written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
	)]);
	let mut remote = node_at("a.txt", uuid);
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);

	assert!(
		!remote.whole().holds("a.txt"),
		"the pre-move path is vacated"
	);
	assert_eq!(remote.whole().at("b.txt").unwrap().remote_uuid, uuid);
	let local = HashMap::from([(
		"b.txt".to_string(),
		LocalNode {
			rel_path: "b.txt".to_string(),
			kind: NodeKind::File,
			size: 5,
			mtime_millis: 111,
			content_hash: Some(hash(3)),
		},
	)]);
	assert!(
		plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local,
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole
		)
		.actions
		.is_empty(),
		"both sides agree once the move is folded in: the pass has nothing to do"
	);
}

/// The destination was taken while our move was in flight (another client uploaded over it,
/// and the cache learned of that before it learned of the move). We cannot claim `b.txt` — but
/// we know for certain the file is no longer at `a.txt`, and leaving it there hands the
/// reconciler an untracked remote item to trash: the very file we just moved.
#[test]
fn a_move_whose_destination_was_taken_still_vacates_the_pre_move_path() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (uuid, foreign) = (Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, uuid, moved("a.txt", "b.txt"));

	let baseline = HashMap::from([(
		"b.txt".to_string(),
		written_at("b.txt", uuid, NodeKind::File, Some(hash(3))),
	)]);
	let mut remote = node_at("a.txt", uuid);
	remote.extend(node_at("b.txt", foreign).into_whole());
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);

	assert!(
		!remote.whole().holds("a.txt"),
		"the pre-move path is vacated even though the destination is somebody else's"
	);
	assert_eq!(
		remote.whole().at("b.txt").unwrap().remote_uuid,
		foreign,
		"and the destination is left showing what really sits there"
	);
	let local = HashMap::from([(
		"b.txt".to_string(),
		LocalNode {
			rel_path: "b.txt".to_string(),
			kind: NodeKind::File,
			size: 5,
			mtime_millis: 111,
			content_hash: Some(hash(3)),
		},
	)]);
	let actions = plan::reconcile(
		SyncMode::LocalToRemote,
		&tree(&baseline),
		&local,
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	)
	.actions;
	assert!(
		!actions
			.iter()
			.any(|action| describe(action).contains("\"a.txt\"")),
		"the file we moved is not trashed at the path we moved it off: {:?}",
		actions.iter().map(describe).collect::<Vec<_>>()
	);
}

/// A trash the cache has not applied reads as an untracked remote item with nothing local —
/// a deletion to make all over again. Folding takes it out of the view instead, subtree and
/// all, since the server trashes a directory whole.
#[test]
fn a_pending_trash_takes_the_item_out_of_the_view() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (dir, child) = (Uuid::new_v4(), Uuid::new_v4());
	pending.record(
		&observations,
		PAIR,
		dir,
		PendingKind::Trashed {
			path: "d".to_string(),
		},
	);

	let mut remote = Side::from(HashMap::from([
		(
			"d".to_string(),
			RemoteNode {
				rel_path: "d".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
				stable_uuid: None,
				content_hash: None,
				size: 0,
				modified_millis: 0,
			},
		),
		(
			"d/x.txt".to_string(),
			RemoteNode {
				rel_path: "d/x.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: child,
				stable_uuid: Some(StableUuid::new_for_test(child)),
				content_hash: Some(hash(4)),
				size: 5,
				modified_millis: 0,
			},
		),
	]));
	// Trashing dropped both baseline rows, and the local side is gone too.
	let (baseline, local) = (HashMap::new(), HashMap::new());
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);
	assert!(
		remote.whole().is_empty(),
		"the trashed directory takes its subtree with it"
	);
	assert!(
		holds.trashed.contains(&dir),
		"the deletion of that uuid is still suppressed, whatever a view shows"
	);
	assert!(
		plan::reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local,
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"nothing is trashed a second time"
	);
}

/// The upload replaced `old` at the path: the cache is simply behind on us, so the view has to
/// show OUR item there — the one the next edit is measured against.
#[test]
fn a_pending_create_is_folded_over_the_uuid_it_replaced() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

	let baseline = written_row(new, hash(1));
	let mut remote = node_at("a.txt", old);
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);
	assert_eq!(
		remote.whole().at("a.txt").unwrap().remote_uuid,
		new,
		"the pre-write occupant is what a lagging cache shows; ours supersedes it"
	);
	assert_eq!(
		remote.whole().len(),
		1,
		"and does not leave the old one behind"
	);

	// A further local edit is then pushed rather than left alone.
	let actions = plan::reconcile(
		SyncMode::LocalToRemote,
		&tree(&baseline),
		&local_map(hash(2)),
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	)
	.actions;
	assert_eq!(
		actions.iter().map(describe).collect::<Vec<_>>(),
		vec!["upload file \"a.txt\"".to_string()],
	);
}

/// Two uploads to one path inside the window: the second's record is retired by the very lag
/// the fold exists for (the cache still shows the version the FIRST one replaced, which reads
/// as foreign to it), so the view has to be corrected from the surviving record. Left to the
/// snapshot, a two-way pass reads both sides as changed and raises a conflict nobody caused.
#[test]
fn two_writes_to_one_path_inside_the_window_show_the_latest() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (old, first, second) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, first, created("a.txt", Some(old)));
	pending.record(&observations, PAIR, second, created("a.txt", Some(first)));

	let baseline = written_row(second, hash(2));
	let mut remote = node_at("a.txt", old);
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);
	assert_eq!(
		remote.whole().at("a.txt").unwrap().remote_uuid,
		second,
		"the latest write is what the path holds"
	);
	assert!(
		plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local_map(hash(2)),
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"and both sides agree, so there is no conflict to raise"
	);
}

/// The other side of the same-round race, on the client that WON it. Its upload is the remote
/// head, but the cache applied the version that upload superseded and has not announced ours
/// yet — so the path shows a foreign uuid of our own lineage. Reading that as somebody else's
/// write left the winner reconciling against the version it had already replaced: a foreign
/// edit over an unconfirmed push, i.e. a persisted conflict on the client whose copy IS the
/// head. Our write has to stand in the view instead.
#[test]
fn the_version_our_own_upload_superseded_does_not_read_as_a_foreign_write() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));

	// The row our upload left: our bytes under our uuid, the file's lineage, and the shared
	// base still the last content both sides were known to hold.
	let lineage = StableUuid::new_for_test(base);
	let mut baseline = written_row(ours, hash(2));
	let row = baseline.get_mut("a.txt").unwrap();
	row.remote_stable_uuid = Some(lineage);
	row.agreed_hash = Some(hash(0));

	// What the lagging cache shows: the other client's version of the SAME file, which landed
	// first and which our upload went on top of.
	let mut remote = node_at("a.txt", theirs);
	let mut node = remote.whole().at("a.txt").unwrap().into_owned();
	node.stable_uuid = Some(lineage);
	node.content_hash = Some(hash(1));
	remote.insert("a.txt".to_string(), node);

	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1,
		"the cache has announced nothing of ours, so what it shows is the version we replaced"
	);
	assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, ours);
	assert!(
		plan::reconcile(
			SyncMode::TwoWay,
			&tree(&baseline),
			&local_map(hash(2)),
			&remote.whole(),
			&holds,
			plan::PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"the client holding the head must not conflict with the version it superseded"
	);
}

/// The state above is also the ONE the record cannot settle by itself — a version of the same
/// file that is neither ours nor the one we replaced reads the same whether the cache is
/// behind on our write or skipped straight past it. That is what the server is asked about;
/// nothing else is.
#[test]
fn only_a_same_lineage_stranger_at_a_pending_creates_path_is_worth_a_lookup() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));
	let lineage = StableUuid::new_for_test(base);
	let mut baseline = written_row(ours, hash(2));
	baseline.get_mut("a.txt").unwrap().remote_stable_uuid = Some(lineage);

	let same_lineage = |uuid: Uuid| {
		let mut nodes = node_at("a.txt", uuid);
		let mut node = nodes.whole().at("a.txt").unwrap().into_owned();
		node.stable_uuid = Some(lineage);
		nodes.insert("a.txt".to_string(), node);
		nodes
	};
	for settled in [
		// The cache has caught up to our own write.
		same_lineage(ours),
		// It is showing the version our upload superseded.
		same_lineage(base),
		// A different FILE has taken the path over — the fold refuses it outright.
		node_at("a.txt", Uuid::new_v4()),
		// Nothing at the path at all.
		Side::default(),
	] {
		assert!(
			pending
				.strangers(PAIR, &tree(&baseline), &settled.whole())
				.is_empty(),
			"the record answers for this state on its own"
		);
	}

	assert_eq!(
		pending.strangers(PAIR, &tree(&baseline), &same_lineage(theirs).whole()),
		vec![("a.txt".to_string(), ours, theirs)]
			.into_iter()
			.map(|(path, ours_uuid, stranger)| (ours, path, ours_uuid, stranger))
			.collect::<Vec<_>>()
	);
}

/// What the lookup's answer does: with the record retired, the stranger stands and the pass
/// reconciles against it instead of waiting out the grace window.
#[test]
fn retiring_a_superseded_create_lets_the_stranger_stand() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (base, ours, theirs) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, ours, created("a.txt", Some(base)));
	let lineage = StableUuid::new_for_test(base);
	let mut baseline = written_row(ours, hash(2));
	baseline.get_mut("a.txt").unwrap().remote_stable_uuid = Some(lineage);
	let mut remote = node_at("a.txt", theirs);
	let mut node = remote.whole().at("a.txt").unwrap().into_owned();
	node.stable_uuid = Some(lineage);
	remote.insert("a.txt".to_string(), node);

	pending.retire(ours);
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		0,
		"with the record gone there is nothing left to paint over the snapshot"
	);
	assert_eq!(remote.whole().at("a.txt").unwrap().remote_uuid, theirs);
}

/// A third uuid at the path is not our write lagging: somebody else wrote there after us, and
/// the reconciler has to see that immediately rather than wait out the grace window.
#[test]
fn a_foreign_uuid_at_the_path_is_never_painted_over() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (old, new) = (Uuid::new_v4(), Uuid::new_v4());
	pending.record(&observations, PAIR, new, created("a.txt", Some(old)));

	let foreign = Uuid::new_v4();
	let mut remote = node_at("a.txt", foreign);
	pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(
			PAIR,
			&tree(&written_row(new, hash(1))),
			&mut remote,
			&mut BTreeSet::new()
		),
		0,
		"a uuid that is neither ours nor the one we replaced is a foreign write"
	);
	assert_eq!(
		remote.whole().at("a.txt").unwrap().remote_uuid,
		foreign,
		"and it must stand, so the pass reconciles against it at once"
	);
}

/// A path held in conflict is re-reported every pass until the caller resolves it, and its
/// subtree stays suppressed — including when this engine has just written there.
#[test]
fn a_conflicted_path_is_re_reported_even_with_a_write_folded_at_it() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("d", None));

	let baseline = HashMap::from([(
		"d".to_string(),
		BaselineEntry {
			state: BaselineState::Conflicted,
			..written_at("d", uuid, NodeKind::Dir, None)
		},
	)]);
	// A new local file under the held path: an upload the pass must not attempt while the
	// name itself is unresolved.
	let local = HashMap::from([(
		"d/x.txt".to_string(),
		LocalNode {
			rel_path: "d/x.txt".to_string(),
			kind: NodeKind::File,
			size: 5,
			mtime_millis: 111,
			content_hash: Some(hash(2)),
		},
	)]);

	let mut remote = Side::default();
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		1
	);
	let actions = plan::reconcile(
		SyncMode::TwoWay,
		&tree(&baseline),
		&local,
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	)
	.actions;
	assert_eq!(
		actions.iter().map(describe).collect::<Vec<_>>(),
		vec!["conflict \"d\"".to_string()],
		"the held conflict is still reported, and nothing under it is acted on"
	);
}

/// Each pair enumerates its OWN remote subtree, so another pair's snapshot is silent about a
/// write it never covers. Reading that silence as evidence retired the record — and a retired
/// trash is trashed a second time by the owning pair's next pass.
#[test]
fn another_pairs_pass_leaves_a_write_it_cannot_see_alone() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (trashed, created) = (Uuid::new_v4(), Uuid::new_v4());
	pending.record(
		&observations,
		PAIR,
		trashed,
		PendingKind::Trashed {
			path: "t.txt".to_string(),
		},
	);
	pending.record(
		&observations,
		PAIR,
		created,
		PendingKind::Created {
			path: "note.txt".to_string(),
			replaced: None,
		},
	);
	// The other pair happens to hold a path of the same name — its own file, under its own
	// root — and never lists the trashed uuid at all.
	let foreign = node_at("note.txt", Uuid::new_v4());

	let holds = pending.settle(PAIR + 1, &observations.snapshot(), &foreign.whole());
	assert!(
		holds.trashed.is_empty(),
		"a pass holds nothing on behalf of another pair"
	);
	assert_eq!(
		pending.fold_into(
			PAIR + 1,
			&Baseline::default(),
			&mut copied(&foreign),
			&mut BTreeSet::new()
		),
		0,
		"nor does it fold another pair's writes into its own view"
	);

	let mut remote = node_at("gone.txt", trashed);
	let holds = pending.settle(PAIR, &observations.snapshot(), &remote.whole());
	assert_eq!(
		holds.trashed,
		HashSet::from([trashed]),
		"the owning pair's snapshot still shows the item untrashed"
	);
	let baseline = HashMap::from([(
		"note.txt".to_string(),
		written_at("note.txt", created, NodeKind::File, Some(hash(1))),
	)]);
	assert_eq!(
		pending.fold_into(PAIR, &tree(&baseline), &mut remote, &mut BTreeSet::new()),
		2,
		"and still has not caught up to either write"
	);
	assert!(!remote.whole().holds("gone.txt"), "the trash is folded out");
	assert_eq!(remote.whole().at("note.txt").unwrap().remote_uuid, created);
}

/// A pass indexes the snapshot only for writes of its own, and a pair that journalled none
/// indexes nothing. What it must NOT skip is the retirement that reads a record by itself: the
/// cache announcing a uuid retires that record whichever pair's pass happens to see it, or a
/// write nobody passes over again would be folded into the view for ever.
#[test]
fn a_pass_with_no_write_of_its_own_still_retires_an_announced_one() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("a.txt", None));
	observations.note_uuids([uuid]);

	let holds = pending.settle(
		PAIR + 1,
		&observations.snapshot(),
		&Side::<RemoteNode>::default().whole(),
	);

	assert!(
		holds.trashed.is_empty(),
		"a pass holds nothing on behalf of another pair"
	);
	assert!(
		pending.uuids().is_empty(),
		"the announcement retires the record even though the passing pair owns no write"
	);
}

/// The oldest surviving write's stamp bounds what still has to be remembered; with nothing
/// pending, nothing does.
#[test]
fn observations_are_pruned_to_the_oldest_live_write() {
	let observations = Observations::default();
	let pending = PendingWrites::default();
	observations.note_uuids([Uuid::new_v4(), Uuid::new_v4()]);

	let uuid = Uuid::new_v4();
	pending.record(&observations, PAIR, uuid, created("a.txt", None));
	observations.prune_before(pending.oldest_stamp());
	assert!(
		observations.snapshot().is_empty(),
		"observations older than every live write are unreachable"
	);

	observations.note_uuids([uuid]);
	pending.settle(
		PAIR,
		&observations.snapshot(),
		&Side::<RemoteNode>::default().whole(),
	);
	observations.prune_before(pending.oldest_stamp());
	assert!(
		observations.snapshot().is_empty(),
		"with no pending write left there is nothing to remember at all"
	);
}

/// A long pass journals one record per remote write, and no settle runs until the next pass.
/// A record past [`PENDING_CREATE_GRACE`] is one no settle would keep, so it goes as the
/// writes after it are journalled: however many writes a pass makes, it holds about one grace
/// window of them.
#[test]
fn a_long_pass_holds_about_one_grace_window_of_its_writes() {
	const WRITES: u32 = 20_000;
	let pending = PendingWrites::default();
	let start = Instant::now();
	let ((), peak) = alloc_meter::peak_bytes(|| {
		// One write a second, so all but the last few minutes' worth are past the grace by the
		// end — journalled the way the apply does it, retiring what has expired first.
		for i in 0..WRITES {
			let at = start + Duration::from_secs(u64::from(i));
			pending.expire_at(at);
			pending.insert(
				PAIR,
				Uuid::from_u128(u128::from(i)),
				created(&format!("dir/file-{i}.txt"), None),
				u64::from(i),
				at,
			);
		}
	});
	assert!(
		peak < 128 * 1024,
		"journalling {WRITES} writes held {peak} bytes at once"
	);
}

/// The pass after a first sync is what retires that sync's push records: it persists what the
/// announcements confirmed and forgets them, and with no pending write left it keeps none of
/// the announcements either. A map keeps its capacity when its entries go, so the journal has
/// to hand that back too, or an idle watch keeps a whole first sync's worth of it allocated.
#[test]
fn a_settled_push_journal_gives_its_memory_back() {
	const PUSHES: u32 = 20_000;
	let observations = Observations::default();
	// On some platforms the lock allocates on first use, which is not what is measured here.
	observations.stamp();
	let uuids: Vec<Uuid> = (0..PUSHES)
		.map(|i| Uuid::from_u128(u128::from(i)))
		.collect();
	let start = Instant::now();
	// One upload every 150 ms, each announced by the cache as it lands.
	let at = |i: u32| start + Duration::from_millis(150) * i;
	let ((), retained) = alloc_meter::net_bytes(|| {
		{
			let mut state = observations.state();
			for (i, uuid) in (0..PUSHES).zip(&uuids) {
				state.see(*uuid, at(i));
				state.watch_push(*uuid, Some(StableUuid::new_for_test(*uuid)), None, at(i));
			}
		}
		// The pass after it, once nothing it uploaded is recent enough to be asked about: every
		// push confirmed and persisted, and every pending write retired.
		let settled = at(PUSHES) + MINTED_TTL;
		observations.forget_pushes(&uuids, settled);
		observations.prune_before(None);
	});
	assert_eq!(
		retained, 0,
		"the journal kept bytes allocated for {PUSHES} pushes it has retired"
	);
}

/// A long pass is announced item by item, and an announcement is only ever consulted by a
/// write inside the grace window. Older ones go as new ones arrive, so however long the pass,
/// the journal holds about one grace window of announcements.
#[test]
fn a_long_stream_of_announcements_holds_about_one_grace_window_of_them() {
	const ANNOUNCED: u32 = 20_000;
	let observations = Observations::default();
	observations.stamp();
	let start = Instant::now();
	let ((), peak) = alloc_meter::peak_bytes(|| {
		let mut state = observations.state();
		for i in 0..ANNOUNCED {
			state.see(
				Uuid::from_u128(u128::from(i)),
				start + Duration::from_millis(150) * i,
			);
		}
	});
	// The window is 1,500 announcements of 40 bytes. The rest is the map: a swept one grows
	// over the slots its removals leave behind, so it briefly holds two tables at twice that.
	assert!(
		peak < 320 * 1024,
		"{ANNOUNCED} announcements held {peak} bytes at once"
	);
}

/// A remote view of `nodes` files, `dir/file-{i}.txt` under uuid `i`.
fn wide_view(nodes: u32) -> Side<RemoteNode> {
	(0..nodes)
		.map(|i| {
			let path = format!("dir/file-{i}.txt");
			let node = RemoteNode {
				rel_path: path.clone(),
				kind: NodeKind::File,
				remote_uuid: Uuid::from_u128(u128::from(i)),
				stable_uuid: None,
				content_hash: None,
				size: 0,
				modified_millis: 0,
			};
			(path, node)
		})
		.collect()
}

/// The pass after a big push settles a handful of records against the whole view. Only the
/// uuids those records name are worth finding in it, so that is all the settle keeps.
#[test]
fn settling_a_few_writes_against_a_wide_view_keeps_only_their_uuids() {
	const NODES: u32 = 100_000;
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (listed, moved_away, trashed, unlisted) = (
		Uuid::from_u128(7),
		Uuid::from_u128(8),
		Uuid::from_u128(9),
		Uuid::from_u128(u128::from(NODES) + 1),
	);
	pending.record(&observations, PAIR, listed, created("dir/file-7.txt", None));
	pending.record(
		&observations,
		PAIR,
		moved_away,
		moved("dir/file-8.txt", "elsewhere.txt"),
	);
	pending.record(
		&observations,
		PAIR,
		trashed,
		PendingKind::Trashed {
			path: "dir/file-9.txt".to_string(),
		},
	);
	pending.record(&observations, PAIR, unlisted, created("new.txt", None));
	let remote = wide_view(NODES);
	let observed = observations.snapshot();

	let (holds, peak) =
		alloc_meter::peak_bytes(|| pending.settle(PAIR, &observed, &remote.whole()));

	assert_eq!(
		pending.uuids(),
		HashSet::from([moved_away, trashed, unlisted]),
		"the create the view lists is settled; the cache is behind on the other three"
	);
	assert_eq!(holds.trashed, HashSet::from([trashed]));
	assert!(
		peak < 4 * 1024,
		"settling 4 writes against {NODES} nodes held {peak} bytes at once"
	);
}

/// A record whose item a foreign rename put on a path it does not name is looked up by uuid,
/// over the whole view. Only the uuids the journal names are worth indexing for that.
#[test]
fn folding_a_write_its_paths_miss_indexes_only_the_journalled_uuids() {
	const NODES: u32 = 100_000;
	let observations = Observations::default();
	let pending = PendingWrites::default();
	// We trashed it at `gone.txt`; the cache lists it at the path it was renamed to.
	let trashed = Uuid::from_u128(9);
	pending.record(
		&observations,
		PAIR,
		trashed,
		PendingKind::Trashed {
			path: "gone.txt".to_string(),
		},
	);
	let mut remote = wide_view(NODES);
	let baseline = Baseline::default();
	let mut changed = BTreeSet::new();

	let (folded, peak) =
		alloc_meter::peak_bytes(|| pending.fold_into(PAIR, &baseline, &mut remote, &mut changed));

	assert_eq!(folded, 1);
	assert!(
		!remote.whole().holds("dir/file-9.txt"),
		"the trashed item is taken out wherever the view holds it"
	);
	assert_eq!(remote.whole().len(), 99_999);
	assert!(
		peak < 4 * 1024,
		"folding 1 write into {NODES} nodes held {peak} bytes at once"
	);
}

/// [`wide_view`], as a whole pass reads it: staged, under a `dir` directory of its own; with the
/// directory its stage is in.
fn staged_wide_view(nodes: u32) -> (tempfile::TempDir, Side<RemoteNode>) {
	let (root, dir) = (Uuid::from_u128(1 << 64), Uuid::from_u128((1 << 64) + 1));
	let item = |uuid, parent, name: String| crate::cache::RemoteItem {
		uuid,
		parent,
		name,
		stable_uuid: None,
		hash: None,
		size: 0,
		modified_millis: 0,
	};
	let items =
		std::iter::once((true, item(dir, root, "dir".to_owned()))).chain((0..nodes).map(|i| {
			let uuid = Uuid::from_u128(u128::from(i));
			(false, item(uuid, dir, format!("file-{i}.txt")))
		}));
	let (stage_dir, view) = super::stage::staged_view(root, PairRoot::Directory, items, &[]);
	(stage_dir, view.nodes)
}

/// [`settling_a_few_writes_against_a_wide_view_keeps_only_their_uuids`] over a staged view,
/// which seeks the records' uuids in its index instead of walking the tree.
#[test]
fn settling_a_few_writes_against_a_staged_view_seeks_only_their_uuids() {
	const NODES: u32 = 100_000;
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let (listed, moved_away, trashed, unlisted) = (
		Uuid::from_u128(7),
		Uuid::from_u128(8),
		Uuid::from_u128(9),
		Uuid::from_u128(u128::from(NODES) + 1),
	);
	pending.record(&observations, PAIR, listed, created("dir/file-7.txt", None));
	pending.record(
		&observations,
		PAIR,
		moved_away,
		moved("dir/file-8.txt", "elsewhere.txt"),
	);
	pending.record(
		&observations,
		PAIR,
		trashed,
		PendingKind::Trashed {
			path: "dir/file-9.txt".to_string(),
		},
	);
	pending.record(&observations, PAIR, unlisted, created("new.txt", None));
	let (_dir, remote) = staged_wide_view(NODES);
	let observed = observations.snapshot();

	let (holds, peak) =
		alloc_meter::peak_bytes(|| pending.settle(PAIR, &observed, &remote.whole()));

	assert_eq!(
		pending.uuids(),
		HashSet::from([moved_away, trashed, unlisted]),
		"the create the view lists is settled; the cache is behind on the other three"
	);
	assert_eq!(holds.trashed, HashSet::from([trashed]));
	assert!(
		peak < 4 * 1024,
		"settling 4 writes against {NODES} staged nodes held {peak} bytes at once"
	);
}

/// [`folding_a_write_its_paths_miss_indexes_only_the_journalled_uuids`] over a staged view: the
/// item a foreign rename moved is found by its uuid, and taken out where the view holds it.
#[test]
fn folding_a_write_its_paths_miss_over_a_staged_view_seeks_its_uuid() {
	const NODES: u32 = 100_000;
	let observations = Observations::default();
	let pending = PendingWrites::default();
	let trashed = Uuid::from_u128(9);
	pending.record(
		&observations,
		PAIR,
		trashed,
		PendingKind::Trashed {
			path: "gone.txt".to_string(),
		},
	);
	let (_dir, mut remote) = staged_wide_view(NODES);
	let baseline = Baseline::default();
	let mut changed = BTreeSet::new();

	let (folded, peak) =
		alloc_meter::peak_bytes(|| pending.fold_into(PAIR, &baseline, &mut remote, &mut changed));

	assert_eq!(folded, 1);
	assert!(
		!remote.whole().holds("dir/file-9.txt"),
		"the trashed item is taken out wherever the view holds it"
	);
	assert_eq!(
		remote.whole().len(),
		100_000,
		"the directory and 99,999 files"
	);
	assert!(
		peak < 4 * 1024,
		"folding 1 write into {NODES} staged nodes held {peak} bytes at once"
	);
}

/// Comfortably PAST the confirmation window, and comfortably INSIDE it. Both are expressed in
/// terms of [`CONFIRM_TENURE`] so retuning the constant cannot leave a fixture on the wrong side
/// of it while the test still passes for the wrong reason.
fn past_window() -> Duration {
	CONFIRM_TENURE * 2
}

fn inside_window() -> Duration {
	CONFIRM_TENURE / 2
}

/// One push of ours, announced `announced_ago` before "now" and superseded (if at all) that
/// long after the announcement.
fn tenure(
	announced_ago: Option<Duration>,
	superseded_after: Option<Duration>,
) -> (PushTenure, Instant) {
	// Built forwards from a real `Instant`, never backwards: subtracting from `Instant::now()`
	// underflows on a machine that has been up for less than the offset.
	let announced = Instant::now();
	let now = announced + announced_ago.unwrap_or_default();
	(
		PushTenure {
			lineage: None,
			recorded: announced,
			announced: announced_ago.map(|_| announced),
			superseded: superseded_after.map(|after| announced + after),
		},
		now,
	)
}

/// The pass's drive-lock ceiling earns its place only by staying inside the confirmation window:
/// a pass that missed another client's release has to poll again, take the lock and upload while
/// the round it queued behind is still a race by the engine's own measure. Two sleeps' worth of
/// room is the whole of it — there is no third poll left inside the window.
#[test]
fn the_pass_lock_ceiling_polls_again_inside_the_confirmation_window() {
	assert!(
		PASS_LOCK_MAX_SLEEP * 2 <= CONFIRM_TENURE,
		"a missed release costs up to {PASS_LOCK_MAX_SLEEP:?}, which has to leave room inside \
			 CONFIRM_TENURE ({CONFIRM_TENURE:?}) for the round it lets through"
	);
	assert!(
		PASS_LOCK_MAX_SLEEP < MAX_SLEEP_TIME_DEFAULT,
		"the point of the engine's own ceiling is that it is shorter than the shared default \
			 ({MAX_SLEEP_TIME_DEFAULT:?}), which is past the window on its own"
	);
}

/// A pass that lets the drive lock go has to stay away from it long enough for a client on the
/// default ladder to poll once, or the release frees nothing: the pass would take the lock back
/// before anyone else asked.
#[test]
fn a_released_drive_lock_stays_free_past_one_default_poll() {
	assert!(
		PASS_LOCK_YIELD > MAX_SLEEP_TIME_DEFAULT,
		"a pass asking again after {PASS_LOCK_YIELD:?} can beat a client sleeping out \
			 {MAX_SLEEP_TIME_DEFAULT:?} every time"
	);
	assert!(
		PASS_LOCK_MAX_HOLD > PASS_LOCK_YIELD,
		"a pass that stays away longer than it holds the lock spends most of a large push idle"
	);
}

#[test]
fn a_push_that_stood_as_the_head_long_enough_is_confirmed() {
	let (push, now) = tenure(Some(past_window()), None);
	assert_eq!(push.verdict(now), PushVerdict::Confirmed);
}

#[test]
fn a_push_superseded_inside_the_window_is_not_confirmed() {
	let (push, now) = tenure(Some(past_window()), Some(inside_window()));
	assert_eq!(
		push.verdict(now),
		PushVerdict::Unconfirmed,
		"another client was editing at the same time; its version is not an edit made after ours"
	);
}

#[test]
fn a_push_superseded_after_the_window_is_still_confirmed() {
	let (push, now) = tenure(Some(past_window() * 2), Some(past_window()));
	assert_eq!(
		push.verdict(now),
		PushVerdict::Confirmed,
		"our version stood for the full window before anything landed on it"
	);
}

#[test]
fn a_push_that_is_still_ripening_is_not_confirmed_yet() {
	let (push, now) = tenure(Some(inside_window()), None);
	assert_eq!(push.verdict(now), PushVerdict::Unconfirmed);
}

#[test]
fn a_push_the_cache_never_announced_has_no_verdict() {
	let (push, now) = tenure(None, None);
	assert_eq!(
		push.verdict(now),
		PushVerdict::Unknown,
		"with no observation the server's version chain is the only evidence left"
	);
}

#[test]
fn watching_a_push_picks_up_an_announcement_that_beat_it() {
	let observations = Observations::default();
	let uuid = Uuid::new_v4();
	// The cache can commit our own upload's event before the upload call has returned.
	observations.note_uuids([uuid]);
	observations.watch_push(uuid, None, None);
	assert_eq!(
		observations.push_verdict(uuid, Instant::now() + past_window()),
		PushVerdict::Confirmed,
		"the announcement that arrived first still dates the push"
	);
}

/// A push of ours the cache announced long enough ago to have stood its tenure, and still the
/// remote head.
fn stood(observations: &Observations, uuid: Uuid) {
	let announced = Instant::now()
		.checked_sub(past_window())
		.expect("the monotonic clock has run longer than two confirmation windows");
	observations.state().insert_push(
		uuid,
		PushTenure {
			lineage: None,
			recorded: announced,
			announced: Some(announced),
			superseded: None,
		},
	);
}

/// Every rule a confirmation applies, read a page of rows at a time and staged past its window:
/// the snapshot listing our own version (rule (c), a whole pass's only), the cache's
/// announcements dating a push, and no evidence at all. Then what a real pass makes of it: the
/// rows the store holds once they are persisted, and the push records of exactly those rows
/// retired.
#[tokio::test]
async fn a_confirmation_pages_its_rows_stages_them_past_a_window_and_persists_them() {
	const EACH: u128 = 4;
	let path = std::env::temp_dir().join(format!("filen_sync_confirm_pages_{}.db", Uuid::new_v4()));
	let engine = SyncEngine::open(Arc::new(offline_client()), path.clone())
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
		.unwrap();
	let store = engine.pair_store(pair).await.unwrap();
	let mut remote = Side::<RemoteNode>::default();
	let mut rows = Vec::new();
	// Listed: the snapshot holds our version at the row's path.
	// Stood: a foreign version is there, but the cache dates ours past its tenure.
	// Ripening: the same, but ours has not stood long enough yet.
	// Silent: nothing listed, nothing announced.
	for (kind, at) in [("listed", 0), ("stood", 1), ("ripening", 2), ("silent", 3)] {
		for n in 0..EACH {
			let uuid = Uuid::from_u128(at * 100 + n);
			let rel = format!("{kind}/{n}.txt");
			rows.push(written_at(&rel, uuid, NodeKind::File, Some(hash(1))));
			match kind {
				"listed" => {
					remote.insert(rel.clone(), remote_file(&rel, uuid, hash(1), 5));
				}
				"stood" | "ripening" => {
					let foreign = Uuid::from_u128(1000 + at * 100 + n);
					remote.insert(rel.clone(), remote_file(&rel, foreign, hash(2), 5));
					if kind == "stood" {
						stood(&engine.observed, uuid);
					} else {
						// Announced just now, before the upload call returned.
						engine.observed.note_uuids([uuid]);
						engine.observed.watch_push(uuid, None, None);
					}
				}
				_ => {}
			}
		}
	}
	// Held in conflict, and listed: never confirmed.
	let conflicted = BaselineEntry {
		state: BaselineState::Conflicted,
		..written_at(
			"listed/held.txt",
			Uuid::from_u128(99),
			NodeKind::File,
			Some(hash(1)),
		)
	};
	remote.insert(
		conflicted.rel_path.clone(),
		remote_file(&conflicted.rel_path, Uuid::from_u128(99), hash(1), 5),
	);
	rows.push(conflicted);
	{
		let store = locked(&store);
		let changes: Vec<BaselineChange<'_>> = rows.iter().map(BaselineChange::Upsert).collect();
		store.apply_changes(pair, &changes).unwrap();
	}
	let baseline = locked(&store).baseline(pair).unwrap();
	let window = NonZeroUsize::new(3).unwrap();
	let agreed = |view: &Baseline, kind: &str| {
		(0..EACH)
			.map(|n| view.get(&format!("{kind}/{n}.txt")).unwrap().agreed_hash)
			.collect::<Vec<_>>()
	};

	// A change-scoped pass reads no rule (c): the listed rows wait for the next whole pass.
	let (derived, lent) = engine
		.confirm(
			pair,
			&baseline,
			Evidence::Derived(mem::take(&mut remote)),
			window,
		)
		.await
		.unwrap();
	lent.put_back(&mut remote);
	assert_eq!(derived.len(), 4, "the stood rows alone");

	let (confirmed, _) = engine
		.confirm(pair, &baseline, Evidence::Snapshot(remote), window)
		.await
		.unwrap();
	assert!(
		matches!(confirmed, Confirmed::Staged(_)),
		"{} rows past a window of 3 are held",
		confirmed.len()
	);
	assert_eq!(confirmed.len(), 8, "the listed rows and the stood ones");
	let mut view = baseline.clone();
	view.confirm(&confirmed);
	let confirmed_at = vec![Some(hash(1)); 4];
	assert_eq!(agreed(&view, "listed"), confirmed_at);
	assert_eq!(agreed(&view, "stood"), confirmed_at);
	assert_eq!(agreed(&view, "ripening"), vec![None; 4]);
	assert_eq!(agreed(&view, "silent"), vec![None; 4]);
	assert_eq!(view.get("listed/held.txt").unwrap().agreed_hash, None);
	assert_eq!(
		view.unconfirmed()
			.map(|row| row.rel_path)
			.filter(|rel| !rel.starts_with("ripening/") && !rel.starts_with("silent/"))
			.collect::<Vec<_>>(),
		Vec::<String>::new(),
		"every row it confirmed has left the unconfirmed set"
	);
	drop((view, baseline, derived));

	engine
		.persist_confirmed(pair, &store, confirmed)
		.await
		.unwrap();
	let persisted = locked(&store).baseline(pair).unwrap();
	assert_eq!(agreed(&persisted, "listed"), confirmed_at);
	assert_eq!(agreed(&persisted, "stood"), confirmed_at);
	assert_eq!(agreed(&persisted, "ripening"), vec![None; 4]);
	assert_eq!(persisted.unconfirmed().count(), 8);
	let later = Instant::now() + past_window();
	assert_eq!(
		engine.observed.push_verdict(Uuid::from_u128(100), later),
		PushVerdict::Unknown,
		"a persisted row's push record is retired"
	);
	assert_eq!(
		engine.observed.push_verdict(Uuid::from_u128(200), later),
		PushVerdict::Confirmed,
		"an unconfirmed row's record stays for the pass that confirms it"
	);
	drop((persisted, store, engine));
	std::fs::remove_file(&path).ok();
}

/// The pass after a whole-tree push finds every row unconfirmed and confirms them all. It holds a
/// window of them at a time: the rows it reads, and those it confirmed until they move into a
/// stage, where its view of the rows then reads their markers.
#[test]
fn confirming_twenty_thousand_pushes_holds_a_window_of_them() {
	const ROWS: u128 = 20_000;
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.unwrap();
	let path = std::env::temp_dir().join(format!("filen_sync_confirm_peak_{}.db", Uuid::new_v4()));
	let engine = runtime
		.block_on(SyncEngine::open(Arc::new(offline_client()), path.clone()))
		.unwrap();
	let mut remote = Side::<RemoteNode>::default();
	let rows: Vec<BaselineEntry> = (0..ROWS)
		.map(|n| {
			let uuid = Uuid::from_u128(n);
			let rel = format!("d{:03}/f{n:05}.txt", n % 100);
			remote.insert(rel.clone(), remote_file(&rel, uuid, hash(1), 5));
			written_at(&rel, uuid, NodeKind::File, Some(hash(1)))
		})
		.collect();
	let baseline = Baseline::from_rows(rows);
	// On some platforms the lock allocates on first use, which is not what is measured here.
	engine.observed.stamp();
	// On this thread, which is the thread the meter counts: a pass reads and stages the same
	// pages on blocking threads (see `SyncEngine::confirm`). Nothing here waits on the network —
	// no row has a foreign version to ask the server about. The view's markers are put in inside
	// it too: they are the other half of what is held. The snapshot comes back out, so its nodes
	// are not freed inside the measurement.
	let ((confirmed, view, snapshot), peak) = alloc_meter::peak_bytes(|| {
		let (confirmed, snapshot) = confirm_here(Confirming::new(
			baseline.clone(),
			Evidence::Snapshot(remote),
			Arc::clone(&engine.observed),
			|| engine.stage(PAIR),
			stage::WINDOW,
		))
		.unwrap();
		let mut view = baseline.clone();
		view.confirm(&confirmed);
		(confirmed, view, snapshot)
	});
	assert_eq!(confirmed.len(), 20_000);
	// A window is 1,024 rows of about 250 bytes, read and held once each: half a MiB, and the
	// view reads its markers out of the stage. Read and held whole, with a marker per row held
	// in a map, the same rows came to 18 MiB.
	assert!(
		peak < 1024 * 1024,
		"confirming {ROWS} pushes held {peak} bytes at once"
	);
	assert!(!view.any_unconfirmed());
	assert_eq!(
		view.get("d007/f00007.txt").unwrap().agreed_hash,
		Some(hash(1))
	);
	drop((view, baseline, confirmed, snapshot, engine));
	std::fs::remove_file(&path).ok();
}

#[test]
fn a_push_forgotten_after_a_pass_read_it_has_no_verdict_left() {
	let observations = Observations::default();
	let uuid = Uuid::new_v4();
	observations.note_uuids([uuid]);
	observations.watch_push(uuid, None, None);
	observations.forget_pushes(&[uuid], Instant::now());
	assert_eq!(
		observations.push_verdict(uuid, Instant::now() + past_window()),
		PushVerdict::Unknown
	);
}

/// Reading the verdicts is what `plan_pair` does too, and it persists nothing. If the read
/// retired the records, a dry run inside the confirmation window would leave the pass after it
/// with an unconfirmed row and no evidence left to confirm it — a false conflict against an
/// edit made long after ours.
#[test]
fn reading_the_announcements_leaves_them_for_the_pass_that_persists() {
	let observations = Observations::default();
	let uuid = Uuid::new_v4();
	observations.note_uuids([uuid]);
	observations.watch_push(uuid, None, None);
	let baseline = written_row(uuid, hash(1));
	let ripe = Instant::now() + past_window();

	let unconfirmed: Vec<BaselineEntry> = tree(&baseline).unconfirmed().collect();
	let read = || observed_confirmations(&observations, &unconfirmed, None, ripe).0;
	assert_eq!(read(), std::collections::HashSet::from([uuid]));
	assert_eq!(
		read(),
		std::collections::HashSet::from([uuid]),
		"a dry run that wrote nothing must not have consumed the evidence"
	);

	// The pass that persisted the row is the one that retires it.
	observations.forget_pushes(&[uuid], Instant::now());
	assert!(read().is_empty());
}

/// What the version chain cannot say: which versions came out of THIS engine. Two uploads a
/// second apart carry the same server stamp, so without this the previous one reads as a
/// stranger's edit that our push buried.
#[test]
fn a_version_this_engine_uploaded_is_remembered_as_its_own() {
	let observations = Observations::default();
	let (ours, theirs) = (Uuid::new_v4(), Uuid::new_v4());
	observations.watch_push(ours, None, None);

	assert!(observations.minted(ours));
	assert!(!observations.minted(theirs), "another client's version");

	// Confirming the push retires its tenure record, not the fact that we made it: the pass after
	// it still has to recognise the version as ours.
	observations.forget_pushes(&[ours], Instant::now());
	assert!(observations.minted(ours));

	// Past the window a pass reasons about, it goes.
	observations.forget_pushes(&[], Instant::now() + MINTED_TTL);
	assert!(!observations.minted(ours));
}

/// A push is superseded through the lineage index, so the index has to hold exactly the records
/// being watched: an entry that outlives its record would keep a retired push reachable, and a
/// record missing from it would never learn it was superseded — a push confirmed by a tenure it
/// did not have, which pulls another client's concurrent edit over ours without a word.
#[test]
fn another_version_of_a_file_we_pushed_supersedes_it_through_the_lineage_index() {
	let mut state = ObservationState::default();
	let lineage = StableUuid::new_for_test(Uuid::new_v4());
	let (ours, replaced) = (Uuid::new_v4(), Uuid::new_v4());
	let now = Instant::now();
	let watched = || PushTenure {
		lineage: Some(lineage),
		recorded: now,
		announced: Some(now),
		superseded: None,
	};
	state.insert_push(replaced, watched());
	state.insert_push(ours, watched());

	// The version our own upload replaced is not watched any more, so nothing about the file
	// speaks for it.
	state.drop_push(replaced);
	state.supersede_lineage(lineage, now + inside_window());
	let ripe = now + past_window();
	assert_eq!(
		state.pushes[&ours].verdict(ripe),
		PushVerdict::Unconfirmed,
		"another client's version landed on ours inside the window"
	);

	state.drop_push(ours);
	assert!(
		state.pushes_by_lineage.is_empty(),
		"the index outlived the records it points at"
	);
}

#[test]
fn a_re_push_of_the_same_path_retires_the_version_it_replaced() {
	let observations = Observations::default();
	let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
	observations.note_uuids([first]);
	observations.watch_push(first, None, None);
	observations.watch_push(second, None, Some(first));
	assert_eq!(
		observations.push_verdict(first, Instant::now() + past_window()),
		PushVerdict::Unknown,
		"the row moved on to the new version, so the old record answers for nothing"
	);
}

/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
/// Where the rows record each skipped item, as the engine resolves it off the store.
fn synced_for(baseline: &Baseline, skipped: &[plan::SkippedRemote]) -> SyncedPaths {
	let (uuids, lineages) = plan::skipped_ids(skipped);
	baseline.synced_paths(&uuids, &lineages)
}

fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
	Baseline::from_rows(rows.values().cloned())
}

/// A database already holding a table `t`, with `t`'s DDL.
fn db_with(table: &str) -> rusqlite::Connection {
	let conn = open_sqlite_in_memory().unwrap();
	conn.execute_batch(&format!("CREATE TABLE t ({table});"))
		.unwrap();
	conn
}

#[test]
fn a_full_disk_is_an_io_error_that_keeps_sqlites_answer() {
	let conn = db_with("v BLOB");
	// A file at its page cap answers a write as a full disk does, with `SQLITE_FULL`.
	conn.execute_batch("PRAGMA max_page_count = 2;").unwrap();
	let full = conn
		.execute("INSERT INTO t VALUES (zeroblob(1000000))", [])
		.unwrap_err();
	assert_eq!(
		full.sqlite_error_code(),
		Some(rusqlite::ErrorCode::DiskFull)
	);

	let error = db_error(full, "writing");
	assert_eq!(error.kind(), ErrorKind::IO);
	assert_eq!(
		error
			.downcast_ref::<rusqlite::Error>()
			.and_then(rusqlite::Error::sqlite_error_code),
		Some(rusqlite::ErrorCode::DiskFull)
	);
}

#[test]
fn an_extended_io_error_is_an_io_error() {
	let fsync = rusqlite::Error::SqliteFailure(
		rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_FSYNC),
		None,
	);
	assert_eq!(db_error(fsync, "committing").kind(), ErrorKind::IO);
}

#[test]
fn a_database_that_cannot_be_opened_is_an_io_error() {
	let dir = tempfile::tempdir().unwrap();
	let missing = open_sqlite(dir.path().join("missing").join("baseline.db")).unwrap_err();
	assert_eq!(
		missing.sqlite_error_code(),
		Some(rusqlite::ErrorCode::CannotOpen)
	);
	assert_eq!(db_error(missing, "opening").kind(), ErrorKind::IO);
}

#[test]
fn a_constraint_violation_stays_an_internal_error() {
	let conn = db_with("k INTEGER PRIMARY KEY");
	conn.execute("INSERT INTO t VALUES (1)", []).unwrap();
	let duplicate = conn.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
	assert_eq!(
		duplicate.sqlite_error_code(),
		Some(rusqlite::ErrorCode::ConstraintViolation)
	);
	assert_eq!(db_error(duplicate, "writing").kind(), ErrorKind::Internal);
}

#[test]
fn a_write_to_a_database_opened_read_only_is_an_io_error() {
	let dir = tempfile::tempdir().unwrap();
	let path = dir.path().join("baseline.db");
	open_sqlite(&path)
		.unwrap()
		.execute_batch("CREATE TABLE t (k INTEGER);")
		.unwrap();
	// How SQLite opens a file the file system will not let it write; the flag rather than a
	// chmod, so the test holds when run as root too.
	let conn = open_sqlite_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
	let refused = conn.execute("INSERT INTO t VALUES (1)", []).unwrap_err();
	assert_eq!(
		refused.sqlite_error().map(|failure| failure.extended_code),
		Some(rusqlite::ffi::SQLITE_READONLY)
	);
	assert_eq!(db_error(refused, "writing").kind(), ErrorKind::IO);
}

#[test]
fn a_database_file_moved_away_while_open_is_an_io_error() {
	let moved = rusqlite::Error::SqliteFailure(
		rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_READONLY_DBMOVED),
		None,
	);
	assert_eq!(db_error(moved, "writing").kind(), ErrorKind::IO);
}

/// The remote root's `.filenignore` in [`dry_run_over_a_remote_rule_file`]'s cache.
const REMOTE_RULE_FILE: Uuid = Uuid::from_u128(3);

/// A dry run of a pair in `mode` over a local tree holding `a.tmp`, `b.txt` and `local` (paths
/// and contents), whose remote root holds a `.filenignore` reading `*.tmp` and nothing else,
/// with `rows` for its baseline. Offline: the remote is the cache's rows, and the rule file's
/// body is handed over as the one the pair's last pass read, so no server is asked for it.
/// Answers the paths the pass ignores, each with its deciding line, and how many stage numbers
/// the read took: the snapshot's, the plan's, one per walk of the tree, and the confirmation's
/// where a row awaits one.
async fn dry_run_over_a_remote_rule_file(
	mode: SyncMode,
	local: &[(&str, &str)],
	rows: &[BaselineEntry],
) -> (Vec<(String, String)>, u64) {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("local");
	std::fs::create_dir(&root).unwrap();
	for (path, contents) in [("a.tmp", "scratch"), ("b.txt", "kept")]
		.iter()
		.chain(local)
	{
		std::fs::write(root.join(path), contents).unwrap();
	}
	let account = Uuid::from_u128(1);
	let remote_root = Uuid::from_u128(2);
	let cache = dir.path().join("cache.db");
	let mut state = CacheState::new_on_path(&cache, account);
	state
		.upsert_dirs([&cacheable_dir(remote_root, account, "pair")].into_iter())
		.unwrap();
	state
		.upsert_files(
			[&cacheable_file(
				REMOTE_RULE_FILE,
				remote_root,
				".filenignore",
			)]
			.into_iter(),
		)
		.unwrap();
	let client = Arc::new(offline_client());
	client.configure_cache(cache, |_| {}).await.unwrap();
	let engine = SyncEngine::open(client, dir.path().join("sync.db"))
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(root.to_str().unwrap(), remote_root, mode)
		.unwrap();
	let store = engine.pair_store(pair).await.unwrap();
	let changes: Vec<BaselineChange<'_>> = rows.iter().map(BaselineChange::Upsert).collect();
	locked(&store).apply_changes(pair, &changes).unwrap();
	engine.remote_rule_bodies.lock().await.insert(
		pair,
		HashMap::from([(REMOTE_RULE_FILE, Arc::from("*.tmp\n"))]),
	);

	let before = engine.stages.load(Ordering::Relaxed);
	let outcome = engine.plan_pair(pair).await.unwrap();
	let stages = engine.stages.load(Ordering::Relaxed) - before;
	drop((engine, store, state));
	let ignored = outcome
		.ignored
		.into_iter()
		.map(|ignored| (ignored.rel_path, ignored.pattern))
		.collect();
	(ignored, stages)
}

/// A two-way pair whose only `.filenignore` is the remote's walks its tree with that file's
/// rules: the walk a pushing pair starts before the remote is read matched with the user's
/// rules alone, so it is given up and the tree walked again.
#[tokio::test]
async fn a_two_way_walk_matches_with_a_rule_file_only_the_remote_holds() {
	let (ignored, stages) = dry_run_over_a_remote_rule_file(SyncMode::TwoWay, &[], &[]).await;
	assert_eq!(ignored, vec![("a.tmp".to_owned(), "*.tmp".to_owned())]);
	assert_eq!(stages, 4, "the snapshot, the plan and two walks");
}

/// So does a pair that only pushes, whose synced `.filenignore` the disk has lost: its rules
/// govern until that loss propagates.
#[tokio::test]
async fn a_push_only_walk_matches_with_a_synced_rule_file_the_disk_lost() {
	let synced = synced_file(
		".filenignore",
		REMOTE_RULE_FILE,
		Blake3Hash::from([1; 32]),
		6,
	);
	let (ignored, stages) =
		dry_run_over_a_remote_rule_file(SyncMode::LocalToRemote, &[], &[synced]).await;
	assert_eq!(ignored, vec![("a.tmp".to_owned(), "*.tmp".to_owned())]);
	// The row is a push the pass has yet to confirm, and the confirmation takes a number for
	// the stage it would make past a window.
	assert_eq!(
		stages, 5,
		"the confirmation, the snapshot, the plan and two walks"
	);
}

/// A pair whose `.filenignore` is on disk reads no remote copy, so the walk started before the
/// remote was read stands: the tree is walked once, with the rules on disk.
#[tokio::test]
async fn a_walk_with_its_rule_file_on_disk_walks_the_tree_once() {
	let (ignored, stages) =
		dry_run_over_a_remote_rule_file(SyncMode::TwoWay, &[(".filenignore", "b.*\n")], &[]).await;
	assert_eq!(ignored, vec![("b.txt".to_owned(), "b.*".to_owned())]);
	assert_eq!(stages, 3, "the snapshot, the plan and one walk");
}

/// The stamp a hashless remote file and its local copy share to the millisecond.
const SEED_STAMP: i64 = 1_700_000_000_123;

/// A hashed local file of 5 bytes, stamped [`SEED_STAMP`].
fn seed_local(rel: &str) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		size: 5,
		mtime_millis: SEED_STAMP,
		content_hash: Some(hash(7)),
	}
}

/// What another client uploaded at `rel` with no BLAKE3: the size and stamp of [`seed_local`].
fn seed_remote(rel: &str, uuid: u128) -> RemoteNode {
	RemoteNode {
		content_hash: None,
		size: 5,
		modified_millis: SEED_STAMP,
		..remote_file(rel, Uuid::from_u128(uuid), hash(7), 5)
	}
}

/// [`prepared`] over `rows` as a store of its own holds them, read as a pass reads its rows, so its
/// seeding stage has somewhere to write; with that store.
fn prepared_on_store(
	mode: SyncMode,
	rows: Vec<BaselineEntry>,
	local: Vec<LocalNode>,
	remote: Vec<RemoteNode>,
) -> (BaselineStore, Prepared) {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store.create_pair("/local", Uuid::nil(), mode).unwrap();
	for row in &rows {
		store.upsert_entry(pair, row).unwrap();
	}
	let mut prep = prepared(
		mode,
		rows.into_iter()
			.map(|row| (row.rel_path.clone(), row))
			.collect(),
		local,
		view(remote, Vec::new(), &[]),
		HashMap::new(),
	);
	prep.record.id = pair;
	prep.baseline = store.baseline(pair).unwrap();
	prep.tracked_at_start = prep.baseline.len();
	(store, prep)
}

/// Every action and conflict a screened pass plans at `path`.
fn planned_at(screened: &ResidentScreened, path: &str) -> Vec<String> {
	screened
		.decision
		.safe
		.iter()
		.chain(&screened.decision.held)
		.filter(|action| action.rel_path() == path)
		.map(|action| format!("{action:?}"))
		.chain(
			screened
				.conflicts
				.iter()
				.filter(|conflict| conflict.rel_path == path)
				.map(|conflict| format!("{conflict:?}")),
		)
		.collect()
}

/// A hashless remote file with the local copy's size and stamp is recorded as synced before the
/// pass plans, and then nothing is planned at it in any mode, where without the row every mode
/// plans a conflict or a transfer there. Seeded over a whole pass's staged sides, read off the
/// stage's index of hashless files; a second seeding finds nothing left to write.
#[test]
fn a_seeded_hashless_file_plans_nothing_in_every_mode() {
	for mode in MODES {
		let (store, mut unseeded) = prepared_on_store(
			mode,
			Vec::new(),
			vec![seed_local("a.bin")],
			vec![seed_remote("a.bin", 1)],
		);
		let control = reconcile_and_screen_resident(&unseeded, screen_state(&unseeded));
		assert!(
			!planned_at(&control, "a.bin").is_empty(),
			"{mode:?}: without a row the path is planned"
		);

		let (_stages, local, remote) =
			stage::staged_sides([seed_local("a.bin")], [seed_remote("a.bin", 1)], 2);
		unseeded.local_scan.nodes = local;
		unseeded.remote_view.nodes = remote;
		let mut prep = unseeded;
		let seeded = prep.seed_unhashed(&store).unwrap();
		assert_eq!(seeded.size_and_mtime, 1, "{mode:?}");
		assert!(prep.baseline.contains_key("a.bin"), "{mode:?}");
		let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
		assert_eq!(
			planned_at(&screened, "a.bin"),
			Vec::<String>::new(),
			"{mode:?}"
		);

		assert_eq!(
			prep.seed_unhashed(&store).unwrap(),
			SeedCounts::default(),
			"{mode:?}"
		);
		assert_eq!(store.entries(prep.record.id).unwrap().len(), 1, "{mode:?}");
	}
}

/// The rows a pass seeds are no history: the pass that writes its pair's first rows is still a
/// first sync, and the deletion it plans is still held for approval.
#[test]
fn seeding_a_first_sync_keeps_its_deletions_held() {
	let (store, mut prep) = prepared_on_store(
		SyncMode::LocalToRemote,
		Vec::new(),
		vec![seed_local("a.bin")],
		vec![
			seed_remote("a.bin", 1),
			remote_file("gone.bin", Uuid::from_u128(2), hash(3), 4),
		],
	);
	prep.record.delete_guard = DeleteGuard::default();
	assert_eq!(prep.seed_unhashed(&store).unwrap().total(), 1);
	assert!(!prep.baseline.is_empty());
	let state = screen_state(&prep);
	assert!(state.first_sync);
	assert_eq!(state.tracked, 0);
	let screened = reconcile_and_screen_resident(&prep, state);
	assert_eq!(
		screened.decision.reason,
		Some(GuardReason::FirstSyncWithDeletions { deletions: 1 })
	);
	assert_eq!(
		screened.decision.held,
		[SyncAction::TrashRemote {
			rel_path: "gone.bin".to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(2),
		}]
	);
}

/// The rows are read again once seeded, and the confirmation the pass made of a push of its own is
/// put back on them: the confirmed row still reads as agreed, which the store's rows alone do not
/// say until the pass persists it.
#[test]
fn the_confirmations_a_pass_made_survive_its_seeding() {
	let pushed = BaselineEntry {
		agreed_hash: None,
		..file_row("pushed.bin", Uuid::from_u128(5), hash(4))
	};
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		vec![pushed.clone()],
		vec![seed_local("a.bin"), local_file("pushed.bin", hash(4))],
		vec![
			seed_remote("a.bin", 1),
			remote_file("pushed.bin", Uuid::from_u128(5), hash(4), 4),
		],
	);
	let confirmed = Confirmed::Held(vec![BaselineEntry {
		agreed_hash: Some(hash(4)),
		..pushed
	}]);
	prep.baseline.confirm(&confirmed);
	prep.confirmed = confirmed;
	assert!(!prep.baseline.any_unconfirmed());

	assert_eq!(prep.seed_unhashed(&store).unwrap().total(), 1);
	assert!(prep.baseline.contains_key("a.bin"));
	assert_eq!(
		prep.baseline.get("pushed.bin").unwrap().agreed_hash,
		Some(hash(4))
	);
	assert!(!prep.baseline.any_unconfirmed());
	assert!(
		store.baseline(prep.record.id).unwrap().any_unconfirmed(),
		"the store's own rows still await the confirmation"
	);
}

/// A pass that will be refused plans nothing, and seeds nothing either.
#[test]
fn a_refused_pass_seeds_nothing() {
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		Vec::new(),
		vec![seed_local("a.bin")],
		vec![seed_remote("a.bin", 1)],
	);
	prep.local_scan.errors.push(ScanError::DuplicateName {
		rel_path: "x/Y".to_string(),
	});
	assert_eq!(prep.seed_unhashed(&store).unwrap(), SeedCounts::default());
	assert!(prep.baseline.is_empty());
	assert_eq!(store.entries(prep.record.id).unwrap(), []);
}

/// A change-scoped pass seeds the paths it decides, and leaves a rowless path it does not decide to
/// the pass that does.
#[test]
fn a_scoped_pass_seeds_only_the_paths_it_decides() {
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		vec![file_row("kept.bin", Uuid::from_u128(9), hash(1))],
		vec![seed_local("a.bin"), seed_local("b.bin")],
		vec![seed_remote("a.bin", 1), seed_remote("b.bin", 2)],
	);
	prep.read = PassRead::Scoped(BTreeSet::from(["a.bin".to_string()]));
	assert_eq!(prep.seed_unhashed(&store).unwrap().total(), 1);
	assert!(prep.baseline.contains_key("a.bin"));
	assert!(!prep.baseline.contains_key("b.bin"));
	assert_eq!(prep.tracked_at_start, 1);
	assert_eq!(screen_state(&prep).tracked, 1);
}

/// A pass seeds only a path it acts on: not one under a remote path the cache shows mid-transition,
/// one with a failure streak, one under a directory whose ignore rules it could not read, or a
/// remote item it cannot sync. Only the plain file beside them is recorded.
#[test]
fn a_pass_seeds_no_path_it_holds_or_blocks() {
	let paths = [
		"held/a.bin",
		"failing.bin",
		"blocked/c.bin",
		"unknown.bin",
		"plain.bin",
	];
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		Vec::new(),
		paths.iter().map(|path| seed_local(path)).collect(),
		paths
			.iter()
			.zip(1..)
			.map(|(path, uuid)| seed_remote(path, uuid))
			.collect(),
	);
	prep.holds.held_remote = BTreeSet::from(["held".to_string()]);
	prep.failures = HashMap::from([("failing.bin".to_string(), failure(9, 0))]);
	prep.facts.ignore_blocked = BTreeSet::from(["blocked".to_string()]);
	prep.facts.unknown_remote = BTreeMap::from([(
		"unknown.bin".to_string(),
		UnsyncableReason::RemoteUndecodable,
	)]);
	assert_eq!(prep.seed_unhashed(&store).unwrap().total(), 1);
	let seeded: Vec<String> = store
		.entries(prep.record.id)
		.unwrap()
		.into_iter()
		.map(|row| row.rel_path)
		.collect();
	assert_eq!(seeded, ["plain.bin"]);
}

/// A pass whose remote ignore rules for the whole pair could not be used acts on nothing, and seeds
/// nothing either.
#[test]
fn a_pass_with_the_whole_pair_ignore_blocked_seeds_nothing() {
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		Vec::new(),
		vec![seed_local("a.bin"), seed_local("dir/b.bin")],
		vec![seed_remote("a.bin", 1), seed_remote("dir/b.bin", 2)],
	);
	prep.remote_blocked = BTreeSet::from([String::new()]);
	assert_eq!(prep.seed_unhashed(&store).unwrap(), SeedCounts::default());
	assert_eq!(store.entries(prep.record.id).unwrap(), []);
}

/// Every transfer a screened pass plans, safe or held.
fn transfers(screened: &ResidentScreened) -> Vec<&SyncAction> {
	screened
		.decision
		.safe
		.iter()
		.chain(&screened.decision.held)
		.filter(|action| {
			matches!(
				action,
				SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. }
			)
		})
		.collect()
}

/// The path of every conflict a screened pass holds.
fn conflict_paths(screened: &ResidentScreened) -> Vec<&str> {
	screened
		.conflicts
		.iter()
		.map(|conflict| conflict.rel_path.as_str())
		.collect()
}

/// Whether two of `pair`'s rows in `store` record one remote uuid or one lineage.
fn rows_share_a_remote_item(store: &BaselineStore, pair: PairId) -> bool {
	let rows = store.entries(pair).unwrap();
	let uuids: HashSet<Uuid> = rows.iter().filter_map(|row| row.remote_uuid).collect();
	let lineages: HashSet<StableUuid> = rows
		.iter()
		.filter_map(|row| row.remote_stable_uuid)
		.collect();
	uuids.len() != rows.iter().filter(|row| row.remote_uuid.is_some()).count()
		|| lineages.len()
			!= rows
				.iter()
				.filter(|row| row.remote_stable_uuid.is_some())
				.count()
}

/// A directory renamed the same way on both sides, holding a hashless file: the file's row under
/// the old name still records its remote item, so nothing is seeded under the new name. The pass
/// plans what it planned before the seeding stage existed: no transfer, and the file under the new
/// name held as a conflict, the planner pairing no rename of a hashless file made on both sides.
#[test]
fn seeding_under_a_directory_renamed_on_both_sides_transfers_nothing() {
	let (dir, file) = (Uuid::from_u128(20), Uuid::from_u128(21));
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		vec![
			dir_row("old", dir),
			BaselineEntry {
				size: Some(5),
				local_mtime: Some(SEED_STAMP),
				remote_modified: Some(SEED_STAMP),
				..file_row("old/f.bin", file, hash(7))
			},
		],
		vec![local_dir("new"), seed_local("new/f.bin")],
		vec![remote_dir("new", dir), seed_remote("new/f.bin", 21)],
	);
	assert_eq!(prep.seed_unhashed(&store).unwrap(), SeedCounts::default());
	assert!(!rows_share_a_remote_item(&store, prep.record.id));
	prep.fold_dir_moves().unwrap();
	let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
	assert_eq!(transfers(&screened), Vec::<&SyncAction>::new());
	assert_eq!(conflict_paths(&screened), ["new/f.bin"]);
}

/// A hashless file renamed the same way on both sides, read by a change-scoped pass that decides
/// both names: the row at the old name still records the file's remote item, so the new name is
/// not seeded as a second row for it — the shape the directory-move fold refuses to read — and the
/// rename is left to the planner, which holds the new name as a conflict and transfers nothing, as
/// it did before the seeding stage existed.
#[test]
fn a_scoped_pass_does_not_seed_a_file_renamed_on_both_sides() {
	let file = Uuid::from_u128(21);
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		vec![BaselineEntry {
			size: Some(5),
			local_mtime: Some(SEED_STAMP),
			remote_modified: Some(SEED_STAMP),
			..file_row("a.bin", file, hash(7))
		}],
		vec![seed_local("b.bin")],
		vec![seed_remote("b.bin", 21)],
	);
	prep.read = PassRead::Scoped(BTreeSet::from(["a.bin".to_string(), "b.bin".to_string()]));
	assert_eq!(prep.seed_unhashed(&store).unwrap(), SeedCounts::default());
	assert!(!rows_share_a_remote_item(&store, prep.record.id));
	prep.fold_dir_moves().unwrap();
	let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
	assert_eq!(transfers(&screened), Vec::<&SyncAction>::new());
	assert_eq!(conflict_paths(&screened), ["b.bin"]);
}

/// A [`seed_local`] touched after its remote was stamped, so the stamps alone are no evidence.
fn touched_local(rel: &str) -> LocalNode {
	LocalNode {
		mtime_millis: SEED_STAMP + 5000,
		..seed_local(rel)
	}
}

/// Import a filen-sync state into `pair` that lists each of `files` — path and remote version — as
/// synced at the size and mtime of [`touched_local`].
fn import_touched(store: &BaselineStore, pair: PairId, files: &[(&str, u128)]) {
	store
		.replace_legacy_synced::<rusqlite::Error>(pair, |put| {
			for &(rel_path, uuid) in files {
				let raw = format!("/{rel_path}");
				let item = |side| LegacyItem {
					rel_path,
					raw: &raw,
					is_file: true,
					size: Some(5),
					side,
				};
				put(&item(LegacySide::Remote {
					uuid: Some(Uuid::from_u128(uuid)),
				}))?;
				put(&item(LegacySide::Local {
					mtime: Some(SEED_STAMP + 5000),
				}))?;
			}
			Ok(())
		})
		.unwrap();
}

/// A hashless remote file whose local copy was touched after the remote was stamped, so the stamps
/// alone are no evidence: planned in every mode without more, and recorded as synced, with nothing
/// planned at it, once the pair's imported filen-sync state lists that version at the local size
/// and mtime.
#[test]
fn a_hashless_file_the_imported_state_lists_plans_nothing_in_every_mode() {
	let touched = touched_local("a.bin");
	for mode in MODES {
		let (store, mut prep) = prepared_on_store(
			mode,
			Vec::new(),
			vec![touched.clone()],
			vec![seed_remote("a.bin", 1)],
		);
		assert_eq!(
			prep.seed_unhashed(&store).unwrap(),
			SeedCounts::default(),
			"{mode:?}"
		);
		assert!(
			!planned_at(
				&reconcile_and_screen_resident(&prep, screen_state(&prep)),
				"a.bin"
			)
			.is_empty(),
			"{mode:?}: without the import the path is planned"
		);

		import_touched(&store, prep.record.id, &[("a.bin", 1)]);
		let seeded = prep.seed_unhashed(&store).unwrap();
		assert_eq!(seeded.imported, 1, "{mode:?}");
		assert_eq!(seeded.total(), 1, "{mode:?}");
		let row = prep.baseline.get("a.bin").unwrap();
		assert_eq!(row.local_mtime, Some(SEED_STAMP + 5000), "{mode:?}");
		assert_eq!(row.remote_modified, Some(SEED_STAMP), "{mode:?}");
		let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
		assert_eq!(
			planned_at(&screened, "a.bin"),
			Vec::<String>::new(),
			"{mode:?}"
		);
	}
}

/// A change-scoped pass weighs the imported state at the paths it decides: the one it decides is
/// recorded as synced and planned nothing, and the one it does not decide is left without a row
/// for the pass that does.
#[test]
fn a_scoped_pass_seeds_from_the_imported_state_only_the_paths_it_decides() {
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		Vec::new(),
		vec![touched_local("a.bin"), touched_local("b.bin")],
		vec![seed_remote("a.bin", 1), seed_remote("b.bin", 2)],
	);
	import_touched(&store, prep.record.id, &[("a.bin", 1), ("b.bin", 2)]);
	prep.read = PassRead::Scoped(BTreeSet::from(["a.bin".to_string()]));
	assert_eq!(
		prep.seed_unhashed(&store).unwrap(),
		SeedCounts {
			imported: 1,
			empty: 0,
			size_and_mtime: 0,
		}
	);
	assert!(prep.baseline.contains_key("a.bin"));
	assert!(!prep.baseline.contains_key("b.bin"));
	assert_eq!(store.entry(prep.record.id, "b.bin").unwrap(), None);
	let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
	assert_eq!(planned_at(&screened, "a.bin"), Vec::<String>::new());
}

/// [`prepared_on_store`] over a whole read of `local` and `remote` as a pass stages them, so the
/// seeding stage reads its candidates off the stage's index of hashless files; with that store
/// and the directory holding the stages.
fn staged_on_store(
	mode: SyncMode,
	local: Vec<LocalNode>,
	remote: Vec<RemoteNode>,
) -> (BaselineStore, Prepared, tempfile::TempDir) {
	let (store, mut prep) = prepared_on_store(mode, Vec::new(), local.clone(), remote.clone());
	let (stages, local, remote) = stage::staged_sides(local, remote, 2);
	prep.local_scan.nodes = local;
	prep.remote_view.nodes = remote;
	(store, prep, stages)
}

/// A dry run weighs a hashless file by the evidence a pass weighs it by, and plans nothing at one a
/// pass would record as synced, in every mode — from the row the pass would write, held in the dry
/// run's own view of the rows: the store gains none, and the read is still a first sync.
#[test]
fn a_dry_run_plans_nothing_at_a_hashless_file_a_pass_would_seed_and_writes_no_row() {
	for mode in MODES {
		let (store, mut prep, _stages) = staged_on_store(
			mode,
			vec![seed_local("a.bin")],
			vec![seed_remote("a.bin", 1)],
		);
		assert_eq!(
			prep.preview_unhashed(&store).unwrap(),
			SeedCounts {
				imported: 0,
				empty: 0,
				size_and_mtime: 1,
			},
			"{mode:?}"
		);
		assert_eq!(
			prep.baseline.get("a.bin"),
			Some(seed::seeded_row(
				"a.bin",
				&seed_local("a.bin"),
				&seed_remote("a.bin", 1)
			)),
			"{mode:?}"
		);
		let state = screen_state(&prep);
		assert!(state.first_sync, "{mode:?}");
		assert_eq!(state.tracked, 0, "{mode:?}");
		assert_eq!(
			planned_at(&reconcile_and_screen_resident(&prep, state), "a.bin"),
			Vec::<String>::new(),
			"{mode:?}"
		);
		assert_eq!(store.entries(prep.record.id).unwrap(), [], "{mode:?}");
	}
}

/// A dry run weighs the pair's imported filen-sync state too: a hashless file whose stamps say
/// nothing, but which that state lists at the remote's version and the local size and mtime, is
/// planned nothing, and neither the store's rows nor the imported state change.
#[test]
fn a_dry_run_plans_nothing_at_a_hashless_file_the_imported_state_lists() {
	for mode in MODES {
		let (store, mut prep, _stages) = staged_on_store(
			mode,
			vec![touched_local("a.bin")],
			vec![seed_remote("a.bin", 1)],
		);
		import_touched(&store, prep.record.id, &[("a.bin", 1)]);
		assert_eq!(
			prep.preview_unhashed(&store).unwrap(),
			SeedCounts {
				imported: 1,
				empty: 0,
				size_and_mtime: 0,
			},
			"{mode:?}"
		);
		assert_eq!(
			planned_at(
				&reconcile_and_screen_resident(&prep, screen_state(&prep)),
				"a.bin"
			),
			Vec::<String>::new(),
			"{mode:?}"
		);
		assert_eq!(store.entries(prep.record.id).unwrap(), [], "{mode:?}");
		assert_eq!(
			store.legacy_synced_count(prep.record.id).unwrap(),
			1,
			"{mode:?}"
		);
	}
}

/// Where the evidence does not hold — the stamps differ and the imported state lists another
/// remote version — a dry run still plans the path, as the pass after it will: a conflict in
/// two-way, a transfer in the one-way modes.
#[test]
fn a_dry_run_still_plans_a_hashless_file_the_evidence_does_not_hold_for() {
	for mode in MODES {
		let (store, mut prep, _stages) = staged_on_store(
			mode,
			vec![touched_local("a.bin")],
			vec![seed_remote("a.bin", 1)],
		);
		import_touched(&store, prep.record.id, &[("a.bin", 2)]);
		assert_eq!(
			prep.preview_unhashed(&store).unwrap(),
			SeedCounts::default(),
			"{mode:?}"
		);
		assert!(!prep.baseline.contains_key("a.bin"), "{mode:?}");
		let screened = reconcile_and_screen_resident(&prep, screen_state(&prep));
		if mode == SyncMode::TwoWay {
			assert_eq!(conflict_paths(&screened), ["a.bin"]);
		} else {
			assert_eq!(transfers(&screened).len(), 1, "{mode:?}");
		}
	}
}

/// A dry run of a pass that will be refused weighs nothing, as the pass seeds nothing.
#[test]
fn a_refused_dry_run_weighs_nothing() {
	let (store, mut prep, _stages) = staged_on_store(
		SyncMode::TwoWay,
		vec![seed_local("a.bin")],
		vec![seed_remote("a.bin", 1)],
	);
	prep.local_scan.errors.push(ScanError::DuplicateName {
		rel_path: "x/Y".to_string(),
	});
	assert_eq!(
		prep.preview_unhashed(&store).unwrap(),
		SeedCounts::default()
	);
	assert!(prep.baseline.is_empty());
}

/// A dry run reads the rows it would seed over the confirmations the pass made, which stay on the
/// rows it already had.
#[test]
fn the_confirmations_a_dry_run_made_survive_its_seeding() {
	let pushed = BaselineEntry {
		agreed_hash: None,
		..file_row("pushed.bin", Uuid::from_u128(5), hash(4))
	};
	let (store, mut prep) = prepared_on_store(
		SyncMode::TwoWay,
		vec![pushed.clone()],
		vec![seed_local("a.bin"), local_file("pushed.bin", hash(4))],
		vec![
			seed_remote("a.bin", 1),
			remote_file("pushed.bin", Uuid::from_u128(5), hash(4), 4),
		],
	);
	prep.baseline.confirm(&Confirmed::Held(vec![BaselineEntry {
		agreed_hash: Some(hash(4)),
		..pushed
	}]));
	assert_eq!(prep.preview_unhashed(&store).unwrap().total(), 1);
	assert!(prep.baseline.contains_key("a.bin"));
	assert_eq!(
		prep.baseline.get("pushed.bin").unwrap().agreed_hash,
		Some(hash(4))
	);
	assert_eq!(prep.baseline.len(), 2);
	assert_eq!(store.entries(prep.record.id).unwrap().len(), 1);
}

/// A remote file's size and `lastModified` as [`cacheable_file`] caches one, with no BLAKE3.
const CACHED_SIZE: u64 = 7;
const CACHED_STAMP: i64 = 1_234;

/// An engine with a fresh two-way pair and no rows, whose local root holds `a.bin`, [`CACHED_SIZE`]
/// bytes stamped `local_mtime`, and whose remote root holds a hashless `a.bin` as
/// [`cacheable_file`] caches one: what a pair migrated from filen-sync reads first. Offline: the
/// remote is the cache's rows. With the directory everything is in and the cache, which must
/// outlive the engine's reads.
async fn engine_over_a_hashless_file(
	local_mtime: i64,
) -> (tempfile::TempDir, SyncEngine, PairId, CacheState) {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("local");
	std::fs::create_dir(&root).unwrap();
	let file = root.join("a.bin");
	std::fs::write(&file, [b'x'; 7]).unwrap();
	std::fs::File::options()
		.write(true)
		.open(&file)
		.unwrap()
		.set_modified(
			std::time::UNIX_EPOCH + Duration::from_millis(u64::try_from(local_mtime).unwrap()),
		)
		.unwrap();
	let account = Uuid::from_u128(1);
	let remote_root = Uuid::from_u128(2);
	let cache = dir.path().join("cache.db");
	let mut state = CacheState::new_on_path(&cache, account);
	state
		.upsert_dirs([&cacheable_dir(remote_root, account, "pair")].into_iter())
		.unwrap();
	let remote = cacheable_file(Uuid::from_u128(3), remote_root, "a.bin");
	assert_eq!(
		(remote.size, remote.last_modified.timestamp_millis()),
		(CACHED_SIZE, CACHED_STAMP)
	);
	state.upsert_files([&remote].into_iter()).unwrap();
	let client = Arc::new(offline_client());
	client.configure_cache(cache, |_| {}).await.unwrap();
	let engine = SyncEngine::open(client, dir.path().join("sync.db"))
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(root.to_str().unwrap(), remote_root, SyncMode::TwoWay)
		.unwrap();
	(dir, engine, pair, state)
}

/// The paths `pair`'s rows are at, as its store holds them.
async fn stored_paths(engine: &SyncEngine, pair: PairId) -> Vec<String> {
	let store = engine.pair_store(pair).await.unwrap();
	locked(&store)
		.entries(pair)
		.unwrap()
		.into_iter()
		.map(|row| row.rel_path)
		.collect()
}

/// `plan_pair` on a pair that has never synced plans nothing at a hashless file whose size and
/// stamp match the local one, and writes no row; the pass after it records the row as before.
#[tokio::test]
async fn a_dry_run_of_a_never_synced_pair_plans_what_its_first_pass_will() {
	let (_dir, engine, pair, _cache) = engine_over_a_hashless_file(CACHED_STAMP).await;
	let outcome = engine.plan_pair(pair).await.unwrap();
	assert_eq!(outcome.refused, None);
	assert_eq!(outcome.conflicts, []);
	assert_eq!(outcome.actions, []);
	assert_eq!(outcome.held, []);
	assert_eq!(stored_paths(&engine, pair).await, Vec::<String>::new());

	drop(engine.prepare(pair, None, Seeding::Write).await.unwrap());
	assert_eq!(stored_paths(&engine, pair).await, ["a.bin"]);
}

/// The same for a file only the imported filen-sync state vouches for: its local copy was touched
/// since the hashless upload, and that state lists it at the local size and mtime.
#[tokio::test]
async fn a_dry_run_plans_nothing_at_a_file_the_imported_state_lists() {
	let touched = CACHED_STAMP + 5000;
	let (_dir, engine, pair, _cache) = engine_over_a_hashless_file(touched).await;
	let state = tempfile::tempdir().unwrap();
	std::fs::write(
		state.path().join("previousLocalTree"),
		format!(
			"{{\"prop\":\"/a.bin\",\"data\":{{\"type\":\"file\",\"size\":{CACHED_SIZE},\"lastModified\":{touched}}}}}\n"
		),
	)
	.unwrap();
	std::fs::write(
		state.path().join("previousRemoteTree"),
		format!(
			"{{\"prop\":\"/a.bin\",\"data\":{{\"type\":\"file\",\"uuid\":\"{}\",\"size\":{CACHED_SIZE}}}}}\n",
			Uuid::from_u128(3)
		),
	)
	.unwrap();
	assert_eq!(
		engine
			.import_legacy_state(pair, state.path().to_path_buf())
			.await
			.unwrap()
			.imported,
		1
	);

	let outcome = engine.plan_pair(pair).await.unwrap();
	assert_eq!(outcome.conflicts, []);
	assert_eq!(outcome.actions, []);
	assert_eq!(stored_paths(&engine, pair).await, Vec::<String>::new());

	drop(engine.prepare(pair, None, Seeding::Write).await.unwrap());
	assert_eq!(stored_paths(&engine, pair).await, ["a.bin"]);
}

/// Without evidence — the local copy was touched, and nothing was imported — the dry run still
/// shows the conflict the pass will hold.
#[tokio::test]
async fn a_dry_run_still_shows_the_conflict_where_no_evidence_holds() {
	let (_dir, engine, pair, _cache) = engine_over_a_hashless_file(CACHED_STAMP + 1).await;
	let outcome = engine.plan_pair(pair).await.unwrap();
	let conflicts: Vec<&str> = outcome
		.conflicts
		.iter()
		.map(|conflict| conflict.rel_path.as_str())
		.collect();
	assert_eq!(conflicts, ["a.bin"]);
	assert_eq!(stored_paths(&engine, pair).await, Vec::<String>::new());
}

/// Only a pair whose remote root is the drive root holds the account's data directory.
#[tokio::test]
async fn only_a_pair_rooted_at_the_drive_root_is_drive_rooted() {
	let (engine, _, path) = engine_with_pair("pair_root").await;
	assert_eq!(
		engine.pair_root(engine.client.root().uuid()),
		PairRoot::Drive
	);
	assert_eq!(engine.pair_root(Uuid::from_u128(7)), PairRoot::Directory);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

/// A dry run of a pair rooted at the drive plans nothing in the account's data directory on
/// either side — neither the remote one's file nor a local one's — while everything beside it
/// plans as usual. Offline: the remote is the cache's rows.
#[tokio::test]
async fn a_drive_rooted_dry_run_plans_nothing_in_the_account_data_directory() {
	let dir = tempfile::tempdir().unwrap();
	let root = dir.path().join("local");
	std::fs::create_dir_all(root.join(".filen")).unwrap();
	std::fs::write(root.join(".filen").join("local.bin"), b"l").unwrap();
	std::fs::write(root.join("kept.txt"), b"k").unwrap();
	let client = Arc::new(offline_client());
	let drive = client.root().uuid();
	let account_data = Uuid::from_u128(2);
	let cache = dir.path().join("cache.db");
	let mut state = CacheState::new_on_path(&cache, drive);
	state
		.upsert_dirs([&cacheable_dir(account_data, drive, ".filen")].into_iter())
		.unwrap();
	state
		.upsert_files(
			[
				&cacheable_file(Uuid::from_u128(3), account_data, "chat.bin"),
				&cacheable_file(Uuid::from_u128(4), drive, "docs.txt"),
			]
			.into_iter(),
		)
		.unwrap();
	client.configure_cache(cache, |_| {}).await.unwrap();
	let engine = SyncEngine::open(client, dir.path().join("sync.db"))
		.await
		.unwrap();
	let (pair, _) = locked(&engine.control)
		.create_pair(root.to_str().unwrap(), drive, SyncMode::TwoWay)
		.unwrap();

	let outcome = engine.plan_pair(pair).await.unwrap();
	assert_eq!(outcome.refused, None);
	let mut planned: Vec<&str> = outcome
		.actions
		.iter()
		.chain(&outcome.held)
		.map(|action| action.rel_path.as_str())
		.collect();
	planned.sort_unstable();
	assert_eq!(planned, ["docs.txt", "kept.txt"]);
	assert_eq!(outcome.conflicts, []);
	assert_eq!(outcome.unsyncable, []);
	assert_eq!(outcome.ignored, []);
	drop((engine, state));
}

/// A filen-sync state directory whose two trees list one file alike.
fn filen_sync_state() -> tempfile::TempDir {
	let dir = tempfile::tempdir().unwrap();
	std::fs::write(
		dir.path().join("previousLocalTree"),
		"{\"prop\":\"/a.txt\",\"data\":{\"type\":\"file\",\"size\":5,\"lastModified\":1700000000123}}\n",
	)
	.unwrap();
	std::fs::write(
		dir.path().join("previousRemoteTree"),
		format!(
			"{{\"prop\":\"/a.txt\",\"data\":{{\"type\":\"file\",\"uuid\":\"{}\",\"size\":5}}}}\n",
			Uuid::from_u128(10)
		),
	)
	.unwrap();
	dir
}

#[tokio::test]
async fn filen_sync_state_imports_into_a_pair_that_has_not_synced() {
	let (engine, pair, path) = engine_with_pair("legacy_import").await;
	let state = filen_sync_state();
	let import = engine
		.import_legacy_state(pair, state.path().to_path_buf())
		.await
		.unwrap();
	assert_eq!(
		import,
		LegacyImport {
			imported: 1,
			skipped: 0
		}
	);
	assert_eq!(
		locked(&engine.control).legacy_synced_count(pair).unwrap(),
		1
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn filen_sync_state_is_refused_once_the_pair_has_a_row() {
	let (engine, pair, path) = engine_with_pair("legacy_import_synced").await;
	locked(&engine.control)
		.upsert_entry(
			pair,
			&file_row("b.txt", Uuid::from_u128(5), Blake3Hash::from([1; 32])),
		)
		.unwrap();
	let state = filen_sync_state();
	let error = engine
		.import_legacy_state(pair, state.path().to_path_buf())
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidState);
	assert!(error.to_string().contains("has already synced"), "{error}");
	assert_eq!(
		locked(&engine.control).legacy_synced_count(pair).unwrap(),
		0
	);
	drop(engine);
	std::fs::remove_file(&path).ok();
}

#[tokio::test]
async fn filen_sync_state_is_refused_for_an_unknown_pair() {
	let (engine, pair, path) = engine_with_pair("legacy_import_unknown").await;
	let state = filen_sync_state();
	let error = engine
		.import_legacy_state(pair + 1, state.path().to_path_buf())
		.await
		.unwrap_err();
	assert_eq!(error.kind(), ErrorKind::InvalidState);
	assert!(error.to_string().contains("unknown sync pair"), "{error}");
	drop(engine);
	std::fs::remove_file(&path).ok();
}
