use std::num::NonZeroUsize;

use chrono::{DateTime, Utc};

use super::{
	super::{
		changes::SUBTREE_CAP,
		derive,
		engine::{Observations, PendingKind, PendingWrites},
		ignore::{IgnoreLevel, IgnoreSource, Origin},
		scan::{ACCOUNT_DATA_DIR, LEGACY_TRASH_DIR},
		side::{FromRow, test_support::copied},
		stage::{Stage, WINDOW, reconcile_windows, staged_sides},
	},
	test_support::{confirm_agreed_content, confirm_agreed_pushes, fold_subsumed_deletes},
	*,
};
use crate::{alloc_meter, sync_engine::SyncMode};

/// Reconcile with no pending writes — the ordinary case; the cache-lag window has its own tests.
///
/// Planned twice, and required to come out the same: in memory over the maps, and as a whole pass
/// plans it, window by window over the same sides staged (see [`windowed`]), a window of one
/// entry and one of [`WINDOW`]. So every test written against this holds the windowed reconcile
/// to the resident one as well.
fn plan(
	mode: SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> Vec<SyncAction> {
	let rows = tree(baseline);
	let actions = reconcile(
		mode,
		&rows,
		local,
		remote,
		&PassHolds::default(),
		PassPaths::Whole,
	)
	.actions;
	let (_stages, staged_local, staged_remote) =
		staged_sides(local.values().cloned(), remote.values().cloned(), 2);
	for window in [1, WINDOW.get()] {
		assert_eq!(
			actions,
			windowed(
				mode,
				&rows,
				&staged_local,
				&staged_remote,
				&PassHolds::default(),
				window
			)
			.actions,
			"{mode:?}: the windowed reconcile, {window} entries a window, planned something else"
		);
	}
	actions
}

/// The plan a whole pass makes of its two staged sides, reconciled `window` entries of each input
/// a window into a stage of its own (see [`reconcile_windows`]).
fn windowed(
	mode: SyncMode,
	rows: &Baseline,
	local: &Side<LocalNode>,
	remote: &Side<RemoteNode>,
	holds: &PassHolds,
	window: usize,
) -> Plan {
	let (Side::Staged(local), Side::Staged(remote)) = (local, remote) else {
		panic!("only a whole pass's staged sides are reconciled window by window")
	};
	let window = NonZeroUsize::new(window).expect("a test's window holds an entry");
	let (_dir, stage) = Stage::in_temp_dir();
	reconcile_windows(mode, rows, local, remote, holds, stage, window)
		.and_then(|windows| windows.into_plan(holds))
		.expect("reconciling into a temp stage")
		.0
}

/// The pair the fold tests write as.
const PAIR: super::super::baseline::PairId = 1;

/// What a pass does: correct the snapshot with the engine's own unacknowledged writes, then
/// reconcile against the corrected view.
fn plan_folded(
	mode: SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	writes: &PendingWrites,
) -> Vec<SyncAction> {
	let mut remote = Side::from(remote.clone());
	let baseline = tree(baseline);
	writes.fold_into(PAIR, &baseline, &mut remote, &mut BTreeSet::new());
	reconcile(
		mode,
		&baseline,
		local,
		&remote.of(&baseline),
		&PassHolds::default(),
		PassPaths::Whole,
	)
	.actions
}

/// What a pass plans once the case-only directory renames are folded into its inputs: the
/// renames first, then the reconciled rest.
///
/// Planned twice, and required to come out the same: from the three inputs whole, and as a
/// change-scoped pass holds them — carried sides that decide only what differs from the rows
/// (see [`carried_twin`]). So every fold test written against this is a scoped-versus-whole one
/// as well, where a scoped fold otherwise meets only the generated cases.
fn plan_with_renames(
	mode: SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> Vec<SyncAction> {
	let (mut rows, mut whole_local, mut whole_remote) = (
		tree(baseline),
		Side::from(local.clone()),
		Side::from(remote.clone()),
	);
	let mut actions = fold_dir_moves(
		mode,
		&mut rows,
		&mut whole_local,
		&mut whole_remote,
		&BTreeSet::new(),
		None,
	)
	.unwrap();
	actions.extend(
		reconcile(
			mode,
			&rows,
			&whole_local.of(&rows),
			&whole_remote.of(&rows),
			&PassHolds::default(),
			PassPaths::Whole,
		)
		.actions,
	);

	let rows = tree(baseline);
	let (twin_local, twin_remote, decided) = carried_twin(&rows, local, remote);
	assert_eq!(
		actions,
		plan_scoped(mode, &rows, twin_local, twin_remote, decided),
		"{mode:?}: the carried, change-scoped twin planned something else"
	);
	for page in [1, 2, 1024] {
		assert_eq!(
			actions,
			plan_staged(mode, baseline, local, remote, page),
			"{mode:?}: the staged sides, read {page} row(s) a statement, planned something else"
		);
	}
	actions
}

/// [`plan_with_renames`]'s whole fold and reconcile, over the sides a whole pass stages, read
/// `page` rows a statement.
fn plan_staged(
	mode: SyncMode,
	baseline: &HashMap<String, BaselineEntry>,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
	page: usize,
) -> Vec<SyncAction> {
	let mut rows = tree(baseline);
	let (_stages, mut local, mut remote) =
		staged_sides(local.values().cloned(), remote.values().cloned(), page);
	let mut actions = fold_dir_moves(
		mode,
		&mut rows,
		&mut local,
		&mut remote,
		&BTreeSet::new(),
		None,
	)
	.unwrap();
	let reconciled = reconcile(
		mode,
		&rows,
		&local.of(&rows),
		&remote.of(&rows),
		&PassHolds::default(),
		PassPaths::Whole,
	)
	.actions;
	for window in [1, 7] {
		assert_eq!(
			reconciled,
			windowed(mode, &rows, &local, &remote, &PassHolds::default(), window).actions,
			"{mode:?}: the folded sides, read {page} row(s) a statement and reconciled {window} \
				 a window, planned something else"
		);
	}
	actions.extend(reconciled);
	actions
}

/// The two sides of a test case as a whole map each, or as the sides a whole pass stages, read
/// a row a statement; with the directory of the stages, which must outlive the sides.
fn backed(
	local: HashMap<String, LocalNode>,
	remote: HashMap<String, RemoteNode>,
	staged: bool,
) -> (Option<tempfile::TempDir>, Side<LocalNode>, Side<RemoteNode>) {
	if !staged {
		return (None, Side::from(local), Side::from(remote));
	}
	let (stages, local, remote) = staged_sides(local.into_values(), remote.into_values(), 1);
	(Some(stages), local, remote)
}

/// The two sides a change-scoped pass would hold over `rows` where a whole read found `local`
/// and `remote`, and the set it decides: every row carried, and an observation recorded — the
/// node, or a tombstone — at each path where a whole side holds something other than what the
/// row there carries. Only those paths join the set [`derive::from_baseline`] starts it with,
/// which is the least a pass may decide ([`PassPaths::Changed`]'s contract).
///
/// Nothing is held beyond what the whole run holds, so the two plans can be compared: the
/// rows [`derive`] would hold are uncarryable, and each is in the set already.
fn carried_twin(
	rows: &Baseline,
	local: &HashMap<String, LocalNode>,
	remote: &HashMap<String, RemoteNode>,
) -> (Side<LocalNode>, Side<RemoteNode>, BTreeSet<String>) {
	let derive::Derived {
		local: mut twin_local,
		remote: mut twin_remote,
		mut decided,
		..
	} = derive::from_baseline(rows, BTreeSet::new());
	let mut paths: BTreeSet<String> = local.keys().chain(remote.keys()).cloned().collect();
	rows.visit_row_paths(|path| {
		paths.insert(path.to_owned());
	});
	for path in paths {
		// Both sides recorded: neither is skipped because the other already put the path in
		// the set.
		let local_moved = observe_whole(rows, &mut twin_local, local, &path);
		let remote_moved = observe_whole(rows, &mut twin_remote, remote, &path);
		if local_moved || remote_moved {
			decided.insert(path);
		}
	}
	(twin_local, twin_remote, decided)
}

/// Record on `side` what `whole` holds at `path`, if that is not what the row there carries,
/// and answer whether it recorded anything.
fn observe_whole<T: FromRow + PartialEq>(
	rows: &Baseline,
	side: &mut Side<T>,
	whole: &HashMap<String, T>,
	path: &str,
) -> bool {
	if side.of(rows).at(path).as_deref() == whole.get(path) {
		return false;
	}
	match whole.get(path) {
		Some(node) => side.insert(path.to_owned(), node.clone()),
		None => {
			side.remove(rows, path);
		}
	}
	true
}

/// A synced directory holding `a.txt`, spelled `base_name` in the baseline, `local_name` on disk
/// and `remote_name` on the remote.
fn case_tree(
	base_name: &str,
	local_name: &str,
	remote_name: &str,
	dir: Uuid,
	file: Uuid,
) -> (
	HashMap<String, BaselineEntry>,
	HashMap<String, LocalNode>,
	HashMap<String, RemoteNode>,
) {
	let child = |name: &str| format!("{name}/a.txt");
	let baseline = HashMap::from([
		(base_name.to_string(), base_dir(base_name, dir)),
		(
			child(base_name),
			base_file(&child(base_name), file, [1; 32]),
		),
	]);
	let local = HashMap::from([
		(local_name.to_string(), local_dir(local_name)),
		(child(local_name), local_file(&child(local_name), [1; 32])),
	]);
	let remote = HashMap::from([
		(remote_name.to_string(), remote_dir_node(remote_name, dir)),
		(
			child(remote_name),
			remote_file(&child(remote_name), file, [1; 32]),
		),
	]);
	(baseline, local, remote)
}

/// A directory renamed only by case on the LOCAL side is renamed in place on the remote, and
/// nothing else happens: no create at the new spelling (the dedup would hand it the same
/// directory) and no trash at the old one (which would take that directory down with its
/// subtree).
#[test]
fn a_local_case_only_dir_rename_renames_the_remote_dir_in_place() {
	for mode in [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::LocalBackup,
	] {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
		assert_eq!(
			plan_with_renames(mode, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "Docs".to_string(),
				to_path: "docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}],
			"{mode:?}"
		);
	}
}

/// The mirror: a directory renamed only by case on the REMOTE side is renamed in place locally,
/// instead of quarantining the local subtree and re-creating it.
#[test]
fn a_remote_case_only_dir_rename_renames_the_local_dir_in_place() {
	for mode in [
		SyncMode::TwoWay,
		SyncMode::RemoteToLocal,
		SyncMode::RemoteBackup,
	] {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (baseline, local, remote) = case_tree("Docs", "Docs", "docs", dir, file);
		assert_eq!(
			plan_with_renames(mode, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "Docs".to_string(),
				to_path: "docs".to_string(),
				kind: NodeKind::Dir,
			}],
			"{mode:?}"
		);
	}
}

/// A one-way mode puts its SOURCE's spelling back on a destination whose case drifted, whatever
/// the baseline says.
#[test]
fn a_one_way_mode_renames_its_destination_to_the_source_spelling() {
	let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
	let (baseline, local, remote) = case_tree("Docs", "Docs", "docs", dir, file);
	assert_eq!(
		plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote),
		vec![SyncAction::MoveRemote {
			from_path: "docs".to_string(),
			to_path: "Docs".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: dir,
		}]
	);
}

/// Without the fold the same inputs plan the destructive shape this exists to prevent — a create
/// at one spelling and a directory trash at the other. Pins why the fold has to run first.
#[test]
fn a_case_only_dir_rename_planned_per_path_trashes_the_directory() {
	let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
	let (baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		actions.contains(&SyncAction::TrashRemote {
			rel_path: "Docs".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: dir,
		}),
		"{actions:?}"
	);
}

/// A child that changed in the same pass is still planned — at the new spelling, after the
/// rename.
#[test]
fn a_child_edited_under_a_case_renamed_dir_is_pushed_at_the_new_spelling() {
	let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
	let (baseline, mut local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
	local.insert("docs/a.txt".to_string(), local_file("docs/a.txt", [2; 32]));
	let rename = SyncAction::MoveRemote {
		from_path: "Docs".to_string(),
		to_path: "docs".to_string(),
		kind: NodeKind::Dir,
		remote_uuid: dir,
	};
	let mut ordered = vec![
		SyncAction::UploadFile {
			rel_path: "docs/a.txt".to_string(),
		},
		rename.clone(),
	];
	order_actions(&mut ordered);
	assert_eq!(
		ordered[0], rename,
		"the rename runs before anything under it"
	);
	assert_eq!(
		plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			rename,
			SyncAction::UploadFile {
				rel_path: "docs/a.txt".to_string(),
			},
		]
	);
}

/// A case change at two levels is two renames, the parent first, and nothing more.
#[test]
fn nested_case_only_dir_renames_fold_parent_first() {
	let (outer, inner, file) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let baseline = HashMap::from([
		("Docs".to_string(), base_dir("Docs", outer)),
		("Docs/Sub".to_string(), base_dir("Docs/Sub", inner)),
		(
			"Docs/Sub/a.txt".to_string(),
			base_file("Docs/Sub/a.txt", file, [1; 32]),
		),
	]);
	let local = HashMap::from([
		("docs".to_string(), local_dir("docs")),
		("docs/sub".to_string(), local_dir("docs/sub")),
		(
			"docs/sub/a.txt".to_string(),
			local_file("docs/sub/a.txt", [1; 32]),
		),
	]);
	let remote = HashMap::from([
		("Docs".to_string(), remote_dir_node("Docs", outer)),
		("Docs/Sub".to_string(), remote_dir_node("Docs/Sub", inner)),
		(
			"Docs/Sub/a.txt".to_string(),
			remote_file("Docs/Sub/a.txt", file, [1; 32]),
		),
	]);
	assert_eq!(
		plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			SyncAction::MoveRemote {
				from_path: "Docs".to_string(),
				to_path: "docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: outer,
			},
			SyncAction::MoveRemote {
				from_path: "docs/Sub".to_string(),
				to_path: "docs/sub".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: inner,
			},
		]
	);
}

/// A path held in conflict is not renamed: the conflict hold decides it.
#[test]
fn a_conflicted_dir_is_not_case_renamed() {
	for staged in [false, true] {
		let (dir, file) = (Uuid::new_v4(), Uuid::new_v4());
		let (mut baseline, local, remote) = case_tree("Docs", "docs", "Docs", dir, file);
		let (_stages, mut local, mut remote) = backed(local, remote, staged);
		baseline.get_mut("Docs").unwrap().state = BaselineState::Conflicted;
		let mut baseline = tree(&baseline);
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&BTreeSet::new(),
				None,
			)
			.unwrap()
			.is_empty(),
			"staged: {staged}"
		);
	}
}

/// The uuids of a synced directory tree `<dir>/a.txt`, `<dir>/sub/`, `<dir>/sub/b.txt`.
#[derive(Clone, Copy)]
struct TreeIds {
	dir: Uuid,
	a: Uuid,
	sub: Uuid,
	b: Uuid,
}

impl TreeIds {
	fn new() -> Self {
		Self {
			dir: Uuid::new_v4(),
			a: Uuid::new_v4(),
			sub: Uuid::new_v4(),
			b: Uuid::new_v4(),
		}
	}

	fn paths(dir: &str) -> [String; 4] {
		[
			dir.to_string(),
			format!("{dir}/a.txt"),
			format!("{dir}/sub"),
			format!("{dir}/sub/b.txt"),
		]
	}

	fn baseline(self, dir: &str) -> Vec<(String, BaselineEntry)> {
		let [d, a, s, b] = Self::paths(dir);
		vec![
			(d.clone(), base_dir(&d, self.dir)),
			(a.clone(), base_file(&a, self.a, [1; 32])),
			(s.clone(), base_dir(&s, self.sub)),
			(b.clone(), base_file(&b, self.b, [2; 32])),
		]
	}

	fn local(dir: &str) -> Vec<(String, LocalNode)> {
		let [d, a, s, b] = Self::paths(dir);
		vec![
			(d.clone(), local_dir(&d)),
			(a.clone(), local_file(&a, [1; 32])),
			(s.clone(), local_dir(&s)),
			(b.clone(), local_file(&b, [2; 32])),
		]
	}

	fn remote(self, dir: &str) -> Vec<(String, RemoteNode)> {
		let [d, a, s, b] = Self::paths(dir);
		vec![
			(d.clone(), remote_dir_node(&d, self.dir)),
			(a.clone(), remote_file(&a, self.a, [1; 32])),
			(s.clone(), remote_dir_node(&s, self.sub)),
			(b.clone(), remote_file(&b, self.b, [2; 32])),
		]
	}
}

/// A pair holding the synced tree under `docs/` plus an empty synced `archive/`, with the local
/// side holding it under `local_dir` and the remote under `remote_dir`.
fn moved_tree(
	ids: TreeIds,
	local_at: &str,
	remote_at: &str,
) -> (
	HashMap<String, BaselineEntry>,
	HashMap<String, LocalNode>,
	HashMap<String, RemoteNode>,
) {
	let archive = Uuid::new_v4();
	let mut baseline: HashMap<_, _> = ids.baseline("docs").into_iter().collect();
	baseline.insert("archive".to_string(), base_dir("archive", archive));
	let mut local: HashMap<_, _> = TreeIds::local(local_at).into_iter().collect();
	local.insert("archive".to_string(), local_dir("archive"));
	let mut remote: HashMap<_, _> = ids.remote(remote_at).into_iter().collect();
	remote.insert("archive".to_string(), remote_dir_node("archive", archive));
	(baseline, local, remote)
}

fn dir_moves_in(actions: &[SyncAction]) -> Vec<&SyncAction> {
	actions
		.iter()
		.filter(|action| {
			matches!(
				action,
				SyncAction::MoveRemote {
					kind: NodeKind::Dir,
					..
				} | SyncAction::MoveLocal {
					kind: NodeKind::Dir,
					..
				}
			)
		})
		.collect()
}

/// A directory the remote moved and renamed is ONE local move — children carried with it,
/// nothing downloaded, quarantined or re-created — in every mode that pulls.
#[test]
fn a_remote_dir_move_is_one_local_move() {
	for mode in [
		SyncMode::TwoWay,
		SyncMode::RemoteToLocal,
		SyncMode::RemoteBackup,
	] {
		let (baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "archive/documents");
		assert_eq!(
			plan_with_renames(mode, &baseline, &local, &remote),
			vec![SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "archive/documents".to_string(),
				kind: NodeKind::Dir,
			}],
			"{mode:?}"
		);
	}
}

/// A directory the local side moved and renamed, with its subtree intact, is ONE remote move of
/// the same uuid in every mode that pushes.
#[test]
fn a_local_dir_move_is_one_remote_move_of_the_same_uuid() {
	for mode in [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::LocalBackup,
	] {
		let ids = TreeIds::new();
		let (baseline, local, remote) = moved_tree(ids, "archive/documents", "docs");
		assert_eq!(
			plan_with_renames(mode, &baseline, &local, &remote),
			vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "archive/documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: ids.dir,
			}],
			"{mode:?}"
		);
	}
}

/// Without the fold a local directory rename is the per-path plan: a new directory, a move per
/// file, and the old directory trashed. Pins what the fold replaces.
#[test]
fn a_local_dir_rename_planned_per_path_re_creates_the_directory() {
	let ids = TreeIds::new();
	let (baseline, local, remote) = moved_tree(ids, "documents", "docs");
	let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		actions.contains(&SyncAction::TrashRemote {
			rel_path: "docs".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: ids.dir,
		}),
		"{actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::CreateRemoteDir {
			rel_path: "documents".to_string(),
		}),
		"{actions:?}"
	);
}

/// A local rename whose subtree also changed is not matched: an edited child, a child added and
/// a child left behind each break the signature, and the pass falls back to the per-path plan.
#[test]
fn a_local_dir_rename_with_a_changed_subtree_is_not_matched() {
	type Change = fn(&mut HashMap<String, LocalNode>);
	let changes: [(&str, Change); 4] = [
		("a child edited", |local| {
			local.insert(
				"documents/a.txt".to_string(),
				local_file("documents/a.txt", [9; 32]),
			);
		}),
		("a child added", |local| {
			local.insert(
				"documents/new.txt".to_string(),
				local_file("documents/new.txt", [9; 32]),
			);
		}),
		("a child left behind", |local| {
			local.remove("documents/sub/b.txt");
			local.insert("b.txt".to_string(), local_file("b.txt", [2; 32]));
		}),
		("a child renamed", |local| {
			let node = local.remove("documents/a.txt").unwrap();
			local.insert(
				"documents/renamed.txt".to_string(),
				LocalNode {
					rel_path: "documents/renamed.txt".to_string(),
					..node
				},
			);
		}),
	];
	for (label, change) in changes {
		let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
		change(&mut local);
		let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
		// An unchanged directory inside may still move on its own, into the new parent.
		assert!(
			moves_onto(&actions, "documents").is_none(),
			"{label}: {actions:?}"
		);
	}
}

/// A subtree that holds no file at all says nothing about identity: two empty directories are
/// not a move.
#[test]
fn a_local_rename_of_a_subtree_without_files_is_not_matched() {
	let (dir, sub) = (Uuid::new_v4(), Uuid::new_v4());
	let baseline = map(vec![
		("old", base_dir("old", dir)),
		("old/sub", base_dir("old/sub", sub)),
	]);
	let remote = map(vec![
		("old", remote_dir_node("old", dir)),
		("old/sub", remote_dir_node("old/sub", sub)),
	]);
	let local = map(vec![
		("new", local_dir("new")),
		("new/sub", local_dir("new/sub")),
	]);
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
}

/// Two candidates on either end make the match a guess, so neither is taken.
#[test]
fn an_ambiguous_local_dir_rename_is_not_matched() {
	// One vanished tree, two new copies of it.
	let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
	local.extend(TreeIds::local("copy"));
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(dir_moves_in(&actions).is_empty(), "two new: {actions:?}");

	// Two vanished trees with the same content, one new copy.
	let (mut baseline, local, mut remote) = moved_tree(TreeIds::new(), "documents", "docs");
	let twin = TreeIds::new();
	baseline.extend(twin.baseline("twin"));
	remote.extend(twin.remote("twin"));
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		dir_moves_in(&actions).is_empty(),
		"two vanished: {actions:?}"
	);
}

/// A destination already taken — on the side being moved, or in the baseline — is not moved
/// onto.
#[test]
fn a_dir_move_onto_an_existing_path_is_not_planned() {
	// Push: the remote already holds another directory under the new name. The tree is not
	// moved onto it; what lands inside it is the per-path plan's business (its `sub/` may still
	// move in as a directory of its own).
	let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "documents", "docs");
	remote.insert(
		"documents".to_string(),
		remote_dir_node("documents", Uuid::new_v4()),
	);
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		!dir_moves_in(&actions)
			.iter()
			.any(|action| action.rel_path() == "documents"),
		"push: {actions:?}"
	);

	// Pull: the local side already holds a directory where the remote moved it.
	let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
	local.insert("documents".to_string(), local_dir("documents"));
	let actions = plan_with_renames(SyncMode::RemoteToLocal, &baseline, &local, &remote);
	assert!(
		moves_onto(&actions, "documents").is_none(),
		"pull: {actions:?}"
	);

	// Pull: the baseline still tracks something at the destination.
	let (mut baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
	baseline.insert(
		"documents/x.txt".to_string(),
		base_file("documents/x.txt", Uuid::new_v4(), [3; 32]),
	);
	let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		moves_onto(&actions, "documents").is_none(),
		"baseline: {actions:?}"
	);
}

/// The directory move in `actions` whose destination is `to`, if any.
fn moves_onto<'a>(actions: &'a [SyncAction], to: &str) -> Option<&'a SyncAction> {
	dir_moves_in(actions)
		.into_iter()
		.find(|action| action.rel_path() == to)
}

/// A move into a directory the other side has new is still one move: the reconcile plans the
/// create of the new parent, which the engine runs just before the move. A parent the baseline
/// records is not one the pass creates, so that move is left to the per-path plan.
#[test]
fn a_dir_move_into_a_new_parent_is_one_move() {
	let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "fresh/documents", "docs");
	local.insert("fresh".to_string(), local_dir("fresh"));
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		moves_onto(&actions, "fresh/documents").is_some(),
		"push: {actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::CreateRemoteDir {
			rel_path: "fresh".to_string(),
		}),
		"push: {actions:?}"
	);

	let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "docs", "fresh/documents");
	remote.insert(
		"fresh".to_string(),
		remote_dir_node("fresh", Uuid::new_v4()),
	);
	let actions = plan_with_renames(SyncMode::RemoteToLocal, &baseline, &local, &remote);
	assert!(
		moves_onto(&actions, "fresh/documents").is_some(),
		"pull: {actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::CreateLocalDir {
			rel_path: "fresh".to_string(),
		}),
		"pull: {actions:?}"
	);

	let (mut baseline, mut local, remote) = moved_tree(TreeIds::new(), "fresh/documents", "docs");
	local.insert("fresh".to_string(), local_dir("fresh"));
	baseline.insert("fresh".to_string(), base_dir("fresh", Uuid::new_v4()));
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		moves_onto(&actions, "fresh/documents").is_none(),
		"a recorded parent the remote lost: {actions:?}"
	);
}

/// A conflict held anywhere in the subtree keeps the directory where it is.
#[test]
fn a_dir_holding_a_conflict_is_not_moved() {
	let (mut baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
	baseline.get_mut("docs/sub/b.txt").unwrap().state = BaselineState::Conflicted;
	let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
}

/// A path the cache is holding at either end keeps the directory where it is.
#[test]
fn a_dir_under_a_held_remote_path_is_not_moved() {
	for staged in [false, true] {
		let (baseline, local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
		let (_stages, mut local, mut remote) = backed(local, remote, staged);
		let held = BTreeSet::from(["documents/sub/b.txt".to_string()]);
		let mut baseline = tree(&baseline);
		assert!(
			fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&held,
				None,
			)
			.unwrap()
			.is_empty(),
			"staged: {staged}"
		);
	}
}

/// A pushed directory move is carried at the scope a pass read, not just at the whole map. The
/// two ends are all the detector is given here — the baseline row's path and the new local
/// directory — which is what an observation that moved a directory names.
#[test]
fn a_dir_move_is_carried_at_the_scope_a_pass_read() {
	let (baseline, local, remote) = moved_tree(TreeIds::new(), "documents", "docs");
	let mut scope = BTreeSet::from(["docs".to_string(), "documents".to_string()]);
	let (mut whole_baseline, mut whole_local, mut whole_remote) = (
		tree(&baseline),
		Side::from(local.clone()),
		Side::from(remote.clone()),
	);
	let (mut scoped_baseline, mut scoped_local, mut scoped_remote) = (
		tree(&baseline),
		Side::from(local.clone()),
		Side::from(remote.clone()),
	);
	let by_whole = fold_dir_moves(
		SyncMode::TwoWay,
		&mut whole_baseline,
		&mut whole_local,
		&mut whole_remote,
		&BTreeSet::new(),
		None,
	)
	.unwrap();
	let by_scope = fold_dir_moves(
		SyncMode::TwoWay,
		&mut scoped_baseline,
		&mut scoped_local,
		&mut scoped_remote,
		&BTreeSet::new(),
		Some(&mut scope),
	)
	.unwrap();
	assert!(
		!by_whole.is_empty(),
		"the whole fold carried no directory move, so this compared nothing"
	);
	assert_eq!(by_whole, by_scope, "the narrowed fold missed the move");
	assert_eq!(whole_local, scoped_local);
	assert_eq!(whole_remote, scoped_remote);

	// And whole over the sides a whole pass stages.
	let (_stages, mut staged_local, mut staged_remote) = backed(local, remote, true);
	let mut staged_baseline = tree(&baseline);
	let by_stage = fold_dir_moves(
		SyncMode::TwoWay,
		&mut staged_baseline,
		&mut staged_local,
		&mut staged_remote,
		&BTreeSet::new(),
		None,
	)
	.unwrap();
	assert_eq!(by_whole, by_stage, "the staged fold missed the move");
	assert_eq!(whole_local.into_whole(), staged_local.into_whole());
	assert_eq!(whole_remote.into_whole(), staged_remote.into_whole());
}

/// What a change-scoped pass plans from carried sides: the fold at the pass's own set, then the
/// reconcile at the set the fold hands back.
fn plan_scoped(
	mode: SyncMode,
	rows: &Baseline,
	mut local: Side<LocalNode>,
	mut remote: Side<RemoteNode>,
	mut decided: BTreeSet<String>,
) -> Vec<SyncAction> {
	let mut rows = rows.clone();
	let mut actions = fold_dir_moves(
		mode,
		&mut rows,
		&mut local,
		&mut remote,
		&BTreeSet::new(),
		Some(&mut decided),
	)
	.unwrap();
	actions.extend(
		reconcile(
			mode,
			&rows,
			&local.of(&rows),
			&remote.of(&rows),
			&PassHolds::default(),
			PassPaths::Changed(&decided),
		)
		.actions,
	);
	actions
}

/// `decided` with every move of `moves` replayed over it in order, as the fold re-keys the set
/// it is handed: that set as it would come back had the fold taken nothing out of it.
fn rekeyed(decided: BTreeSet<String>, moves: &[SyncAction]) -> BTreeSet<String> {
	moves.iter().fold(decided, |set, action| {
		let (from, to) = action.endpoints();
		set.into_iter()
			.map(|path| moved_path(&path, from, to).unwrap_or(path))
			.collect()
	})
}

/// Fold and reconcile carried sides three ways — at the set the fold hands back, at that set
/// with nothing taken out of it, and whole — and require one plan of all three. Hands back the
/// plan and the set the fold handed back.
fn plan_three_ways(
	mode: SyncMode,
	rows: &Baseline,
	local: Side<LocalNode>,
	remote: Side<RemoteNode>,
	decided: BTreeSet<String>,
) -> (Vec<SyncAction>, BTreeSet<String>) {
	let (mut folded, mut folded_local, mut folded_remote) =
		(rows.clone(), copied(&local), copied(&remote));
	let mut pruned = decided.clone();
	let moves = fold_dir_moves(
		mode,
		&mut folded,
		&mut folded_local,
		&mut folded_remote,
		&BTreeSet::new(),
		Some(&mut pruned),
	)
	.unwrap();
	// What staying put recorded is in the fold's set and in no replay of the moves.
	let unpruned: BTreeSet<String> = rekeyed(decided, &moves).union(&pruned).cloned().collect();
	let plan_at = |paths| {
		let mut actions = moves.clone();
		actions.extend(
			reconcile(
				mode,
				&folded,
				&folded_local.of(&folded),
				&folded_remote.of(&folded),
				&PassHolds::default(),
				paths,
			)
			.actions,
		);
		actions
	};
	let plan = plan_at(PassPaths::Changed(&pruned));
	assert_eq!(
		plan,
		plan_at(PassPaths::Changed(&unpruned)),
		"{mode:?}: the set the fold pruned planned something the unpruned one did not"
	);
	let (mut whole_rows, mut whole_local, mut whole_remote) = (rows.clone(), local, remote);
	let mut whole = fold_dir_moves(
		mode,
		&mut whole_rows,
		&mut whole_local,
		&mut whole_remote,
		&BTreeSet::new(),
		None,
	)
	.unwrap();
	whole.extend(
		reconcile(
			mode,
			&whole_rows,
			&whole_local.of(&whole_rows),
			&whole_remote.of(&whole_rows),
			&PassHolds::default(),
			PassPaths::Whole,
		)
		.actions,
	);
	assert_eq!(
		plan, whole,
		"{mode:?}: the scoped pass planned something the whole one did not"
	);
	(plan, pruned)
}

/// What a pushed directory move proved it carried unchanged leaves the set; what the remote
/// recorded under it does not. A new version of a file announced inside the moved directory is
/// still planned at its new path, and so is a deletion there, while the files the move carried
/// untouched are no longer decided at all.
#[test]
fn a_remote_edit_inside_a_pushed_directory_move_is_still_planned() {
	let dir = Uuid::new_v4();
	let files = ["a.txt", "b.txt", "c.txt"].map(|name| (name, Uuid::new_v4()));
	let baseline = Baseline::from_rows(
		std::iter::once(base_dir("docs", dir)).chain(
			files
				.iter()
				.map(|(name, uuid)| base_file(&format!("docs/{name}"), *uuid, [1; 32])),
		),
	);
	let edited = Uuid::new_v4();
	for mode in [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::LocalBackup,
	] {
		for trashed in [false, true] {
			// What a pass holds once it observed the rename: the old name gone, the new one
			// walked...
			let (mut local, mut remote) = (Side::carried(), Side::carried());
			let mut decided = BTreeSet::from(["docs".to_string(), "documents".to_string()]);
			local.remove(&baseline, "docs");
			local.insert("documents".to_string(), local_dir("documents"));
			for (name, _) in &files {
				let (from, to) = (format!("docs/{name}"), format!("documents/{name}"));
				local.remove(&baseline, &from);
				local.insert(to.clone(), local_file(&to, [1; 32]));
				decided.extend([from, to]);
			}
			// ...and announced on the remote meanwhile, a new version of `a.txt` and, the
			// second time round, `b.txt` trashed.
			remote.insert(
				"docs/a.txt".to_string(),
				remote_version("docs/a.txt", files[0].1, edited, [9; 32]),
			);
			if trashed {
				remote.remove(&baseline, "docs/b.txt");
			}
			let (plan, pruned) = plan_three_ways(mode, &baseline, local, remote, decided);
			let mut expected = vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}];
			expected.push(match mode {
				SyncMode::TwoWay => SyncAction::DownloadFile {
					rel_path: "documents/a.txt".to_string(),
					remote_uuid: edited,
				},
				_ => SyncAction::UploadFile {
					rel_path: "documents/a.txt".to_string(),
				},
			});
			if trashed {
				expected.push(match mode {
					SyncMode::TwoWay => SyncAction::DeleteLocal {
						rel_path: "documents/b.txt".to_string(),
						kind: NodeKind::File,
					},
					_ => SyncAction::UploadFile {
						rel_path: "documents/b.txt".to_string(),
					},
				});
			}
			assert_eq!(plan, expected, "{mode:?}, b.txt trashed: {trashed}");
			assert!(
				pruned.contains("documents") && pruned.contains("documents/a.txt"),
				"{mode:?}: the destination and the edited file stay decided: {pruned:?}"
			);
			assert_eq!(
				pruned.contains("documents/b.txt"),
				trashed,
				"{mode:?}: b.txt stays decided exactly when the remote recorded its trashing: \
					 {pruned:?}"
			);
			assert!(
				!pruned.contains("documents/c.txt"),
				"{mode:?}: the file carried untouched leaves the set: {pruned:?}"
			);
		}
	}
}

/// A case-only directory rename is a remote move too, but matched by name alone: it proves
/// nothing about what the directory holds, so the set is only re-keyed and a child edited in
/// the same pass is still pushed.
#[test]
fn a_case_only_directory_rename_decides_everything_it_carried() {
	let (dir, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let baseline = Baseline::from_rows([
		base_dir("Docs", dir),
		base_file("Docs/a.txt", a, [1; 32]),
		base_file("Docs/b.txt", b, [2; 32]),
	]);
	let mut local = Side::carried();
	let mut decided = BTreeSet::new();
	for (old, new) in [
		("Docs", "docs"),
		("Docs/a.txt", "docs/a.txt"),
		("Docs/b.txt", "docs/b.txt"),
	] {
		local.remove(&baseline, old);
		decided.extend([old.to_string(), new.to_string()]);
	}
	local.extend([
		("docs".to_string(), local_dir("docs")),
		("docs/a.txt".to_string(), local_file("docs/a.txt", [1; 32])),
		("docs/b.txt".to_string(), local_file("docs/b.txt", [9; 32])),
	]);
	let (plan, pruned) = plan_three_ways(
		SyncMode::LocalToRemote,
		&baseline,
		local,
		Side::carried(),
		decided.clone(),
	);
	assert_eq!(
		plan,
		vec![
			SyncAction::MoveRemote {
				from_path: "Docs".to_string(),
				to_path: "docs".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			},
			SyncAction::UploadFile {
				rel_path: "docs/b.txt".to_string(),
			},
		]
	);
	assert_eq!(
		pruned,
		rekeyed(decided, &plan[..1]),
		"the set is re-keyed, and nothing leaves it"
	);
}

/// `mv z new; mv b new/b` at a change-scoped pass. `new` matches `z` only once the nested
/// `new/b` is set aside, so that move proves nothing about `new/b` and takes nothing out of the
/// set: the next iteration still finds `new/b` among the local directories no row records, and
/// folds the second move as a whole pass does.
#[test]
fn a_move_matched_without_its_nested_move_prunes_nothing() {
	let (b, z) = (Uuid::new_v4(), Uuid::new_v4());
	let baseline = Baseline::from_rows([
		base_dir("b", b),
		base_file("b/1.txt", Uuid::new_v4(), [1; 32]),
		base_dir("z", z),
		base_file("z/2.txt", Uuid::new_v4(), [2; 32]),
	]);
	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		let mut local = Side::carried();
		let mut decided = BTreeSet::new();
		for (old, new) in [
			("z", "new"),
			("z/2.txt", "new/2.txt"),
			("b", "new/b"),
			("b/1.txt", "new/b/1.txt"),
		] {
			local.remove(&baseline, old);
			decided.extend([old.to_string(), new.to_string()]);
		}
		local.extend([
			("new".to_string(), local_dir("new")),
			("new/2.txt".to_string(), local_file("new/2.txt", [2; 32])),
			("new/b".to_string(), local_dir("new/b")),
			(
				"new/b/1.txt".to_string(),
				local_file("new/b/1.txt", [1; 32]),
			),
		]);
		let (plan, _) = plan_three_ways(mode, &baseline, local, Side::carried(), decided);
		assert_eq!(
			plan,
			vec![
				SyncAction::MoveRemote {
					from_path: "z".to_string(),
					to_path: "new".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: z,
				},
				SyncAction::MoveRemote {
					from_path: "b".to_string(),
					to_path: "new/b".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: b,
				},
			],
			"{mode:?}"
		);
	}
}

/// A push matched on the new directory's whole signature still takes nothing out of the set
/// where the rows under it do not answer for the remote side: once with a row there that no
/// side is carried from — the engine holds such a row and so refuses the move, a caller that
/// does not hold it leaves it unobserved — and once with a whole remote side, whose every node
/// is one the pass recorded.
#[test]
fn a_proven_directory_move_prunes_nothing_its_rows_do_not_answer_for() {
	let dir = Uuid::new_v4();
	let files = ["a.txt", "b.txt", "c.txt"].map(|name| (name, Uuid::new_v4()));
	for whole_remote in [false, true] {
		let mut baseline = Baseline::from_rows(std::iter::once(base_dir("docs", dir)).chain(
			files.iter().map(|(name, uuid)| {
				let row = base_file(&format!("docs/{name}"), *uuid, [1; 32]);
				match (*name, whole_remote) {
					// Synced, with the hash and size the move is matched on, but no remote
					// stamp: a row neither side is carried from.
					("b.txt", false) => BaselineEntry {
						remote_modified: None,
						..row
					},
					_ => row,
				}
			}),
		));
		let mut local = Side::carried();
		let mut decided = BTreeSet::from(["docs".to_string(), "documents".to_string()]);
		local.remove(&baseline, "docs");
		local.insert("documents".to_string(), local_dir("documents"));
		for (name, _) in &files {
			let (from, to) = (format!("docs/{name}"), format!("documents/{name}"));
			local.remove(&baseline, &from);
			// One path, which the side and the set each keep a copy of.
			local.insert(to.clone(), local_file(&to, [1; 32]));
			decided.extend([from, to]);
		}
		let mut remote = match whole_remote {
			false => Side::carried(),
			true => std::iter::once(("docs".to_string(), remote_dir_node("docs", dir)))
				.chain(files.iter().map(|(name, uuid)| {
					let path = format!("docs/{name}");
					let node = remote_file(&path, *uuid, [1; 32]);
					(path, node)
				}))
				.collect(),
		};
		// The set as the fold is handed it, to replay the move over once the fold has its own.
		let original = decided.clone();
		let moves = fold_dir_moves(
			SyncMode::LocalToRemote,
			&mut baseline,
			&mut local,
			&mut remote,
			&BTreeSet::new(),
			Some(&mut decided),
		)
		.unwrap();
		assert_eq!(
			moves,
			vec![SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir,
			}],
			"whole remote: {whole_remote}"
		);
		assert_eq!(
			decided,
			rekeyed(original, &moves),
			"whole remote: {whole_remote}: nothing may leave the set"
		);
	}
}

/// The remote deleted a file and then moved its directory, inside one delta: the view
/// tombstones the file where it was and carries the rest across. Folding the move moves the
/// rows under BOTH sides, and a carried remote side that followed them held the deleted file
/// again at its new path — so the pass planned nothing there, and no later scoped pass would
/// either, where a whole pass deletes the local copy.
#[test]
fn a_file_the_remote_deleted_before_moving_its_directory_is_still_deleted() {
	let (dir, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let rows = [
		base_dir("docs", dir),
		base_file("docs/a.txt", a, [1; 32]),
		base_file("docs/b.txt", b, [2; 32]),
	];
	let baseline = Baseline::from_rows(rows.clone());
	let whole_baseline: HashMap<String, BaselineEntry> = rows
		.into_iter()
		.map(|row| (row.rel_path.clone(), row))
		.collect();
	let whole_local = map(vec![
		("docs", local_dir("docs")),
		("docs/a.txt", local_file("docs/a.txt", [1; 32])),
		("docs/b.txt", local_file("docs/b.txt", [2; 32])),
	]);
	let whole_remote = map(vec![
		("documents", remote_dir_node("documents", dir)),
		(
			"documents/a.txt",
			remote_file("documents/a.txt", a, [1; 32]),
		),
	]);
	let expected = vec![
		SyncAction::MoveLocal {
			from_path: "docs".to_string(),
			to_path: "documents".to_string(),
			kind: NodeKind::Dir,
		},
		SyncAction::DeleteLocal {
			rel_path: "documents/b.txt".to_string(),
			kind: NodeKind::File,
		},
	];
	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		assert_eq!(
			plan_with_renames(mode, &whole_baseline, &whole_local, &whole_remote),
			expected,
			"whole, {mode:?}"
		);
		// What the delta leaves on a carried view: `b.txt` gone where it was, then the
		// directory and what is left in it moved across.
		let mut remote = Side::carried();
		for path in ["docs/b.txt", "docs", "docs/a.txt"] {
			remote.remove(&baseline, path);
		}
		remote.extend(whole_remote.clone());
		let decided = [
			"docs",
			"docs/a.txt",
			"docs/b.txt",
			"documents",
			"documents/a.txt",
		]
		.map(str::to_string)
		.into();
		assert_eq!(
			plan_scoped(mode, &baseline, Side::carried(), remote, decided),
			expected,
			"scoped, {mode:?}"
		);
	}
}

/// The side a pulled directory move does not carry stays put by recording a node or a tombstone
/// at each row the move would otherwise take with it. Past the cap a change-scoped pass reads
/// both sides whole instead, refused before either side is re-keyed; at the cap the move folds.
#[test]
fn a_move_whose_side_staying_put_reads_more_rows_than_the_cap_is_a_whole_read() {
	let dir = Uuid::from_u128(1);
	for (count, expected) in [
		(SUBTREE_CAP, Ok(1)),
		(SUBTREE_CAP + 1, Err(FullPassReason::RemoteOverflow)),
	] {
		let files: Vec<(String, Uuid)> = (0..count)
			.map(|n| (format!("{n:05}.txt"), Uuid::from_u128(10_000 + n as u128)))
			.collect();
		let mut baseline = Baseline::from_rows(
			std::iter::once(base_dir("docs", dir)).chain(
				files
					.iter()
					.map(|(name, uuid)| base_file(&format!("docs/{name}"), *uuid, [1; 32])),
			),
		);
		// What the delta leaves on a carried view: the directory and what it holds moved across.
		let mut remote = Side::carried();
		let mut decided = BTreeSet::from(["docs".to_string(), "documents".to_string()]);
		remote.remove(&baseline, "docs");
		remote.insert("documents".to_string(), remote_dir_node("documents", dir));
		for (name, uuid) in &files {
			let (from, to) = (format!("docs/{name}"), format!("documents/{name}"));
			remote.remove(&baseline, &from);
			remote.insert(to.clone(), remote_file(&to, *uuid, [1; 32]));
			decided.extend([from, to]);
		}

		let moves = fold_dir_moves(
			SyncMode::TwoWay,
			&mut baseline,
			&mut Side::carried(),
			&mut remote,
			&BTreeSet::new(),
			Some(&mut decided),
		);

		assert_eq!(moves.map(|moves| moves.len()), expected, "{count} file(s)");
		assert_eq!(
			decided.contains("docs"),
			expected.is_err(),
			"{count} file(s): the set is re-keyed only by a move that folded"
		);
	}
}

/// Both levels of a nested directory renamed by case on the local side, at a change-scoped
/// pass. The outer rename moves the rows under the local side too, and a carried local side
/// that followed them held the inner directory again under its old spelling — so the inner
/// rename was never found, and the pass created the inner directory afresh instead.
#[test]
fn nested_case_only_dir_renames_fold_parent_first_at_a_pass_scope() {
	let (outer, inner, file) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let baseline = Baseline::from_rows([
		base_dir("Docs", outer),
		base_dir("Docs/Sub", inner),
		base_file("Docs/Sub/a.txt", file, [1; 32]),
	]);
	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		// What a pass holds once it observed the renames: the old spellings gone, the new
		// ones walked.
		let mut local = Side::carried();
		let mut decided = BTreeSet::new();
		for (old, new) in [
			("Docs", "docs"),
			("Docs/Sub", "docs/sub"),
			("Docs/Sub/a.txt", "docs/sub/a.txt"),
		] {
			local.remove(&baseline, old);
			decided.extend([old.to_string(), new.to_string()]);
		}
		local.extend([
			("docs".to_string(), local_dir("docs")),
			("docs/sub".to_string(), local_dir("docs/sub")),
			(
				"docs/sub/a.txt".to_string(),
				local_file("docs/sub/a.txt", [1; 32]),
			),
		]);
		assert_eq!(
			plan_scoped(mode, &baseline, local, Side::carried(), decided),
			vec![
				SyncAction::MoveRemote {
					from_path: "Docs".to_string(),
					to_path: "docs".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: outer,
				},
				SyncAction::MoveRemote {
					from_path: "docs/Sub".to_string(),
					to_path: "docs/sub".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: inner,
				},
			],
			"{mode:?}"
		);
	}
}

/// A change-scoped pass folding a directory rename reads the renamed directory's rows a fixed
/// number of times, whatever it holds. On the first iteration: once for the move sources and
/// both sides' directories together, once for whether anything local is left at the source,
/// once for the source's signature, and once to check, path for path, that the new directory
/// whose signature it matched holds the same tree — which the twin check and the match of the
/// source reuse rather than read again. The second, which finds nothing, reads only what the
/// move did not prove unchanged: here the renamed directory itself, since the remote side
/// recorded nothing under it (see [`fold_dir_moves`]).
#[test]
fn a_scoped_directory_rename_fold_reads_the_renamed_rows_a_fixed_number_of_times() {
	const FILES: usize = 2_000;
	let dir = Uuid::new_v4();
	let files: Vec<(String, String)> = (0..FILES)
		.map(|n| (format!("dir/{n:05}.txt"), format!("moved_dir/{n:05}.txt")))
		.collect();
	let mut baseline = Baseline::from_rows(
		std::iter::once(base_dir("dir", dir)).chain(
			files
				.iter()
				.map(|(from, _)| base_file(from, Uuid::new_v4(), [1; 32])),
		),
	);
	// What a pass holds once it observed the rename: the old name gone, the new one walked.
	let (mut local, mut remote) = (Side::carried(), Side::carried());
	local.remove(&baseline, "dir");
	local.insert("moved_dir".to_string(), local_dir("moved_dir"));
	let mut decided = BTreeSet::from(["dir".to_string(), "moved_dir".to_string()]);
	for (from, to) in &files {
		local.remove(&baseline, from);
		local.insert(to.clone(), local_file(to, [1; 32]));
		decided.extend([from.clone(), to.clone()]);
	}
	let before = baseline.reads_for_test();
	let moves = fold_dir_moves(
		SyncMode::TwoWay,
		&mut baseline,
		&mut local,
		&mut remote,
		&BTreeSet::new(),
		Some(&mut decided),
	)
	.unwrap();
	let after = baseline.reads_for_test();
	assert_eq!(
		moves,
		vec![SyncAction::MoveRemote {
			from_path: "dir".to_string(),
			to_path: "moved_dir".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: dir,
		}]
	);
	let (statements, rows) = (after.0 - before.0, after.1 - before.1);
	assert!(
		rows < 9 * FILES / 2,
		"the fold read {rows} row(s) in {statements} statement(s): more than four and a half \
			 times the {FILES} file(s) it moved"
	);
}

/// A whole fold over staged sides of a directory renamed locally holds a digest per directory it
/// matches, not a copy of each one's subtree: 20,000 files in 200 directories, which as maps of
/// every path under every candidate were megabytes, fold holding under 1 MiB.
#[test]
fn a_staged_fold_of_a_pushed_rename_holds_no_copy_of_its_subtree() {
	const DIRS: u32 = 200;
	const FILES: u32 = 100;
	let root = Uuid::from_u128(1);
	let (mut rows, mut local, mut remote) = (
		vec![base_dir("a", root)],
		vec![local_dir("b")],
		vec![remote_dir_node("a", root)],
	);
	for dir in 0..DIRS {
		let (from, to) = (format!("a/d{dir:03}"), format!("b/d{dir:03}"));
		let dir_uuid = Uuid::from_u128(1_000 + u128::from(dir));
		rows.push(base_dir(&from, dir_uuid));
		local.push(local_dir(&to));
		remote.push(remote_dir_node(&from, dir_uuid));
		for file in 0..FILES {
			let (from, to) = (
				format!("{from}/f{file:03}.txt"),
				format!("{to}/f{file:03}.txt"),
			);
			let uuid = Uuid::from_u128(100_000 + u128::from(dir * FILES + file));
			let hash = [u8::try_from(file).expect("fewer than 256 files a directory"); 32];
			rows.push(base_file(&from, uuid, hash));
			local.push(local_file(&to, hash));
			remote.push(remote_file(&from, uuid, hash));
		}
	}
	let mut rows = Baseline::from_rows(rows);
	let (_stages, mut local, mut remote) = staged_sides(local, remote, 1024);
	let (moves, peak) = alloc_meter::peak_bytes(|| {
		fold_dir_moves(
			SyncMode::TwoWay,
			&mut rows,
			&mut local,
			&mut remote,
			&BTreeSet::new(),
			None,
		)
		.unwrap()
	});
	assert_eq!(
		moves,
		vec![SyncAction::MoveRemote {
			from_path: "a".to_string(),
			to_path: "b".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: root,
		}]
	);
	assert!(peak < 1024 * 1024, "the fold held {peak} bytes at once");
}

/// A whole fold over staged sides that finds no move holds nothing per directory: 20,000
/// synced directories, each where its row records it on both sides, fold holding the pages
/// its walks read and no list of the rows or of the remote's directories.
#[test]
fn a_staged_fold_that_finds_no_move_holds_nothing_per_directory() {
	const TOP: u32 = 200;
	const NESTED: u32 = 99;
	let (mut rows, mut local, mut remote) = (Vec::new(), Vec::new(), Vec::new());
	for top in 0..TOP {
		for nested in 0..=NESTED {
			let path = match nested {
				0 => format!("d{top:03}"),
				_ => format!("d{top:03}/e{nested:02}"),
			};
			let uuid = Uuid::from_u128(u128::from(top * 1_000 + nested) + 1);
			rows.push(base_dir(&path, uuid));
			local.push(local_dir(&path));
			remote.push(remote_dir_node(&path, uuid));
		}
	}
	assert_eq!(rows.len(), 20_000);
	let mut rows = Baseline::from_rows(rows);
	let (_stages, mut local, mut remote) = staged_sides(local, remote, 1024);
	for mode in ALL_MODES {
		let (moves, peak) = alloc_meter::peak_bytes(|| {
			fold_dir_moves(
				mode,
				&mut rows,
				&mut local,
				&mut remote,
				&BTreeSet::new(),
				None,
			)
			.unwrap()
		});
		assert_eq!(moves, Vec::new(), "{mode:?}");
		assert!(
			peak < 512 * 1024,
			"{mode:?}: the fold held {peak} bytes at once"
		);
	}
}

/// A remote move whose local subtree was edited meanwhile is still one move: the edit is
/// planned at the new path, after the move.
#[test]
fn a_child_edited_under_a_remotely_moved_dir_is_pushed_at_its_new_path() {
	let (baseline, mut local, remote) = moved_tree(TreeIds::new(), "docs", "documents");
	local.insert("docs/a.txt".to_string(), local_file("docs/a.txt", [9; 32]));
	assert_eq!(
		plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			},
			SyncAction::UploadFile {
				rel_path: "documents/a.txt".to_string(),
			},
		]
	);
}

/// A move inside a moved directory — the remote moved `docs/` and renamed `sub/` in it — is the
/// outer move first, then the inner one at the outer's new path, and nothing for the files.
#[test]
fn nested_dir_moves_fold_the_outer_one_first() {
	let ids = TreeIds::new();
	let (baseline, local, mut remote) = moved_tree(ids, "docs", "documents");
	let node = |path: &str| RemoteNode {
		rel_path: path.to_string(),
		..remote["documents/sub/b.txt"].clone()
	};
	let b = node("documents/renamed/b.txt");
	remote.remove("documents/sub/b.txt");
	remote.remove("documents/sub");
	remote.insert("documents/renamed/b.txt".to_string(), b);
	remote.insert(
		"documents/renamed".to_string(),
		remote_dir_node("documents/renamed", ids.sub),
	);
	assert_eq!(
		plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			SyncAction::MoveLocal {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
			},
			SyncAction::MoveLocal {
				from_path: "documents/sub".to_string(),
				to_path: "documents/renamed".to_string(),
				kind: NodeKind::Dir,
			},
		]
	);
}

/// One side renamed `z/` to `new/` and moved `b/` into it as `new/b`. Both are directory moves,
/// the outer one first: `b` sorts before `z`, and folding `b` first would put it where `z` is
/// going, leaving `z` to the per-path plan (a create, a move per file, and a deletion of `z`).
#[test]
fn a_dir_moved_into_another_moved_dir_folds_both_moves() {
	let (b, z) = (Uuid::new_v4(), Uuid::new_v4());
	let (b_file, z_file) = (Uuid::new_v4(), Uuid::new_v4());
	let baseline = map(vec![
		("b", base_dir("b", b)),
		("b/1.txt", base_file("b/1.txt", b_file, [1; 32])),
		("z", base_dir("z", z)),
		("z/2.txt", base_file("z/2.txt", z_file, [2; 32])),
	]);
	let paths = |z_at: &str, b_at: &str| {
		[
			z_at.to_string(),
			format!("{z_at}/2.txt"),
			b_at.to_string(),
			format!("{b_at}/1.txt"),
		]
	};
	let local = |z_at: &str, b_at: &str| {
		let [zd, zf, bd, bf] = paths(z_at, b_at);
		map(vec![
			(zd.as_str(), local_dir(&zd)),
			(zf.as_str(), local_file(&zf, [2; 32])),
			(bd.as_str(), local_dir(&bd)),
			(bf.as_str(), local_file(&bf, [1; 32])),
		])
	};
	let remote = |z_at: &str, b_at: &str| {
		let [zd, zf, bd, bf] = paths(z_at, b_at);
		map(vec![
			(zd.as_str(), remote_dir_node(&zd, z)),
			(zf.as_str(), remote_file(&zf, z_file, [2; 32])),
			(bd.as_str(), remote_dir_node(&bd, b)),
			(bf.as_str(), remote_file(&bf, b_file, [1; 32])),
		])
	};

	for mode in [SyncMode::TwoWay, SyncMode::RemoteToLocal] {
		assert_eq!(
			plan_with_renames(mode, &baseline, &local("z", "b"), &remote("new", "new/b")),
			vec![
				SyncAction::MoveLocal {
					from_path: "z".to_string(),
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
	}
	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		assert_eq!(
			plan_with_renames(mode, &baseline, &local("new", "new/b"), &remote("z", "b")),
			vec![
				SyncAction::MoveRemote {
					from_path: "z".to_string(),
					to_path: "new".to_string(),
					kind: NodeKind::Dir,
					remote_uuid: z,
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
	}
}

/// `mv docs documents; rm -r backup`, where `backup` held only a file `docs` holds too. The
/// subdirectory that moved along inside `docs` is not a move of its own, so `documents` without it
/// must not pass for `backup`: `docs` is the move and `backup` is trashed.
#[test]
fn a_deleted_dir_does_not_claim_a_renamed_dir_that_holds_it() {
	let (backup, docs, sub) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let (backup_readme, docs_readme, f) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
	let baseline = map(vec![
		("backup", base_dir("backup", backup)),
		(
			"backup/readme.md",
			base_file("backup/readme.md", backup_readme, [1; 32]),
		),
		("docs", base_dir("docs", docs)),
		(
			"docs/readme.md",
			base_file("docs/readme.md", docs_readme, [1; 32]),
		),
		("docs/sub", base_dir("docs/sub", sub)),
		("docs/sub/f", base_file("docs/sub/f", f, [2; 32])),
	]);
	let local = map(vec![
		("documents", local_dir("documents")),
		(
			"documents/readme.md",
			local_file("documents/readme.md", [1; 32]),
		),
		("documents/sub", local_dir("documents/sub")),
		("documents/sub/f", local_file("documents/sub/f", [2; 32])),
	]);
	let remote = map(vec![
		("backup", remote_dir_node("backup", backup)),
		(
			"backup/readme.md",
			remote_file("backup/readme.md", backup_readme, [1; 32]),
		),
		("docs", remote_dir_node("docs", docs)),
		(
			"docs/readme.md",
			remote_file("docs/readme.md", docs_readme, [1; 32]),
		),
		("docs/sub", remote_dir_node("docs/sub", sub)),
		("docs/sub/f", remote_file("docs/sub/f", f, [2; 32])),
	]);
	for mode in [SyncMode::TwoWay, SyncMode::LocalToRemote] {
		let actions = plan_with_renames(mode, &baseline, &local, &remote);
		assert_eq!(
			dir_moves_in(&actions),
			vec![&SyncAction::MoveRemote {
				from_path: "docs".to_string(),
				to_path: "documents".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: docs,
			}],
			"{mode:?}: {actions:?}"
		);
		assert!(
			actions.contains(&SyncAction::TrashRemote {
				rel_path: "backup".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: backup,
			}),
			"{mode:?}: {actions:?}"
		);
	}
}

/// A pushed move that re-parented the directory and then failed to rename it leaves the remote at
/// the new parent under the old name. With the rows recorded there, the next pass plans the rename
/// alone; with the rows still at the source, it finds the directory at neither end and plans no
/// move at all — which is why apply records the half-done step.
#[test]
fn a_half_done_local_dir_move_is_finished_by_the_next_pass() {
	let ids = TreeIds::new();
	let archive = Uuid::new_v4();
	let tree = |baseline_at: &str| {
		let mut baseline: HashMap<_, _> = ids.baseline(baseline_at).into_iter().collect();
		baseline.insert("archive".to_string(), base_dir("archive", archive));
		let mut local: HashMap<_, _> = TreeIds::local("archive/documents").into_iter().collect();
		local.insert("archive".to_string(), local_dir("archive"));
		let mut remote: HashMap<_, _> = ids.remote("archive/docs").into_iter().collect();
		remote.insert("archive".to_string(), remote_dir_node("archive", archive));
		(baseline, local, remote)
	};

	let (baseline, local, remote) = tree("archive/docs");
	assert_eq!(
		plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::MoveRemote {
			from_path: "archive/docs".to_string(),
			to_path: "archive/documents".to_string(),
			kind: NodeKind::Dir,
			remote_uuid: ids.dir,
		}]
	);

	let (baseline, local, remote) = tree("docs");
	let actions = plan_with_renames(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(dir_moves_in(&actions).is_empty(), "{actions:?}");
}

/// A moved directory is re-parented before it is renamed, so the name it has in between must be
/// free on the remote too.
#[test]
fn a_local_dir_move_whose_intermediate_name_is_taken_is_not_planned() {
	let (baseline, local, mut remote) = moved_tree(TreeIds::new(), "archive/documents", "docs");
	remote.insert(
		"archive/docs".to_string(),
		remote_dir_node("archive/docs", Uuid::new_v4()),
	);
	let actions = plan_with_renames(SyncMode::LocalToRemote, &baseline, &local, &remote);
	// The unchanged `sub` inside may still move on its own, under the new `archive/documents`.
	assert!(
		moves_onto(&actions, "archive/documents").is_none(),
		"{actions:?}"
	);
}

fn ms(millis: i64) -> DateTime<Utc> {
	DateTime::from_timestamp_millis(millis).unwrap()
}

/// A remote file item with a fresh uuid, named `name` under `parent`.
fn cacheable_file(parent: Uuid, name: &'static str) -> RemoteItem {
	let uuid = Uuid::new_v4();
	RemoteItem {
		uuid,
		parent,
		name: name.to_string(),
		stable_uuid: Some(filen_types::fs::StableUuid::new_for_test(uuid)),
		hash: Some(Blake3Hash::from([5; 32])),
		size: 5,
		modified_millis: 7,
	}
}

fn local_file(rel: &str, hash: [u8; 32]) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		size: 10,
		mtime_millis: 1,
		content_hash: Some(Blake3Hash::from(hash)),
	}
}

fn local_dir(rel: &str) -> LocalNode {
	LocalNode {
		rel_path: rel.to_string(),
		kind: NodeKind::Dir,
		size: 0,
		mtime_millis: 1,
		content_hash: None,
	}
}

/// A remote file whose lineage id is its own uuid — the shape a file that has never been
/// re-uploaded has. [`remote_version`] models a later version of the same lineage.
fn remote_file(rel: &str, uuid: Uuid, hash: [u8; 32]) -> RemoteNode {
	RemoteNode {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: uuid,
		stable_uuid: Some(StableUuid::new_for_test(uuid)),
		content_hash: Some(Blake3Hash::from(hash)),
		size: 10,
		modified_millis: 1,
	}
}

/// A new VERSION of the file whose lineage is `lineage`: a fresh uuid, the same whole-life id.
fn remote_version(rel: &str, lineage: Uuid, uuid: Uuid, hash: [u8; 32]) -> RemoteNode {
	RemoteNode {
		stable_uuid: Some(StableUuid::new_for_test(lineage)),
		..remote_file(rel, uuid, hash)
	}
}

fn remote_dir_node(rel: &str, uuid: Uuid) -> RemoteNode {
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

fn base_dir(rel: &str, uuid: Uuid) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel.to_string(),
		kind: NodeKind::Dir,
		remote_uuid: Some(uuid),
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
	}
}

fn base_file(rel: &str, uuid: Uuid, hash: [u8; 32]) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid),
		content_hash: Some(Blake3Hash::from(hash)),
		size: Some(10),
		local_mtime: Some(1),
		remote_modified: Some(1),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		// The ordinary shape: what the row records is what both sides were last known to hold.
		agreed_hash: Some(Blake3Hash::from(hash)),
	}
}

/// The row a push left behind: our content on top of a DIFFERENT agreed one, and no snapshot
/// has confirmed the push since.
fn base_file_pushed(rel: &str, uuid: Uuid, hash: [u8; 32], agreed: [u8; 32]) -> BaselineEntry {
	BaselineEntry {
		agreed_hash: Some(Blake3Hash::from(agreed)),
		..base_file(rel, uuid, hash)
	}
}

/// Rule (d), the case it exists for: our push is unconfirmed and a foreign version of the same
/// file now holds the path with different bytes. Both sides moved off the agreed content, so
/// this is a two-way conflict rather than a pull that would bury our copy.
#[test]
fn a_foreign_edit_over_an_unconfirmed_push_is_a_conflict() {
	let lineage = Uuid::new_v4();
	// The row: we pushed [1;32] on top of the agreed [0;32]; nothing has confirmed it.
	let baseline = map(vec![(
		"a.txt",
		base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}],
		"a concurrent edit of a file we just pushed must be surfaced, not pulled over"
	);
}

/// The negative: once a pass has confirmed our push (agreed == what the row records), a foreign
/// version on top of it is an edit made AFTER ours and pulls exactly as it always did.
#[test]
fn a_foreign_edit_over_a_confirmed_push_still_pulls() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [1; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
	)]);
	let new_uuid = remote["a.txt"].remote_uuid;
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: new_uuid,
		}]
	);
}

/// No agreed content on record (a file this side created and pushed, nothing confirmed since)
/// is an UNCONFIRMED push, not consent: a foreign version on top of it is surfaced, not pulled
/// over. This is what makes two clients creating the same name concurrently non-silent — both
/// hold exactly this row, so with `None` read as agreement the loser's copy would be buried.
#[test]
fn a_foreign_edit_with_no_agreed_content_recorded_is_a_conflict() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![(
		"a.txt",
		BaselineEntry {
			agreed_hash: None,
			..base_file("a.txt", lineage, [1; 32])
		},
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}]
	);
}

/// A DIFFERENT file taking the path over is a replacement, not a concurrent edit of ours: the
/// destination has to converge on whatever holds the path, so it pulls.
#[test]
fn a_replacement_over_an_unconfirmed_push_is_not_a_concurrent_edit() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![(
		"a.txt",
		base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_file("a.txt", Uuid::new_v4(), [2; 32]),
	)]);
	let other_uuid = remote["a.txt"].remote_uuid;
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: other_uuid,
		}]
	);
}

/// A foreign version carrying the SAME bytes we pushed is nobody diverging — the other client
/// wrote what we already hold. Nothing happens at all.
#[test]
fn a_content_equal_foreign_version_over_an_unconfirmed_push_is_no_conflict() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![(
		"a.txt",
		base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [1; 32]),
	)]);
	assert!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote).is_empty(),
		"identical bytes under a new version id are not a divergence"
	);
}

/// The one-way modes are untouched: each has an authoritative side and no divergence to
/// surface, so the same inputs re-push (local-authoritative) or pull (remote-authoritative).
#[test]
fn one_way_modes_ignore_the_agreed_content_marker() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![(
		"a.txt",
		base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
	)]);
	let new_uuid = remote["a.txt"].remote_uuid;
	for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
		assert_eq!(
			plan(mode, &baseline, &local, &remote),
			vec![SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			}],
			"{mode:?} must re-push the local copy"
		);
	}
	for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
		assert_eq!(
			plan(mode, &baseline, &local, &remote),
			vec![SyncAction::DownloadFile {
				rel_path: "a.txt".to_string(),
				remote_uuid: new_uuid,
			}],
			"{mode:?} must pull the remote copy"
		);
	}
}

/// Rule (c): the snapshot listing our own version at the path is what makes the row's content
/// agreed. Only the rows that actually moved come back, so only those are persisted.
#[test]
fn the_raw_snapshot_showing_our_version_advances_the_agreed_content() {
	let uuid = Uuid::new_v4();
	let mut baseline = tree(&map(vec![(
		"a.txt",
		base_file_pushed("a.txt", uuid, [1; 32], [0; 32]),
	)]));
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [1; 32]))]);

	let advanced = confirm_agreed_content(&mut baseline, &remote);
	assert_eq!(
		advanced
			.iter()
			.map(|e| e.rel_path.as_str())
			.collect::<Vec<_>>(),
		vec!["a.txt"],
		"the confirmed row is handed back to be persisted"
	);
	assert_eq!(
		baseline.get("a.txt").unwrap().agreed_hash,
		Some(Blake3Hash::from([1; 32])),
		"the marker moved onto what the row records"
	);
	// Idempotent: a second pass over an already-agreed row has nothing to persist.
	assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
}

/// A foreign version at the path proves nothing about our push, and neither does an empty
/// snapshot: the marker stays where it was.
#[test]
fn a_path_the_snapshot_does_not_confirm_leaves_the_agreed_content_alone() {
	let ours = Uuid::new_v4();
	let row = base_file_pushed("a.txt", ours, [1; 32], [0; 32]);
	for remote in [
		map(vec![(
			"a.txt",
			remote_version("a.txt", ours, Uuid::new_v4(), [2; 32]),
		)]),
		HashMap::new(),
	] {
		let mut baseline = tree(&map(vec![("a.txt", row.clone())]));
		assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
		assert_eq!(
			baseline.get("a.txt").unwrap().agreed_hash,
			Some(Blake3Hash::from([0; 32])),
			"an unconfirmed push must not advance its own marker"
		);
	}
}

/// A conflicted row is a holding cell for a divergence, not a converged state — confirming it
/// would claim both sides agree on the loser's content.
#[test]
fn a_conflicted_row_is_never_confirmed() {
	let uuid = Uuid::new_v4();
	let mut baseline = tree(&map(vec![(
		"a.txt",
		BaselineEntry {
			state: BaselineState::Conflicted,
			..base_file_pushed("a.txt", uuid, [1; 32], [0; 32])
		},
	)]));
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [1; 32]))]);
	assert!(confirm_agreed_content(&mut baseline, &remote).is_empty());
}

/// The tenure rule's other half: evidence from OUTSIDE the snapshot (the cache's announcement
/// timestamps, or the server's version chain) names the version uuids it vouches for, and the
/// same row rule applies to them.
#[test]
fn a_push_some_other_evidence_vouches_for_advances_the_agreed_content() {
	let uuid = Uuid::new_v4();
	let mut baseline = tree(&map(vec![
		("a.txt", base_file_pushed("a.txt", uuid, [1; 32], [0; 32])),
		(
			"b.txt",
			base_file_pushed("b.txt", Uuid::new_v4(), [2; 32], [0; 32]),
		),
	]));
	let advanced = confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid]));

	assert_eq!(
		advanced
			.iter()
			.map(|e| e.rel_path.as_str())
			.collect::<Vec<_>>(),
		vec!["a.txt"],
		"only the vouched-for version's row moves"
	);
	let advanced_row = baseline.get("a.txt").unwrap();
	assert_eq!(advanced_row.agreed_hash, advanced_row.content_hash);
	assert_eq!(
		baseline.get("b.txt").unwrap().agreed_hash,
		Some(Blake3Hash::from([0; 32])),
		"a row nothing vouched for is left alone"
	);
}

#[test]
fn a_row_that_is_not_an_unconfirmed_push_is_never_advanced_by_tenure() {
	let uuid = Uuid::new_v4();
	for row in [
		// Already agreed.
		base_file_pushed("a.txt", uuid, [1; 32], [1; 32]),
		// Held in conflict.
		BaselineEntry {
			state: BaselineState::Conflicted,
			..base_file_pushed("a.txt", uuid, [1; 32], [0; 32])
		},
		// A directory has no content for the two sides to agree on.
		base_dir("a.txt", uuid),
	] {
		let agreed = row.agreed_hash;
		let mut baseline = tree(&map(vec![("a.txt", row)]));
		assert!(confirm_agreed_pushes(&mut baseline, &HashSet::from([uuid])).is_empty());
		assert_eq!(baseline.get("a.txt").unwrap().agreed_hash, agreed);
	}
}

/// The server-side evidence: how long our version stood before the one that superseded it.
#[test]
fn the_version_chain_dates_a_push_by_what_landed_on_top_of_it() {
	let tenure = std::time::Duration::from_secs(30);
	let (ours, foreign, older) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

	// Superseded well after the window: an edit made against our version, so pulling it is right.
	let chain = vec![(foreign, ms(100_000)), (ours, ms(60_000)), (older, ms(0))];
	assert_eq!(
		version_chain_verdict(&chain, ours, foreign, tenure),
		Some(true)
	);

	// Superseded INSIDE the window: the other client was editing at the same time.
	let chain = vec![(foreign, ms(65_000)), (ours, ms(60_000)), (older, ms(0))];
	assert_eq!(
		version_chain_verdict(&chain, ours, foreign, tenure),
		Some(false)
	);

	// A second foreign edit later on does not undo the first. What buried our version is
	// whatever landed on it FIRST, so the row stays unconfirmed even though the version the
	// snapshot now shows is minutes newer.
	let latest = Uuid::new_v4();
	let chain = vec![
		(latest, ms(100_000)),
		(foreign, ms(65_000)),
		(ours, ms(60_000)),
		(older, ms(0)),
	];
	assert_eq!(
		version_chain_verdict(&chain, ours, latest, tenure),
		Some(false),
		"the edit that buried ours landed inside the window; a later one on top of THAT says \
			 nothing about ours"
	);

	// The same stamp on both — a race the server could not separate — is no tenure at all,
	// whichever way the listing happens to order them.
	let chain = vec![(ours, ms(60_000)), (foreign, ms(60_000))];
	assert_eq!(
		version_chain_verdict(&chain, ours, foreign, tenure),
		Some(false)
	);

	// A chain missing either version (versioning disabled) is no evidence at all.
	assert_eq!(
		version_chain_verdict(&[(foreign, ms(65_000))], ours, foreign, tenure),
		None
	);
}

/// The pusher's side of a concurrent edit: what our own upload landed on top of.
#[test]
fn an_upload_that_landed_on_a_version_this_pass_never_saw_reports_it() {
	let (ours, replaced, stranger) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());

	// The ordinary push: ours is the only version as recent as the one the snapshot showed.
	assert_eq!(
		interleaved_version(
			&[(ours, ms(60_000)), (replaced, ms(30_000))],
			ours,
			replaced
		),
		None
	);

	// Somebody else's edit landed on it too — our bytes are on top of it and nothing else this
	// pass can see says so.
	let raced = [
		(ours, ms(60_000)),
		(stranger, ms(45_000)),
		(replaced, ms(30_000)),
	];
	assert_eq!(
		interleaved_version(&raced, ours, replaced),
		Some(stranger),
		"the version that landed on the one this pass saw is the one we buried"
	);

	// The same, with the racing pair stamped identically so the listing orders them the other
	// way round: the answer comes from the stamps, so it does not move.
	let tied = [
		(stranger, ms(60_000)),
		(ours, ms(60_000)),
		(replaced, ms(30_000)),
	];
	assert_eq!(interleaved_version(&tied, ours, replaced), Some(stranger));

	// A version stamped in the very second of the one we replaced is NOT reported: inside one
	// second the chain cannot say which came first, and our own previous upload lands there
	// often enough that guessing would hold a conflict on a path nobody else touched. The cost
	// is a race that tight going unreported on this side — its author still surfaces it.
	let tied_with_replaced = [
		(ours, ms(60_000)),
		(stranger, ms(45_000)),
		(replaced, ms(45_000)),
	];
	assert_eq!(
		interleaved_version(&tied_with_replaced, ours, replaced),
		None
	);

	// Versions older than the one the snapshot showed are the file's history, not an interleave.
	let older = [
		(ours, ms(60_000)),
		(replaced, ms(45_000)),
		(stranger, ms(10_000)),
	];
	assert_eq!(interleaved_version(&older, ours, replaced), None);

	// A chain that does not carry the version we replaced is no evidence about it.
	assert_eq!(
		interleaved_version(&[(ours, ms(60_000))], ours, replaced),
		None
	);
	assert_eq!(
		interleaved_version(
			&[(stranger, ms(45_000)), (replaced, ms(30_000))],
			ours,
			replaced
		),
		Some(stranger),
		"a version on top of the one we replaced counts even when ours is no longer listed"
	);
}

#[test]
fn a_local_rename_becomes_a_remote_move_not_a_re_upload() {
	let uuid = Uuid::new_v4();
	// Baseline + remote still have the file at a.txt; locally it now lives at b.txt (same hash).
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
	assert_eq!(
		plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
		vec![SyncAction::MoveRemote {
			from_path: "a.txt".to_string(),
			to_path: "b.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		}],
		"a local move re-parents the remote item, not trash + re-upload"
	);
}

#[test]
fn a_remote_rename_becomes_a_local_move_not_a_re_download() {
	let uuid = Uuid::new_v4();
	// Baseline + local have it at a.txt; the remote now carries the SAME uuid at b.txt.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
	let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);
	assert_eq!(
		plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
		vec![SyncAction::MoveLocal {
			from_path: "a.txt".to_string(),
			to_path: "b.txt".to_string(),
			kind: NodeKind::File,
		}],
		"a remote move renames the local file, not delete + re-download"
	);
}

#[test]
fn a_re_upload_of_identical_bytes_is_not_read_as_a_remote_edit() {
	// Another client (or a version restore) put the SAME content back under a new version uuid,
	// while the local side genuinely edited the file. Reading the re-mint as a remote edit would
	// invent a conflict with no second side; only the local edit actually happened, so it pushes.
	let lineage = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [0; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [0; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::UploadFile {
			rel_path: "a.txt".to_string(),
		}],
		"a re-mint that changed no bytes is not a remote change"
	);
}

#[test]
fn a_new_version_of_the_same_file_is_still_an_edit_and_a_different_file_is_still_a_change() {
	let lineage = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [0; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [0; 32]))]);

	// Same lineage, new version, DIFFERENT bytes: an edit — pulled.
	let edited = map(vec![(
		"a.txt",
		remote_version("a.txt", lineage, Uuid::new_v4(), [2; 32]),
	)]);
	let new_uuid = edited["a.txt"].remote_uuid;
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &edited),
		vec![SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: new_uuid,
		}]
	);

	// A DIFFERENT lineage at the same path: a replacement — converged on the same way.
	let replaced = map(vec![(
		"a.txt",
		remote_file("a.txt", Uuid::new_v4(), [2; 32]),
	)]);
	let other_uuid = replaced["a.txt"].remote_uuid;
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &replaced),
		vec![SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: other_uuid,
		}]
	);
}

#[test]
fn a_remote_move_that_carried_an_edit_stays_one_item_and_pulls_the_new_version() {
	// The other side renamed a.txt -> b.txt AND edited it before this pass ran, so the uuid the
	// baseline recorded names nothing live. Matched on the lineage id, this is one file that
	// moved and changed — a local rename plus a pull — not a local deletion and an unrelated
	// download (which the mass-delete guard would hold, and a backup mode would refuse).
	let lineage = Uuid::new_v4();
	let new_uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
	let remote = map(vec![(
		"b.txt",
		remote_version("b.txt", lineage, new_uuid, [6; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			SyncAction::MoveLocal {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
			},
			SyncAction::DownloadFile {
				rel_path: "b.txt".to_string(),
				remote_uuid: new_uuid,
			},
		],
		"the move and the edit it carried are applied together"
	);
}

/// The guard is about a local CHANGE, not about a row that happens to record no local content.
/// A `KeepLocal` resolution anchors the row to the remote and clears the local half on purpose,
/// so the row sits at `content_hash: None` until the re-push. Comparing a hash against `None`
/// reports "changed" for every such row, and a remote rename landing in that window would then
/// be refused as a move: a second conflict at the source plus a download at the new name,
/// instead of the rename the remote actually made.
#[test]
fn a_remote_rename_before_a_kept_local_re_push_is_still_a_move() {
	let uuid = Uuid::new_v4();
	// The row a KeepLocal resolution leaves: the remote's anchor, no local evidence at all.
	let kept = BaselineEntry {
		content_hash: None,
		size: None,
		local_mtime: None,
		agreed_hash: None,
		..base_file("a.txt", uuid, [5; 32])
	};
	let baseline = map(vec![("a.txt", kept)]);
	// The local copy still holds the bytes the caller kept; the remote renamed its own (still
	// diverged) copy a.txt -> b.txt, same uuid.
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [2; 32]))]);

	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::MoveLocal {
			from_path: "a.txt".to_string(),
			to_path: "b.txt".to_string(),
			kind: NodeKind::File,
		}],
		"the rename must be carried across, with nothing else planned at either path"
	);

	// The pending push follows on the NEXT pass, at the new name. Planned against the row the
	// apply step actually writes for this move — not a hand-made one: the row's local half is
	// what decides whether the scanner even looks at the file again, so a test that invented it
	// could pass over an engine that never pushes the kept copy at all.
	let moved = map(vec![(
		"b.txt",
		super::super::apply::moved_file_row(
			"b.txt",
			baseline.get("a.txt"),
			remote.get("b.txt"),
			Some(1),
		),
	)]);
	assert_eq!(
		moved["b.txt"].content_hash, None,
		"the moved row records no local content, so the scanner must re-hash at the new path"
	);
	let local_moved = map(vec![("b.txt", local_file("b.txt", [1; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &moved, &local_moved, &remote),
		vec![SyncAction::UploadFile {
			rel_path: "b.txt".to_string(),
		}],
		"the kept local copy must be pushed at its new name"
	);
}

/// The other half of that guard. "No local content on record" excuses the row from being read as
/// a local EDIT, but it is not agreement either: the copy a `KeepLocal` resolution kept is
/// diverged from what the remote holds, by construction. So a remote change that renamed AND
/// edited the file is not carried across it — the rename would put the kept bytes under the
/// paired download, which is the loss the unconfirmed-push rule exists to stop, one rename away.
/// Same outcome as that rule's: the source surfaces, and the moved version arrives untracked
/// under its new name.
#[test]
fn a_remote_rename_that_carried_an_edit_is_not_moved_onto_a_kept_local_copy() {
	let lineage = Uuid::new_v4();
	let new_uuid = Uuid::new_v4();
	let kept = BaselineEntry {
		content_hash: None,
		size: None,
		local_mtime: None,
		agreed_hash: None,
		..base_file("a.txt", lineage, [5; 32])
	};
	let baseline = map(vec![("a.txt", kept)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"b.txt",
		remote_version("b.txt", lineage, new_uuid, [2; 32]),
	)]);

	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
		"the kept copy must not be renamed under the download: {actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}),
		"the divergence must be surfaced: {actions:?}"
	);
	assert!(
		actions.iter().any(|a| matches!(
			a,
			SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
		)),
		"the moved version still arrives under its new name: {actions:?}"
	);
}

/// The same guard on the UUID-matched branch: a remote move whose content did NOT change is
/// still not a move when the local copy at its source was edited. Consuming it would rename the
/// edited file to the destination and pull the remote's bytes over it — and a same-size edit
/// satisfies the scanner's (size, mtime) fast path afterwards, so the loss is never noticed.
#[test]
fn a_local_edit_at_a_uuid_matched_remote_moves_source_is_not_a_move() {
	let uuid = Uuid::new_v4();
	// The remote renamed a.txt -> b.txt (same uuid, same bytes); a.txt was edited locally.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
	let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);

	// TwoWay: both sides changed the same item, so the source is held for the caller and the
	// moved version still arrives under its new name.
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
		"an edited local copy must not be renamed out from under the edit: {actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}),
		"the divergence must be surfaced: {actions:?}"
	);
	assert!(
		actions.iter().any(|a| matches!(
			a,
			SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
		)),
		"the moved version still arrives at its new name: {actions:?}"
	);

	// RemoteToLocal: the remote is authoritative, so the local edit is quarantined by the
	// deletion of the source rather than carried to the destination and overwritten there.
	assert_eq!(
		plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
		vec![
			SyncAction::DownloadFile {
				rel_path: "b.txt".to_string(),
				remote_uuid: uuid,
			},
			SyncAction::DeleteLocal {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
			},
		]
	);

	// RemoteBackup never deletes on its destination, so the edited file simply stays put.
	assert_eq!(
		plan(SyncMode::RemoteBackup, &baseline, &local, &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "b.txt".to_string(),
			remote_uuid: uuid,
		}]
	);
}

#[test]
fn a_local_edit_stops_a_lineage_move_from_laundering_the_divergence() {
	// Same remote move+edit, but the local copy diverged too. Consuming that as a move would
	// rename the local file and pull straight over it — a both-sides conflict silently resolved
	// in the remote's favour. It must fall through to the ordinary reconcile instead.
	let lineage = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
	let remote = map(vec![(
		"b.txt",
		remote_version("b.txt", lineage, Uuid::new_v4(), [6; 32]),
	)]);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
		"a diverged local copy must not be consumed as a move: {actions:?}"
	);
}

/// The move branch reads the same rule as the per-path reconcile. A foreign version of our own
/// lineage that moved AND edited over a push nothing confirmed is a divergence, and consuming
/// it as a move would rename our unconfirmed copy to the destination for the paired download to
/// write over — the same silent loss the rule exists to stop, just one rename away. Leaving the
/// pair alone is not enough either: the source would then read as a remote-side deletion and be
/// deleted locally. The source is surfaced, and the moved version still arrives at its new name.
#[test]
fn a_lineage_move_over_an_unconfirmed_push_surfaces_instead_of_moving() {
	let lineage = Uuid::new_v4();
	let new_uuid = Uuid::new_v4();
	// We pushed [1;32] over the agreed [0;32]; nothing confirmed it. The other client edited
	// the same file to [2;32] and renamed it in the same window.
	let baseline = map(vec![(
		"a.txt",
		base_file_pushed("a.txt", lineage, [1; 32], [0; 32]),
	)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"b.txt",
		remote_version("b.txt", lineage, new_uuid, [2; 32]),
	)]);

	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
		"our unconfirmed copy must not be renamed under the download: {actions:?}"
	);
	assert!(
		!actions.iter().any(|a| matches!(
			a,
			SyncAction::DeleteLocal { rel_path, .. } if rel_path == "a.txt"
		)),
		"nor deleted as though the remote had dropped it: {actions:?}"
	);
	assert!(
		actions.contains(&SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}),
		"the divergence must be surfaced: {actions:?}"
	);
	assert!(
		actions.iter().any(|a| matches!(
			a,
			SyncAction::DownloadFile { rel_path, .. } if rel_path == "b.txt"
		)),
		"the moved version still arrives under its new name: {actions:?}"
	);
}

/// The negative, so the guard cannot swallow the ordinary move+edit: with the push confirmed
/// (or nothing pushed at all) the pair is still carried across as one item.
#[test]
fn a_lineage_move_over_a_confirmed_push_is_still_a_move() {
	let lineage = Uuid::new_v4();
	let new_uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", lineage, [1; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"b.txt",
		remote_version("b.txt", lineage, new_uuid, [2; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![
			SyncAction::MoveLocal {
				from_path: "a.txt".to_string(),
				to_path: "b.txt".to_string(),
				kind: NodeKind::File,
			},
			SyncAction::DownloadFile {
				rel_path: "b.txt".to_string(),
				remote_uuid: new_uuid,
			},
		]
	);
}

#[test]
fn a_rename_in_a_backup_mode_moves_the_destination_instead_of_copying_it() {
	let uuid = Uuid::new_v4();
	// LocalBackup: the source (local) renamed a.txt -> b.txt. The remote backup follows with a
	// metadata-only re-parent/rename, not an upload of b.txt with a.txt left behind.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
	assert_eq!(
		plan(SyncMode::LocalBackup, &baseline, &local, &remote),
		vec![SyncAction::MoveRemote {
			from_path: "a.txt".to_string(),
			to_path: "b.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		}],
		"a backup destination follows a source rename instead of accumulating both names"
	);

	// RemoteBackup: the mirror image — the source (remote) renamed, the local backup follows.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
	let remote = map(vec![("b.txt", remote_file("b.txt", uuid, [5; 32]))]);
	assert_eq!(
		plan(SyncMode::RemoteBackup, &baseline, &local, &remote),
		vec![SyncAction::MoveLocal {
			from_path: "a.txt".to_string(),
			to_path: "b.txt".to_string(),
			kind: NodeKind::File,
		}]
	);
}

#[test]
fn a_backup_mode_still_never_propagates_a_real_deletion() {
	// The distinction move detection must preserve: a rename is a move, but a deletion with no
	// new home for the content stays suppressed on a backup destination.
	let uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
	assert!(
		plan(SyncMode::LocalBackup, &baseline, &HashMap::new(), &remote).is_empty(),
		"a local deletion must not reach a remote backup"
	);
	let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
	assert!(
		plan(SyncMode::RemoteBackup, &baseline, &local, &HashMap::new()).is_empty(),
		"a remote deletion must not reach a local backup"
	);
}

#[test]
fn ambiguous_content_falls_back_to_delete_plus_create() {
	let uuid = Uuid::new_v4();
	// a.txt vanished locally, but TWO new local files share its content — no unambiguous move.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![
		("b.txt", local_file("b.txt", [5; 32])),
		("c.txt", local_file("c.txt", [5; 32])),
	]);
	let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveRemote { .. })),
		"ambiguous content must NOT be guessed as a move: {actions:?}"
	);
	assert!(
		actions
			.iter()
			.any(|a| matches!(a, SyncAction::TrashRemote { .. })),
		"a.txt is instead trashed + the new files uploaded"
	);
}

/// A change-scoped two-way pass asks both sides about each of its paths in one walk to build
/// move detection's two target maps, so the rows at those paths are read once for both maps and
/// once more for the move sources. After a directory rename those paths are every row under the
/// new name, and both sides need the row there: the local side to learn the remote holds each
/// file it found, the remote side for the node the row carries.
#[test]
fn a_scoped_move_detection_reads_the_decided_rows_once_for_both_target_maps() {
	const FILES: usize = 2_000;
	let paths: Vec<String> = (0..FILES)
		.map(|n| format!("moved_dir/{n:05}.txt"))
		.collect();
	let baseline = Baseline::from_rows(
		std::iter::once(base_dir("moved_dir", Uuid::new_v4())).chain(
			paths
				.iter()
				.map(|path| base_file(path, Uuid::new_v4(), [1; 32])),
		),
	);
	// The local nodes a pass found walking the renamed directory, over rows that carry the
	// remote's.
	let (mut local, remote) = (Side::carried(), Side::carried());
	local.insert("moved_dir".to_string(), local_dir("moved_dir"));
	let mut decided = BTreeSet::from(["moved_dir".to_string()]);
	for path in &paths {
		local.insert(path.clone(), local_file(path, [1; 32]));
		decided.insert(path.clone());
	}
	let (mut actions, mut consumed) = (Vec::new(), HashSet::new());
	let before = baseline.reads_for_test();
	detect_moves(
		SyncMode::TwoWay,
		&baseline,
		&local.of(&baseline),
		&remote.of(&baseline),
		PassPaths::Changed(&decided),
		&mut actions,
		&mut consumed,
	);
	let after = baseline.reads_for_test();
	assert!(actions.is_empty(), "{actions:?}");
	let (statements, rows) = (after.0 - before.0, after.1 - before.1);
	assert!(
		rows < 5 * FILES / 2,
		"move detection read {rows} row(s) in {statements} statement(s): more than twice the \
			 {FILES} file(s) under the renamed directory"
	);
}

/// The content of the `n`th file of a generated tree: distinct for every file.
fn nth_hash(n: usize) -> [u8; 32] {
	let mut hash = [0; 32];
	hash[..8].copy_from_slice(&u64::try_from(n).unwrap().to_le_bytes());
	hash
}

/// The paths of a generated tree of `files` files, a thousand directories wide.
fn nth_paths(files: usize) -> Vec<String> {
	(0..files)
		.map(|n| format!("d{:03}/f{n:06}.txt", n % 1_000))
		.collect()
}

/// The rows of a generated tree of `files` synced files, the `n`th recording the uuid `n + 1`,
/// and a remote holding each file where its row says.
fn nth_synced(files: usize) -> (Vec<String>, Baseline, HashMap<String, RemoteNode>) {
	let paths = nth_paths(files);
	let uuid = |n: usize| Uuid::from_u128(1 + u128::try_from(n).unwrap());
	let baseline = Baseline::from_rows(
		paths
			.iter()
			.enumerate()
			.map(|(n, path)| base_file(path, uuid(n), nth_hash(n))),
	);
	let remote = paths
		.iter()
		.enumerate()
		.map(|(n, path)| (path.clone(), remote_file(path, uuid(n), nth_hash(n))))
		.collect();
	(paths, baseline, remote)
}

/// A local side holding every file of a generated tree, with the content its row records.
fn nth_local(paths: &[String]) -> HashMap<String, LocalNode> {
	paths
		.iter()
		.enumerate()
		.map(|(n, path)| (path.clone(), local_file(path, nth_hash(n))))
		.collect()
}

/// A first sync has a target for every new file and no row a move could start from, so move
/// detection learns that there is nothing to join before it indexes a single target.
#[test]
fn move_detection_on_a_first_sync_indexes_no_target() {
	const FILES: usize = 100_000;
	let baseline = Baseline::from_rows(std::iter::empty());
	let local = nth_local(&nth_paths(FILES));
	let remote: HashMap<String, RemoteNode> = HashMap::new();
	let (mut actions, mut consumed) = (Vec::new(), HashSet::new());
	let ((), peak) = alloc_meter::peak_bytes(|| {
		detect_moves(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			PassPaths::Whole,
			&mut actions,
			&mut consumed,
		);
	});
	assert_eq!(actions, Vec::new());
	assert!(
		peak < 64 * 1024,
		"move detection held {peak} bytes at once over {FILES} new files and no row"
	);
}

/// A converged pass has a target for every remote file and no row whose file left its path,
/// so move detection keeps nothing per file there either, in every mode that pulls: it reads
/// the rows to learn there is no source, and indexes nothing.
#[test]
fn move_detection_over_a_converged_tree_indexes_no_target() {
	const FILES: usize = 100_000;
	let (paths, baseline, remote) = nth_synced(FILES);
	let local = nth_local(&paths);
	for mode in [
		SyncMode::TwoWay,
		SyncMode::RemoteToLocal,
		SyncMode::RemoteBackup,
	] {
		let (mut actions, mut consumed) = (Vec::new(), HashSet::new());
		let ((), peak) = alloc_meter::peak_bytes(|| {
			detect_moves(
				mode,
				&baseline,
				&local,
				&remote,
				PassPaths::Whole,
				&mut actions,
				&mut consumed,
			);
		});
		assert_eq!(actions, Vec::new(), "{mode:?}");
		// The walk of the rows holds two of the store's 256-row pages at once, about 125 KiB
		// here: the floor of any walk of them, where an index of the targets is megabytes.
		assert!(
			peak < 192 * 1024,
			"{mode:?}: move detection held {peak} bytes at once over {FILES} converged files"
		);
	}
}

/// A local mass deletion beside one file moved under a new name has far more rows to move from
/// than new files to move to: move detection indexes the one new file rather than every deleted
/// row's content, and still finds the move.
#[test]
fn move_detection_after_a_mass_local_deletion_indexes_the_new_files() {
	const FILES: usize = 10_000;
	let (paths, baseline, remote) = nth_synced(FILES);
	let local = HashMap::from([(
		"moved.txt".to_string(),
		local_file("moved.txt", nth_hash(0)),
	)]);
	let (mut actions, mut consumed) = (Vec::new(), HashSet::new());
	let ((), peak) = alloc_meter::peak_bytes(|| {
		detect_moves(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			PassPaths::Whole,
			&mut actions,
			&mut consumed,
		);
	});
	assert_eq!(
		actions,
		vec![SyncAction::MoveRemote {
			from_path: paths[0].clone(),
			to_path: "moved.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(1),
		}]
	);
	assert!(
		peak < 192 * 1024,
		"move detection held {peak} bytes at once over {FILES} deleted files and one new one"
	);
}

/// The pull half's mirror: a remote mass trash beside one file moved under a new name has far
/// more rows whose file left its path than remote files to find one at, so the remote files
/// are indexed, and the move is still found.
#[test]
fn move_detection_after_a_mass_remote_trash_indexes_the_remote_files() {
	const FILES: usize = 10_000;
	let (paths, baseline, mut remote) = nth_synced(FILES);
	let local = nth_local(&paths);
	let moved = RemoteNode {
		rel_path: "moved.txt".to_string(),
		..remote.remove(&paths[0]).unwrap()
	};
	let remote = HashMap::from([("moved.txt".to_string(), moved)]);
	let (mut actions, mut consumed) = (Vec::new(), HashSet::new());
	let ((), peak) = alloc_meter::peak_bytes(|| {
		detect_moves(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			PassPaths::Whole,
			&mut actions,
			&mut consumed,
		);
	});
	assert_eq!(
		actions,
		vec![SyncAction::MoveLocal {
			from_path: paths[0].clone(),
			to_path: "moved.txt".to_string(),
			kind: NodeKind::File,
		}]
	);
	assert!(
		peak < 192 * 1024,
		"move detection held {peak} bytes at once over {FILES} trashed files and one moved one"
	);
}

/// A reconcile reads the same rows with debug logging on as with it off: its debug line asks
/// nothing of the sides. A carried side counts its nodes by asking the rows about every path
/// it recorded, so counting the local one read every row under a directory moved away — in a
/// pass deciding one path, and only where debug logging was on to pay for it.
#[test]
fn a_reconcile_reads_the_same_rows_with_debug_logging_on() {
	const FILES: usize = 2_000;
	// One pass's inputs over rows of their own, so each run is counted from the same start: a
	// directory whose every file the local side found gone, one of them decided. The same
	// uuids each time, so the two plans can be compared.
	let pass = || {
		let baseline = Baseline::from_rows((0..FILES).map(|n| {
			base_file(
				&format!("dir/{n:05}.txt"),
				Uuid::from_u128(1 + n as u128),
				[1; 32],
			)
		}));
		let mut local: Side<LocalNode> = Side::carried();
		for n in 0..FILES {
			local.remove(&baseline, &format!("dir/{n:05}.txt"));
		}
		(baseline, local)
	};
	let decided = BTreeSet::from(["dir/00000.txt".to_string()]);
	let reconciled = |(baseline, local): (Baseline, Side<LocalNode>)| {
		let remote: Side<RemoteNode> = Side::carried();
		let before = baseline.reads_for_test();
		let plan = reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local.of(&baseline),
			&remote.of(&baseline),
			&PassHolds::default(),
			PassPaths::Changed(&decided),
		);
		let after = baseline.reads_for_test();
		(plan.actions, (after.0 - before.0, after.1 - before.1))
	};
	let quiet =
		tracing::subscriber::with_default(tracing::subscriber::NoSubscriber::default(), || {
			assert!(!tracing::enabled!(tracing::Level::DEBUG));
			reconciled(pass())
		});
	let logged = tracing::subscriber::with_default(tracing_subscriber::registry(), || {
		assert!(
			tracing::enabled!(tracing::Level::DEBUG),
			"the debug lines must be live for this to test them"
		);
		reconciled(pass())
	});
	assert_eq!(
		quiet, logged,
		"the plan and the (statements, rows) it read, with debug logging off and on"
	);
}

fn map<T>(items: Vec<(&str, T)>) -> HashMap<String, T> {
	items.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

// ------------------------------------------------------------------------
// Backlog::AdoptDestination — the mode-switch re-seed and what it plans
// ------------------------------------------------------------------------

/// The re-seed picks exactly the paths whose SOURCE side is gone under the NEW mode — tracked or
/// not — and anchors each row to what the destination holds now.
#[test]
fn the_reseed_adopts_only_the_paths_the_source_no_longer_has() {
	let gone = Uuid::new_v4();
	let both = Uuid::new_v4();
	let baseline = map(vec![
		("gone.txt", base_file("gone.txt", gone, [1; 32])),
		("both.txt", base_file("both.txt", both, [2; 32])),
	]);
	// gone.txt survives only on the remote; both.txt is still on both sides. untracked.txt is a
	// remote item the pair never synced.
	let local = map(vec![("both.txt", local_file("both.txt", [2; 32]))]);
	let remote = map(vec![
		("gone.txt", remote_file("gone.txt", gone, [1; 32])),
		("both.txt", remote_file("both.txt", both, [2; 32])),
		(
			"untracked.txt",
			remote_file("untracked.txt", Uuid::new_v4(), [3; 32]),
		),
	]);

	let rows = adopt_destination_rows(SyncMode::LocalToRemote, &tree(&baseline), &local, &remote);
	let mut adopted: Vec<&str> = rows.iter().map(|r| r.rel_path.as_str()).collect();
	adopted.sort_unstable();
	assert_eq!(
		adopted,
		vec!["gone.txt", "untracked.txt"],
		"the standing source deletion AND the destination-only item the pair never tracked; \
			 both.txt is on both sides, so it is neither"
	);
	let row = rows.iter().find(|r| r.rel_path == "gone.txt").unwrap();
	assert_eq!(row.state, BaselineState::Adopted);
	assert_eq!(row.remote_uuid, Some(gone), "anchored to the remote copy");
	assert_eq!(row.content_hash, Some(Blake3Hash::from([1; 32])));
	assert_eq!(row.agreed_hash, None, "one side only: nothing is agreed");

	// The mirror image: with the REMOTE as the source, the local-only copy is the one adopted.
	let rows = adopt_destination_rows(
		SyncMode::RemoteToLocal,
		&tree(&baseline),
		&remote_as_local(),
		&map(vec![]),
	);
	assert!(
		rows.iter().all(|r| r.remote_uuid.is_none()),
		"a local-side adoption records no remote anchor: {rows:?}"
	);
}

/// A file another client created straight on the destination has no baseline row, so before the
/// switch adopted it a one-way mirror read it as an item to remove and trashed it on the very
/// next pass. The adoption is what makes the destination's contents what the caller asked to
/// keep, rather than only the pair's own standing backlog.
#[test]
fn the_reseed_adopts_a_destination_only_item_the_pair_never_tracked() {
	let theirs = Uuid::new_v4();
	let remote = map(vec![(
		"theirs.txt",
		remote_file("theirs.txt", theirs, [7; 32]),
	)]);
	let (baseline, local) = (map(vec![]), map(vec![]));

	// Without a row the mirror removes it.
	assert_eq!(
		plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
		vec![SyncAction::TrashRemote {
			rel_path: "theirs.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: theirs,
		}]
	);

	let rows = adopt_destination_rows(SyncMode::LocalToRemote, &tree(&baseline), &local, &remote);
	assert_eq!(
		rows.iter().map(|r| r.rel_path.as_str()).collect::<Vec<_>>(),
		vec!["theirs.txt"]
	);
	assert_eq!(rows[0].state, BaselineState::Adopted);
	assert_eq!(rows[0].remote_uuid, Some(theirs));

	let adopted = map(vec![("theirs.txt", rows.into_iter().next().unwrap())]);
	assert!(
		plan(SyncMode::LocalToRemote, &adopted, &local, &remote).is_empty(),
		"the adopted copy must survive the switch"
	);

	// Two-way needs no row: an untracked destination-only item already flows back on its own,
	// and a row here would only take the path out of move detection.
	assert!(adopt_destination_rows(SyncMode::TwoWay, &tree(&baseline), &local, &remote).is_empty());
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "theirs.txt".to_string(),
			remote_uuid: theirs,
		}]
	);
}

/// The mirror image, with the LOCAL tree as the destination: a file someone dropped into the
/// local root is adopted rather than deleted by the first pull-only pass after the switch.
#[test]
fn the_reseed_adopts_a_local_only_item_when_the_local_side_is_the_destination() {
	let local = map(vec![("dropped.txt", local_file("dropped.txt", [7; 32]))]);
	let (baseline, remote) = (map(vec![]), map(vec![]));

	assert_eq!(
		plan(SyncMode::RemoteToLocal, &baseline, &local, &remote),
		vec![SyncAction::DeleteLocal {
			rel_path: "dropped.txt".to_string(),
			kind: NodeKind::File,
		}]
	);

	let rows = adopt_destination_rows(SyncMode::RemoteToLocal, &tree(&baseline), &local, &remote);
	assert_eq!(
		rows.iter().map(|r| r.rel_path.as_str()).collect::<Vec<_>>(),
		vec!["dropped.txt"]
	);
	assert!(rows[0].remote_uuid.is_none(), "anchored to the local copy");

	let adopted = map(vec![("dropped.txt", rows.into_iter().next().unwrap())]);
	assert!(plan(SyncMode::RemoteToLocal, &adopted, &local, &remote).is_empty());
}

/// A local tree standing in for "the local side kept both files, the remote lost them".
fn remote_as_local() -> HashMap<String, LocalNode> {
	map(vec![
		("gone.txt", local_file("gone.txt", [1; 32])),
		("both.txt", local_file("both.txt", [2; 32])),
	])
}

/// What the adoption buys: a mirror mode leaves the adopted copy alone instead of trashing it,
/// where the same state with an ordinary synced row is a deletion (which is what
/// `Backlog::Propagate` keeps).
#[test]
fn a_mirror_leaves_an_adopted_destination_copy_alone() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
	let local = map(vec![]);

	// Propagate: the standing row makes this a deletion under the new mode.
	let synced = map(vec![("gone.txt", base_file("gone.txt", uuid, [1; 32]))]);
	assert_eq!(
		plan(SyncMode::LocalToRemote, &synced, &local, &remote),
		vec![SyncAction::TrashRemote {
			rel_path: "gone.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		}]
	);

	// AdoptDestination: the same two sides, with the re-seeded row, plan nothing at all.
	let adopted = map(vec![(
		"gone.txt",
		adopted_from_remote("gone.txt", &remote["gone.txt"]),
	)]);
	assert!(
		plan(SyncMode::LocalToRemote, &adopted, &local, &remote).is_empty(),
		"an adopted destination copy must not be deleted, or re-created"
	);
	// And a backup mode never deleted it either way.
	assert!(plan(SyncMode::LocalBackup, &adopted, &local, &remote).is_empty());
}

/// The re-seed adopts only from the sides it is handed, and `reconfigure_pair` hands it the two a
/// pass reads — the FILTERED scan and view (pinned live by MODE-22c) — so an ignored path is at
/// neither of them and nothing is adopted there, tracked or not. Were it adopted, the copy
/// the rule hides would become an intended one, and the mirror would keep it for good instead of
/// syncing the path like a first sync once the rule goes.
#[test]
fn the_reseed_adopts_nothing_at_an_ignored_path() {
	let (hidden, theirs) = (Uuid::new_v4(), Uuid::new_v4());
	// `secret.psd` was synced before a rule hid it; `theirs.psd` is a destination-only copy the
	// same rule hides. Neither is in the filtered sides the pass hands the re-seed.
	let baseline = map(vec![(
		"secret.psd",
		base_file("secret.psd", hidden, [1; 32]),
	)]);
	let (local, remote) = (map(vec![]), map(vec![]));
	for mode in [
		SyncMode::LocalToRemote,
		SyncMode::RemoteToLocal,
		SyncMode::TwoWay,
		SyncMode::LocalBackup,
		SyncMode::RemoteBackup,
	] {
		let rows = adopt_destination_rows(mode, &tree(&baseline), &local, &remote);
		assert!(rows.is_empty(), "{mode:?}: {rows:?}");
	}

	// The same switch on sides nothing hides: both copies ARE adopted, so it is the filtering
	// that decides, not the shape of the pair.
	let unfiltered = map(vec![
		("secret.psd", remote_file("secret.psd", hidden, [1; 32])),
		("theirs.psd", remote_file("theirs.psd", theirs, [2; 32])),
	]);
	let rows = adopt_destination_rows(
		SyncMode::LocalToRemote,
		&tree(&baseline),
		&local,
		&unfiltered,
	);
	let mut adopted: Vec<&str> = rows.iter().map(|r| r.rel_path.as_str()).collect();
	adopted.sort_unstable();
	assert_eq!(adopted, vec!["secret.psd", "theirs.psd"]);
}

/// TwoWay has no destination to spare: the adopted copy reads as newly created on the side that
/// still holds it and flows back to the other one.
#[test]
fn two_way_pulls_an_adopted_destination_copy_back() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
	let adopted = map(vec![(
		"gone.txt",
		adopted_from_remote("gone.txt", &remote["gone.txt"]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &adopted, &map(vec![]), &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "gone.txt".to_string(),
			remote_uuid: uuid,
		}]
	);
}

/// The row retires on its own: as soon as the source has something at the path again the two
/// sides reconcile normally (here: the same bytes on both sides, so the pass records the
/// converged state), and once neither side holds the path the row is dropped.
#[test]
fn an_adopted_row_retires_as_soon_as_either_side_moves() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("gone.txt", remote_file("gone.txt", uuid, [1; 32]))]);
	let adopted = map(vec![(
		"gone.txt",
		adopted_from_remote("gone.txt", &remote["gone.txt"]),
	)]);

	// The source is back with the SAME content: nothing to transfer, but the row must stop
	// being an adoption.
	let local = map(vec![("gone.txt", local_file("gone.txt", [1; 32]))]);
	assert_eq!(
		plan(SyncMode::LocalToRemote, &adopted, &local, &remote),
		vec![SyncAction::AdoptBaseline {
			rel_path: "gone.txt".to_string(),
		}]
	);
	// Back with DIFFERENT content: the source is authoritative again.
	let edited = map(vec![("gone.txt", local_file("gone.txt", [9; 32]))]);
	assert_eq!(
		plan(SyncMode::LocalToRemote, &adopted, &edited, &remote),
		vec![SyncAction::UploadFile {
			rel_path: "gone.txt".to_string(),
		}]
	);
	// The destination lost it too (someone deleted it there): the stale row is retired.
	assert_eq!(
		plan(
			SyncMode::LocalToRemote,
			&adopted,
			&map(vec![]),
			&map(vec![])
		),
		vec![SyncAction::AdoptBaseline {
			rel_path: "gone.txt".to_string(),
		}]
	);
}

/// An adopted row is one side's standing copy, not a synced pair, so move detection must not
/// match it: the "moved" item was never on the source side to begin with.
#[test]
fn an_adopted_row_is_never_a_move_endpoint() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("moved.txt", remote_file("moved.txt", uuid, [1; 32]))]);
	let adopted = map(vec![(
		"gone.txt",
		adopted_from_remote("gone.txt", &remote_file("gone.txt", uuid, [1; 32])),
	)]);
	let actions = plan(SyncMode::TwoWay, &adopted, &map(vec![]), &remote);
	assert!(
		!actions
			.iter()
			.any(|a| matches!(a, SyncAction::MoveLocal { .. })),
		"the adopted row was matched as a move source: {actions:?}"
	);
}

/// The push half of a two-way pass judges the rows the pull half's walk kept for it: a local
/// rename is a remote move, and of two rows gone with the same content the first in path order
/// is its source — as a walk of its own would have picked.
#[test]
fn a_two_way_local_rename_is_a_remote_move_and_the_first_same_hash_source_wins() {
	let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
	let baseline = map(vec![
		("a.txt", base_file("a.txt", a, [5; 32])),
		("b.txt", base_file("b.txt", b, [5; 32])),
	]);
	let remote = map(vec![
		("a.txt", remote_file("a.txt", a, [5; 32])),
		("b.txt", remote_file("b.txt", b, [5; 32])),
	]);
	let local = map(vec![("c.txt", local_file("c.txt", [5; 32]))]);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	let moves: Vec<_> = actions
		.iter()
		.filter(|action| matches!(action, SyncAction::MoveRemote { .. }))
		.collect();
	assert_eq!(
		moves,
		vec![&SyncAction::MoveRemote {
			from_path: "a.txt".to_string(),
			to_path: "c.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: a,
		}],
		"{actions:?}"
	);
	assert!(
		!actions.contains(&SyncAction::UploadFile {
			rel_path: "c.txt".to_string()
		}),
		"the renamed file was uploaded again: {actions:?}"
	);
}

#[test]
fn local_to_remote_uploads_new_and_trashes_remote_only() {
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let stale = Uuid::new_v4();
	let remote = map(vec![("old.txt", remote_file("old.txt", stale, [2; 32]))]);
	let actions = plan(SyncMode::LocalToRemote, &HashMap::new(), &local, &remote);
	assert_eq!(
		actions,
		vec![
			SyncAction::UploadFile {
				rel_path: "a.txt".to_string(),
			},
			SyncAction::TrashRemote {
				rel_path: "old.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: stale,
			},
		],
		"transfers ordered before deletes (deletes run last)"
	);
}

#[test]
fn local_backup_uploads_but_never_trashes() {
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![(
		"old.txt",
		remote_file("old.txt", Uuid::new_v4(), [2; 32]),
	)]);
	let actions = plan(SyncMode::LocalBackup, &HashMap::new(), &local, &remote);
	assert_eq!(
		actions,
		vec![SyncAction::UploadFile {
			rel_path: "a.txt".to_string(),
		}],
		"backup pushes the new file but leaves the remote-only file untouched"
	);
}

#[test]
fn remote_to_local_downloads_and_deletes_local_only() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("r.txt", remote_file("r.txt", uuid, [3; 32]))]);
	let local = map(vec![("extra.txt", local_file("extra.txt", [4; 32]))]);
	let actions = plan(SyncMode::RemoteToLocal, &HashMap::new(), &local, &remote);
	assert_eq!(
		actions,
		vec![
			SyncAction::DownloadFile {
				rel_path: "r.txt".to_string(),
				remote_uuid: uuid,
			},
			SyncAction::DeleteLocal {
				rel_path: "extra.txt".to_string(),
				kind: NodeKind::File,
			},
		],
		"transfers ordered before deletes (deletes run last)"
	);
}

const ALL_MODES: [SyncMode; 5] = [
	SyncMode::LocalToRemote,
	SyncMode::RemoteToLocal,
	SyncMode::LocalBackup,
	SyncMode::RemoteBackup,
	SyncMode::TwoWay,
];

#[test]
fn identical_content_adopts_the_baseline_in_every_mode() {
	let uuid = Uuid::new_v4();
	let local = map(vec![("a.txt", local_file("a.txt", [7; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [7; 32]))]);
	// An already-converged path with no baseline row is adopted (no transfer) in EVERY mode:
	// without it a pair opened on an already-synced tree never seeds a baseline, so every later
	// pass still looks like a first sync and the guard holds all deletions forever.
	for mode in ALL_MODES {
		assert_eq!(
			plan(mode, &HashMap::new(), &local, &remote),
			vec![SyncAction::AdoptBaseline {
				rel_path: "a.txt".to_string()
			}],
			"{mode:?} must adopt the converged path"
		);
	}
	// Once the baseline records it, the pass is a clean no-op — adopting must not re-trigger.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [7; 32]))]);
	for mode in ALL_MODES {
		assert!(
			plan(mode, &baseline, &local, &remote).is_empty(),
			"{mode:?} must be a no-op once the baseline is current"
		);
	}
}

#[test]
fn a_baseline_row_for_a_path_gone_from_both_sides_is_adopted_away() {
	let uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [7; 32]))]);
	for mode in ALL_MODES {
		assert_eq!(
			plan(mode, &baseline, &HashMap::new(), &HashMap::new()),
			vec![SyncAction::AdoptBaseline {
				rel_path: "a.txt".to_string()
			}],
			"{mode:?} must drop the stale baseline row for a convergently-deleted path"
		);
	}
}

#[test]
fn one_way_modes_adopt_a_matching_dir_so_the_pair_stops_looking_like_a_first_sync() {
	let uuid = Uuid::new_v4();
	let remote_dir = RemoteNode {
		rel_path: "d".to_string(),
		kind: NodeKind::Dir,
		remote_uuid: uuid,
		stable_uuid: None,
		content_hash: None,
		size: 0,
		modified_millis: 0,
	};
	let local = map(vec![("d", local_dir("d"))]);
	let remote = map(vec![("d", remote_dir)]);
	assert_eq!(
		plan(SyncMode::LocalToRemote, &HashMap::new(), &local, &remote),
		vec![SyncAction::AdoptBaseline {
			rel_path: "d".to_string()
		}]
	);
}

#[test]
fn create_actions_order_parents_before_children() {
	let local = map(vec![
		("dir/sub/c.txt", local_file("dir/sub/c.txt", [1; 32])),
		("dir", local_dir("dir")),
		("dir/sub", local_dir("dir/sub")),
	]);
	let actions = plan(
		SyncMode::LocalToRemote,
		&HashMap::new(),
		&local,
		&HashMap::new(),
	);
	let paths: Vec<_> = actions.iter().map(|a| a.rel_path()).collect();
	assert_eq!(paths, vec!["dir", "dir/sub", "dir/sub/c.txt"]);
}

#[test]
fn dir_rename_moves_children_before_trashing_the_old_dir() {
	// Local rename of `old/` -> `new/` (with child `x.txt`). The child is a hash-matched move;
	// the old dir is trashed. The move MUST be ordered before the old-dir trash, otherwise the
	// cascading dir delete destroys the move source (the headline ordering bug).
	let dir_uuid = Uuid::new_v4();
	let file_uuid = Uuid::new_v4();
	let base_dir = BaselineEntry {
		rel_path: "old".to_string(),
		kind: NodeKind::Dir,
		remote_uuid: Some(dir_uuid),
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
	let baseline = map(vec![
		("old", base_dir),
		("old/x.txt", base_file("old/x.txt", file_uuid, [7; 32])),
	]);
	// Remote still holds the pre-rename tree.
	let remote_dir = RemoteNode {
		rel_path: "old".to_string(),
		kind: NodeKind::Dir,
		remote_uuid: dir_uuid,
		stable_uuid: None,
		content_hash: None,
		size: 0,
		modified_millis: 0,
	};
	let remote = map(vec![
		("old", remote_dir),
		("old/x.txt", remote_file("old/x.txt", file_uuid, [7; 32])),
	]);
	// Locally the dir and its child have been renamed to `new/`.
	let local = map(vec![
		("new", local_dir("new")),
		("new/x.txt", local_file("new/x.txt", [7; 32])),
	]);

	let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert_eq!(
		actions,
		vec![
			SyncAction::CreateRemoteDir {
				rel_path: "new".to_string(),
			},
			SyncAction::MoveRemote {
				from_path: "old/x.txt".to_string(),
				to_path: "new/x.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: file_uuid,
			},
			SyncAction::TrashRemote {
				rel_path: "old".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: dir_uuid,
			},
		],
		"create new dir, MOVE the child out, THEN trash the old dir — never trash first"
	);
}

#[test]
fn a_directory_replace_delete_runs_after_the_moves_out_of_that_directory() {
	// The file `item` and the directory `box/` (holding inner.txt) swap names: `item` becomes a
	// directory holding inner.txt, `box` becomes the file. Trashing a directory is RECURSIVE, so
	// the trash of `box` must not run before the move that carries `box/inner.txt` out of it —
	// the server would then reject the move with `cannot_move_this_file`.
	let item_uuid = Uuid::new_v4();
	let box_uuid = Uuid::new_v4();
	let inner_uuid = Uuid::new_v4();
	let baseline = map(vec![
		("item", base_file("item", item_uuid, [1; 32])),
		("box", base_dir("box", box_uuid)),
		(
			"box/inner.txt",
			base_file("box/inner.txt", inner_uuid, [2; 32]),
		),
	]);
	let remote = map(vec![
		("item", remote_file("item", item_uuid, [1; 32])),
		("box", remote_dir_node("box", box_uuid)),
		(
			"box/inner.txt",
			remote_file("box/inner.txt", inner_uuid, [2; 32]),
		),
	]);
	let local = map(vec![
		("item", local_dir("item")),
		("item/inner.txt", local_file("item/inner.txt", [2; 32])),
		("box", local_file("box", [1; 32])),
	]);

	let actions = plan(SyncMode::LocalToRemote, &baseline, &local, &remote);
	assert_eq!(
		actions,
		vec![
			// The old FILE at `item` goes first — nothing hangs off it and the dir create needs
			// the name free.
			SyncAction::TrashRemote {
				rel_path: "item".to_string(),
				kind: NodeKind::File,
				remote_uuid: item_uuid,
			},
			SyncAction::CreateRemoteDir {
				rel_path: "item".to_string(),
			},
			SyncAction::MoveRemote {
				from_path: "box/inner.txt".to_string(),
				to_path: "item/inner.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: inner_uuid,
			},
			// Only now may the old DIRECTORY be trashed; the upload of the new file `box`
			// follows it.
			SyncAction::TrashRemote {
				rel_path: "box".to_string(),
				kind: NodeKind::Dir,
				remote_uuid: box_uuid,
			},
			SyncAction::UploadFile {
				rel_path: "box".to_string(),
			},
		],
		"a recursive dir trash must follow the moves out of it, and precede the create at its path"
	);
}

/// The paths a pass both deletes and creates are the first halves of its type flips; a path
/// only one side of that touches is not among them.
#[test]
fn the_replaced_paths_are_the_deletions_a_create_fills_again() {
	let actions = [
		SyncAction::TrashRemote {
			rel_path: "x".to_owned(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(1),
		},
		SyncAction::DeleteLocal {
			rel_path: "y".to_owned(),
			kind: NodeKind::Dir,
		},
		SyncAction::TrashRemote {
			rel_path: "z".to_owned(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(2),
		},
		SyncAction::DeleteLocal {
			rel_path: "v".to_owned(),
			kind: NodeKind::File,
		},
		SyncAction::CreateRemoteDir {
			rel_path: "x".to_owned(),
		},
		SyncAction::DownloadFile {
			rel_path: "y".to_owned(),
			remote_uuid: Uuid::from_u128(3),
		},
		SyncAction::UploadFile {
			rel_path: "w".to_owned(),
		},
	];
	assert_eq!(
		replaced_paths(&actions),
		HashSet::from(["x".to_owned(), "y".to_owned()])
	);
}

/// A first sync plans nothing but creates, so it replaces nothing, and finding that out
/// allocates nothing — not an owned path per planned item, which apply would keep until the
/// pass ends.
#[test]
fn a_first_sync_keeps_no_path_for_its_replaced_deletions() {
	let actions: Vec<SyncAction> = (0..10_000)
		.map(|n| SyncAction::UploadFile {
			rel_path: format!("d{:02}/f{n:05}.txt", n % 100),
		})
		.collect();
	let (replaced, peak) = alloc_meter::peak_bytes(|| replaced_paths(&actions));
	assert_eq!(peak, 0, "10_000 uploads and no deletion");
	assert!(replaced.is_empty());
}

/// The same holds the other way round: a pass that deletes a whole tree and creates nothing
/// replaces nothing, and allocates nothing to find that out.
#[test]
fn a_mass_deletion_keeps_no_path_for_its_replaced_deletions() {
	let actions: Vec<SyncAction> = (0..10_000)
		.map(|n| SyncAction::DeleteLocal {
			rel_path: format!("d{:02}/f{n:05}.txt", n % 100),
			kind: NodeKind::File,
		})
		.collect();
	let (replaced, peak) = alloc_meter::peak_bytes(|| replaced_paths(&actions));
	assert_eq!(peak, 0, "10_000 deletions and no create");
	assert!(replaced.is_empty());
}

#[test]
fn a_just_written_remote_item_the_cache_has_not_caught_up_to_is_folded_in_not_acted_on() {
	let uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [5; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [5; 32]))]);
	// The snapshot is behind: the uuid the pass just recorded is not in it yet.
	let remote = HashMap::new();
	let observations = Observations::default();
	let writes = PendingWrites::default();
	writes.record(
		&observations,
		PAIR,
		uuid,
		PendingKind::Created {
			path: "a.txt".to_string(),
			replaced: None,
		},
	);
	for mode in ALL_MODES {
		assert!(
			plan_folded(mode, &baseline, &local, &remote, &writes).is_empty(),
			"{mode:?} must neither re-transfer nor delete a write the cache has not seen"
		);
	}
	// Once the uuid is no longer pending (the grace window elapsed), the snapshot is believed
	// again: the file reads as a remote-side deletion and is re-pushed.
	assert_eq!(
		plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
		vec![SyncAction::UploadFile {
			rel_path: "a.txt".to_string(),
		}],
		"past the grace window the old behaviour returns"
	);
}

/// Trashing an item drops its baseline row, so a snapshot that has not applied the trash yet
/// reads it as an untracked remote file with nothing local — a deletion to make all over
/// again. Only the deletion of that uuid is suppressed.
#[test]
fn a_remote_deletion_this_engine_already_made_is_not_repeated() {
	let uuid = Uuid::new_v4();
	let remote = map(vec![("note.txt", remote_file("note.txt", uuid, [5; 32]))]);
	let (baseline, local) = (HashMap::new(), HashMap::new());

	assert_eq!(
		plan(SyncMode::LocalToRemote, &baseline, &local, &remote),
		vec![SyncAction::TrashRemote {
			rel_path: "note.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		}],
		"without the hold this is exactly the duplicate trash"
	);
	assert!(
		reconcile(
			SyncMode::LocalToRemote,
			&tree(&baseline),
			&local,
			&remote,
			&PassHolds {
				trashed: HashSet::from([uuid]),
				..Default::default()
			},
			PassPaths::Whole,
		)
		.actions
		.is_empty(),
		"a trash this engine already applied must not be applied again"
	);
}

/// The ACTION is suppressed, not the path: a delete-then-recreate uploads a new file at the
/// very path that was just trashed, and freezing the path would stall that upload.
#[test]
fn a_pending_trash_does_not_freeze_its_path_against_a_new_file() {
	let uuid = Uuid::new_v4();
	// The cache still lists the trashed file; a fresh local file now holds the same name.
	let remote = map(vec![("note.txt", remote_file("note.txt", uuid, [5; 32]))]);
	let local = map(vec![("note.txt", local_file("note.txt", [9; 32]))]);

	assert_eq!(
		reconcile(
			SyncMode::LocalToRemote,
			&Baseline::default(),
			&local,
			&remote,
			&PassHolds {
				trashed: HashSet::from([uuid]),
				..Default::default()
			},
			PassPaths::Whole,
		)
		.actions,
		vec![SyncAction::UploadFile {
			rel_path: "note.txt".to_string(),
		}],
		"the re-created file still uploads"
	);
}

#[test]
fn a_directory_this_pass_created_is_not_created_a_second_time() {
	let dir_uuid = Uuid::new_v4();
	let file_uuid = Uuid::new_v4();
	let baseline = map(vec![
		("d", base_dir("d", dir_uuid)),
		("d/x.txt", base_file("d/x.txt", file_uuid, [1; 32])),
	]);
	let local = map(vec![
		("d", local_dir("d")),
		("d/x.txt", local_file("d/x.txt", [1; 32])),
	]);
	// Neither the new dir nor its child has reached the cache yet.
	let observations = Observations::default();
	let writes = PendingWrites::default();
	writes.record(
		&observations,
		PAIR,
		dir_uuid,
		PendingKind::Created {
			path: "d".to_string(),
			replaced: None,
		},
	);
	writes.record(
		&observations,
		PAIR,
		file_uuid,
		PendingKind::Created {
			path: "d/x.txt".to_string(),
			replaced: None,
		},
	);
	assert!(
		plan_folded(
			SyncMode::LocalToRemote,
			&baseline,
			&local,
			&HashMap::new(),
			&writes
		)
		.is_empty(),
		"a dir this pass created must not be re-created, and its unchanged child must not be \
			 re-uploaded"
	);
}

/// The same pass, one step later: the cache has announced the CHILD's upload but not the
/// directory's creation. The announcement retires the child's record — it is evidence the cache
/// has caught up, and a snapshot that still lists nothing there is evidence the file is gone
/// (trashed, or superseded by another client), so the pass must put it back. Folding the
/// directory in is what gives that upload a parent to land in; the directory itself is still not
/// created a second time.
#[test]
fn a_child_the_cache_has_announced_is_re_uploaded_into_the_folded_parent() {
	let dir_uuid = Uuid::new_v4();
	let file_uuid = Uuid::new_v4();
	let baseline = map(vec![
		("d", base_dir("d", dir_uuid)),
		("d/x.txt", base_file("d/x.txt", file_uuid, [1; 32])),
	]);
	let local = map(vec![
		("d", local_dir("d")),
		("d/x.txt", local_file("d/x.txt", [1; 32])),
	]);
	let observations = Observations::default();
	let writes = PendingWrites::default();
	writes.record(
		&observations,
		PAIR,
		dir_uuid,
		PendingKind::Created {
			path: "d".to_string(),
			replaced: None,
		},
	);
	assert_eq!(
		plan_folded(
			SyncMode::LocalToRemote,
			&baseline,
			&local,
			&HashMap::new(),
			&writes
		),
		vec![SyncAction::UploadFile {
			rel_path: "d/x.txt".to_string(),
		}]
	);
}

#[test]
fn an_unapplied_move_is_not_undone_by_a_snapshot_still_showing_the_old_path() {
	let uuid = Uuid::new_v4();
	// The pass moved a.txt -> b.txt and advanced its baseline; the cache still lists the uuid at
	// a.txt. Acting on that would trash a.txt (the very item that was moved) and re-upload
	// b.txt — in two-way, even quarantine the local file first.
	let baseline = map(vec![("b.txt", base_file("b.txt", uuid, [5; 32]))]);
	let local = map(vec![("b.txt", local_file("b.txt", [5; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [5; 32]))]);
	let observations = Observations::default();
	let writes = PendingWrites::default();
	writes.record(
		&observations,
		PAIR,
		uuid,
		PendingKind::Moved {
			from: "a.txt".to_string(),
			to: "b.txt".to_string(),
		},
	);
	for mode in ALL_MODES {
		assert!(
			plan_folded(mode, &baseline, &local, &remote, &writes).is_empty(),
			"{mode:?} must leave both ends of an unapplied move alone"
		);
	}
	// The engine only marks the uuid pending while the snapshot still shows the PRE-move state;
	// once it does not, the path is acted on again (another client moving the item after us
	// must reconverge immediately, not wait out the grace window).
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::MoveLocal {
			from_path: "b.txt".to_string(),
			to_path: "a.txt".to_string(),
			kind: NodeKind::File,
		}]
	);
}

#[test]
fn is_safe_name_rejects_traversal_and_separators() {
	assert!(is_safe_name("normal.txt"));
	assert!(is_safe_name("a file with spaces"));
	assert!(!is_safe_name(""));
	assert!(!is_safe_name("."));
	assert!(!is_safe_name(".."));
	assert!(!is_safe_name("a/b"), "embedded separator");
	assert!(!is_safe_name("a\\b"), "embedded backslash");
}

fn remote_dir(name: &str, parent: Uuid) -> RemoteItem {
	RemoteItem {
		uuid: Uuid::new_v4(),
		parent,
		name: name.to_string(),
		stable_uuid: None,
		hash: None,
		size: 0,
		modified_millis: 1,
	}
}

fn undecodable(parent: Uuid, stable_uuid: Option<StableUuid>) -> UndecodableItem {
	UndecodableItem {
		uuid: Uuid::new_v4(),
		parent,
		stable_uuid,
	}
}

#[test]
fn a_traversal_name_never_enters_the_view_and_its_subtree_shares_its_record() {
	let root = Uuid::new_v4();
	// A direct child literally named ".." (it would escape the root on pull), and a normal
	// directory beside it.
	let evil = remote_dir("..", root);
	let ok = remote_dir("ok", root);
	// A child under the traversal-named dir is left out with it, recorded under its record.
	let under_evil = remote_dir("x", evil.uuid);
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		&[evil.clone(), ok, under_evil.clone()],
		&[],
		&[],
		None,
	);
	assert_eq!(view.nodes.whole().paths().collect::<Vec<_>>(), vec!["ok"]);
	let record = |remote_uuid| SkippedRemote {
		remote_uuid,
		stable_uuid: None,
		rel_path: "..".to_string(),
		path_is_dir: true,
		reason: UnsyncableReason::RemoteInvalidName {
			name: "..".to_string(),
		},
	};
	assert_eq!(
		view.skipped,
		vec![record(evil.uuid), record(under_evil.uuid)]
	);
}

/// The cache's undecodable records carry every sync root's; only those under THIS root are the
/// view's, placed at the directory that holds them — and one inside an undecodable directory
/// takes that directory's record.
#[test]
fn undecodable_items_are_recorded_under_their_parent_when_they_belong_to_the_root() {
	let root = Uuid::new_v4();
	let sub = remote_dir("sub", root);
	let lineage = StableUuid::new_for_test(Uuid::new_v4());
	let in_sub = undecodable(sub.uuid, Some(lineage));
	let garbled_dir = undecodable(root, None);
	let under_garbled = undecodable(garbled_dir.uuid, Some(lineage));
	let elsewhere = undecodable(Uuid::new_v4(), None);
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		std::slice::from_ref(&sub),
		&[],
		&[in_sub, garbled_dir, under_garbled, elsewhere],
		None,
	);
	assert_eq!(view.nodes.whole().paths().collect::<Vec<_>>(), vec!["sub"]);
	let recorded: Vec<(Uuid, &str)> = view
		.skipped
		.iter()
		.map(|s| {
			assert_eq!(s.reason, UnsyncableReason::RemoteUndecodable);
			(s.remote_uuid, s.rel_path.as_str())
		})
		.collect();
	assert_eq!(
		recorded,
		vec![
			(in_sub.uuid, "sub"),
			(garbled_dir.uuid, ""),
			(under_garbled.uuid, "")
		]
	);
	assert_eq!(view.skipped[0].stable_uuid, Some(lineage));
}

/// A synced item moved under a directory the view skips is still found, by its own uuid: its
/// synced path is unknown, not absent, however deep under the skipped directory it sits. A synced
/// directory's descendants add no line of their own, and the strangers under one skipped
/// directory report that directory once.
#[test]
fn a_synced_item_moved_under_a_skipped_directory_is_unknown_not_absent() {
	let root = Uuid::new_v4();
	let evil = remote_dir("..", root);
	let moved = cacheable_file(evil.uuid, "a.txt");
	let stranger = cacheable_file(evil.uuid, "b.txt");
	let nested = remote_dir("deeper", evil.uuid);
	let deep = cacheable_file(nested.uuid, "c.txt");
	let garbled = undecodable(root, None);
	let in_garbled = undecodable(garbled.uuid, None);
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		&[evil.clone(), nested.clone()],
		&[moved.clone(), stranger, deep.clone()],
		&[garbled, in_garbled],
		None,
	);
	assert!(
		view.nodes.whole().is_empty(),
		"{:?}",
		view.nodes.whole().paths().collect::<Vec<_>>()
	);
	let invalid = UnsyncableReason::RemoteInvalidName {
		name: "..".to_string(),
	};
	for uuid in [evil.uuid, moved.uuid, nested.uuid, deep.uuid] {
		let record = view
			.skipped
			.iter()
			.find(|s| s.remote_uuid == uuid)
			.unwrap_or_else(|| panic!("{uuid} not recorded: {:?}", view.skipped));
		assert_eq!((record.rel_path.as_str(), &record.reason), ("..", &invalid));
	}

	let baseline = HashMap::from([
		(
			"docs/a.txt".to_string(),
			base_file("docs/a.txt", moved.uuid, [1; 32]),
		),
		("sub".to_string(), base_dir("sub", nested.uuid)),
		(
			"sub/c.txt".to_string(),
			base_file("sub/c.txt", deep.uuid, [2; 32]),
		),
	]);
	let rows = tree(&baseline);
	let (unknown, never_synced) =
		unknown_remote_paths(&synced_for(&rows, &view.skipped), &view.skipped);
	assert_eq!(
		unknown,
		BTreeMap::from([
			("docs/a.txt".to_string(), invalid.clone()),
			("sub".to_string(), invalid.clone()),
		])
	);
	assert_eq!(
		never_synced,
		vec![
			UnsyncablePath {
				rel_path: String::new(),
				reason: UnsyncableReason::RemoteUndecodable,
			},
			UnsyncablePath {
				rel_path: "..".to_string(),
				reason: invalid,
			},
		]
	);
}

#[test]
fn a_synced_path_whose_item_was_skipped_is_unknown_and_a_stranger_is_only_reported() {
	let synced_uuid = Uuid::new_v4();
	let lineage = StableUuid::new_for_test(synced_uuid);
	let baseline = HashMap::from([
		(
			"docs".to_string(),
			BaselineEntry {
				kind: NodeKind::Dir,
				remote_stable_uuid: None,
				..base_file("docs", Uuid::new_v4(), [0; 32])
			},
		),
		(
			"docs/a.txt".to_string(),
			base_file("docs/a.txt", synced_uuid, [1; 32]),
		),
	]);
	let skipped = vec![
		// A foreign same-name upload left `a.txt` as a NEW version the cache cannot decode: a
		// different uuid, the same lineage.
		SkippedRemote {
			remote_uuid: Uuid::new_v4(),
			stable_uuid: Some(lineage),
			rel_path: "docs".to_string(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteUndecodable,
		},
		// Two undecodable strangers in one directory report one line.
		SkippedRemote {
			remote_uuid: Uuid::new_v4(),
			stable_uuid: None,
			rel_path: "docs".to_string(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteUndecodable,
		},
		SkippedRemote {
			remote_uuid: Uuid::new_v4(),
			stable_uuid: None,
			rel_path: "docs".to_string(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteUndecodable,
		},
	];
	let rows = tree(&baseline);
	let (unknown, never_synced) = unknown_remote_paths(&synced_for(&rows, &skipped), &skipped);
	assert_eq!(
		unknown,
		BTreeMap::from([(
			"docs/a.txt".to_string(),
			UnsyncableReason::RemoteUndecodable
		)])
	);
	assert_eq!(
		never_synced,
		vec![UnsyncablePath {
			rel_path: "docs".to_string(),
			reason: UnsyncableReason::RemoteUndecodable,
		}]
	);
}

fn root_rules(text: &str) -> IgnoreRules {
	let (source, errors) = IgnoreSource::parse(text, Origin::File { dir: "" }).unwrap();
	assert!(errors.is_empty(), "{errors:?}");
	let mut rules = IgnoreRules::default();
	rules.insert_file(String::new(), source);
	rules
}

/// The rule the local scan follows, on the remote side: a BUILT-IN default hit is a root only
/// where a row sits at or under it. The item is out of the view either way, so a `.DS_Store`
/// synced before the defaults existed keeps its row, its report line and its untracking, while
/// the ones nobody ever synced stop being carried as roots at all.
#[test]
fn a_default_rule_hit_on_the_remote_is_a_root_only_where_a_row_sits_at_or_under_it() {
	let root = Uuid::new_v4();
	let docs = remote_dir("docs", root);
	let dirs = [docs.clone()];
	let files = [
		cacheable_file(root, ".DS_Store"),
		cacheable_file(docs.uuid, ".DS_Store"),
		cacheable_file(docs.uuid, "keep.txt"),
	];
	// No lines of its own: the built-in defaults are what hides a `.DS_Store`.
	let rules = root_rules("");

	let untracked = build_remote_view(
		root,
		PairRoot::Directory,
		&dirs,
		&files,
		&[],
		Some(ViewFilter {
			rules: &rules,
			baseline: &Baseline::default(),
		}),
	);

	let mut paths: Vec<String> = untracked
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort_unstable();
	assert_eq!(paths, ["docs", "docs/keep.txt"], "both are still hidden");
	assert!(
		untracked.ignored.is_empty(),
		"nothing was synced at either, so neither is a root: {:?}",
		untracked.ignored
	);
	assert_eq!(untracked.ignored_default_untracked, 2);

	let baseline = HashMap::from([(
		"docs/.DS_Store".to_string(),
		base_file("docs/.DS_Store", Uuid::new_v4(), [7; 32]),
	)]);

	let tracked = build_remote_view(
		root,
		PairRoot::Directory,
		&dirs,
		&files,
		&[],
		Some(ViewFilter {
			rules: &rules,
			baseline: &tree(&baseline),
		}),
	);

	assert_eq!(
		tracked.ignored.keys().collect::<Vec<_>>(),
		vec!["docs/.DS_Store"],
		"the one with a row is a root, and is reported and untracked as before"
	);
	assert_eq!(
		tracked.ignored_default_untracked, 1,
		"the root's own `.DS_Store` is still untracked"
	);
}

/// An ignored item is left out of the view with everything under it, and only the top of each
/// ignored subtree is recorded. Ignored case-twins no longer refuse the pass, and an item the view
/// cannot place inside an ignored directory is not reported: it is out of sync, not unsyncable.
#[test]
fn an_ignored_remote_item_leaves_the_view_and_only_its_top_is_recorded() {
	let root = Uuid::new_v4();
	let build = remote_dir("build", root);
	let deep = remote_dir("deep", build.uuid);
	let evil_in_build = remote_dir("..", build.uuid);
	let lineage = StableUuid::new_for_test(Uuid::new_v4());
	let dirs = [build.clone(), deep.clone(), evil_in_build];
	let files = [
		cacheable_file(build.uuid, "o.bin"),
		cacheable_file(deep.uuid, "x.bin"),
		cacheable_file(root, "a.TMP"),
		cacheable_file(root, "A.tmp"),
		cacheable_file(root, "keep.txt"),
	];
	let stranger = undecodable(root, None);
	let undecodables = [undecodable(build.uuid, Some(lineage)), stranger];

	let raw = build_remote_view(
		root,
		PairRoot::Directory,
		&dirs,
		&files,
		&undecodables,
		None,
	);
	assert!(raw.has_collisions, "unfiltered, the twins collide");
	assert_eq!(raw.skipped.len(), 3, "{:?}", raw.skipped);
	assert!(raw.ignored.is_empty());

	let rules = root_rules("build/\n*.tmp");
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		&dirs,
		&files,
		&undecodables,
		Some(ViewFilter {
			rules: &rules,
			baseline: &Baseline::default(),
		}),
	);
	assert_eq!(
		view.nodes.whole().paths().collect::<Vec<_>>(),
		vec!["keep.txt"]
	);
	assert!(!view.has_collisions);
	let by = |pattern: &str| IgnoreDecision {
		level: IgnoreLevel::File { dir: String::new() },
		pattern: pattern.to_string(),
	};
	assert_eq!(
		view.ignored,
		BTreeMap::from([
			("A.tmp".to_string(), by("*.tmp")),
			("a.TMP".to_string(), by("*.tmp")),
			("build".to_string(), by("build/")),
		])
	);
	assert_eq!(
		view.skipped,
		vec![SkippedRemote {
			remote_uuid: stranger.uuid,
			stable_uuid: None,
			rel_path: String::new(),
			path_is_dir: true,
			reason: UnsyncableReason::RemoteUndecodable,
		}]
	);
	let baseline = HashMap::new();
	let rows = tree(&baseline);
	let (_, never_synced) = unknown_remote_paths(&synced_for(&rows, &view.skipped), &view.skipped);
	assert_eq!(never_synced.len(), 1, "{never_synced:?}");
}

/// The narrowed filter a change-scoped pass runs asks the rules about the keys that pass
/// DECIDED, and about nothing else.
///
/// Both directions have to hold or the narrowing is wrong in one of the two ways that matter. A
/// decided path the rules hide has to leave with everything under it, exactly as the whole form
/// drops it — otherwise a remote create inside an ignored directory would be reconciled and
/// pulled. A path NO producer moved has to stay, which is the saving itself: its local half is
/// carried from the same baseline row, so the pair reads as converged and the reconcile — which
/// visits only the decided set — plans nothing at it.
#[test]
fn the_narrowed_filter_hides_the_decided_paths_and_leaves_the_carried_ones() {
	let root = Uuid::new_v4();
	let build = remote_dir("build", root);
	let deep = remote_dir("deep", build.uuid);
	let dirs = [build.clone(), deep.clone()];
	let files = [
		cacheable_file(build.uuid, "o.bin"),
		cacheable_file(deep.uuid, "x.bin"),
		cacheable_file(root, "keep.txt"),
	];
	let rules = root_rules("build/");
	let no_rows = Baseline::default();
	let filter = || ViewFilter {
		rules: &rules,
		baseline: &no_rows,
	};
	let decided = |paths: &[&str]| -> BTreeSet<String> {
		paths.iter().map(|path| (*path).to_string()).collect()
	};
	let sorted = |view: &RemoteView| {
		let mut paths: Vec<String> = view.nodes.whole().paths().map(Cow::into_owned).collect();
		paths.sort_unstable();
		paths
	};
	let all = [
		"build",
		"build/deep",
		"build/deep/x.bin",
		"build/o.bin",
		"keep.txt",
	];

	// Nothing the rules hide was decided: the whole ignored subtree is still carried, and the
	// pass reports no root of its own (the carried facts hold the ones earlier passes found).
	let mut carried = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	carried.filter_changed(filter(), &decided(&["keep.txt"]));
	assert_eq!(
		sorted(&carried),
		all,
		"a node no producer moved must not cost the pass a rule match"
	);
	assert!(carried.ignored.is_empty(), "{:?}", carried.ignored);

	// The directory itself decided — a remote move of it into view — takes its subtree with it.
	let mut moved = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	moved.filter_changed(filter(), &decided(&["build"]));
	assert_eq!(sorted(&moved), ["keep.txt"]);
	assert_eq!(moved.ignored.keys().collect::<Vec<_>>(), vec!["build"]);

	// One item created under an ALREADY ignored directory: only that key was decided, so only
	// it leaves — and the root recorded is the directory the rule names, not the item.
	let mut created = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	created.filter_changed(filter(), &decided(&["build/deep/x.bin"]));
	assert_eq!(
		sorted(&created),
		["build", "build/deep", "build/o.bin", "keep.txt"]
	);
	assert_eq!(created.ignored.keys().collect::<Vec<_>>(), vec!["build"]);

	// The same three inputs read WHOLE: every hidden node goes, whatever was decided. This is
	// what the narrowed form is allowed to differ from, and the difference is the saving.
	let mut whole = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	whole.filter(Some(filter()));
	assert_eq!(sorted(&whole), ["keep.txt"]);
}

/// The narrowed collision check folds each decided key against the other decided keys, the held
/// paths and the CARRIED keys — the last asked of the baseline rather than by scanning the
/// view — and refuses the pass on any of the three.
///
/// The last case in this test is the one the narrowing deliberately does not check: two keys
/// the pass merely carried. A pass that finds a collision refuses, and a refusal forces the
/// next pass to read whole, so no such pair can reach a scoped pass undetected. Pinned here so
/// that reasoning has to be revisited if the refusal ever stops forcing a whole read.
#[test]
fn the_narrowed_collision_check_folds_every_key_a_decided_one_can_meet() {
	let root = Uuid::new_v4();
	let lower = cacheable_file(root, "note.txt");
	let upper = RemoteItem {
		uuid: Uuid::new_v4(),
		name: "Note.txt".to_string(),
		..lower.clone()
	};
	let no_rows = Baseline::default();
	let decided = |paths: &[&str]| -> BTreeSet<String> {
		paths.iter().map(|path| (*path).to_string()).collect()
	};

	// Against a CARRIED key: the row is what the carried node is, so the baseline answers.
	let rows = HashMap::from([(
		"note.txt".to_string(),
		base_file("note.txt", lower.uuid, [3; 32]),
	)]);
	let carried_rows = tree(&rows);
	let mut against_carried = place_remote_items(
		root,
		PairRoot::Directory,
		&[],
		&[lower.clone(), upper.clone()],
		&[],
	);
	against_carried.resolve_collisions_changed(&carried_rows, &decided(&["Note.txt"]));
	assert!(against_carried.has_collisions);
	assert!(
		!against_carried.nodes.whole().holds("Note.txt"),
		"the decided key is the loser; the carried one the pass is not touching stays"
	);
	assert!(against_carried.nodes.whole().holds("note.txt"));

	// Against another DECIDED key, with no row behind either.
	let mut both_decided = place_remote_items(
		root,
		PairRoot::Directory,
		&[],
		&[lower.clone(), upper.clone()],
		&[],
	);
	both_decided.resolve_collisions_changed(&no_rows, &decided(&["Note.txt", "note.txt"]));
	assert!(both_decided.has_collisions);
	assert_eq!(
		both_decided.nodes.whole().len(),
		1,
		"one of the twins leaves"
	);

	// Against a HELD path, which took its key when the view held it.
	let mut against_held = place_remote_items(
		root,
		PairRoot::Directory,
		&[],
		std::slice::from_ref(&upper),
		&[],
	);
	against_held.held_paths = decided(&["note.txt"]);
	against_held.resolve_collisions_changed(&no_rows, &decided(&["Note.txt"]));
	assert!(against_held.has_collisions);
	assert!(!against_held.nodes.whole().holds("Note.txt"));

	// Two keys the pass merely CARRIED: not folded, by the induction above.
	let mut carried_only = place_remote_items(root, PairRoot::Directory, &[], &[lower, upper], &[]);
	carried_only.resolve_collisions_changed(&carried_rows, &BTreeSet::new());
	assert!(
		!carried_only.has_collisions,
		"a pass that found this pair would have refused and forced the next read whole"
	);
	assert_eq!(carried_only.nodes.whole().len(), 2);
}

/// The narrowed filter reads the rows at the decided keys once for both of its halves: the
/// rules need each key's node, and whether the view holds a key is what that read already
/// answered. After a directory rename those keys are every row under the old name, which the
/// collision check then reads once more out of the folded index, for the carried keys a
/// decided one could fold onto.
#[test]
fn a_scoped_filter_reads_the_decided_rows_once_for_both_of_its_halves() {
	const FILES: usize = 2_000;
	let files: Vec<(String, String)> = (0..FILES)
		.map(|n| (format!("dir/{n:05}.txt"), format!("moved_dir/{n:05}.txt")))
		.collect();
	let baseline = Baseline::from_rows(
		std::iter::once(base_dir("dir", Uuid::new_v4())).chain(
			files
				.iter()
				.map(|(from, _)| base_file(from, Uuid::new_v4(), [1; 32])),
		),
	);
	// Both ends of every re-keyed path, as a pass records them; the remote holds the old ends.
	let mut decided = BTreeSet::from(["dir".to_string(), "moved_dir".to_string()]);
	for (from, to) in &files {
		decided.extend([from.clone(), to.clone()]);
	}
	let rules = IgnoreRules::default();
	let mut view = RemoteView {
		nodes: Side::carried(),
		has_collisions: false,
		held_paths: BTreeSet::new(),
		skipped: Vec::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
	};
	let before = baseline.reads_for_test();
	view.filter_changed(
		ViewFilter {
			rules: &rules,
			baseline: &baseline,
		},
		&decided,
	);
	let after = baseline.reads_for_test();
	assert!(!view.has_collisions);
	let (statements, rows) = (after.0 - before.0, after.1 - before.1);
	assert!(
		rows < 5 * FILES / 2,
		"the filter read {rows} row(s) in {statements} statement(s): more than twice the \
			 {FILES} file(s) under the renamed directory"
	);
}

/// A root pattern of `*` empties the filtered view, rule file included, while the unfiltered one
/// still lists the remote: the pass reads its rule files and an emptied remote from that one, or
/// every such pass would read as a vanished remote.
#[test]
fn ignoring_everything_empties_only_the_filtered_view() {
	let root = Uuid::new_v4();
	let sub = remote_dir("sub", root);
	let files = [
		cacheable_file(root, ".filenignore"),
		cacheable_file(sub.uuid, "f.txt"),
	];
	let dirs = std::slice::from_ref(&sub);
	let raw = build_remote_view(root, PairRoot::Directory, dirs, &files, &[], None);
	assert_eq!(raw.nodes.whole().len(), 3);
	assert!(raw.nodes.whole().holds(".filenignore"));

	let view = build_remote_view(
		root,
		PairRoot::Directory,
		dirs,
		&files,
		&[],
		Some(ViewFilter {
			rules: &root_rules("*"),
			baseline: &Baseline::default(),
		}),
	);
	assert!(
		view.nodes.whole().is_empty(),
		"{:?}",
		view.nodes.whole().paths().collect::<Vec<_>>()
	);
	assert_eq!(
		view.ignored.keys().collect::<Vec<_>>(),
		vec![".filenignore", "sub"]
	);
}

/// The streamed build answers what the materialized one answers — the nodes, the held paths
/// and the records for everything that could not be placed — including when every item
/// arrives BEFORE the directory that holds it. That order is the one thing a read can do to a
/// builder that a pair of slices cannot, and the statement promises no other.
#[test]
fn a_streamed_view_is_the_view_the_slices_build() {
	let root = Uuid::new_v4();
	let a = remote_dir("A", root);
	let deep = remote_dir("deep", a.uuid);
	// Legal on the server, unusable as one local path component: placed nowhere, and nothing
	// under it is placeable either.
	let bad = remote_dir("bad\\x", root);
	let leaf = cacheable_file(deep.uuid, "leaf.txt");
	let under_bad = cacheable_file(bad.uuid, "file.txt");
	let orphan = cacheable_file(Uuid::new_v4(), "orphan.txt");
	let undecodable = [UndecodableItem {
		uuid: Uuid::new_v4(),
		parent: deep.uuid,
		stable_uuid: None,
	}];

	let dirs = [a.clone(), deep.clone(), bad.clone()];
	let files = [leaf.clone(), under_bad.clone(), orphan.clone()];
	let materialized = place_remote_items(root, PairRoot::Directory, &dirs, &files, &undecodable);
	assert!(
		materialized.nodes.whole().holds("A/deep/leaf.txt") && materialized.skipped.len() == 4,
		"the fixture has to exercise placement AND every way of failing it: {:?} {:?}",
		materialized.nodes.whole().paths().collect::<Vec<_>>(),
		materialized.skipped,
	);

	let mut builder = ViewBuilder::with_capacity(root, PairRoot::Directory, 0);
	crate::cache::SnapshotSink::undecodable(&mut builder, undecodable.to_vec());
	// Children first, every parent last: the worst order a read could hand these over in.
	for (is_dir, item) in [
		(false, &leaf),
		(false, &under_bad),
		(false, &orphan),
		(true, &deep),
		(true, &bad),
		(true, &a),
	] {
		crate::cache::SnapshotSink::item(&mut builder, is_dir, item.clone());
	}
	let streamed = builder.finish();

	assert_eq!(streamed.nodes, materialized.nodes, "the same node set");
	assert_eq!(streamed.held_paths, materialized.held_paths);
	// The records are the same set; only their ORDER follows the order the rows arrived in,
	// and nothing reads them in order (`unknown_remote_paths` keys them by path).
	let sorted = |mut skipped: Vec<SkippedRemote>| {
		skipped.sort_by(|left, right| {
			(&left.rel_path, left.remote_uuid).cmp(&(&right.rel_path, right.remote_uuid))
		});
		skipped
	};
	assert_eq!(sorted(streamed.skipped), sorted(materialized.skipped));
}

/// A file recorded under an unplaceable DIRECTORY takes that directory's path, so the rules have
/// to be asked about that path as the directory it is. A rule written for directories only
/// (`bad*/`) hides the directory; asked as a file it missed, the record outlived the directory
/// it describes, and every pass then reported the path the rule exists to silence.
#[test]
fn a_directory_only_rule_hides_what_was_recorded_under_an_unplaceable_directory() {
	let root = Uuid::new_v4();
	// Legal on the server, unusable as one local path component, so the view cannot place it.
	let bad = remote_dir("bad\\x", root);
	let child = cacheable_file(bad.uuid, "file.txt");
	let dirs = [bad];
	let files = [child];

	let placed = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	assert!(
		placed.nodes.whole().is_empty(),
		"{:?}",
		placed.nodes.whole().paths().collect::<Vec<_>>()
	);
	assert_eq!(
		placed.skipped.len(),
		2,
		"the directory and the file that takes its record: {:?}",
		placed.skipped
	);
	assert!(
		placed.skipped.iter().all(|skip| skip.path_is_dir),
		"both records name the directory's path, so both are directory paths: {:?}",
		placed.skipped
	);

	let mut view = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	view.filter(Some(ViewFilter {
		rules: &root_rules("bad*/"),
		baseline: &Baseline::default(),
	}));
	assert!(
		view.skipped.is_empty(),
		"the rule hides the directory, so nothing recorded under it is unsyncable: {:?}",
		view.skipped
	);
}

/// The placed view keeps BOTH halves of a case-only pair — the consumers that read the view
/// before it is filtered see the remote as it is — and the filter is what resolves them and
/// refuses the pass.
#[test]
fn the_placed_view_keeps_both_case_twins_and_the_filter_resolves_them() {
	let root = Uuid::new_v4();
	let lower = cacheable_file(root, "note.txt");
	let upper = RemoteItem {
		uuid: Uuid::new_v4(),
		name: "Note.txt".to_string(),
		..lower.clone()
	};
	let files = [lower, upper];

	let mut view = place_remote_items(root, PairRoot::Directory, &[], &files, &[]);
	let mut placed: Vec<String> = view
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	placed.sort_unstable();
	assert_eq!(placed, vec!["Note.txt", "note.txt"]);
	assert!(
		!view.has_collisions,
		"placement itself refuses nothing: the reads that come before the filter are about the \
			 remote as it is"
	);

	view.filter(None);
	assert!(view.has_collisions, "the filter is what refuses the pass");
	assert_eq!(view.nodes.whole().len(), 1, "the loser leaves the view");
}

/// A path the cache lists twice byte-identically is HELD, and stays held where a rule hides it:
/// both halves are hidden together and nothing can act on the path, so the pass that finds the
/// cache no longer mid-transition there is the one that untracks it and reports its rule. Until
/// then the path is in neither `nodes` nor `ignored`.
#[test]
fn a_held_duplicate_the_rules_hide_stays_held_and_is_not_reported_as_ignored() {
	let root = Uuid::new_v4();
	let build = remote_dir("build", root);
	let twin = cacheable_file(build.uuid, "out.log");
	let second = RemoteItem {
		uuid: Uuid::new_v4(),
		..twin.clone()
	};
	let dirs = [build];
	let files = [twin, second];

	let mut view = place_remote_items(root, PairRoot::Directory, &dirs, &files, &[]);
	assert_eq!(
		view.held_paths
			.iter()
			.map(String::as_str)
			.collect::<Vec<_>>(),
		vec!["build/out.log"]
	);
	assert!(!view.nodes.whole().holds("build/out.log"));

	view.filter(Some(ViewFilter {
		rules: &root_rules("build/"),
		baseline: &Baseline::default(),
	}));
	assert_eq!(
		view.held_paths
			.iter()
			.map(String::as_str)
			.collect::<Vec<_>>(),
		vec!["build/out.log"],
		"a held path stays held whatever the rules say"
	);
	assert_eq!(
		view.ignored.keys().collect::<Vec<_>>(),
		vec!["build"],
		"the held path itself was never a node, so no rule reports it"
	);
}

#[test]
fn build_remote_view_excludes_the_quarantine_dir_name() {
	let root = Uuid::new_v4();
	let trash = remote_dir(QUARANTINE_DIR, root);
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		std::slice::from_ref(&trash),
		&[],
		&[],
		None,
	);
	assert!(
		view.nodes.whole().is_empty(),
		"a remote folder named like the quarantine dir must be excluded from the view"
	);
}

/// A remote item named like filen-sync's trash is in no view, at the root or nested, and nor is
/// anything under it — so nothing there is downloaded, or trashed for being absent locally. Only
/// the exact name is reserved: a longer name or another case is synced like any item.
#[test]
fn build_remote_view_excludes_filen_syncs_trash_at_any_depth_by_its_exact_name() {
	let root = Uuid::from_u128(1);
	let top_trash = remote_dir(LEGACY_TRASH_DIR, root);
	let photos = remote_dir("Photos", root);
	let nested_trash = remote_dir(LEGACY_TRASH_DIR, photos.uuid);
	let deep = remote_dir("deep", nested_trash.uuid);
	let other_case = remote_dir(".Filen.Trash.Local", root);
	let files = [
		cacheable_file(top_trash.uuid, "old.txt"),
		cacheable_file(deep.uuid, "x.jpg"),
		// A file with the folder's name is reserved too.
		cacheable_file(root, LEGACY_TRASH_DIR),
		cacheable_file(root, ".filen.trash.local.txt"),
		cacheable_file(other_case.uuid, "y.txt"),
	];
	let view = build_remote_view(
		root,
		PairRoot::Directory,
		&[top_trash, photos, nested_trash, deep, other_case],
		&files,
		&[],
		None,
	);

	let mut paths: Vec<String> = view
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(
		paths,
		vec![
			".Filen.Trash.Local",
			".Filen.Trash.Local/y.txt",
			".filen.trash.local.txt",
			"Photos",
		]
	);
	assert!(
		view.skipped.is_empty(),
		"filen-sync's trash is left out without a record: {:?}",
		view.skipped
	);
	assert!(!view.has_collisions);
}

/// The account's data directory is in no view of a pair rooted at the drive, in any case, and nor
/// is anything under it — not even a record of an item it could not place. A `.filen` deeper down,
/// a longer name, and the same tree under a pair rooted at a directory are the user's.
#[test]
fn build_remote_view_excludes_the_account_data_directory_at_the_top_of_a_drive_rooted_pair() {
	let root = Uuid::from_u128(1);
	let account = remote_dir(ACCOUNT_DATA_DIR, root);
	let chats = remote_dir("chats", account.uuid);
	let photos = remote_dir("Photos", root);
	let nested = remote_dir(ACCOUNT_DATA_DIR, photos.uuid);
	let files = [
		cacheable_file(chats.uuid, "a.bin"),
		// An unsafe name, which anywhere else is recorded as unsyncable.
		cacheable_file(chats.uuid, ".."),
		cacheable_file(nested.uuid, "x.jpg"),
		cacheable_file(root, ".filen.txt"),
	];
	let dirs = [account, chats, photos, nested];
	let paths = |view: &RemoteView| {
		let mut paths: Vec<String> = view
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		paths
	};

	let drive = build_remote_view(root, PairRoot::Drive, &dirs, &files, &[], None);
	assert_eq!(
		paths(&drive),
		vec![
			".filen.txt",
			"Photos",
			"Photos/.filen",
			"Photos/.filen/x.jpg"
		]
	);
	assert!(
		drive.skipped.is_empty(),
		"the account's data directory is left out without a record: {:?}",
		drive.skipped
	);
	assert!(!drive.has_collisions);
	// The server dedups the drive root's names case-insensitively, so this is that directory too.
	let other_case = remote_dir(".Filen", root);
	let under = cacheable_file(other_case.uuid, "y.txt");
	let view = build_remote_view(root, PairRoot::Drive, &[other_case], &[under], &[], None);
	assert_eq!(paths(&view), Vec::<String>::new());

	let directory = build_remote_view(root, PairRoot::Directory, &dirs, &files, &[], None);
	assert_eq!(
		paths(&directory),
		vec![
			".filen",
			".filen.txt",
			".filen/chats",
			".filen/chats/a.bin",
			"Photos",
			"Photos/.filen",
			"Photos/.filen/x.jpg",
		]
	);
	assert_eq!(directory.skipped.len(), 1, "{:?}", directory.skipped);
}

#[test]
fn to_event_maps_each_action_to_its_in_progress_event() {
	assert_eq!(
		SyncAction::UploadFile {
			rel_path: "a.txt".into()
		}
		.to_event(),
		SyncEvent::Uploading {
			rel_path: "a.txt".into()
		}
	);
	assert_eq!(
		SyncAction::DownloadFile {
			rel_path: "b.txt".into(),
			remote_uuid: Uuid::nil(),
		}
		.to_event(),
		SyncEvent::Downloading {
			rel_path: "b.txt".into()
		}
	);
	assert_eq!(
		SyncAction::TrashRemote {
			rel_path: "gone".into(),
			kind: NodeKind::File,
			remote_uuid: Uuid::nil(),
		}
		.to_event(),
		SyncEvent::TrashingRemote {
			rel_path: "gone".into()
		}
	);
	assert_eq!(
		SyncAction::MoveRemote {
			from_path: "x".into(),
			to_path: "y".into(),
			kind: NodeKind::File,
			remote_uuid: Uuid::nil(),
		}
		.to_event(),
		SyncEvent::MovingRemote {
			from: "x".into(),
			to: "y".into(),
		}
	);
}

#[test]
fn two_way_pushes_local_edit_and_pulls_remote_edit() {
	let uuid = Uuid::new_v4();
	// Local edited (hash diverged from baseline), remote unchanged -> push.
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::UploadFile {
			rel_path: "a.txt".to_string(),
		}]
	);

	// Remote edited (new uuid), local unchanged -> pull.
	let new_uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
	let local = map(vec![("a.txt", local_file("a.txt", [0; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [1; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::DownloadFile {
			rel_path: "a.txt".to_string(),
			remote_uuid: new_uuid,
		}]
	);
}

#[test]
fn two_way_both_sides_diverge_is_a_conflict_but_same_edit_converges() {
	let uuid = Uuid::new_v4();
	let new_uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);

	// Both edited to DIFFERENT content -> conflict.
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [2; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "a.txt".to_string(),
		}]
	);

	// Both edited to the SAME content -> converged (no conflict, no transfer); the now-stale
	// baseline is refreshed to the converged state via AdoptBaseline.
	let local = map(vec![("a.txt", local_file("a.txt", [9; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", new_uuid, [9; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::AdoptBaseline {
			rel_path: "a.txt".to_string(),
		}],
		"identical concurrent edits converge without a conflict and re-baseline"
	);
}

#[test]
fn two_way_propagates_a_local_delete_to_the_remote() {
	let uuid = Uuid::new_v4();
	let baseline = map(vec![("a.txt", base_file("a.txt", uuid, [0; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
	// Local deleted (absent), remote unchanged -> trash remote.
	let actions = plan(SyncMode::TwoWay, &baseline, &HashMap::new(), &remote);
	assert_eq!(
		actions,
		vec![SyncAction::TrashRemote {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: uuid,
		}]
	);
}

/// A directory deleted locally is planned per item, which is what the guard counts, and the
/// fold leaves the one trash of the directory for the apply: the server takes the subtree with
/// it, and trashing the files first left each one loose in the Filen trash.
#[test]
fn a_deleted_directory_is_planned_per_item_and_trashed_whole() {
	let ids = [1, 2, 3, 4].map(Uuid::from_u128);
	let baseline = map(vec![
		("docs", base_dir("docs", ids[0])),
		("docs/a.txt", base_file("docs/a.txt", ids[1], [1; 32])),
		("docs/sub", base_dir("docs/sub", ids[2])),
		(
			"docs/sub/c.txt",
			base_file("docs/sub/c.txt", ids[3], [3; 32]),
		),
	]);
	let remote = map(vec![
		("docs", remote_dir_node("docs", ids[0])),
		("docs/a.txt", remote_file("docs/a.txt", ids[1], [1; 32])),
		("docs/sub", remote_dir_node("docs/sub", ids[2])),
		(
			"docs/sub/c.txt",
			remote_file("docs/sub/c.txt", ids[3], [3; 32]),
		),
	]);
	let trash = |rel_path: &str, kind, id| SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: id,
	};
	let mut actions = plan(SyncMode::LocalToRemote, &baseline, &HashMap::new(), &remote);
	assert_eq!(
		actions,
		vec![
			trash("docs/sub/c.txt", NodeKind::File, ids[3]),
			trash("docs/sub", NodeKind::Dir, ids[2]),
			trash("docs/a.txt", NodeKind::File, ids[1]),
			trash("docs", NodeKind::Dir, ids[0]),
		]
	);
	let subsumed = fold_subsumed_deletes(&mut actions);
	assert_eq!(actions, vec![trash("docs", NodeKind::Dir, ids[0])]);
	assert_eq!(
		subsumed.under(&actions[0]),
		["docs/sub/c.txt", "docs/sub", "docs/a.txt"],
		"the directory's apply drops the rows of every deletion it took"
	);
}

/// Each deletion under a deleted directory goes to the top-most deleted directory above it, the
/// one no deletion above takes along — on the local side as on the remote.
#[test]
fn a_deletion_goes_to_the_top_most_deleted_directory_above_it() {
	let delete = |rel_path: &str, kind| SyncAction::DeleteLocal {
		rel_path: rel_path.to_string(),
		kind,
	};
	let mut actions = vec![
		delete("pics/sub/q.png", NodeKind::File),
		delete("pics/sub", NodeKind::Dir),
		delete("pics/p.png", NodeKind::File),
		delete("pics", NodeKind::Dir),
	];
	let subsumed = fold_subsumed_deletes(&mut actions);
	assert_eq!(actions, vec![delete("pics", NodeKind::Dir)]);
	assert_eq!(
		subsumed.under(&actions[0]),
		["pics/sub/q.png", "pics/sub", "pics/p.png"]
	);
	assert!(
		subsumed
			.under(&delete("pics/sub", NodeKind::Dir))
			.is_empty()
	);
}

/// A directory deletion takes only its own side's deletions under it: the other side's at the
/// same paths, and a sibling that only shares the directory's name as a prefix, stay.
#[test]
fn a_directory_deletion_leaves_the_other_sides_and_a_prefix_siblings_alone() {
	let trash = |rel_path: &str, kind| SyncAction::TrashRemote {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: Uuid::from_u128(1),
	};
	let other_side = SyncAction::DeleteLocal {
		rel_path: "docs/e.txt".to_string(),
		kind: NodeKind::File,
	};
	let mut actions = vec![
		trash("docs/x.txt", NodeKind::File),
		trash("docs2/d.txt", NodeKind::File),
		trash("docs", NodeKind::Dir),
		other_side.clone(),
	];
	let subsumed = fold_subsumed_deletes(&mut actions);
	assert_eq!(
		actions,
		vec![
			trash("docs2/d.txt", NodeKind::File),
			trash("docs", NodeKind::Dir),
			other_side,
		]
	);
	assert_eq!(
		subsumed.under(&trash("docs", NodeKind::Dir)),
		["docs/x.txt"]
	);
}

/// The paths a pass decides are the union of all three sides, and the baseline's third of that
/// union is the one part the two side maps do not already carry: a row for a path NEITHER side
/// holds any more. Those rows are contributed by a walk of the tree rather than by materializing
/// every row's path, so it is worth pinning that such a row is still reached — at the top level
/// and nested under a directory both sides still hold — beside the keys the maps do carry.
#[test]
fn a_row_neither_side_holds_is_reconciled_beside_the_keys_the_sides_carry() {
	let (dir, kept, nested, top) = (
		Uuid::new_v4(),
		Uuid::new_v4(),
		Uuid::new_v4(),
		Uuid::new_v4(),
	);
	let baseline = map(vec![
		("d", base_dir("d", dir)),
		("d/kept.txt", base_file("d/kept.txt", kept, [1; 32])),
		// Gone from both sides, under a directory both sides still hold ...
		("d/gone.txt", base_file("d/gone.txt", nested, [2; 32])),
		// ... and at the top level.
		("gone.txt", base_file("gone.txt", top, [3; 32])),
	]);
	let local = map(vec![
		("d", local_dir("d")),
		("d/kept.txt", local_file("d/kept.txt", [1; 32])),
		// A key only one side carries, which no baseline row names.
		("d/fresh.txt", local_file("d/fresh.txt", [4; 32])),
	]);
	let remote = map(vec![
		("d", remote_dir_node("d", dir)),
		("d/kept.txt", remote_file("d/kept.txt", kept, [1; 32])),
	]);

	let mut actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	actions.sort_by(|a, b| a.rel_path().cmp(b.rel_path()));
	assert_eq!(
		actions,
		vec![
			SyncAction::UploadFile {
				rel_path: "d/fresh.txt".to_string(),
			},
			SyncAction::AdoptBaseline {
				rel_path: "d/gone.txt".to_string(),
			},
			SyncAction::AdoptBaseline {
				rel_path: "gone.txt".to_string(),
			},
		],
		"a row neither side holds is retired; a path both sides hold unchanged plans nothing"
	);
}

#[test]
fn conflicted_baseline_rows_are_re_reported_but_never_acted_on() {
	let uuid = Uuid::new_v4();
	let mut entry = base_file("a.txt", uuid, [0; 32]);
	entry.state = BaselineState::Conflicted;
	let baseline = map(vec![("a.txt", entry)]);
	let local = map(vec![("a.txt", local_file("a.txt", [1; 32]))]);
	let remote = map(vec![("a.txt", remote_file("a.txt", uuid, [0; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "a.txt".to_string()
		}],
		"a held conflict is re-reported, never re-acted on"
	);
}

#[test]
fn a_conflicted_baseline_row_holds_its_whole_subtree() {
	// `thing` is held in conflict as a FILE; locally it is now a directory with a child. The
	// child must not be planned — the remote dir `thing` cannot exist while the held file does,
	// so the upload would fail with a missing remote parent.
	let uuid = Uuid::new_v4();
	let mut held = base_file("thing", uuid, [0; 32]);
	held.state = BaselineState::Conflicted;
	let baseline = map(vec![("thing", held)]);
	let local = map(vec![
		("thing", local_dir("thing")),
		("thing/child.txt", local_file("thing/child.txt", [1; 32])),
	]);
	let remote = map(vec![("thing", remote_file("thing", uuid, [0; 32]))]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "thing".to_string()
		}],
		"nothing under a held conflict may be planned"
	);
}

#[test]
fn a_held_conflict_is_never_laundered_into_a_remote_move() {
	// The conflicting remote item is renamed server-side while the conflict is still held. Move
	// detection matches by uuid and must NOT consume the held path: doing so would rename the
	// local copy, drop the Conflicted row and re-anchor the baseline to the remote's content,
	// after which the next pass reads the untouched local bytes as a one-sided edit and pushes
	// them over the remote's — silently destroying one side of an unresolved conflict.
	let uuid = Uuid::new_v4();
	let mut held = base_file("thing", uuid, [1; 32]);
	held.state = BaselineState::Conflicted;
	let baseline = map(vec![("thing", held)]);
	let local = map(vec![("thing", local_file("thing", [1; 32]))]);
	let remote = map(vec![(
		"thing_renamed",
		remote_file("thing_renamed", uuid, [2; 32]),
	)]);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		actions.contains(&SyncAction::Conflict {
			rel_path: "thing".to_string()
		}),
		"the held conflict must still be reported: {actions:?}"
	);
	assert!(
		!actions.iter().any(|a| matches!(
			a,
			SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. }
		)),
		"a held conflict must never be consumed as a move: {actions:?}"
	);
}

#[test]
fn a_held_conflict_is_never_laundered_into_a_local_move() {
	// Mirror direction: the local half of a held conflict is deleted and its bytes reappear at a
	// new local path. Content-hash move detection must not re-parent the still-divergent remote
	// file onto that name and mark the row Synced, which would mask the conflict for good.
	let uuid = Uuid::new_v4();
	let mut held = base_file("thing", uuid, [1; 32]);
	held.state = BaselineState::Conflicted;
	let baseline = map(vec![("thing", held)]);
	let local = map(vec![("moved.txt", local_file("moved.txt", [1; 32]))]);
	let remote = map(vec![("thing", remote_file("thing", uuid, [2; 32]))]);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert!(
		actions.contains(&SyncAction::Conflict {
			rel_path: "thing".to_string()
		}),
		"the held conflict must still be reported: {actions:?}"
	);
	assert!(
		!actions.iter().any(|a| matches!(
			a,
			SyncAction::MoveLocal { .. } | SyncAction::MoveRemote { .. }
		)),
		"a held conflict must never be consumed as a move: {actions:?}"
	);
}

#[test]
fn a_conflict_surfaced_this_pass_suppresses_its_descendants() {
	// Same shape, but with NO persisted Conflicted row yet: the conflict on `thing` is surfaced
	// by THIS pass, so its descendants must already be suppressed (they would otherwise error
	// in apply before the row is ever written).
	let uuid = Uuid::new_v4();
	let baseline = map(vec![("thing", base_file("thing", uuid, [0; 32]))]);
	let local = map(vec![
		("thing", local_dir("thing")),
		("thing/child.txt", local_file("thing/child.txt", [1; 32])),
	]);
	// Remote edited `thing` (new uuid) while local flipped it to a dir -> both sides changed.
	let remote = map(vec![(
		"thing",
		remote_file("thing", Uuid::new_v4(), [2; 32]),
	)]);
	assert_eq!(
		plan(SyncMode::TwoWay, &baseline, &local, &remote),
		vec![SyncAction::Conflict {
			rel_path: "thing".to_string()
		}],
		"the conflict is reported, its subtree is not acted on"
	);
}

#[test]
fn a_conflict_does_not_suppress_an_unrelated_sibling_prefix() {
	// `thing` is held; `thing2/x.txt` merely shares a name PREFIX and must still be planned.
	let uuid = Uuid::new_v4();
	let mut held = base_file("thing", uuid, [0; 32]);
	held.state = BaselineState::Conflicted;
	let baseline = map(vec![("thing", held)]);
	let local = map(vec![
		("thing", local_file("thing", [1; 32])),
		("thing2", local_dir("thing2")),
		("thing2/x.txt", local_file("thing2/x.txt", [3; 32])),
	]);
	let remote = map(vec![("thing", remote_file("thing", uuid, [0; 32]))]);
	let actions = plan(SyncMode::TwoWay, &baseline, &local, &remote);
	assert_eq!(
		actions,
		vec![
			SyncAction::CreateRemoteDir {
				rel_path: "thing2".to_string()
			},
			SyncAction::UploadFile {
				rel_path: "thing2/x.txt".to_string()
			},
			SyncAction::Conflict {
				rel_path: "thing".to_string()
			},
		],
		"only the held path is withheld; a mere name-prefix sibling still syncs"
	);
}

/// The cache can list a re-upload's successor and its not-yet-trashed predecessor at once, under
/// the exact same name. Refusing the whole pass for that made every pass a no-op until the
/// cache caught up; only the one path is withheld.
#[test]
fn a_byte_identical_duplicate_remote_name_holds_only_that_path() {
	let root = Uuid::new_v4();
	let predecessor = cacheable_file(root, "note.txt");
	let successor = RemoteItem {
		uuid: Uuid::new_v4(),
		..predecessor.clone()
	};
	let sibling = cacheable_file(root, "other.txt");

	let view = build_remote_view(
		root,
		PairRoot::Directory,
		&[],
		&[predecessor, successor, sibling.clone()],
		&[],
		None,
	);

	assert!(
		!view.has_collisions,
		"a cache in transition must not refuse the pass"
	);
	assert_eq!(view.held_paths, BTreeSet::from(["note.txt".to_string()]));
	assert!(
		!view.nodes.whole().holds("note.txt"),
		"neither half of the transition may be reconciled against"
	);
	assert_eq!(
		view.nodes.whole().at("other.txt").unwrap().remote_uuid,
		sibling.uuid,
		"every other path still syncs"
	);
}

/// The counter-example that rules out seeking to the nearest key at or before the path: with `a`
/// and `a/b` recorded, the key before `a/c` is `a/b`, which is no ancestor of it, while the
/// ancestor `a` lies further back. Walking the path's own ancestors finds it.
#[test]
fn a_root_lookup_walks_ancestors_rather_than_seeking_to_the_nearest_key() {
	let roots: BTreeSet<String> = ["a", "a/b"].map(String::from).into();

	assert!(
		at_or_under_root(&roots, "a/c"),
		"`a` is an ancestor of `a/c`"
	);
	assert!(at_or_under_root(&roots, "a"), "a root is at itself");
	assert!(at_or_under_root(&roots, "a/b/deep"));
	// Neighbours that merely share the prefix are not under it.
	for outside in ["ab", "a0", "a.", "b", ""] {
		assert!(!at_or_under_root(&roots, outside), "{outside:?}");
	}
}

/// `""` is the pair root: the ancestor walk never yields it and its subtree range holds nothing,
/// which is exactly what `is_under(path, "")` says. A caller for which it means "everything is
/// under it" has to say so itself.
#[test]
fn the_pair_root_covers_only_itself() {
	let roots: BTreeSet<String> = ["", "a"].map(String::from).into();

	assert!(at_or_under_root(&roots, ""), "the pair root is at itself");
	assert!(!is_under("x/y", ""));
	assert!(roots.range(subtree_bounds("")).next().is_none());
}

/// The subtree range is the subtree and nothing else: `/` is 0x2F and `0` is 0x30, so the
/// half-open range stops exactly where the names that only share the prefix begin.
#[test]
fn a_subtree_range_holds_exactly_the_subtree() {
	let roots: BTreeSet<String> = ["a", "a/b", "a/b/c", "a.", "a0", "ab", "b"]
		.map(String::from)
		.into();

	let under: Vec<&str> = roots
		.range(subtree_bounds("a"))
		.map(String::as_str)
		.collect();

	assert_eq!(under, ["a/b", "a/b/c"]);
	assert!(roots.range(subtree_bounds("b")).next().is_none());
}

/// A case-only collision is a real one — the server does allow both names, and no 1:1 local
/// mapping exists — so the whole-pass refusal stays.
#[test]
fn a_case_only_remote_collision_still_refuses_the_pass() {
	let root = Uuid::new_v4();
	let lower = cacheable_file(root, "note.txt");
	let upper = RemoteItem {
		uuid: Uuid::new_v4(),
		name: "Note.txt".to_string(),
		..lower.clone()
	};

	let view = build_remote_view(root, PairRoot::Directory, &[], &[lower, upper], &[], None);

	assert!(
		view.has_collisions,
		"a case-only collision is not a transition"
	);
	assert!(view.held_paths.is_empty());
}

#[test]
fn build_remote_view_resolves_paths_and_flags_collisions() {
	let root = Uuid::new_v4();
	let sub = remote_dir("sub", root);
	let file = cacheable_file(sub.uuid, "f.txt");
	let orphan = RemoteItem {
		uuid: Uuid::new_v4(),
		parent: Uuid::new_v4(), // a parent not in the snapshot -> orphan, skipped
		..file.clone()
	};

	let view = build_remote_view(
		root,
		PairRoot::Directory,
		std::slice::from_ref(&sub),
		&[file.clone(), orphan.clone()],
		&[],
		None,
	);
	assert!(!view.has_collisions);
	let mut paths: Vec<String> = view
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort();
	assert_eq!(
		paths,
		vec!["sub", "sub/f.txt"],
		"orphan skipped, path resolved"
	);
	assert_eq!(
		view.skipped,
		vec![SkippedRemote {
			remote_uuid: orphan.uuid,
			stable_uuid: orphan.stable_uuid,
			rel_path: "f.txt".to_string(),
			path_is_dir: false,
			reason: UnsyncableReason::RemoteBrokenParent,
		}],
		"the orphan is recorded, not silently dropped"
	);
	assert_eq!(
		view.nodes.whole().at("sub/f.txt").unwrap().remote_uuid,
		file.uuid
	);
	assert_eq!(
		view.nodes.whole().at("sub/f.txt").unwrap().content_hash,
		Some(Blake3Hash::from([5; 32]))
	);
}

/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
/// Where the rows record each skipped item, as a pass resolves it off the store before it asks.
fn synced_for(baseline: &Baseline, skipped: &[SkippedRemote]) -> SyncedPaths {
	let (uuids, lineages) = skipped_ids(skipped);
	baseline.synced_paths(&uuids, &lineages)
}

fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
	Baseline::from_rows(rows.values().cloned())
}

/// A dirty-set reconcile plans exactly what a whole-map reconcile plans.
///
/// Generated rather than illustrative, because the narrowing is only safe by virtue of what the
/// DERIVATION guarantees about the paths it leaves out. So the cases are built the way a pass
/// builds them: [`derive::from_baseline`] carries the rows into the two maps, then mutations
/// stand in for what the pass observed, each recording the paths it moved off their row — which
/// is the whole of [`PassPaths::Changed`]'s contract. The corpus carries the shapes that break a
/// naive narrowing: every row state the baseline can hold, sibling names that fold together,
/// paths that are prefixes of one another, held conflicts, pushes no snapshot confirmed, moves
/// on either side, a directory rename for the fold to find, and a subtree moved with its files
/// for it to carry.
mod scoped {
	use rand::{Rng, SeedableRng, rngs::StdRng};

	use super::*;

	/// How many generated cases each property runs over.
	const CASES: u64 = 400;

	/// The paths a case is drawn from, `(path, is_dir)`: nesting, two pairs of names that fold
	/// together (`a`/`A`, `a/b.txt`/`a/B.txt`), a name that is a string prefix of a sibling's
	/// (`a/b` and `a/b.txt`) and of another directory's (`a`, `ab`), a file and a directory
	/// sharing a stem (`z`, `z.txt`), a rule file, and a childless directory spelled two ways
	/// (`empty`, `Empty`) for the directory-move fold to have something it can actually carry.
	const TREE: &[(&str, bool)] = &[
		("a", true),
		("a/b", true),
		("a/b/c.txt", false),
		("a/b.txt", false),
		("a/B.txt", false),
		("a/c", true),
		("a/c/d.txt", false),
		("ab", true),
		("ab/x.txt", false),
		("A", true),
		("A/y.txt", false),
		("d", true),
		("d/e", true),
		("d/e/f", true),
		("d/e/f/g.txt", false),
		("d/.filenignore", false),
		("top.txt", false),
		("Top.txt", false),
		// Two fold pairs an ASCII-only lowering would keep apart: `Ä`/`ä`, and
		// `İ`, whose fold is a character LONGER than the name it came from.
		("\u{c4}.txt", false),
		("\u{e4}.txt", false),
		("\u{130}.txt", false),
		("i\u{307}.txt", false),
		("z", true),
		("z/1.txt", false),
		("z.txt", false),
		("empty", true),
		("Empty", true),
	];

	const MODES: [SyncMode; 5] = [
		SyncMode::TwoWay,
		SyncMode::LocalToRemote,
		SyncMode::LocalBackup,
		SyncMode::RemoteToLocal,
		SyncMode::RemoteBackup,
	];

	/// The directories of [`TREE`] a staged subtree move carries (see [`stage_move`]): each
	/// holds a file, which a pushed move needs to be matched at all, and between them they
	/// reach every depth the tree has, a directory nested in another and a rule file.
	const MOVABLE: [&str; 6] = ["a/b", "a/c", "ab", "z", "d/e/f", "d"];

	/// What a generated baseline row records. Every state a row can be in, plus the two
	/// half-recorded shapes a resolution writes on purpose.
	#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
	enum Shape {
		/// No row at all: a path the baseline has never tracked.
		NoRow,
		Synced,
		/// A push no snapshot has confirmed yet.
		Unconfirmed,
		/// What a `KeepLocal` resolution leaves: the remote half cleared.
		NoRemoteHalf,
		/// What a `KeepRemote` resolution leaves: the local half cleared.
		NoLocalHalf,
		Conflicted,
		Overwritten,
		Adopted,
	}

	const SHAPES: [Shape; 8] = [
		Shape::NoRow,
		Shape::Synced,
		Shape::Unconfirmed,
		Shape::NoRemoteHalf,
		Shape::NoLocalHalf,
		Shape::Conflicted,
		Shape::Overwritten,
		Shape::Adopted,
	];

	/// What a generated observation found — one path moved off the node its row carries.
	#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
	enum Change {
		LocalEdit,
		LocalGone,
		LocalNew,
		LocalFlip,
		RemoteEdit,
		RemoteGone,
		RemoteNew,
		RemoteFlip,
		LocalMove,
		RemoteMove,
	}

	const CHANGES: [Change; 10] = [
		Change::LocalEdit,
		Change::LocalGone,
		Change::LocalNew,
		Change::LocalFlip,
		Change::RemoteEdit,
		Change::RemoteGone,
		Change::RemoteNew,
		Change::RemoteFlip,
		Change::LocalMove,
		Change::RemoteMove,
	];

	fn index_of(path: &str) -> usize {
		TREE.iter()
			.position(|(candidate, _)| *candidate == path)
			.expect("the generated tree holds that path")
	}

	fn kind_at(index: usize) -> NodeKind {
		if TREE[index].1 {
			NodeKind::Dir
		} else {
			NodeKind::File
		}
	}

	fn uuid_at(index: usize) -> Uuid {
		Uuid::from_u128(1000 + index as u128)
	}

	fn hash_at(index: usize, version: u8) -> [u8; 32] {
		let index = u8::try_from(index).expect("the tree has fewer than 256 paths");
		[index.wrapping_mul(7).wrapping_add(version); 32]
	}

	/// The row a converged pass would have written for `index`, bent into `shape`.
	fn row(index: usize, shape: Shape) -> Option<BaselineEntry> {
		let (path, is_dir) = TREE[index];
		let uuid = uuid_at(index);
		let mut entry = if is_dir {
			base_dir(path, uuid)
		} else {
			base_file(path, uuid, hash_at(index, 0))
		};
		match shape {
			Shape::NoRow => return None,
			Shape::Synced => {}
			Shape::Unconfirmed => entry.agreed_hash = None,
			Shape::NoRemoteHalf => {
				entry.remote_uuid = None;
				entry.remote_stable_uuid = None;
			}
			Shape::NoLocalHalf => {
				entry.local_mtime = None;
				entry.content_hash = None;
			}
			Shape::Conflicted => entry.state = BaselineState::Conflicted,
			Shape::Overwritten => entry.state = BaselineState::Overwritten,
			Shape::Adopted => entry.state = BaselineState::Adopted,
		}
		Some(entry)
	}

	fn local_node_at(index: usize, kind: NodeKind, version: u8) -> LocalNode {
		let (path, _) = TREE[index];
		match kind {
			NodeKind::Dir => local_dir(path),
			NodeKind::File => local_file(path, hash_at(index, version)),
		}
	}

	fn remote_node_at(index: usize, kind: NodeKind, version: u8) -> RemoteNode {
		let (path, _) = TREE[index];
		match kind {
			NodeKind::Dir => remote_dir_node(path, uuid_at(index)),
			// A new version of the same lineage where `version` moved, which is what an edit
			// announced on an existing file looks like.
			NodeKind::File => remote_version(
				path,
				uuid_at(index),
				Uuid::from_u128(9000 + index as u128 * 4 + u128::from(version)),
				hash_at(index, version),
			),
		}
	}

	fn at(path: &str, mut node: LocalNode) -> LocalNode {
		node.rel_path = path.to_string();
		node
	}

	fn at_remote(path: &str, mut node: RemoteNode) -> RemoteNode {
		node.rel_path = path.to_string();
		node
	}

	/// Put one observed change into the maps, and the paths it is evidence about into the set.
	fn observe(
		change: Change,
		index: usize,
		elsewhere: usize,
		baseline: &Baseline,
		local: &mut Side<LocalNode>,
		remote: &mut Side<RemoteNode>,
		decided: &mut BTreeSet<String>,
	) {
		let (path, _) = TREE[index];
		let (other, _) = TREE[elsewhere];
		let kind = kind_at(index);
		let flipped = match kind {
			NodeKind::Dir => NodeKind::File,
			NodeKind::File => NodeKind::Dir,
		};
		decided.insert(path.to_string());
		match change {
			Change::LocalEdit => {
				local.insert(path.to_string(), local_node_at(index, kind, 1));
			}
			Change::LocalGone => {
				local.remove(baseline, path);
			}
			Change::LocalNew => {
				local.insert(path.to_string(), local_node_at(index, kind, 0));
			}
			Change::LocalFlip => {
				local.insert(path.to_string(), local_node_at(index, flipped, 1));
			}
			Change::RemoteEdit => {
				remote.insert(path.to_string(), remote_node_at(index, kind, 1));
			}
			Change::RemoteGone => {
				remote.remove(baseline, path);
			}
			Change::RemoteNew => {
				remote.insert(path.to_string(), remote_node_at(index, kind, 0));
			}
			Change::RemoteFlip => {
				remote.insert(path.to_string(), remote_node_at(index, flipped, 1));
			}
			// A rename carries what the row records to another path. Both ends are evidence, so
			// both are decided — which is what the watcher's rename pair and the cache's
			// `Renamed` entry each give the pass.
			Change::LocalMove => {
				decided.insert(other.to_string());
				local.remove(baseline, path);
				local.insert(other.to_string(), at(other, local_node_at(index, kind, 0)));
			}
			Change::RemoteMove => {
				decided.insert(other.to_string());
				remote.remove(baseline, path);
				remote.insert(
					other.to_string(),
					at_remote(other, remote_node_at(index, kind, 0)),
				);
			}
		}
	}

	/// Move whatever the local side holds at and under `dir` to `moved`, as a pass that watched
	/// the rename records it: a tombstone at each source path, the node at its destination, and
	/// both ends decided. Staged after the random changes, so what they did inside `dir` moves
	/// with it: the fold meets a subtree that matches its rows exactly, one a local change broke,
	/// and one the remote changed underneath it.
	fn stage_move(
		dir: &str,
		baseline: &Baseline,
		local: &mut Side<LocalNode>,
		decided: &mut BTreeSet<String>,
	) {
		let mut paths = local.subtree_paths(baseline, dir);
		if local.of(baseline).holds(dir) {
			paths.push(dir.to_string());
		}
		for path in paths {
			let Some(node) = local.remove(baseline, &path) else {
				continue;
			};
			let to = moved_path(&path, dir, "moved")
				.expect("every path staged is the moved directory or under it");
			local.insert(to.clone(), at(&to, node));
			decided.insert(path);
			decided.insert(to);
		}
	}

	/// One generated case: what a change-scoped pass holds when it reaches the reconcile.
	struct Case {
		baseline: Baseline,
		local: Side<LocalNode>,
		remote: Side<RemoteNode>,
		decided: BTreeSet<String>,
		holds: PassHolds,
	}

	/// What the corpus exercised, so a generator that degenerated cannot pass by proving
	/// nothing.
	#[derive(Default)]
	struct Coverage {
		shapes: BTreeSet<Shape>,
		changes: BTreeSet<Change>,
		actions: usize,
		folds: usize,
		/// Directory moves pushed to the remote whose subtree holds a file: the fold that
		/// carries rows the remote side still names, which only [`stage_move`] reaches.
		pushed_folds: usize,
	}

	fn generate(seed: u64, seen: &mut Coverage) -> Case {
		let mut rng = StdRng::seed_from_u64(seed);
		// Decided up front because it constrains the rows: a case-only directory rename is only
		// ever folded where nothing at either end is withheld, and a row that records one side
		// only withholds its path (`derive::Derived::held`). Drawn here, applied below.
		let stage_rename = rng.random_range(0..3) == 0;
		let renamed = [index_of("empty"), index_of("Empty")];
		// The subtree a staged move carries, from a stream of its own so every draw of this one
		// lands where it did before the move was staged. Its rows are forced `Synced` for the
		// reason the rename's are: a pushed move is only folded out of a subtree the baseline
		// vouches for whole.
		let staged = {
			let mut staging = StdRng::seed_from_u64(seed ^ 0x6d6f_7665_6400_0000);
			(staging.random_range(0..3) == 0)
				.then(|| MOVABLE[staging.random_range(0..MOVABLE.len())])
		};
		let mut rows = Vec::new();
		for (index, &(path, _)) in TREE.iter().enumerate() {
			let shape = if stage_rename && renamed.contains(&index) {
				Shape::Synced
			} else {
				// Drawn whether or not the move overrides it, so the draws after it stay put.
				let drawn = SHAPES[rng.random_range(0..SHAPES.len())];
				match staged {
					Some(dir) if path == dir || is_under(path, dir) => Shape::Synced,
					_ => drawn,
				}
			};
			seen.shapes.insert(shape);
			if let Some(entry) = row(index, shape) {
				rows.push(entry);
			}
		}
		let baseline = Baseline::from_rows(rows);
		// Exactly as a pass assembles them: the rows carried into both maps, and every row that
		// cannot stand in for itself in the set and held.
		let derive::Derived {
			mut local,
			mut remote,
			mut decided,
			held,
			..
		} = derive::from_baseline(&baseline, BTreeSet::new());
		for _ in 0..rng.random_range(0..7) {
			let change = CHANGES[rng.random_range(0..CHANGES.len())];
			seen.changes.insert(change);
			observe(
				change,
				rng.random_range(0..TREE.len()),
				rng.random_range(0..TREE.len()),
				&baseline,
				&mut local,
				&mut remote,
				&mut decided,
			);
		}
		// One directory spelled `empty` on disk and `Empty` on the remote, for the fold to
		// find. Staged outright: hitting both halves of a case-only rename at random is rare,
		// and the fold is one of the shapes this has to cover.
		if stage_rename {
			local.insert("empty".to_string(), local_dir("empty"));
			local.remove(&baseline, "Empty");
			remote.insert(
				"Empty".to_string(),
				remote_dir_node("Empty", uuid_at(index_of("Empty"))),
			);
			remote.remove(&baseline, "empty");
			decided.insert("empty".to_string());
			decided.insert("Empty".to_string());
		}
		if let Some(dir) = staged {
			stage_move(dir, &baseline, &mut local, &mut decided);
		}
		let mut held_remote = held;
		// The cache showing one name twice withholds that path. A pass always decides such a
		// path: what withheld it was an announcement about it.
		if rng.random_range(0..4) == 0 {
			let path = TREE[rng.random_range(0..TREE.len())].0.to_string();
			decided.insert(path.clone());
			held_remote.insert(path);
		}
		// A path the set names that nothing sits on — a changelist entry for something created
		// and deleted between two passes. Spurious dirt must change nothing.
		if rng.random_range(0..3) == 0 {
			decided.insert(format!(
				"{}/never-synced",
				TREE[rng.random_range(0..TREE.len())].0
			));
		}
		Case {
			baseline,
			local,
			remote,
			decided,
			holds: PassHolds {
				held_remote,
				..PassHolds::default()
			},
		}
	}

	/// Both reconciles over one case's inputs, asserted equal down to the deferral count.
	fn assert_same_plan(seed: u64, mode: SyncMode, case: &Case, seen: &mut Coverage) {
		let Case {
			baseline,
			local,
			remote,
			decided,
			holds,
		} = case;
		let (local_ref, remote_ref) = (local.of(baseline), remote.of(baseline));
		let whole = reconcile(
			mode,
			baseline,
			&local_ref,
			&remote_ref,
			holds,
			PassPaths::Whole,
		);
		let scoped = reconcile(
			mode,
			baseline,
			&local_ref,
			&remote_ref,
			holds,
			PassPaths::Changed(decided),
		);
		assert_eq!(
			whole.actions, scoped.actions,
			"seed {seed}, {mode:?}: the narrowed reconcile planned something else"
		);
		assert_eq!(
			whole.deferred_paths, scoped.deferred_paths,
			"seed {seed}, {mode:?}: the narrowed reconcile counted different deferrals"
		);
		seen.actions += whole.actions.len();
	}

	#[test]
	fn a_dirty_set_reconcile_plans_what_a_whole_map_reconcile_plans() {
		let mut seen = Coverage::default();
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			for mode in MODES {
				assert_same_plan(seed, mode, &case, &mut seen);
			}
		}
		assert_eq!(seen.shapes.len(), SHAPES.len(), "{:?}", seen.shapes);
		assert_eq!(seen.changes.len(), CHANGES.len(), "{:?}", seen.changes);
		assert!(
			seen.actions > 1_000,
			"the corpus planned only {} action(s), so the two plans agreed on almost nothing",
			seen.actions
		);
	}

	/// The case as it stands once the directory-move fold has run over it — at the case's own
	/// scope when `scoped`, whole otherwise — the moves that took, and the case's own set with
	/// those moves replayed over it.
	///
	/// The replay is the set a whole fold, which is handed none, is read with afterwards. A
	/// scoped fold's own set is the replay less what a proven move carried unchanged, plus what
	/// the side a move did not carry recorded to stay put (see `Side::stay_put`).
	fn fold(
		case: &Case,
		mode: SyncMode,
		scoped: bool,
	) -> (Case, Vec<SyncAction>, BTreeSet<String>) {
		let mut baseline = case.baseline.clone();
		let mut local = copied(&case.local);
		let mut remote = copied(&case.remote);
		let mut decided = case.decided.clone();
		let moves = fold_dir_moves(
			mode,
			&mut baseline,
			&mut local,
			&mut remote,
			&case.holds.held_remote,
			scoped.then_some(&mut decided),
		)
		.unwrap();
		let replayed = rekeyed(case.decided.clone(), &moves);
		let decided = if scoped {
			decided
		} else {
			rekeyed(decided, &moves)
		};
		let folded = Case {
			baseline,
			local,
			remote,
			decided,
			holds: PassHolds {
				held_remote: case.holds.held_remote.clone(),
				..PassHolds::default()
			},
		};
		(folded, moves, replayed)
	}

	/// The same property after the directory-move fold a pass runs first, at the same scope the
	/// pass gives both of them. The fold re-keys the maps and the baseline together, so the set
	/// has to follow them — which is what `Prepared::fold_dir_moves` does for the real pass, and
	/// what this pins.
	#[test]
	fn a_dirty_set_reconcile_agrees_once_a_directory_move_is_folded() {
		let mut seen = Coverage::default();
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			for mode in MODES {
				let (after, moves, _) = fold(&case, mode, true);
				seen.folds += moves.len();
				assert_same_plan(seed, mode, &after, &mut seen);
			}
		}
		assert!(
			seen.folds > 100,
			"only {} directory move(s) were folded, so this tested almost nothing",
			seen.folds
		);
	}

	/// The rows a pass reads out of the store, held to the resident tree they used to be read
	/// out of, over every generated case: the case's own rows, then the same rows once the
	/// directory-move fold has run over them in each mode — the fold window, with the table
	/// still holding every moved row under its source — then a few more random moves and
	/// confirmations on top.
	///
	/// The expectation is the TREE, real code given the same rows and the same edits, not an
	/// answer this generator writes down: `rows::tests::assert_alike` asks both every question
	/// the boundary answers, at every path either side has held, every ancestor, every folded
	/// spelling and every string prefix that is no path prefix. The corpus is what reaches the
	/// shapes the hand-written ones cannot enumerate: case-only collisions, `Ä`/`ä` and `İ`,
	/// nested ignored roots, a decided path whose collision partner is not decided.
	#[test]
	fn the_store_backed_rows_answer_every_case_as_the_resident_tree_does() {
		use crate::sync_engine::{rows::tests as rows, tree::Tree};

		let mut seen = Coverage::default();
		let (mut folds, mut edits) = (0, 0);
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			let held: Vec<BaselineEntry> = case.baseline.iter().collect();
			let paths = || {
				TREE.iter()
					.map(|(path, _)| path.to_string())
					.chain(case.decided.iter().cloned())
					.chain(["moved".to_string(), "a/moved/deep".to_string()])
			};
			let unedited = Tree::from_rows(held.clone());
			rows::assert_alike(
				&format!("seed {seed}"),
				&case.baseline,
				&unedited,
				&rows::probes(paths()),
			);
			for mode in MODES {
				let (mut after, moves, _) = fold(&case, mode, true);
				let mut oracle = unedited.clone();
				for action in &moves {
					let (from, to) = action.endpoints();
					oracle.move_subtree(from, to);
				}
				folds += moves.len();
				let mut probes = rows::probes(paths().chain(oracle.paths()));
				rows::assert_alike(
					&format!("seed {seed} {mode:?}, {} fold(s)", moves.len()),
					&after.baseline,
					&oracle,
					&probes,
				);
				if mode != SyncMode::TwoWay {
					continue;
				}
				let mut rng = StdRng::seed_from_u64(seed ^ 0x5eed);
				for step in 0..3 {
					let (what, _) = rows::random_edit(
						&mut rng,
						step,
						&mut after.baseline,
						&mut oracle,
						&probes,
					);
					edits += 1;
					probes = rows::probes(probes.into_iter().chain(oracle.paths()));
					rows::assert_alike(
						&format!("seed {seed}, then {what}"),
						&after.baseline,
						&oracle,
						&probes,
					);
				}
			}
		}
		assert!(
			folds > 100 && edits > 1_000,
			"only {folds} fold(s) and {edits} edit(s): this tested almost nothing"
		);
	}

	/// The fold at the pass's scope carries exactly the moves a whole-map fold carries, and
	/// leaves the three inputs in the same shape.
	///
	/// This is the half the property above cannot see: it reconciles whatever the fold left, so
	/// a narrowed fold that MISSED a move would still agree with itself — the reconcile would
	/// simply plan the delete-and-create the fold exists to avoid, at both ends, and both
	/// reconciles would plan it alike.
	#[test]
	fn a_scoped_directory_move_fold_carries_what_a_whole_one_carries() {
		let mut seen = Coverage::default();
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			for mode in MODES {
				let (whole, by_whole, _) = fold(&case, mode, false);
				let (scoped, by_scope, _) = fold(&case, mode, true);
				seen.folds += by_whole.len();
				seen.pushed_folds += by_whole
					.iter()
					.filter(|action| pushed_with_files(&whole.baseline, action))
					.count();
				assert_eq!(
					by_whole, by_scope,
					"seed {seed}, {mode:?}: the narrowed fold carried other moves"
				);
				assert_eq!(
					whole.local, scoped.local,
					"seed {seed}, {mode:?}: the narrowed fold left another local side"
				);
				assert_eq!(
					whole.remote, scoped.remote,
					"seed {seed}, {mode:?}: the narrowed fold left another remote side"
				);
			}
		}
		assert!(
			seen.folds > 100,
			"only {} directory move(s) were folded, so this tested almost nothing",
			seen.folds
		);
		assert!(
			seen.pushed_folds > 50,
			"only {} pushed directory move(s) carried a file, so the carried subtree was barely \
				 tested",
			seen.pushed_folds
		);
	}

	/// The case's two sides as whole maps, read off its carried ones, less the shapes a whole
	/// pass never folds over and a whole map folds in no fixed order: a second spelling of a
	/// folded name on either side — the walk refuses it, the view's filter drops it — and a
	/// second remote directory of one uuid. The first of each in path order is kept.
	fn whole_maps(case: &Case) -> (HashMap<String, LocalNode>, HashMap<String, RemoteNode>) {
		fn first_of_each<T>(
			nodes: impl Iterator<Item = (String, T)>,
			dir_uuid: impl Fn(&T) -> Option<Uuid>,
		) -> HashMap<String, T> {
			let mut nodes: Vec<(String, T)> = nodes.collect();
			nodes.sort_by(|(left, _), (right, _)| left.cmp(right));
			let (mut names, mut uuids) = (HashSet::new(), HashSet::new());
			nodes
				.into_iter()
				.filter(|(path, node)| {
					names.insert(collision_key(path))
						&& dir_uuid(node).is_none_or(|uuid| uuids.insert(uuid))
				})
				.collect()
		}
		(
			first_of_each(
				case.local
					.of(&case.baseline)
					.iter()
					.map(|(path, node)| (path.into_owned(), node.into_owned())),
				|_| None,
			),
			first_of_each(
				case.remote
					.of(&case.baseline)
					.iter()
					.map(|(path, node)| (path.into_owned(), node.into_owned())),
				|node: &RemoteNode| (node.kind == NodeKind::Dir).then_some(node.remote_uuid),
			),
		)
	}

	/// The fold over the sides a whole pass stages carries exactly the moves the fold over
	/// whole maps carries, and leaves both sides and the rows as that one leaves them, over
	/// every generated case in every mode: read a row, two, seven and a page a statement, and
	/// re-keyed as many a statement.
	#[test]
	fn a_staged_directory_move_fold_carries_what_a_whole_map_one_carries() {
		let mut seen = Coverage::default();
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			let (local, remote) = whole_maps(&case);
			for mode in MODES {
				let mut rows = case.baseline.clone();
				let (mut whole_local, mut whole_remote) =
					(Side::from(local.clone()), Side::from(remote.clone()));
				let by_map = fold_dir_moves(
					mode,
					&mut rows,
					&mut whole_local,
					&mut whole_remote,
					&case.holds.held_remote,
					None,
				)
				.unwrap();
				seen.folds += by_map.len();
				seen.pushed_folds += by_map
					.iter()
					.filter(|action| pushed_with_files(&rows, action))
					.count();
				let (whole_local, whole_remote) =
					(whole_local.into_whole(), whole_remote.into_whole());
				let rows: Vec<BaselineEntry> = rows.iter().collect();
				for page in [1, 2, 7, 1024] {
					let context = format!("seed {seed}, {mode:?}, page {page}");
					let (_stages, mut staged_local, mut staged_remote) =
						staged_sides(local.values().cloned(), remote.values().cloned(), page);
					let mut staged_rows = case.baseline.clone();
					let by_stage = fold_dir_moves(
						mode,
						&mut staged_rows,
						&mut staged_local,
						&mut staged_remote,
						&case.holds.held_remote,
						None,
					)
					.unwrap();
					assert_eq!(
						by_map, by_stage,
						"{context}: the staged fold carried other moves"
					);
					assert_eq!(
						whole_local,
						staged_local.into_whole(),
						"{context}: another local side"
					);
					assert_eq!(
						whole_remote,
						staged_remote.into_whole(),
						"{context}: another remote side"
					);
					assert_eq!(
						rows,
						staged_rows.iter().collect::<Vec<_>>(),
						"{context}: other rows"
					);
				}
			}
		}
		assert!(
			seen.folds > 100,
			"only {} directory move(s) were folded, so this tested almost nothing",
			seen.folds
		);
		assert!(
			seen.pushed_folds > 50,
			"only {} pushed directory move(s) carried a file",
			seen.pushed_folds
		);
	}

	/// A whole pass's reconcile, window by window over its staged sides, plans what the resident
	/// reconcile plans over the same sides — every action, in order, and every deferral — over
	/// every generated case in every mode, before and after the directory-move fold, a window of
	/// one entry of each input, two, seven and sixty-four.
	///
	/// Nothing the corpus generates is left out: the resident reconcile is given the staged sides,
	/// as a whole pass gives them, so both read the remote in path order. Two files of one lineage,
	/// which a move finds in no fixed order over a map, are pinned on their own (see
	/// `stage::reconcile`'s tests).
	#[test]
	fn a_windowed_reconcile_plans_what_the_resident_one_plans() {
		let mut seen = Coverage::default();
		let (mut file_moves, mut deferred) = (0, 0);
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			let (local, remote) = whole_maps(&case);
			for mode in MODES {
				for folded in [false, true] {
					let (_stages, mut staged_local, mut staged_remote) =
						staged_sides(local.values().cloned(), remote.values().cloned(), 2);
					let mut rows = case.baseline.clone();
					if folded {
						seen.folds += fold_dir_moves(
							mode,
							&mut rows,
							&mut staged_local,
							&mut staged_remote,
							&case.holds.held_remote,
							None,
						)
						.unwrap()
						.len();
					}
					let resident = reconcile(
						mode,
						&rows,
						&staged_local.of(&rows),
						&staged_remote.of(&rows),
						&case.holds,
						PassPaths::Whole,
					);
					for window in [1, 2, 7, 64] {
						let context =
							format!("seed {seed}, {mode:?}, folded {folded}, window {window}");
						let windowed = windowed(
							mode,
							&rows,
							&staged_local,
							&staged_remote,
							&case.holds,
							window,
						);
						assert_eq!(
							resident.actions, windowed.actions,
							"{context}: the windowed reconcile planned something else"
						);
						assert_eq!(
							resident.deferred_paths, windowed.deferred_paths,
							"{context}: the windowed reconcile counted other deferrals"
						);
					}
					seen.actions += resident.actions.len();
					deferred += resident.deferred_paths;
					file_moves += resident
						.actions
						.iter()
						.filter(|action| {
							matches!(
								action,
								SyncAction::MoveLocal {
									kind: NodeKind::File,
									..
								} | SyncAction::MoveRemote {
									kind: NodeKind::File,
									..
								}
							)
						})
						.count();
				}
			}
		}
		assert!(
			seen.actions > 1_000,
			"the corpus planned only {} action(s)",
			seen.actions
		);
		assert!(
			file_moves > 50,
			"only {file_moves} file move(s) were joined, so the join was barely tested"
		);
		assert!(
			deferred > 50,
			"only {deferred} path(s) were deferred, so the held paths were barely tested"
		);
		assert!(
			seen.folds > 100,
			"only {} directory move(s) were folded first",
			seen.folds
		);
	}

	/// The signatures a pushed move is matched by, held to what they digest: every path under a
	/// directory, relative to it, with its kind, hash and size — the map each candidate's
	/// signature was before it was a digest. Over every generated case, every directory either
	/// the rows or the local side could hold, read all at once as the fold reads them: a
	/// signature is there exactly where the map is, says a file is under it exactly where the
	/// map holds one, two are equal exactly where their maps are, and the path-for-path
	/// comparisons that decide a match agree with the maps.
	#[test]
	fn signatures_are_equal_exactly_where_the_trees_they_digest_are() {
		type Tree = BTreeMap<String, (NodeKind, Option<Blake3Hash>, u64)>;
		fn tree_of(
			root: &str,
			entries: impl Iterator<Item = (SignatureEntry, String)>,
		) -> Option<Tree> {
			entries
				.map(|(entry, path)| Some((path.strip_prefix(root)?.to_string(), entry?)))
				.collect()
		}
		let roots: Vec<&str> = TREE
			.iter()
			.filter(|(_, is_dir)| *is_dir)
			.map(|(path, _)| *path)
			.chain(["moved", "nothing-here"])
			.collect();
		let mut seen = Coverage::default();
		let (mut equal, mut matched) = (0, 0);
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			let local = Side::from(whole_maps(&case).0);
			let local = local.of(&case.baseline);
			let by_rows = read_signatures(roots.iter().copied(), |root, add| {
				for row in case.baseline.subtree(root) {
					add(&row.rel_path, row_signature_entry(&row));
				}
			});
			let by_local = read_signatures(roots.iter().copied(), |root, add| {
				for (path, node) in local.under(root) {
					add(&path, local_signature_entry(&node));
				}
			});
			// `(on the rows, root, its signature, its tree)` for both sides.
			let mut read = Vec::new();
			for &root in &roots {
				let rows = tree_of(
					root,
					case.baseline
						.subtree(root)
						.map(|row| (row_signature_entry(&row), row.rel_path)),
				);
				let on_disk = tree_of(
					root,
					local
						.under(root)
						.map(|(path, node)| (local_signature_entry(&node), path.into_owned())),
				);
				for (on_rows, by, tree) in [(true, &by_rows, rows), (false, &by_local, on_disk)] {
					let signature = by[root];
					assert_eq!(
						signature.map(|(_, files)| files),
						tree.as_ref().map(|tree| {
							tree.values().any(|(kind, ..)| *kind == NodeKind::File)
						}),
						"seed {seed}: {root:?} on the rows: {on_rows}"
					);
					read.push((on_rows, root, signature.map(|(digest, _)| digest), tree));
				}
			}
			for (left_rows, left, left_signature, left_tree) in &read {
				for (right_rows, right, right_signature, right_tree) in &read {
					let (Some(left_tree), Some(right_tree)) = (left_tree, right_tree) else {
						continue;
					};
					let same = left_tree == right_tree;
					let at = format!(
						"seed {seed}: {left:?} (rows: {left_rows}) and {right:?} (rows: \
							 {right_rows})"
					);
					assert_eq!(left_signature == right_signature, same, "{at}");
					let decided = match (left_rows, right_rows) {
						(true, true) => same_rows(&case.baseline, left, right),
						(true, false) => {
							same_tree(&case.baseline, left, &local, right, &BTreeSet::new())
						}
						_ => continue,
					};
					assert_eq!(decided, same, "{at}: compared path for path");
					if same && !left_tree.is_empty() && (left, left_rows) != (right, right_rows) {
						equal += 1;
						matched += usize::from(left != right && *left_rows != *right_rows);
					}
				}
			}
		}
		// A corpus whose trees never matched would let the equalities above pass by proving
		// nothing about them.
		assert!(equal > 500, "only {equal} pair(s) of equal trees");
		assert!(
			matched > 50,
			"only {matched} tree(s) on the rows matched one on disk"
		);
	}

	/// The fold at the pass's scope, pruned, and the reconcile after it plan exactly what the
	/// unpruned set and a whole pass plan, over every generated case in every mode — the staged
	/// subtree moves included, with the remote edits, deletions and moves the random changes
	/// drew under them. And what the fold took out of the set is only what the rule allows:
	/// paths strictly under the destination of a pushed move that the remote side recorded
	/// nothing at.
	#[test]
	fn a_pruned_fold_and_reconcile_plan_what_the_unpruned_and_whole_ones_plan() {
		let mut seen = Coverage::default();
		let mut pruned_paths = 0;
		for seed in 0..CASES {
			let case = generate(seed, &mut seen);
			for mode in MODES {
				let (whole, by_whole, _) = fold(&case, mode, false);
				let (scoped, by_scope, replayed) = fold(&case, mode, true);
				let context = format!("seed {seed}, {mode:?}");
				assert_eq!(by_whole, by_scope, "{context}: other moves");
				assert_eq!(whole.local, scoped.local, "{context}: another local side");
				assert_eq!(
					whole.remote, scoped.remote,
					"{context}: another remote side"
				);
				assert_eq!(
					whole.baseline.iter().collect::<Vec<_>>(),
					scoped.baseline.iter().collect::<Vec<_>>(),
					"{context}: other rows"
				);
				seen.pushed_folds += by_scope
					.iter()
					.filter(|action| pushed_with_files(&scoped.baseline, action))
					.count();
				let pruned = &scoped.decided;
				// What staying put recorded is in the fold's set and in no replay of the moves.
				let unpruned: BTreeSet<String> = replayed.union(pruned).cloned().collect();
				for path in unpruned.difference(pruned) {
					let under_a_push = by_scope.iter().any(|action| {
						matches!(
							action,
							SyncAction::MoveRemote {
								kind: NodeKind::Dir,
								..
							}
						) && is_under(path, action.rel_path())
					});
					assert!(
						under_a_push && scoped.remote.untouched(path),
						"{context}: {path:?} left the set, and is not a path a pushed move \
							 carried with nothing recorded on the remote"
					);
					pruned_paths += 1;
				}
				let plan_at = |paths| {
					reconcile(
						mode,
						&scoped.baseline,
						&scoped.local.of(&scoped.baseline),
						&scoped.remote.of(&scoped.baseline),
						&scoped.holds,
						paths,
					)
				};
				let expected = plan_at(PassPaths::Whole);
				for (what, set) in [("unpruned", &unpruned), ("pruned", pruned)] {
					let plan = plan_at(PassPaths::Changed(set));
					assert_eq!(
						expected.actions, plan.actions,
						"{context}: the {what} set planned something else"
					);
					assert_eq!(
						expected.deferred_paths, plan.deferred_paths,
						"{context}: the {what} set counted other deferrals"
					);
				}
			}
		}
		assert!(
			seen.pushed_folds > 50,
			"only {} pushed directory move(s) carried a file",
			seen.pushed_folds
		);
		assert!(
			pruned_paths > 100,
			"only {pruned_paths} path(s) left the set, so the pruning was barely exercised"
		);
	}

	/// Whether `action` is a directory move pushed to the remote whose subtree, where the fold
	/// left `baseline`, holds a file.
	fn pushed_with_files(baseline: &Baseline, action: &SyncAction) -> bool {
		matches!(
			action,
			SyncAction::MoveRemote {
				kind: NodeKind::Dir,
				..
			}
		) && baseline
			.subtree(action.rel_path())
			.any(|row| row.kind == NodeKind::File)
	}

	/// The two names of every fold class [`TREE`] spells twice — what a case-only sibling
	/// collision is generated out of, which is the one collision that loses user data (the
	/// server dedups on the lowercased name hash, so the second spelling never lands).
	///
	/// The last two pairs fold together only under [`collision_key`]'s full-Unicode lowering.
	/// An ASCII-only fold keeps both spellings apart, and the second pair's fold is LONGER
	/// than the name it came from (`U+0130` lowers to two scalars), so a check written with
	/// `to_ascii_lowercase` — or one that assumed a fold preserves length — stops refusing a
	/// pass the server would have collapsed.
	const TWINS: [(&str, &str); 6] = [
		("a", "A"),
		("a/b.txt", "a/B.txt"),
		("top.txt", "Top.txt"),
		("empty", "Empty"),
		("\u{c4}.txt", "\u{e4}.txt"),
		("\u{130}.txt", "i\u{307}.txt"),
	];

	/// The `.filenignore` bodies a view case is filtered with: the pair root's, and the one in
	/// `d` — the directory [`TREE`] holds a rule-file path for, so a case can be hidden by a
	/// level that is not the root's.
	///
	/// Set 3 is the nested pair: `d/e` and `d/e/f` both match a rule of their own, and only
	/// the top-most is ever the root a hit is recorded under. Set 5 hides through the file at
	/// depth alone, and nests inside it.
	const RULE_SETS: [(&str, &str); 6] = [
		("", ""),
		("z/\n", ""),
		("*.txt\n", ""),
		("d/e\nd/e/f\n", ""),
		("a\nab\n", ""),
		("", "e/f\ne/f/g.txt\n"),
	];

	fn rules_of(set: usize) -> IgnoreRules {
		let (at_root, at_d) = RULE_SETS[set];
		let mut rules = IgnoreRules::default();
		for (dir, text) in [("", at_root), ("d", at_d)] {
			if text.is_empty() {
				continue;
			}
			let (source, errors) = IgnoreSource::parse(text, Origin::File { dir })
				.expect("the generated rule text compiles");
			assert!(errors.is_empty(), "{errors:?}");
			rules.insert_file(dir.to_owned(), source);
		}
		rules
	}

	/// What a change-scoped pass reaches [`RemoteView::filter_changed`] with, plus everything
	/// the WHOLE form needs to be run over the very same input.
	struct ViewCase {
		nodes: Side<RemoteNode>,
		held_paths: BTreeSet<String>,
		skipped: Vec<SkippedRemote>,
		decided: BTreeSet<String>,
		rules: IgnoreRules,
		baseline: Baseline,
	}

	impl ViewCase {
		/// A fresh view over this case's input: each form is run over its own copy, so neither
		/// can be handed what the other left.
		fn view(&self) -> RemoteView {
			RemoteView {
				nodes: copied(&self.nodes),
				has_collisions: false,
				held_paths: self.held_paths.clone(),
				skipped: self.skipped.clone(),
				ignored: BTreeMap::new(),
				ignored_default_untracked: 0,
			}
		}

		fn filter(&self) -> ViewFilter<'_> {
			ViewFilter {
				rules: &self.rules,
				baseline: &self.baseline,
			}
		}
	}

	/// What the view corpus provably reached, counted off the FINISHED cases rather than off
	/// the generator's intentions — a shape that was staged and then scrubbed away again is
	/// not a shape the properties tested.
	#[derive(Debug, Default)]
	struct ViewCoverage {
		cases: usize,
		/// Fold classes the view still holds more than one spelling of, with at least one of
		/// them decided: the collision the narrowed check has to find.
		fold_classes: usize,
		/// Of those, the ones whose partner is NOT decided — answered out of the baseline's
		/// rows instead of by scanning the view.
		partner_not_decided: usize,
		/// Of those, the ones where a spelling is HELD rather than in the view.
		held_folds: usize,
		/// Of those, the ones an ASCII-only fold would have kept apart.
		non_ascii_folds: usize,
		/// Decided nodes the rules hide.
		hidden_decided: usize,
		/// Of those, the directories with nodes under them — the subtree the narrowed form
		/// drops without asking the rules about it.
		hidden_subtrees: usize,
		/// Of those, the ones that match a rule of their OWN and are still recorded under an
		/// ancestor's root: nested ignored roots.
		nested_roots: usize,
		/// Of those, the ones decided by a `.filenignore` below the pair root.
		depth_rule_hits: usize,
		/// Carried nodes the rules hide, counted as [`scrub`] REMOVES them. Not a bound on
		/// anything the properties saw: by construction they never see one.
		scrubbed_under_hidden: usize,
		/// Carried nodes folding onto a held path or onto another carried node, likewise
		/// counted as the scrub removes them.
		scrubbed_folded: usize,
	}

	impl ViewCoverage {
		/// The bounds every one of the three properties is only worth running above. Each is a
		/// shape the narrowing can be wrong at; a generator that stopped producing one would
		/// otherwise let the property pass by proving nothing.
		fn assert_reached(&self) {
			assert_eq!(u64::try_from(self.cases), Ok(CASES), "{self:?}");
			assert!(self.fold_classes > 300, "{self:?}");
			assert!(self.partner_not_decided > 100, "{self:?}");
			assert!(self.held_folds > 70, "{self:?}");
			assert!(self.non_ascii_folds > 80, "{self:?}");
			assert!(self.hidden_decided > 350, "{self:?}");
			assert!(self.hidden_subtrees > 70, "{self:?}");
			assert!(self.nested_roots > 40, "{self:?}");
			assert!(self.depth_rule_hits > 30, "{self:?}");
			self.assert_scrub_stayed_active();
		}

		/// A different claim from the nine above, kept apart from them so it cannot be read as
		/// a tenth: these two count what [`scrub`] took OUT of each case, so they say the
		/// scrub is still doing its work — never that a property exercised either shape. The
		/// two shapes it removes are covered by
		/// [`a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops`] instead.
		fn assert_scrub_stayed_active(&self) {
			assert!(self.scrubbed_under_hidden > 350, "{self:?}");
			assert!(self.scrubbed_folded > 70, "{self:?}");
		}
	}

	/// Take the carried half of a view back to the state the narrowing's safety argument
	/// ASSUMES, which is the state the pass before this one left.
	///
	/// Both removals are things an earlier whole read already did. A carried node came off a
	/// baseline row, and a row only exists where the rules let the node through — the rules
	/// stand still across a scoped pass, so no carried node is hidden by them. And a pass that
	/// finds a fold REFUSES, which forces the next pass to read whole, so no two carried keys
	/// fold together either. Without this the properties would be comparing the narrowed form
	/// against inputs no pass can be handed, and the divergences they found would be the two
	/// this module pins deliberately in
	/// [`a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops`].
	fn scrub(case: &mut ViewCase, seen: &mut ViewCoverage) {
		let mut memo = HashMap::new();
		let held_keys: HashSet<String> = case
			.held_paths
			.iter()
			.map(|path| collision_key(path))
			.collect();
		let mut claimed: HashSet<String> = HashSet::new();
		// Sorted, so which spelling of a carried pair survives the scrub is not the hash
		// order of a map — the one thing the two forms are allowed to disagree about.
		let mut carried: Vec<(String, bool)> = Nodes::iter(&case.nodes.of(&case.baseline))
			.filter(|(path, _)| !case.decided.contains(path.as_ref()))
			.map(|(path, node)| (path.into_owned(), node.kind == NodeKind::Dir))
			.collect();
		carried.sort();
		for (path, is_dir) in carried {
			if case.rules.ignored_root(&path, is_dir, &mut memo).is_some() {
				seen.scrubbed_under_hidden += 1;
				case.nodes.remove(&case.baseline, &path);
				continue;
			}
			let key = collision_key(&path);
			if held_keys.contains(&key) || !claimed.insert(key) {
				seen.scrubbed_folded += 1;
				case.nodes.remove(&case.baseline, &path);
			}
		}
	}

	/// What the finished case actually holds, which is what the bounds above are read off.
	fn measure(case: &ViewCase, seen: &mut ViewCoverage) {
		seen.cases += 1;
		let mut classes: HashMap<String, Vec<String>> = HashMap::new();
		for path in case.nodes.of(&case.baseline).paths() {
			classes
				.entry(collision_key(&path))
				.or_default()
				.push(path.into_owned());
		}
		// A held path took its key when the view held it, so it is part of its fold class.
		for held in &case.held_paths {
			classes
				.entry(collision_key(held))
				.or_default()
				.push(held.clone());
		}
		for spellings in classes.into_values() {
			let decided = spellings
				.iter()
				.filter(|path| case.decided.contains(*path))
				.count();
			if spellings.len() < 2 || decided == 0 {
				continue;
			}
			seen.fold_classes += 1;
			if decided < spellings.len() {
				seen.partner_not_decided += 1;
			}
			if spellings.iter().any(|path| case.held_paths.contains(path)) {
				seen.held_folds += 1;
			}
			if spellings
				.iter()
				.any(|path| !path.eq_ignore_ascii_case(&spellings[0]))
			{
				seen.non_ascii_folds += 1;
			}
		}
		let mut memo = HashMap::new();
		for (path, node) in Nodes::iter(&case.nodes.of(&case.baseline)) {
			if !case.decided.contains(path.as_ref()) {
				continue;
			}
			let is_dir = node.kind == NodeKind::Dir;
			let Some((root, decision)) = case.rules.ignored_root(&path, is_dir, &mut memo) else {
				continue;
			};
			seen.hidden_decided += 1;
			if is_dir && !case.nodes.subtree_paths(&case.baseline, &path).is_empty() {
				seen.hidden_subtrees += 1;
			}
			if root != path && case.rules.decide(&path, is_dir).is_some() {
				seen.nested_roots += 1;
			}
			if matches!(decision.level, IgnoreLevel::File { ref dir } if !dir.is_empty()) {
				seen.depth_rule_hits += 1;
			}
		}
	}

	/// The remote half of one generated case, as a view the two filter forms can be run over.
	///
	/// The fold class is STAGED rather than left to the draw: two spellings of one name reach
	/// the collision check together far too rarely for a corpus this size to pin anything
	/// about them, which is the same reason [`generate`] stages its directory rename.
	fn view_case(seed: u64, case: &Case, seen: &mut ViewCoverage) -> ViewCase {
		// Its own stream, so the case's draws do not shift when this one changes.
		let mut rng = StdRng::seed_from_u64(seed ^ 0x7fff_ffff_0000_0001);
		let mut out = ViewCase {
			nodes: copied(&case.remote),
			held_paths: BTreeSet::new(),
			skipped: Vec::new(),
			decided: case.decided.clone(),
			rules: rules_of(rng.random_range(0..RULE_SETS.len())),
			baseline: case.baseline.clone(),
		};
		let place = |nodes: &mut Side<RemoteNode>, path: &str| {
			let index = index_of(path);
			nodes.insert(path.to_owned(), remote_node_at(index, kind_at(index), 0));
		};
		let (lower, upper) = TWINS[rng.random_range(0..TWINS.len())];
		match rng.random_range(0..4) {
			// Both spellings decided: what one pass's worth of remote announcements can
			// introduce on its own.
			0 => {
				for path in [lower, upper] {
					place(&mut out.nodes, path);
					out.decided.insert(path.to_owned());
				}
			}
			// One decided, its partner CARRIED — the case the narrowed check answers out of
			// the baseline's rows instead of by scanning the view, so the partner needs one.
			// Its node is built rather than carried off that row: `hide` and the collision
			// check read a node's path and its kind and nothing else, and both match the row.
			1 => {
				place(&mut out.nodes, lower);
				place(&mut out.nodes, upper);
				out.decided.insert(upper.to_owned());
				out.decided.remove(lower);
				out.baseline
					.upsert_for_test(&row(index_of(lower), Shape::Synced).expect("a synced row"));
			}
			// One spelling HELD — the cache mid-transition — and the other decided. A held
			// path is always decided: what withheld it was an announcement about it.
			2 => {
				place(&mut out.nodes, upper);
				out.nodes.remove(&out.baseline, lower);
				for path in [lower, upper] {
					out.decided.insert(path.to_owned());
				}
				out.held_paths.insert(lower.to_owned());
			}
			_ => {}
		}
		// A decided path the rules HIDE, and — where it is a directory — its whole subtree
		// decided with it. That is the one shape a scoped pass ever pays a subtree walk for: a
		// remote create or move INTO an already-ignored directory, whose re-key records every
		// end. Left to the draw it happens too rarely to test the narrowed hide's own removal.
		if rng.random_range(0..2) == 0 {
			let mut memo = HashMap::new();
			let hidden: Vec<&str> = TREE
				.iter()
				.filter(|(path, is_dir)| out.rules.ignored_root(path, *is_dir, &mut memo).is_some())
				.map(|(path, _)| *path)
				.collect();
			if let Some(root) = hidden
				.get(rng.random_range(0..hidden.len().max(1)))
				.copied()
			{
				let under = format!("{root}/");
				for (path, _) in TREE
					.iter()
					.filter(|(path, _)| *path == root || path.starts_with(&under))
				{
					place(&mut out.nodes, path);
					out.decided.insert((*path).to_owned());
				}
			}
		}
		out.skipped = (0..rng.random_range(0..3u32))
			.map(|n| {
				let (path, is_dir) = TREE[rng.random_range(0..TREE.len())];
				SkippedRemote {
					remote_uuid: Uuid::from_u128(7000 + u128::from(n)),
					stable_uuid: None,
					rel_path: path.to_owned(),
					path_is_dir: is_dir,
					reason: match n % 3 {
						0 => UnsyncableReason::RemoteUndecodable,
						1 => UnsyncableReason::RemoteInvalidName {
							name: path.to_owned(),
						},
						_ => UnsyncableReason::RemoteBrokenParent,
					},
				}
			})
			.collect();
		scrub(&mut out, seen);
		measure(&out, seen);
		out
	}

	/// Every view case of the corpus, with the coverage the bounds are checked against.
	fn view_cases() -> (Vec<(u64, ViewCase)>, ViewCoverage) {
		let mut generated = Coverage::default();
		let mut seen = ViewCoverage::default();
		let cases = (0..CASES)
			.map(|seed| {
				(
					seed,
					view_case(seed, &generate(seed, &mut generated), &mut seen),
				)
			})
			.collect();
		(cases, seen)
	}

	fn sorted_paths(baseline: &Baseline, view: &RemoteView) -> Vec<String> {
		let mut paths: Vec<String> = view
			.nodes
			.of(baseline)
			.paths()
			.map(Cow::into_owned)
			.collect();
		paths.sort();
		paths
	}

	/// The names the surviving nodes CLAIM, folded the way the server dedups them. This is
	/// what the two collision checks have to agree on even where they dropped different
	/// spellings of one name.
	fn claimed_keys(baseline: &Baseline, view: &RemoteView) -> BTreeSet<String> {
		view.nodes
			.of(baseline)
			.paths()
			.map(|path| collision_key(&path))
			.collect()
	}

	/// Everything [`RemoteView::hide`] touches, at both scopes. Exact down to the path: the
	/// rules answer per node, so nothing here is free to depend on an iteration order.
	fn assert_same_hidden(seed: u64, baseline: &Baseline, whole: &RemoteView, scoped: &RemoteView) {
		assert_eq!(
			sorted_paths(baseline, whole),
			sorted_paths(baseline, scoped),
			"seed {seed}: the narrowed hide left another set of nodes"
		);
		assert_eq!(
			whole.ignored, scoped.ignored,
			"seed {seed}: the narrowed hide recorded other ignored roots"
		);
		assert_eq!(
			whole.ignored_default_untracked, scoped.ignored_default_untracked,
			"seed {seed}: the narrowed hide counted other untracked default hits"
		);
		assert_eq!(
			whole.skipped, scoped.skipped,
			"seed {seed}: the narrowed hide kept other unplaceable records"
		);
	}

	/// Everything the two collision checks must agree on.
	///
	/// The REFUSAL is the contract at every case: a fold either leaves no 1:1 local mapping or
	/// it does not, and [`has_collisions`](RemoteView::has_collisions) is the whole of what the
	/// engine reads of this step. Where none fired, the two views must also be identical
	/// outright — there is no licence to differ at all.
	///
	/// Where one DID fire, the pass is refused before it reconciles anything and the view is
	/// dropped unread, so what a refused view still holds is not a contract — and the two forms
	/// do differ there. The narrowed one can empty a whole fold class where the whole form keeps
	/// a spelling: its losers leave only once the loop is over, so two decided twins each still
	/// find the other's row and node and both are dropped. What is still owed in that case is
	/// the direction a LOST refusal would show up in — the narrowed form never keeps a name the
	/// whole one dropped.
	fn assert_same_claimed(
		seed: u64,
		baseline: &Baseline,
		what: &str,
		whole: &RemoteView,
		scoped: &RemoteView,
	) {
		assert_eq!(
			whole.has_collisions, scoped.has_collisions,
			"seed {seed}, {what}: the two forms disagree about refusing the pass"
		);
		if whole.has_collisions {
			assert!(
				claimed_keys(baseline, scoped).is_subset(&claimed_keys(baseline, whole)),
				"seed {seed}, {what}: the narrowed form kept a name the whole one dropped"
			);
			return;
		}
		assert_eq!(
			sorted_paths(baseline, whole),
			sorted_paths(baseline, scoped),
			"seed {seed}, {what}: nothing was refused, so the two views must be identical"
		);
	}

	/// The narrowed ignore step hides exactly what the whole one hides.
	///
	/// The expectation comes from [`RemoteView::hide`] at [`PassPaths::Whole`] — the form a
	/// whole-read pass still runs in production, not a copy of it written here.
	#[test]
	fn a_scoped_hide_removes_what_a_whole_hide_removes() {
		let (cases, seen) = view_cases();
		for (seed, case) in &cases {
			let mut whole = case.view();
			whole.hide(case.filter(), PassPaths::Whole);
			let mut scoped = case.view();
			scoped.hide(case.filter(), PassPaths::Changed(&case.decided));
			assert_same_hidden(*seed, &case.baseline, &whole, &scoped);
		}
		seen.assert_reached();
	}

	/// The narrowed collision check refuses exactly what the whole one refuses, and leaves the
	/// same names claimed.
	#[test]
	fn a_scoped_collision_check_refuses_what_a_whole_one_refuses() {
		let (cases, seen) = view_cases();
		for (seed, case) in &cases {
			let mut whole = case.view();
			whole.resolve_collisions(&case.baseline);
			let mut scoped = case.view();
			// The decided keys the view holds, which is what `hide` hands over when it runs
			// first.
			let kept: Vec<&String> = case
				.decided
				.iter()
				.filter(|path| scoped.nodes.of(&case.baseline).holds(path))
				.collect();
			scoped.resolve_collisions_changed(&case.baseline, kept);
			assert_same_claimed(*seed, &case.baseline, "collisions", &whole, &scoped);
		}
		seen.assert_reached();
	}

	/// The two steps composed, in the order a pass runs them: the narrowed filter leaves the
	/// view the whole filter leaves.
	///
	/// Not implied by the two properties above. They each run over the view as it arrives,
	/// where this runs the collision check over whatever the ignore step left — and a
	/// narrowing that hid the wrong node would hand the fold a different set of names to
	/// resolve than the whole form ever sees.
	#[test]
	fn a_scoped_filter_leaves_the_view_a_whole_filter_leaves() {
		let (cases, seen) = view_cases();
		for (seed, case) in &cases {
			let mut whole = case.view();
			whole.filter(Some(case.filter()));
			let mut scoped = case.view();
			scoped.filter_changed(case.filter(), &case.decided);
			assert_same_claimed(*seed, &case.baseline, "filter", &whole, &scoped);
			assert_eq!(
				whole.ignored, scoped.ignored,
				"seed {seed}: the narrowed filter recorded other ignored roots"
			);
			assert_eq!(
				whole.ignored_default_untracked, scoped.ignored_default_untracked,
				"seed {seed}: the narrowed filter counted other untracked default hits"
			);
			assert_eq!(
				whole.skipped, scoped.skipped,
				"seed {seed}: the narrowed filter kept other unplaceable records"
			);
		}
		seen.assert_reached();
	}

	/// The two carried shapes the narrowed filter KEEPS where the whole one drops them — the
	/// exact divergence [`scrub`] takes out of the corpus above, on the record rather than
	/// left as a gap nobody wrote down.
	///
	/// Both are the safe direction. A carried node's LOCAL half comes off the same row, so the
	/// pair reads as converged there and a reconcile driven by the decided set plans nothing
	/// at it; dropping the remote half alone is what would read as a local deletion. The
	/// second shape only DELAYS a refusal: the held path is decided, so the pass after the one
	/// that unholds it folds it against the carried row and refuses then.
	#[test]
	fn a_scoped_filter_keeps_the_two_carried_shapes_a_whole_one_drops() {
		let of = |path: &str| {
			let index = index_of(path);
			(path.to_owned(), remote_node_at(index, kind_at(index), 0))
		};
		let case = ViewCase {
			nodes: ["z", "z/1.txt", "top.txt"].into_iter().map(of).collect(),
			// `Top.txt` listed twice by a cache mid-transition, so the view holds neither copy.
			held_paths: BTreeSet::from(["Top.txt".to_owned()]),
			skipped: Vec::new(),
			// Nothing moved off its row: every node here is one the pass CARRIED.
			decided: BTreeSet::new(),
			rules: rules_of(1),
			baseline: Baseline::from_rows([
				base_dir("z", uuid_at(index_of("z"))),
				base_file("z/1.txt", uuid_at(index_of("z/1.txt")), hash_at(0, 0)),
				base_file("top.txt", uuid_at(index_of("top.txt")), hash_at(1, 0)),
			]),
		};

		let mut whole = case.view();
		whole.filter(Some(case.filter()));
		assert!(
			sorted_paths(&case.baseline, &whole).is_empty(),
			"the whole form drops the rows left at a hidden path and refuses the folded one"
		);
		assert!(whole.has_collisions);
		assert_eq!(whole.ignored.keys().collect::<Vec<_>>(), vec!["z"]);

		let mut scoped = case.view();
		scoped.filter_changed(case.filter(), &case.decided);
		assert_eq!(
			sorted_paths(&case.baseline, &scoped),
			["top.txt", "z", "z/1.txt"],
			"a carried row at a hidden path, and one folding onto a held path, both stay"
		);
		assert!(
			!scoped.has_collisions,
			"the pass is not refused over a pair it is not reconciling"
		);
		assert!(
			scoped.ignored.is_empty(),
			"a root an earlier pass found is re-supplied by the carried facts, not re-derived"
		);
	}

	/// The same two shapes once the pass DECIDED their keys — a directory renamed away records
	/// every row under its old name — are asked about like any other decided key, and the
	/// narrowed filter drops them exactly as the whole one does: the hidden rows leave with
	/// their root recorded, and the row folding onto a held path refuses the pass. The nodes
	/// are carried off the rows here, so what the view holds at each key is the rows' answer.
	#[test]
	fn a_scoped_filter_drops_the_two_carried_shapes_once_they_are_decided() {
		let case = ViewCase {
			nodes: Side::carried(),
			held_paths: BTreeSet::from(["Top.txt".to_owned()]),
			skipped: Vec::new(),
			decided: ["top.txt", "z", "z/1.txt"]
				.into_iter()
				.map(str::to_owned)
				.collect(),
			rules: rules_of(1),
			baseline: Baseline::from_rows([
				base_dir("z", uuid_at(index_of("z"))),
				base_file("z/1.txt", uuid_at(index_of("z/1.txt")), hash_at(0, 0)),
				base_file("top.txt", uuid_at(index_of("top.txt")), hash_at(1, 0)),
			]),
		};

		let mut whole = case.view();
		whole.filter(Some(case.filter()));
		let mut scoped = case.view();
		scoped.filter_changed(case.filter(), &case.decided);
		for (form, view) in [("whole", &whole), ("scoped", &scoped)] {
			assert!(
				sorted_paths(&case.baseline, view).is_empty(),
				"{form}: the rows at a hidden path leave, and the folded one is refused"
			);
			assert!(view.has_collisions, "{form}");
			assert_eq!(view.ignored.keys().collect::<Vec<_>>(), vec!["z"], "{form}");
		}
	}
}
