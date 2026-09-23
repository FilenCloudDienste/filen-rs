//! The local half of a change-scoped pass: re-observe the paths the changelist names, instead of
//! walking the whole tree.
//!
//! The rule this module exists to keep is that absence is only ever produced by a `stat` on the
//! path itself or on one of its ancestors. A lost event may DELAY a deletion; it may never invent
//! one. So [`Absence`] has a private field, no constructor, no `Default` and no `From`: only the
//! `NotFound` arm of [`observe_local`]'s own `stat`, in this module, can mint one, and the assembly
//! that builds a pass's local map out of these observations cannot write one by hand. The remote
//! side's `changes::Gone` is the same shape for the same reason.
//!
//! What one dirty path costs: one `stat` per ancestor (shared across the dirty set), one `stat` on
//! the path itself, a `read_dir` of the directory it sits in for the folded sibling names, and —
//! when it is a directory — one [`scan_subtree`] walk of it. Walking the whole directory is what
//! makes a directory-level event safe to act on, which is what FSEvents coalesces bursts into.
//!
//! A path with NO observation, here or at an ancestor, is not absent: it is a path this pass got no
//! local evidence for (an unreadable ancestor, a directory replaced by a file, a file that vanished
//! between the `stat` and the hash, a socket where a row sits). Its baseline row is carried
//! unchanged and the next pass looks again.

use std::{
	collections::{BTreeMap, BTreeSet},
	io::ErrorKind,
	path::Path,
};

use unicode_normalization::UnicodeNormalization;

use super::{
	baseline::NodeKind,
	ignore::{IgnoreDecision, IgnoreRules, rule_file_dir},
	scan::{
		LocalNode, LocalScan, RuleFiles, ScanError, collision_key, fast_path_hash, hash_file,
		load_rule_file, name_rejection, record, scan_subtree,
	},
	tree::Baseline,
};
use crate::io::FilenMetaExt;

/// Every name one directory holds, folded by [`collision_key`]: the folded name, to the names that
/// folded onto it. More than one name under a key is a collision — a single local path cannot hold
/// both, and the engine refuses to reconcile a pair with one.
pub(super) type FoldedNames = BTreeMap<String, BTreeSet<String>>;

/// Evidence that nothing is at a path.
///
/// Mintable only inside this module (private field, no constructor), and only from a `stat` that
/// answered `NotFound` on that very path. See the module docs.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Absence {
	gone: String,
}

impl Absence {
	/// The path whose own `stat` answered `NotFound`. Equal to the key the observation is filed
	/// under: a dirty path whose ANCESTOR is the one that is gone collapses to that ancestor, since
	/// what was observed is "this ancestor is gone", not "this leaf is gone".
	pub(super) fn path(&self) -> &str {
		&self.gone
	}
}

/// What one re-observed path is.
#[derive(Debug)]
pub(super) enum LocalObservation {
	/// A file is there: its stat, hashed through the baseline fast-path exactly as the whole-tree
	/// walk hashes it, plus the validator's reason where the remote would reject its name. A
	/// rejected name is still observed — omitting it would read downstream as a local deletion, so
	/// renaming a synced file into one would trash its remote copy.
	File {
		node: LocalNode,
		name_rejection: Option<String>,
	},
	/// A directory is there: the walk of it and everything under it, keyed against the pair root
	/// like a whole-tree scan's nodes, with that walk's own facts and errors.
	Dir(Box<LocalScan>),
	/// Nothing is there.
	Absent(Absence),
	/// The ignore rules hide this path, and so everything under it. NOT evidence of absence: what
	/// the rules hide is untracked by the rules' own path, never deleted by this one.
	Hidden(IgnoreDecision),
}

impl LocalObservation {
	/// The paths under this observation that its evidence does NOT cover, because the walk pruned
	/// them: an ignored entry, and a symlinked directory read under its real path. A row at or
	/// under one of these is not absent merely because no node was observed for it.
	///
	/// A rejected NAME is not one of them: those entries are observed like any other and their
	/// subtree is walked (`LocalScan::invalid_names`), they simply cannot be pushed.
	pub(super) fn uncovered_roots(&self) -> impl Iterator<Item = &String> {
		let scan = match self {
			Self::Dir(scan) => Some(scan),
			Self::File { .. } | Self::Absent(_) | Self::Hidden(_) => None,
		};
		scan.into_iter()
			.flat_map(|scan| scan.ignored.keys().chain(scan.aliased_dirs.keys()))
	}
}

/// What a change-scoped pass observed locally.
#[derive(Debug)]
pub(super) struct LocalObservations {
	/// One entry per re-observed path, keyed by the path the observation is ABOUT: the dirty path
	/// itself, or the nearest ancestor that is gone or hidden.
	///
	/// A dirty path with no entry here and none at an ancestor got no local evidence this pass (see
	/// the module docs). It is not absent.
	pub(super) observed: BTreeMap<String, LocalObservation>,
	/// The folded names of each directory an observation sits in (`""` is the pair root), for the
	/// per-directory collision check and for pairing a rename inside that directory. Raw: the rule
	/// files, the engine's own staging files and the quarantine bin are all names the filesystem
	/// would collide with, so they are all in it.
	pub(super) siblings: BTreeMap<String, FoldedNames>,
	/// Ancestor directories whose `.filenignore` could not be used, as
	/// [`LocalScan::ignore_blocked`] records them for a whole-tree walk. A dirty subtree's own
	/// blocked directories are in its [`LocalObservation::Dir`] scan.
	pub(super) ignore_blocked: BTreeSet<String>,
	/// `false` if anything could have been MISSED: an ancestor that could not be read, a failed
	/// hash, a subtree walk that came back incomplete, or the pair root gone. The absence a
	/// complete observation produces is what the pass may act on; an incomplete one holds
	/// deletions, exactly as an incomplete scan does.
	pub(super) complete: bool,
	/// Every error, in the shape a whole-tree scan reports them. A
	/// [`DuplicateName`](ScanError::DuplicateName) can only come from a subtree walk, inside its
	/// own [`LocalObservation::Dir`] scan.
	pub(super) errors: Vec<ScanError>,
}

/// What the ancestors of one dirty path said.
enum Ancestors<'p> {
	/// Every ancestor is a readable directory the rules do not hide.
	Walked,
	/// This ancestor is gone. The entry collapses to it.
	Gone(&'p str),
	/// The rules hide this ancestor, and with it everything below.
	Hidden(&'p str, IgnoreDecision),
	/// An ancestor could not be read, or is not a directory: no evidence for this path.
	Unreadable,
}

/// Re-observe `dirty` under `root` — the local half of plan 3.4's step 2.
///
/// `dirty` is the change-scoped pass's local path set (`changes::LocalDirty`), root-relative and
/// keyed exactly as a scan keys its nodes. Sorted, so an ancestor is observed before anything under
/// it and its walk answers for the rest.
///
/// `rules` comes in as the pass's own levels (user level, and the remote rule files when the remote
/// is the source of truth) and comes back carrying every `.filenignore` this observation read: the
/// ancestors of each dirty path, top down, and whatever its subtree walks found. Loading the
/// ancestors first is what makes the rules in force the same ones a whole-tree walk would have
/// applied at that depth.
pub(super) fn observe_local(
	root: &Path,
	baseline: &Baseline,
	mut rules: IgnoreRules,
	rule_files: &RuleFiles,
	dirty: &BTreeSet<String>,
) -> (LocalObservations, IgnoreRules) {
	let mut out = LocalObservations {
		observed: BTreeMap::new(),
		siblings: BTreeMap::new(),
		ignore_blocked: BTreeSet::new(),
		complete: true,
		errors: Vec::new(),
	};
	// Nothing below the root is evidence of anything unless the root is there: a removed volume, a
	// share that dropped or a lazily unmounted tree answers `NotFound` for every path under it, and
	// reading that as a tree of deletions is what the walk's own closing stat guards against.
	if let Err(source) = std::fs::metadata(root) {
		out.complete = false;
		record(
			&mut out.errors,
			root,
			ScanError::Io {
				rel_path: String::new(),
				source,
			},
		);
		return (out, rules);
	}
	if rule_files.reads("") {
		load_rule_file(
			root,
			"",
			root,
			&mut rules,
			&mut out.ignore_blocked,
			&mut out.errors,
		);
	}
	// The dirty set walks the same directories over and over; the cursor keeps the last one in hand.
	let mut rows = baseline.cursor();
	// Ancestors already stat'ed, found to be directories the rules do not hide, and whose own rule
	// file is loaded. Depth × |dirty| stats is the cost plan 3.4 budgets; this is what keeps it to
	// the distinct ancestors instead.
	let mut walked: BTreeSet<String> = BTreeSet::new();

	// A `.filenignore` decides what is hidden below its own directory, so what changed when it
	// changed is that DIRECTORY — the rules it holds are what the walk of it applies. (The remote
	// side re-derives the same directory for the same reason.)
	//
	// Rewritten into the set BEFORE it is walked, not while: a directory is an ancestor of the very
	// entries it holds, and a rule file sorts after siblings whose names start below `.`, so
	// rewriting in the loop can file the directory AFTER something under it. `covered` only looks
	// upward, so both would stand — and two nested observations are what `merge_local` and
	// `assembly_accounted` are both written against.
	let dirty: BTreeSet<&str> = dirty
		.iter()
		.map(|path| rule_file_dir(path).unwrap_or(path))
		.collect();
	for path in dirty {
		if covered(&out.observed, path) {
			continue;
		}
		match walk_ancestors(root, path, &mut rules, rule_files, &mut walked, &mut out) {
			Ancestors::Walked => {}
			Ancestors::Gone(at) => {
				let absence = Absence {
					gone: at.to_owned(),
				};
				insert(&mut out, root, at, LocalObservation::Absent(absence));
				continue;
			}
			Ancestors::Hidden(at, decision) => {
				insert(&mut out, root, at, LocalObservation::Hidden(decision));
				continue;
			}
			Ancestors::Unreadable => continue,
		}

		let path_buf = if path.is_empty() {
			root.to_path_buf()
		} else {
			root.join(path)
		};
		// Follows symlinks, so an entry is classified by its target — the walk classifies it the
		// same way.
		let stat = std::fs::metadata(&path_buf);
		// A path that is gone says nothing about which kind it was, and the rules can hide a
		// directory where they would not hide a file of the same name. The row it had is the best
		// answer there is; a path with no row and nothing on disk is hidden either way or neither.
		let is_dir = match &stat {
			Ok(metadata) => metadata.is_dir(),
			Err(_) => rows.get(path).is_some_and(|row| row.kind == NodeKind::Dir),
		};
		// Before the absence below, not after: a deletion under a path the rules hide is the
		// untrack path's business, never a propagated one.
		if !path.is_empty()
			&& let Some(hit) = rules.decide(path, is_dir)
		{
			insert(&mut out, root, path, LocalObservation::Hidden(hit.into()));
			continue;
		}
		match stat {
			Ok(_) if is_dir => {
				let (scan, carried) = scan_subtree(root, path, baseline, rules, rule_files);
				rules = carried;
				out.complete &= scan.complete;
				insert(&mut out, root, path, LocalObservation::Dir(Box::new(scan)));
			}
			Ok(metadata) if metadata.is_file() => {
				let size = metadata.len();
				let mtime = FilenMetaExt::modified(&metadata).timestamp_millis();
				let content_hash = match fast_path_hash(rows.get(path).as_ref(), size, mtime) {
					Some(hash) => Some(hash),
					None => match hash_file(&path_buf) {
						Ok(hash) => Some(hash),
						// Gone between the stat and the hash: the race the walk answers by skipping
						// the entry. No node and no absence — this pass has no reading for the
						// path, and the next one takes it again.
						Err(source) if source.kind() == ErrorKind::NotFound => {
							tracing::debug!(
								"local observation of {}: {path:?} vanished after its stat",
								root.display()
							);
							continue;
						}
						Err(source) => {
							out.complete = false;
							record(
								&mut out.errors,
								root,
								ScanError::Io {
									rel_path: path.to_owned(),
									source,
								},
							);
							continue;
						}
					},
				};
				let node = LocalNode {
					rel_path: path.to_owned(),
					kind: NodeKind::File,
					size,
					mtime_millis: mtime,
					content_hash,
				};
				insert(
					&mut out,
					root,
					path,
					LocalObservation::File {
						node,
						name_rejection: name_rejection(path),
					},
				);
			}
			// Sockets, FIFOs and devices: not syncable, and the walk skips them too. A successful
			// stat is not evidence of absence, so the path simply has no reading this pass.
			Ok(_) => tracing::debug!(
				"local observation of {}: {path:?} is not a file or a directory",
				root.display()
			),
			Err(source) if source.kind() == ErrorKind::NotFound => {
				// The one place an absence is minted. The root's own disappearance is not this: it
				// is checked above and again below, because every path under a removed volume
				// answers `NotFound`.
				let absence = Absence {
					gone: path.to_owned(),
				};
				insert(&mut out, root, path, LocalObservation::Absent(absence));
			}
			Err(source) => {
				out.complete = false;
				record(
					&mut out.errors,
					root,
					ScanError::Io {
						rel_path: path.to_owned(),
						source,
					},
				);
			}
		}
	}

	// The root again, after the fact. A volume that went away while this ran answered `NotFound`
	// for every path under it, and each of those answers would otherwise stand as an absence. What
	// was observed present is still true; what was observed absent is not evidence any more.
	if let Err(source) = std::fs::metadata(root) {
		out.complete = false;
		record(
			&mut out.errors,
			root,
			ScanError::Io {
				rel_path: String::new(),
				source,
			},
		);
		out.observed
			.retain(|_, observation| !matches!(observation, LocalObservation::Absent(_)));
	}
	(out, rules)
}

/// Stat every ancestor of `path`, top down, loading each one's `.filenignore` before descending —
/// so the rules deciding `path` are the ones a whole-tree walk would have had in force there.
///
/// Stops at the first ancestor that answers: nothing under an ignored directory can be re-included
/// and its own rule file is never read (the prune a whole-tree walk does at the top-most hidden
/// directory), and nothing under a directory that is gone or unreadable can be observed at all.
fn walk_ancestors<'p>(
	root: &Path,
	path: &'p str,
	rules: &mut IgnoreRules,
	rule_files: &RuleFiles,
	walked: &mut BTreeSet<String>,
	out: &mut LocalObservations,
) -> Ancestors<'p> {
	for (cut, _) in path.match_indices('/') {
		let ancestor = &path[..cut];
		if walked.contains(ancestor) {
			continue;
		}
		let dir_path = root.join(ancestor);
		match std::fs::metadata(&dir_path) {
			Ok(metadata) if metadata.is_dir() => {}
			// Something that is not a directory standing where one has to be does prove nothing is
			// under it — but the ancestor itself is PRESENT, so there is no path here to call
			// absent, and minting one for the leaf would delete rows on the strength of a type flip
			// this pass has not observed at its own path. The flip is dirty in its own right when
			// it was seen.
			Ok(_) => return Ancestors::Unreadable,
			Err(source) if source.kind() == ErrorKind::NotFound => {
				return Ancestors::Gone(ancestor);
			}
			Err(source) => {
				out.complete = false;
				record(
					&mut out.errors,
					root,
					ScanError::Io {
						rel_path: ancestor.to_owned(),
						source,
					},
				);
				return Ancestors::Unreadable;
			}
		}
		if let Some(hit) = rules.decide(ancestor, true) {
			return Ancestors::Hidden(ancestor, hit.into());
		}
		if rule_files.reads(ancestor) {
			load_rule_file(
				root,
				ancestor,
				&dir_path,
				rules,
				&mut out.ignore_blocked,
				&mut out.errors,
			);
		}
		walked.insert(ancestor.to_owned());
	}
	Ancestors::Walked
}

/// Whether an observation already answers for `path`: one at the path itself, or at an ancestor. A
/// subtree walk lists what is under it, and an ancestor that is gone or hidden answers for
/// everything below it.
///
/// A walk prunes what the rules hide and what it reads under a real path instead
/// ([`LocalObservation::uncovered_roots`]), so "covered" means the walk reached it or said why it
/// did not — never that a node was found.
fn covered(observed: &BTreeMap<String, LocalObservation>, path: &str) -> bool {
	observed.contains_key("")
		|| observed.contains_key(path)
		|| path
			.match_indices('/')
			.any(|(cut, _)| observed.contains_key(&path[..cut]))
}

/// File one observation, and — once per directory — the folded names of the directory it sits in.
fn insert(out: &mut LocalObservations, root: &Path, at: &str, observation: LocalObservation) {
	// A hidden path is not reconciled at all, so nothing asks what its name would collide with.
	if !matches!(observation, LocalObservation::Hidden(_)) && !at.is_empty() {
		let dir = at.rsplit_once('/').map_or("", |(dir, _)| dir);
		if !out.siblings.contains_key(dir)
			&& let Some(folded) = read_siblings(root, dir, out)
		{
			out.siblings.insert(dir.to_owned(), folded);
		}
	}
	out.observed.insert(at.to_owned(), observation);
}

/// The folded names `dir` holds, from one non-recursive `read_dir`. Folded with the engine's own
/// [`collision_key`] over NFC-normalised names, which is what the rest of the pass folds with —
/// globset's case-insensitive flag folds ASCII only.
fn read_siblings(root: &Path, dir: &str, out: &mut LocalObservations) -> Option<FoldedNames> {
	let dir_path = if dir.is_empty() {
		root.to_path_buf()
	} else {
		root.join(dir)
	};
	let entries = match std::fs::read_dir(&dir_path) {
		Ok(entries) => entries,
		// Gone since the ancestor walk stat'ed it as a directory: a race, not missing evidence (the
		// same rule the walk applies to an entry it listed), and there is nothing left in it to
		// collide with either.
		Err(source) if source.kind() == ErrorKind::NotFound => return None,
		Err(source) => {
			out.complete = false;
			record(
				&mut out.errors,
				root,
				ScanError::Io {
					rel_path: dir.to_owned(),
					source,
				},
			);
			return None;
		}
	};
	let mut folded = FoldedNames::new();
	for entry in entries {
		let entry = match entry {
			Ok(entry) => entry,
			Err(source) => {
				out.complete = false;
				record(
					&mut out.errors,
					root,
					ScanError::Io {
						rel_path: dir.to_owned(),
						source,
					},
				);
				return None;
			}
		};
		let name = entry.file_name();
		let Some(name) = name.to_str().map(|name| name.nfc().collect::<String>()) else {
			out.complete = false;
			record(
				&mut out.errors,
				root,
				ScanError::NonUtf8Name {
					lossy_path: entry.path().to_string_lossy().into_owned(),
				},
			);
			continue;
		};
		folded.entry(collision_key(&name)).or_default().insert(name);
	}
	Some(folded)
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::super::side::Nodes;

	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use super::*;
	use crate::sync_engine::{
		baseline::{BaselineEntry, BaselineState},
		ignore::FILENIGNORE,
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
		let dirty: BTreeSet<String> = paths.iter().map(|path| (*path).to_owned()).collect();
		observe_local(
			root,
			baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&dirty,
		)
		.0
	}

	fn sorted_nodes(scan: &LocalScan) -> Vec<String> {
		let mut paths: Vec<String> = scan.nodes.paths().map(|path| path.into_owned()).collect();
		paths.sort_unstable();
		paths
	}

	fn keys(out: &LocalObservations) -> Vec<&str> {
		out.observed.keys().map(String::as_str).collect()
	}

	/// A dirty rule file is observed as its DIRECTORY, and that directory answers for the entries
	/// under it — including a dirty sibling whose name sorts before `.filenignore`, which the set
	/// hands over first. Two observations that nest would break the rule `derive::merge_local` and
	/// `engine::assembly_accounted` are written against.
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

		fs::remove_dir_all(&root).ok();
	}
}
