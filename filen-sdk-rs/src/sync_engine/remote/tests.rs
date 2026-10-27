use std::borrow::Cow;

use uuid::Uuid;

use filen_types::crypto::Blake3Hash;

use super::*;
use crate::{
	alloc_meter,
	cache::{CacheEvent, CacheEventType, DirEvent, FileEvent},
	fs::file::{cache::CacheableFile, meta::DecryptedFileMeta},
	sync_engine::{
		baseline::{BaselineEntry, BaselineState},
		changes::{
			PairChanges,
			tests::{cache_event, cacheable_dir, cacheable_file, dt, file_key},
		},
		ignore::FILENIGNORE,
	},
};

/// The pair's remote root: every derived path is relative to it.
fn root() -> Uuid {
	Uuid::from_u128(1_000)
}

fn item_file(uuid: Uuid, parent: Uuid, name: &str) -> RemoteItem {
	RemoteItem {
		uuid,
		parent,
		name: name.to_owned(),
		stable_uuid: Some(StableUuid::new_for_test(uuid)),
		hash: None,
		size: 7,
		modified_millis: 1_234,
	}
}

fn item_dir(uuid: Uuid, parent: Uuid, name: &str) -> RemoteItem {
	RemoteItem {
		uuid,
		parent,
		name: name.to_owned(),
		stable_uuid: None,
		hash: None,
		size: 0,
		modified_millis: 99,
	}
}

fn placed(path: &str, item: &RemoteItem) -> (String, RemoteNode) {
	let mut node = node_of(item);
	node.rel_path = path.to_owned();
	(path.to_owned(), node)
}

fn file_node(path: &str, uuid: Uuid) -> (String, RemoteNode) {
	placed(path, &item_file(uuid, Uuid::nil(), path))
}

fn dir_node(path: &str, uuid: Uuid) -> (String, RemoteNode) {
	placed(path, &item_dir(uuid, Uuid::nil(), path))
}

fn view(nodes: impl IntoIterator<Item = (String, RemoteNode)>) -> Side<RemoteNode> {
	nodes.into_iter().collect()
}

/// The pair state a change-scoped pass starts from: the resident rows, and the derived map
/// `derive::from_baseline` makes of exactly those rows.
///
/// The two are built together because that pairing is what `observe_remote` is handed — the
/// engine takes `derived.remote` and the very baseline it came from, and
/// `RemoteObservation::path_of` reads the tree's uuid index for every node the delta leaves
/// alone. A map over a baseline that knows nothing about it is a shape no pass produces.
fn derived(nodes: impl IntoIterator<Item = (String, RemoteNode)>) -> (Baseline, Side<RemoteNode>) {
	let nodes = view(nodes);
	let baseline = Baseline::from_rows(nodes.whole().iter().map(|(_, node)| {
		let mut entry = row(&node.rel_path, node.kind, Some(node.remote_uuid));
		entry.remote_stable_uuid = node.stable_uuid;
		entry
	}));
	(baseline, nodes)
}

fn row(rel_path: &str, kind: NodeKind, remote_uuid: Option<Uuid>) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_owned(),
		kind,
		remote_uuid,
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

fn meta(name: &str) -> DecryptedFileMeta<'static> {
	DecryptedFileMeta {
		name: Cow::Owned(name.to_owned()),
		size: 7,
		mime: Cow::Borrowed("text/plain"),
		key: file_key(),
		last_modified: dt(500),
		created: None,
		hash: None,
	}
}

/// The delta a batch of cache events produces, taken exactly as a pass takes it — the only way
/// a `Gone` can be minted, which is the point of its private fields.
fn delta(events: &[CacheEvent<'static>]) -> Vec<RemoteDeltaEntry> {
	let changes = PairChanges::new();
	changes.note_tree_size(4_000);
	changes.note_remote_batch(&mut events.iter());
	let entries = changes.take().take_remote();
	assert!(
		!entries.is_empty(),
		"the producer collapsed the batch instead of recording it"
	);
	entries
}

fn no_ancestry() -> impl FnMut(Uuid) -> rusqlite::Result<Vec<RemoteItem>> {
	|uuid: Uuid| -> rusqlite::Result<Vec<RemoteItem>> {
		panic!("no ancestry read was expected, but one was made for {uuid}")
	}
}

/// `observe_remote` as a pass reaches it: the ids the delta names resolved off the baseline
/// first, exactly as the engine resolves them off the store.
///
/// `also` is for the ids a test goes on to ASK about itself (`out.path_of(..)`), which a real
/// pass would only ask about because the delta named them.
fn observed_with(
	baseline: &Baseline,
	nodes: Side<RemoteNode>,
	delta: &[RemoteDeltaEntry],
	also: &[Uuid],
	ancestry: Ancestry<'_>,
) -> RemoteObserved {
	let mut uuids = delta_uuids(delta);
	uuids.extend_from_slice(also);
	let placed = baseline.synced_paths(&uuids, &[]);
	observe_remote(
		root(),
		PairRoot::Directory,
		baseline,
		nodes,
		delta,
		placed,
		ancestry,
	)
}

fn applied(observed: RemoteObserved) -> RemoteObservation {
	match observed {
		RemoteObserved::Applied(observation) => *observation,
		RemoteObserved::Full(reason) => {
			panic!("expected a derived view, got a full pass: {reason}")
		}
	}
}

fn paths(out: &RemoteObservation) -> Vec<String> {
	let mut paths: Vec<String> = out
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort_unstable();
	paths
}

fn listed(set: &BTreeSet<String>) -> Vec<&str> {
	set.iter().map(String::as_str).collect()
}

/// Two entries can name one path — a create over a path a removal just vacated, and the same
/// pair the other way round. What the cache dispatched LAST is the state, in both orders.
#[test]
fn entries_naming_one_path_are_applied_in_dispatch_order() {
	let file = Uuid::from_u128(1);
	let born = || {
		cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(file, root(), "a.txt"))),
		)
	};
	let died = || cache_event(Some(2), CacheEventType::File(FileEvent::Removed(file)));

	let empty = Baseline::default();
	let out = applied(observed_with(
		&empty,
		Side::default(),
		&delta(&[born(), died()]),
		&[file],
		&mut no_ancestry(),
	));
	assert!(out.nodes.whole().is_empty(), "{:?}", out.nodes);
	assert!(out.path_of(&empty, file).is_none());
	assert_eq!(listed(&out.touched), vec!["a.txt"]);

	let out = applied(observed_with(
		&empty,
		Side::default(),
		&delta(&[died(), born()]),
		&[file],
		&mut no_ancestry(),
	));
	assert_eq!(paths(&out), vec!["a.txt"], "the create is the last word");
	assert_eq!(out.path_of(&empty, file).as_deref(), Some("a.txt"));
}

/// An item that arrives in the account's data directory of a drive-rooted pair is in no view, as a
/// whole read leaves it out, and one that moves there leaves the view; a pair rooted at a
/// directory places both.
#[test]
fn an_item_arriving_in_the_account_data_directory_of_a_drive_rooted_pair_is_in_no_view() {
	let (account, moved, created) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
	let events = [
		cache_event(
			Some(1),
			CacheEventType::File(FileEvent::New(cacheable_file(created, account, "c.bin"))),
		),
		cache_event(
			Some(2),
			CacheEventType::File(FileEvent::New(cacheable_file(moved, account, "a.txt"))),
		),
	];
	let delta = delta(&events);
	// Neither view holds the directory: the cache's ancestry is what places it.
	let mut ancestry = |uuid: Uuid| -> rusqlite::Result<Vec<RemoteItem>> {
		Ok(vec![
			item_file(uuid, account, "f"),
			item_dir(account, root(), ".filen"),
		])
	};
	let mut observe = |pair_root| {
		let (baseline, nodes) = derived([
			file_node("a.txt", moved),
			file_node("b.txt", Uuid::from_u128(4)),
		]);
		let placed = baseline.synced_paths(&delta_uuids(&delta), &[]);
		applied(observe_remote(
			root(),
			pair_root,
			&baseline,
			nodes,
			&delta,
			placed,
			&mut ancestry,
		))
	};
	assert_eq!(paths(&observe(PairRoot::Drive)), vec!["b.txt"]);
	assert_eq!(
		paths(&observe(PairRoot::Directory)),
		vec![".filen/a.txt", ".filen/c.bin", "b.txt"]
	);
}

/// A subtree created — or moved in — out of view: neither the derived map nor the baseline has
/// ever seen its parent chain, so the cache's own indexed walk is what says where it sits. One
/// walk, not one read per ancestor.
#[test]
fn a_subtree_created_out_of_view_takes_its_path_from_the_cached_ancestry() {
	let outer = Uuid::from_u128(11);
	let inner = Uuid::from_u128(12);
	let file = Uuid::from_u128(13);
	let chain = vec![
		item_file(file, inner, "f.txt"),
		item_dir(inner, outer, "sub"),
		item_dir(outer, root(), "out"),
	];
	let events = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::New(cacheable_file(file, inner, "f.txt"))),
	)];

	let mut reads = 0usize;
	let mut ancestry = |uuid: Uuid| {
		reads += 1;
		assert_eq!(uuid, file, "the chain is asked for by the item's own uuid");
		Ok(chain.clone())
	};
	let empty = Baseline::default();
	let out = applied(observed_with(
		&empty,
		Side::default(),
		&delta(&events),
		&[],
		&mut ancestry,
	));

	assert_eq!(reads, 1, "one indexed walk answers the whole chain");
	assert_eq!(paths(&out), vec!["out/sub/f.txt"]);
	assert_eq!(out.path_of(&empty, file).as_deref(), Some("out/sub/f.txt"));
}

/// An item gone takes everything under it: the server trashes a directory's subtree with it,
/// and so does the view.
#[test]
fn a_gone_directory_takes_its_subtree_with_it() {
	let dir = Uuid::from_u128(21);
	let sub = Uuid::from_u128(22);
	let keep = Uuid::from_u128(25);
	let (baseline, nodes) = derived([
		dir_node("d", dir),
		file_node("d/a.txt", Uuid::from_u128(23)),
		dir_node("d/s", sub),
		file_node("d/s/b.txt", Uuid::from_u128(24)),
		file_node("keep.txt", keep),
	]);
	let events = [cache_event(
		Some(1),
		CacheEventType::Dir(DirEvent::Removed(dir)),
	)];

	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&events),
		&[keep, dir, sub],
		&mut no_ancestry(),
	));

	assert_eq!(paths(&out), vec!["keep.txt"]);
	assert_eq!(out.path_of(&baseline, keep).as_deref(), Some("keep.txt"));
	assert!(out.path_of(&baseline, dir).is_none());
	assert!(out.path_of(&baseline, sub).is_none());
	assert_eq!(
		listed(&out.touched),
		vec!["d"],
		"the root of the removed subtree is the dirty path; a walk of it covers the rest"
	);
}

/// A rename onto a name another item holds cannot be true of the server — it has no two
/// byte-identical names under one parent — so it is the cache showing a transition. The one
/// path is withheld, exactly as the whole-tree view withholds it, and the pass runs.
#[test]
fn a_rename_onto_an_occupied_path_withholds_that_path() {
	let held = Uuid::from_u128(31);
	let mover = Uuid::from_u128(32);
	let (baseline, nodes) = derived([
		file_node("a.txt", held),
		file_node("b.txt", mover),
		file_node("c.txt", Uuid::from_u128(33)),
	]);
	let events = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::MetadataChanged {
			uuid: mover,
			meta: meta("a.txt"),
		}),
	)];

	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&events),
		&[],
		&mut no_ancestry(),
	));

	assert_eq!(paths(&out), vec!["c.txt"]);
	assert_eq!(listed(&out.held_paths), vec!["a.txt"]);
	assert_eq!(listed(&out.touched), vec!["a.txt", "b.txt"]);
}

/// The server re-mints a file's uuid on every content edit, so a new uuid landing on a path a
/// file of the SAME lineage holds is a new version of it rather than a second item: it
/// replaces, and nothing is withheld.
#[test]
fn a_new_version_of_the_same_file_replaces_the_path_it_lands_on() {
	let old = Uuid::from_u128(41);
	let new = Uuid::from_u128(42);
	let lineage = StableUuid::new_for_test(old);
	let mut previous = item_file(old, root(), "a.txt");
	previous.stable_uuid = Some(lineage);
	let (baseline, nodes) = derived([placed("a.txt", &previous)]);
	let successor = CacheableFile {
		stable_uuid: lineage,
		..cacheable_file(new, root(), "a.txt")
	};
	let events = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::New(successor)),
	)];

	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&events),
		&[old],
		&mut no_ancestry(),
	));

	assert_eq!(out.nodes.whole().at("a.txt").unwrap().remote_uuid, new);
	assert!(out.path_of(&baseline, old).is_none());
	assert_eq!(out.path_of(&baseline, new).as_deref(), Some("a.txt"));
	assert!(out.held_paths.is_empty(), "{:?}", out.held_paths);
}

/// A removal that names a successor is a re-mint, not a deletion. Until something puts the
/// successor somewhere the path is withheld — showing it as absent is the fabricated deletion
/// this side must never produce.
#[test]
fn a_removal_that_names_a_successor_withholds_its_path() {
	let old = Uuid::from_u128(51);
	let new = Uuid::from_u128(52);
	let lineage = StableUuid::new_for_test(old);
	let pair = || {
		derived([
			file_node("a.txt", old),
			file_node("keep.txt", Uuid::from_u128(53)),
		])
	};
	let trashed = || {
		cache_event(
			Some(1),
			CacheEventType::File(FileEvent::Trashed {
				uuid: old,
				stable_uuid: lineage,
				new_uuid: Some(new),
			}),
		)
	};

	let (baseline, nodes) = pair();
	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&[trashed()]),
		&[],
		&mut no_ancestry(),
	));
	assert_eq!(paths(&out), vec!["keep.txt"]);
	assert_eq!(listed(&out.held_paths), vec!["a.txt"]);

	// The successor's own upsert is usually in the same delta, and then the path simply holds
	// the new version.
	let successor = CacheableFile {
		stable_uuid: lineage,
		..cacheable_file(new, root(), "a.txt")
	};
	let (baseline, nodes) = pair();
	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&[
			trashed(),
			cache_event(Some(2), CacheEventType::File(FileEvent::New(successor))),
		]),
		&[],
		&mut no_ancestry(),
	));
	assert_eq!(out.nodes.whole().at("a.txt").unwrap().remote_uuid, new);
	assert!(out.held_paths.is_empty(), "{:?}", out.held_paths);
}

/// A directory move carries the subtree the cache still lists under its old path.
#[test]
fn a_moved_directory_carries_its_subtree_to_the_new_path() {
	let top = Uuid::from_u128(61);
	let dir = Uuid::from_u128(62);
	let file = Uuid::from_u128(63);
	let (baseline, nodes) = derived([
		dir_node("top", top),
		dir_node("d", dir),
		file_node("d/a.txt", file),
	]);
	let events = [cache_event(
		Some(1),
		CacheEventType::Dir(DirEvent::Move(cacheable_dir(dir, top, "d"))),
	)];

	let out = applied(observed_with(
		&baseline,
		nodes,
		&delta(&events),
		&[file],
		&mut no_ancestry(),
	));

	assert_eq!(paths(&out), vec!["top", "top/d", "top/d/a.txt"]);
	assert_eq!(out.path_of(&baseline, file).as_deref(), Some("top/d/a.txt"));
	assert_eq!(
		out.nodes.whole().at("top/d/a.txt").unwrap().rel_path,
		"top/d/a.txt"
	);
	assert_eq!(listed(&out.touched), vec!["d", "top/d"]);
}

/// A directory moved or trashed with more rows under it than a change-scoped pass expands one
/// change into is a whole read: the view would otherwise hold a node or a tombstone at every one
/// of those paths. At the cap it is still derived, path by path; one row past it, it is not.
#[test]
fn a_directory_changed_with_more_rows_under_it_than_the_cap_is_a_whole_read() {
	let top = Uuid::from_u128(1_001);
	let dir = Uuid::from_u128(1_002);
	let tree = |files: usize| {
		derived(
			[dir_node("top", top), dir_node("d", dir)]
				.into_iter()
				.chain((0..files).map(|n| {
					file_node(
						&format!("d/{n:05}.txt"),
						Uuid::from_u128(10_000 + n as u128),
					)
				})),
		)
	};
	let moved = || {
		cache_event(
			Some(1),
			CacheEventType::Dir(DirEvent::Move(cacheable_dir(dir, top, "d"))),
		)
	};
	let trashed = || cache_event(Some(1), CacheEventType::Dir(DirEvent::Removed(dir)));
	// The directory and every file, at both ends of the move; the directory and every file,
	// where it was trashed.
	for (label, event, changed_at_cap) in [
		("moved", moved(), 2 * SUBTREE_CAP + 2),
		("trashed", trashed(), SUBTREE_CAP + 1),
	] {
		let (baseline, nodes) = tree(SUBTREE_CAP);
		let events = [event];
		let out = applied(observed_with(
			&baseline,
			nodes,
			&delta(&events),
			&[],
			&mut no_ancestry(),
		));
		assert_eq!(out.changed.len(), changed_at_cap, "{label}");

		let (baseline, nodes) = tree(SUBTREE_CAP + 1);
		match observed_with(&baseline, nodes, &delta(&events), &[], &mut no_ancestry()) {
			RemoteObserved::Full(reason) => {
				assert_eq!(reason, FullPassReason::RemoteOverflow, "{label}");
			}
			RemoteObserved::Applied(out) => panic!(
				"{label}: a subtree past the cap was derived path by path ({} path(s) changed)",
				out.changed.len()
			),
		}
	}
}

/// A directory that cannot be placed is refused for the subtree it carries, and that subtree is
/// all that is listed to find out: a carried side answers it off the rows under the directory,
/// where listing every path the side holds builds a node for every row of the pair.
#[test]
fn an_unplaceable_directory_lists_only_its_own_subtree() {
	const ROWS: u128 = 50_000;
	let dir = Uuid::from_u128(2_001);
	let taken = Uuid::from_u128(2_002);
	let synced_dir = |path: &str, uuid: Uuid| row(path, NodeKind::Dir, Some(uuid));
	let synced_file = BaselineEntry {
		content_hash: Some(Blake3Hash::from([7; 32])),
		size: Some(7),
		local_mtime: Some(1_234),
		remote_modified: Some(1_234),
		..row("taken", NodeKind::File, Some(taken))
	};
	let baseline = Baseline::from_rows(
		[
			synced_dir("d", dir),
			synced_dir("d/s", Uuid::from_u128(2_003)),
			synced_file,
		]
		.into_iter()
		.chain((0..ROWS).map(|n| synced_dir(&format!("w/{n:05}"), Uuid::from_u128(100_000 + n)))),
	);
	let entries = vec![RemoteDeltaEntry {
		id: Some(1),
		change: RemoteChange::Renamed {
			uuid: dir,
			name: "taken".to_owned(),
			content: None,
		},
	}];

	let (observed, peak) = alloc_meter::peak_bytes(|| {
		observed_with(
			&baseline,
			Side::carried(),
			&entries,
			&[],
			&mut no_ancestry(),
		)
	});
	match observed {
		RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteUnplaceable),
		RemoteObserved::Applied(out) => {
			panic!("a subtree was dropped instead: {:?}", out.nodes)
		}
	}
	assert!(
		peak < 1 << 20,
		"refusing one directory of one subdirectory peaked at {peak} bytes over a {ROWS}-row pair"
	);
}

/// A rule file's CONTENTS decide what is hidden for its whole directory, so a change to one
/// re-derives that directory — the remote twin of `observe_local` mapping a dirty rule file to
/// its directory. The producer collapses such an event to a full pass today
/// (`FullPassReason::RulesChanged`), so the entry is built here as a relaxed one would record
/// it.
#[test]
fn a_changed_rule_file_re_derives_its_directory() {
	let dir = Uuid::from_u128(71);
	let rule = Uuid::from_u128(72);
	let (baseline, nodes) = derived([dir_node("d", dir)]);
	let entries = vec![RemoteDeltaEntry {
		id: Some(1),
		change: RemoteChange::Upsert(item_file(rule, dir, FILENIGNORE)),
	}];

	let out = applied(observed_with(
		&baseline,
		nodes,
		&entries,
		&[],
		&mut no_ancestry(),
	));

	assert_eq!(listed(&out.rule_dirs), vec!["d"]);
	assert!(out.nodes.whole().holds(&format!("d/{FILENIGNORE}")));
}

/// A directory the derivation cannot place — the name it was renamed to is taken — cannot
/// simply leave the view: its children moved with it, to paths nothing announced names, and
/// dropping them would read as a tree of deletions.
#[test]
fn a_directory_that_cannot_be_placed_asks_for_a_full_pass() {
	let dir = Uuid::from_u128(101);
	let (baseline, nodes) = derived([
		dir_node("d", dir),
		file_node("d/a.txt", Uuid::from_u128(102)),
		file_node("taken", Uuid::from_u128(103)),
	]);
	let entries = vec![RemoteDeltaEntry {
		id: Some(1),
		change: RemoteChange::Renamed {
			uuid: dir,
			name: "taken".to_owned(),
			content: None,
		},
	}];

	match observed_with(&baseline, nodes, &entries, &[], &mut no_ancestry()) {
		RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteUnplaceable),
		RemoteObserved::Applied(out) => {
			panic!("a subtree was dropped instead: {:?}", out.nodes)
		}
	}
}

/// A delta that empties the view is not applied: an emptied remote is the shape a backend fault
/// takes, and the guard weighs it against a whole-tree read, never against a derivation.
#[test]
fn a_delta_that_empties_the_view_asks_for_a_full_pass() {
	let file = Uuid::from_u128(81);
	let baseline = Baseline::from_rows([row("a.txt", NodeKind::File, Some(file))]);
	let nodes = view([file_node("a.txt", file)]);
	let events = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::Removed(file)),
	)];

	match observed_with(&baseline, nodes, &delta(&events), &[], &mut no_ancestry()) {
		RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteEmptied),
		RemoteObserved::Applied(out) => panic!("{:?}", out.nodes),
	}
}

/// An empty ancestry means the cache does not know the item — UNKNOWN, never "at the root".
/// Deriving a path from it would be a guess, and a guess on this side ends in a deletion.
#[test]
fn an_item_the_cache_cannot_place_asks_for_a_full_pass() {
	let orphan = Uuid::from_u128(91);
	let stranger = Uuid::from_u128(92);
	let events = [cache_event(
		Some(1),
		CacheEventType::File(FileEvent::New(cacheable_file(orphan, stranger, "f.txt"))),
	)];
	let mut ancestry = |_: Uuid| Ok(Vec::new());

	match observed_with(
		&Baseline::default(),
		Side::default(),
		&delta(&events),
		&[],
		&mut ancestry,
	) {
		RemoteObserved::Full(reason) => assert_eq!(reason, FullPassReason::RemoteUnplaceable),
		RemoteObserved::Applied(out) => panic!("{:?}", out.nodes),
	}
}
