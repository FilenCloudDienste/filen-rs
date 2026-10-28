use super::super::side::NodesAt;
use std::{
	collections::BTreeMap,
	fs,
	path::{Path, PathBuf},
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use uuid::Uuid;

use super::*;
use crate::{
	alloc_meter,
	cache::{CacheEvent, CacheEventType, FileEvent, RemoteItem},
	io::FilenMetaExt,
	sync_engine::{
		SyncMode,
		baseline::BaselineState,
		changes::{
			DIRTY_CAP, PairChanges, RemoteChange, RemoteDeltaEntry, SUBTREE_CAP,
			tests::{cache_event, cacheable_file},
		},
		engine::assembly_bounds,
		facts::carry_over,
		ignore::{FILENIGNORE, IgnoreRules},
		observe::observe_local,
		plan::{PassHolds, RemoteView, SyncAction, place_remote_items},
		remote::{RemoteObserved, delta_uuids, observe_remote},
		scan::{LocalScan, PairRoot, RuleFiles, ScanGaps, ScanRoot, scan_local},
	},
};

/// The pair's remote root: every derived path is relative to it.
const REMOTE_ROOT: Uuid = Uuid::from_u128(1);

/// A converged pair: a real tree on disk, the remote items that mirror it, and the baseline the
/// pass after convergence would have written — rows shaped exactly as `apply` writes them.
struct Pair {
	root: PathBuf,
	/// One uuid per synced path, so the remote view and the rows agree on identity.
	uuids: BTreeMap<String, Uuid>,
	rows: Vec<BaselineEntry>,
	items: Vec<RemoteItem>,
}

fn temp_root() -> PathBuf {
	let dir = std::env::temp_dir().join(format!("filen_derive_test_{}", Uuid::new_v4()));
	fs::create_dir_all(&dir).unwrap();
	dir
}

fn write(root: &Path, rel_path: &str, body: &[u8]) {
	let path = root.join(rel_path);
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).unwrap();
	}
	fs::write(path, body).unwrap();
}

fn scan(root: &Path, baseline: &Baseline) -> LocalScan {
	scan_local(
		ScanRoot {
			path: root,
			pair_root: PairRoot::Directory,
		},
		baseline,
		IgnoreRules::default(),
		RuleFiles::Read,
	)
	.0
}

/// The remote item one scanned node stands for, keyed by the uuids `Pair` handed out: a file
/// carries the content the scan hashed, a directory carries nothing but its name.
fn item_of(node: &LocalNode, uuids: &BTreeMap<String, Uuid>) -> RemoteItem {
	let (dir, name) = node
		.rel_path
		.rsplit_once('/')
		.map_or(("", node.rel_path.as_str()), |(dir, name)| (dir, name));
	let uuid = uuids[&node.rel_path];
	RemoteItem {
		uuid,
		parent: if dir.is_empty() {
			REMOTE_ROOT
		} else {
			uuids[dir]
		},
		name: name.to_owned(),
		stable_uuid: (node.kind == NodeKind::File).then(|| StableUuid::new_for_test(uuid)),
		hash: node.content_hash,
		size: if node.kind == NodeKind::File {
			node.size
		} else {
			0
		},
		modified_millis: if node.kind == NodeKind::File {
			node.mtime_millis
		} else {
			99
		},
	}
}

fn view(items: &[RemoteItem]) -> RemoteView {
	let (dirs, files): (Vec<RemoteItem>, Vec<RemoteItem>) = items
		.iter()
		.cloned()
		.partition(|item| item.stable_uuid.is_none());
	let mut view = place_remote_items(REMOTE_ROOT, PairRoot::Directory, &dirs, &files, &[]);
	// A whole-read view: its backing never consults a baseline, so an empty one is exact.
	view.filter(None);
	view
}

/// The row a converged pass writes for one path, shaped as `apply`'s own writers shape it: a
/// directory records no content and no remote stamp, a file records both sides whole.
fn synced_row(local: &LocalNode, remote: &RemoteNode) -> BaselineEntry {
	let is_file = local.kind == NodeKind::File;
	BaselineEntry {
		rel_path: local.rel_path.clone(),
		kind: local.kind,
		remote_uuid: Some(remote.remote_uuid),
		content_hash: is_file.then_some(local.content_hash).flatten(),
		size: is_file.then_some(local.size),
		local_mtime: Some(local.mtime_millis),
		remote_modified: is_file.then_some(remote.modified_millis),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: remote.stable_uuid,
		agreed_hash: is_file.then_some(local.content_hash).flatten(),
	}
}

impl Pair {
	/// A tree with a bit of everything a scan keys: nesting, a non-ASCII name, an empty
	/// directory, and files whose content differs.
	fn converged() -> Self {
		let root = temp_root();
		write(&root, "top.txt", b"top");
		write(&root, "docs/notes.txt", b"notes");
		write(&root, "docs/deep/inner.bin", b"inner-bytes");
		write(&root, "docs/Ärger.txt", b"umlaut");
		fs::create_dir_all(root.join("empty")).unwrap();

		let scanned = scan(&root, &Baseline::default());
		assert!(scanned.complete, "{:?}", scanned.errors);
		let mut paths: Vec<String> = scanned
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		let uuids: BTreeMap<String, Uuid> = paths
			.iter()
			.enumerate()
			.map(|(at, path)| (path.clone(), Uuid::from_u128(100 + at as u128)))
			.collect();
		let items: Vec<RemoteItem> = scanned
			.nodes
			.whole()
			.iter()
			.map(|(_, node)| item_of(&node, &uuids))
			.collect();
		let remote = view(&items);
		let remote_nodes = remote.nodes.whole();
		let rows = scanned
			.nodes
			.whole()
			.iter()
			.map(|(_, node)| {
				let at = remote_nodes.at(&node.rel_path).expect("the view holds it");
				synced_row(&node, &at)
			})
			.collect();
		Self {
			root,
			uuids,
			rows,
			items,
		}
	}

	fn baseline(&self) -> Baseline {
		Baseline::from_rows(self.rows.clone())
	}

	/// What a whole-tree pass reads: the scan of the tree as it is, and the view of the items as
	/// they are.
	fn whole_tree(&self, baseline: &Baseline) -> (Side<LocalNode>, RemoteView) {
		let scanned = scan(&self.root, baseline);
		assert!(scanned.complete, "{:?}", scanned.errors);
		(scanned.nodes, view(&self.items))
	}

	/// What a change-scoped pass assembles: the baseline carried forward, the announced remote
	/// changes applied to it, and the dirty paths re-observed on disk.
	fn derived(
		&self,
		baseline: &Baseline,
		dirty: BTreeSet<String>,
		delta: &[RemoteDeltaEntry],
	) -> Derived {
		assembled(&self.root, baseline, dirty, delta)
	}
}

/// [`Pair::derived`] over any tree on disk at `root`.
fn assembled(
	root: &Path,
	baseline: &Baseline,
	dirty: BTreeSet<String>,
	delta: &[RemoteDeltaEntry],
) -> Derived {
	let mut derived = from_baseline(baseline, dirty);
	let mut ancestry = |uuid: Uuid| -> rusqlite::Result<Vec<RemoteItem>> {
		panic!("no ancestry read was expected, but one was made for {uuid}")
	};
	let placed = baseline.synced_paths(&delta_uuids(delta), &[]);
	let mut observed = match observe_remote(
		REMOTE_ROOT,
		PairRoot::Directory,
		baseline,
		std::mem::take(&mut derived.remote),
		delta,
		placed,
		&mut ancestry,
	) {
		RemoteObserved::Applied(observation) => *observation,
		RemoteObserved::Full(reason) => panic!("expected a derived view: {reason}"),
	};
	derived.remote = observed.nodes;
	derived.held.extend(observed.held_paths);
	derived.dirty.extend(observed.touched);
	// The exact keys the delta moved, as `prepare_scoped` records them: what the pass
	// DECIDES, where `dirty` says where to look.
	derived.decided.append(&mut observed.changed);
	let (mut local, _) = observe_local(
		root,
		PairRoot::Directory,
		baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		&derived.dirty,
	)
	.expect("a dirty set the caps narrow a pass to");
	assert!(local.complete, "{:?}", local.errors);
	merge_local(&mut derived, baseline, &mut local);
	derived
}

impl Drop for Pair {
	fn drop(&mut self) {
		fs::remove_dir_all(&self.root).ok();
	}
}

/// The dirty set a pass starts from, in the shape [`from_baseline`] takes it.
fn dirty_paths(dirty: &[&str]) -> BTreeSet<String> {
	dirty.iter().map(|path| (*path).to_string()).collect()
}

/// The delta a pass takes from its changelist for those events — minted through
/// `PairChanges`, which is the one way a `Gone` can exist at all.
fn delta_of(events: &[CacheEvent<'static>]) -> Vec<RemoteDeltaEntry> {
	let changes = PairChanges::new();
	changes.note_tree_size(1_000);
	changes.note_remote_batch(&mut events.iter());
	changes.take().take_remote()
}

/// What the reconcile makes of the maps a change-scoped pass derived — over the paths that
/// pass DECIDED, so a plan here is one a narrowed read really produces.
fn scoped_plan(baseline: &Baseline, derived: &Derived) -> Vec<SyncAction> {
	let holds = PassHolds {
		held_remote: derived.held.clone(),
		..PassHolds::default()
	};
	plan::reconcile(
		SyncMode::TwoWay,
		baseline,
		&derived.local.of(baseline),
		&derived.remote.of(baseline),
		&holds,
		plan::PassPaths::Changed(&derived.decided),
	)
	.actions
}

fn sorted(keys: impl IntoIterator<Item = String>) -> Vec<String> {
	let mut keys: Vec<String> = keys.into_iter().collect();
	keys.sort();
	keys
}

/// The two maps, node for node, against what a whole-tree read produces — with the differences
/// derivation cannot avoid named rather than skipped: a directory row records no remote stamp, so a
/// derived directory carries `0` where the view carries the server's created stamp, and a file row
/// records its mtime to the millisecond only (see the module docs).
fn assert_same_maps(
	baseline: &Baseline,
	derived: &Derived,
	local: &Side<LocalNode>,
	remote: &Side<RemoteNode>,
) {
	// Bound once: each reader hands back a `Cow` borrowed from itself, which a carried side
	// builds on the spot, so a reader made fresh per statement dies under the value it lent.
	let derived_local = derived.local.of(baseline);
	let derived_remote = derived.remote.of(baseline);
	let local_nodes = local.whole();
	let remote_nodes = remote.whole();
	assert_eq!(
		sorted(derived_local.paths().map(|path| path.into_owned())),
		sorted(local_nodes.paths().map(|path| path.into_owned())),
		"the derived local map holds exactly the paths a whole-tree scan does"
	);
	for (path, node) in local_nodes.iter() {
		let at = derived_local.at(&path).expect("the derived map holds it");
		let (derived_node, node) = (&*at, &*node);
		if node.kind == NodeKind::Dir {
			assert_eq!(
				derived_node,
				&LocalNode {
					mtime_millis: derived_node.mtime_millis,
					..node.clone()
				},
				"local directory at {path:?} differs by more than its mtime"
			);
		} else {
			assert_eq!(
				&LocalNode {
					mtime_nanos: 0,
					..derived_node.clone()
				},
				&LocalNode {
					mtime_nanos: 0,
					..node.clone()
				},
				"local file at {path:?} differs by more than what its row cannot record"
			);
		}
	}
	let visible: Vec<String> = remote_nodes
		.paths()
		.filter(|path| !derived.held.contains(path.as_ref()))
		.map(|path| path.into_owned())
		.collect();
	assert_eq!(
		sorted(derived_remote.paths().map(|path| path.into_owned())),
		sorted(visible),
		"the derived remote map holds every path the view does, bar the withheld ones"
	);
	for (path, node) in remote_nodes.iter() {
		let Some(at) = derived_remote.at(&path) else {
			continue;
		};
		let (derived_node, node) = (&*at, &*node);
		if node.kind == NodeKind::Dir {
			assert_eq!(
				derived_node,
				&RemoteNode {
					modified_millis: derived_node.modified_millis,
					..node.clone()
				},
				"remote directory at {path:?} differs by more than its created stamp"
			);
		} else {
			assert_eq!(derived_node, node, "remote node at {path:?}");
		}
	}
}

/// The plan is what the maps are for: the same actions, in the same order, from the derived
/// maps as from the whole-tree ones. This is what pins the directory stamps as the non-evidence
/// the module docs claim they are.
///
/// Both sides are reconciled with the SAME holds, the derived pass's: the question here is what
/// the two map pairs plan where they both describe a path, and a withheld path is one the
/// derived maps deliberately describe on neither side.
fn assert_same_plan(
	baseline: &Baseline,
	derived: &Derived,
	local: &Side<LocalNode>,
	remote: &Side<RemoteNode>,
) -> Vec<SyncAction> {
	let holds = PassHolds {
		held_remote: derived.held.clone(),
		..PassHolds::default()
	};
	let whole = plan::reconcile(
		SyncMode::TwoWay,
		baseline,
		&local.whole(),
		&remote.whole(),
		&holds,
		plan::PassPaths::Whole,
	);
	let scoped = plan::reconcile(
		SyncMode::TwoWay,
		baseline,
		&derived.local.of(baseline),
		&derived.remote.of(baseline),
		&holds,
		plan::PassPaths::Whole,
	);
	assert_eq!(
		scoped.actions, whole.actions,
		"the change-scoped maps planned something else"
	);
	// And at the scope the pass really hands the reconcile. The maps above are compared over
	// every key they hold; this asks the narrower question the pass asks, so a producer that
	// moved a key off the node its row carries and did NOT record it is a key nothing decides
	// — which shows up here as a missing action and nowhere else.
	assert_eq!(
		scoped_plan(baseline, derived),
		whole.actions,
		"a key the producers moved is missing from the decided set"
	);
	whole.actions
}

/// An unchanged tree: the maps a change-scoped pass assembles are the maps a whole-tree read
/// produces, path for path and value for value.
#[test]
fn an_unchanged_tree_derives_the_maps_a_whole_tree_read_produces() {
	let pair = Pair::converged();
	let baseline = pair.baseline();

	let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
	let (local, remote) = pair.whole_tree(&baseline);

	assert!(
		derived.held.is_empty(),
		"every row records both sides: {:?}",
		derived.held
	);
	assert_same_maps(&baseline, &derived, &local, &remote.nodes);
	let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
	assert!(
		actions.is_empty(),
		"a converged pair plans nothing: {actions:?}"
	);
}

/// Rows that record one side only — the states the engine writes on purpose — are in the dirty
/// set and in the held set, and in neither map. The local side then comes back from the
/// observation, so the local map still equals a whole-tree scan's.
#[test]
fn rows_that_record_one_side_are_dirty_held_and_derived_from_neither() {
	let mut pair = Pair::converged();
	write(&pair.root, "conflict.txt", b"conflicted");
	write(&pair.root, "overwritten.txt", b"overwritten");
	write(&pair.root, "adopted.txt", b"adopted");
	write(&pair.root, "kept-local.txt", b"kept local");
	write(&pair.root, "kept-remote.txt", b"kept remote");

	// Every one of them is synced on both sides as far as the world is concerned: they are on
	// disk, they are in the remote view, and only the ROW is half-written.
	let scanned = scan(&pair.root, &Baseline::default());
	let anomalies = [
		"conflict.txt",
		"overwritten.txt",
		"adopted.txt",
		"kept-local.txt",
		"kept-remote.txt",
	];
	let scanned_nodes = scanned.nodes.whole();
	for (at, path) in anomalies.iter().enumerate() {
		let uuid = Uuid::from_u128(900 + at as u128);
		pair.uuids.insert((*path).to_string(), uuid);
		let node = scanned_nodes.at(path).expect("the scan holds it");
		pair.items.push(item_of(&node, &pair.uuids));
	}
	let remote_view = view(&pair.items);
	let view_nodes = remote_view.nodes.whole();
	for path in anomalies {
		let node = scanned_nodes.at(path).expect("the scan holds it");
		let at = view_nodes.at(path).expect("the view holds it");
		let row = synced_row(&node, &at);
		pair.rows.push(match path {
			"conflict.txt" => BaselineEntry {
				state: BaselineState::Conflicted,
				local_kind: Some(NodeKind::File),
				remote_kind: Some(NodeKind::File),
				remote_hash: Some(Blake3Hash::from([7; 32])),
				remote_size: Some(11),
				..row
			},
			"overwritten.txt" => BaselineEntry {
				state: BaselineState::Overwritten,
				local_kind: Some(NodeKind::File),
				remote_kind: Some(NodeKind::File),
				remote_hash: Some(Blake3Hash::from([8; 32])),
				remote_size: Some(12),
				..row
			},
			"adopted.txt" => BaselineEntry {
				state: BaselineState::Adopted,
				local_mtime: None,
				..row
			},
			// A `KeepLocal` resolution: the remote anchor stands, the local half is cleared
			// until the re-push.
			"kept-local.txt" => BaselineEntry {
				content_hash: None,
				size: None,
				local_mtime: None,
				agreed_hash: None,
				..row
			},
			// A `KeepRemote` resolution, mirrored: the local anchor stands and the remote half
			// is cleared, so the remote reads as the change and is pulled.
			_ => BaselineEntry {
				remote_uuid: None,
				remote_stable_uuid: None,
				remote_modified: None,
				..row
			},
		});
	}
	let baseline = pair.baseline();

	let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
	let (local, remote) = pair.whole_tree(&baseline);

	assert_eq!(
		sorted(derived.held.iter().cloned()),
		sorted(anomalies.iter().map(|path| (*path).to_string())),
		"exactly the half-written rows are withheld"
	);
	assert!(
		anomalies.iter().all(|path| derived.dirty.contains(*path)),
		"a withheld row is always re-observed: {:?}",
		derived.dirty
	);
	assert!(
		anomalies
			.iter()
			.all(|path| !derived.remote.of(&baseline).holds(path)),
		"nothing is derived from a half-written row"
	);
	// The local side is not derived from them either — it is observed, which is why the map is
	// still whole.
	assert_same_maps(&baseline, &derived, &local, &remote.nodes);
	let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
	assert!(
		!actions.iter().any(|action| matches!(
			action,
			SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
		)),
		"a withheld path must never plan a deletion: {actions:?}"
	);
}

/// The mirror of the equality test: a known change set on both sides, and the derived maps are
/// the maps of the CHANGED tree — the local ones from the observations, the remote ones from
/// the announced changes.
#[test]
fn a_changed_tree_derives_the_maps_of_the_change() {
	let mut pair = Pair::converged();
	let baseline = pair.baseline();

	// Local: an edit, a deletion, a new file, a new directory with a file in it, and a whole
	// subtree removed.
	write(&pair.root, "top.txt", b"top, edited");
	fs::remove_file(pair.root.join("docs/notes.txt")).unwrap();
	write(&pair.root, "fresh.txt", b"fresh");
	write(&pair.root, "added/one.txt", b"one");
	fs::remove_dir_all(pair.root.join("docs/deep")).unwrap();

	// Remote: one file created, one removed. The delta is minted from real cache events, which
	// is the only way a `Gone` can exist at all.
	let born = Uuid::from_u128(500);
	let doomed = pair.uuids["docs/Ärger.txt"];
	pair.items.push(RemoteItem {
		uuid: born,
		parent: REMOTE_ROOT,
		name: "remote-new.txt".to_owned(),
		stable_uuid: Some(StableUuid::new_for_test(born)),
		hash: None,
		size: 7,
		modified_millis: 1_234,
	});
	pair.items.retain(|item| item.uuid != doomed);
	let delta = [
		cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(
				born,
				REMOTE_ROOT,
				"remote-new.txt",
			))),
		),
		cache_event(Some(2), CacheEventType::File(FileEvent::Removed(doomed))),
	];

	let derived = pair.derived(
		&baseline,
		dirty_paths(&[
			"top.txt",
			"docs/notes.txt",
			"fresh.txt",
			"added",
			"docs/deep",
		]),
		&delta_of(&delta),
	);
	let (local, remote) = pair.whole_tree(&baseline);

	assert!(derived.held.is_empty(), "{:?}", derived.held);
	assert_same_maps(&baseline, &derived, &local, &remote.nodes);
	let actions = assert_same_plan(&baseline, &derived, &local, &remote.nodes);
	assert!(
		actions.len() >= 6,
		"the change set plans a pass's worth of work: {actions:?}"
	);
}

/// An interrupted pass hands the next one BOTH halves of what it took, and that pass re-plans
/// the same action from them without reading either side whole.
///
/// The action here is REMOTE-originated — the local delete answering a `Gone` — which is the
/// one the local half alone cannot reproduce: the baseline row a dirty path's remote side is
/// derived from still records the item, so a hand-back missing the delta plans nothing at all
/// and the deletion waits for the safety net. Both shapes are asserted, the broken one first.
#[test]
fn an_interrupted_passs_remote_action_is_re_planned_from_both_halves() {
	let mut pair = Pair::converged();
	let doomed = pair.uuids["docs/Ärger.txt"];
	pair.items.retain(|item| item.uuid != doomed);
	let baseline = pair.baseline();
	let announced = delta_of(&[cache_event(
		Some(1),
		CacheEventType::File(FileEvent::Removed(doomed)),
	)]);

	// The pass that plans the delete: nothing announced locally, one removal announced
	// remotely.
	let first = pair.derived(&baseline, dirty_paths(&[]), &announced);
	let planned = scoped_plan(&baseline, &first);
	assert_eq!(planned.len(), 1, "{planned:?}");
	assert!(
		matches!(planned[0], SyncAction::DeleteLocal { .. })
			&& planned[0].rel_path() == "docs/Ärger.txt",
		"the announced removal plans the local delete: {planned:?}"
	);

	// It is cut short before applying it, so no row was written and the file is still on disk.
	// What it owes is its plan's paths...
	let (no_paths, no_failures) = (BTreeSet::new(), std::collections::HashMap::new());
	let ends = planned.iter().flat_map(|action| {
		let (from, to) = action.endpoints();
		[from, to]
	});
	let owed: BTreeSet<String> = carry_over(ends, &[], &no_paths, &no_failures)
		.map(str::to_owned)
		.collect();
	assert_eq!(owed, BTreeSet::from(["docs/Ärger.txt".to_string()]));

	// ...and handing back that half ALONE — the shape this stage's first attempt shipped —
	// plans nothing: the row the remote side is derived from still records the item.
	let local_only = pair.derived(&baseline, owed.clone(), &[]);
	assert!(
		scoped_plan(&baseline, &local_only).is_empty(),
		"the remote half is what carries the absence; without it the deletion is lost"
	);

	// Both halves, through the changelist the next pass takes from: the same action again.
	let changes = PairChanges::new();
	changes.cover_local();
	changes.note_owed(owed.iter().map(String::as_str));
	changes.note_owed_remote(announced);
	let mut scope = changes.take();
	assert_eq!(
		scope.full_pass_reason(baseline.len()),
		None,
		"a hand-back must not send the next pass back to a whole read"
	);
	let second = pair.derived(&baseline, scope.take_local(), &scope.take_remote());
	assert_eq!(
		scoped_plan(&baseline, &second),
		planned,
		"the next pass must re-plan what the interrupted one did not get to"
	);
}

/// A path the baseline has no row for cannot be derived — and the consequence is a deferred
/// CREATE, never a deletion. (A whole-tree pass pushes it; this one leaves it for the next one.)
#[test]
fn a_path_with_no_row_is_missed_by_derivation_and_never_deleted() {
	let pair = Pair::converged();
	// Drop the row for a directory that still holds rows under it, exactly as an untracked
	// destination-only parent leaves the tree: a node in the baseline with no row of its own.
	let rows: Vec<BaselineEntry> = pair
		.rows
		.iter()
		.filter(|row| row.rel_path != "docs")
		.cloned()
		.collect();
	let baseline = Baseline::from_rows(rows);

	let derived = pair.derived(&baseline, dirty_paths(&[]), &[]);
	let (local, remote) = pair.whole_tree(&baseline);

	assert!(
		local.whole().holds("docs") && !derived.local.of(&baseline).holds("docs"),
		"a whole-tree scan sees the directory; derivation has no row to carry"
	);
	assert!(
		!derived.remote.of(&baseline).holds("docs"),
		"and no remote node either"
	);
	let holds = PassHolds::default();
	let scoped = plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&derived.local.of(&baseline),
		&derived.remote.of(&baseline),
		&holds,
		plan::PassPaths::Whole,
	);
	assert!(
		!scoped.actions.iter().any(|action| matches!(
			action,
			SyncAction::DeleteLocal { .. } | SyncAction::TrashRemote { .. }
		)),
		"a path in neither map is decided by nobody: {:?}",
		scoped.actions
	);
	let whole = plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&local.whole(),
		&remote.nodes.whole(),
		&holds,
		plan::PassPaths::Whole,
	);
	assert!(
		whole.actions.len() > scoped.actions.len(),
		"the whole-tree pass is the one that picks it up: {:?}",
		whole.actions
	);
}

/// Two siblings whose names fold together are two rows, and two keys, in both maps. The store
/// compares bytewise and so do the maps; the collision is the reconcile's business, not the
/// assembly's. (Only the baseline can hold such a pair on a case-insensitive filesystem, which
/// is why this is asserted on the rows rather than on disk.)
#[test]
fn case_only_siblings_stay_two_rows_in_both_maps() {
	let uuid = |at: u8| Uuid::from_u128(700 + u128::from(at));
	let row = |rel_path: &str, at: u8| BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid(at)),
		content_hash: Some(Blake3Hash::from([at; 32])),
		size: Some(3),
		local_mtime: Some(10),
		remote_modified: Some(20),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid(at))),
		agreed_hash: Some(Blake3Hash::from([at; 32])),
	};
	let baseline = Baseline::from_rows([row("A.txt", 1), row("a.txt", 2)]);

	let derived = from_baseline(&baseline, BTreeSet::new());

	assert_eq!(
		sorted(
			derived
				.local
				.of(&baseline)
				.paths()
				.map(|path| path.into_owned())
		),
		vec!["A.txt", "a.txt"]
	);
	assert_eq!(
		derived
			.remote
			.of(&baseline)
			.at("A.txt")
			.unwrap()
			.remote_uuid,
		uuid(1),
		"the folded name is not the key"
	);
	assert_eq!(
		derived
			.remote
			.of(&baseline)
			.at("a.txt")
			.unwrap()
			.remote_uuid,
		uuid(2)
	);
}

/// What a walk PRUNED it never looked at: the rows under an ignored entry stay, while the rows
/// the walk covered and did not find are gone.
#[test]
fn a_walk_carries_the_rows_it_pruned_and_drops_the_ones_it_covered() {
	let pair = Pair::converged();
	// `.DS_Store` is hidden by the built-in defaults, and tracked, so the walk records it as a
	// pruned root rather than as noise.
	write(&pair.root, "docs/.DS_Store", b"x");
	let mut rows = pair.rows.clone();
	let template = pair
		.rows
		.iter()
		.find(|row| row.rel_path == "docs/notes.txt")
		.unwrap()
		.clone();
	rows.push(BaselineEntry {
		rel_path: "docs/.DS_Store".to_string(),
		remote_uuid: Some(Uuid::from_u128(800)),
		..template
	});
	let baseline = Baseline::from_rows(rows);
	// Everything under `docs` is covered by the walk of it — except what the walk pruned.
	fs::remove_file(pair.root.join("docs/notes.txt")).unwrap();

	let derived = pair.derived(&baseline, dirty_paths(&["docs"]), &[]);

	assert!(
		!derived.local.of(&baseline).holds("docs/notes.txt"),
		"the walk covered it and did not find it"
	);
	assert!(
		derived.local.of(&baseline).holds("docs/.DS_Store"),
		"the walk pruned it, so its row is not evidence of absence: {:?}",
		sorted(
			derived
				.local
				.of(&baseline)
				.paths()
				.map(|path| path.into_owned())
		)
	);
	assert!(derived.local.of(&baseline).holds("docs/deep/inner.bin"));
}

/// A path the rules hide is taken out of the LOCAL map as well as the remote one, and held
/// where a row was carried there, so the two halves agree and nothing is planned at it.
///
/// The shape that reaches this is an `untrack_ignored` whose `delete_subtrees` failed: rows
/// survive under a root the rules hide. Carrying the local node while the view's filter drops
/// the remote one reads as a remote deletion — a local delete planned on a file the user still
/// has, with only the pass's `Blocking` standing in front of it.
#[test]
fn a_hidden_path_with_a_carried_row_is_dropped_held_and_plans_nothing() {
	let pair = Pair::converged();
	let baseline = pair.baseline();
	// The rule arrives after the rows: what it hides is tracked, and stayed tracked.
	fs::write(pair.root.join(FILENIGNORE), "deep/\n").unwrap();

	let mut derived = from_baseline(&baseline, dirty_paths(&["docs/deep"]));
	let (mut observed, rules) = observe_local(
		&pair.root,
		PairRoot::Directory,
		&baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		&derived.dirty,
	)
	.unwrap();
	assert!(
		matches!(observed.observed["docs/deep"], LocalObservation::Hidden(_)),
		"the rule has to be what the observation reports: {:?}",
		observed.observed
	);
	merge_local(&mut derived, &baseline, &mut observed);
	// The remote half, filtered with the very rules the observation matched with — which is
	// what `prepare_scoped` does a few lines after its own `merge_local`.
	let mut view = RemoteView {
		nodes: std::mem::take(&mut derived.remote),
		collisions: BTreeSet::new(),
		held_paths: BTreeSet::new(),
		skipped: Vec::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
	};
	view.filter_changed(
		plan::ViewFilter {
			rules: &rules,
			baseline: &baseline,
		},
		&derived.decided,
	);
	derived.remote = view.nodes;

	for path in ["docs/deep", "docs/deep/inner.bin"] {
		assert!(
			!derived.local.of(&baseline).holds(path),
			"the local half still describes {path:?}: {:?}",
			sorted(
				derived
					.local
					.of(&baseline)
					.paths()
					.map(|path| path.into_owned())
			)
		);
		assert!(
			!derived.remote.of(&baseline).holds(path),
			"the remote half still describes {path:?}"
		);
	}
	assert!(
		derived.held.contains("docs/deep"),
		"a hidden path with rows behind it is withheld, or its two absences retire the row: \
			 {:?}",
		derived.held
	);
	let planned = scoped_plan(&baseline, &derived);
	assert!(
		planned.is_empty(),
		"a hidden path is decided by nobody: {planned:?}"
	);
}

/// An incomplete walk may have missed anything, so it drops nothing: an absence it seems to
/// show is exactly the absence I1 forbids inventing.
#[cfg(unix)]
#[test]
fn an_incomplete_walk_drops_no_row() {
	use std::os::unix::fs::PermissionsExt;

	let pair = Pair::converged();
	let baseline = pair.baseline();
	let locked = pair.root.join("docs/deep");
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

	let mut derived = from_baseline(&baseline, BTreeSet::from(["docs".to_string()]));
	let (mut observed, _) = observe_local(
		&pair.root,
		PairRoot::Directory,
		&baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		&derived.dirty,
	)
	.unwrap();
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
	assert!(!observed.complete, "the walk could not read a subtree");
	merge_local(&mut derived, &baseline, &mut observed);

	assert!(
		derived.local.of(&baseline).holds("docs/deep/inner.bin"),
		"an unreadable subtree's rows are carried, not deleted: {:?}",
		sorted(
			derived
				.local
				.of(&baseline)
				.paths()
				.map(|path| path.into_owned())
		)
	);
}

/// Every [`BaselineState`], listed once. [`state_index`] is what makes this exhaustive rather
/// than merely long enough: it has no wildcard arm, so a new variant does not compile until it
/// is named there, and the assertion in the test below then demands it be listed here too.
const ALL_STATES: [BaselineState; 4] = [
	BaselineState::Synced,
	BaselineState::Conflicted,
	BaselineState::Overwritten,
	BaselineState::Adopted,
];

/// Where each state sits in [`ALL_STATES`].
fn state_index(state: BaselineState) -> usize {
	match state {
		BaselineState::Synced => 0,
		BaselineState::Conflicted => 1,
		BaselineState::Overwritten => 2,
		BaselineState::Adopted => 3,
	}
}

/// [`BaselineEntry::carryable`] must be exactly "`Synced`, and the fields [`extracted`] reads
/// are all there" — checked against a second implementation of each half, over every row shape.
///
/// Both directions matter and both can fail here. A rule LOOSER than the extraction would have
/// the resident tree index a row as carryable that derives nothing: a path in neither map, held
/// by nobody, which is the shape invariant I1 forbids. A rule STRICTER than it fabricates no
/// absence, but [`from_baseline`] puts such a row in `dirty` AND `held` in one statement, so it
/// stalls its path on every pass rather than one. Every state, both kinds, and each optional
/// field the extraction reads cleared in turn — which is every way a row can fail to describe a
/// side.
#[test]
fn the_carryable_rule_and_the_extraction_agree_on_every_row_shape() {
	let uuid = Uuid::from_u128(1);
	let hash = Blake3Hash::from([1; 32]);
	let whole = BaselineEntry {
		rel_path: "x".to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid),
		content_hash: Some(hash),
		size: Some(3),
		local_mtime: Some(10),
		remote_modified: Some(20),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: Some(hash),
	};
	/// One way a row can fail to describe a side: what to call it in the failure message,
	/// and the field it clears.
	type Clear = (&'static str, fn(&mut BaselineEntry));
	let clears: [Clear; 8] = [
		("nothing", |_| {}),
		("remote_uuid", |row| row.remote_uuid = None),
		("content_hash", |row| row.content_hash = None),
		("size", |row| row.size = None),
		("local_mtime", |row| row.local_mtime = None),
		("remote_modified", |row| row.remote_modified = None),
		("remote_stable_uuid", |row| row.remote_stable_uuid = None),
		("agreed_hash", |row| row.agreed_hash = None),
	];
	let mut checked = 0;
	let mut carried_some = 0;
	let mut extracted_some = 0;
	for (at, state) in ALL_STATES.into_iter().enumerate() {
		assert_eq!(
			state_index(state),
			at,
			"{state:?} is not where the wildcard-free match puts it: the corpus has drifted \
				 from the enum"
		);
		for kind in [NodeKind::File, NodeKind::Dir] {
			for (what, clear) in &clears {
				let mut row = BaselineEntry {
					state,
					kind,
					..whole.clone()
				};
				clear(&mut row);
				assert_eq!(
					row.carryable(),
					row.state == BaselineState::Synced && extracted(&row).is_some(),
					"{state:?} {kind:?} with {what} cleared: the rule and the extraction disagree"
				);
				checked += 1;
				carried_some += usize::from(carried(&row).is_some());
				extracted_some += usize::from(extracted(&row).is_some());
			}
		}
	}
	assert_eq!(
		checked,
		ALL_STATES.len() * 2 * clears.len(),
		"every state/kind/field combination is covered"
	);
	// Both halves of the assertion have to answer both ways over this corpus, or it could not
	// have failed. The extraction succeeds for 10 rows of EVERY state — a directory needs only
	// its remote uuid (7 of 8 clears), a file needs all four fields (3 of 8) — and the state
	// term then keeps the `Synced` quarter of those.
	assert_eq!(
		extracted_some,
		10 * ALL_STATES.len(),
		"the extraction must answer both ways, or the state term alone would carry the test"
	);
	assert_eq!(
		carried_some, 10,
		"the corpus must exercise both answers, not just one"
	);
}

/// A dirty path the pass got no reading for — a socket, a vanished file, an unreadable
/// ancestor — keeps its row: no observation is not an absence.
#[test]
fn a_dirty_path_with_no_observation_keeps_its_row() {
	let pair = Pair::converged();
	let baseline = pair.baseline();
	let mut derived = from_baseline(&baseline, BTreeSet::new());
	let mut observed = LocalObservations {
		observed: BTreeMap::new(),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		gaps: ScanGaps::default(),
		errors: Vec::new(),
	};

	merge_local(&mut derived, &baseline, &mut observed);

	assert!(derived.local.of(&baseline).holds("top.txt"));
	assert!(derived.local.of(&baseline).holds("docs/deep/inner.bin"));
}

/// A walk's nodes MOVE into the local side rather than being copied out of the scan, so the pass
/// stops holding two copies of every node it walked — and the assembly check, which counts what
/// each walk found, takes the same bounds before the merge as from observations nothing has
/// touched, and the merged map lands inside them.
///
/// The shape is a directory rename, the largest walk a scoped pass makes: an absence at the old
/// path and a walk of the new one.
#[test]
fn merging_a_walk_moves_its_nodes_and_leaves_the_assembly_bounds_as_they_were() {
	let pair = Pair::converged();
	let baseline = pair.baseline();
	fs::rename(pair.root.join("docs"), pair.root.join("moved")).unwrap();
	let mut derived = from_baseline(&baseline, dirty_paths(&["docs", "moved"]));
	let dirty = derived.dirty.clone();
	let observe = || {
		observe_local(
			&pair.root,
			PairRoot::Directory,
			&baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&dirty,
		)
		.unwrap()
		.0
	};
	let (mut merged, untouched) = (observe(), observe());
	let held = derived.held.clone();

	let bounds = assembly_bounds(&baseline, &merged, &held);
	merge_local(&mut derived, &baseline, &mut merged);

	let walks: Vec<&LocalScan> = merged
		.observed
		.values()
		.filter_map(|observation| match observation {
			LocalObservation::Dir(scan) => Some(&**scan),
			_ => None,
		})
		.collect();
	assert_eq!(walks.len(), 1, "{:?}", merged.observed.keys());
	assert!(
		walks[0].nodes.whole().is_empty(),
		"the walk still holds the nodes it handed the local side: the pass holds them twice"
	);
	let local = derived.local.of(&baseline);
	for path in ["moved", "moved/notes.txt", "moved/deep/inner.bin"] {
		assert!(
			local.holds(path),
			"{path:?} was walked and is not in the local side"
		);
	}
	assert!(!local.holds("docs/notes.txt"), "the old path is gone");
	assert_eq!(
		bounds,
		assembly_bounds(&baseline, &untouched, &held),
		"the bounds taken before the merge are the ones the untouched observations give"
	);
	assert!(
		bounds.contains(&local.len()),
		"the merged map ({} node(s)) is outside {bounds:?}",
		local.len()
	);
}

/// A change-scoped pass as `prepare_scoped` and the plan run it, on this thread: assembled from
/// the changelists `lists` builds, its directory moves folded, and reconciled at the set it
/// decided. Hands back how many actions it planned and the most bytes it held at once — the
/// changelists it took included, since they are built inside.
fn scoped_pass_peak(
	root: &Path,
	baseline: &Baseline,
	lists: impl FnOnce() -> (BTreeSet<String>, Vec<RemoteDeltaEntry>),
) -> (usize, u64) {
	alloc_meter::peak_bytes(|| {
		let (dirty, delta) = lists();
		let mut derived = assembled(root, baseline, dirty, &delta);
		let mut rows = baseline.clone();
		let mut planned = plan::fold_dir_moves(
			SyncMode::TwoWay,
			&mut rows,
			&mut derived.local,
			&mut derived.remote,
			&derived.held,
			Some(&mut derived.decided),
		)
		.expect("a pass within the caps folds its moves");
		planned.extend(scoped_plan(&rows, &derived));
		planned.len()
	})
}

/// `entries` new local files on disk under `root` and as many new remote ones announced, and the
/// peak of the change-scoped pass that takes both lists full.
fn both_lists_full(entries: usize) -> (usize, u64) {
	let root = temp_root();
	let local: Vec<String> = (0..entries).map(|n| format!("l{n:05}.txt")).collect();
	for path in &local {
		fs::write(root.join(path), b"x").unwrap();
	}
	let measured = scoped_pass_peak(&root, &Baseline::default(), || {
		let delta = (0..entries)
			.map(|n| {
				let uuid = Uuid::from_u128(10_000 + n as u128);
				RemoteDeltaEntry {
					id: None,
					change: RemoteChange::Upsert(RemoteItem {
						uuid,
						parent: REMOTE_ROOT,
						name: format!("r{n:05}.txt"),
						stable_uuid: Some(StableUuid::new_for_test(uuid)),
						hash: Some(Blake3Hash::from([5; 32])),
						size: 1,
						modified_millis: 1_234,
					}),
				}
			})
			.collect();
		(local.iter().cloned().collect(), delta)
	});
	fs::remove_dir_all(&root).ok();
	measured
}

/// A change-scoped pass at both changelists' caps — as many new local files as the local list
/// holds and as many new remote ones as the remote list holds, each of them planned — fits in
/// 16 MiB, its changelists included. This is what sizes [`DIRTY_CAP`]: 7,700 entries a list
/// peak at 15.90 MiB here, 7,800 at 16.02.
#[test]
fn a_scoped_pass_at_both_changelist_caps_peaks_under_16_mib() {
	let (planned, peak) = both_lists_full(DIRTY_CAP);
	assert_eq!(
		planned,
		2 * DIRTY_CAP,
		"an upload per local file and a download per remote one"
	);
	assert!(
		peak <= 16 << 20,
		"the pass peaked at {peak} bytes at the changelists' cap of {DIRTY_CAP}"
	);
}

/// A change-scoped pass over one directory holding as many files as a pass expands one change
/// into, renamed on the remote and still under its old name on disk: the view re-keys every row,
/// the walk lists every file, and the fold carries them across as one move. The heaviest shape
/// one change at [`SUBTREE_CAP`] takes, and it peaks at 16.66 MiB — past the 16 MiB the
/// changelists' cap is sized to, by a hash table doubling between 14,000 files (10.81 MiB) and
/// 14,336, and bounded here at 17 MiB.
#[test]
fn a_scoped_pass_at_the_subtree_cap_peaks_under_17_mib() {
	let (planned, peak) = renamed_dir_peak(SUBTREE_CAP);
	assert_eq!(planned, 1, "the rename, carried across as one move");
	assert!(
		peak <= 17 << 20,
		"the pass peaked at {peak} bytes over a directory of {SUBTREE_CAP} files"
	);
}

/// `files` files under `d`, synced and still there on disk, with the remote renaming `d` to `e`;
/// and the peak of the change-scoped pass that takes the rename.
fn renamed_dir_peak(files: usize) -> (usize, u64) {
	let root = temp_root();
	let dir = Uuid::from_u128(2);
	fs::create_dir(root.join("d")).unwrap();
	let dir_local = LocalNode {
		rel_path: "d".to_owned(),
		kind: NodeKind::Dir,
		size: 0,
		mtime_millis: 99,
		mtime_nanos: 0,
		content_hash: None,
	};
	let dir_remote = RemoteNode {
		rel_path: "d".to_owned(),
		kind: NodeKind::Dir,
		remote_uuid: dir,
		stable_uuid: None,
		content_hash: None,
		size: 0,
		modified_millis: 99,
	};
	let mut rows = vec![synced_row(&dir_local, &dir_remote)];
	for n in 0..files {
		let rel_path = format!("d/{n:05}");
		let file = root.join(&rel_path);
		fs::write(&file, b"x").unwrap();
		let mtime_millis = FilenMetaExt::modified(&fs::metadata(&file).unwrap()).timestamp_millis();
		let uuid = Uuid::from_u128(10_000 + n as u128);
		let hash = Blake3Hash::from([6; 32]);
		let local = LocalNode {
			rel_path: rel_path.clone(),
			kind: NodeKind::File,
			size: 1,
			mtime_millis,
			mtime_nanos: 0,
			content_hash: Some(hash),
		};
		let remote = RemoteNode {
			rel_path,
			kind: NodeKind::File,
			remote_uuid: uuid,
			stable_uuid: Some(StableUuid::new_for_test(uuid)),
			content_hash: Some(hash),
			size: 1,
			modified_millis: mtime_millis,
		};
		rows.push(synced_row(&local, &remote));
	}
	let baseline = Baseline::from_rows(rows);

	let (planned, peak) = scoped_pass_peak(&root, &baseline, || {
		let renamed = RemoteDeltaEntry {
			id: None,
			change: RemoteChange::Upsert(RemoteItem {
				uuid: dir,
				parent: REMOTE_ROOT,
				name: "e".to_owned(),
				stable_uuid: None,
				hash: None,
				size: 0,
				modified_millis: 99,
			}),
		};
		(BTreeSet::new(), vec![renamed])
	});

	fs::remove_dir_all(&root).ok();
	(planned, peak)
}
