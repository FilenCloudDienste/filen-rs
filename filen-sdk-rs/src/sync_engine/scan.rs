//! The local-side scan: walk a pair's local root into a `rel_path -> LocalNode` map — or, for a
//! whole pass, into the pass's stage ([`scan_local_staged`]) — applying the
//! mtime+size fast-path so an unchanged file is never re-hashed. An edit that kept BOTH its size
//! and its mtime is therefore invisible here; what protects it from being overwritten is the
//! pre-download stash hashing its target (`apply::local_holds_unsynced_content`), not the scan.
//! A subtree walk extends the same trust to a directory its caller proved renamed: a file the
//! fast path misses at its own path may take the hash of the row it left (see [`scan_subtree`]).
//!
//! Paths are NFC-normalized (macOS hands back NFD) and `/`-joined so they key 1:1 against the
//! NFC-normalized remote snapshot and the baseline. Two entries that normalize to the same key are
//! a collision (the engine refuses to reconcile a pair with one — a 1:1 local mapping is
//! required). The scan reports whether it completed: a partial scan (an unreadable subtree, a
//! missing root) must never let the mass-delete guard propagate deletions.
//!
//! Symlinks are followed and their targets read as regular files (Filen has no symlink concept and
//! the engine never writes one); `walkdir`'s loop detection guards against cycles. A symlink that
//! cannot be followed (dangling, or part of a loop) is recorded as an error but does not make the
//! scan incomplete, unless the baseline tracks a directory at that path (see
//! [`LocalScan::complete`]).
//!
//! A symlinked DIRECTORY whose target lies inside the root is not walked: its target is walked under
//! its real path, and walking the link as well would sync one subtree twice under two names. The
//! real path is the one synced, whichever of the two the walk reaches first; the link is recorded in
//! [`LocalScan::aliased_dirs`].

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
	convert::Infallible,
	ffi::OsStr,
	fmt,
	io::{ErrorKind, Read},
	mem,
	num::NonZero,
	ops::ControlFlow,
	path::{Component, Path},
	sync::{
		Mutex,
		atomic::{AtomicBool, Ordering},
		mpsc,
	},
};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::{UnicodeNormalization, is_nfc};

use super::{
	baseline::{BaselineEntry, NodeKind},
	ignore::{
		FILENIGNORE, IgnoreDecision, IgnoreParseError, IgnoreRules, IgnoreSource,
		MAX_RULE_FILE_BYTES, Origin, rule_file_text,
	},
	rows::Baseline,
	side::Side,
	stage::{Paging, Stage, StageSink},
};
use crate::{
	fs::name::ValidatedName,
	io::{DOWNLOAD_TMP_EXT, FilenMetaExt},
};

/// The per-pair local quarantine directory (where remote-propagated deletions are moved instead of
/// being destroyed). Always excluded from the scan so it is never itself synced back up.
pub(crate) const QUARANTINE_DIR: &str = ".filen-sync-trash";

/// One item observed under the local root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LocalNode {
	/// NFC-normalized, `/`-joined path relative to the local root.
	pub(crate) rel_path: String,
	pub(crate) kind: NodeKind,
	/// Size in bytes (0 for directories).
	pub(crate) size: u64,
	/// Modification time in epoch millis.
	pub(crate) mtime_millis: i64,
	/// BLAKE3 of the content (files only): reused from the baseline when `(size, mtime)` matched
	/// (the fast-path) — the row at its own path, or the one at the path a caller proved its
	/// directory was renamed from — otherwise freshly computed.
	pub(crate) content_hash: Option<Blake3Hash>,
}

/// What went wrong for one entry during a scan. Non-fatal individually: the walk carries on, every
/// error is logged at `warn` as it is recorded, and all but a
/// [`DuplicateName`](Self::DuplicateName) (which refuses the pass instead) are reported in the
/// pass's [`SyncReport::errors`](super::SyncReport::errors). Any error marks the whole scan
/// [`incomplete`](LocalScan::complete), except a symlink that cannot be followed and a
/// `.filenignore` that could not be used: what its rules cover is blocked instead (see
/// [`LocalScan::ignore_blocked`]), so nothing was missed, only withheld.
#[derive(Debug)]
pub(crate) enum ScanError {
	/// An entry could not be read (permission, hash failure, a symlink that is dangling or part of a
	/// loop, an unreadable `.filenignore`), or the root itself was gone when the walk finished. An
	/// entry gone or replaced since the walk listed it is NOT one of these, nor is a directory gone
	/// or replaced before the walk read its listing: nothing is wrong with the tree, so they are
	/// logged at `debug` only. A FILE gone between the walk handing it over and the scan's stat or
	/// hash read leaves the scan complete, since the walk observed it; any other such change leaves
	/// it partial.
	Io {
		rel_path: String,
		source: std::io::Error,
	},
	/// An entry whose name is not valid UTF-8 — Filen names are UTF-8, so it cannot be synced.
	NonUtf8Name { lossy_path: String },
	/// Two entries normalize to the same key — the pair cannot be reconciled until the user
	/// resolves it (a single local path cannot hold both).
	DuplicateName { rel_path: String },
	/// A `.filenignore` line that was skipped, or a whole file that did not compile.
	IgnoreRules(IgnoreParseError),
}

impl fmt::Display for ScanError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			// An error on the root itself (a missing root) has no relative path.
			Self::Io { rel_path, source } if rel_path.is_empty() => {
				write!(f, "local root: {source}")
			}
			Self::Io { rel_path, source } => write!(f, "{rel_path}: {source}"),
			Self::NonUtf8Name { lossy_path } => {
				write!(f, "{lossy_path}: the name is not valid UTF-8")
			}
			Self::DuplicateName { rel_path } => {
				write!(
					f,
					"{rel_path}: the names collide once case and Unicode form are folded"
				)
			}
			Self::IgnoreRules(error) => error.fmt(f),
		}
	}
}

/// What the walk found at one path, handed to a [`ScanSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Found {
	/// An entry the scan read.
	Node(LocalNode),
	/// An entry that took its folded name and then could not become a node: a file whose hash could
	/// not be read, and a rule file the scan could not use. The name stays taken — the claim is what
	/// says so — with nothing at it. An entry skipped because it VANISHED is never one: it is gone,
	/// so its name is free.
	Claim(String),
}

impl Found {
	pub(super) fn rel_path(&self) -> &str {
		match self {
			Self::Node(node) => &node.rel_path,
			Self::Claim(rel_path) => rel_path,
		}
	}

	pub(super) fn into_rel_path(self) -> String {
		match self {
			Self::Node(node) => node.rel_path,
			Self::Claim(rel_path) => rel_path,
		}
	}
}

/// Two entries whose paths fold to one name ([`collision_key`]): the one that took it, and the one a
/// [`ScanSink`] refused for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Collision {
	pub(super) taken: String,
	pub(super) refused: String,
}

/// Where a scan puts what it finds: a map in memory ([`MapSink`]), or a whole pass's stage
/// ([`StageSink`]), which holds what a whole tree's walk finds in a file rather than in memory.
///
/// Either one is the walk's collision check: an entry whose path folds to a name another entry
/// already took is refused, and the walk reports the two.
pub(super) trait ScanSink {
	/// What can fail in putting an entry: nothing for a map.
	type Error;

	/// Take `found`'s folded name for it, or — when another entry has it — refuse it and say which.
	fn put(&mut self, found: Found) -> Result<Option<Collision>, Self::Error>;

	/// The nodes put, as the side a pass reads.
	fn finish(self) -> Result<Side<LocalNode>, Self::Error>;
}

/// A [`ScanSink`] into a path-keyed map: a subtree walk's, and the tests'.
pub(super) struct MapSink {
	nodes: HashMap<String, LocalNode>,
	/// A digest of every collision key taken so far, to spot a second entry folding the same way.
	/// The keys themselves are not kept: a hit is rare and is resolved against the paths already
	/// taken, so this costs 16 bytes a node instead of a second copy of every path.
	claimed: HashSet<u128>,
	/// The claims ([`Found::Claim`]): the lookup a hit makes has to find them too — a walk with
	/// nothing wrong puts none.
	claims: Vec<String>,
}

impl MapSink {
	pub(super) fn with_capacity(capacity: usize) -> Self {
		Self {
			nodes: HashMap::with_capacity(capacity),
			claimed: HashSet::with_capacity(capacity),
			claims: Vec::new(),
		}
	}
}

impl ScanSink for MapSink {
	type Error = Infallible;

	fn put(&mut self, found: Found) -> Result<Option<Collision>, Infallible> {
		let key = collision_key(found.rel_path());
		// Naming the twin means folding the paths already taken, which is why it happens on a
		// digest hit only. Finding none means the digests collided rather than the names, and a pair
		// is never refused over that.
		if !self.claimed.insert(collision_hash(&key))
			&& let Some(taken) = self
				.nodes
				.keys()
				.chain(&self.claims)
				.find(|taken| collision_key(taken) == key)
				.cloned()
		{
			return Ok(Some(Collision {
				taken,
				refused: found.into_rel_path(),
			}));
		}
		match found {
			Found::Node(node) => {
				self.nodes.insert(node.rel_path.clone(), node);
			}
			Found::Claim(rel_path) => self.claims.push(rel_path),
		}
		Ok(None)
	}

	fn finish(self) -> Result<Side<LocalNode>, Infallible> {
		Ok(Side::Whole(self.nodes))
	}
}

/// The result of scanning a local root.
#[derive(Debug)]
pub(crate) struct LocalScan {
	pub(crate) nodes: Side<LocalNode>,
	/// `false` if the node set may be INCOMPLETE — a missing/unreadable root, an unreadable
	/// subtree, a directory that changed under the walk (which has no [`ScanError`]), a non-UTF-8
	/// name, or a normalized-name collision. The mass-delete guard refuses to propagate deletions
	/// from an incomplete scan (an empty/half-read source must not nuke the destination).
	///
	/// A symlink that cannot be followed (dangling, or looping back onto an ancestor) leaves it
	/// true: the link itself is all there is, and treating it as missing evidence would hold every
	/// deletion of the pair for as long as the link exists. Unless the baseline has a DIRECTORY row
	/// at that path: then the link used to lead to a tree that was synced (an unmounted drive, say),
	/// and reading that subtree as deleted would propagate it. A file row there is only the copy a
	/// file link was synced as, and its deletion propagates like any other.
	pub(crate) complete: bool,
	pub(crate) errors: Vec<ScanError>,
	/// Paths whose NAME the remote would reject, mapped to the validator's own message. Reported
	/// once at the TOP of a rejected subtree; the engine screens every action for such a path and
	/// its descendants out of the plan.
	///
	/// The nodes themselves stay in `nodes`: this map says "nothing can be pushed here", not "this
	/// is not on disk". Omitting them would make a rename of a synced item into a rejected name
	/// read downstream as a local DELETION and trash the remote copy.
	///
	/// Deliberately NOT an entry in `errors`: those mean the scan may have MISSED something, which
	/// makes every apparent deletion untrustworthy. An unsyncable name is the opposite — a fully
	/// observed item that simply cannot be pushed — so the guard must not be tripped by it.
	pub(crate) invalid_names: BTreeMap<String, String>,
	/// Symlinked directories whose target lies inside the root, mapped to the target's own path.
	/// Neither the link nor anything under it is in `nodes`: the target is scanned under its real
	/// path. The engine blocks every action at or under the link, so what an earlier pass synced
	/// there is not read as deleted, and nothing is downloaded through the link.
	pub(crate) aliased_dirs: BTreeMap<String, String>,
	/// The top-most entries the ignore rules hide, with the deciding rule. Neither they nor anything
	/// under them is in `nodes`, hashed or checked for collisions, and a `.filenignore` inside one is
	/// never read.
	pub(crate) ignored: BTreeMap<String, IgnoreDecision>,
	/// How many entries only the BUILT-IN defaults hid with no baseline row at or under them. They
	/// are pruned like any other ignored entry, but they are not roots: one per `.DS_Store` gives a
	/// pass a root under every directory, and each of them is left out of the report
	/// (`Prepared::ignored`) and deletes no row when the pass untracks. Logged, never reported.
	pub(crate) ignored_default_untracked: usize,
	/// Directories whose `.filenignore` could not be used (unreadable, or not compilable as a whole).
	/// Their subtrees are still scanned into `nodes`, since they exist; `""` is the root.
	pub(crate) ignore_blocked: BTreeSet<String>,
}

impl LocalScan {
	/// One line per scan error a pass reports in [`SyncReport::errors`](super::SyncReport::errors):
	/// every error but a [`DuplicateName`](ScanError::DuplicateName), which refuses the pass with
	/// its own line instead.
	pub(crate) fn reported_errors(&self) -> impl Iterator<Item = String> + '_ {
		self.errors
			.iter()
			.filter(|e| !matches!(e, ScanError::DuplicateName { .. }))
			.map(|e| format!("local scan: {e}"))
	}
}

/// Log `error` and collect it.
pub(super) fn record(errors: &mut Vec<ScanError>, root: &Path, error: ScanError) {
	tracing::warn!("local scan of {}: {error}", root.display());
	errors.push(error);
}

/// Whether a walk error is a symlink that cannot be followed: one looping back onto an ancestor, or
/// one whose target does not resolve (dangling, or a link to itself). A symlink to a directory that
/// merely cannot be READ is not one of these — its target exists and was not read.
fn unfollowable_symlink(err: &walkdir::Error) -> bool {
	if err.loop_ancestor().is_some() {
		return true;
	}
	err.path().is_some_and(|path| {
		std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
			&& std::fs::metadata(path).is_err()
	})
}

/// Whether a read failed because the entry, or a directory above it, is gone or is no longer a
/// directory: what the walk listed changed under it.
fn changed_under_walk(source: &std::io::Error) -> bool {
	matches!(
		source.kind(),
		ErrorKind::NotFound | ErrorKind::NotADirectory
	)
}

/// The root-relative path of the directory `entry` links to, if `entry` is a symlinked directory
/// whose canonical target lies inside `canonical_root`. `depth` is the entry's depth below the PAIR
/// ROOT, which is the one directory the rule does not apply to: a root reached through a symlink
/// (macOS's `/var`) is the pair's anchor, not an alias of something else it syncs.
fn alias_target(
	entry: &walkdir::DirEntry,
	canonical_root: Option<&Path>,
	depth: usize,
) -> Option<String> {
	if depth == 0 || !entry.path_is_symlink() || !entry.file_type().is_dir() {
		return None;
	}
	let target = std::fs::canonicalize(entry.path()).ok()?;
	normalize_rel_path(target.strip_prefix(canonical_root?).ok()?)
}

/// The reason the remote would reject `rel_path`'s own name, or `None` if it would accept it.
///
/// Uses the SDK's own validator — the very rule the upload and create-dir paths enforce — rather
/// than a second copy of the rules, so the scan can never disagree with what an upload does. Only
/// the LAST component is checked: every ancestor is itself a scanned entry that was checked when
/// the walk reached it.
pub(super) fn name_rejection(rel_path: &str) -> Option<String> {
	let name = rel_path.rsplit('/').next()?;
	ValidatedName::try_from(name).err().map(|e| e.to_string())
}

/// Whether some ANCESTOR of `rel_path` was already rejected, in which case this entry is part of a
/// subtree that is already reported and must be skipped silently.
fn under_invalid_name(invalid: &BTreeMap<String, String>, rel_path: &str) -> bool {
	let mut path = rel_path;
	while let Some((parent, _)) = path.rsplit_once('/') {
		path = parent;
		if invalid.contains_key(path) {
			return true;
		}
	}
	false
}

/// NFC-normalize a relative path's components (case preserved) and `/`-join them. `None` if any
/// component is non-UTF-8 or not a plain name (a walked subtree only yields `Normal` components).
///
/// Shared with the filesystem watcher's changelist (`changes::relative_key`), so a path an event
/// names is keyed exactly as the walk would key it.
pub(super) fn normalize_rel_path(rel: &Path) -> Option<String> {
	let mut key = String::with_capacity(rel.as_os_str().len());
	for component in rel.components() {
		let Component::Normal(name) = component else {
			return None;
		};
		let name = name.to_str()?;
		// A `Normal` component is never empty, so an empty key means this is the first one.
		if !key.is_empty() {
			key.push('/');
		}
		// Almost every name is NFC already (always so for ASCII): copied as is, it skips running
		// the composition over every character.
		if is_nfc(name) {
			key.push_str(name);
		} else {
			key.extend(name.nfc());
		}
	}
	Some(key)
}

/// The case-insensitive collision key for a (already NFC-normalized) relative path. Filen treats
/// names case-insensitively, so two paths differing only in case collide. Shared with the remote
/// view, which has to fold case the same way for its own duplicate check to mean the same thing.
pub(super) fn collision_key(rel_path: &str) -> String {
	// Same answer as the `char::to_lowercase` fold below, which maps an ASCII char to exactly one
	// ASCII char (see `at_or_under_folded`), without decoding and pushing one char at a time.
	if rel_path.is_ascii() {
		return rel_path.to_ascii_lowercase();
	}
	rel_path.chars().flat_map(char::to_lowercase).collect()
}

/// Whether `path` IS `prefix`, or lies under it, folded the way [`collision_key`] folds, but
/// without materializing either folded form.
///
/// The question a directory move asks of the two side maps about its destination. Folding each
/// key into a [`collision_key`](super::scan::collision_key) first allocates a `String` for every
/// key of the whole tree, to learn — for all but a handful of them — that the very first character
/// already differs. This stops at that character.
pub(super) fn at_or_under_folded(path: &str, prefix: &str) -> bool {
	// Almost every name on a real drive is ASCII, where folding a byte is one instruction and
	// `char::to_lowercase` is a table lookup returning an iterator. The two agree on ASCII by
	// construction — `to_lowercase` maps an ASCII char to exactly one ASCII char — so the fast
	// path is the same answer, not an approximation.
	if path.is_ascii() && prefix.is_ascii() {
		let (path, prefix) = (path.as_bytes(), prefix.as_bytes());
		return path.len() >= prefix.len()
			&& path[..prefix.len()].eq_ignore_ascii_case(prefix)
			&& path.get(prefix.len()).is_none_or(|&byte| byte == b'/');
	}
	// Comparing the two folded CHARACTER streams is comparing the two folded strings: a lowercase
	// expansion never yields `/`, so the separator can only be matched by a real one, and a prefix
	// that runs out mid-expansion leaves a character that is not `/` and is refused here.
	let mut folded = path.chars().flat_map(char::to_lowercase);
	for want in prefix.chars().flat_map(char::to_lowercase) {
		if folded.next() != Some(want) {
			return false;
		}
	}
	matches!(folded.next(), None | Some('/'))
}

/// A 128-bit digest of a [`collision_key`], for the per-pass sets that only ever ask whether some
/// other entry folded the same way. Keeping the digest instead of the key costs 16 bytes an entry
/// rather than a second copy of every path; every hit is checked against the real paths, so two
/// keys that happen to share a digest cost a lookup, never a wrong answer.
pub(super) fn collision_hash(key: &str) -> u128 {
	let digest = blake3::hash(key.as_bytes());
	u128::from_le_bytes(
		digest.as_bytes()[..16]
			.try_into()
			.expect("a blake3 digest is 32 bytes"),
	)
}

/// BLAKE3 of the file at `path`, streamed (no full read into memory).
pub(super) fn hash_file(path: &Path) -> std::io::Result<Blake3Hash> {
	let file = std::fs::File::open(path)?;
	let mut hasher = blake3::Hasher::new();
	hasher.update_reader(&file)?;
	Ok(hasher.finalize().into())
}

/// The fast-path: reuse the baseline's content hash when the file's `(size, mtime)` are unchanged,
/// so an untouched file is never re-hashed. `None` means "diverged or unknown — must hash".
pub(super) fn fast_path_hash(
	baseline: Option<&BaselineEntry>,
	size: u64,
	mtime: i64,
) -> Option<Blake3Hash> {
	let entry = baseline?;
	if entry.kind == NodeKind::File && entry.size == Some(size) && entry.local_mtime == Some(mtime)
	{
		entry.content_hash
	} else {
		None
	}
}

/// What the scan does with `.filenignore` files on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RuleFiles {
	/// Read each accepted directory's file, replacing any rules already given for it.
	Read,
	/// Read only the files of these root-relative directories (`""` is the root) and leave the rest
	/// alone: the rules come from somewhere else (the remote copies, when the remote is the source of
	/// truth).
	Only(BTreeSet<String>),
}

impl RuleFiles {
	pub(super) fn reads(&self, dir: &str) -> bool {
		match self {
			Self::Read => true,
			Self::Only(dirs) => dirs.contains(dir),
		}
	}
}

/// The metadata of the `.filenignore` in `dir_path`, if a FILE by exactly that name is there.
///
/// A directory by that name is an ordinary item (reading one on Windows fails as access denied, not
/// as a directory). So is another spelling that a case-insensitive volume opens under the rule
/// file's name: the remote and a case-sensitive device read `.FilenIgnore` as an ordinary file, and
/// every device has to agree on which file holds rules.
pub(crate) fn rule_file_metadata(dir_path: &Path) -> std::io::Result<Option<std::fs::Metadata>> {
	let metadata = match std::fs::metadata(dir_path.join(FILENIGNORE)) {
		Ok(metadata) if metadata.is_file() => metadata,
		Ok(_) => return Ok(None),
		Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
		Err(e) => return Err(e),
	};
	for entry in std::fs::read_dir(dir_path)? {
		if entry?.file_name() == OsStr::new(FILENIGNORE) {
			return Ok(Some(metadata));
		}
	}
	Ok(None)
}

/// Loads the `.filenignore` of the root-relative directory `dir` at `dir_path` into `rules`. No such
/// file is no rules. A file that cannot be read, that the remote side would refuse (see
/// [`rule_file_text`]) or that does not compile at all blocks `dir`: guessing could push what the
/// user meant to hide. A bad line is only reported.
pub(super) fn load_rule_file(
	root: &Path,
	dir: &str,
	dir_path: &Path,
	rules: &mut IgnoreRules,
	blocked: &mut BTreeSet<String>,
	errors: &mut Vec<ScanError>,
) {
	let rel_path = match dir {
		"" => FILENIGNORE.to_owned(),
		dir => format!("{dir}/{FILENIGNORE}"),
	};
	let read = rule_file_metadata(dir_path).and_then(|found| {
		found
			.map(|_| {
				// One byte past the cap is enough to refuse the file without reading all of it.
				let mut bytes = Vec::new();
				std::fs::File::open(dir_path.join(FILENIGNORE))?
					.take(MAX_RULE_FILE_BYTES + 1)
					.read_to_end(&mut bytes)?;
				Ok(bytes)
			})
			.transpose()
	});
	let bytes = match read {
		Ok(Some(bytes)) => bytes,
		Ok(None) => return,
		// A directory that cannot be listed is the walk's to handle: it leaves the scan incomplete on
		// its own, and reports why unless the directory changed under the walk.
		Err(_) if std::fs::read_dir(dir_path).is_err() => return,
		Err(source) => {
			blocked.insert(dir.to_owned());
			record(errors, root, ScanError::Io { rel_path, source });
			return;
		}
	};
	let text = match rule_file_text(bytes) {
		Ok(text) => text,
		Err(reason) => {
			blocked.insert(dir.to_owned());
			let error = IgnoreParseError {
				origin: Origin::File { dir }.to_string(),
				line: None,
				reason,
			};
			record(errors, root, ScanError::IgnoreRules(error));
			return;
		}
	};
	match IgnoreSource::parse(&text, Origin::File { dir }) {
		Ok((source, line_errors)) => {
			rules.insert_file(dir.to_owned(), source);
			for error in line_errors {
				record(errors, root, ScanError::IgnoreRules(error));
			}
		}
		Err(error) => {
			blocked.insert(dir.to_owned());
			record(errors, root, ScanError::IgnoreRules(error));
		}
	}
}

/// [`scan_local_staged`]'s walk into a `rel_path -> LocalNode` map: the form the tests and the
/// probe hold the staged walk to.
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) fn scan_local(
	root: &Path,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: RuleFiles,
) -> (LocalScan, IgnoreRules) {
	// The tree is what the baseline tracks plus whatever changed since, so the baseline is the one
	// estimate worth having; a pair with none still gets a walk's worth of room up front.
	let Ok(scanned) = scan_local_watched(
		root,
		"",
		baseline,
		rules,
		&rule_files,
		Hooks {
			on_listed: &mut |_| ControlFlow::Continue(()),
			stop: None,
			moved_hash: None,
			max_read_workers: MAX_READ_WORKERS,
		},
		MapSink::with_capacity(baseline.len().max(1024)),
	);
	scanned
}

/// Walk `root` into `stage`: what a whole pass walks, as a row per entry in the pass's stage (see
/// [`StageSink`]). The nodes come back as a [`Side::Staged`], read out of the stage a page at a
/// time, so the tree is in the file rather than in memory from the walk to the end of the plan.
/// `baseline` (keyed by rel_path) drives the fast-path and is read for the symlink rule on
/// [`LocalScan::complete`]. Blocking work; past its first [`INLINE_STATS`] entries the walk stats
/// them, and reads the hashes the fast path misses, on up to [`MAX_READ_WORKERS`] threads of its
/// own and no more than the machine's available parallelism, one started per entry handed over up
/// to that.
///
/// Entries `rules` ignore are pruned: recorded in [`LocalScan::ignored`] and never descended into.
/// Each directory's `.filenignore` that `rule_files` reads joins `rules` before its children are
/// matched. The rules come back as the scan used them, for the rest of the pass to match with.
///
/// Once `stop` is set the walk lists and hands over nothing more, and comes back incomplete once each
/// worker has finished the read it is in, to its end (a hash read cannot be interrupted). That is
/// how a pass gives up a walk it started before it knew the rules to walk with.
///
/// `Err` when the stage cannot be written — a full disk — which fails the pass before it has
/// planned anything.
pub(crate) fn scan_local_staged(
	root: &Path,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: RuleFiles,
	stage: Stage,
	stop: &AtomicBool,
) -> rusqlite::Result<(LocalScan, IgnoreRules)> {
	scan_local_watched(
		root,
		"",
		baseline,
		rules,
		&rule_files,
		Hooks {
			on_listed: &mut |_| ControlFlow::Continue(()),
			stop: Some(stop),
			moved_hash: None,
			max_read_workers: MAX_READ_WORKERS,
		},
		StageSink::new(stage, Paging::PASS)?,
	)
}

/// The most threads a walk reads entries on, beside its own; fewer on a machine whose available
/// parallelism is lower. A first sync's walk spends most of its
/// time reading hashes, open and read alike, and a converged one stat-ing entries; both overlap
/// well, and eight is what that takes on an SSD. It is the one knob to lower for a spinning disk,
/// where parallel readers seek against each other.
const MAX_READ_WORKERS: usize = 8;

/// How many listed entries a walk holds, at most, behind one that is still being read. They are put
/// in the order they were listed, so a slow read holds up the ones after it; at this many the walk
/// waits for it rather than holding more.
const READ_WINDOW: usize = 4096;

/// How many entries a whole walk stats on its own thread before it hands the rest to workers: a
/// tree this small is read before a thread would have started. A file the fast path misses among
/// them still has its hash read on a worker.
const INLINE_STATS: usize = 256;

/// [`scan_subtree`]'s hook answering for a file the fast path missed (see [`Hooks::moved_hash`]).
type MovedHash<'h> = &'h mut dyn FnMut(&str, u64, i64) -> Option<Blake3Hash>;

/// What a walk asks its caller as it goes (see [`scan_local_watched`]).
struct Hooks<'h> {
	/// Called for every entry the walker lists, just before the scan reads it; a `Break` ends the
	/// walk there, incomplete.
	on_listed: &'h mut dyn FnMut(&Path) -> ControlFlow<()>,
	/// [`scan_local_staged`]'s: once set, the walk ends where it is, incomplete, whether it is still
	/// listing or waiting on the reads of what it listed, once the reads under way end.
	stop: Option<&'h AtomicBool>,
	/// [`scan_subtree`]'s: the hash of a file the fast path missed, by its path, size and mtime.
	/// It is asked in walk order, as the walk lists each file, so a walk that has one stats every
	/// entry on its own thread; a walk with none (`None`) stats them on workers.
	moved_hash: Option<MovedHash<'h>>,
	/// How many threads, at most, read entries ([`MAX_READ_WORKERS`] outside the tests), and no more
	/// than the machine's available parallelism; `0` reads them all on the walk's own thread.
	max_read_workers: usize,
}

/// What a listed entry turned out to be once read, followed through a symlink.
enum Reading {
	Dir {
		mtime_millis: i64,
	},
	File {
		size: u64,
		mtime_millis: i64,
		/// The baseline's hash when the fast path took it (or `moved_hash` answered), read otherwise.
		hash: std::io::Result<Blake3Hash>,
	},
	/// A socket, FIFO or device: not syncable, and skipped silently. (A broken symlink never gets
	/// here: following it fails, and the walk hands it over as an error.)
	Other,
}

/// The followed metadata of `entry`, as the walk follows links: a symlinked dir or file is
/// classified by its target.
fn stat(entry: &walkdir::DirEntry) -> std::io::Result<std::fs::Metadata> {
	entry.metadata().map_err(|err| {
		err.into_io_error()
			.unwrap_or_else(|| std::io::Error::other("metadata error"))
	})
}

/// `metadata` as a [`Reading`], with `hash` answering for a file by its size and mtime.
fn reading(
	metadata: &std::fs::Metadata,
	hash: impl FnOnce(u64, i64) -> std::io::Result<Blake3Hash>,
) -> Reading {
	let mtime_millis = FilenMetaExt::modified(metadata).timestamp_millis();
	if metadata.is_dir() {
		Reading::Dir { mtime_millis }
	} else if metadata.is_file() {
		let size = metadata.len();
		Reading::File {
			size,
			mtime_millis,
			hash: hash(size, mtime_millis),
		}
	} else {
		Reading::Other
	}
}

/// Stat `entry` and, for a file, take the hash `row` holds when the fast path allows it and read the
/// file otherwise.
fn read_entry(entry: &walkdir::DirEntry, row: Option<&BaselineEntry>) -> std::io::Result<Reading> {
	Ok(reading(&stat(entry)?, |size, mtime_millis| {
		fast_path_hash(row, size, mtime_millis).map_or_else(|| hash_file(entry.path()), Ok)
	}))
}

/// An entry the walk listed, waiting in a [`Window`] for its turn to be put.
enum Listed {
	/// An error to record, with nothing to put.
	Error(ScanError),
	/// An entry, settled once `read` holds its reading; `None` while a worker reads it.
	Entry {
		rel_path: String,
		/// The type the walker listed this with, followed through a symlink: a directory's entries
		/// are listed from it.
		listed_type: std::fs::FileType,
		/// Whether this is the `.filenignore` of a directory its rules could not be read for: a
		/// read that fails here has been reported already, as that directory's.
		blocked_rule_file: bool,
		read: Option<std::io::Result<Reading>>,
	},
}

/// What the walk does with an entry it listed.
enum Next {
	/// Read on the walk's own thread already.
	Read(std::io::Result<Reading>),
	/// Handed to a worker, with the baseline row the fast path asks about.
	HandOver(Option<BaselineEntry>),
}

/// What a walk has listed and not yet put, in the order it listed it. Entries are read on worker
/// threads, and finish in any order; holding everything listed after an unread entry until it is
/// read keeps what the sink is handed, every error recorded and every rejected name, in walk order
/// — which entry of a twin takes the name included — exactly as a walk reading each entry in turn
/// would have it.
struct Window<'r, S> {
	root: &'r Path,
	listed: VecDeque<Listed>,
	/// How many entries have left the front: the sequence number of the front one.
	front: usize,
	sink: S,
	errors: Vec<ScanError>,
	complete: bool,
	invalid_names: BTreeMap<String, String>,
}

impl<S: ScanSink> Window<'_, S> {
	/// The sequence number the next entry pushed takes.
	fn next_seq(&self) -> usize {
		self.front + self.listed.len()
	}

	/// Hold `listed` behind what is held already, and put whatever that leaves ready at the front.
	/// The entry's sequence number, by which a worker's answer finds it.
	fn push(&mut self, listed: Listed) -> Result<usize, S::Error> {
		let seq = self.next_seq();
		self.listed.push_back(listed);
		self.put_ready()?;
		Ok(seq)
	}

	/// A worker's reading of the entry at `seq`; whatever it leaves ready at the front is put.
	fn answer(&mut self, seq: usize, reading: std::io::Result<Reading>) -> Result<(), S::Error> {
		match seq
			.checked_sub(self.front)
			.and_then(|at| self.listed.get_mut(at))
		{
			Some(Listed::Entry {
				read: slot @ None, ..
			}) => *slot = Some(reading),
			_ => unreachable!("a worker answers once for an entry the window still holds"),
		}
		self.put_ready()
	}

	fn put_ready(&mut self) -> Result<(), S::Error> {
		while let Some(front) = self.listed.front() {
			if matches!(front, Listed::Entry { read: None, .. }) {
				return Ok(());
			}
			let listed = self
				.listed
				.pop_front()
				.expect("just peeked (should be impossible)");
			// The one place an entry leaves the window, so `front` cannot fall out of step with it.
			self.front += 1;
			let Some(found) = self.settle(listed) else {
				continue;
			};
			// Put once the entry is read, so a file is hashed before its name is claimed: a twin
			// that vanishes before its hash read is not one, and a twin whose read fails is
			// reported for both.
			if let Some(Collision { taken, refused }) = self.sink.put(found)? {
				self.complete = false;
				record(
					&mut self.errors,
					self.root,
					ScanError::DuplicateName {
						rel_path: format!("{taken} / {refused}"),
					},
				);
			}
		}
		Ok(())
	}

	/// What a read entry puts, if anything, recording what it has to report.
	fn settle(&mut self, listed: Listed) -> Option<Found> {
		let (rel_path, listed_type, blocked_rule_file, read) = match listed {
			Listed::Error(error) => {
				record(&mut self.errors, self.root, error);
				return None;
			}
			Listed::Entry {
				rel_path,
				listed_type,
				blocked_rule_file,
				read: Some(read),
			} => (rel_path, listed_type, blocked_rule_file, read),
			Listed::Entry { read: None, .. } => {
				unreachable!("only a read entry is settled (should be impossible)")
			}
		};
		// The walk goes by the type it listed an entry with, and the read must agree. A directory
		// gone or replaced by then had its entries listed from what it was, and an entry that became
		// a directory had none listed at all: either way the walk caught it mid-change. So did one
		// that changed between a file and anything else, a symlink or a socket in particular, which
		// would otherwise be dropped as unsyncable and read as the file's deletion. The entry is
		// dropped, what was already read under it stays, and the scan is partial, so its absence and
		// its children's do not read as deletions. Nothing is wrong with the tree, so nothing is
		// reported either: the next pass reads it as it settled.
		let listed_dir = listed_type.is_dir();
		match &read {
			Ok(Reading::File { .. } | Reading::Other) if listed_dir => {
				self.caught_mid_change(&rel_path, "it is no longer a directory");
				return None;
			}
			Err(source) if listed_dir && changed_under_walk(source) => {
				self.caught_mid_change(&rel_path, source);
				return None;
			}
			Ok(Reading::Dir { .. }) if !listed_dir => {
				self.caught_mid_change(&rel_path, "it became a directory");
				return None;
			}
			Ok(Reading::Other) if listed_type.is_file() => {
				self.caught_mid_change(&rel_path, "it is no longer a file");
				return None;
			}
			Ok(Reading::File { .. }) if !listed_type.is_file() => {
				self.caught_mid_change(&rel_path, "it became a file");
				return None;
			}
			_ => {}
		}
		let (kind, size, mtime_millis, hash) = match read {
			Ok(Reading::Dir { mtime_millis }) => (NodeKind::Dir, 0, mtime_millis, None),
			Ok(Reading::File {
				size,
				mtime_millis,
				hash,
			}) => (NodeKind::File, size, mtime_millis, Some(hash)),
			Ok(Reading::Other) => return None,
			// Gone since the walk listed it: the walk observed this entry and something removed it
			// meanwhile, so nothing was missed and the scan still completed. Its absence is real
			// evidence, which is the whole point — a temp file that came and went during the walk
			// must not hold every deletion of the pair. Any other error (a permission denial above
			// all) IS missing evidence and still makes the scan partial.
			Err(source) if source.kind() == ErrorKind::NotFound => {
				self.vanished(&rel_path);
				return None;
			}
			// A directory above it stopped being one: that directory changed under the walk.
			Err(source) if changed_under_walk(&source) => {
				self.caught_mid_change(&rel_path, source);
				return None;
			}
			Err(source) => {
				self.complete = false;
				record(
					&mut self.errors,
					self.root,
					ScanError::Io { rel_path, source },
				);
				return None;
			}
		};

		// A name the remote would reject can never be pushed: report it once, at the top of the
		// subtree, and let the engine screen the whole subtree out of the plan rather than failing
		// an upload every pass. The node itself is still recorded — the scan reports what is on
		// disk, and an entry that exists but is omitted here reads downstream as a local DELETION,
		// so renaming a synced item into a rejected name would trash its remote copy. The scan also
		// still COMPLETED: this is a fully observed item that cannot be synced, not evidence the
		// walk missed anything, so `complete` stays true and the delete guard is unaffected.
		if !under_invalid_name(&self.invalid_names, &rel_path)
			&& let Some(reason) = name_rejection(&rel_path)
		{
			self.invalid_names.insert(rel_path.clone(), reason);
		}

		let content_hash = match hash {
			None => None,
			Some(Ok(hash)) => Some(hash),
			// A rule file the scan could not read has blocked its directory and been reported
			// already. Nothing is planned under a blocked directory, so the missing node cannot
			// read as a deletion.
			Some(Err(_)) if blocked_rule_file => return Some(Found::Claim(rel_path)),
			// Removed between its stat and its hash read: the same race as the stat's, and the same
			// answer.
			Some(Err(source)) if source.kind() == ErrorKind::NotFound => {
				self.vanished(&rel_path);
				return None;
			}
			Some(Err(source)) if changed_under_walk(&source) => {
				self.caught_mid_change(&rel_path, source);
				return Some(Found::Claim(rel_path));
			}
			Some(Err(source)) => {
				self.complete = false;
				record(
					&mut self.errors,
					self.root,
					ScanError::Io {
						rel_path: rel_path.clone(),
						source,
					},
				);
				return Some(Found::Claim(rel_path));
			}
		};
		Some(Found::Node(LocalNode {
			rel_path,
			kind,
			size,
			mtime_millis,
			content_hash,
		}))
	}

	fn vanished(&self, rel_path: &str) {
		tracing::debug!(
			"local scan of {}: {rel_path:?} vanished after the walk listed it",
			self.root.display()
		);
	}

	/// The walk caught `rel_path` mid-change: the scan is partial, but nothing is reported.
	fn caught_mid_change(&mut self, rel_path: &str, why: impl fmt::Display) {
		self.complete = false;
		tracing::debug!(
			"local scan of {}: {rel_path:?} changed under the walk, leaving the scan partial: {why}",
			self.root.display()
		);
	}
}

/// An entry handed to a worker under its sequence number, with the baseline row its fast path asks.
type ReadJob = (usize, walkdir::DirEntry, Option<BaselineEntry>);

/// A worker's answer for the entry it was handed under a sequence number: its reading, or the
/// panic that read raised.
type ReadAnswer = (usize, std::thread::Result<std::io::Result<Reading>>);

/// Read the entries handed over `jobs`, answering each with its sequence number over `done`, until
/// either channel closes. A panic in a read is carried over `done` to the walk's thread and resumed
/// there, as if the walk had read the entry itself.
fn read_worker(jobs: &Mutex<mpsc::Receiver<ReadJob>>, done: &mpsc::Sender<ReadAnswer>) {
	// The lock is let go before the read: only taking a job is serialized.
	while let Ok(Ok((seq, entry, row))) = jobs.lock().map(|jobs| jobs.recv()) {
		let read = std::panic::catch_unwind(|| read_entry(&entry, row.as_ref()));
		if done.send((seq, read)).is_err() {
			return;
		}
	}
}

/// [`scan_local_staged`]'s walk, from `start`, with [`Hooks`] and into the `sink` it is handed. The
/// tests use `on_listed` to produce a race: an entry that is there when the walk lists it and gone
/// when the scan reads it cannot be produced from the outside, and it is the one this scan has to
/// survive without calling the tree partial. [`scan_subtree`] uses it to stop a walk that has
/// listed too much.
fn scan_local_watched<S: ScanSink>(
	root: &Path,
	start: &str,
	baseline: &Baseline,
	mut rules: IgnoreRules,
	rule_files: &RuleFiles,
	Hooks {
		on_listed,
		stop,
		mut moved_hash,
		max_read_workers,
	}: Hooks<'_>,
	sink: S,
) -> Result<(LocalScan, IgnoreRules), S::Error> {
	// Where the walk begins, and how far below the root that is. Every entry is keyed against the
	// ROOT, so a subtree walk produces the very keys a whole-tree walk would; the two rules that go
	// by depth — the pair's own quarantine directory and the alias rule's exemption for the anchor
	// itself — are asked about the depth below the root rather than below the start.
	let start_path = if start.is_empty() {
		root.to_path_buf()
	} else {
		root.join(start)
	};
	let start_depth = if start.is_empty() {
		0
	} else {
		start.split('/').count()
	};
	// The walk asks about one directory's entries at a time, so the lookup keeps that directory in
	// hand instead of resolving every path from the root again.
	let mut rows = baseline.cursor();

	let mut aliased_dirs = BTreeMap::new();
	let mut ignored = BTreeMap::new();
	let mut ignored_default_untracked = 0usize;
	let mut ignore_blocked = BTreeSet::new();
	// Apart from `errors`: the rule files' errors are reported after the walk's.
	let mut rule_errors = Vec::new();
	// Canonical, so a root reached through a symlink (macOS's `/var`) compares with the canonical
	// targets. A root that does not resolve fails the walk below anyway.
	let canonical_root = std::fs::canonicalize(root).ok();

	if start_depth == 0 && rule_files.reads("") {
		load_rule_file(
			root,
			"",
			root,
			&mut rules,
			&mut ignore_blocked,
			&mut rule_errors,
		);
	}

	// Every worker takes its jobs from this one channel, and only the walk sends on it.
	let (job_tx, job_rx) = mpsc::channel();
	let job_rx = Mutex::new(job_rx);
	let walked = std::thread::scope(|scope| {
		// Moved in, so that leaving the scope by any path closes the channel and the workers stop.
		let jobs: mpsc::Sender<ReadJob> = job_tx;
		let (done_tx, done_rx) = mpsc::channel::<ReadAnswer>();
		// One started per entry handed over, up to the cap: a walk that hands over nothing — a small
		// tree, most subtree walks — starts no thread, and one that hands over a file or two starts
		// no more than that.
		let mut workers = 0usize;
		let mut worker_cap = None;
		let mut window = Window {
			root,
			listed: VecDeque::new(),
			front: 0,
			sink,
			errors: Vec::new(),
			complete: true,
			invalid_names: BTreeMap::new(),
		};
		let wait = |window: &mut Window<'_, S>| {
			let (seq, read) = done_rx
				.recv()
				.expect("the walk holds a sender of its own (should be impossible)");
			window.answer(
				seq,
				read.unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
			)
		};

		let stopped = || stop.is_some_and(|stop| stop.load(Ordering::Relaxed));
		// Set once the walk is cut short. What it did not get to is evidence of nothing, and nor is
		// what it listed and has yet to read, so it puts none of those reads: leaving the scope
		// drops the receiver of their answers, and each worker stops at the first answer it cannot
		// send. The scope joins every worker, so the walk still ends only once each has finished the
		// read it was in, to its end.
		let mut cut = false;

		// Filtered by hand rather than through `filter_entry`, so an entry is keyed once for the
		// filter and the read alike. A refused directory is skipped as `filter_entry` skips one.
		let mut walker = walkdir::WalkDir::new(&start_path)
			.follow_links(true)
			.into_iter();
		'walk: while let Some(entry) = walker.next() {
			while window.listed.len() >= READ_WINDOW {
				if stopped() {
					cut = true;
					break 'walk;
				}
				wait(&mut window)?;
			}
			let entry = match entry {
				Ok(entry) => entry,
				Err(err) => {
					let rel_path = err
						.path()
						.and_then(|p| p.strip_prefix(root).ok())
						.and_then(normalize_rel_path)
						.unwrap_or_default();
					// A directory the walk listed but could not descend into does NOT get the rule
					// the stat and the hash read below use for an entry that vanished: its
					// children were never listed, so their absence is not something the walk
					// observed. Trusting it reads a directory moved or removed mid-walk as one
					// deletion per file under it, and the destination — listed before the move — is
					// not in this scan either, so the pass cannot fold the move: it would trash the
					// remote copies and upload them again next pass. Holding those deletions for one
					// pass costs nothing instead.
					//
					// An unreadable directory or entry means the tree is partial. A symlink that
					// leads nowhere does not: there is nothing behind it to have missed — unless a
					// DIRECTORY was synced through it (a link to an unmounted drive), whose subtree
					// would otherwise read as deleted. A file synced as a link's copy is only that
					// copy, and its deletion propagates like any other.
					let unfollowable = unfollowable_symlink(&err);
					if !unfollowable
						|| rows
							.get(&rel_path)
							.is_some_and(|entry| entry.kind == NodeKind::Dir)
					{
						window.complete = false;
					}
					let below_root = err.path().is_some_and(|path| path != root);
					let source = match err.loop_ancestor() {
						Some(ancestor) => std::io::Error::other(format!(
							"symlink loops back onto {}",
							ancestor.display()
						)),
						None => err
							.into_io_error()
							.unwrap_or_else(|| std::io::Error::other("directory walk error")),
					};
					// An entry gone or replaced since its directory was listed, or a directory gone
					// or replaced by a file before its listing was read, changed under the walk:
					// partial as above, but nothing is wrong with the tree, so nothing is reported.
					// A missing pair root is reported, and so is an error that names no path, such
					// as the open the loop check makes on a followed directory: nothing places it
					// below the root.
					if below_root && !unfollowable && changed_under_walk(&source) {
						window.caught_mid_change(&rel_path, source);
						continue;
					}
					window.push(Listed::Error(ScanError::Io { rel_path, source }))?;
					continue;
				}
			};

			let depth = start_depth + entry.depth();
			// `file_type` is the followed type, as the walk follows links.
			let is_dir = entry.file_type().is_dir();
			let rel = entry.path().strip_prefix(root).ok();
			// `None` for a non-UTF-8 path, which the filter lets through and the read reports.
			let mut rel_path = rel.filter(|_| depth > 0).and_then(normalize_rel_path);
			let admitted = depth == 0
				|| 'filter: {
					// The engine's own files come before every rule, so no pattern can re-include one.
					// Never descend into our own quarantine dir (it holds locally-deleted items).
					if depth == 1 && entry.file_name() == OsStr::new(QUARANTINE_DIR) {
						break 'filter false;
					}
					// A `<uuid>.filendl` FILE is the temp file a download in flight is writing into this
					// very tree (see `Client::download_file_to_path`): partial bytes that must never be
					// read as a local item to upload. A directory with that suffix is a legitimate user
					// item.
					if !is_dir && entry.path().extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT)) {
						break 'filter false;
					}
					let Some(rel_path) = rel_path.as_mut() else {
						break 'filter true;
					};
					// Its ancestors were all accepted, so the leaf's own decision is git's answer. A refused
					// entry is not read below, so the maps take its key.
					if let Some(hit) = rules.decide(rel_path, is_dir) {
						// Hidden either way. But a BUILT-IN default hit with nothing synced at or under it
						// is not a root: it is the `.DS_Store` every folder has, and carrying one per folder
						// through the pass costs every later filter its whole directory count. A user-level
						// or `.filenignore` line makes a root whether or not anything was synced there — the
						// user named that path, and the report says so.
						if hit.origin == Origin::Default && !baseline.tracked(rel_path, is_dir) {
							ignored_default_untracked += 1;
						} else {
							ignored.insert(mem::take(rel_path), hit.into());
						}
						break 'filter false;
					}
					if let Some(target) = alias_target(&entry, canonical_root.as_deref(), depth) {
						// Only targets INSIDE the root have a real path that wins. Two links to one
						// directory outside the root are both still walked, and upload it twice.
						aliased_dirs.insert(mem::take(rel_path), target);
						break 'filter false;
					}
					if is_dir && rule_files.reads(rel_path) {
						load_rule_file(
							root,
							rel_path,
							entry.path(),
							&mut rules,
							&mut ignore_blocked,
							&mut rule_errors,
						);
					}
					true
				};
			if !admitted {
				if is_dir {
					walker.skip_current_dir();
				}
				continue;
			}

			if stopped() || on_listed(entry.path()).is_break() {
				cut = true;
				break;
			}

			// The root itself is the pair's anchor, not a synced item. A subtree walk's start IS one.
			if depth == 0 {
				continue;
			}

			let Some(rel) = rel else {
				continue;
			};
			let Some(rel_path) = rel_path else {
				window.complete = false;
				window.push(Listed::Error(ScanError::NonUtf8Name {
					lossy_path: rel.to_string_lossy().into_owned(),
				}))?;
				continue;
			};

			let blocked_rule_file = entry.file_name() == OsStr::new(FILENIGNORE) && {
				let dir = rel_path.rsplit_once('/').map_or("", |(dir, _)| dir);
				rule_files.reads(dir) && ignore_blocked.contains(dir)
			};
			// A walk with a `moved_hash` asks it in walk order, as each file is listed, so it stats
			// every entry here; a whole walk stats its first few here, and hands the rest over. A file
			// that both miss is still read on a worker, which stats it again.
			let next = if moved_hash.is_some()
				|| max_read_workers == 0
				|| window.next_seq() < INLINE_STATS
			{
				match stat(&entry) {
					Ok(metadata) => {
						let is_file = metadata.is_file();
						let known = is_file
							.then(|| {
								let size = metadata.len();
								let mtime_millis =
									FilenMetaExt::modified(&metadata).timestamp_millis();
								fast_path_hash(rows.get(&rel_path).as_ref(), size, mtime_millis)
									.or_else(|| {
										moved_hash.as_deref_mut().and_then(|moved_hash| {
											moved_hash(&rel_path, size, mtime_millis)
										})
									})
							})
							.flatten();
						if is_file && known.is_none() && max_read_workers > 0 {
							Next::HandOver(None)
						} else {
							Next::Read(Ok(reading(&metadata, |_, _| {
								known.map_or_else(|| hash_file(entry.path()), Ok)
							})))
						}
					}
					Err(source) => Next::Read(Err(source)),
				}
			} else {
				// The cursor is asked in walk order, here. The walker's type only decides whether it is
				// asked: `settle` holds the worker's stat against that type, and drops an entry whose
				// type changed.
				Next::HandOver(
					entry
						.file_type()
						.is_file()
						.then(|| rows.get(&rel_path))
						.flatten(),
				)
			};
			let seq = window.push(Listed::Entry {
				rel_path,
				listed_type: entry.file_type(),
				blocked_rule_file,
				read: None,
			})?;
			let row = match next {
				Next::Read(read) => {
					window.answer(seq, read)?;
					continue;
				}
				Next::HandOver(row) => row,
			};
			let cap = *worker_cap.get_or_insert_with(|| {
				std::thread::available_parallelism()
					.map_or(1, NonZero::get)
					.min(max_read_workers)
			});
			if workers < cap {
				let (jobs, done) = (&job_rx, done_tx.clone());
				match std::thread::Builder::new()
					.name("filen-sync-scan".to_owned())
					.spawn_scoped(scope, move || read_worker(jobs, &done))
				{
					Ok(_) => workers += 1,
					// Out of threads: the walk goes on with the workers it has, and reads on its
					// own thread if it has none.
					Err(err) => {
						tracing::warn!(
							"local scan of {}: could not start a read worker, {workers} running: {err}",
							root.display()
						);
						worker_cap = Some(workers);
					}
				}
			}
			// A job is sent only with a worker started to answer it: the waits on `done_rx` would
			// otherwise never return.
			if workers == 0 {
				window.answer(seq, read_entry(&entry, row.as_ref()))?;
			} else {
				jobs.send((seq, entry, row))
					.expect("the workers' channel outlives the walk (should be impossible)");
			}
		}

		// Whatever is still held waits on a read; the walk is over once every one is in. A stop is
		// seen between answers: a wait lasts until the next read ends.
		while !cut && !window.listed.is_empty() {
			cut = stopped();
			if !cut {
				wait(&mut window)?;
			}
		}
		window.complete &= !cut;
		Ok(window)
	});
	let Window {
		sink,
		mut errors,
		mut complete,
		invalid_names,
		..
	} = walked?;

	// A directory that went away DURING the walk answers `NotFound` for every entry it had already
	// listed, and each of those was skipped just above as an entry that vanished — which on its own
	// is indistinguishable from the tree being emptied one file at a time. One stat tells them
	// apart: if what the walk started from is gone (a removed volume, a share that dropped, a lazily
	// unmounted tree), nothing the walk listed is evidence that anything was deleted.
	if let Err(source) = std::fs::metadata(&start_path) {
		complete = false;
		// A subtree's start gone or replaced is a directory below the root that changed under the
		// walk: partial, but nothing is reported. Only a missing pair root is (a removed volume). A
		// subtree walk's caller, `observe_local`, stats the pair root again and reports a removed
		// volume there.
		if !start.is_empty() && changed_under_walk(&source) {
			tracing::debug!(
				"local scan of {}: {start:?} changed under the walk, leaving the scan partial: \
				 {source}",
				root.display()
			);
		} else {
			record(
				&mut errors,
				root,
				ScanError::Io {
					rel_path: start.to_owned(),
					source,
				},
			);
		}
	}

	errors.extend(rule_errors);
	let scan = LocalScan {
		nodes: sink.finish()?,
		complete,
		errors,
		invalid_names,
		aliased_dirs,
		ignored,
		ignored_default_untracked,
		ignore_blocked,
	};
	Ok((scan, rules))
}

/// [`scan_local_staged`]'s walk started at the root-relative directory `start` instead of the pair
/// root: the same walk over one subtree, producing the same ROOT-relative keys — the
/// directory-level re-observation of a change-scoped pass (`observe::observe_local`).
///
/// `rules` must already carry the `.filenignore` files of `start`'s ancestors, and `start` must not
/// itself be hidden by them: what a whole-tree walk would have had in force when it reached `start`
/// is what decides the contents. `observe::observe_local` stats and loads the ancestors for exactly
/// that reason.
///
/// `start` is a node of the scan like any other. Only the pair root is skipped, and only because it
/// is the pair's anchor rather than a synced item.
///
/// `moved_hash` is asked about a file only once the fast path has missed at the file's own path,
/// with its root-relative path, size and mtime; a hash it answers is taken instead of reading the
/// file, and `None` reads it. It is how `observe::observe_local` hands a directory it proved renamed
/// the hashes of the rows its files left. An answer skips the read and with it any error the read
/// would have reported, so what the hook answers for is the caller's trust; a caller with no rename
/// to offer passes `&mut |_, _, _| None`, and the walk is the one it always was.
///
/// `None` for a walk `stop` cut short. It is asked before each entry is read, with how many entries
/// below `start` the walk has listed by then, that one included: a change-scoped pass stops a walk
/// that lists more than it would hold a node for each of, and reads whole instead. What such a
/// walk found is evidence of nothing, so none of it is handed back.
pub(crate) fn scan_subtree(
	root: &Path,
	start: &str,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: &RuleFiles,
	moved_hash: &mut dyn FnMut(&str, u64, i64) -> Option<Blake3Hash>,
	stop: &dyn Fn(usize) -> bool,
) -> Option<(LocalScan, IgnoreRules)> {
	let start_path = root.join(start);
	let mut listed = 0usize;
	let mut cut = false;
	let Ok(walked) = scan_local_watched(
		root,
		start,
		baseline,
		rules,
		rule_files,
		Hooks {
			on_listed: &mut |at| {
				// The start is listed too, and is no entry below it.
				listed += usize::from(at != start_path);
				cut = stop(listed);
				if cut {
					ControlFlow::Break(())
				} else {
					ControlFlow::Continue(())
				}
			},
			stop: None,
			moved_hash: Some(moved_hash),
			max_read_workers: MAX_READ_WORKERS,
		},
		// Sized to the walk and not to the pair: a subtree walk covers one directory, and the pair's
		// count would size its table to the tree instead — a 50k-entry directory of a 100k-row pair
		// was walked into a 13.8 MB table where the entries it found need half that, and the gap
		// grows with the pair. It starts at a walk's worth and grows to what it finds.
		MapSink::with_capacity(1024),
	);
	(!cut).then_some(walked)
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::super::side::{Nodes, NodesAt, test_support::sorted_entries};

	use rand::{Rng, SeedableRng, rngs::StdRng};
	use uuid::Uuid;

	use super::*;
	use crate::sync_engine::{baseline::BaselineState, ignore::IgnoreLevel};

	fn temp_root() -> std::path::PathBuf {
		let dir = std::env::temp_dir().join(format!("filen_scan_test_{}", Uuid::new_v4()));
		fs::create_dir_all(&dir).unwrap();
		dir
	}

	/// A scan with no user level, reading the `.filenignore` files on disk.
	fn scan_plain(root: &Path, baseline: &HashMap<String, BaselineEntry>) -> LocalScan {
		scan_local(
			root,
			&tree(baseline),
			IgnoreRules::default(),
			RuleFiles::Read,
		)
		.0
	}

	fn sorted_paths(scan: &LocalScan) -> Vec<String> {
		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort_unstable();
		paths
	}

	#[test]
	fn normalize_rel_path_keeps_ascii_and_composes_decomposed_names() {
		let key = |path: &str| normalize_rel_path(Path::new(path));
		assert_eq!(key("a/b.txt").as_deref(), Some("a/b.txt"));
		assert_eq!(key("single").as_deref(), Some("single"));
		assert_eq!(key("").as_deref(), Some(""));
		assert_eq!(key("cafe\u{301}").as_deref(), Some("caf\u{e9}"));
		assert_eq!(
			key("Docs/re\u{301}sume\u{301}/caf\u{e9}/x.txt").as_deref(),
			Some("Docs/r\u{e9}sum\u{e9}/caf\u{e9}/x.txt")
		);
		// Hangul jamo compose too, and an already composed name is kept byte for byte.
		assert_eq!(
			key("\u{1100}\u{1161}/\u{ac00}").as_deref(),
			Some("\u{ac00}/\u{ac00}")
		);
		assert_eq!(key("../escape"), None);
		assert_eq!(key("/abs"), None);
	}

	#[cfg(unix)]
	#[test]
	fn normalize_rel_path_refuses_a_non_utf8_component() {
		use std::os::unix::ffi::OsStrExt;
		let path = Path::new(OsStr::from_bytes(b"ok/bad\xff/leaf"));
		assert_eq!(normalize_rel_path(path), None);
	}

	#[test]
	fn collision_key_folds_case() {
		assert_eq!(collision_key("A/B.txt"), "a/b.txt");
		assert_eq!(collision_key("already/low.txt"), "already/low.txt");
		assert_eq!(collision_key("Dir/İstanbul.TXT"), "dir/i\u{307}stanbul.txt");
		assert_eq!(collision_key("ẞ/Ä"), "ß/ä");
	}

	#[test]
	fn collision_key_folds_ascii_like_the_full_unicode_fold() {
		let unicode_fold = |s: &str| s.chars().flat_map(char::to_lowercase).collect::<String>();
		let every_ascii_char = (0u8..=127).map(char::from).collect::<String>();
		for path in [
			every_ascii_char.as_str(),
			"MiXeD/Case/Path.TXT",
			"ZZ/@[`{/~_-",
		] {
			assert_eq!(collision_key(path), unicode_fold(path), "{path:?}");
		}
	}

	#[test]
	fn scans_a_tree_into_normalized_nodes() {
		let root = temp_root();
		fs::write(root.join("a.txt"), b"hello").unwrap();
		fs::create_dir(root.join("sub")).unwrap();
		fs::write(root.join("sub").join("b.txt"), b"world").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert!(
			scan.complete,
			"a clean tree scans completely: {:?}",
			scan.errors
		);

		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);

		let nodes = scan.nodes.whole();
		let a = nodes.at("a.txt").unwrap();
		assert_eq!(a.kind, NodeKind::File);
		assert_eq!(a.size, 5);
		assert!(
			a.content_hash.is_some(),
			"files are hashed without a baseline"
		);
		assert_eq!(nodes.at("sub").unwrap().kind, NodeKind::Dir);
		assert!(
			nodes.at("sub").unwrap().content_hash.is_none(),
			"dirs have no hash"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn fast_path_reuses_baseline_hash_when_size_and_mtime_match() {
		let root = temp_root();
		let file = root.join("a.txt");
		fs::write(&file, b"hello").unwrap();
		let meta = fs::metadata(&file).unwrap();
		let mtime = FilenMetaExt::modified(&meta).timestamp_millis();

		// A baseline whose (size, mtime) match the file but carries a SENTINEL hash that is NOT the
		// real content hash. The fast-path must hand back the sentinel without re-hashing.
		let sentinel = Blake3Hash::from([0xAB; 32]);
		let baseline = HashMap::from([(
			"a.txt".to_string(),
			BaselineEntry {
				rel_path: "a.txt".to_string(),
				kind: NodeKind::File,
				remote_uuid: None,
				content_hash: Some(sentinel),
				size: Some(5),
				local_mtime: Some(mtime),
				remote_modified: None,
				state: BaselineState::Synced,
				local_kind: None,
				remote_kind: None,
				remote_hash: None,
				remote_size: None,
				remote_stable_uuid: None,
				agreed_hash: None,
			},
		)]);

		let scan = scan_plain(&root, &baseline);
		assert_eq!(
			scan.nodes.whole().at("a.txt").unwrap().content_hash,
			Some(sentinel),
			"unchanged (size, mtime) reuses the baseline hash — no re-hash"
		);

		// A baseline with a stale size forces a real re-hash (sentinel must NOT survive).
		let stale = HashMap::from([(
			"a.txt".to_string(),
			BaselineEntry {
				size: Some(999),
				..baseline["a.txt"].clone()
			},
		)]);
		let rescan = scan_plain(&root, &stale);
		assert_ne!(
			rescan.nodes.whole().at("a.txt").unwrap().content_hash,
			Some(sentinel),
			"a diverged size triggers a fresh hash"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn missing_root_is_reported_incomplete() {
		let root = std::env::temp_dir().join(format!("filen_scan_absent_{}", Uuid::new_v4()));
		let scan = scan_plain(&root, &HashMap::new());
		assert!(
			!scan.complete,
			"a missing root scans incomplete (guards mass-delete)"
		);
		assert!(scan.nodes.whole().is_empty());
		assert!(!scan.errors.is_empty());
	}

	#[test]
	fn in_flight_download_temp_files_are_excluded() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		// What `download_file_to_path` writes into the tree while a transfer is in flight.
		fs::write(
			root.join("dee76e0e-0000-0000-0000-000000000000.filendl"),
			b"half",
		)
		.unwrap();
		// A DIRECTORY with that suffix is a legitimate user item and stays.
		fs::create_dir(root.join("notes.filendl")).unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["keep.txt", "notes.filendl"],
			"a partial download must never be seen as a local file to upload"
		);
		assert!(
			scan.complete,
			"skipping our own staging file is not an error"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_name_the_remote_would_reject_is_reported_once_but_still_observed() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		// `CON` is a reserved device name and `bad.` ends in a dot: both are creatable on unix and
		// both are refused by the SDK's own name validator, so no upload of them can ever succeed.
		fs::write(root.join("CON"), b"reserved").unwrap();
		fs::create_dir(root.join("bad.")).unwrap();
		fs::write(root.join("bad.").join("inner.txt"), b"child").unwrap();
		fs::create_dir_all(root.join("bad.").join("deeper")).unwrap();
		fs::write(root.join("bad.").join("deeper").join("x.txt"), b"deep").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		// The scan reports the disk as it is: a rejected name is unpushable, not invisible. Dropping
		// it here would make a rename INTO such a name look like a local deletion one layer up.
		assert_eq!(
			paths,
			vec![
				"CON",
				"bad.",
				"bad./deeper",
				"bad./deeper/x.txt",
				"bad./inner.txt",
				"keep.txt"
			]
		);
		assert!(
			scan.nodes.whole().at("CON").unwrap().content_hash.is_some(),
			"a rejected name is hashed like any other file, so a rename of a synced file into one \
			 is still recognized as that file"
		);

		let reported: Vec<_> = scan.invalid_names.keys().cloned().collect();
		assert_eq!(
			reported,
			vec!["CON", "bad."],
			"each rejected name is reported ONCE, not once per descendant"
		);
		assert!(
			scan.invalid_names["bad."].contains("dot or space"),
			"the report carries the validator's own reason: {:?}",
			scan.invalid_names["bad."]
		);

		// Crucially the scan still COMPLETED: an unsyncable name is not missing evidence, so the
		// delete guard must not start holding every deletion because of it.
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_valid_name_that_merely_contains_a_rejected_one_is_kept() {
		let root = temp_root();
		for name in ["CONSOLE", "console.txt", "CON.txt", "a.b", ".hidden"] {
			fs::write(root.join(name), b"x").unwrap();
		}
		let scan = scan_plain(&root, &HashMap::new());
		assert!(
			scan.invalid_names.is_empty(),
			"none of these are rejected by the SDK validator: {:?}",
			scan.invalid_names
		);
		assert_eq!(scan.nodes.whole().len(), 5);
		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn quarantine_dir_is_excluded() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let quarantine = root.join(QUARANTINE_DIR);
		fs::create_dir(&quarantine).unwrap();
		fs::write(quarantine.join("trashed.txt"), b"old").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		let paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		assert_eq!(
			paths,
			vec!["keep.txt"],
			"the quarantine subtree is not scanned"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[cfg(unix)]
	#[test]
	fn an_unreadable_directory_is_reported_with_its_path() {
		use std::os::unix::fs::PermissionsExt;

		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		fs::create_dir(root.join("locked")).unwrap();
		fs::write(root.join("locked").join("inner.txt"), b"y").unwrap();
		fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o755)).unwrap();

		assert!(!scan.complete, "an unreadable subtree is missing evidence");
		let reported: Vec<String> = scan.reported_errors().collect();
		assert_eq!(reported.len(), 1, "{reported:?}");
		assert!(
			reported[0].starts_with("local scan: locked: "),
			"the line names the path it is about: {reported:?}"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// The built-in defaults hide a `.DS_Store` in every folder. Each one is pruned, but only the
	/// ones the baseline still tracks become ignored ROOTS: recording the rest gives the pass a root
	/// under every directory, and each is left out of the report and deletes no row when the pass
	/// untracks. A tracked one — a file with a row, or a directory with a row under it — is a root
	/// exactly as before.
	#[test]
	fn a_default_rule_hit_is_a_root_only_where_a_row_sits_at_or_under_it() {
		let root = temp_root();
		for dir in ["a", "b", "c"] {
			fs::create_dir(root.join(dir)).unwrap();
			fs::write(root.join(dir).join(".DS_Store"), b"x").unwrap();
			fs::write(root.join(dir).join("keep.txt"), b"y").unwrap();
		}
		// A default-ignored DIRECTORY, which is the case that has to look at a whole subtree.
		fs::create_dir(root.join("a").join(".Trashes")).unwrap();
		fs::write(root.join("a").join(".Trashes").join("t.bin"), b"z").unwrap();

		let untracked = scan_plain(&root, &HashMap::new());
		assert!(
			untracked.ignored.is_empty(),
			"nothing was ever synced there, so none of it is a root: {:?}",
			untracked.ignored
		);
		assert_eq!(
			untracked.ignored_default_untracked, 4,
			"3 files and 1 directory"
		);
		assert!(
			!untracked.nodes.whole().holds("a/.DS_Store")
				&& !untracked.nodes.whole().holds("a/.Trashes"),
			"they are still hidden, and still not descended into"
		);

		let row = |rel_path: &str, kind: NodeKind| BaselineEntry {
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
		};
		let baseline = HashMap::from([
			(
				"b/.DS_Store".to_string(),
				row("b/.DS_Store", NodeKind::File),
			),
			(
				"a/.Trashes/t.bin".to_string(),
				row("a/.Trashes/t.bin", NodeKind::File),
			),
		]);

		let tracked = scan_plain(&root, &baseline);
		assert_eq!(
			tracked.ignored.keys().collect::<Vec<_>>(),
			vec!["a/.Trashes", "b/.DS_Store"],
			"a row AT the path and a row UNDER it both make a root"
		);
		assert_eq!(
			tracked.ignored_default_untracked, 2,
			"the other two `.DS_Store`s are still untracked"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A FILE that is removed between the walk listing it and the scan reading it is not missing
	/// evidence: the walk covered the whole tree and the entry is genuinely gone. Calling such a
	/// scan incomplete holds every deletion of the pair, and a temp file written and removed while
	/// the walk runs (a build, an editor saving) is enough to do it.
	#[test]
	fn a_file_that_vanishes_after_the_walk_listed_it_keeps_the_scan_complete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		fs::write(root.join("temp.txt"), b"y").unwrap();

		let baseline = HashMap::new();
		let scan = scan_watched(
			&root,
			&tree(&baseline),
			// Removed the moment the walker hands the entry over, before the scan stats it.
			&mut |path| {
				if path.ends_with("temp.txt") {
					fs::remove_file(path).unwrap();
				}
				ControlFlow::Continue(())
			},
		);

		assert!(
			scan.complete,
			"an entry the walk itself listed and that is gone now is not missing evidence: {:?}",
			scan.errors
		);
		assert!(
			scan.errors.is_empty(),
			"nothing to report either — it is not an error that a file was deleted: {:?}",
			scan.errors
		);
		assert_eq!(
			sorted_paths(&scan),
			vec!["keep.txt"],
			"what is gone is gone: it is absent, which is what the next pass acts on"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A directory the walk has gone into and that is something else by the time it is read was
	/// caught mid-change: entries may have been listed under what it was, so neither a node at its
	/// path nor its absence is evidence, and the scan is partial instead. Nothing is wrong with the
	/// tree, so the read reports nothing. There are more of them than the walk stats inline, so the
	/// workers' stats meet it too.
	// Windows does not remove a directory the walker holds open.
	#[cfg(unix)]
	#[test]
	fn a_directory_replaced_by_a_file_once_listed_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let swapped = INLINE_STATS + 16;
		for i in 0..swapped {
			fs::create_dir(root.join(format!("swap{i:03}"))).unwrap();
		}

		let baseline = HashMap::new();
		// A name recreated while its directory is still being listed may be listed again, with its
		// new type (POSIX leaves that open): each one is swapped once.
		let mut done = HashSet::new();
		let scan = scan_watched(
			&root,
			&tree(&baseline),
			// The walker has opened the directory by the time it hands it over.
			&mut |path| {
				if path
					.file_name()
					.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
					&& done.insert(path.to_owned())
				{
					fs::remove_dir(path).unwrap();
					fs::write(path, b"now a file").unwrap();
				}
				ControlFlow::Continue(())
			},
		);

		assert!(!scan.complete);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_swapped_from(&scan, NodeKind::Dir);

		fs::remove_dir_all(&root).ok();
	}

	/// The mirror case: a file the walk listed, and so did not go into, that is a directory by the
	/// time it is read had none of its entries listed. A node with nothing under it would read every
	/// synced child as a deletion, so the scan is partial instead, and again reports nothing.
	#[test]
	fn a_file_replaced_by_a_directory_once_listed_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let swapped = INLINE_STATS + 16;
		for i in 0..swapped {
			fs::write(root.join(format!("swap{i:03}")), b"a file").unwrap();
		}

		let baseline = HashMap::new();
		// Swapped once each, as above.
		let mut done = HashSet::new();
		let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
			if path
				.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
				&& done.insert(path.to_owned())
			{
				fs::remove_file(path).unwrap();
				fs::create_dir(path).unwrap();
			}
			ControlFlow::Continue(())
		});

		assert!(!scan.complete);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_swapped_from(&scan, NodeKind::File);

		fs::remove_dir_all(&root).ok();
	}

	/// A file the walk listed that is a symlink by the time it is read is neither a file nor a
	/// directory to the read, which stats the entry itself. Dropped as unsyncable, it would read as
	/// the file's deletion, so the scan is partial instead, and again reports nothing.
	#[cfg(unix)]
	#[test]
	fn a_file_replaced_by_a_symlink_once_listed_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let swapped = INLINE_STATS + 16;
		for i in 0..swapped {
			fs::write(root.join(format!("swap{i:03}")), b"a file").unwrap();
		}

		let baseline = HashMap::new();
		// Swapped once each, as above. A swapped name listed again is a link to `keep.txt`, and so a
		// file again.
		let mut done = HashSet::new();
		let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
			if path
				.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("swap"))
				&& done.insert(path.to_owned())
			{
				fs::remove_file(path).unwrap();
				std::os::unix::fs::symlink(root.join("keep.txt"), path).unwrap();
			}
			ControlFlow::Continue(())
		});

		assert!(!scan.complete);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		let nodes = sorted_entries(&scan.nodes.whole());
		assert!(
			nodes.iter().any(|(path, _)| path == "keep.txt"),
			"{nodes:?}"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// What a swap test's scan keeps: `keep.txt`, and no node of the type a swapped entry was
	/// listed with. A swapped name listed again with its new type may be a node of that type.
	fn assert_swapped_from(scan: &LocalScan, listed: NodeKind) {
		let nodes = sorted_entries(&scan.nodes.whole());
		assert!(
			nodes.iter().any(|(path, _)| path == "keep.txt"),
			"{nodes:?}"
		);
		assert!(
			!nodes
				.iter()
				.any(|(path, node)| path.starts_with("swap") && node.kind == listed),
			"{nodes:?}"
		);
	}

	/// A directory the walk listed that is gone by the time it is read had its entries listed from
	/// what it was: the scan is partial, and the read reports nothing, as for one replaced by a
	/// file. There are more of them than the walk stats inline, so the workers' stats meet it too.
	// Windows does not remove a directory the walker holds open.
	#[cfg(unix)]
	#[test]
	fn a_directory_removed_once_listed_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let removed = INLINE_STATS + 16;
		for i in 0..removed {
			fs::create_dir(root.join(format!("gone{i:03}"))).unwrap();
		}

		let baseline = HashMap::new();
		let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
			if path
				.file_name()
				.is_some_and(|name| name.to_string_lossy().starts_with("gone"))
			{
				fs::remove_dir(path).unwrap();
			}
			ControlFlow::Continue(())
		});

		assert!(!scan.complete);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_eq!(sorted_paths(&scan), vec!["keep.txt"]);

		fs::remove_dir_all(&root).ok();
	}

	/// A DIRECTORY the walk listed but could not descend into is the opposite case: its children
	/// were never listed, so their absence is not something the walk observed. Trusting it reads
	/// every file under a directory that was moved or removed mid-walk as a deletion — and the
	/// destination, listed before the move, is not in the scan either, so the pass cannot see the
	/// move. One held pass costs nothing; the next one folds it. Nothing is wrong with the tree, so
	/// nothing is reported.
	#[test]
	fn a_directory_the_walk_could_not_descend_into_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		// `walkdir` opens a directory as it hands the entry over, so the entry that vanishes
		// BETWEEN listing and descent is one of the siblings already buffered in an open stream.
		let outer = root.join("outer");
		fs::create_dir(&outer).unwrap();
		for i in 0..64 {
			let dir = outer.join(format!("d{i:02}"));
			fs::create_dir(&dir).unwrap();
			fs::write(dir.join("inner.txt"), b"z").unwrap();
		}

		let baseline = HashMap::new();
		let mut swept = false;
		let scan = scan_watched(
			&root,
			&tree(&baseline),
			// The first time a child of `outer` is listed, every child of `outer` goes — while the
			// walk still holds that directory's open stream and has yet to descend into any of them.
			&mut |path| {
				if !swept && path.parent() == Some(outer.as_path()) {
					swept = true;
					for child in fs::read_dir(&outer).unwrap() {
						fs::remove_dir_all(child.unwrap().path()).unwrap();
					}
				}
				ControlFlow::Continue(())
			},
		);

		assert!(
			!scan.complete,
			"the children of a directory the walk never descended into were not observed absent"
		);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	/// A file whose directory is replaced by a file once the walk listed it fails its read with
	/// ENOTDIR: that directory changed under the walk, as one gone before the walk descends into it
	/// does, so the scan is partial and nothing is reported. There are more of them than the walk
	/// stats inline, so the workers' stats meet it too. Only the stat is met here: a hash read cannot
	/// be made to fail that way from the outside, between the stat and the read.
	// Windows does not remove a directory the walker holds open.
	#[cfg(unix)]
	#[test]
	fn a_file_whose_directory_is_replaced_by_a_file_leaves_the_scan_incomplete() {
		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		let swapped = INLINE_STATS + 16;
		for i in 0..swapped {
			let dir = root.join(format!("swap{i:03}"));
			fs::create_dir(&dir).unwrap();
			fs::write(dir.join("inner.txt"), b"z").unwrap();
		}

		let baseline = HashMap::new();
		// A name recreated while its directory is still being listed may be listed again: each
		// directory is swapped once.
		let mut done = HashSet::new();
		let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
			if path.ends_with("inner.txt")
				&& let Some(dir) = path.parent()
				&& done.insert(dir.to_owned())
			{
				fs::remove_dir_all(dir).unwrap();
				fs::write(dir, b"now a file").unwrap();
			}
			ControlFlow::Continue(())
		});

		assert_eq!(done.len(), swapped);
		assert!(!scan.complete);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert!(
			!sorted_paths(&scan)
				.iter()
				.any(|path| path.ends_with("inner.txt")),
			"{:?}",
			sorted_paths(&scan)
		);

		fs::remove_dir_all(&root).ok();
	}

	/// `A.txt` and `a.txt` in `root`, two names that fold together; `None` where the filesystem
	/// folds case and holds one file for the two.
	///
	/// No pair of names such a filesystem keeps apart folds together either, since it folds the
	/// way the key does, so no walk there can meet a twin. The cases the twin tests guard are
	/// reachable wherever the filesystem is case-sensitive, which is where a twin can exist at all.
	#[cfg(unix)]
	fn case_twins(root: &Path) -> Option<[std::path::PathBuf; 2]> {
		let (upper, lower) = ("A.txt", "a.txt");
		assert_eq!(
			collision_key(upper),
			collision_key(lower),
			"the fixture needs two names that fold together"
		);
		let twins = [root.join(upper), root.join(lower)];
		fs::write(&twins[0], b"x").unwrap();
		fs::write(&twins[1], b"y").unwrap();
		(fs::read_dir(root).unwrap().count() == 2).then_some(twins)
	}

	/// A twin whose first spelling claimed the folded name and then bailed — a file the scan could
	/// not read — must still refuse the pass. The claim is what says the name is taken; forgetting
	/// it lets the second spelling through as an ordinary new file, and since the server folds case
	/// too, uploading it lands on top of the remote copy the first spelling is synced as.
	#[cfg(unix)]
	#[test]
	fn a_twin_whose_first_spelling_could_not_be_read_is_still_a_collision() {
		use std::os::unix::fs::PermissionsExt;

		let root = temp_root();
		let Some([upper, lower]) = case_twins(&root) else {
			fs::remove_dir_all(&root).ok();
			return;
		};

		let baseline = HashMap::new();
		let mut blocked = false;
		let scan = scan_watched(
			&root,
			&tree(&baseline),
			// Whichever of the two the walk lists FIRST is made unreadable before the scan hashes
			// it, so it claims the folded name and then bails. Which one that is depends on the
			// directory's order, and the answer must not.
			&mut |path| {
				if !blocked && (path == upper || path == lower) {
					blocked = true;
					fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
				}
				ControlFlow::Continue(())
			},
		);

		assert!(
			scan.errors
				.iter()
				.any(|error| matches!(error, ScanError::DuplicateName { .. })),
			"the pass must still be refused over the collision: {:?}",
			scan.errors
		);
		assert!(
			sorted_paths(&scan).is_empty(),
			"and neither spelling may be planned: {:?}",
			sorted_paths(&scan)
		);

		fs::set_permissions(&upper, fs::Permissions::from_mode(0o644)).ok();
		fs::set_permissions(&lower, fs::Permissions::from_mode(0o644)).ok();
		fs::remove_dir_all(&root).ok();
	}

	/// A twin that vanishes once the walk has listed and stat'ed it, before its hash is read, is
	/// gone, as any file that vanishes then is: the other spelling is a node, and the pass is not
	/// refused over a twin that is no longer there. A name is taken once its entry is read, not
	/// before its hash is.
	#[cfg(unix)]
	#[test]
	fn a_twin_that_vanishes_before_its_hash_is_read_is_no_collision() {
		let root = temp_root();
		if case_twins(&root).is_none() {
			fs::remove_dir_all(&root).ok();
			return;
		}

		let mut asked: Vec<String> = Vec::new();
		let Ok((scan, _)) = scan_local_watched(
			&root,
			"",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			Hooks {
				on_listed: &mut |_| ControlFlow::Continue(()),
				stop: None,
				// Asked about each file between its stat and its hash read, with no baseline to
				// take a hash from: the second of the two is removed there.
				moved_hash: Some(&mut |rel_path, _, _| {
					asked.push(rel_path.to_owned());
					if asked.len() == 2 {
						fs::remove_file(root.join(rel_path)).unwrap();
					}
					None
				}),
				max_read_workers: MAX_READ_WORKERS,
			},
			MapSink::with_capacity(4),
		);

		let [first, _] = asked.as_slice() else {
			panic!("the walk reads both twins: {asked:?}")
		};
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_eq!(sorted_paths(&scan), vec![first.clone()]);

		fs::remove_dir_all(&root).ok();
	}

	/// A twin whose hash cannot be read, reached once the other spelling holds the name, reports
	/// both what stopped the read and the collision: the pass is refused over the twin, and told
	/// why that file was not read.
	#[cfg(unix)]
	#[test]
	fn a_twin_whose_second_spelling_could_not_be_read_reports_the_read_and_the_collision() {
		use std::os::unix::fs::PermissionsExt;

		let root = temp_root();
		let Some(twins) = case_twins(&root) else {
			fs::remove_dir_all(&root).ok();
			return;
		};

		let baseline = HashMap::new();
		let mut listed: Vec<String> = Vec::new();
		let scan = scan_watched(
			&root,
			&tree(&baseline),
			// The second of the two the walk lists is made unreadable before the scan hashes it.
			&mut |path| {
				if twins.iter().any(|twin| twin == path) {
					let name = path.file_name().and_then(OsStr::to_str).unwrap();
					listed.push(name.to_owned());
					if listed.len() == 2 {
						fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
					}
				}
				ControlFlow::Continue(())
			},
		);

		for twin in &twins {
			fs::set_permissions(twin, fs::Permissions::from_mode(0o644)).ok();
		}
		let [first, second] = listed.as_slice() else {
			panic!("the walk lists both twins: {listed:?}")
		};
		assert!(!scan.complete);
		let [
			ScanError::Io {
				rel_path: unread, ..
			},
			ScanError::DuplicateName { rel_path: named },
		] = scan.errors.as_slice()
		else {
			panic!("the read error, then the collision: {:?}", scan.errors)
		};
		assert_eq!(unread, second);
		assert_eq!(named, &format!("{first} / {second}"));
		assert_eq!(sorted_paths(&scan), vec![first.clone()]);

		fs::remove_dir_all(&root).ok();
	}

	/// A root that goes away DURING the walk — a removed volume, an unreachable share — answers
	/// `NotFound` for every entry it had already listed, which on its own is indistinguishable from
	/// each of those entries being deleted. One stat after the walk tells the two apart; without it
	/// the scan reports a complete observation of an empty tree, and every row of the pair reads as
	/// a deletion.
	#[test]
	fn a_root_that_vanishes_during_the_walk_leaves_the_scan_incomplete() {
		let root = temp_root();
		// Flat on purpose: no directory to descend into, so the root check is the only thing that
		// can catch this.
		for name in ["a.txt", "b.txt", "c.txt"] {
			fs::write(root.join(name), b"x").unwrap();
		}

		let baseline = HashMap::new();
		let mut gone = false;
		let scan = scan_watched(&root, &tree(&baseline), &mut |path| {
			if !gone && path.parent() == Some(root.as_path()) {
				gone = true;
				fs::remove_dir_all(&root).unwrap();
			}
			ControlFlow::Continue(())
		});

		assert!(
			!scan.complete,
			"the root itself is gone: nothing the walk listed is evidence of a deletion"
		);
		assert!(
			matches!(
				scan.errors.as_slice(),
				[ScanError::Io { rel_path, source }]
					if rel_path.is_empty() && source.kind() == ErrorKind::NotFound
			),
			"a missing pair root is a removed volume, and is reported: {:?}",
			scan.errors
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_name_collision_is_not_reported_as_an_error_line() {
		let scan = LocalScan {
			nodes: Side::default(),
			complete: false,
			errors: vec![
				ScanError::DuplicateName {
					rel_path: "A.txt / a.txt".to_string(),
				},
				ScanError::NonUtf8Name {
					lossy_path: "bad\u{FFFD}".to_string(),
				},
				ScanError::Io {
					rel_path: String::new(),
					source: std::io::Error::from(std::io::ErrorKind::NotFound),
				},
			],
			invalid_names: BTreeMap::new(),
			aliased_dirs: BTreeMap::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
			ignore_blocked: BTreeSet::new(),
		};
		let reported: Vec<String> = scan.reported_errors().collect();
		assert_eq!(
			reported,
			vec![
				"local scan: bad\u{FFFD}: the name is not valid UTF-8".to_string(),
				format!(
					"local scan: local root: {}",
					std::io::Error::from(std::io::ErrorKind::NotFound)
				),
			],
			"a collision refuses the pass with its own line; everything else is reported"
		);
	}

	#[cfg(unix)]
	#[test]
	fn a_dangling_symlink_or_a_loop_is_reported_but_keeps_the_scan_complete() {
		use std::os::unix::fs::symlink;

		let root = temp_root();
		fs::write(root.join("keep.txt"), b"x").unwrap();
		symlink(root.join("nowhere"), root.join("dangling")).unwrap();
		symlink(root.join("itself"), root.join("itself")).unwrap();
		fs::create_dir(root.join("cycle")).unwrap();
		symlink(root.join("cycle"), root.join("cycle").join("self")).unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert!(
			scan.complete,
			"a link that leads nowhere hides nothing, so deletions must not be held for it: {:?}",
			scan.errors
		);
		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		assert_eq!(paths, vec!["cycle", "keep.txt"]);

		let mut errored: Vec<&str> = scan
			.errors
			.iter()
			.map(|e| match e {
				ScanError::Io { rel_path, .. } => rel_path.as_str(),
				other => panic!("unexpected scan error {other:?}"),
			})
			.collect();
		errored.sort_unstable();
		assert_eq!(errored, vec!["cycle/self", "dangling", "itself"]);
		assert_eq!(scan.reported_errors().count(), 3);

		fs::remove_dir_all(&root).ok();
	}

	#[cfg(unix)]
	#[test]
	fn a_symlink_to_a_directory_inside_the_root_is_scanned_once_under_its_real_path() {
		use std::os::unix::fs::symlink;

		let root = temp_root();
		let outside = temp_root();
		fs::write(outside.join("far.txt"), b"far").unwrap();
		fs::create_dir_all(root.join("real").join("sub")).unwrap();
		fs::write(root.join("real").join("a.txt"), b"a").unwrap();
		fs::write(root.join("real").join("sub").join("b.txt"), b"b").unwrap();
		// Links named to sort before and after their target, so walk order cannot decide the winner.
		symlink(root.join("real"), root.join("alias")).unwrap();
		symlink(root.join("real"), root.join("zlias")).unwrap();
		// Nested inside its own target's parent, and spelled relative.
		symlink("sub", root.join("real").join("inner")).unwrap();
		// A link to a file inside the root stays a copy; a link leading outside is walked.
		symlink(root.join("real").join("a.txt"), root.join("copy.txt")).unwrap();
		symlink(&outside, root.join("away")).unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		let mut paths: Vec<String> = scan
			.nodes
			.whole()
			.paths()
			.map(|path| path.into_owned())
			.collect();
		paths.sort();
		assert_eq!(
			paths,
			vec![
				"away",
				"away/far.txt",
				"copy.txt",
				"real",
				"real/a.txt",
				"real/sub",
				"real/sub/b.txt",
			]
		);
		assert_eq!(
			scan.aliased_dirs,
			BTreeMap::from([
				("alias".to_string(), "real".to_string()),
				("real/inner".to_string(), "real/sub".to_string()),
				("zlias".to_string(), "real".to_string()),
			])
		);
		let mut blocked: Vec<&String> = scan
			.invalid_names
			.keys()
			.chain(scan.aliased_dirs.keys())
			.collect();
		blocked.sort();
		assert_eq!(blocked, vec!["alias", "real/inner", "zlias"]);

		fs::remove_dir_all(&root).ok();
		fs::remove_dir_all(&outside).ok();
	}

	/// A link synced as a copy of the file it pointed at, whose target is then deleted, is a
	/// dangling link to a FILE: reported, but the scan stays complete, so the target's deletion (and
	/// every other one) is not held for as long as the link exists.
	#[cfg(unix)]
	#[test]
	fn a_dangling_file_symlink_the_baseline_tracks_keeps_the_scan_complete() {
		use std::os::unix::fs::symlink;

		let root = temp_root();
		symlink(root.join("target.txt"), root.join("link.txt")).unwrap();
		let file_row = |rel_path: &str| BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: None,
			content_hash: None,
			size: Some(1),
			local_mtime: Some(1),
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		};
		let baseline = HashMap::from([
			("link.txt".to_string(), file_row("link.txt")),
			("target.txt".to_string(), file_row("target.txt")),
		]);

		let scan = scan_plain(&root, &baseline);
		assert!(
			scan.complete,
			"a dead link to a synced file must not hold every deletion: {:?}",
			scan.errors
		);
		assert_eq!(scan.reported_errors().count(), 1, "{:?}", scan.errors);
		assert!(
			scan.nodes.whole().is_empty(),
			"{:?}",
			scan.nodes.whole().paths().collect::<Vec<_>>()
		);

		fs::remove_dir_all(&root).ok();
	}

	#[cfg(unix)]
	#[test]
	fn a_dangling_symlink_the_baseline_tracks_marks_the_scan_incomplete() {
		use std::os::unix::fs::symlink;

		let root = temp_root();
		// A link to a directory that was synced through it, and is now gone (an unmounted drive).
		symlink(root.join("unmounted"), root.join("media")).unwrap();
		let baseline = HashMap::from([(
			"media".to_string(),
			BaselineEntry {
				rel_path: "media".to_string(),
				kind: NodeKind::Dir,
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
			},
		)]);

		let scan = scan_plain(&root, &baseline);
		assert!(
			!scan.complete,
			"what was synced behind the link must not read as deleted"
		);
		assert_eq!(scan.reported_errors().count(), 1, "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn an_ignored_entry_is_pruned_and_reported_top_most() {
		let root = temp_root();
		fs::write(root.join(FILENIGNORE), "build/\n*.log\n").unwrap();
		fs::create_dir_all(root.join("build").join("deep")).unwrap();
		fs::write(root.join("build").join("deep").join("a.txt"), b"a").unwrap();
		fs::write(root.join("build").join("b.log"), b"b").unwrap();
		fs::write(root.join("x.log"), b"x").unwrap();
		fs::write(root.join("keep.txt"), b"k").unwrap();
		fs::create_dir_all(root.join("sub").join("tmp")).unwrap();
		fs::write(root.join("sub").join(FILENIGNORE), "/tmp\n").unwrap();
		fs::write(root.join("sub").join("tmp").join("t.txt"), b"t").unwrap();
		fs::write(root.join("sub").join(".DS_Store"), b"junk").unwrap();
		fs::create_dir(root.join("tmp")).unwrap();

		let (scan, rules) = scan_local(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		);
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert_eq!(
			sorted_paths(&scan),
			vec![".filenignore", "keep.txt", "sub", "sub/.filenignore", "tmp"]
		);
		// `build/b.log` matches `*.log` too, but the walk never went into `build`.
		let by = |level: IgnoreLevel, pattern: &str| IgnoreDecision {
			level,
			pattern: pattern.to_string(),
		};
		let root_file = |pattern: &str| by(IgnoreLevel::File { dir: String::new() }, pattern);
		assert_eq!(
			scan.ignored,
			BTreeMap::from([
				("build".to_string(), root_file("build/")),
				(
					"sub/tmp".to_string(),
					by(
						IgnoreLevel::File {
							dir: "sub".to_string()
						},
						"/tmp"
					)
				),
				("x.log".to_string(), root_file("*.log")),
			])
		);
		// `sub/.DS_Store` is hidden and pruned like the rest, but only the built-in defaults hide it
		// and this baseline tracks nothing there, so it is counted rather than carried as a root.
		assert_eq!(scan.ignored_default_untracked, 1);
		assert!(scan.ignore_blocked.is_empty());
		assert!(
			rules.decide("sub/tmp", true).is_some(),
			"the rules come back holding the files the scan read"
		);

		// When the remote copies are the ones that count, the files on disk are ordinary items.
		let (skipped, rules) = scan_local(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Only(BTreeSet::new()),
		);
		assert!(skipped.nodes.whole().holds("build/deep/a.txt"));
		// With no rule file read, the built-in defaults are all that hides anything, and nothing was
		// synced at the one entry they hide.
		assert!(
			skipped.ignored.is_empty(),
			"{:?} is hidden but untracked, so it is no root",
			skipped.ignored
		);
		assert_eq!(skipped.ignored_default_untracked, 1);
		assert!(rules.decide("x.log", false).is_none());
		// Except the ones named: a synced rule file the remote has lost still governs.
		let (only, rules) = scan_local(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Only(BTreeSet::from(["sub".to_string()])),
		);
		assert!(only.nodes.whole().holds("build/deep/a.txt"));
		assert_eq!(only.ignored.keys().collect::<Vec<_>>(), vec!["sub/tmp"]);
		assert_eq!(only.ignored_default_untracked, 1);
		assert!(rules.decide("x.log", false).is_none());

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn ignored_case_twins_do_not_refuse_the_pass() {
		let root = temp_root();
		fs::write(root.join("Twin.txt"), b"a").unwrap();
		fs::write(root.join("twin.txt"), b"b").unwrap();
		// A case-insensitive volume (the macOS and Windows defaults) keeps one file under both
		// names, and there is nothing to collide.
		if fs::read_dir(&root).unwrap().count() == 2 {
			let unruled = scan_plain(&root, &HashMap::new());
			assert!(!unruled.complete, "without a rule the twins collide");

			fs::write(root.join(FILENIGNORE), "TWIN.TXT\n").unwrap();
			let scan = scan_plain(&root, &HashMap::new());
			assert!(scan.complete, "{:?}", scan.errors);
			assert!(scan.errors.is_empty(), "{:?}", scan.errors);
			assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
			assert_eq!(
				scan.ignored.keys().collect::<Vec<_>>(),
				vec!["Twin.txt", "twin.txt"]
			);
		}
		fs::remove_dir_all(&root).ok();
	}

	#[cfg(unix)]
	#[test]
	fn an_unusable_filenignore_blocks_its_directory_and_keeps_the_scan_complete() {
		use std::os::unix::fs::PermissionsExt;

		let root = temp_root();
		let locked = root.join("locked");
		fs::create_dir(&locked).unwrap();
		fs::write(locked.join(FILENIGNORE), "*\n").unwrap();
		fs::write(locked.join("a.txt"), b"a").unwrap();
		fs::set_permissions(locked.join(FILENIGNORE), fs::Permissions::from_mode(0o000)).unwrap();
		// One bad line: reported, and the rest of the file still applies.
		fs::create_dir(root.join("bad")).unwrap();
		fs::write(root.join("bad").join(FILENIGNORE), "[z-a]\n*.o\n").unwrap();
		fs::write(root.join("bad").join("x.o"), b"o").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		fs::set_permissions(locked.join(FILENIGNORE), fs::Permissions::from_mode(0o644)).unwrap();

		assert!(
			scan.complete,
			"what the rules would cover was withheld, not missed: {:?}",
			scan.errors
		);
		assert_eq!(scan.ignore_blocked, BTreeSet::from(["locked".to_string()]));
		assert!(
			scan.nodes.whole().holds("locked/a.txt"),
			"{:?}",
			scan.nodes.whole().paths().collect::<Vec<_>>()
		);
		assert_eq!(scan.ignored.keys().collect::<Vec<_>>(), vec!["bad/x.o"]);
		let reported: Vec<String> = scan.reported_errors().collect();
		assert_eq!(reported.len(), 2, "{reported:?}");
		assert!(
			reported
				.iter()
				.any(|line| line.starts_with("local scan: locked/.filenignore: ")),
			"{reported:?}"
		);
		assert!(
			reported
				.iter()
				.any(|line| line.starts_with("local scan: bad/.filenignore:1: ")),
			"{reported:?}"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A body the remote side would refuse (over the size cap, or not UTF-8) blocks its directory on
	/// disk too, so the device that wrote it sees the same error every other device does.
	#[test]
	fn a_rule_file_the_remote_would_refuse_blocks_its_directory_on_disk_too() {
		let root = temp_root();
		fs::create_dir(root.join("latin")).unwrap();
		fs::write(root.join("latin").join(FILENIGNORE), b"caf\xe9/\n").unwrap();
		fs::write(root.join("latin").join("a.txt"), b"a").unwrap();
		fs::create_dir(root.join("huge")).unwrap();
		let too_large = usize::try_from(MAX_RULE_FILE_BYTES).unwrap() + 1;
		fs::write(root.join("huge").join(FILENIGNORE), vec![b'#'; too_large]).unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert!(scan.complete, "{:?}", scan.errors);
		assert_eq!(
			scan.ignore_blocked,
			BTreeSet::from(["huge".to_string(), "latin".to_string()])
		);
		let reported: Vec<String> = scan.reported_errors().collect();
		assert_eq!(reported.len(), 2, "{reported:?}");
		for dir in ["huge", "latin"] {
			let prefix = format!("local scan: {dir}/.filenignore: ");
			assert!(
				reported.iter().any(|line| line.starts_with(&prefix)),
				"{prefix}: {reported:?}"
			);
		}

		fs::remove_dir_all(&root).ok();
	}

	/// Only a FILE named exactly `.filenignore` holds rules. A directory of that name is an ordinary
	/// item (Windows reports reading one as access denied, not as a directory), and another spelling is
	/// an ordinary file even where the volume opens it under the rule file's name, as the remote and a
	/// case-sensitive device read it.
	#[test]
	fn only_a_file_named_exactly_filenignore_holds_rules() {
		let root = temp_root();
		fs::create_dir_all(root.join("docs").join(FILENIGNORE)).unwrap();
		fs::write(root.join("docs").join(FILENIGNORE).join("x.txt"), b"x").unwrap();
		fs::create_dir(root.join("proj")).unwrap();
		fs::write(root.join("proj").join(".FilenIgnore"), b"secret\n").unwrap();
		fs::write(root.join("proj").join("secret"), b"s").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert!(scan.complete, "{:?}", scan.errors);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);
		assert!(scan.ignore_blocked.is_empty(), "{:?}", scan.ignore_blocked);
		assert!(scan.ignored.is_empty(), "{:?}", scan.ignored);
		assert_eq!(
			sorted_paths(&scan),
			vec![
				"docs",
				"docs/.filenignore",
				"docs/.filenignore/x.txt",
				"proj",
				"proj/.FilenIgnore",
				"proj/secret"
			]
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_whitelist_inside_an_ignored_directory_has_no_effect() {
		let root = temp_root();
		fs::write(root.join(FILENIGNORE), "dir/\n!dir/keep\n").unwrap();
		fs::create_dir(root.join("dir")).unwrap();
		fs::write(root.join("dir").join(FILENIGNORE), "!keep\nother\n").unwrap();
		fs::write(root.join("dir").join("keep"), b"k").unwrap();

		let (scan, rules) = scan_local(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
		);
		assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
		assert_eq!(scan.ignored.keys().collect::<Vec<_>>(), vec!["dir"]);
		assert!(
			rules.decide("dir/other", false).is_none(),
			"the file inside the ignored directory is never read"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn the_engines_own_files_stay_outside_every_rule() {
		let root = temp_root();
		fs::write(root.join(FILENIGNORE), "!.filen-sync-trash/\n*.filendl\n").unwrap();
		fs::create_dir(root.join(QUARANTINE_DIR)).unwrap();
		fs::write(root.join(QUARANTINE_DIR).join("old.txt"), b"old").unwrap();
		fs::write(root.join("dee76e0e.filendl"), b"half").unwrap();

		let scan = scan_plain(&root, &HashMap::new());
		assert_eq!(sorted_paths(&scan), vec![FILENIGNORE]);
		assert!(
			scan.ignored.is_empty(),
			"an internal file is not an ignored item: {:?}",
			scan.ignored
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A subtree walk keys against the pair ROOT and starts at its own directory. The rules that go
	/// by depth still mean the root: the quarantine bin is the root's own child, so a user directory
	/// of that name nested deeper is an ordinary item.
	#[test]
	fn a_subtree_walk_keys_against_the_root_and_starts_at_its_own_directory() {
		let root = temp_root();
		fs::write(root.join("top.txt"), b"t").unwrap();
		fs::create_dir(root.join(QUARANTINE_DIR)).unwrap();
		fs::write(root.join(QUARANTINE_DIR).join("trashed.txt"), b"old").unwrap();
		let nested = root.join("sub").join(QUARANTINE_DIR);
		fs::create_dir_all(&nested).unwrap();
		fs::write(nested.join("mine.txt"), b"m").unwrap();

		let (scan, _) = scan_subtree(
			&root,
			"sub",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			&mut |_, _, _| None,
			&|_| false,
		)
		.expect("a walk nothing stops");

		assert!(scan.complete, "{:?}", scan.errors);
		assert_eq!(
			sorted_paths(&scan),
			vec![
				"sub",
				"sub/.filen-sync-trash",
				"sub/.filen-sync-trash/mine.txt"
			],
			"the start directory is a node, its keys are root-relative, and only the ROOT's own \
			 quarantine bin is the engine's"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A subtree walk whose start is removed once the walk has opened it finds every entry it lists
	/// gone and the start gone after it. That start is a directory below the root that changed under
	/// the walk, as any other is: the scan is partial and nothing is reported.
	// Windows does not remove a directory the walker holds open.
	#[cfg(unix)]
	#[test]
	fn a_subtree_start_removed_during_the_walk_leaves_the_scan_incomplete_and_unreported() {
		let root = temp_root();
		let start = root.join("sub");
		fs::create_dir(&start).unwrap();
		for name in ["a.txt", "b.txt", "c.txt"] {
			fs::write(start.join(name), b"x").unwrap();
		}

		let (scan, _) = scan_subtree(
			&root,
			"sub",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			&mut |_, _, _| None,
			&|listed| {
				if listed == 1 {
					fs::remove_dir_all(&start).unwrap();
				}
				false
			},
		)
		.expect("a walk nothing stops");

		assert!(!start.exists());
		assert!(
			!scan.complete,
			"what the walk listed under a start that is gone is evidence of nothing"
		);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	/// A subtree start removed after the caller stat'd it and before the walk read it is the same
	/// change caught a moment earlier: the walk's first read answers `NotFound` at the start, and
	/// the scan is partial with nothing reported.
	#[test]
	fn a_subtree_start_gone_before_the_walk_reads_it_leaves_the_scan_incomplete_and_unreported() {
		let root = temp_root();

		let (scan, _) = scan_subtree(
			&root,
			"sub",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			&mut |_, _, _| None,
			&|_| false,
		)
		.expect("a walk nothing stops");

		assert!(
			!scan.complete,
			"a start that is gone lists nothing that is evidence of a deletion"
		);
		assert!(scan.errors.is_empty(), "{:?}", scan.errors);

		fs::remove_dir_all(&root).ok();
	}

	/// A subtree walk is sized to the walk and not to the pair: the pair's row count is what the
	/// WHOLE tree holds, and a table reserved for it around one directory's entries is the tree's
	/// worth of memory spent on a fraction of it.
	#[test]
	fn a_subtree_walk_is_sized_to_the_walk_and_a_whole_one_to_the_pair() {
		let root = temp_root();
		fs::create_dir(root.join("sub")).unwrap();
		fs::write(root.join("sub").join("a.txt"), b"a").unwrap();
		let rows = 4096;
		let baseline = Baseline::from_rows((0..rows).map(|at| BaselineEntry {
			rel_path: format!("elsewhere/{at}.txt"),
			kind: NodeKind::File,
			remote_uuid: None,
			content_hash: Some(Blake3Hash::from([1; 32])),
			size: Some(1),
			local_mtime: Some(1),
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		}));
		assert_eq!(baseline.len(), rows);

		let (subtree, _) = scan_subtree(
			&root,
			"sub",
			&baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			&mut |_, _, _| None,
			&|_| false,
		)
		.expect("a walk nothing stops");
		let whole = scan_local(&root, &baseline, IgnoreRules::default(), RuleFiles::Read).0;

		assert_eq!(sorted_paths(&subtree), vec!["sub", "sub/a.txt"]);
		let subtree_room = subtree.nodes.into_whole().capacity();
		assert!(
			subtree_room < rows,
			"a walk of one directory reserved room for {subtree_room} node(s), the {rows}-row pair's \
			 worth"
		);
		let whole_room = whole.nodes.into_whole().capacity();
		assert!(
			whole_room >= rows,
			"the whole-tree walk still starts with room for the pair: {whole_room}"
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A whole walk into the pass's stage finds what a walk into a map finds — the nodes, every fact
	/// the walk reports and every error — over generated trees: nested directories, a non-ASCII
	/// name, a name the remote rejects, entries a `.filenignore` hides, a fast path that takes a
	/// hash off the baseline rather than the file, and, where a file can be made unreadable, one
	/// whose hash cannot be read: a claim, which neither side holds as a node.
	///
	/// Collisions are the sinks' own test (`stage::local`): a case-insensitive volume — this one, on
	/// macOS — cannot hold the twins a walk would have to meet.
	#[test]
	fn a_whole_walk_into_the_stage_finds_what_a_walk_into_a_map_finds() {
		// No two of these fold together, so any volume holds every one of them.
		const NAMES: &[&str] = &[
			"a",
			"b",
			"docs",
			"notes.md",
			"x.log",
			"\u{c4}rger",
			"CON",
			"bad.",
			"z.txt",
			"keep",
		];
		let mut unreadable = 0usize;
		for seed in 0..16u64 {
			let mut rng = StdRng::seed_from_u64(seed);
			let root = temp_root();
			let mut dirs = vec![root.clone()];
			let mut files = Vec::new();
			for _ in 0..rng.random_range(8..40) {
				let parent = dirs[rng.random_range(0..dirs.len())].clone();
				let path = parent.join(NAMES[rng.random_range(0..NAMES.len())]);
				if path.exists() {
					continue;
				}
				if rng.random_range(0..3) == 0 {
					fs::create_dir(&path).unwrap();
					dirs.push(path);
				} else {
					let size = rng.random_range(0..64);
					fs::write(&path, vec![b'x'; size]).unwrap();
					files.push(path);
				}
			}
			if rng.random_range(0..2) == 0 {
				fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
			}
			// Half the cases take every hash the baseline vouches for off the baseline: a hash
			// no file has, so that only the fast path can have produced it.
			let baseline = if rng.random_range(0..2) == 0 {
				Baseline::default()
			} else {
				let walked = scan_local(
					&root,
					&Baseline::default(),
					IgnoreRules::default(),
					RuleFiles::Read,
				)
				.0;
				Baseline::from_rows(sorted_entries(&walked.nodes.whole()).into_iter().map(
					|(rel_path, node)| BaselineEntry {
						rel_path,
						kind: node.kind,
						remote_uuid: None,
						content_hash: Some(Blake3Hash::from([0xAB; 32])),
						size: Some(node.size),
						local_mtime: Some(node.mtime_millis),
						remote_modified: None,
						state: BaselineState::Synced,
						local_kind: None,
						remote_kind: None,
						remote_hash: None,
						remote_size: None,
						remote_stable_uuid: None,
						agreed_hash: None,
					},
				))
			};
			#[cfg(unix)]
			let locked = (!files.is_empty() && rng.random_range(0..2) == 0).then(|| {
				use std::os::unix::fs::PermissionsExt;
				let file = files[rng.random_range(0..files.len())].clone();
				fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
				file
			});

			let (map, _) = scan_local(&root, &baseline, IgnoreRules::default(), RuleFiles::Read);
			let (_dir, stage) = Stage::in_temp_dir();
			let (staged, _) = scan_local_staged(
				&root,
				&baseline,
				IgnoreRules::default(),
				RuleFiles::Read,
				stage,
				&AtomicBool::new(false),
			)
			.unwrap();

			#[cfg(unix)]
			if let Some(file) = locked {
				use std::os::unix::fs::PermissionsExt;
				fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
				unreadable += usize::from(!map.complete);
			}
			assert!(matches!(staged.nodes, Side::Staged(_)), "seed {seed}");
			assert_eq!(
				sorted_entries(&staged.nodes.whole()),
				sorted_entries(&map.nodes.whole()),
				"seed {seed}: nodes"
			);
			assert_eq!(staged.complete, map.complete, "seed {seed}: complete");
			let errors = |scan: &LocalScan| -> Vec<String> {
				scan.errors.iter().map(ToString::to_string).collect()
			};
			assert_eq!(errors(&staged), errors(&map), "seed {seed}: errors");
			assert_eq!(staged.invalid_names, map.invalid_names, "seed {seed}");
			assert_eq!(staged.aliased_dirs, map.aliased_dirs, "seed {seed}");
			assert_eq!(staged.ignored, map.ignored, "seed {seed}");
			assert_eq!(
				staged.ignored_default_untracked, map.ignored_default_untracked,
				"seed {seed}"
			);
			assert_eq!(staged.ignore_blocked, map.ignore_blocked, "seed {seed}");
			let held = Nodes::len(&staged.nodes.whole());
			assert!(
				held > 4,
				"seed {seed}: a tree of {held} node(s) proves little"
			);
			fs::remove_dir_all(&root).ok();
		}
		#[cfg(unix)]
		assert!(
			unreadable > 2,
			"only {unreadable} case(s) with an unreadable file"
		);
		#[cfg(not(unix))]
		let _ = unreadable;
	}

	/// A map sink that also keeps everything put into it, in the order it was put.
	struct Recording<'a> {
		map: MapSink,
		put: &'a mut Vec<Found>,
	}

	impl ScanSink for Recording<'_> {
		type Error = Infallible;

		fn put(&mut self, found: Found) -> Result<Option<Collision>, Infallible> {
			self.put.push(found.clone());
			self.map.put(found)
		}

		fn finish(self) -> Result<Side<LocalNode>, Infallible> {
			self.map.finish()
		}
	}

	/// Reading entries on worker threads — their stats, and the hashes the fast path missed —
	/// changes nothing a walk reports: what it puts and in which order, which entry of a twin wins,
	/// every error in its order, and whether it completed. The tree holds more entries than the walk
	/// holds while one is read, a name the remote rejects (a file, and a directory with entries
	/// under it), an ignored file, a file removed once listed, files and a `.filenignore` that cannot
	/// be read, and, where the volume keeps them apart, case twins. Every directory holds a symlink
	/// to a file, one to a directory inside the root and one to a directory outside it, and two
	/// dangling ones, so the links past [`INLINE_STATS`] that resolve are stat-ed on a worker. It
	/// walks with no baseline, and once with a baseline whose every hash only the fast path can
	/// produce.
	#[cfg(unix)]
	#[test]
	fn a_walk_reading_entries_on_workers_finds_what_one_reading_them_inline_finds() {
		use std::os::unix::fs::{PermissionsExt, symlink};

		let root = temp_root();
		let outside = temp_root();
		fs::write(outside.join("o1.txt"), b"outside 1").unwrap();
		fs::write(outside.join("o2.txt"), b"outside 2").unwrap();
		for dir in 0..5 {
			let dir_path = root.join(format!("d{dir}"));
			fs::create_dir(&dir_path).unwrap();
			for file in 0..1100 {
				fs::write(
					dir_path.join(format!("f{file:04}.txt")),
					format!("{dir}/{file}"),
				)
				.unwrap();
			}
			symlink(dir_path.join("f0000.txt"), dir_path.join("link.txt")).unwrap();
			symlink(root.join("d0").join("deep"), dir_path.join("alias")).unwrap();
			symlink(&outside, dir_path.join("ext")).unwrap();
			symlink(root.join("nowhere"), dir_path.join("dangling")).unwrap();
			symlink(root.join("nowhere_dir"), dir_path.join("dangling_dir")).unwrap();
		}
		fs::create_dir(root.join("d0").join("deep")).unwrap();
		fs::write(root.join("d0").join("deep").join("leaf.txt"), b"leaf").unwrap();
		fs::write(root.join("bad."), b"rejected name").unwrap();
		fs::create_dir_all(root.join("rej.").join("sub")).unwrap();
		fs::write(
			root.join("rej.").join("child.txt"),
			b"under a rejected name",
		)
		.unwrap();
		fs::write(root.join("rej.").join("sub").join("bad."), b"under it too").unwrap();
		fs::write(root.join(FILENIGNORE), "*.log\n").unwrap();
		fs::write(root.join("d1").join("x.log"), b"ignored").unwrap();
		// Alone in its directory, so putting it back between the walks leaves every other
		// directory's order as it was. Removing it changes the directory's mtime, which a walk
		// reading on workers may read after the removal: that one mtime is left out of the
		// comparison.
		let gone_dir = root.join("gone");
		fs::create_dir(&gone_dir).unwrap();
		let gone = gone_dir.join("temp.txt");
		let restore_gone = || fs::write(&gone, b"gone").unwrap();
		restore_gone();
		let locked = [
			root.join("d1").join("f0100.txt"),
			root.join("d3").join("f0500.txt"),
			root.join("d3").join("f0501.txt"),
		];
		let rule_file = root.join("d2").join(FILENIGNORE);
		fs::write(&rule_file, "*.tmp\n").unwrap();
		for path in locked.iter().chain([&rule_file]) {
			fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
		}
		let twins = case_twins(&root.join("d4")).is_some();

		let walk = |baseline: &Baseline, max_read_workers: usize| {
			let mut put = Vec::new();
			let Ok((scan, _)) = scan_local_watched(
				&root,
				"",
				baseline,
				IgnoreRules::default(),
				&RuleFiles::Read,
				Hooks {
					on_listed: &mut |path| {
						if path == gone {
							fs::remove_file(path).unwrap();
						}
						ControlFlow::Continue(())
					},
					stop: None,
					moved_hash: None,
					max_read_workers,
				},
				Recording {
					map: MapSink::with_capacity(1024),
					put: &mut put,
				},
			);
			restore_gone();
			(scan, put)
		};
		let errors = |scan: &LocalScan| -> Vec<String> {
			scan.errors.iter().map(ToString::to_string).collect()
		};
		let without_gone_mtime = |found: &Found| match found {
			Found::Node(node) if node.rel_path == "gone" => Found::Node(LocalNode {
				mtime_millis: 0,
				..node.clone()
			}),
			found => found.clone(),
		};
		let assert_same = |(pooled, pooled_put): &(LocalScan, Vec<Found>),
		                   (inline, inline_put): &(LocalScan, Vec<Found>)| {
			assert!(
				inline_put.len() > READ_WINDOW,
				"the tree must outgrow the window: {} put",
				inline_put.len()
			);
			assert_eq!(
				pooled_put
					.iter()
					.map(without_gone_mtime)
					.collect::<Vec<_>>(),
				inline_put
					.iter()
					.map(without_gone_mtime)
					.collect::<Vec<_>>(),
				"what is put, and in which order"
			);
			assert_eq!(errors(pooled), errors(inline));
			assert_eq!(pooled.complete, inline.complete);
			assert_eq!(pooled.invalid_names, inline.invalid_names);
			assert_eq!(pooled.aliased_dirs, inline.aliased_dirs);
			assert_eq!(pooled.ignored, inline.ignored);
			assert_eq!(pooled.ignore_blocked, inline.ignore_blocked);
			let nodes = |scan: &LocalScan| {
				let mut nodes = sorted_entries(&scan.nodes.whole());
				for (_, node) in nodes.iter_mut().filter(|(rel_path, _)| rel_path == "gone") {
					node.mtime_millis = 0;
				}
				nodes
			};
			assert_eq!(nodes(pooled), nodes(inline));
		};

		let inline = walk(&Baseline::default(), 0);
		let pooled = walk(&Baseline::default(), MAX_READ_WORKERS);
		assert_same(&pooled, &inline);

		// Every hash the baseline vouches for is one no file has, so only the fast path can have
		// produced it.
		let fake = Blake3Hash::from([0xAB; 32]);
		let row = |rel_path: String, kind, size, local_mtime| BaselineEntry {
			rel_path,
			kind,
			remote_uuid: None,
			content_hash: (kind == NodeKind::File).then_some(fake),
			size,
			local_mtime,
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		};
		let baseline =
			Baseline::from_rows(sorted_entries(&inline.0.nodes.whole()).into_iter().map(
				|(rel_path, node)| {
					row(
						rel_path,
						node.kind,
						Some(node.size),
						Some(node.mtime_millis),
					)
				},
			));
		let inline_fast = walk(&baseline, 0);
		let pooled_fast = walk(&baseline, MAX_READ_WORKERS);
		assert_same(&pooled_fast, &inline_fast);

		for path in locked.iter().chain([&rule_file]) {
			fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
		}
		let (inline, _) = inline;
		assert_eq!(
			inline
				.errors
				.iter()
				.filter(|error| matches!(error, ScanError::Io { .. }))
				.count(),
			locked.len() + 1 + 2 * 5,
			"each unreadable file, the unreadable rule file and the dangling links: {:?}",
			inline.errors
		);
		assert_eq!(
			inline
				.errors
				.iter()
				.any(|error| matches!(error, ScanError::DuplicateName { .. })),
			twins,
			"{:?}",
			inline.errors
		);
		assert!(!inline.complete);
		assert_eq!(
			inline.invalid_names.keys().collect::<Vec<_>>(),
			vec!["bad.", "rej."]
		);
		assert_eq!(
			inline.aliased_dirs.keys().cloned().collect::<Vec<_>>(),
			(0..5)
				.map(|dir| format!("d{dir}/alias"))
				.collect::<Vec<_>>()
		);
		assert_eq!(inline.ignored.keys().collect::<Vec<_>>(), vec!["d1/x.log"]);
		assert_eq!(inline.ignore_blocked.iter().collect::<Vec<_>>(), vec!["d2"]);
		let nodes = sorted_entries(&inline.nodes.whole());
		for linked in ["d3/link.txt", "d3/ext/o1.txt", "d3/ext/o2.txt"] {
			assert!(
				nodes.iter().any(|(rel_path, _)| rel_path == linked),
				"{linked} is walked through its link"
			);
		}
		let files = sorted_entries(&pooled_fast.0.nodes.whole())
			.into_iter()
			.filter(|(_, node)| node.kind == NodeKind::File)
			.collect::<Vec<_>>();
		assert!(files.len() > READ_WINDOW);
		assert!(
			files
				.iter()
				.all(|(_, node)| node.content_hash == Some(fake)),
			"every file takes the baseline's hash on the fast path"
		);

		fs::remove_dir_all(&root).ok();
		fs::remove_dir_all(&outside).ok();
	}

	/// A root holding `files` files and nothing else.
	fn flat_tree(files: usize) -> std::path::PathBuf {
		let root = temp_root();
		for file in 0..files {
			fs::write(root.join(format!("f{file:04}.txt")), file.to_string()).unwrap();
		}
		root
	}

	/// A map sink that counts what it takes, and sets `stop` as it takes its `at`th entry.
	struct StopAt<'a> {
		map: MapSink,
		put: usize,
		at: usize,
		stop: &'a AtomicBool,
	}

	impl ScanSink for StopAt<'_> {
		type Error = Infallible;

		fn put(&mut self, found: Found) -> Result<Option<Collision>, Infallible> {
			self.put += 1;
			if self.put == self.at {
				self.stop.store(true, Ordering::Relaxed);
			}
			self.map.put(found)
		}

		fn finish(self) -> Result<Side<LocalNode>, Infallible> {
			self.map.finish()
		}
	}

	/// A walk its caller stops before it lists anything stages nothing, and says it is partial. The
	/// tree is directories alone, which the walk reads on its own thread and puts as it lists them,
	/// so a walk that went on listing would stage them.
	#[test]
	fn a_staged_walk_stopped_before_it_starts_stages_nothing() {
		let root = temp_root();
		for dir in ["a", "b", "c"] {
			fs::create_dir(root.join(dir)).unwrap();
		}
		let (_dir, stage) = Stage::in_temp_dir();
		let (scan, _) = scan_local_staged(
			&root,
			&Baseline::default(),
			IgnoreRules::default(),
			RuleFiles::Read,
			stage,
			&AtomicBool::new(true),
		)
		.unwrap();
		assert!(!scan.complete);
		assert_eq!(Nodes::len(&scan.nodes.whole()), 0);
		fs::remove_dir_all(&root).ok();
	}

	/// A walk cut short while it lists waits for none of the reads it handed over. With one worker
	/// and no baseline every file is read there, and the walk puts nothing before it waits on a
	/// read, so a walk that waited would have put every file listed before the cut.
	#[test]
	fn a_walk_cut_short_while_listing_puts_none_of_the_reads_under_way() {
		let root = flat_tree(300);
		let mut listed = 0usize;
		let Ok((scan, _)) = scan_local_watched(
			&root,
			"",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			Hooks {
				on_listed: &mut |path| {
					listed += usize::from(path != root);
					if listed == 200 {
						ControlFlow::Break(())
					} else {
						ControlFlow::Continue(())
					}
				},
				stop: None,
				moved_hash: None,
				max_read_workers: 1,
			},
			Recording {
				map: MapSink::with_capacity(300),
				put: &mut Vec::new(),
			},
		);
		assert!(!scan.complete);
		assert_eq!(Nodes::len(&scan.nodes.whole()), 0);
		fs::remove_dir_all(&root).ok();
	}

	/// A walk stopped once it has listed everything waits for no more of its reads. One worker
	/// answers the reads in the order they were handed over, so each answer puts exactly one file,
	/// and the walk stops at the one put as the stop is set.
	#[test]
	fn a_walk_stopped_while_it_waits_on_its_reads_waits_for_no_more_of_them() {
		let root = flat_tree(300);
		let stop = AtomicBool::new(false);
		let Ok((scan, _)) = scan_local_watched(
			&root,
			"",
			&Baseline::default(),
			IgnoreRules::default(),
			&RuleFiles::Read,
			Hooks {
				on_listed: &mut |_| ControlFlow::Continue(()),
				stop: Some(&stop),
				moved_hash: None,
				max_read_workers: 1,
			},
			StopAt {
				map: MapSink::with_capacity(300),
				put: 0,
				at: 100,
				stop: &stop,
			},
		);
		assert!(!scan.complete);
		assert_eq!(Nodes::len(&scan.nodes.whole()), 100);
		fs::remove_dir_all(&root).ok();
	}

	/// A walk of `root` into a map, with `on_listed` called for each entry it lists.
	fn scan_watched(
		root: &Path,
		baseline: &Baseline,
		on_listed: &mut dyn FnMut(&Path) -> ControlFlow<()>,
	) -> LocalScan {
		let Ok((scan, _)) = scan_local_watched(
			root,
			"",
			baseline,
			IgnoreRules::default(),
			&RuleFiles::Read,
			Hooks {
				on_listed,
				stop: None,
				moved_hash: None,
				max_read_workers: MAX_READ_WORKERS,
			},
			MapSink::with_capacity(1024),
		);
		scan
	}

	/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
	fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
		Baseline::from_rows(rows.values().cloned())
	}
}
