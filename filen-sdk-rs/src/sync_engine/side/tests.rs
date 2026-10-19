use super::{
	test_support::{copied, sorted_entries},
	*,
};

/// A marker node is never derived from a row — it is not either side of a pass — so a carried
/// side of one holds exactly its overlay, which is what the overlay tests below want.
impl FromRow for u32 {
	fn from_row(_: &BaselineEntry) -> Option<Self> {
		None
	}
}

impl FromRow for String {
	fn from_row(_: &BaselineEntry) -> Option<Self> {
		None
	}
}

/// The owning shape, over a materialized map: what it holds is what the traits answer for.
#[test]
fn a_side_is_the_map_it_holds() {
	let rows = Baseline::default();
	let mut side: Side<u32> = Side::default();
	assert!(Nodes::is_empty(&side.whole()));
	side.insert("docs".to_owned(), 0);
	side.extend([("docs/a.txt".to_owned(), 1), ("docsx".to_owned(), 2)]);

	assert_eq!(Nodes::len(&side.whole()), 3);
	assert_eq!(side.whole().at("docs/a.txt").as_deref(), Some(&1));
	assert!(side.whole().holds("docs"));
	assert!(side.whole().occupied("DOCS/A.TXT"));
	assert_eq!(
		side.subtree_paths(&rows, "docs"),
		vec!["docs/a.txt".to_owned()]
	);
	assert_eq!(side.remove(&rows, "docsx"), Some(2));
	assert_eq!(side.remove(&rows, "docsx"), None);

	side.retain(&rows, |path, _| path != "docs/a.txt");
	assert_eq!(side.whole().paths().collect::<Vec<_>>(), vec!["docs"]);
}

/// Re-keying takes the subtree AND the directory itself, and tells each node where it landed.
#[test]
fn rekeying_a_subtree_moves_the_root_with_it() {
	let rows = Baseline::default();
	let mut side: Side<String> = ["docs", "docs/a.txt", "docs/deep/b.txt", "docsx", "other"]
		.into_iter()
		.map(|path| (path.to_owned(), path.to_owned()))
		.collect();

	side.rekey_subtree(&rows, "docs", "notes", |node, path| *node = path.to_owned());

	let mut paths: Vec<String> = side.whole().paths().map(Cow::into_owned).collect();
	paths.sort();
	assert_eq!(
		paths,
		vec!["docsx", "notes", "notes/a.txt", "notes/deep/b.txt", "other"]
	);
	assert!(
		side.whole().iter().all(|(path, node)| *node == *path),
		"every moved node was told its new path"
	);
}

/// Two sides are equal when they hold the same nodes at the same paths.
#[test]
fn sides_compare_node_for_node() {
	let of = |paths: &[&str]| -> Side<u32> {
		paths
			.iter()
			.zip(0..)
			.map(|(path, at)| ((*path).to_owned(), at))
			.collect()
	};

	assert_eq!(of(&["a", "b"]), of(&["a", "b"]));
	assert_ne!(of(&["a", "b"]), of(&["a"]));
	assert_ne!(of(&["a", "b"]), of(&["a", "c"]));
}

/// A carried side read against an empty tree would answer "holds nothing" for every row it
/// carries, so the test-only reader refuses it rather than letting the assertions go quiet.
#[test]
#[should_panic(expected = "must be read against the baseline it derives from")]
fn the_test_reader_refuses_a_carried_side() {
	let side: Side<u32> = Side::carried();
	let _ = side.whole();
}

fn map(paths: &[&str]) -> HashMap<String, u32> {
	paths
		.iter()
		.zip(0..)
		.map(|(path, at)| ((*path).to_owned(), at))
		.collect()
}

#[test]
fn a_map_answers_about_one_path() {
	let side = map(&["docs", "docs/a.txt", "docsx"]);

	assert_eq!(side.at("docs/a.txt").as_deref(), Some(&1));
	assert!(side.holds("docs/a.txt"));
	assert_eq!(side.at("docs/b.txt"), None);
	assert!(!side.holds("docs/b.txt"));
}

#[test]
fn occupied_folds_the_case_and_takes_the_subtree_but_not_a_prefix() {
	let side = map(&["Docs/a.txt", "notes"]);

	assert!(
		side.occupied("docs"),
		"a directory nothing is keyed by is still occupied by what sits under it, folded"
	);
	assert!(side.occupied("DOCS/A.TXT"), "the node itself, folded");
	assert!(side.occupied("notes"));
	assert!(
		!side.occupied("note"),
		"a key that merely starts with the path does not occupy it"
	);
	assert!(!side.occupied("docs2"));
}

#[test]
fn under_is_the_subtree_in_path_order_and_not_the_directory_itself() {
	let side = map(&[
		"docs",
		"docs/z.txt",
		"docs/a.txt",
		"docs/deep/b.txt",
		"docs/deep",
		"docsx",
		"other",
	]);

	let under: Vec<String> = side
		.under("docs")
		.map(|(path, _)| path.into_owned())
		.collect();
	assert_eq!(
		under,
		vec!["docs/a.txt", "docs/deep", "docs/deep/b.txt", "docs/z.txt"]
	);
}

#[test]
fn a_map_enumerates_every_path_it_holds() {
	let side = map(&["a", "b/c"]);

	let mut paths: Vec<String> = side.paths().map(Cow::into_owned).collect();
	paths.sort();
	assert_eq!(paths, vec!["a", "b/c"]);
	assert_eq!(Nodes::len(&side), 2);
	assert!(!Nodes::is_empty(&side));
	assert_eq!(Nodes::iter(&side).count(), 2);
	assert!(Nodes::is_empty(&map(&[])));
}

// ------------------------------------------------------------------------------------------
// The oracle: the materialised backing is the expectation the derived one is held to.
//
// Both backings are built over the SAME generated tree, handed the SAME operations in the same
// order, and compared after every one of them. The expectation is not written down here — it is
// whatever `Side::Whole` answers, which is the backing a whole read produces. It is an oracle
// for the CONTAINER: what the two answer given the same per-row derivation (see
// [`materialised`]), not whether that derivation is right.
// ------------------------------------------------------------------------------------------

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rand::{Rng, SeedableRng, rngs::StdRng};
use uuid::Uuid;

use super::super::baseline::{BaselineState, NodeKind};
use super::super::plan::RemoteNode;

const CASES: u64 = 200;

/// The paths a case is drawn from, `(path, is_dir)`: nesting, two case-only sibling pairs
/// (`a`/`A`, `top.txt`/`Top.txt`), a pair an ASCII-only fold keeps apart (`Ä`/`ä`), a name that
/// is a string prefix of a sibling's (`a/b` and `a/b.txt`) and of another directory's (`a`,
/// `ab`), a file and a directory sharing a stem (`z`, `z.txt`), and a childless directory for a
/// re-key to carry. A deep path whose ancestors draw `NoRow` leaves PATH-ONLY nodes in the
/// tree, which is a shape a pass meets and neither side holds a node for.
const PATHS: &[(&str, bool)] = &[
	("a", true),
	("a/b", true),
	("a/b/c.txt", false),
	("a/b.txt", false),
	("a/B.txt", false),
	("ab", true),
	("ab/x.txt", false),
	("A", true),
	("A/y.txt", false),
	("d", true),
	("d/e", true),
	("d/e/f.txt", false),
	("top.txt", false),
	("Top.txt", false),
	("\u{c4}.txt", false),
	("\u{e4}.txt", false),
	("z", true),
	("z/1.txt", false),
	("z.txt", false),
	("empty", true),
];

/// Every row state the baseline can hold, plus the two half-recorded shapes a resolution writes
/// on purpose and a file row missing a field its local half needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Shape {
	/// No row at all: the path is in the tree only if a descendant put it there.
	NoRow,
	Synced,
	/// A push no snapshot has confirmed yet — still `Synced`, so still carryable.
	Unconfirmed,
	/// What a `KeepLocal` resolution leaves: the remote half cleared.
	NoRemoteHalf,
	/// What a `KeepRemote` resolution leaves: the local half cleared.
	NoLocalHalf,
	Conflicted,
	Overwritten,
	Adopted,
	/// A file row that records no size: it describes no side fully, so it describes neither.
	FileMissingSize,
}

const SHAPES: [Shape; 9] = [
	Shape::NoRow,
	Shape::Synced,
	Shape::Unconfirmed,
	Shape::NoRemoteHalf,
	Shape::NoLocalHalf,
	Shape::Conflicted,
	Shape::Overwritten,
	Shape::Adopted,
	Shape::FileMissingSize,
];

fn uuid_at(index: usize) -> Uuid {
	Uuid::from_u128(1000 + index as u128)
}

fn node_at(index: usize, version: u8) -> RemoteNode {
	let (path, is_dir) = PATHS[index % PATHS.len()];
	RemoteNode {
		rel_path: path.to_owned(),
		kind: if is_dir {
			NodeKind::Dir
		} else {
			NodeKind::File
		},
		remote_uuid: uuid_at(index),
		stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid_at(index))),
		content_hash: (!is_dir).then(|| Blake3Hash::from([version; 32])),
		size: index as u64,
		modified_millis: 1_700_000_000_000 + index as i64,
	}
}

fn row_at(index: usize, shape: Shape) -> Option<BaselineEntry> {
	if shape == Shape::NoRow {
		return None;
	}
	let (path, is_dir) = PATHS[index];
	let uuid = uuid_at(index);
	let hash =
		Blake3Hash::from([u8::try_from(index % 251).expect("a remainder of 251 fits a byte"); 32]);
	let mut row = BaselineEntry {
		rel_path: path.to_owned(),
		kind: if is_dir {
			NodeKind::Dir
		} else {
			NodeKind::File
		},
		remote_uuid: Some(uuid),
		content_hash: (!is_dir).then_some(hash),
		size: (!is_dir).then_some(index as u64),
		local_mtime: Some(1_700_000_000_000 + index as i64),
		remote_modified: Some(1_700_000_000_500 + index as i64),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
		agreed_hash: (!is_dir).then_some(hash),
	};
	match shape {
		Shape::NoRow => unreachable!("returned above"),
		Shape::Synced => {}
		Shape::Unconfirmed => row.agreed_hash = None,
		Shape::NoRemoteHalf => row.remote_uuid = None,
		Shape::NoLocalHalf => {
			row.content_hash = None;
			row.size = None;
			row.local_mtime = None;
		}
		Shape::Conflicted => row.state = BaselineState::Conflicted,
		Shape::Overwritten => row.state = BaselineState::Overwritten,
		Shape::Adopted => row.state = BaselineState::Adopted,
		Shape::FileMissingSize => row.size = None,
	}
	Some(row)
}

/// The side a whole read would have produced over `baseline`: one node per row that stands in
/// for both sides.
///
/// WHAT THIS PINS, exactly: both backings are filled through the same per-row derivation
/// (`derive::carried`, which is what `RemoteNode::from_row` is), so the oracle below compares
/// the two CONTAINERS and not the derivation. A per-row bug — a field carried wrong, a
/// carryability gate wrong — would produce the same wrong node on both sides and pass here.
/// What checks the derivation itself is `derive`'s `assert_same_maps`, which holds a derived
/// side against a real local scan and a real remote view.
fn materialised(baseline: &Baseline) -> Side<RemoteNode> {
	let mut map: HashMap<String, RemoteNode> = HashMap::new();
	baseline.visit_rows(|row| {
		if let Some(node) = RemoteNode::from_row(row) {
			map.insert(row.rel_path.clone(), node);
		}
	});
	Side::Whole(map)
}

/// Every path the two backings are questioned about: the corpus, spellings that fold onto it,
/// paths no row ever held, and the empty path.
fn probe_paths() -> Vec<String> {
	let mut out: Vec<String> = PATHS.iter().map(|(path, _)| (*path).to_owned()).collect();
	out.extend(
		[
			"",
			"A/Y.TXT",
			"TOP.TXT",
			"\u{c4}.TXT",
			"a/b/c.TXT",
			"nope",
			"a/nope",
			"z.tx",
			"zz",
			"moved0",
			"moved1",
			"moved0/1.txt",
		]
		.into_iter()
		.map(str::to_owned),
	);
	out
}

fn sorted_paths(side: &SideRef<'_, RemoteNode>) -> Vec<String> {
	let mut out: Vec<String> = side.paths().map(Cow::into_owned).collect();
	out.sort();
	out
}

/// Every read the two traits expose, asked of both backings.
///
/// ITERATION ORDER is deliberately not compared: a map has the hash order of its table and a
/// derived side has the tree's, so every enumeration is sorted first. A consumer whose output
/// depended on the raw order was depending on an accident.
fn assert_answers_alike(
	seed: u64,
	step: &str,
	baseline: &Baseline,
	whole: &Side<RemoteNode>,
	carried: &Side<RemoteNode>,
) {
	let (w, c) = (whole.of(baseline), carried.of(baseline));
	let where_ = format!("seed {seed} after {step}");
	let (pw, pc) = (sorted_paths(&w), sorted_paths(&c));
	if pw != pc {
		let overlay_at = |path: &str| match carried {
			Side::Whole(_) => "<whole>".to_owned(),
			Side::Staged(_) => "<staged>".to_owned(),
			Side::Carried(overlay) => match overlay.get(path) {
				Recorded::Nothing => "no-entry".to_owned(),
				Recorded::Tombstone => "TOMBSTONE".to_owned(),
				Recorded::Node(_) => "live".to_owned(),
			},
		};
		for path in pw.iter().chain(pc.iter()) {
			if pw.contains(path) != pc.contains(path) {
				let row = baseline.get(path);
				eprintln!(
					"  DIVERGED at {path:?}: whole={} carried={} | index.carryable={} contains_key={} overlay={}",
					pw.contains(path),
					pc.contains(path),
					baseline.carryable(path),
					baseline.contains_key(path),
					overlay_at(path),
				);
				eprintln!(
					"      row: present={} row.carryable()={:?} state={:?} kind={:?} uuid={} size={:?} mtime={:?} hash={} rmod={:?}",
					row.is_some(),
					row.as_ref().map(BaselineEntry::carryable),
					row.as_ref().map(|r| r.state),
					row.as_ref().map(|r| r.kind),
					row.as_ref().is_some_and(|r| r.remote_uuid.is_some()),
					row.as_ref().and_then(|r| r.size),
					row.as_ref().and_then(|r| r.local_mtime),
					row.as_ref().is_some_and(|r| r.content_hash.is_some()),
					row.as_ref().and_then(|r| r.remote_modified),
				);
			}
		}
	}
	assert_eq!(pw, pc, "{where_}: paths");
	assert_eq!(Nodes::len(&w), Nodes::len(&c), "{where_}: len");
	assert_eq!(
		Nodes::is_empty(&w),
		Nodes::is_empty(&c),
		"{where_}: is_empty"
	);
	assert_eq!(sorted_entries(&w), sorted_entries(&c), "{where_}: iter");
	for path in probe_paths() {
		assert_eq!(
			w.at(&path).map(Cow::into_owned),
			c.at(&path).map(Cow::into_owned),
			"{where_}: at({path:?})"
		);
		assert_eq!(w.holds(&path), c.holds(&path), "{where_}: holds({path:?})");
		assert_eq!(
			w.occupied(&path),
			c.occupied(&path),
			"{where_}: occupied({path:?})"
		);
	}
	// A caller holding the baseline's row asks with it instead of its path, and must be told
	// the same.
	for row in baseline.iter() {
		for (name, side) in [("whole", &w), ("carried", &c)] {
			assert_eq!(
				side.at_row(&row).map(Cow::into_owned),
				side.at(&row.rel_path).map(Cow::into_owned),
				"{where_}: {name}.at_row({:?})",
				row.rel_path
			);
		}
	}
	for (dir, _) in PATHS.iter().filter(|(_, is_dir)| *is_dir) {
		let mut under_w: Vec<String> = w.under(dir).map(|(path, _)| path.into_owned()).collect();
		let mut under_c: Vec<String> = c.under(dir).map(|(path, _)| path.into_owned()).collect();
		under_w.sort();
		under_c.sort();
		assert_eq!(under_w, under_c, "{where_}: under({dir:?})");
		let (mut sub_w, mut sub_c) = (
			whole.subtree_paths(baseline, dir),
			carried.subtree_paths(baseline, dir),
		);
		sub_w.sort();
		sub_c.sort();
		assert_eq!(sub_w, sub_c, "{where_}: subtree_paths({dir:?})");
	}
}

/// Both backings answer alike over every row shape, through every mutation the type exposes.
///
/// The directory move is the one step whose two forms differ on purpose: a materialised side
/// re-keys its own nodes, where a carried side re-keys its OVERLAY and lets
/// `Baseline::move_subtree` carry the rows it derives from. Running them in that order, against
/// one shared tree, is what pins the contract `Side::rekey_subtree` documents.
#[test]
fn a_carried_side_answers_exactly_as_the_materialised_one() {
	let mut shapes_seen: std::collections::BTreeSet<Shape> = std::collections::BTreeSet::new();
	let mut carried_rows = 0usize;
	let mut uncarried_rows = 0usize;
	let mut moves = 0usize;
	for seed in 0..CASES {
		let mut rng = StdRng::seed_from_u64(seed);
		let rows: Vec<BaselineEntry> = (0..PATHS.len())
			.filter_map(|index| {
				let shape = SHAPES[rng.random_range(0..SHAPES.len())];
				shapes_seen.insert(shape);
				row_at(index, shape)
			})
			.collect();
		let mut baseline = Baseline::from_rows(rows);
		baseline.visit_rows(|row| {
			if row.carryable() {
				carried_rows += 1;
			} else {
				uncarried_rows += 1;
			}
		});

		let mut whole = materialised(&baseline);
		let mut carried = Side::carried();
		assert_answers_alike(seed, "assembly", &baseline, &whole, &carried);

		for step in 0..8u8 {
			let index = rng.random_range(0..PATHS.len());
			let path = PATHS[index].0;
			let what;
			match rng.random_range(0..5) {
				0 => {
					what = format!("insert({path:?})");
					let node = node_at(index, step + 1);
					whole.insert(path.to_owned(), node.clone());
					carried.insert(path.to_owned(), node);
				}
				1 => {
					// A path NO row tracks, which only the overlay can hold.
					let fresh = format!("{path}/fresh{step}");
					what = format!("insert-fresh({fresh:?})");
					let node = node_at(index, step + 1);
					whole.insert(fresh.clone(), node.clone());
					carried.insert(fresh, node);
				}
				2 => {
					what = format!("remove({path:?})");
					// `forget` is `remove` for a caller holding the row: asked of copies of
					// both backings, it must say whether `remove` hands a node back and leave
					// the side `remove` leaves.
					let forgotten = baseline.get(path).map(|row| {
						let (mut w, mut c) = (copied(&whole), copied(&carried));
						((w.forget(&row), w), (c.forget(&row), c))
					});
					let removed = (
						whole.remove(&baseline, path),
						carried.remove(&baseline, path),
					);
					assert_eq!(
						removed.0, removed.1,
						"seed {seed} step {step}: remove({path:?}) handed back different nodes"
					);
					if let Some(((w_held, w), (c_held, c))) = forgotten {
						assert_eq!(
							(w_held, c_held),
							(removed.0.is_some(), removed.1.is_some()),
							"seed {seed} step {step}: forget({path:?}) and remove disagree on \
								 whether the side held a node"
						);
						assert!(
							w == whole && c == carried,
							"seed {seed} step {step}: forget({path:?}) left another side than \
								 remove did"
						);
					}
				}
				3 => {
					let keep_files = rng.random_range(0..2) == 0;
					what = format!("retain(files={keep_files})");
					whole.retain(&baseline, |_, node| {
						(node.kind == NodeKind::File) == keep_files
					});
					carried.retain(&baseline, |_, node| {
						(node.kind == NodeKind::File) == keep_files
					});
				}
				_ => {
					// A destination nothing holds, which is the only kind a pass ever folds
					// onto: `next_dir_move` and `next_case_only_dir_rename` both refuse an
					// occupied one. Moving onto an occupied destination OVERWRITES the row
					// sitting there (`move_subtree` is `remove_subtrees` + `upsert`), and a
					// side assembled before that cannot see it — an input no pass produces,
					// so generating it would compare the two backings on an unreachable
					// state rather than on anything the engine can reach.
					let to = format!("moved{step}");
					assert!(
						!baseline.contains_key(&to)
							&& !whole.of(&baseline).occupied(&to)
							&& !carried.of(&baseline).occupied(&to),
						"seed {seed} step {step}: destination {to:?} is occupied, which is \
							 the one shape a fold never produces"
					);
					what = format!("rekey({path:?} -> {to:?})");
					// The SIDES first, then the rows: a carried side re-keys only what this
					// pass observed, and the rows it derives from move underneath it.
					whole.rekey_subtree(&baseline, path, &to, |node, at| {
						node.rel_path = at.to_owned();
					});
					carried.rekey_subtree(&baseline, path, &to, |node, at| {
						node.rel_path = at.to_owned();
					});
					baseline.move_subtree(path, &to);
					moves += 1;
				}
			}
			assert_answers_alike(
				seed,
				&format!("step {step} {what}"),
				&baseline,
				&whole,
				&carried,
			);
		}
	}
	// A corpus that stopped producing these would let the properties above pass by proving
	// nothing about the shapes the two backings can actually disagree on.
	assert_eq!(shapes_seen.len(), SHAPES.len(), "{shapes_seen:?}");
	assert!(carried_rows > 500, "only {carried_rows} carryable row(s)");
	assert!(
		uncarried_rows > 500,
		"only {uncarried_rows} uncarryable row(s)"
	);
	assert!(moves > 100, "only {moves} directory move(s)");
}

/// A tombstone is a path, not a node's slot: what a carried side reserves for the nodes a pass
/// puts in it is not spent on the paths it found empty beside them. A directory renamed away
/// tombstones every row under it, so a table sized for the walk of its new name and shared with
/// those tombstones is a table that doubles on the way.
#[test]
fn tombstones_take_no_room_from_the_nodes() {
	let rows = Baseline::default();
	let mut side: Side<u32> = Side::carried();
	side.reserve(1000);
	let room = side.capacity();
	for at in 0..1000u32 {
		side.remove(&rows, &format!("gone/{at}"));
		side.insert(format!("found/{at}"), at);
	}

	assert_eq!(
		side.capacity(),
		room,
		"a thousand tombstones took the room reserved for a thousand nodes"
	);
	let read = side.of(&rows);
	assert!(!read.holds("gone/7") && read.holds("found/7"));
	assert_eq!(Nodes::len(&read), 1000);
}

/// A carried side LENDS the nodes its overlay holds, as a materialized one lends every node: a
/// walk of a directory the pass observed whole — a renamed directory's destination — would
/// otherwise copy every node under it to hand it out.
#[test]
fn a_carried_side_lends_the_nodes_it_holds() {
	let rows = Baseline::from_rows([row_named(0, "carried.txt")]);
	let mut side: Side<RemoteNode> = Side::carried();
	let mut placed = node_at(4, 1);
	placed.rel_path = "dir/placed.txt".to_owned();
	side.insert(placed.rel_path.clone(), placed);
	let read = side.of(&rows);

	let under: Vec<_> = read.under("dir").collect();
	assert_eq!(under.len(), 1);
	assert!(
		matches!(under[0], (Cow::Borrowed(_), Cow::Borrowed(_))),
		"under() copied a node the side holds"
	);
	let mut walked = 0;
	for (path, node) in read.iter() {
		walked += 1;
		assert_eq!(
			matches!(node, Cow::Borrowed(_)),
			path == "dir/placed.txt",
			"iter() at {path:?}: a held node is lent, a row's is built"
		);
	}
	assert_eq!(walked, 2, "the carried row and the placed node");
}

/// A row at an arbitrary path, in the one shape both sides carry.
fn row_named(index: usize, path: &str) -> BaselineEntry {
	let mut row = row_at(index, Shape::Synced).expect("a synced row");
	row.rel_path = path.to_owned();
	row
}

/// The carried-walk counters move when a carried side really is walked whole.
///
/// Every benchmark row publishes zero for these, and so would a counter that is never
/// incremented at all: a WHOLE pass takes the [`Side::Whole`] arm, so no scenario in the
/// harness can tell a correct zero from a mis-wired one. The published zeros are evidence that
/// a change-scoped pass holds no second copy of the tree ONLY if this fires.
///
/// Asserted as DELTAS rather than absolutes: the counters are process-global and the tests
/// around this one walk carried sides of their own, on other threads, in the same process. A
/// delta cannot be fooled in the direction that matters — a counter that never moves fails this
/// however the rest of the suite is scheduled — where an equality would be flaky, which is its
/// own bug.
#[cfg(feature = "bench-internals")]
#[test]
fn a_carried_side_walked_whole_is_counted() {
	let baseline = Baseline::from_rows([row_named(0, "a"), row_named(1, "b"), row_named(2, "c")]);
	let rows = baseline.carryable_rows() as u64;
	assert!(
		rows > 0,
		"the fixture carries no rows, so walking it would count nothing and this test could \
			 not fail"
	);
	let mut side: Side<u32> = Side::carried();
	side.insert("placed.txt".to_owned(), 7);

	let before = carried_walks();
	let walked: Vec<String> = side.of(&baseline).paths().map(Cow::into_owned).collect();
	let after = carried_walks();

	assert!(
		after.whole_calls > before.whole_calls,
		"a carried side was walked whole and the call went uncounted"
	);
	assert!(
		after.whole_rows >= before.whole_rows + rows,
		"a carried side walked {rows} row(s) and the row counter did not move by them"
	);
	// And the walk actually ran to the end: a marker node derives nothing from a row, so the
	// overlay entry is the one path such a side holds.
	assert_eq!(walked, vec!["placed.txt".to_string()]);
}

/// A subtree moving onto a path this pass already recorded something at: the TOMBSTONES there
/// go, and what a producer PLACED there stays.
///
/// Directed because the generated oracle cannot reach either half — every destination it folds
/// onto is a fresh `moved{step}`, and nothing is ever keyed under that prefix, so the sweep
/// `rekey_subtree` runs over the destination is dead code in that corpus. Both halves are real:
/// a tombstone left standing answers "absent" over the rows that have just arrived, and a live
/// entry swept away is a node the whole backing keeps. The expectation is the materialised
/// backing, as it is above.
#[test]
fn a_move_supersedes_the_destination_tombstones_and_spares_the_rest() {
	let mut baseline = Baseline::from_rows([
		row_named(0, "dst"),
		row_named(2, "dst/keep.txt"),
		row_named(1, "src"),
		row_named(3, "src/x.txt"),
	]);
	let mut whole = materialised(&baseline);
	let mut carried = Side::carried();
	assert_answers_alike(0, "assembly", &baseline, &whole, &carried);

	// What `hide`, `resolve_collisions` and `merge_local`'s `drop_rows` leave at a destination:
	// an observation that nothing is there.
	for path in ["dst", "dst/keep.txt"] {
		assert_eq!(
			whole.remove(&baseline, path),
			carried.remove(&baseline, path),
			"remove({path:?}) handed back different nodes"
		);
	}
	// And what a producer placed there: one beside the landing paths, which no move of `src`
	// touches, and one ON a landing path, which the arriving row supersedes.
	for (at, index) in [("dst/live.txt", 4), ("dst/x.txt", 6)] {
		let mut live = node_at(index, 1);
		live.rel_path = at.to_owned();
		whole.insert(at.to_owned(), live.clone());
		carried.insert(at.to_owned(), live);
	}
	assert_answers_alike(0, "the destination recorded", &baseline, &whole, &carried);

	// The sides first, then the rows, as `fold_dir_moves` runs them.
	whole.rekey_subtree(&baseline, "src", "dst", |node, at| {
		node.rel_path = at.to_owned()
	});
	carried.rekey_subtree(&baseline, "src", "dst", |node, at| {
		node.rel_path = at.to_owned()
	});
	baseline.move_subtree("src", "dst");
	assert_answers_alike(0, "src -> dst", &baseline, &whole, &carried);

	assert_eq!(
		sorted_paths(&carried.of(&baseline)),
		["dst", "dst/live.txt", "dst/x.txt"],
		"the moved subtree answers over the tombstones at the paths it LANDS on; the node a \
			 producer placed beside them survives, and the tombstone over the row the move never \
			 touched (`dst/keep.txt`, which `move_subtree` leaves where it is) still stands"
	);
	assert_eq!(
		carried
			.of(&baseline)
			.at("dst/x.txt")
			.expect("the moved row answers")
			.remote_uuid,
		uuid_at(3),
		"the row that MOVED onto the path answers there, not the node a producer had put on it"
	);
}

/// The pair root is a path like any other to ASK about, and a side holding a node there is
/// occupied at it whichever backing it has.
///
/// The generator never places one, so the empty path the oracle probes every step is a
/// comparison neither backing can fail there; this is the state that makes it discriminate.
#[test]
fn both_backings_answer_alike_at_the_pair_root() {
	let baseline = Baseline::from_rows([row_named(0, "a")]);
	let mut whole = materialised(&baseline);
	let mut carried = Side::carried();
	let mut node = node_at(2, 1);
	node.rel_path = String::new();
	whole.insert(String::new(), node.clone());
	carried.insert(String::new(), node);

	assert_answers_alike(0, "a node at the pair root", &baseline, &whole, &carried);
	assert!(
		carried.of(&baseline).occupied(""),
		"a side holding a node at the pair root is occupied there"
	);
}
