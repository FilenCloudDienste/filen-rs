//! What the tests stage a pass's sides with, and the oracle a staged remote view is held to.

use std::{collections::BTreeMap, num::NonZeroUsize};

use uuid::Uuid;

use super::{
	super::{
		plan::{RemoteNode, RemoteView},
		scan::{Found, LocalNode, PairRoot, ScanSink, collision_key},
		side::{Nodes, Side},
	},
	Paging, RemoteSink, Stage, StageSink, stage_path,
};
use crate::cache::{RemoteItem, SnapshotSink, UndecodableItem};

impl Stage {
	/// A stage beside a baseline in a directory of its own, which goes with the directory.
	pub(in super::super) fn in_temp_dir() -> (tempfile::TempDir, Self) {
		let dir = tempfile::tempdir().expect("a temp dir for the stage");
		let stage = Self::create(stage_path(&dir.path().join("baseline.db"), 1, 0))
			.expect("creating a stage");
		(dir, stage)
	}
}

/// Every statement over a staged side taking `rows` rows at most, for a test that walks, moves and
/// places a handful of nodes across several statements.
pub(in super::super) fn paging_of(rows: usize) -> Paging {
	let rows = NonZeroUsize::new(rows).expect("a test pages at least one row a statement");
	Paging {
		page: rows,
		rekey: rows,
		children: rows,
	}
}

/// A whole pass's two sides staged from `local` and `remote`, as its walk and its snapshot stage
/// them, read `page` rows a statement (see [`paging_of`]); with the directory their stages are in.
/// The local nodes must not fold together, as a walk's never do.
pub(in super::super) fn staged_sides(
	local: impl IntoIterator<Item = LocalNode>,
	remote: impl IntoIterator<Item = RemoteNode>,
	page: usize,
) -> (tempfile::TempDir, Side<LocalNode>, Side<RemoteNode>) {
	let paging = paging_of(page);
	let dir = tempfile::tempdir().expect("a temp dir for the stages");
	let baseline = dir.path().join("baseline.db");
	let stage = |pass| Stage::create(stage_path(&baseline, 1, pass)).expect("creating a stage");
	let mut walk = StageSink::new(stage(0), paging).expect("a stage for the walk");
	for node in local {
		let put = walk.put(Found::Node(node)).expect("staging a local node");
		assert_eq!(put, None, "two local nodes fold together");
	}
	let local = walk.finish().expect("staging the walk");
	let mut remote_side = RemoteSink::new(stage(1), paging)
		.and_then(|snapshot| snapshot.finish(Uuid::nil(), PairRoot::Directory))
		.expect("an empty staged snapshot")
		.nodes;
	remote_side.extend(remote.into_iter().map(|node| (node.rel_path.clone(), node)));
	(dir, local, remote_side)
}

/// A view staged from `items` and `undecodable`, handed over in that order as a read hands them, in
/// a stage of its own and placed under `root`; with the directory its stage is in.
pub(in super::super) fn staged_view(
	root: Uuid,
	pair_root: PairRoot,
	items: impl IntoIterator<Item = (bool, RemoteItem)>,
	undecodable: &[UndecodableItem],
) -> (tempfile::TempDir, RemoteView) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut sink = RemoteSink::new(stage, Paging::PASS).expect("a stage for the remote snapshot");
	sink.undecodable(undecodable.to_vec());
	for (is_dir, item) in items {
		sink.item(is_dir, item);
	}
	(
		dir,
		sink.finish(root, pair_root)
			.expect("placing the staged snapshot"),
	)
}

/// Hold a view a whole pass staged to the one `ViewBuilder` built from the same items: the same
/// nodes at the same paths, the same held paths, ignored roots and count of untracked default hits,
/// and the same records in any order (nothing reads them in order: `unknown_remote_paths` keys them
/// by path). Where the views found collisions, both name the same paths for them and each kept one
/// node of every folded name, and which one is the one thing they may differ in: the pass holds
/// back every path of the group either way.
pub(in super::super) fn assert_views_alike(at: &str, resident: &RemoteView, staged: &RemoteView) {
	assert!(
		matches!(staged.nodes, Side::Staged(_)),
		"{at}: the staged view is staged"
	);
	assert_eq!(staged.collisions, resident.collisions, "{at}: collisions");
	assert_eq!(staged.held_paths, resident.held_paths, "{at}: held_paths");
	assert_eq!(staged.ignored, resident.ignored, "{at}: ignored");
	assert_eq!(
		staged.ignored_default_untracked, resident.ignored_default_untracked,
		"{at}: ignored_default_untracked"
	);
	let records = |view: &RemoteView| {
		let mut records = view.skipped.clone();
		records.sort_by_key(|record| record.remote_uuid);
		records
	};
	assert_eq!(records(staged), records(resident), "{at}: skipped");
	let nodes = |view: &RemoteView| -> BTreeMap<String, RemoteNode> {
		view.nodes
			.whole()
			.iter()
			.map(|(path, node)| (path.into_owned(), node.into_owned()))
			.collect()
	};
	let (resident_nodes, staged_nodes) = (nodes(resident), nodes(staged));
	assert_eq!(
		staged.nodes.whole().len(),
		staged_nodes.len(),
		"{at}: the staged view counts what it holds"
	);
	if !resident.has_collisions() {
		assert_eq!(staged_nodes, resident_nodes, "{at}: nodes");
		return;
	}
	let folded = |nodes: &BTreeMap<String, RemoteNode>| -> BTreeMap<String, RemoteNode> {
		let folded: BTreeMap<String, RemoteNode> = nodes
			.values()
			.map(|node| (collision_key(&node.rel_path), node.clone()))
			.collect();
		assert_eq!(
			folded.len(),
			nodes.len(),
			"{at}: two nodes left fold together"
		);
		folded
	};
	let (resident_folded, staged_folded) = (folded(&resident_nodes), folded(&staged_nodes));
	assert_eq!(
		staged_folded.keys().collect::<Vec<_>>(),
		resident_folded.keys().collect::<Vec<_>>(),
		"{at}: the folded names left"
	);
	for (key, kept) in &resident_folded {
		let twin = staged_folded
			.get(key)
			.expect("the folded names were just compared");
		// The same node, or the other of two twins.
		if twin.rel_path == kept.rel_path {
			assert_eq!(twin, kept, "{at}: the node at {key:?}");
		}
	}
}
