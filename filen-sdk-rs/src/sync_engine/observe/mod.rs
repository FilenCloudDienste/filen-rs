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
//!
//! A path observed gone, hidden or walked takes every row under it out of the derived local map one
//! by one, and a walk holds a node per entry it lists. So past [`SUBTREE_CAP`] rows under such a
//! path, or entries below a walked one, the observation stops and the pass reads the local side
//! whole instead ([`FullPassReason::LocalOverflow`]). So does a walk paired with a directory that
//! large, before it takes a hash from those rows. The rows under the path that stand in for
//! neither side are not counted: every pass holds those whatever changed (see [`cap_under`]).

use std::{
	cell::RefCell,
	collections::{BTreeMap, BTreeSet, HashMap},
	io::ErrorKind,
	path::Path,
};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;

use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
	changes::{FullPassReason, SUBTREE_CAP, past_cap},
	ignore::{FILENIGNORE, IgnoreDecision, IgnoreRules, rule_file_dir},
	plan::{ancestors, is_under, subtree_bounds},
	rows::Baseline,
	scan::{
		LocalNode, LocalScan, PairRoot, RuleFiles, ScanError, ScanRoot, collision_key,
		fast_path_hash, hash_file, load_rule_file, mtime_nanos, name_rejection, record,
		scan_subtree,
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
	/// Renamed from the one gone directory [`Renames::bind`] found, which holds more rows than
	/// [`cap_under`] allows: the walk stops at its next entry, and the pass reads the local side
	/// whole instead of reading those rows into memory.
	Oversized,
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
		let under = baseline.count_subtree(from);
		if past_cap(under, cap_under(baseline, self.dirty, from)) {
			return Source::Oversized;
		}
		let mut rows = HashMap::with_capacity(under + 1);
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
/// applied at that depth. `pair_root` says whether the tree holds the account's data directory,
/// which its subtree walks never enter, as a whole walk does not.
///
/// `Err` is a dirty path whose subtree is too large to narrow a pass to (see the module docs): the
/// observation stops there, and the pass reads the local side whole instead.
pub(super) fn observe_local(
	root: &Path,
	pair_root: PairRoot,
	baseline: &Baseline,
	mut rules: IgnoreRules,
	rule_files: &RuleFiles,
	dirty: &BTreeSet<String>,
) -> Result<(LocalObservations, IgnoreRules), FullPassReason> {
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
		return Ok((out, rules));
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
				within_cap(baseline, &dirty, at)?;
				let absence = Absence {
					gone: at.to_owned(),
				};
				insert(&mut out, root, at, LocalObservation::Absent(absence));
				continue;
			}
			Ancestors::Hidden(at, decision) => {
				within_cap(baseline, &dirty, at)?;
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
			within_cap(baseline, &dirty, path)?;
			insert(&mut out, root, path, LocalObservation::Hidden(hit.into()));
			continue;
		}
		match stat {
			Ok(_) if is_dir => {
				// Asked before the walk: a directory whose rows are past the cap is a whole read
				// however little of it is still on disk. The walk stops past the same cap.
				let cap = within_cap(baseline, &dirty, path)?;
				// Shared between the two hooks the walk is handed: the pairing writes it, and the
				// walk stops once it says the directory paired with is too large.
				let source = RefCell::new(Source::Unprobed);
				let (mut scan, mut carried) = scan_subtree(
					ScanRoot {
						path: root,
						pair_root,
					},
					path,
					baseline,
					rules,
					rule_files,
					&mut |rel_path, size, mtime| {
						renames.hash(path, &mut source.borrow_mut(), rel_path, size, mtime)
					},
					&|listed| {
						past_cap(listed, cap) || matches!(*source.borrow(), Source::Oversized)
					},
				)
				.ok_or(FullPassReason::LocalOverflow)?;
				let source = source.into_inner();
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
					(scan, carried) = scan_subtree(
						ScanRoot {
							path: root,
							pair_root,
						},
						path,
						baseline,
						carried,
						rule_files,
						&mut |_, _, _| None,
						&|listed| past_cap(listed, cap),
					)
					.ok_or(FullPassReason::LocalOverflow)?;
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
						Err(source) if source.kind() == ErrorKind::NotADirectory => {
							changed_under_observation(&mut out, root, path, &source);
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
					mtime_nanos: mtime_nanos(&metadata, mtime),
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
				within_cap(baseline, &dirty, path)?;
				let absence = Absence {
					gone: path.to_owned(),
				};
				insert(&mut out, root, path, LocalObservation::Absent(absence));
			}
			Err(source) if source.kind() == ErrorKind::NotADirectory => {
				changed_under_observation(&mut out, root, path, &source);
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
	Ok((out, rules))
}

/// Whether the rows strictly under `at` are few enough for an observation there: one that `at` is
/// gone, hidden or walked takes each of them out of the derived local map (`derive::merge_local`),
/// leaving a tombstone per row. Past [`cap_under`] the pass reads the local side whole instead.
///
/// Hands back that cap, which also bounds the entries a walk of `at` lists.
fn within_cap(
	baseline: &Baseline,
	dirty: &BTreeSet<&str>,
	at: &str,
) -> Result<usize, FullPassReason> {
	let cap = cap_under(baseline, dirty, at);
	if past_cap(baseline.count_subtree(at), cap) {
		return Err(FullPassReason::LocalOverflow);
	}
	Ok(cap)
}

/// The most rows strictly under `at` an observation there may take out of the derived local map,
/// or entries a walk of `at` may list below it: [`SUBTREE_CAP`], plus the rows under `at` that stand
/// in for neither side.
///
/// Those rows are no change's expansion. `derive::from_baseline` puts every one of them in the
/// dirty set of every pass, which is where they are counted from, so a pass holds a path for each
/// whether or not anything changed, and a whole read need not retire them: a one-way mode leaves
/// the destination's adopted copy where it is. Counted, a directory of them past the cap would
/// make every pass read whole. What else lies under `at` still counts.
fn cap_under(baseline: &Baseline, dirty: &BTreeSet<&str>, at: &str) -> usize {
	// Every path is below the pair root, where `subtree_bounds("")` spans none. Bound out here, as
	// the range borrows the bounds.
	let bounds = subtree_bounds(at);
	let under = if at.is_empty() {
		dirty.range::<&str, _>(..)
	} else {
		dirty.range::<&str, _>(bounds.start.as_str()..bounds.end.as_str())
	};
	SUBTREE_CAP.saturating_add(under.filter(|path| baseline.uncarryable(path)).count())
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
			Err(source) if source.kind() == ErrorKind::NotADirectory => {
				changed_under_observation(out, root, ancestor, &source);
				return Ancestors::Unreadable;
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

/// A `stat` or read of `path` that met a file where a directory above it stood when it was
/// stat'd — for this path or for one observed before it: that directory changed under the
/// observation, as one the scan's walk finds replaced does. Nothing is wrong with the tree, so
/// nothing is reported, but the path has no reading, so the observation is partial and the next
/// pass looks again.
fn changed_under_observation(
	out: &mut LocalObservations,
	root: &Path,
	path: &str,
	source: &std::io::Error,
) {
	out.complete = false;
	tracing::debug!(
		"local observation of {}: {path:?} changed under the observation, leaving it partial: \
		 {source}",
		root.display()
	);
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
		// Gone, or replaced by a file, since the ancestor walk stat'ed it as a directory: a race, not
		// missing evidence (the same rule the walk applies to an entry it listed), and there is
		// nothing left in it to collide with either.
		Err(source)
			if matches!(
				source.kind(),
				ErrorKind::NotFound | ErrorKind::NotADirectory
			) =>
		{
			return None;
		}
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
mod tests;
