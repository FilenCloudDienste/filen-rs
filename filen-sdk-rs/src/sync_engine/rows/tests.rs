use std::collections::BTreeSet;

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rand::{Rng, SeedableRng, rngs::StdRng};
use uuid::Uuid;

use super::{
	super::{
		baseline::{BaselineEntry, BaselineState, NodeKind},
		plan::confirm_agreed,
		stage::{ConfirmedRows, Stage},
		tree::Tree,
	},
	*,
};

fn file(rel_path: &str, hash: u8) -> BaselineEntry {
	let uuid = Uuid::new_v4();
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: Some(uuid),
		content_hash: Some(Blake3Hash::from([hash; 32])),
		size: Some(7),
		local_mtime: Some(12),
		remote_modified: Some(34),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
		agreed_hash: Some(Blake3Hash::from([hash; 32])),
	}
}

/// A synced file row with contents `hash`, for another module's tests.
pub(in super::super) fn file_for_test(hash: u8) -> BaselineEntry {
	file("unnamed", hash)
}

fn dir(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		kind: NodeKind::Dir,
		content_hash: None,
		size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
		..file(rel_path, 0)
	}
}

/// Every shape the view has to carry across an edit: a destination a move lands on in part, a
/// sibling that is a string prefix and not a path prefix, rows under a path no row names, two
/// siblings that fold together, the foldings `collision_key` and ASCII lowering disagree on
/// (Ä, Greek sigma, Cyrillic, and the dotted İ whose lowering is LONGER than it), a name that
/// sorts between a directory and its own subtree (`docs b`, `docs.txt`), and a row of every
/// index — unconfirmed, conflicted, uncarryable, rule file.
fn corpus() -> Vec<BaselineEntry> {
	vec![
		dir("docs"),
		file("docs/a.txt", 1),
		dir("docs/sub"),
		file("docs/sub/deep.txt", 2),
		file("docs/.filenignore", 3),
		file("docsx", 4),
		file("docs b", 18),
		file("docs.txt", 19),
		dir("archive"),
		dir("archive/docs"),
		file("archive/docs/a.txt", 5),
		file("archive/docs/keep.txt", 6),
		file("orphan/deep/leaf.txt", 7),
		dir("Ärger"),
		file("Ärger/ä.txt", 8),
		dir("ärger"),
		file("ärger/b.txt", 9),
		dir("ΣΑΣ"),
		file("ΣΑΣ/σ.txt", 10),
		dir("σας"),
		dir("Документы"),
		file("Документы/файл.txt", 11),
		dir("İ"),
		file("İ/x.txt", 12),
		file("i̇x", 13),
		BaselineEntry {
			agreed_hash: None,
			..file("docs/pushed.txt", 14)
		},
		BaselineEntry {
			agreed_hash: None,
			..file("pushed-top.txt", 15)
		},
		BaselineEntry {
			state: BaselineState::Conflicted,
			remote_hash: Some(Blake3Hash::from([16; 32])),
			..file("docs/sub/held.txt", 16)
		},
		BaselineEntry {
			remote_uuid: None,
			..file("archive/local-only.txt", 17)
		},
	]
}

/// The paths every question is asked at: each path either side has ever held, each ancestor of
/// one, a folded spelling of each, and each with a character appended — the string prefix
/// that is not a path prefix.
pub(in super::super) fn probes(paths: impl IntoIterator<Item = String>) -> Vec<String> {
	let mut out = BTreeSet::from([String::new()]);
	for path in paths {
		let mut at = path.as_str();
		loop {
			out.insert(at.to_string());
			out.insert(at.to_uppercase());
			out.insert(at.to_lowercase());
			out.insert(format!("{at}x"));
			match at.rsplit_once('/') {
				Some((parent, _)) => at = parent,
				None => break,
			}
		}
	}
	out.into_iter().collect()
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
	items.sort();
	items
}

fn by_path(mut rows: Vec<BaselineEntry>) -> Vec<BaselineEntry> {
	rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
	rows
}

/// The contract every enumeration keeps in place of the tree's walk order: strictly ascending
/// by path, so a parent before everything under it and no row twice.
fn assert_path_order(context: &str, paths: &[String]) {
	for pair in paths.windows(2) {
		assert!(
			pair[0] < pair[1],
			"{context}: {:?} came before {:?}, out of path order",
			pair[0],
			pair[1]
		);
	}
}

/// Every question the boundary answers, asked of `view` and of `oracle` — the resident TREE
/// holding the same rows with the same edits written into it — and required to be the same
/// answer. The tree is real code, not an expectation written down: a question the two answer
/// differently fails whatever either answer is.
///
/// Where the tree has an order the reader does not keep (its walk: siblings by folded name,
/// each subtree contiguous), the answers are compared as sets and the reader's own order is
/// held to its contract instead (see the module doc).
pub(in super::super) fn assert_alike(
	context: &str,
	view: &Baseline,
	oracle: &Tree,
	probes: &[String],
) {
	assert_eq!(view.len(), oracle.len(), "{context}: len");
	assert_eq!(
		view.carryable_rows(),
		oracle.carryable_rows(),
		"{context}: carryable_rows"
	);
	assert_eq!(
		view.has_remote_rows(),
		oracle.has_remote_rows(),
		"{context}: has_remote_rows"
	);
	assert_eq!(
		view.any_unconfirmed(),
		oracle.any_unconfirmed(),
		"{context}: any_unconfirmed"
	);
	let rows: Vec<BaselineEntry> = view.iter().collect();
	assert_path_order(
		&format!("{context}: iter"),
		&rows
			.iter()
			.map(|row| row.rel_path.clone())
			.collect::<Vec<_>>(),
	);
	assert_eq!(rows, by_path(oracle.iter().collect()), "{context}: iter");
	let mut visited = Vec::new();
	view.visit_rows(|row| visited.push(row.clone()));
	assert_eq!(visited, rows, "{context}: visit_rows");
	let mut dirs = Vec::new();
	view.visit_dir_rows(|row| dirs.push(row.clone()));
	assert_eq!(
		dirs,
		rows.iter()
			.filter(|row| row.kind == NodeKind::Dir)
			.cloned()
			.collect::<Vec<_>>(),
		"{context}: visit_dir_rows"
	);
	let mut paths = Vec::new();
	view.visit_row_paths(|path| paths.push(path.to_string()));
	assert_eq!(
		paths,
		sorted(oracle.paths().collect()),
		"{context}: visit_row_paths"
	);
	assert_eq!(
		by_path(view.unconfirmed().collect()),
		by_path(oracle.unconfirmed().collect()),
		"{context}: unconfirmed"
	);
	assert_eq!(
		sorted(view.uncarryable_paths().collect()),
		sorted(oracle.uncarryable_paths().collect()),
		"{context}: uncarryable_paths"
	);
	assert_eq!(
		sorted(view.rule_file_rows().collect()),
		sorted(oracle.rule_file_rows().collect()),
		"{context}: rule_file_rows"
	);
	// The cursor asked in path order, which is what it pages for, and then backwards, which
	// defeats every page it reads: both must answer as `get` does.
	for order in [false, true] {
		let mut cursor = view.cursor();
		let asked: Vec<&String> = if order {
			probes.iter().rev().collect()
		} else {
			probes.iter().collect()
		};
		for path in asked {
			assert_eq!(
				cursor.get(path),
				oracle.get(path),
				"{context}: at {path:?}: cursor (reversed: {order})"
			);
		}
	}
	for path in probes {
		let at = format!("{context}: at {path:?}");
		assert_eq!(view.get(path), oracle.get(path), "{at}: get");
		assert_eq!(
			view.contains_key(path),
			oracle.contains_key(path),
			"{at}: contains_key"
		);
		assert_eq!(
			view.carryable(path),
			oracle.carryable(path),
			"{at}: carryable"
		);
		for is_dir in [false, true] {
			assert_eq!(
				view.tracked(path, is_dir),
				oracle.tracked(path, is_dir),
				"{at}: tracked({is_dir})"
			);
		}
		assert_eq!(view.occupied(path), oracle.occupied(path), "{at}: occupied");
		assert_eq!(
			view.folded_row_paths(path),
			sorted(oracle.folded_row_paths(path)),
			"{at}: folded_row_paths"
		);
		// Every row the folded walk offers, as a set: offered in full when nothing is taken.
		let (mut offered, mut oracle_offered) = (Vec::new(), Vec::new());
		let view_any = view.any_folded_row_at_or_under(path, &mut |row: &str| {
			offered.push(row.to_string());
			false
		});
		let oracle_any = oracle.any_folded_row_at_or_under(path, &mut |row: &str| {
			oracle_offered.push(row.to_string());
			false
		});
		assert_eq!(
			(view_any, sorted(offered.clone())),
			(oracle_any, sorted(oracle_offered)),
			"{at}: any_folded_row_at_or_under"
		);
		// And it stops at the first row taken, whichever that is.
		if let Some(last) = offered.last().cloned() {
			let mut after = 0;
			assert!(
				view.any_folded_row_at_or_under(path, &mut |row: &str| {
					after += 1;
					row == last
				}),
				"{at}: any_folded_row_at_or_under takes a row it offered"
			);
			assert_eq!(after, offered.len(), "{at}: offered past the row it took");
		}
		assert_eq!(
			view.subtree_all_synced(path),
			oracle.subtree_all_synced(path),
			"{at}: subtree_all_synced"
		);
		let under: Vec<BaselineEntry> = view.subtree(path).collect();
		assert_path_order(
			&format!("{at}: subtree"),
			&under
				.iter()
				.map(|row| row.rel_path.clone())
				.collect::<Vec<_>>(),
		);
		assert_eq!(
			under,
			by_path(oracle.subtree(path).collect()),
			"{at}: subtree"
		);
		let mut under_paths = Vec::new();
		view.visit_subtree_paths(path, |p| under_paths.push(p.to_string()));
		let mut oracle_under = Vec::new();
		oracle.visit_subtree_paths(path, |p| oracle_under.push(p.to_string()));
		assert_eq!(
			under_paths,
			sorted(oracle_under),
			"{at}: visit_subtree_paths"
		);
		assert_eq!(
			view.count_subtree(path),
			under_paths.len(),
			"{at}: count_subtree"
		);
	}
}

/// The fold window. A folded directory move re-keys a subtree the store still holds under its
/// source, and `plan::next_dir_move` then asks `occupied(to)` and `plan::parents_ready` asks
/// `occupied(parent)` of the pass's view — which must answer with the source vacated and the
/// destination occupied, exactly as a copy of the tree with the move written into it does,
/// while the TABLE still holds the rows under the source.
#[test]
fn a_folded_move_answers_the_fold_windows_questions_as_a_moved_copy_does() {
	let rows = corpus();
	let mut view = Baseline::from_rows(rows.clone());
	let mut oracle = Tree::from_rows(rows);
	view.move_subtree("docs", "archive/docs");
	oracle.move_subtree("docs", "archive/docs");
	assert!(view.edits.is_some(), "the move is an edit beside the table");
	assert!(
		view.rows
			.as_deref()
			.is_some_and(|snapshot| snapshot.row("docs/a.txt").is_some()),
		"the table still holds the source: this is the window, not a committed move"
	);

	let asked = [
		// The source, a folded spelling of it, and a string prefix of it that is no path prefix.
		"docs",
		"DOCS",
		"Docs",
		"docsx",
		"doc",
		// The destination, its folded spellings, its parent, and the same prefix shapes.
		"archive/docs",
		"ARCHIVE/DOCS",
		"Archive/Docs",
		"archive/docsx",
		"archive/doc",
		"archive",
		"ARCHIVE",
		// A row the move landed on, one it did not, and one it brought.
		"archive/docs/a.txt",
		"archive/docs/keep.txt",
		"archive/docs/sub/deep.txt",
		"ARCHIVE/DOCS/SUB",
	];
	let mut answers = BTreeSet::new();
	for path in asked {
		let answer = view.occupied(path);
		assert_eq!(answer, oracle.occupied(path), "occupied({path:?})");
		answers.insert(answer);
	}
	assert_eq!(
		answers.len(),
		2,
		"the questions asked have to include both answers, or they prove nothing"
	);
	assert_alike(
		"docs -> archive/docs",
		&view,
		&oracle,
		&probes(asked.iter().map(|path| path.to_string())),
	);
}

/// Apply one random edit — a move or a confirmation — to both the view and the oracle, and say
/// what it was. The sequences this draws chain moves through each other's sources and
/// destinations, move a subtree back onto where it came from, rename by case alone, move a
/// directory into its own subtree, and advance the markers of rows that have and have not
/// moved.
pub(in super::super) fn random_edit(
	rng: &mut StdRng,
	step: usize,
	view: &mut Baseline,
	oracle: &mut Tree,
	probes: &[String],
) -> (String, bool) {
	// A path among those the copy holds now, or any probe at all — which reaches paths no row
	// names, folded spellings and nothing at all.
	let held = oracle.paths().collect::<Vec<_>>();
	let pick = |rng: &mut StdRng, from: &[String]| from[rng.random_range(0..from.len())].clone();
	if rng.random_range(0..6) == 0 {
		// A confirmation as the engine makes one: every unconfirmed row a predicate takes,
		// advanced to its own content. The tree's form of it is one `set_agreed` per row.
		let salt = rng.random_range(0..3_usize);
		let takes = |row: &BaselineEntry| !(row.rel_path.len() + salt).is_multiple_of(3);
		let mut confirmed = view.confirm_where(takes);
		let mut expected: Vec<BaselineEntry> = oracle
			.unconfirmed()
			.filter(takes)
			.collect::<Vec<_>>()
			.into_iter()
			.filter_map(|row| oracle.set_agreed(&row.rel_path, row.content_hash))
			.collect();
		confirmed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		expected.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		assert_eq!(
			confirmed, expected,
			"step {step}: confirm_where confirmed different rows"
		);
		return (format!("confirm_where(salt {salt})"), false);
	}
	if rng.random_range(0..4) == 0 {
		let path = if held.is_empty() || rng.random_range(0..3) == 0 {
			pick(rng, probes)
		} else {
			pick(rng, &held)
		};
		let hash = match rng.random_range(0..3) {
			0 => None,
			1 => oracle.get(&path).and_then(|row| row.content_hash),
			_ => Some(Blake3Hash::from([rng.random_range(0..4); 32])),
		};
		assert_eq!(
			view.set_agreed(&path, hash),
			oracle.set_agreed(&path, hash),
			"step {step}: set_agreed({path:?}) handed back different rows"
		);
		(format!("set_agreed({path:?}, {hash:?})"), false)
	} else {
		let from = if held.is_empty() || rng.random_range(0..4) == 0 {
			pick(rng, probes)
		} else {
			pick(rng, &held)
		};
		let to = match rng.random_range(0..5) {
			// A destination under a path the pair holds, or a fresh one.
			0 => format!("{}/moved{step}", pick(rng, probes)),
			1 => format!("moved{step}"),
			// The source by case alone.
			2 => from.to_uppercase(),
			// Anything the pair holds now: a move onto an occupied destination.
			_ if !held.is_empty() => pick(rng, &held),
			_ => pick(rng, probes),
		};
		let to = to.trim_start_matches('/').to_string();
		view.move_subtree(&from, &to);
		oracle.move_subtree(&from, &to);
		(format!("move_subtree({from:?} -> {to:?})"), true)
	}
}

/// A confirmation as a whole pass makes one of more rows than a window: every unconfirmed row
/// `takes` accepts, advanced to its own content, moved into `stage` and put into the view out of
/// it. The tree's form of it is one `set_agreed` per row. Whether it confirmed anything.
fn confirm_staged(
	view: &mut Baseline,
	oracle: &mut Tree,
	takes: impl Fn(&BaselineEntry) -> bool,
	stage: Stage,
) -> bool {
	let mut stage = Some(stage);
	let mut confirmed = ConfirmedRows::new(0);
	for row in view.unconfirmed().filter(|row| takes(row)) {
		confirmed
			.push(confirm_agreed(row), || {
				Ok(stage.take().expect("one stage per confirmation"))
			})
			.expect("staging a confirmed row");
	}
	let confirmed = confirmed.finish().expect("staging the confirmed rows");
	view.confirm(&confirmed);
	for row in oracle
		.unconfirmed()
		.filter(|row| takes(row))
		.collect::<Vec<_>>()
	{
		oracle.set_agreed(&row.rel_path, row.content_hash);
	}
	matches!(confirmed, Confirmed::Staged(_))
}

/// How many layers the view's edits have, and whether the top one is a mapped move.
fn layers(view: &Baseline) -> (usize, bool) {
	view.edits.as_deref().map_or((0, false), |edits| {
		(
			edits.layers.len(),
			matches!(edits.layers.last(), Some(Layer::Moved { .. })),
		)
	})
}

/// Every edit sequence a pass can write, and many it cannot, answered alike by the view and by
/// the tree with the same edits written into it — after every single edit. Short sequences
/// from many seeds, then long ones that stack mapped moves many layers deep: moves through each
/// other's destinations, back onto their sources, into their own subtrees, with rows written and
/// markers advanced between them.
#[test]
fn every_edit_sequence_answers_as_a_copy_with_the_same_edits_does() {
	let rows = corpus();
	let everything = probes(
		rows.iter()
			.map(|row| row.rel_path.clone())
			.chain(["new", "new/inner", "archive/new", "DOCS"].map(String::from)),
	);
	// Before any edit too: the view is the table alone there, and its answers the table's own
	// statements — the one place `count_subtree` is a `COUNT` rather than the walk it counts.
	assert_alike(
		"unedited",
		&Baseline::from_rows(rows.clone()),
		&Tree::from_rows(rows.clone()),
		&everything,
	);
	let (mut mapped, mut written, mut confirms, mut deepest) = (0, 0, 0, 0);
	let mut staged = 0;
	for (seeds, steps) in [(0..60, 8), (1000..1020, 24)] {
		for seed in seeds {
			let mut rng = StdRng::seed_from_u64(seed);
			let mut view = Baseline::from_rows(rows.clone());
			let mut oracle = Tree::from_rows(rows.clone());
			let mut probes = everything.clone();
			// A whole pass's confirmation past its window, read out of its stage as the bottom
			// layer, under everything the sequence then stacks on it.
			let (_stage_dir, stage) = Stage::in_temp_dir();
			if seed % 3 == 0 && confirm_staged(&mut view, &mut oracle, |_| true, stage) {
				staged += 1;
				assert_alike(&format!("seed {seed} staged"), &view, &oracle, &probes);
			}
			for step in 0..steps {
				let (before, _) = layers(&view);
				let (what, moved) = random_edit(&mut rng, step, &mut view, &mut oracle, &probes);
				let (after, top_mapped) = layers(&view);
				if !moved {
					confirms += 1;
				} else if after > before && top_mapped {
					mapped += 1;
				} else if after > 0 && !top_mapped {
					written += 1;
				}
				deepest = deepest.max(after);
				probes.extend(oracle.paths());
				probes.sort();
				probes.dedup();
				assert_alike(
					&format!("seed {seed} step {step} {what}"),
					&view,
					&oracle,
					&probes,
				);
			}
		}
	}
	// Both kinds of move, and views many layers deep, or the comparison above proved less than it
	// says.
	assert!(
		mapped > 150 && written > 100 && confirms > 100 && deepest >= 8 && staged > 20,
		"{mapped} mapped move(s), {written} written, {confirms} confirmation(s), {staged} \
			 staged, {deepest} layer(s) at most"
	);
}

/// A folded move of a directory holding many rows is a MAPPING: its edits hold two paths,
/// whatever the subtree holds, and nothing of the rows it carries — while every question asked
/// of the moved subtree, at its new place and its old one and under any spelling, answers as a
/// copy with the move written into it does.
#[test]
fn a_folded_move_of_a_large_directory_holds_none_of_its_rows() {
	let mut rows = vec![
		dir("big"),
		dir("big/inner"),
		dir("Other"),
		file("Other/x", 9),
	];
	for n in 0..(PAGE * 8) {
		rows.push(file(&format!("big/{n:05}.txt"), 1));
		rows.push(file(&format!("big/inner/{n:05}.txt"), 2));
	}
	let mut view = Baseline::from_rows(rows.clone());
	let mut oracle = Tree::from_rows(rows);
	view.move_subtree("big", "Renamed/Big");
	oracle.move_subtree("big", "Renamed/Big");
	let edits = view.edits.as_deref().expect("the move is an edit");
	assert!(
		edits
			.layers
			.iter()
			.all(|layer| matches!(layer, Layer::Moved { .. })),
		"the move wrote rows: {} layer(s), {} written",
		edits.layers.len(),
		edits
			.layers
			.iter()
			.map(|layer| match layer {
				Layer::Block(block) => block.written.len(),
				Layer::Moved { .. } => 0,
			})
			.sum::<usize>()
	);
	// The pass then confirms beside it and moves a directory inside the moved one, the way a
	// fold of nested moves stacks them.
	view.move_subtree("Renamed/Big/inner", "Renamed/inner2");
	oracle.move_subtree("Renamed/Big/inner", "Renamed/inner2");
	let probes = probes(
		[
			"big/00001.txt",
			"big/inner/00002.txt",
			"Renamed/Big/00003.txt",
			"Renamed/inner2/00004.txt",
			"RENAMED/BIG/INNER",
			"Other/x",
		]
		.map(String::from),
	);
	assert_alike("big -> Renamed/Big, inner out", &view, &oracle, &probes);
}

/// A pass that edits nothing reads the table itself, and one that edits never writes into it: a
/// clone taken before the edits still reads the rows as they were.
#[test]
fn an_edit_never_reaches_the_table() {
	let before = Baseline::from_rows(corpus());
	let mut pass = before.clone();
	assert!(
		pass.set_agreed("nothing/here", None).is_none() && pass.edits.is_none(),
		"a confirmation that finds no row begins no edits"
	);
	let rows: Vec<BaselineEntry> = before.iter().collect();
	pass.move_subtree("docs", "moved");
	pass.set_agreed("pushed-top.txt", None);
	pass.set_agreed("moved/pushed.txt", Some(Blake3Hash::from([14; 32])));
	assert!(
		Arc::ptr_eq(
			pass.rows.as_ref().expect("rows"),
			before.rows.as_ref().expect("rows")
		),
		"the pass still reads the same snapshot"
	);
	assert_eq!(before.iter().collect::<Vec<_>>(), rows);
	assert!(!pass.contains_key("docs/a.txt") && pass.contains_key("moved/a.txt"));
}

/// The rows a dry run seeds read as a copy with them written in does — at fresh paths, under a
/// directory the table holds rows in, a case variant of a row beside it, over a staged
/// confirmation and under a move folded after them — and never reach the table.
#[test]
fn seeded_rows_answer_as_a_copy_with_them_written_in_does() {
	let rows = corpus();
	let before = Baseline::from_rows(rows.clone());
	let mut view = before.clone();
	let mut oracle = Tree::from_rows(rows.clone());
	let (_stage_dir, stage) = Stage::in_temp_dir();
	confirm_staged(&mut view, &mut oracle, |_| true, stage);
	let seeded = vec![
		file("docs/seeded.bin", 21),
		file("DOCS/A.TXT.bin", 22),
		file("fresh/deep/seeded.bin", 23),
	];
	view.write_seeded(seeded.clone());
	for row in &seeded {
		oracle.upsert(row);
	}
	let probes = probes(seeded.iter().map(|row| row.rel_path.clone()));
	assert_alike("seeded", &view, &oracle, &probes);
	view.move_subtree("docs", "moved");
	oracle.move_subtree("docs", "moved");
	assert_alike(
		"seeded, then moved",
		&view,
		&oracle,
		&probes
			.iter()
			.cloned()
			.chain(["moved/seeded.bin".to_string()])
			.collect::<Vec<_>>(),
	);
	assert_eq!(before.iter().collect::<Vec<_>>(), by_path(rows));
}

/// A staged confirmation of more markers than the stage reads a page — every other row of a
/// directory larger than that — reads as markers held one by one do, through a walk across the
/// stage's page boundaries, lookups in and out of order, and a move of part of it on top.
#[test]
fn a_staged_confirmation_answers_across_its_page_boundaries() {
	let mut rows = vec![dir("big"), dir("big/moved"), file("other.txt", 3)];
	for n in 0..2600 {
		let at = if n % 5 == 0 { "big/moved" } else { "big" };
		rows.push(BaselineEntry {
			agreed_hash: None,
			..file(&format!("{at}/{n:05}.txt"), 1)
		});
	}
	let mut view = Baseline::from_rows(rows.clone());
	let mut oracle = Tree::from_rows(rows);
	let (_stage_dir, stage) = Stage::in_temp_dir();
	assert!(confirm_staged(
		&mut view,
		&mut oracle,
		|row| !row.rel_path.ends_with("3.txt"),
		stage
	));
	let probes = probes(
		[
			"big/00001.txt",
			"big/00003.txt",
			"big/moved/00005.txt",
			"big/02598.txt",
			"elsewhere/00005.txt",
		]
		.map(String::from),
	);
	assert_alike("staged", &view, &oracle, &probes);
	// Every row, by lookup: backwards, which reads a row a statement, then forwards.
	let paths: Vec<String> = oracle.paths().collect();
	for path in paths.iter().rev().chain(&paths) {
		assert_eq!(view.get(path), oracle.get(path), "{path}");
	}
	view.move_subtree("big/moved", "elsewhere");
	oracle.move_subtree("big/moved", "elsewhere");
	assert_alike("staged, then moved", &view, &oracle, &probes);
	// A second staged confirmation, of the rest, over a view already edited: read into markers
	// held one by one, above the move.
	let (_rest_dir, rest) = Stage::in_temp_dir();
	assert!(confirm_staged(&mut view, &mut oracle, |_| true, rest));
	assert!(!view.any_unconfirmed());
	assert_alike("staged, moved, staged again", &view, &oracle, &probes);
}

/// A page boundary falls inside every range the reader pages over: more rows than a page under
/// one directory, under one folded spelling, and in the whole pair — with a moved-away subtree
/// in the middle of them for the enumeration to skip past.
#[test]
fn every_paged_read_answers_across_its_page_boundaries() {
	let mut rows = vec![dir("big"), dir("BIG"), dir("big/moved")];
	for n in 0..(PAGE * 2 + 7) {
		rows.push(file(&format!("big/{n:04}.txt"), 1));
		rows.push(file(&format!("BIG/{n:04}.txt"), 2));
	}
	for n in 0..(PAGE + 3) {
		rows.push(file(&format!("big/moved/{n:04}.txt"), 3));
	}
	let mut view = Baseline::from_rows(rows.clone());
	let mut oracle = Tree::from_rows(rows);
	let probes = probes(["big/0001.txt", "big/moved/0002.txt", "elsewhere"].map(String::from));
	assert_alike("unedited", &view, &oracle, &probes);
	view.move_subtree("big/moved", "elsewhere");
	oracle.move_subtree("big/moved", "elsewhere");
	assert_alike("with a paged subtree moved away", &view, &oracle, &probes);
}
