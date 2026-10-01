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
//! A directory whose walk finds it NEW to the pair — no row at its path — is often one the user
//! renamed, and then its files hold the very bytes the rows they left recorded. Opening and reading
//! every one of them again is most of what such a walk costs, so the walk may take a file's hash from
//! the row at the same place under the directory it came from, on the fast path's own terms: equal
//! size and equal mtime. [`Renames`] pairs the two out of nothing but what this pass observes: the
//! one dirty path whose row is a `Synced` directory, whose own `stat` answers `NotFound`, and whose
//! row at the first file the walk could not answer from that file's own path records that file's
//! size and mtime. No such directory, or more than one, and every file is read. Every file is read
//! as well when any other dirty path has a row there that records the file too. The source made
//! again after the rename, renamed into by another directory or replaced by a file is on disk once
//! more, so it is no candidate, but its rows vouch for the walk's files as much as those of a gone
//! directory that only looks alike: two stories for where the walk came from, and the pass sees
//! both. A walk that took hashes must then have the shape of the rows it took them from — the same
//! paths below it, each of the same kind — or it is walked again reading every file, so an entry
//! added or removed inside it, or a rename inside it that changes the set of names below it, leaves
//! no hash from the pairing standing. The shape is all that check sees: names swapped among files
//! of equal size and mtime keep every path's kind, size and mtime as its row records them, so each
//! of those files keeps the hash its row recorded at that relative path. That is the trust below,
//! and the answer the fast path gives the same swap in a directory that did not move.
//!
//! The trust is the fast path's, keyed by the path a file had under the directory the pairing
//! establishes instead of by its own: the one dirty directory, gone or still on disk, whose rows
//! vouch for the first file the walk could not answer — and that one is gone. Bytes that changed
//! behind an equal `(size, mtime)` keep the old hash. And like the fast path, a reused hash
//! never opens the file, so a file this process cannot read takes its row's hash where a fresh read
//! would have reported it. A `.filenignore` is the one file never answered this way: what it holds
//! decides what the walk hides, and one that cannot be read has to bail and block its directory
//! exactly as it does anywhere else.
//!
//! A path with NO observation, here or at an ancestor, is not absent: it is a path this pass got no
//! local evidence for (an unreadable ancestor, a directory replaced by a file, a file that vanished
//! between the `stat` and the hash, a socket where a row sits). Its baseline row is carried
//! unchanged and the next pass looks again.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	io::ErrorKind,
	path::Path,
};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
	ignore::{FILENIGNORE, IgnoreDecision, IgnoreRules, rule_file_dir},
	plan::{ancestors, is_under},
	rows::Baseline,
	scan::{
		LocalNode, LocalScan, RuleFiles, ScanError, collision_key, fast_path_hash, hash_file,
		load_rule_file, name_rejection, record, scan_subtree,
	},
	side::Nodes,
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

/// Pairs a directory walk with the directory it was renamed from, out of nothing but what this pass
/// observes itself (see the module docs).
struct Renames<'a> {
	root: &'a Path,
	baseline: &'a Baseline,
	dirty: &'a BTreeSet<&'a str>,
	/// The dirty paths a walk may have come from ([`Origins::of`]): found when the first walk asks, and
	/// kept for the pass.
	origins: Option<Origins<'a>>,
}

/// The dirty paths below the pair root, split by whether a walk may be paired with them.
struct Origins<'a> {
	/// Gone from disk, with a `Synced` directory row: the only paths a walk is paired with.
	gone: Vec<&'a str>,
	/// Every other one: on disk, or gone without a `Synced` directory row. Never paired with, but its
	/// rows may vouch for a walked file as well as a gone one's do, and then the walk has two stories
	/// for where it came from.
	others: Vec<&'a str>,
}

/// Where one directory walk's files may take their hashes from.
enum Source {
	/// No file has missed the fast path yet.
	Unprobed,
	/// Not a rename this pass can prove: every file is read.
	Refused,
	/// Renamed from the one gone directory [`Renames::bind`] found: its rows keyed by their path
	/// below it — `""` is that directory itself, and every other key starts with `/`, as a walked
	/// path does below the walked directory — each with its [`Stamp`] where it has one, and how many
	/// files took theirs.
	Bound {
		rows: HashMap<String, (NodeKind, Option<Stamp>)>,
		reused: usize,
	},
}

/// A `Synced` file row's size, mtime and hash, when it records all three: what a file of a directory
/// renamed away from the row may take its hash from.
#[derive(Debug, Clone, Copy)]
struct Stamp {
	size: u64,
	mtime: i64,
	hash: Blake3Hash,
}

impl Stamp {
	/// Anything but `Synced` records a divergence or a one-sided copy, not the bytes this side held.
	fn of(row: &BaselineEntry) -> Option<Self> {
		if row.state != BaselineState::Synced || row.kind != NodeKind::File {
			return None;
		}
		Some(Self {
			size: row.size?,
			mtime: row.local_mtime?,
			hash: row.content_hash?,
		})
	}

	/// The recorded hash, for a file of `size` and `mtime`: the fast path's own equalities, and the
	/// one place both the pairing and each file ask them.
	fn hash_for(self, size: u64, mtime: i64) -> Option<Blake3Hash> {
		(self.size == size && self.mtime == mtime).then_some(self.hash)
	}
}

impl<'a> Renames<'a> {
	/// The hash the walk of `to` may give its file `rel_path` of `size` and `mtime` without reading
	/// it, asked once the fast path has missed at the file's own path.
	fn hash(
		&mut self,
		to: &str,
		source: &mut Source,
		rel_path: &str,
		size: u64,
		mtime: i64,
	) -> Option<Blake3Hash> {
		// What it holds decides what the walk hides, and one that cannot be read has to bail and
		// block its directory as it does anywhere else: a rule file is always read, and no walk is
		// paired at one either.
		if rel_path.rsplit('/').next() == Some(FILENIGNORE) {
			return None;
		}
		let below = rel_path.strip_prefix(to)?;
		if matches!(source, Source::Unprobed) {
			*source = self.bind(to, below, size, mtime);
		}
		let Source::Bound { rows, reused } = source else {
			return None;
		};
		let hash = rows.get(below)?.1?.hash_for(size, mtime)?;
		*reused += 1;
		Some(hash)
	}

	/// Pair the walk of `to` at the first file it could not answer from that file's own path: the
	/// one `below` it, of `size` and `mtime`. Once per walk, whatever the answer — a directory of new
	/// files that is no rename would otherwise probe every gone directory once per file.
	fn bind(&mut self, to: &str, below: &str, size: u64, mtime: i64) -> Source {
		// A fold only ever moves a directory into a path the pair holds no row at, so one with a row
		// is no rename this pass can prove — and asking that first keeps a re-walk of a synced
		// directory from probing anything more. Nothing is renamed into the pair root.
		if to.is_empty() || self.baseline.get(to).is_some() {
			return Source::Refused;
		}
		let baseline = self.baseline;
		let origins = self
			.origins
			.get_or_insert_with(|| Origins::of(self.root, baseline, self.dirty));
		// Not `to` itself, and not a path nested in `to` or `to` nested in it: a directory the walk
		// is inside of or has under it rather than where it came from — a consistent baseline and a
		// settled disk hold neither, and a pairing across one would splice the wrong paths together.
		// Dropped before any row is read: in a pass that holds nothing but a rename's two ends, the
		// end being walked is the only other path, and asking about it could only miss.
		let apart = |path: &str| path != to && !is_under(path, to) && !is_under(to, path);
		let vouches = |path: &str| {
			baseline
				.get(&format!("{path}{below}"))
				.as_ref()
				.and_then(Stamp::of)
				.and_then(|stamp| stamp.hash_for(size, mtime))
				.is_some()
		};
		// Exactly one gone directory whose row records this file. Two are two stories, and the walk
		// is not told which one is true by guessing.
		let mut matched = origins
			.gone
			.iter()
			.copied()
			.filter(|&path| apart(path) && vouches(path));
		let (Some(from), None) = (matched.next(), matched.next()) else {
			return Source::Refused;
		};
		// And no other dirty path whose row records it either. The directory a walk came from does
		// not have to be gone for its rows to vouch for the walk's files: made again after the
		// rename, renamed into by another, or replaced by a file, its path is there once more, and
		// the one gone directory that matched may only look alike. So the rows decide, not what
		// stands at the path now.
		if origins
			.others
			.iter()
			.copied()
			.filter(|&path| apart(path))
			.any(vouches)
		{
			return Source::Refused;
		}
		let mut rows = HashMap::with_capacity(baseline.count_subtree(from) + 1);
		rows.insert(String::new(), (NodeKind::Dir, None));
		for row in baseline.subtree(from) {
			let stamp = Stamp::of(&row);
			// The path below `from`, in the allocation the row already made for its path.
			let mut key = row.rel_path;
			key.drain(..from.len());
			rows.insert(key, (row.kind, stamp));
		}
		tracing::debug!(
			"local observation of {}: {to:?} is {from:?} renamed; its files take the hashes their \
			 rows recorded wherever size and mtime are unchanged",
			self.root.display()
		);
		Source::Bound { rows, reused: 0 }
	}
}

impl Source {
	/// Whether the walk of `to` may stand. One that took no hash from the rows read every file, as it
	/// always did. One that took some must have the shape of the rows it took them from — the same
	/// paths below it, each of the same kind — or the pairing was not the rename it looked like, and
	/// nothing it handed out may stand.
	fn fits(&self, to: &str, scan: &LocalScan, baseline: &Baseline) -> bool {
		let Self::Bound { rows, reused } = self else {
			return true;
		};
		if *reused == 0 {
			return true;
		}
		let nodes = scan.nodes.of(baseline);
		nodes.len() == rows.len()
			&& nodes.iter().all(|(path, node)| {
				path.strip_prefix(to)
					.and_then(|below| rows.get(below))
					.is_some_and(|&(kind, _)| kind == node.kind)
			})
	}
}

impl<'a> Origins<'a> {
	/// Split `dirty` for the pairing. A path is gone when its `stat` answers `NotFound` and its row
	/// is a `Synced` directory: every directory a walk this pass makes may be paired with. The `stat`
	/// is the one an [`Absence`] is minted from, taken here for itself, so the answer does not depend
	/// on whether the loop has reached the path yet; it is evidence for the pairing only, and mints
	/// nothing.
	///
	/// The `stat` comes first, and the rows are asked only about the paths that are gone. A rename's
	/// two ends are usually siblings, and a cursor asked about both reads their directory's range for
	/// the second — up to a page of rows, to learn that the end still on disk is no candidate, which
	/// the `stat` says for nothing.
	fn of(root: &Path, baseline: &Baseline, dirty: &BTreeSet<&'a str>) -> Self {
		let mut rows = baseline.cursor();
		let (gone, others) = dirty
			.iter()
			.copied()
			.filter(|path| !path.is_empty())
			.partition(|path| {
				std::fs::metadata(root.join(path))
					.is_err_and(|error| error.kind() == ErrorKind::NotFound)
					&& rows.get(path).is_some_and(|row| {
						row.kind == NodeKind::Dir && row.state == BaselineState::Synced
					})
			});
		Self { gone, others }
	}
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
	// `assembly_bounds` are both written against.
	let dirty: BTreeSet<&str> = dirty
		.iter()
		.map(|path| rule_file_dir(path).unwrap_or(path))
		.collect();
	let mut renames = Renames {
		root,
		baseline,
		dirty: &dirty,
		origins: None,
	};
	for &path in &dirty {
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
				let mut source = Source::Unprobed;
				let (mut scan, mut carried) = scan_subtree(
					root,
					path,
					baseline,
					rules,
					rule_files,
					&mut |rel_path, size, mtime| {
						renames.hash(path, &mut source, rel_path, size, mtime)
					},
				);
				if !source.fits(path, &scan, baseline) {
					// Walked twice, rather than re-reading only the files that took a hash: a rename
					// that also changed what the directory holds is the rare case, and the second
					// walk is exactly the one a pass without the pairing makes. The rules the first
					// walk loaded are safe to carry into it: loading a rule file only ever adds or
					// replaces its own directory's rules, and the second walk loads every one it
					// finds again.
					tracing::debug!(
						"local observation of {}: {path:?} does not have the shape of the directory \
						 it was paired with; walking it again, reading every file",
						root.display()
					);
					(scan, carried) =
						scan_subtree(root, path, baseline, carried, rule_files, &mut |_, _, _| {
							None
						});
				}
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
	for ancestor in ancestors(path) {
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
		|| ancestors(path).any(|ancestor| observed.contains_key(ancestor))
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

	use super::super::side::NodesAt;

	use uuid::Uuid;

	use super::*;

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
}
