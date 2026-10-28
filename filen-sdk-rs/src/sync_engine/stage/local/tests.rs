use std::{borrow::Cow, collections::HashMap};

use filen_types::crypto::Blake3Hash;
use rand::{Rng, SeedableRng, rngs::StdRng};

use super::{
	super::{
		super::{
			rows::Baseline,
			scan::MapSink,
			side::{Nodes, NodesAt, test_support::sorted_entries},
		},
		paging_of,
		tests::row,
	},
	*,
};
use crate::alloc_meter;

const CASES: u64 = 200;

fn node(rel_path: &str, kind: NodeKind, version: u8) -> LocalNode {
	LocalNode {
		rel_path: rel_path.to_owned(),
		kind,
		size: match kind {
			NodeKind::File => u64::from(version) + 1,
			NodeKind::Dir => 0,
		},
		mtime_millis: 1_700_000_000_000 + i64::from(version),
		// Either side of the millisecond, as Unix reads an mtime.
		mtime_nanos: (i32::from(version) - 128) * 3_000,
		content_hash: (kind == NodeKind::File).then(|| Blake3Hash::from([version; 32])),
	}
}

/// A side staged in a fresh stage of its own, through the walk's sink, read and re-keyed as
/// `paging` says; with the directory its stage is in.
fn staged(
	nodes: impl IntoIterator<Item = LocalNode>,
	paging: Paging,
) -> (tempfile::TempDir, Side<LocalNode>) {
	let (dir, stage) = Stage::in_temp_dir();
	let mut sink = StageSink::new(stage, paging).unwrap();
	for node in nodes {
		assert_eq!(sink.put(Found::Node(node)).unwrap(), None);
	}
	(dir, sink.finish().unwrap())
}

/// The walk's collision check, asked of both sinks: every entry put is answered alike — taken,
/// or refused with the same twin named — and what is left is the same nodes. The paths fold
/// onto one another every way the walk can meet: by case, by a non-ASCII case, a directory and
/// a file, and the identical path twice (two names NFC folds together).
#[test]
fn the_stage_sink_takes_and_refuses_what_the_map_sink_does() {
	const PATHS: &[&str] = &[
		"a",
		"A",
		"a/b.txt",
		"A/B.TXT",
		"a/B.txt",
		"ab",
		"\u{c4}.txt",
		"\u{e4}.txt",
		"\u{c4}.TXT",
		"z",
		"z.txt",
		"Z.txt",
		"docs",
		"docs/x",
		"DOCS/x",
	];
	let (mut refused, mut claimed, mut taken_by_claim) = (0, 0, 0);
	for seed in 0..CASES {
		let mut rng = StdRng::seed_from_u64(seed);
		let (_dir, stage) = Stage::in_temp_dir();
		let (mut map, mut stage) = (
			MapSink::with_capacity(0),
			StageSink::new(stage, Paging::PASS).unwrap(),
		);
		let mut claims: Vec<String> = Vec::new();
		for step in 0..rng.random_range(1..=24u8) {
			let path = PATHS[rng.random_range(0..PATHS.len())];
			let found = match rng.random_range(0..4) {
				0 => Found::Claim(path.to_owned()),
				1 => Found::Node(node(path, NodeKind::Dir, step)),
				_ => Found::Node(node(path, NodeKind::File, step)),
			};
			let Ok(by_map) = map.put(found.clone());
			let by_stage = stage.put(found.clone()).unwrap();
			assert_eq!(by_stage, by_map, "seed {seed} step {step}: {found:?}");
			match (&found, by_map) {
				(_, Some(collision)) => {
					refused += 1;
					taken_by_claim += usize::from(claims.contains(&collision.taken));
				}
				(Found::Claim(path), None) => {
					claimed += 1;
					claims.push(path.clone());
				}
				(Found::Node(_), None) => {}
			}
		}
		let Ok(map) = map.finish();
		let stage = stage.finish().unwrap();
		assert_eq!(
			sorted_entries(&stage.whole()),
			sorted_entries(&map.whole()),
			"seed {seed}: the nodes left"
		);
		assert_eq!(Nodes::len(&stage.whole()), Nodes::len(&map.whole()));
	}
	// A corpus that stopped producing these would let the equality above pass by proving
	// nothing about the refusals.
	assert!(refused > 1000, "only {refused} refusal(s)");
	assert!(claimed > 200, "only {claimed} claim(s)");
	assert!(
		taken_by_claim > 50,
		"only {taken_by_claim} refusal(s) by a claim"
	);
}

fn sorted(paths: impl Iterator<Item = String>) -> Vec<String> {
	let mut out: Vec<String> = paths.collect();
	out.sort();
	out
}

/// The paths a staged side is held to the map over: no two fold together, as a walk leaves
/// them. Nesting, a name that is a string prefix of a sibling's (`a/b` and `a/b.txt`) and of
/// another directory's (`a`, `ab`), a file and a directory sharing a stem (`z`, `z.txt`), a
/// non-ASCII name with a subtree, and a childless directory for a move to carry.
const CORPUS: &[(&str, NodeKind)] = &[
	("a", NodeKind::Dir),
	("a/b", NodeKind::Dir),
	("a/b/c.txt", NodeKind::File),
	("a/b.txt", NodeKind::File),
	("ab", NodeKind::Dir),
	("ab/x.txt", NodeKind::File),
	("d", NodeKind::Dir),
	("d/e", NodeKind::Dir),
	("d/e/f.txt", NodeKind::File),
	("top.txt", NodeKind::File),
	("\u{c4}", NodeKind::Dir),
	("\u{c4}/y.txt", NodeKind::File),
	("z", NodeKind::Dir),
	("z/1.txt", NodeKind::File),
	("z.txt", NodeKind::File),
	("empty", NodeKind::Dir),
];

/// Every path the two are questioned about: the corpus in its own order and in reverse —
/// a read-ahead walked forward and one asked out of order — spellings that fold onto it,
/// paths nothing holds, the pair root, and where the moves below land.
fn probe_paths() -> Vec<String> {
	let corpus = CORPUS.iter().map(|(path, _)| *path);
	corpus
		.clone()
		.chain(corpus.rev())
		.chain([
			"",
			"A",
			"A/B.TXT",
			"TOP.TXT",
			"\u{e4}/Y.TXT",
			"a/nope",
			"z.tx",
			"zz",
			"moved0",
			"moved3/1.txt",
			"MOVED5",
		])
		.map(str::to_owned)
		.collect()
}

/// Every read the two traits expose, asked of a map and of a staged side.
///
/// Iteration order is not compared: a map has its table's, and only the enumerations are
/// sorted first.
fn assert_answers_alike(at: &str, whole: &Side<LocalNode>, staged: &Side<LocalNode>) {
	let (w, s) = (whole.whole(), staged.whole());
	assert_eq!(
		sorted(w.paths().map(Cow::into_owned)),
		sorted(s.paths().map(Cow::into_owned)),
		"{at}: paths"
	);
	// In path order on both, which is what the directory-move fold walks them in.
	assert_eq!(
		w.dirs()
			.map(|(path, node)| (path.into_owned(), node.into_owned()))
			.collect::<Vec<_>>(),
		s.dirs()
			.map(|(path, node)| (path.into_owned(), node.into_owned()))
			.collect::<Vec<_>>(),
		"{at}: dirs"
	);
	assert_eq!(Nodes::len(&w), Nodes::len(&s), "{at}: len");
	assert_eq!(Nodes::is_empty(&w), Nodes::is_empty(&s), "{at}: is_empty");
	assert_eq!(sorted_entries(&w), sorted_entries(&s), "{at}: iter");
	assert_eq!(
		sorted(whole.own_keys().map(Cow::into_owned)),
		sorted(staged.own_keys().map(Cow::into_owned)),
		"{at}: own_keys"
	);
	let rows = Baseline::default();
	for path in probe_paths() {
		assert_eq!(
			w.at(&path).map(Cow::into_owned),
			s.at(&path).map(Cow::into_owned),
			"{at}: at({path:?})"
		);
		assert_eq!(w.holds(&path), s.holds(&path), "{at}: holds({path:?})");
		assert_eq!(
			w.occupied(&path),
			s.occupied(&path),
			"{at}: occupied({path:?})"
		);
		assert_eq!(
			w.at_row(&row(&path)).map(Cow::into_owned),
			s.at_row(&row(&path)).map(Cow::into_owned),
			"{at}: at_row({path:?})"
		);
		// In path order on both, which is what a signature is read in.
		assert_eq!(
			w.under(&path)
				.map(|(path, node)| (path.into_owned(), node.into_owned()))
				.collect::<Vec<_>>(),
			s.under(&path)
				.map(|(path, node)| (path.into_owned(), node.into_owned()))
				.collect::<Vec<_>>(),
			"{at}: under({path:?})"
		);
		assert_eq!(
			sorted(whole.subtree_paths(&rows, &path).into_iter()),
			sorted(staged.subtree_paths(&rows, &path).into_iter()),
			"{at}: subtree_paths({path:?})"
		);
	}
}

/// A staged side answers every read exactly as the map a walk into memory would have made,
/// through every edit a side has, read and re-keyed one, two and seven rows at a time as well
/// as a pass's pages: the expectation is not written down here, it is whatever the map answers.
#[test]
fn a_staged_side_answers_exactly_as_the_materialised_one() {
	let mut moves = 0usize;
	for paging in [paging_of(1), paging_of(2), paging_of(7), Paging::PASS] {
		for seed in 0..CASES {
			let mut rng = StdRng::seed_from_u64(seed);
			let nodes: Vec<LocalNode> = CORPUS
				.iter()
				.filter(|_| rng.random_range(0..3) != 0)
				.map(|(path, kind)| node(path, *kind, 0))
				.collect();
			let mut whole: Side<LocalNode> = nodes
				.iter()
				.map(|node| (node.rel_path.clone(), node.clone()))
				.collect();
			let (_dir, mut staged) = staged(nodes, paging);
			let at = |step: &str| format!("{paging:?} seed {seed} after {step}");
			assert_answers_alike(&at("the walk"), &whole, &staged);

			let rows = Baseline::default();
			for step in 0..8u8 {
				let (path, kind) = CORPUS[rng.random_range(0..CORPUS.len())];
				let what = match rng.random_range(0..7) {
					0 => {
						let put = node(path, kind, step + 1);
						whole.insert(path.to_owned(), put.clone());
						staged.insert(path.to_owned(), put);
						format!("insert({path:?})")
					}
					1 => {
						// A path the walk never found, which only a later edit can add.
						let fresh = format!("{path}/fresh{step}");
						let put = node(&fresh, NodeKind::File, step + 1);
						whole.insert(fresh.clone(), put.clone());
						staged.insert(fresh.clone(), put);
						format!("insert({fresh:?})")
					}
					2 => {
						assert_eq!(
							whole.remove(&rows, path),
							staged.remove(&rows, path),
							"{}: remove({path:?}) handed back different nodes",
							at("a remove")
						);
						format!("remove({path:?})")
					}
					3 => {
						assert_eq!(
							whole.forget(&row(path)),
							staged.forget(&row(path)),
							"{}: forget({path:?})",
							at("a forget")
						);
						format!("forget({path:?})")
					}
					4 => {
						let keep = if rng.random_range(0..2) == 0 {
							NodeKind::File
						} else {
							NodeKind::Dir
						};
						whole.retain(&rows, |_, node| node.kind == keep);
						staged.retain(&rows, |_, node| node.kind == keep);
						format!("retain({keep:?})")
					}
					5 => {
						let root = node("", NodeKind::Dir, step + 1);
						whole.insert(String::new(), root.clone());
						staged.insert(String::new(), root);
						"insert(\"\")".to_owned()
					}
					_ => {
						// A destination free under every spelling, which is the only kind a
						// directory move lands on (`next_dir_move` refuses an occupied one).
						let to = format!("moved{step}");
						assert!(!whole.whole().occupied(&to));
						let set_path =
							|node: &mut LocalNode, at: &str| node.rel_path = at.to_owned();
						whole.rekey_subtree(&rows, path, &to, set_path);
						staged.rekey_subtree(&rows, path, &to, set_path);
						moves += 1;
						format!("rekey({path:?} -> {to:?})")
					}
				};
				assert_answers_alike(&at(&format!("step {step} {what}")), &whole, &staged);
			}
			assert_eq!(
				sorted_entries(&whole.whole()),
				{
					let mut handed: Vec<(String, LocalNode)> = staged.into_entries(&rows).collect();
					handed.sort_by(|left, right| left.0.cmp(&right.0));
					handed
				},
				"{paging:?} seed {seed}: into_entries"
			);
		}
	}
	assert!(moves > 500, "only {moves} directory move(s)");
}

/// Two staged sides compare as two maps do, by the nodes they hold at their paths and not by
/// which table holds them: two stages of the same walk read alike however they are paged, and
/// one node put over its twin's tells them apart.
#[test]
fn staged_sides_compare_node_for_node() {
	let nodes = || CORPUS.iter().map(|(path, kind)| node(path, *kind, 0));
	let (_one_dir, mut one) = staged(nodes(), paging_of(2));
	let (_other_dir, other) = staged(nodes(), Paging::PASS);
	assert_eq!(one, other);
	one.insert("top.txt".to_owned(), node("top.txt", NodeKind::File, 1));
	assert_ne!(one, other);
}

/// What a walk into the stage and a read of all of it back hold in memory is a page, not the
/// tree: 20,000 nodes into a map hold a few MiB.
#[test]
fn a_staged_walk_and_a_read_of_it_hold_a_page_of_the_tree() {
	const NODES: u16 = 20_000;
	let (_dir, stage) = Stage::in_temp_dir();
	let (read, peak) = alloc_meter::peak_bytes(|| {
		let mut sink = StageSink::new(stage, Paging::PASS).unwrap();
		for n in 0..NODES {
			let path = format!("d{:02}/f{n:05}.txt", n % 100);
			let put = sink.put(Found::Node(node(&path, NodeKind::File, 1)));
			assert_eq!(put.unwrap(), None);
		}
		sink.finish().unwrap().whole().iter().count()
	});
	assert_eq!(read, usize::from(NODES));
	assert!(
		peak < 1024 * 1024,
		"staging and reading back the walk held {peak} bytes at once"
	);
}

/// A claim holds its folded name while the walk runs and is gone once it is done: the side reads
/// nodes and nothing else.
#[test]
fn a_claim_refuses_its_twin_and_leaves_no_node() {
	let (_dir, stage) = Stage::in_temp_dir();
	let mut sink = StageSink::new(stage, Paging::PASS).unwrap();
	assert_eq!(sink.put(Found::Claim("Notes".to_owned())).unwrap(), None);
	assert_eq!(
		sink.put(Found::Node(node("notes", NodeKind::Dir, 0)))
			.unwrap(),
		Some(Collision {
			taken: "Notes".to_owned(),
			refused: "notes".to_owned(),
		})
	);
	assert_eq!(
		sink.put(Found::Node(node("kept.txt", NodeKind::File, 0)))
			.unwrap(),
		None
	);
	let side = sink.finish().unwrap();
	assert_eq!(
		sorted_entries(&side.whole()),
		vec![("kept.txt".to_owned(), node("kept.txt", NodeKind::File, 0))]
	);
	assert!(!side.whole().occupied("NOTES"));
	assert_eq!(Nodes::len(&side.whole()), 1);
}

/// The oracle's corpus is what a walk leaves: no two of its paths fold together, so a staged
/// side can hold all of it.
#[test]
fn the_oracle_corpus_holds_no_two_paths_that_fold_together() {
	let mut folded: HashMap<String, &str> = HashMap::new();
	for (path, _) in CORPUS {
		assert_eq!(folded.insert(collision_key(path), path), None, "{path:?}");
	}
}
