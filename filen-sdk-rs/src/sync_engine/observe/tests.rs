use std::fs;

use super::super::{derive::from_baseline, side::NodesAt};

use uuid::Uuid;

use super::*;
use crate::{
	alloc_meter,
	sync_engine::{ScanGap, scan::UnrecordedRoot},
};

fn temp_root() -> std::path::PathBuf {
	let dir = std::env::temp_dir().join(format!("filen_observe_test_{}", Uuid::new_v4()));
	fs::create_dir_all(&dir).unwrap();
	dir
}

fn row(rel_path: &str, kind: NodeKind) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind,
		remote_uuid: None,
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

/// The dirty set as the changelist hands it over, and the observation of it with no baseline.
fn observe(root: &Path, paths: &[&str]) -> LocalObservations {
	observe_with(root, &Baseline::default(), paths)
}

fn observe_with(root: &Path, baseline: &Baseline, paths: &[&str]) -> LocalObservations {
	observed_or_whole(root, baseline, paths).expect("a dirty set the caps narrow a pass to")
}

/// [`observe_with`], or the reason the pass reads the local side whole instead.
fn observed_or_whole(
	root: &Path,
	baseline: &Baseline,
	paths: &[&str],
) -> Result<LocalObservations, FullPassReason> {
	let dirty: BTreeSet<String> = paths.iter().map(|path| (*path).to_owned()).collect();
	observe_local(
		root,
		PairRoot::Directory,
		baseline,
		IgnoreRules::default(),
		&RuleFiles::Read,
		&dirty,
	)
	.map(|(observed, _)| observed)
}

fn sorted_nodes(scan: &LocalScan) -> Vec<String> {
	let mut paths: Vec<String> = scan
		.nodes
		.whole()
		.paths()
		.map(|path| path.into_owned())
		.collect();
	paths.sort_unstable();
	paths
}

fn keys(out: &LocalObservations) -> Vec<&str> {
	out.observed.keys().map(String::as_str).collect()
}

/// A dirty rule file is observed as its DIRECTORY, and that directory answers for the entries
/// under it — including a dirty sibling whose name sorts before `.filenignore`, which the set
/// hands over first. Two observations that nest would break the rule `derive::merge_local` and
/// `engine::assembly_bounds` are written against.
#[test]
fn a_dirty_rule_file_is_observed_as_the_directory_that_holds_its_siblings() {
	let root = temp_root();
	fs::create_dir(root.join("a")).unwrap();
	// `!` (0x21) sorts before `.` (0x2E), so this leaf comes out of the set first.
	fs::write(root.join("a").join("!x.txt"), b"x").unwrap();
	fs::write(root.join("a").join(FILENIGNORE), "*.log\n").unwrap();

	let out = observe(&root, &["a/!x.txt", &format!("a/{FILENIGNORE}")]);

	assert_eq!(
		keys(&out),
		vec!["a"],
		"the rule file's directory answers for the sibling too"
	);

	fs::remove_dir_all(&root).ok();
}

/// The information a missing ancestor carries is "this ancestor is gone", not "this leaf is
/// gone": every dirty path below it collapses onto the ancestor, and the rows under it are the
/// caller's to expand.
#[test]
fn a_missing_ancestor_collapses_the_entry_to_that_ancestor() {
	let root = temp_root();
	fs::create_dir(root.join("a")).unwrap();

	let out = observe(&root, &["a/b/c.txt", "a/b/d.txt"]);

	assert!(out.complete, "{:?}", out.errors);
	assert_eq!(
		keys(&out),
		vec!["a/b"],
		"two leaves under one gone directory are one observation"
	);
	let LocalObservation::Absent(absence) = &out.observed["a/b"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		absence.path(),
		"a/b",
		"the evidence names the path whose own stat answered NotFound"
	);
	assert!(
		out.siblings.contains_key("a"),
		"the directory it was in is read for the collision check: {:?}",
		out.siblings
	);

	fs::remove_dir_all(&root).ok();
}

/// The NotFound split: on a path, it is a deletion; on the pair root, it is a removed volume
/// answering for every path under it, which is missing evidence and no deletion at all.
#[test]
fn a_missing_path_is_absent_but_a_missing_root_is_only_incomplete() {
	let root = temp_root();
	fs::write(root.join("keep.txt"), b"k").unwrap();

	let out = observe(&root, &["gone.txt"]);
	assert!(out.complete, "{:?}", out.errors);
	assert!(
		matches!(out.observed["gone.txt"], LocalObservation::Absent(_)),
		"{:?}",
		out.observed
	);

	let absent_root =
		std::env::temp_dir().join(format!("filen_observe_no_root_{}", Uuid::new_v4()));
	let out = observe(&absent_root, &["gone.txt"]);
	assert!(
		!out.complete,
		"a missing root is missing evidence, not a tree of deletions"
	);
	assert!(out.observed.is_empty(), "{:?}", out.observed);
	assert_eq!(out.errors.len(), 1, "{:?}", out.errors);

	fs::remove_dir_all(&root).ok();
}

/// The rules deciding a path at depth are the ones a whole-tree walk would have had in force
/// there: every ancestor's `.filenignore`, down to the path's own directory. An ignored
/// directory takes the entry with it, as git's own rule has it.
#[test]
fn the_rules_in_force_at_depth_are_the_ancestors_own_rule_files() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
	let sub = root.join("deep").join("sub");
	fs::create_dir_all(&sub).unwrap();
	fs::write(root.join("deep").join(FILENIGNORE), "hide/\n").unwrap();
	fs::create_dir(root.join("deep").join("hide")).unwrap();
	fs::write(root.join("deep").join("hide").join("y.txt"), b"y").unwrap();
	fs::write(sub.join(FILENIGNORE), "keep.bin\n").unwrap();
	fs::write(sub.join("x.log"), b"l").unwrap();
	fs::write(sub.join("keep.bin"), b"b").unwrap();
	fs::write(sub.join("x.txt"), b"t").unwrap();

	let out = observe(
		&root,
		&[
			"deep/hide/y.txt",
			"deep/sub/keep.bin",
			"deep/sub/x.log",
			"deep/sub/x.txt",
		],
	);

	assert!(out.complete, "{:?}", out.errors);
	let LocalObservation::Hidden(decision) = &out.observed["deep/sub/x.log"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(decision.pattern, "*.log", "the root's own rule file");
	let LocalObservation::Hidden(decision) = &out.observed["deep/sub/keep.bin"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		decision.pattern, "keep.bin",
		"the rule file of the path's OWN directory, three levels down"
	);
	let LocalObservation::Hidden(decision) = &out.observed["deep/hide"] else {
		panic!(
			"a mid-depth rule file hides the directory, and the entry collapses onto it: {:?}",
			out.observed
		);
	};
	assert_eq!(decision.pattern, "hide/");
	assert!(
		!out.observed.contains_key("deep/hide/y.txt"),
		"{:?}",
		out.observed
	);
	let LocalObservation::File {
		node,
		name_rejection,
	} = &out.observed["deep/sub/x.txt"]
	else {
		panic!("{:?}", out.observed);
	};
	assert!(node.content_hash.is_some(), "a present file is hashed");
	assert!(name_rejection.is_none());

	fs::remove_dir_all(&root).ok();
}

/// A pair that only pulls reads its local root file in a change-scoped pass as a whole one does:
/// its rules decide the dirty paths, and the file itself is no item of the observation.
#[test]
fn a_pulling_pairs_local_root_rule_file_decides_a_scoped_observation() {
	let root = temp_root();
	fs::write(root.join(FILENIGNORE), ".*\n!/.filenignore\n").unwrap();
	fs::write(root.join(".env"), b"e").unwrap();
	fs::write(root.join("a.txt"), b"a").unwrap();
	let dirty: BTreeSet<String> = [".env", "a.txt"].map(str::to_owned).into();
	let rule_files = RuleFiles::Only {
		synced: BTreeSet::new(),
		local_root: Some(UnrecordedRoot { remote_copy: None }),
	};

	let (out, rules) = observe_local(
		&root,
		PairRoot::Directory,
		&Baseline::default(),
		IgnoreRules::default(),
		&rule_files,
		&dirty,
	)
	.expect("a dirty set the caps narrow a pass to");

	assert!(out.complete, "{:?}", out.errors);
	let LocalObservation::Hidden(decision) = &out.observed[".env"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(decision.pattern, ".*");
	assert!(matches!(
		out.observed["a.txt"],
		LocalObservation::File { .. }
	));
	assert!(
		rules.decide(FILENIGNORE, false).is_some(),
		"the file keeps itself out"
	);

	fs::remove_dir_all(&root).ok();
}

/// A directory-level event means walk that directory — what makes FSEvents' coalescing safe.
/// What the walk pruned is named, so a row under it is not read as absent.
#[test]
fn a_dirty_directory_is_walked_and_names_what_its_walk_pruned() {
	let root = temp_root();
	fs::create_dir_all(root.join("d").join("inner")).unwrap();
	fs::write(root.join("d").join("inner").join("a.txt"), b"a").unwrap();
	fs::write(root.join("d").join(".DS_Store"), b"x").unwrap();
	// Tracked, so the default rule's hit is a root the walk records rather than noise.
	let baseline = Baseline::from_rows([row("d/.DS_Store", NodeKind::File)]);

	let out = observe_with(&root, &baseline, &["d", "d/inner/a.txt"]);

	assert!(out.complete, "{:?}", out.errors);
	assert_eq!(
		keys(&out),
		vec!["d"],
		"the walk of the directory answers for everything under it"
	);
	let LocalObservation::Dir(scan) = &out.observed["d"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		sorted_nodes(scan),
		vec!["d", "d/inner", "d/inner/a.txt"],
		"the start directory is a node, and the keys are root-relative"
	);
	assert_eq!(
		out.observed["d"]
			.uncovered_roots()
			.map(String::as_str)
			.collect::<Vec<_>>(),
		vec!["d/.DS_Store"],
		"the walk pruned it, so its row is not absent for want of a node"
	);

	fs::remove_dir_all(&root).ok();
}

/// The sibling set folds with the engine's own key, over NFC: globset's case-insensitive flag
/// folds ASCII only, and macOS hands names back decomposed.
#[test]
fn the_sibling_set_folds_non_ascii_names_the_way_the_engine_does() {
	let root = temp_root();
	fs::create_dir(root.join("dir")).unwrap();
	fs::write(root.join("dir").join("Ä.txt"), b"a").unwrap();
	fs::write(root.join("dir").join("e\u{301}clair.txt"), b"e").unwrap();

	let out = observe(&root, &["dir/Ä.txt"]);

	assert!(out.complete, "{:?}", out.errors);
	let folded = &out.siblings["dir"];
	assert_eq!(
		folded
			.get(&collision_key("ä.txt"))
			.map(|names| names.iter().map(String::as_str).collect::<Vec<_>>()),
		Some(vec!["Ä.txt"]),
		"an ASCII-only fold would leave Ä alone: {folded:?}"
	);
	assert!(
		folded.contains_key("éclair.txt"),
		"a decomposed name is keyed as its composed form: {folded:?}"
	);

	fs::remove_dir_all(&root).ok();
}

/// A rule file's own change rewrites what is hidden below its directory, so the DIRECTORY is
/// what has to be re-observed — with the new rules in force.
#[test]
fn a_dirty_rule_file_re_observes_its_directory() {
	let root = temp_root();
	fs::create_dir(root.join("d")).unwrap();
	fs::write(root.join("d").join(FILENIGNORE), "*.tmp\n").unwrap();
	fs::write(root.join("d").join("a.txt"), b"a").unwrap();
	fs::write(root.join("d").join("b.tmp"), b"b").unwrap();

	let out = observe(&root, &[&format!("d/{FILENIGNORE}")]);

	assert!(out.complete, "{:?}", out.errors);
	assert_eq!(keys(&out), vec!["d"]);
	let LocalObservation::Dir(scan) = &out.observed["d"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		sorted_nodes(scan),
		vec!["d", "d/.filenignore", "d/a.txt"],
		"the file's own rules decide the walk it triggered"
	);
	assert_eq!(scan.ignored.keys().collect::<Vec<_>>(), vec!["d/b.tmp"]);

	fs::remove_dir_all(&root).ok();
}

/// An observed file keeps the nanoseconds its millisecond mtime drops, as the scan does.
#[test]
fn a_present_file_keeps_the_nanoseconds_its_millisecond_mtime_drops() {
	let root = temp_root();
	let file = root.join("a.txt");
	fs::write(&file, b"hello").unwrap();
	fs::File::options()
		.write(true)
		.open(&file)
		.unwrap()
		.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(1_234_600_000))
		.unwrap();

	let out = observe_with(&root, &Baseline::default(), &["a.txt"]);

	let LocalObservation::File { node, .. } = &out.observed["a.txt"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		i128::from(node.mtime_millis) * 1_000_000 + i128::from(node.mtime_nanos),
		1_234_600_000
	);

	fs::remove_dir_all(&root).ok();
}

/// A present file carries the whole-tree walk's own fast-path: unchanged `(size, mtime)` reuses
/// the baseline's hash instead of re-reading the file.
#[test]
fn a_present_file_carries_the_fast_paths_hash() {
	let root = temp_root();
	let file = root.join("a.txt");
	fs::write(&file, b"hello").unwrap();
	let mtime = FilenMetaExt::modified(&fs::metadata(&file).unwrap()).timestamp_millis();
	let sentinel = Blake3Hash::from([0xAB; 32]);
	let baseline = Baseline::from_rows([BaselineEntry {
		content_hash: Some(sentinel),
		size: Some(5),
		local_mtime: Some(mtime),
		..row("a.txt", NodeKind::File)
	}]);

	let out = observe_with(&root, &baseline, &["a.txt"]);

	let LocalObservation::File { node, .. } = &out.observed["a.txt"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		node.content_hash,
		Some(sentinel),
		"unchanged (size, mtime) reuses the baseline hash — no re-hash"
	);
	assert_eq!(node.size, 5);
	assert_eq!(node.mtime_millis, mtime);

	fs::remove_dir_all(&root).ok();
}

/// An ancestor stat'd as a directory for an earlier dirty path and replaced by a file since
/// fails the next stat below it with ENOTDIR: that directory changed under the observation, as
/// one the walk finds replaced does. The path has no reading, so the observation is partial, and
/// nothing is reported. Called directly: the swap has to land between two dirty paths, which
/// nothing outside the observation can time.
// Windows answers a file standing in a path with NotFound, not ENOTDIR.
#[cfg(unix)]
#[test]
fn an_ancestor_replaced_by_a_file_leaves_the_observation_incomplete_and_unreported() {
	let root = temp_root();
	fs::write(root.join("a"), b"was a directory").unwrap();
	let mut out = LocalObservations {
		observed: BTreeMap::new(),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		gaps: ScanGaps::default(),
		errors: Vec::new(),
	};
	let mut walked = BTreeSet::from(["a".to_owned()]);

	let ancestors = walk_ancestors(
		&root,
		"a/b/c.txt",
		&mut IgnoreRules::default(),
		&RuleFiles::Read,
		&mut walked,
		&mut out,
	);

	assert!(matches!(ancestors, Ancestors::Unreadable));
	assert!(!out.complete);
	assert!(out.errors.is_empty(), "{:?}", out.errors);
	assert_eq!(walked, BTreeSet::from(["a".to_owned()]));

	fs::remove_dir_all(&root).ok();
}

/// A read that fails for any reason OTHER than NotFound could have hidden anything: it makes the
/// observation incomplete and mints no absence.
#[cfg(unix)]
#[test]
fn a_permission_denial_is_missing_evidence_not_an_absence() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let locked = root.join("locked");
	fs::create_dir_all(locked.join("sub")).unwrap();
	fs::write(locked.join("a.txt"), b"a").unwrap();
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

	let out = observe(&root, &["locked/a.txt", "locked/sub/b.txt"]);
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

	assert!(
		!out.complete,
		"a directory that cannot be read may hide anything"
	);
	assert!(
		out.observed.is_empty(),
		"a permission denial is not an absence: {:?}",
		out.observed
	);
	assert_eq!(out.errors.len(), 2, "{:?}", out.errors);
	let denied = |rel_path: &str| ScanGap {
		rel_path: rel_path.to_owned(),
		cause: ScanGapCause::Unreadable {
			kind: ErrorKind::PermissionDenied,
		},
	};
	assert_eq!(
		out.gaps.clone().into_parts(),
		(vec![denied("locked/a.txt"), denied("locked/sub")], 2),
		"each read the locked directory refused names its path"
	);

	fs::remove_dir_all(&root).ok();
}

/// A `Synced` file row recording `file` as it is on disk, size and mtime, with `hash`.
fn stamped(rel_path: &str, file: &Path, hash: Blake3Hash) -> BaselineEntry {
	let metadata = fs::metadata(file).unwrap();
	BaselineEntry {
		content_hash: Some(hash),
		size: Some(metadata.len()),
		local_mtime: Some(FilenMetaExt::modified(&metadata).timestamp_millis()),
		..row(rel_path, NodeKind::File)
	}
}

/// The hash no read of `x` or `y` produces, which the rows of a renamed directory record for it.
fn sentinel(name: &str) -> Blake3Hash {
	Blake3Hash::from([if name == "x" { 0xA1 } else { 0xB2 }; 32])
}

/// A directory renamed from `from` to `to`: `to/x` and `to/y` on disk, and the rows `from` left
/// — a `Synced` directory and its two files, each recording its file's size and mtime as they are
/// on disk and its [`sentinel`]. A sentinel in the observation was taken from a row; any other
/// hash was read.
fn lay_out_rename(root: &Path, from: &str, to: &str) -> Vec<BaselineEntry> {
	fs::create_dir_all(root.join(to)).unwrap();
	let mut rows = vec![row(from, NodeKind::Dir)];
	for name in ["x", "y"] {
		let file = root.join(to).join(name);
		fs::write(&file, name).unwrap();
		rows.push(stamped(&format!("{from}/{name}"), &file, sentinel(name)));
	}
	rows
}

/// The hashes the walk of `to` gave `to/x` and `to/y`.
fn walked_hashes(out: &LocalObservations, to: &str) -> [Option<Blake3Hash>; 2] {
	let LocalObservation::Dir(scan) = &out.observed[to] else {
		panic!("{:?}", out.observed);
	};
	let nodes = scan.nodes.whole();
	["x", "y"].map(|name| {
		nodes
			.at(&format!("{to}/{name}"))
			.and_then(|node| node.content_hash)
	})
}

/// What observing the rename `a` -> `b` of [`lay_out_rename`] gives `b/x` and `b/y` once `change`
/// has edited its rows, its disk and its dirty set: the hashes the walk gave them, the hashes a
/// read gives them, and which of the two the walk met first.
fn observe_rename(
	change: impl FnOnce(&Path, &mut Vec<BaselineEntry>, &mut Vec<&'static str>),
) -> ([Option<Blake3Hash>; 2], [Blake3Hash; 2], String) {
	let root = temp_root();
	let mut rows = lay_out_rename(&root, "a", "b");
	let mut dirty = vec!["a", "b"];
	change(&root, &mut rows, &mut dirty);

	let out = observe_with(&root, &Baseline::from_rows(rows), &dirty);

	assert!(out.complete, "{:?}", out.errors);
	let walked = walked_hashes(&out, "b");
	let read = ["x", "y"].map(|name| hash_file(&root.join("b").join(name)).unwrap());
	// The walk lists a directory in the order `read_dir` hands it over.
	let first = fs::read_dir(root.join("b"))
		.unwrap()
		.map(|entry| entry.unwrap().file_name().into_string().unwrap())
		.find(|name| name == "x" || name == "y")
		.unwrap();
	fs::remove_dir_all(&root).ok();
	(walked, read, first)
}

/// A directory renamed within the pass takes each file's hash from the row the file left instead
/// of reading it, on the fast path's own equalities. Whichever end the dirty set hands over first:
/// the pairing takes its own `stat` of the source, so it does not wait for the loop to reach it.
#[test]
fn a_renamed_directory_takes_its_hashes_from_the_rows_it_left() {
	for (from, to) in [("a", "b"), ("b", "a")] {
		let root = temp_root();
		let baseline = Baseline::from_rows(lay_out_rename(&root, from, to));

		let out = observe_with(&root, &baseline, &[from, to]);

		assert!(out.complete, "{:?}", out.errors);
		assert!(
			matches!(out.observed[from], LocalObservation::Absent(_)),
			"{:?}",
			out.observed
		);
		assert_eq!(
			walked_hashes(&out, to),
			[Some(sentinel("x")), Some(sentinel("y"))],
			"{from} -> {to}: each file takes the hash its row recorded, unread"
		);

		fs::remove_dir_all(&root).ok();
	}
}

/// Everything the pairing cannot prove is read. A file whose row does not vouch for it is read on
/// its own, and the walk is paired once, at the first file it could not answer, so a file met
/// first that no row vouches for leaves the whole walk unpaired. A rename that cannot be proven
/// — the source still there, two sources, a shape the rows do not have, a destination the pair
/// already tracks — reads every file. So does one whose only gone candidate is a lookalike while
/// the real source's path holds something again: its rows vouch for the files too, and that
/// makes two stories whatever now stands there.
#[test]
fn a_renamed_directory_reads_what_it_cannot_prove() {
	let (walked, _, _) = observe_rename(|_, _, _| {});
	assert_eq!(
		walked,
		[Some(sentinel("x")), Some(sentinel("y"))],
		"the rename every case below changes one thing about"
	);

	/// The row `a/x` among `rows`.
	fn row_x(rows: &mut [BaselineEntry]) -> &mut BaselineEntry {
		rows.iter_mut().find(|row| row.rel_path == "a/x").unwrap()
	}
	/// `c`, dirty and gone, with rows that record the same sizes and mtimes as `a`'s but other
	/// hashes: the one gone directory the walk of `b` could be paired with, though it came
	/// from `a`.
	fn gone_lookalike(rows: &mut Vec<BaselineEntry>, dirty: &mut Vec<&'static str>) {
		let lookalike: Vec<BaselineEntry> = rows
			.iter()
			.map(|row| BaselineEntry {
				rel_path: row.rel_path.replacen('a', "c", 1),
				content_hash: row.content_hash.map(|_| Blake3Hash::from([0xCC; 32])),
				..row.clone()
			})
			.collect();
		rows.extend(lookalike);
		dirty.push("c");
	}
	type Change = fn(&Path, &mut Vec<BaselineEntry>, &mut Vec<&'static str>);

	let unvouched: [(&str, Change); 4] = [
		("the row's mtime is a millisecond off", |_, rows, _| {
			*row_x(rows).local_mtime.as_mut().unwrap() += 1;
		}),
		("the row's size is a byte off", |_, rows, _| {
			*row_x(rows).size.as_mut().unwrap() += 1;
		}),
		("the row records a conflict", |_, rows, _| {
			row_x(rows).state = BaselineState::Conflicted;
		}),
		("the row records no hash", |_, rows, _| {
			row_x(rows).content_hash = None;
		}),
	];
	for (case, change) in unvouched {
		let (walked, read, first) = observe_rename(change);
		assert_eq!(walked[0], Some(read[0]), "{case}: x is read");
		let y = if first == "y" { sentinel("y") } else { read[1] };
		assert_eq!(
			walked[1],
			Some(y),
			"{case}: y takes its row's hash only when the walk was paired at it, first"
		);
	}

	let unproven: [(&str, Change); 8] = [
		("the source is still there, a copy", |root, _, _| {
			fs::create_dir(root.join("a")).unwrap();
			for name in ["x", "y"] {
				fs::copy(root.join("b").join(name), root.join("a").join(name)).unwrap();
			}
		}),
		(
			"the source was made again after the rename, and a lookalike is gone",
			|root, rows, dirty| {
				fs::create_dir(root.join("a")).unwrap();
				fs::write(root.join("a").join("new"), "new").unwrap();
				gone_lookalike(rows, dirty);
			},
		),
		(
			"the gone lookalike was renamed into the source's place",
			|root, rows, dirty| {
				fs::create_dir(root.join("a")).unwrap();
				for name in ["x", "y"] {
					fs::write(root.join("a").join(name), format!("c{name}")).unwrap();
				}
				gone_lookalike(rows, dirty);
			},
		),
		(
			"a file stands where the source was, and a lookalike is gone",
			|root, rows, dirty| {
				fs::write(root.join("a"), "a").unwrap();
				gone_lookalike(rows, dirty);
			},
		),
		(
			"a second gone directory held the same files",
			|_, rows, dirty| {
				let twins: Vec<BaselineEntry> = rows
					.iter()
					.map(|row| BaselineEntry {
						rel_path: row.rel_path.replacen('a', "c", 1),
						..row.clone()
					})
					.collect();
				rows.extend(twins);
				dirty.push("c");
			},
		),
		(
			"the destination holds an entry the source did not",
			|root, _, _| {
				fs::create_dir(root.join("b").join("extra")).unwrap();
			},
		),
		(
			"the source held an entry the destination does not",
			|_, rows, _| {
				rows.push(BaselineEntry {
					content_hash: Some(Blake3Hash::from([0xD4; 32])),
					size: Some(1),
					local_mtime: Some(1),
					..row("a/z", NodeKind::File)
				});
			},
		),
		(
			"the destination is a directory the pair tracks",
			|_, rows, _| {
				rows.push(row("b", NodeKind::Dir));
			},
		),
	];
	for (case, change) in unproven {
		let (walked, read, _) = observe_rename(change);
		assert_eq!(walked, read.map(Some), "{case}: every file is read");
	}
}

/// The pair root's own walk is never paired: nothing is renamed into the root, and a root walk
/// that paired would read a gone directory's rows to no end. It reads its files, and it reads no
/// more rows with a gone directory dirty beside it than without one.
#[test]
fn the_pair_root_walk_never_binds() {
	let observe_root = |dirty: &[&str]| {
		let root = temp_root();
		let baseline = Baseline::from_rows(lay_out_rename(&root, "a", "b"));
		let out = observe_with(&root, &baseline, dirty);
		let LocalObservation::Dir(scan) = &out.observed[""] else {
			panic!("{:?}", out.observed);
		};
		let walked = scan
			.nodes
			.whole()
			.at("b/x")
			.and_then(|node| node.content_hash);
		let read = hash_file(&root.join("b").join("x")).unwrap();
		fs::remove_dir_all(&root).ok();
		(walked == Some(read), baseline.reads_for_test())
	};

	let (_, alone) = observe_root(&[""]);
	let (read, beside) = observe_root(&["", "a"]);

	assert!(read, "the root walk reads its files");
	assert_eq!(
		beside, alone,
		"(statements, rows): a gone directory beside the root walk costs it nothing"
	);
}

/// A renamed directory's `.filenignore` is read, never taken from the row it left: what it holds
/// decides what the walk hides. The files beside it still take theirs.
#[test]
fn a_renamed_directory_still_reads_its_rule_file() {
	let root = temp_root();
	let mut rows = lay_out_rename(&root, "a", "b");
	let rule_file = root.join("b").join(FILENIGNORE);
	fs::write(&rule_file, "*.tmp\n").unwrap();
	rows.push(stamped(
		&format!("a/{FILENIGNORE}"),
		&rule_file,
		Blake3Hash::from([0xC3; 32]),
	));

	let out = observe_with(&root, &Baseline::from_rows(rows), &["a", "b"]);

	assert!(out.complete, "{:?}", out.errors);
	let LocalObservation::Dir(scan) = &out.observed["b"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(
		scan.nodes
			.whole()
			.at(&format!("b/{FILENIGNORE}"))
			.and_then(|node| node.content_hash),
		Some(hash_file(&rule_file).unwrap()),
		"the rule file is read"
	);
	assert_eq!(
		walked_hashes(&out, "b"),
		[Some(sentinel("x")), Some(sentinel("y"))],
		"and the files beside it still take their rows' hashes"
	);

	fs::remove_dir_all(&root).ok();
}

/// A rule file that cannot be read blocks its renamed directory exactly as it blocks any: it
/// bails, and the walk that stands is the one a directory nobody paired gets.
#[cfg(unix)]
#[test]
fn an_unreadable_rule_file_blocks_a_renamed_directory_as_it_blocks_any() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let mut rows = lay_out_rename(&root, "a", "b");
	let rule_file = root.join("b").join(FILENIGNORE);
	fs::write(&rule_file, "*.tmp\n").unwrap();
	rows.push(stamped(
		&format!("a/{FILENIGNORE}"),
		&rule_file,
		Blake3Hash::from([0xC3; 32]),
	));
	fs::set_permissions(&rule_file, fs::Permissions::from_mode(0o000)).unwrap();

	let renamed = observe_with(&root, &Baseline::from_rows(rows), &["a", "b"]);
	let unpaired = observe_with(&root, &Baseline::default(), &["b"]);
	fs::set_permissions(&rule_file, fs::Permissions::from_mode(0o644)).unwrap();

	let (LocalObservation::Dir(renamed), LocalObservation::Dir(unpaired)) =
		(&renamed.observed["b"], &unpaired.observed["b"])
	else {
		panic!("{:?} / {:?}", renamed.observed, unpaired.observed);
	};
	assert_eq!(
		renamed.ignore_blocked,
		BTreeSet::from(["b".to_string()]),
		"the directory is blocked: {:?}",
		renamed.errors
	);
	assert!(
		!renamed.nodes.whole().holds(&format!("b/{FILENIGNORE}")),
		"the rule file bailed rather than taking its row's hash"
	);
	assert_eq!(
		renamed.nodes, unpaired.nodes,
		"the walk that stands is the one nobody paired"
	);
	assert_eq!(renamed.ignore_blocked, unpaired.ignore_blocked);
	assert_eq!(renamed.complete, unpaired.complete);
	assert_eq!(renamed.errors.len(), unpaired.errors.len());

	fs::remove_dir_all(&root).ok();
}

/// Taking a row's hash never opens the file, so an ordinary file this process cannot read takes
/// its row's hash in a renamed directory exactly as it does in place: the stamps vouch for it, and
/// the read that would have reported it never happens.
#[cfg(unix)]
#[test]
fn an_unreadable_file_in_a_renamed_directory_takes_its_rows_hash() {
	use std::os::unix::fs::PermissionsExt;

	let root = temp_root();
	let baseline = Baseline::from_rows(lay_out_rename(&root, "a", "b"));
	let locked = root.join("b").join("x");
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

	let out = observe_with(&root, &baseline, &["a", "b"]);
	fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();

	// A failed read inside the walk is reported in the walk's own scan, not beside it.
	let LocalObservation::Dir(scan) = &out.observed["b"] else {
		panic!("{:?}", out.observed);
	};
	assert!(out.complete, "{:?}", scan.errors);
	assert!(
		out.errors.is_empty() && scan.errors.is_empty(),
		"nothing was read to fail: {:?} / {:?}",
		out.errors,
		scan.errors
	);
	assert_eq!(
		walked_hashes(&out, "b"),
		[Some(sentinel("x")), Some(sentinel("y"))],
		"the unreadable file takes its row's hash, unopened"
	);

	fs::remove_dir_all(&root).ok();
}

/// A `Synced` file row at `rel_path` recording a one-byte file as remote item `n`, both sides
/// on record, with nothing on disk behind it.
fn recorded_file(rel_path: &str, n: usize) -> BaselineEntry {
	BaselineEntry {
		remote_uuid: Some(Uuid::from_u128(10_000 + n as u128)),
		content_hash: Some(Blake3Hash::from([3; 32])),
		size: Some(1),
		local_mtime: Some(1),
		remote_modified: Some(1),
		..row(rel_path, NodeKind::File)
	}
}

/// The rows of a `Synced` directory at `dir` holding `files` files, both sides on record.
fn recorded_dir(dir: &str, files: usize) -> Vec<BaselineEntry> {
	let dir_row = BaselineEntry {
		remote_uuid: Some(Uuid::from_u128(1)),
		..row(dir, NodeKind::Dir)
	};
	std::iter::once(dir_row)
		.chain((0..files).map(|n| recorded_file(&format!("{dir}/{n:05}"), n)))
		.collect()
}

/// A path observed gone or hidden takes every row under it out of the derived local map, so
/// past the cap the observation is a whole read instead — whether the dirty path is the
/// directory itself or something under it, and whichever way the rules hide it. At the cap it
/// is still observed.
#[test]
fn a_path_gone_or_hidden_with_more_rows_under_it_than_the_cap_is_a_whole_read() {
	for (label, hidden, dirty) in [
		("gone", false, "d"),
		("gone above the dirty path", false, "d/00000"),
		("hidden", true, "d"),
	] {
		for (files, expected) in [
			(SUBTREE_CAP, Ok("d")),
			(SUBTREE_CAP + 1, Err(FullPassReason::LocalOverflow)),
		] {
			let root = temp_root();
			if hidden {
				fs::create_dir(root.join("d")).unwrap();
				fs::write(root.join(FILENIGNORE), "d/\n").unwrap();
			}
			let baseline = Baseline::from_rows(recorded_dir("d", files));

			let observed = observed_or_whole(&root, &baseline, &[dirty]);

			assert_eq!(
				observed.as_ref().map(keys).map_err(|reason| *reason),
				expected.map(|at| vec![at]),
				"{label}, {files} row(s) under it"
			);
			fs::remove_dir_all(&root).ok();
		}
	}
}

/// A walk holds a node for every entry it lists, so one that lists more than the cap below the
/// directory it walks stops there and the pass reads the local side whole. At the cap the walk
/// stands, the directory itself among its nodes.
#[test]
fn a_walk_listing_more_entries_than_the_cap_is_a_whole_read() {
	let root = temp_root();
	fs::create_dir(root.join("d")).unwrap();
	for n in 0..SUBTREE_CAP {
		fs::write(root.join("d").join(format!("{n:05}")), b"").unwrap();
	}

	let out = observe(&root, &["d"]);
	let LocalObservation::Dir(scan) = &out.observed["d"] else {
		panic!("{:?}", out.observed);
	};
	assert_eq!(sorted_nodes(scan).len(), SUBTREE_CAP + 1);

	fs::write(root.join("d").join("one-more"), b"").unwrap();
	assert_eq!(
		observed_or_whole(&root, &Baseline::default(), &["d"]).map(|out| keys(&out).len()),
		Err(FullPassReason::LocalOverflow)
	);
	fs::remove_dir_all(&root).ok();
}

/// A walk paired with a directory past the cap holds neither that directory's rows nor much of a
/// walk of its own: the pairing counts the rows instead of reading them, and the walk stops at
/// the next entry it lists. The renamed directory sorts first, so the source's own absence is
/// not what stops the observation.
#[test]
fn a_walk_paired_with_a_directory_past_the_cap_stops_without_reading_its_rows() {
	const WALKED: usize = 8_192;
	let root = temp_root();
	fs::create_dir(root.join("b")).unwrap();
	let mut rows = vec![row("z", NodeKind::Dir)];
	for n in 0..=SUBTREE_CAP {
		let name = format!("{n:05}");
		if n < WALKED {
			let file = root.join("b").join(&name);
			fs::write(&file, b"x").unwrap();
			rows.push(stamped(&format!("z/{name}"), &file, sentinel("x")));
		} else {
			rows.push(recorded_file(&format!("z/{name}"), n));
		}
	}
	let baseline = Baseline::from_rows(rows);

	let (observed, peak) =
		alloc_meter::peak_bytes(|| observed_or_whole(&root, &baseline, &["b", "z"]).map(|_| ()));

	assert_eq!(observed, Err(FullPassReason::LocalOverflow));
	assert!(
		peak < 512 << 10,
		"the observation peaked at {peak} bytes: it read the {} rows it was paired with, or \
			 walked on through the {WALKED} files",
		SUBTREE_CAP + 1
	);
	fs::remove_dir_all(&root).ok();
}

/// At the cap the pairing binds and the walk stands: every file takes the hash its row recorded,
/// unread, and the directory it was paired with is observed gone.
#[test]
fn a_walk_paired_with_a_directory_at_the_cap_takes_its_rows_hashes() {
	let root = temp_root();
	fs::create_dir(root.join("b")).unwrap();
	let mut rows = vec![row("z", NodeKind::Dir)];
	for n in 0..SUBTREE_CAP {
		let name = format!("{n:05}");
		let file = root.join("b").join(&name);
		fs::write(&file, b"x").unwrap();
		rows.push(stamped(&format!("z/{name}"), &file, sentinel("x")));
	}

	let out = observe_with(&root, &Baseline::from_rows(rows), &["b", "z"]);

	assert!(out.complete, "{:?}", out.errors);
	assert!(
		matches!(out.observed["z"], LocalObservation::Absent(_)),
		"{:?}",
		keys(&out)
	);
	let LocalObservation::Dir(scan) = &out.observed["b"] else {
		panic!("{:?}", keys(&out));
	};
	let nodes = scan.nodes.whole();
	assert_eq!(
		(0..SUBTREE_CAP)
			.filter(|n| {
				nodes
					.at(&format!("b/{n:05}"))
					.and_then(|node| node.content_hash)
					== Some(sentinel("x"))
			})
			.count(),
		SUBTREE_CAP,
		"each file takes the hash its row recorded"
	);
	fs::remove_dir_all(&root).ok();
}

/// A directory adopted at a mode switch, with more adopted rows under it than the cap, is in the
/// dirty set of every pass whether or not anything changed, and a whole read would not retire it.
/// So with nothing announced the pass stays narrowed: its rows are not counted toward the cap,
/// whether the directory is gone from disk (the destination is the remote) or still there and
/// walked (the destination is the disk). The synced rows under it still count: as many as the
/// cap leave it narrowed, one more is a whole read.
#[test]
fn a_standing_adopted_directory_past_the_cap_leaves_the_pass_narrowed() {
	let adopted = |rel_path: &str, kind| BaselineEntry {
		state: BaselineState::Adopted,
		..row(rel_path, kind)
	};
	for (on_disk, synced, expected) in [
		(false, 0, Ok(("archive".to_owned(), true))),
		(true, 0, Ok(("archive".to_owned(), true))),
		(false, SUBTREE_CAP, Ok(("archive".to_owned(), true))),
		(false, SUBTREE_CAP + 1, Err(FullPassReason::LocalOverflow)),
	] {
		let root = temp_root();
		let mut rows = vec![adopted("archive", NodeKind::Dir)];
		if on_disk {
			fs::create_dir(root.join("archive")).unwrap();
		}
		for n in 0..=SUBTREE_CAP {
			let rel_path = format!("archive/{n:05}");
			if on_disk {
				fs::write(root.join(&rel_path), b"").unwrap();
			}
			rows.push(adopted(&rel_path, NodeKind::File));
		}
		rows.extend((0..synced).map(|n| recorded_file(&format!("archive/s{n:05}"), n)));
		let baseline = Baseline::from_rows(rows);
		let dirty = from_baseline(&baseline, BTreeSet::new()).dirty;
		assert_eq!(dirty.len(), SUBTREE_CAP + 2, "every adopted row is dirty");

		let observed = observe_local(
			&root,
			PairRoot::Directory,
			&baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&dirty,
		)
		.map(|(out, _)| (keys(&out).join(","), out.complete));

		assert_eq!(
			observed, expected,
			"on disk: {on_disk}, {synced} synced row(s) beside the adopted ones"
		);
		fs::remove_dir_all(&root).ok();
	}
}
