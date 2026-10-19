use filen_types::crypto::Blake3Hash;
use uuid::Uuid;

use super::{
	super::{
		baseline::BaselineChange,
		plan::is_under,
		scan::{at_or_under_folded, collision_key},
	},
	*,
};

/// The index's answer and the row's must be the same answer, for every row the tree holds.
///
/// The only place the two halves of the contract meet. [`Tree::carryable`] reads the
/// `uncarryable` set, recorded from the entry a caller handed [`Tree::upsert`], while
/// `derive::carried` asks [`BaselineEntry::carryable`] of the row [`Tree::fill_row`]
/// rebuilds out of the node's flag bits. They agree only while the store round-trips every
/// field the rule reads: narrow one column or drop one flag and the index would call a row
/// carryable that derives nothing — a path in neither map and in neither `dirty` nor `held`,
/// which is a pass planning over an agreement nobody recorded.
fn index_agrees_with_every_row(tree: &Tree) {
	let mut rows = Vec::new();
	tree.visit_rows(|row| rows.push(row.clone()));
	for row in rows {
		assert_eq!(
			tree.carryable(&row.rel_path),
			row.carryable(),
			"{}: the index and the row the store rebuilds disagree",
			row.rel_path
		);
	}
}

/// Every term of [`ResidentTerms`] is in the total, and the total is what
/// [`Tree::resident_bytes`] answers.
///
/// The struct is destructured EXHAUSTIVELY here on purpose: a term added to it and left out of
/// [`ResidentTerms::named`] would otherwise be missing from every total silently, and a
/// resident figure that quietly stopped counting a structure is exactly the kind of number this
/// round exists to stop anyone quoting. A new field fails this at COMPILE time, before it can
/// fail an assertion.
#[cfg(feature = "bench-internals")]
#[test]
fn every_resident_term_is_in_the_total() {
	let tree = Tree::from_rows([
		file("docs/a.txt", Uuid::from_u128(1), [1; 32]),
		file("docs/b.txt", Uuid::from_u128(2), [2; 32]),
		dir("docs"),
	]);
	let terms = tree.resident_terms();
	let ResidentTerms {
		nodes,
		names,
		free,
		children_vecs,
		children_table,
		side,
		agreed,
		uncarryable,
		rule_files,
	} = terms;
	let by_hand = nodes
		+ names
		+ free + children_vecs
		+ children_table
		+ side + agreed
		+ uncarryable
		+ rule_files;
	assert_eq!(
		by_hand,
		terms.total(),
		"a term of the resident tree is not in the total: {:?}",
		terms
	);
	assert_eq!(
		terms.total(),
		tree.resident_bytes(),
		"the split and the sum it is set against disagree"
	);
	// The terms a loaded tree must charge something for, so the split cannot be all zeroes and
	// still pass. `side` and `agreed` are deliberately absent: a converged pair has neither a
	// conflict nor an unconfirmed push, which is the steady state being accounted for.
	for (what, bytes) in [
		("nodes", nodes),
		("names", names),
		("children_vecs", children_vecs),
		("children_table", children_table),
	] {
		assert!(
			bytes > 0,
			"{what} costs nothing on a tree holding three rows"
		);
	}
}

/// The uncarryable index is what a change-scoped pass reads INSTEAD of walking the tree, so it
/// has to follow every write path: a row rewritten one-sided joins it, one rewritten whole
/// leaves it, a moved subtree takes its entries along, and a removed row stops answering for a
/// path that no longer has one. An index that fell behind on any of those would leave a pass
/// deriving both sides from a row that describes neither.
#[test]
fn the_uncarryable_index_follows_every_write_path() {
	let uuid = Uuid::from_u128(9);
	let whole = file("docs/a.txt", uuid, [1; 32]);
	let one_sided = BaselineEntry {
		state: BaselineState::Conflicted,
		..whole.clone()
	};

	let mut tree = Tree::from_rows([whole.clone()]);
	assert_eq!(tree.carryable_rows(), 1);
	assert!(tree.carryable("docs/a.txt"));
	assert_eq!(tree.uncarryable_paths().count(), 0);
	assert!(
		!tree.carryable("docs"),
		"a path with no row of its own carries nothing"
	);
	assert!(!tree.carryable("docs/missing.txt"));
	index_agrees_with_every_row(&tree);

	// Rewritten one-sided: indexed, and carryable no longer.
	tree.upsert(&one_sided);
	assert_eq!(tree.carryable_rows(), 0);
	assert!(!tree.carryable("docs/a.txt"));
	assert_eq!(
		tree.uncarryable_paths().collect::<Vec<_>>(),
		vec!["docs/a.txt".to_string()]
	);
	index_agrees_with_every_row(&tree);

	// A move re-keys it with the row. Without this a pass would re-observe a path nothing is
	// keyed by any more, and carry the row that moved.
	tree.move_subtree("docs", "moved");
	assert_eq!(
		tree.uncarryable_paths().collect::<Vec<_>>(),
		vec!["moved/a.txt".to_string()]
	);
	assert!(!tree.carryable("moved/a.txt"));
	index_agrees_with_every_row(&tree);

	// Rewritten whole: out of the index again.
	tree.upsert(&BaselineEntry {
		rel_path: "moved/a.txt".to_string(),
		..whole.clone()
	});
	assert_eq!(tree.carryable_rows(), 1);
	assert_eq!(tree.uncarryable_paths().count(), 0);
	assert!(tree.carryable("moved/a.txt"));
	index_agrees_with_every_row(&tree);

	// A removed row leaves the index rather than answering for a path with no row.
	tree.upsert(&BaselineEntry {
		rel_path: "moved/b.txt".to_string(),
		state: BaselineState::Adopted,
		..whole.clone()
	});
	assert_eq!(
		tree.uncarryable_paths().collect::<Vec<_>>(),
		vec!["moved/b.txt".to_string()]
	);
	assert_eq!(tree.carryable_rows(), 1);
	tree.remove("moved/b.txt");
	assert_eq!(tree.uncarryable_paths().count(), 0);
	assert_eq!(tree.carryable_rows(), 1);
	index_agrees_with_every_row(&tree);

	// And a subtree removal takes its entries with it.
	tree.upsert(&BaselineEntry {
		rel_path: "moved/deep/c.txt".to_string(),
		state: BaselineState::Adopted,
		..whole.clone()
	});
	assert_eq!(tree.uncarryable_paths().count(), 1);
	tree.remove_subtrees(&BTreeSet::from(["moved/deep".to_string()]));
	assert_eq!(tree.uncarryable_paths().count(), 0);
	assert_eq!(tree.carryable_rows(), 1);
	index_agrees_with_every_row(&tree);
}

/// The folded subtree walk answers the question `occupied` asks, with the caller saying which
/// rows count — so a carried side can ask it about rows the pass has observed away without the
/// walk knowing anything about observations.
#[test]
fn the_folded_subtree_walk_lets_the_caller_choose_which_rows_count() {
	let tree = Tree::from_rows([
		file("Docs/a.txt", Uuid::from_u128(1), [1; 32]),
		file("notes", Uuid::from_u128(2), [2; 32]),
	]);
	let mut anything = |_: &str| true;

	assert!(
		tree.any_folded_row_at_or_under("docs", &mut anything),
		"a directory no row is keyed by is still occupied by what sits under it, folded"
	);
	assert!(tree.any_folded_row_at_or_under("DOCS/A.TXT", &mut anything));
	assert!(tree.any_folded_row_at_or_under("notes", &mut anything));
	assert!(
		!tree.any_folded_row_at_or_under("note", &mut anything),
		"a path a key merely starts with is not occupied"
	);
	assert!(!tree.any_folded_row_at_or_under("docs2", &mut anything));
	assert!(
		!tree.any_folded_row_at_or_under("", &mut anything),
		"the pair root holds nothing, as `occupied` answers for it"
	);

	// The predicate is the point: refusing the one row under `docs` empties it.
	let mut nothing = |_: &str| false;
	assert!(!tree.any_folded_row_at_or_under("docs", &mut nothing));
	let mut only_notes = |path: &str| path == "notes";
	assert!(!tree.any_folded_row_at_or_under("docs", &mut only_notes));
	assert!(tree.any_folded_row_at_or_under("NOTES", &mut only_notes));
}

fn file(rel_path: &str, uuid: Uuid, hash: [u8; 32]) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid),
		content_hash: Some(Blake3Hash::from(hash)),
		size: Some(7),
		local_mtime: Some(12),
		remote_modified: Some(34),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: Some(Blake3Hash::from(hash)),
	}
}

fn dir(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		kind: NodeKind::Dir,
		content_hash: None,
		size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
		..file(rel_path, Uuid::new_v4(), [0; 32])
	}
}

fn paths(baseline: &Tree) -> Vec<String> {
	let mut all: Vec<String> = baseline.paths().collect();
	all.sort();
	all
}

/// The comparator decides which names are the SAME name for the engine, so it has to agree with
/// [`collision_key`] exactly — including the non-ASCII cases that key exists for, which
/// globset's own case-insensitive matching cannot fold.
#[test]
fn the_folded_comparator_agrees_with_the_collision_key() {
	let pairs = [
		("report.txt", "Report.TXT"),
		("Ärger.txt", "ärger.txt"),
		("ÅNGSTRÖM", "ångström"),
		("ΣΊΣΥΦΟΣ", "σίσυφοσ"),
		("ЖУРНАЛ", "журнал"),
		("İ", "i\u{307}"),
		("a", "b"),
		("ä", "b"),
		("z", "ä"),
	];
	for (a, b) in pairs {
		assert_eq!(
			fold_cmp(a, b) == Ordering::Equal,
			collision_key(a) == collision_key(b),
			"{a:?} vs {b:?}"
		);
		assert_eq!(
			fold_cmp(a, b),
			collision_key(a).cmp(&collision_key(b)),
			"{a:?} vs {b:?} order"
		);
	}
}

/// The streaming at-or-under test answers what folding both paths and asking [`is_under`]
/// answers — the form it replaces — including where a lowercase expansion makes the folded
/// path longer than the raw one, which is the case a byte-wise prefix test gets wrong.
#[test]
fn the_folded_at_or_under_test_agrees_with_folding_both_paths() {
	let cases = [
		("Docs", "docs"),
		("docs", "Docs"),
		("docs/a.txt", "Docs"),
		("Docs/sub/a.txt", "docs/SUB"),
		("docsx", "docs"),
		("docs", "docs/a"),
		("docs2/a", "docs"),
		("ÄÖ/x", "äö"),
		("ÄÖx", "äö"),
		("İ/x", "İ"),
		("İ/x", "i"),
		("İ", "i"),
		("ΣΊΣΥΦΟΣ/x", "σίσυφοσ"),
		("a", ""),
		("", ""),
	];
	for (path, prefix) in cases {
		let (folded_path, folded_prefix) = (collision_key(path), collision_key(prefix));
		assert_eq!(
			at_or_under_folded(path, prefix),
			folded_path == folded_prefix || is_under(&folded_path, &folded_prefix),
			"{path:?} at or under {prefix:?}"
		);
	}
}

/// A node is 104 bytes, and which 104 is the whole design of this module.
///
/// Pinned because the cost is per ROW: eight bytes here is eight megabytes on a million-row
/// pair, and a field added without a thought for the width would spend that much without
/// anything saying so. A failure here is not a bug — it is this test asking which is worth
/// more, the new field or the megabytes, and asking it while the field is still easy to move.
///
/// The arithmetic, for whoever it asks: parent 4, the name's offset and length 4 + 4, kind,
/// state and flags 1 each, `remote_uuid` 16, `stable_uuid` 17 (`Uuid` aligns to 1, so an
/// `Option` of one costs a single byte more), `content_hash` 32, and three 8-byte integers.
#[test]
fn a_node_is_104_bytes() {
	assert_eq!(
		size_of::<Node>(),
		104,
		"the per-row width of the resident tree moved"
	);
}

/// The name arena is sized by the TREE, never by the pair's history.
///
/// A rename is a remove and an insert, so every renamed name is appended and the one it
/// replaced is left behind. Without compaction a pair that renames all day grows one structure
/// for as long as it lives, which is not a leak the way Rust counts leaks and is unbounded
/// memory all the same.
///
/// The second half is what makes the first half safe: compaction rewrites the offset of every
/// live node, and one left behind would have a node answering to some other node's name — a
/// silently wrong path, which is the worst thing this tree can produce. So the survivors are
/// read back BY PATH after the arena has been rebuilt under them.
#[test]
fn renaming_forever_does_not_grow_the_name_arena_forever() {
	let keepers: Vec<String> = (0..50).map(|i| format!("keep/k{i}.txt")).collect();
	let mut tree = Tree::from_rows(
		std::iter::once(dir("keep")).chain(
			keepers
				.iter()
				.enumerate()
				.map(|(i, path)| file(path, Uuid::from_u128(i as u128 + 1), [1; 32])),
		),
	);
	let live = tree.names.len();

	// Long names so the floor is reached in a test-sized number of rounds; the churn is a
	// create and a delete, which is what a rename is to the arena.
	let long = "n".repeat(1024);
	let mut widest = 0;
	for round in 0..400u128 {
		let path = format!("keep/{long}{round}.txt");
		tree.upsert(&file(&path, Uuid::from_u128(round + 1000), [2; 32]));
		tree.remove(&path);
		widest = widest.max(tree.names.len());
	}

	assert!(
		widest < 200 * 1024,
		"the arena followed the pair's history rather than its tree: {widest} bytes at its \
			 widest, against {live} of live names"
	);
	assert!(
		tree.dead_names <= tree.names.len(),
		"more of the arena is written off than was written"
	);
	let mut expected: Vec<String> = keepers.clone();
	expected.push("keep".to_owned());
	expected.sort();
	assert_eq!(
		paths(&tree),
		expected,
		"the churn left the tree it started with"
	);
	for path in &keepers {
		assert_eq!(
			tree.get(path).map(|row| row.rel_path).as_deref(),
			Some(path.as_str()),
			"{path:?} does not resolve to itself after the arena was rebuilt under it"
		);
	}
}

/// A row goes in and comes back out whole: every field, including the halves kept beside the
/// nodes (the conflict sides and an agreed marker that differs from this side's content).
#[test]
fn a_row_round_trips_with_full_fidelity() {
	let uuid = Uuid::new_v4();
	let held = BaselineEntry {
		state: BaselineState::Conflicted,
		local_kind: Some(NodeKind::File),
		remote_kind: Some(NodeKind::Dir),
		remote_hash: Some(Blake3Hash::from([9; 32])),
		remote_size: Some(42),
		agreed_hash: Some(Blake3Hash::from([3; 32])),
		..file("a/b/c.txt", uuid, [1; 32])
	};
	let unconfirmed = BaselineEntry {
		agreed_hash: None,
		..file("a/pushed.bin", Uuid::new_v4(), [2; 32])
	};
	let plain = dir("a");
	let baseline = Tree::from_rows([held.clone(), unconfirmed.clone(), plain.clone()]);

	assert_eq!(baseline.get("a/b/c.txt"), Some(held));
	assert_eq!(baseline.get("a/pushed.bin"), Some(unconfirmed));
	assert_eq!(baseline.get("a"), Some(plain));
	assert_eq!(baseline.get("a/b"), None, "a path with no row of its own");
	assert_eq!(baseline.len(), 3, "the path-only node is not a row");
	assert_eq!(paths(&baseline), vec!["a", "a/b/c.txt", "a/pushed.bin"]);
}

/// A lookup names the row that holds the id NOW: a row rewritten with another uuid, a row
/// deleted, and a row moved all have to leave the answer describing where things are.
///
/// The tree no longer indexes either id — the store does — so this asks the way a pass asks,
/// through [`Tree::synced_paths`], which is the walking twin of
/// [`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths).
#[test]
fn the_uuid_and_lineage_lookups_follow_the_rows() {
	let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
	let held_by_the_dir = Uuid::new_v4();
	let mut baseline = Tree::from_rows([
		BaselineEntry {
			remote_uuid: Some(held_by_the_dir),
			..dir("d")
		},
		file("d/x.txt", first, [1; 32]),
	]);
	let lineage = StableUuid::new_for_test(first);
	let at = |tree: &Tree, uuid: Uuid| tree.synced_paths(&[uuid], &[]).path_by_uuid(uuid);
	assert_eq!(at(&baseline, first).as_deref(), Some("d/x.txt"));
	assert_eq!(
		baseline
			.synced_paths(&[], &[lineage])
			.path_by_lineage(lineage)
			.as_deref(),
		Some("d/x.txt")
	);

	// A new version at the same path: the old uuid names nothing any more.
	baseline.upsert(&file("d/x.txt", second, [2; 32]));
	assert_eq!(at(&baseline, first), None);
	assert_eq!(at(&baseline, second).as_deref(), Some("d/x.txt"));

	baseline.move_subtree("d", "e");
	assert_eq!(at(&baseline, second).as_deref(), Some("e/x.txt"));

	baseline.remove("e/x.txt");
	assert_eq!(at(&baseline, second), None);
	// The row that is still there still answers, so the lookup did not simply empty itself: the
	// directory's own claim was re-keyed by the move above.
	assert!(baseline.has_remote_rows());
	assert_eq!(at(&baseline, held_by_the_dir).as_deref(), Some("e"));
}

/// `occupied` is the server's own name dedup asked of the baseline: any spelling, at the path or
/// under it. `""` holds nothing, exactly as the path-keyed form it replaces answered.
#[test]
fn occupied_finds_any_spelling_at_or_under_a_path() {
	let baseline = Tree::from_rows([
		dir("Photos"),
		file("Photos/Ärger.txt", Uuid::new_v4(), [1; 32]),
		dir("empty_parent/inner"),
	]);

	assert!(baseline.occupied("photos"));
	assert!(baseline.occupied("PHOTOS/ärger.txt"));
	assert!(
		baseline.occupied("EMPTY_PARENT"),
		"a path-only node with a row under it"
	);
	assert!(!baseline.occupied("photos/other.txt"));
	assert!(!baseline.occupied("pho"));
	assert!(
		!baseline.occupied(""),
		"nothing is at or under the pair root itself"
	);
}

/// The ignore probe is EXACT-spelling, unlike `occupied`: it asks whether the rows a rule is
/// about to untrack exist, and those are keyed the way the store keys them.
#[test]
fn tracked_asks_about_the_exact_path_and_a_directorys_subtree() {
	let baseline = Tree::from_rows([dir("a"), file("a/b/c.txt", Uuid::new_v4(), [1; 32])]);

	assert!(baseline.tracked("a", false));
	assert!(baseline.tracked("a/b", true), "a row under it");
	assert!(!baseline.tracked("a/b", false), "no row AT it");
	assert!(!baseline.tracked("A", true), "the row is spelled otherwise");
	assert!(!baseline.tracked("a/b/c.txt/deeper", true));
}

/// Two siblings the server would treat as one name can both be in the store (its key compares
/// bytewise), so both have to stay addressable by their exact spelling.
#[test]
fn two_siblings_that_fold_together_are_both_addressable() {
	let (upper, lower) = (Uuid::new_v4(), Uuid::new_v4());
	let baseline = Tree::from_rows([file("A.txt", upper, [1; 32]), file("a.txt", lower, [2; 32])]);

	assert_eq!(baseline.len(), 2);
	assert_eq!(baseline.get("A.txt").unwrap().remote_uuid, Some(upper));
	assert_eq!(baseline.get("a.txt").unwrap().remote_uuid, Some(lower));
	assert!(baseline.occupied("A.TXT"));
}

/// A path with no row of its own stands only while something is under it — and a row taken off
/// a path that still holds children keeps the path reachable.
#[test]
fn a_path_only_node_lives_exactly_as_long_as_its_subtree() {
	let mut baseline = Tree::from_rows([dir("a"), file("a/b/c.txt", Uuid::new_v4(), [1; 32])]);

	baseline.remove("a");
	assert_eq!(baseline.len(), 1);
	assert_eq!(baseline.get("a"), None);
	assert!(
		baseline.tracked("a", true),
		"the row under it is still there"
	);

	baseline.remove("a/b/c.txt");
	assert!(baseline.is_empty());
	assert!(!baseline.tracked("a", true));
	assert!(!baseline.occupied("a"));
	assert!(paths(&baseline).is_empty());
}

/// A directory move re-keys the row at the source and every row under it, overwrites what it
/// lands on, and leaves the rest of the destination's subtree alone — what the store's two
/// `UPDATE OR REPLACE` statements do. A case-only rename is one of these.
#[test]
fn a_subtree_move_re_keys_its_rows_and_overwrites_only_what_it_lands_on() {
	let moved = Uuid::new_v4();
	let mut baseline = Tree::from_rows([
		dir("a"),
		file("a/x.txt", moved, [1; 32]),
		dir("b"),
		file("b/x.txt", Uuid::new_v4(), [2; 32]),
		file("b/kept.txt", Uuid::new_v4(), [3; 32]),
		file("ab.txt", Uuid::new_v4(), [4; 32]),
	]);

	baseline.move_subtree("a", "b");
	assert_eq!(
		paths(&baseline),
		vec!["ab.txt", "b", "b/kept.txt", "b/x.txt"],
		"the sibling sharing the prefix is untouched and b/kept.txt survives"
	);
	assert_eq!(baseline.get("b/x.txt").unwrap().remote_uuid, Some(moved));

	baseline.move_subtree("b", "B");
	assert_eq!(
		paths(&baseline),
		vec!["B", "B/kept.txt", "B/x.txt", "ab.txt"]
	);
	assert_eq!(baseline.get("B/x.txt").unwrap().remote_uuid, Some(moved));
}

/// Untracking an ignored root drops the row at it and every row under it, and nothing else.
#[test]
fn removing_a_subtree_drops_the_root_and_its_rows_only() {
	let (dropped, kept) = (Uuid::new_v4(), Uuid::new_v4());
	let mut baseline = Tree::from_rows([
		dir("build"),
		file("build/out.bin", dropped, [1; 32]),
		file("builder.txt", kept, [2; 32]),
	]);

	baseline.remove_subtrees(&BTreeSet::from(["build".to_string()]));
	assert_eq!(paths(&baseline), vec!["builder.txt"]);
	assert_eq!(baseline.len(), 1);
	// The dropped rows leave the lookup with the subtree; the sibling's claim stays.
	assert_eq!(
		baseline.synced_paths(&[dropped], &[]).path_by_uuid(dropped),
		None
	);
	assert_eq!(
		baseline
			.synced_paths(&[kept], &[])
			.path_by_uuid(kept)
			.as_deref(),
		Some("builder.txt")
	);
	assert!(baseline.has_remote_rows());
}

/// The two shapes the tree cannot represent are refused rather than half-applied: the pair
/// root's slot is not one to hand back (freeing it would make the root its own child), and an
/// index answers for one row, so a second claim on an id displaces the first.
#[test]
fn the_pair_root_and_a_second_claim_on_one_uuid_are_refused() {
	let shared = Uuid::new_v4();
	let mut baseline = Tree::from_rows([dir("d"), file("d/x.txt", shared, [1; 32])]);

	baseline.remove_subtrees(&BTreeSet::from([String::new()]));
	baseline.move_subtree("", "e");
	baseline.move_subtree("d", "");
	assert_eq!(
		paths(&baseline),
		vec!["d", "d/x.txt"],
		"the tree is untouched"
	);

	// The root's slot never reached the free list, so the next row written is a child OF the
	// root and not of itself — which a walk of the tree would otherwise never leave.
	baseline.upsert(&file("after.txt", Uuid::new_v4(), [3; 32]));
	assert_eq!(paths(&baseline), vec!["after.txt", "d", "d/x.txt"]);

	// A second row claiming a uuid the tree already records. The resident map this replaced was
	// last-writer-wins and answered `d/twin.txt`; a seek finds BOTH claimants and answers with
	// the first by path, which is the deliberate difference — deterministic, and never `None`
	// for an id some row still carries.
	baseline.upsert(&file("d/twin.txt", shared, [2; 32]));
	assert_eq!(
		baseline
			.synced_paths(&[shared], &[])
			.path_by_uuid(shared)
			.as_deref(),
		Some("d/twin.txt"),
		"of two rows claiming one uuid the first by path answers"
	);
}

/// The unconfirmed index holds exactly the rows a push left behind — a file, synced, with
/// content on record that is not what the two sides last agreed on — and drops each as its
/// marker advances.
#[test]
fn unconfirmed_holds_the_rows_awaiting_confirmation_only() {
	let pushed = Uuid::new_v4();
	let mut baseline = Tree::from_rows([
		BaselineEntry {
			agreed_hash: None,
			..file("pushed.txt", pushed, [1; 32])
		},
		file("agreed.txt", Uuid::new_v4(), [2; 32]),
		BaselineEntry {
			agreed_hash: None,
			state: BaselineState::Conflicted,
			..file("held.txt", Uuid::new_v4(), [3; 32])
		},
		BaselineEntry {
			agreed_hash: None,
			..dir("d")
		},
	]);

	assert!(baseline.any_unconfirmed());
	assert_eq!(
		baseline
			.unconfirmed()
			.map(|entry| entry.rel_path)
			.collect::<Vec<_>>(),
		vec!["pushed.txt".to_string()]
	);

	let advanced = baseline
		.set_agreed("pushed.txt", Some(Blake3Hash::from([1; 32])))
		.expect("the row is there");
	assert_eq!(advanced.agreed_hash, advanced.content_hash);
	assert!(!baseline.any_unconfirmed());
	assert_eq!(baseline.unconfirmed().count(), 0);
}

/// The gate a directory move takes before it carries a subtree across as one move: a row under
/// it that is not `Synced` is a divergence the per-path plan has to see.
#[test]
fn subtree_all_synced_sees_every_row_under_a_directory() {
	let mut baseline = Tree::from_rows([
		dir("a"),
		file("a/x.txt", Uuid::new_v4(), [1; 32]),
		file("a/deep/y.txt", Uuid::new_v4(), [2; 32]),
	]);
	assert!(baseline.subtree_all_synced("a"));
	assert!(
		baseline.subtree_all_synced("missing"),
		"nothing is not a divergence"
	);

	baseline.upsert(&BaselineEntry {
		state: BaselineState::Conflicted,
		..file("a/deep/y.txt", Uuid::new_v4(), [2; 32])
	});
	assert!(!baseline.subtree_all_synced("a"));
}

/// The store hands its write paths to [`Tree::apply`] as changes; each one has to land the
/// way the statement it mirrors lands.
#[test]
fn applying_a_change_list_mirrors_each_statement() {
	let mut baseline = Tree::from_rows([dir("a"), file("a/x.txt", Uuid::new_v4(), [1; 32])]);
	let created = file("b/new.txt", Uuid::new_v4(), [5; 32]);

	baseline.apply(&[
		BaselineChange::Upsert(&created),
		BaselineChange::Delete("a/x.txt"),
		BaselineChange::MoveSubtree { from: "a", to: "c" },
	]);

	assert_eq!(paths(&baseline), vec!["b/new.txt", "c"]);
	assert_eq!(baseline.len(), 2);
}

/// The cursor is a memo over [`Tree::get`] and has to answer identically — for a path whose
/// directory it has never seen, for one that is no row, for a sibling that folds onto another
/// spelling, and after a miss, which must leave the directory it holds consistent.
#[test]
fn the_cursor_answers_exactly_what_a_fresh_lookup_answers() {
	let baseline = Tree::from_rows([
		dir("a"),
		file("a/x.txt", Uuid::new_v4(), [1; 32]),
		file("a/X.txt", Uuid::new_v4(), [2; 32]),
		file("a/deep/y.txt", Uuid::new_v4(), [3; 32]),
		file("b.txt", Uuid::new_v4(), [4; 32]),
	]);

	let mut cursor = baseline.cursor();
	for path in [
		"a",
		"a/x.txt",
		"a/X.txt",
		"a/deep",
		"a/deep/y.txt",
		"b.txt",
		"missing",
		"missing/deeper.txt",
		"a/gone.txt",
		"a/x.txt",
	] {
		assert_eq!(cursor.get(path), baseline.get(path), "{path}");
	}
}

/// The borrowing walk is what a pass reads the tree with, so it has to hand out exactly the
/// paths the owning one does — in the same order, over a tree that holds a path-only node, two
/// siblings that fold together and nesting on both sides of them — and it has to do it again on a
/// second walk, since it reuses one buffer.
#[test]
fn visiting_the_paths_hands_out_what_collecting_them_does() {
	let baseline = Tree::from_rows([
		dir("a"),
		file("a/x.txt", Uuid::new_v4(), [1; 32]),
		file("a/X.txt", Uuid::new_v4(), [2; 32]),
		// Under `a/deep`, which is a path with no row of its own.
		file("a/deep/y.txt", Uuid::new_v4(), [3; 32]),
		file("a/deep/nested/z.txt", Uuid::new_v4(), [4; 32]),
		file("b.txt", Uuid::new_v4(), [5; 32]),
	]);
	let owned: Vec<String> = baseline.paths().collect();

	let mut visited = Vec::new();
	baseline.visit_row_paths(|path| visited.push(path.to_string()));
	assert_eq!(visited, owned);

	let mut again = Vec::new();
	baseline.visit_row_paths(|path| again.push(path.to_string()));
	assert_eq!(
		again, owned,
		"the reused buffer must not leak between walks"
	);

	let empty = Tree::default();
	let mut none = Vec::new();
	empty.visit_row_paths(|path| none.push(path.to_string()));
	assert!(none.is_empty(), "a pair with no rows visits nothing");
}

/// A row cannot be written for the pair root itself: the store's key includes a path, and a
/// nameless row would be a row nothing can address.
#[test]
fn a_row_with_no_path_is_refused() {
	let mut baseline = Tree::default();
	baseline.upsert(&file("", Uuid::new_v4(), [1; 32]));
	assert!(baseline.is_empty());
}
