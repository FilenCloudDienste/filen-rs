//! The local-side scan: walk a pair's local root into a `rel_path -> LocalNode` map, applying the
//! mtime+size fast-path so an unchanged file is never re-hashed. An edit that kept BOTH its size
//! and its mtime is therefore invisible here; what protects it from being overwritten is the
//! pre-download stash hashing its target (`apply::local_holds_unsynced_content`), not the scan.
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
	cell::RefCell,
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	ffi::OsStr,
	fmt,
	io::{ErrorKind, Read},
	path::{Component, Path},
};

use filen_types::crypto::Blake3Hash;
use unicode_normalization::UnicodeNormalization;

use super::{
	baseline::{BaselineEntry, NodeKind},
	ignore::{
		FILENIGNORE, IgnoreDecision, IgnoreParseError, IgnoreRules, IgnoreSource,
		MAX_RULE_FILE_BYTES, Origin, rule_file_text,
	},
	tree::Baseline,
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
	/// (the fast-path), otherwise freshly computed.
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
	/// An entry could not be read (permission, a directory that vanished before the walk could
	/// descend into it, hash failure, a symlink that is dangling or part of a loop, an unreadable
	/// `.filenignore`), or the root itself was gone when the walk finished. A FILE the walk listed
	/// and that is gone by the time the scan stats or hashes it is NOT one of these: the walk
	/// observed it, so it is skipped silently.
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

/// The result of scanning a local root.
#[derive(Debug)]
pub(crate) struct LocalScan {
	pub(crate) nodes: HashMap<String, LocalNode>,
	/// `false` if the node set may be INCOMPLETE — a missing/unreadable root, an unreadable
	/// subtree, a non-UTF-8 name, or a normalized-name collision. The mass-delete guard refuses to
	/// propagate deletions from an incomplete scan (an empty/half-read source must not nuke the
	/// destination).
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
	while let Some(cut) = path.rfind('/') {
		path = &path[..cut];
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
	let mut parts = Vec::new();
	for component in rel.components() {
		match component {
			Component::Normal(os) => parts.push(os.to_str()?.nfc().collect::<String>()),
			_ => return None,
		}
	}
	Some(parts.join("/"))
}

/// The case-insensitive collision key for a (already NFC-normalized) relative path. Filen treats
/// names case-insensitively, so two paths differing only in case collide. Shared with the remote
/// view, which has to fold case the same way for its own duplicate check to mean the same thing.
pub(super) fn collision_key(rel_path: &str) -> String {
	rel_path.chars().flat_map(char::to_lowercase).collect()
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
		// A directory that cannot be listed is the walk's error to report, and it leaves the scan
		// incomplete on its own.
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

/// Walk `root` into a `rel_path -> LocalNode` map. `baseline` (keyed by rel_path) drives the
/// fast-path and is read for the symlink rule on [`LocalScan::complete`]. Blocking work — the
/// engine calls this on a blocking thread.
///
/// Entries `rules` ignore are pruned: recorded in [`LocalScan::ignored`] and never descended into.
/// Each directory's `.filenignore` that `rule_files` reads joins `rules` before its children are
/// matched. The rules come back as the scan used them, for the rest of the pass to match with.
pub(crate) fn scan_local(
	root: &Path,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: RuleFiles,
) -> (LocalScan, IgnoreRules) {
	scan_local_watched(root, "", baseline, rules, &rule_files, &mut |_| {})
}

/// [`scan_local`], with a hook called for every entry the walker lists, just before the scan reads
/// it. It exists for the tests: an entry that is there when the walk lists it and gone when the
/// scan reads it is a race no test can produce from the outside, and it is the one this scan has
/// to survive without calling the tree partial.
fn scan_local_watched(
	root: &Path,
	start: &str,
	baseline: &Baseline,
	mut rules: IgnoreRules,
	rule_files: &RuleFiles,
	on_listed: &mut dyn FnMut(&Path),
) -> (LocalScan, IgnoreRules) {
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
	// The tree is what the baseline tracks plus whatever changed since, so the baseline is the one
	// estimate worth having; a pair with none still gets a walk's worth of room up front.
	let capacity = baseline.len().max(1024);
	// The walk asks about one directory's entries at a time, so the lookup keeps that directory in
	// hand instead of resolving every path from the root again.
	let mut rows = baseline.cursor();
	let mut nodes: HashMap<String, LocalNode> = HashMap::with_capacity(capacity);
	let mut errors = Vec::new();
	let mut invalid_names = BTreeMap::new();
	let mut complete = true;
	// A digest of every collision key taken so far, to spot a second entry folding the same way.
	// The keys themselves are not kept: a hit is rare and is resolved against the paths already
	// taken, so this costs 16 bytes a node instead of a second copy of every path.
	let mut claimed: HashSet<u128> = HashSet::with_capacity(capacity);
	// Paths that claimed a folded name and then bailed before becoming a node: a file whose hash
	// could not be read, and a rule file the scan could not use. The claim is what says the name is
	// taken, so the lookup below has to find them too — a walk with nothing wrong pushes none. An
	// entry skipped because it VANISHED is deliberately not here: it is gone, so its name is free.
	let mut bailed: Vec<String> = Vec::new();

	let mut aliased_dirs = BTreeMap::new();
	let mut ignored = BTreeMap::new();
	let mut ignored_default_untracked = 0usize;
	// Shared: the filter fills it, and the walk loop reads it for a rule file it cannot hash.
	let ignore_blocked = RefCell::new(BTreeSet::new());
	// Apart from `errors`, which the walk loop writes while the filter is alive.
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
			&mut ignore_blocked.borrow_mut(),
			&mut rule_errors,
		);
	}

	let walker = walkdir::WalkDir::new(&start_path)
		.follow_links(true)
		.into_iter()
		.filter_entry(|e| {
			let depth = start_depth + e.depth();
			if depth == 0 {
				return true;
			}
			// The engine's own files come before every rule, so no pattern can re-include one.
			// Never descend into our own quarantine dir (it holds locally-deleted items).
			if depth == 1 && e.file_name() == OsStr::new(QUARANTINE_DIR) {
				return false;
			}
			// A `<uuid>.filendl` FILE is the temp file a download in flight is writing into this
			// very tree (see `Client::download_file_to_path`): partial bytes that must never be read
			// as a local item to upload. A directory with that suffix is a legitimate user item.
			// `file_type` is the followed type, as the walk follows links.
			let is_dir = e.file_type().is_dir();
			if !is_dir && e.path().extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT)) {
				return false;
			}
			// A non-UTF-8 path is reported by the walk loop.
			let Some(rel_path) = e
				.path()
				.strip_prefix(root)
				.ok()
				.and_then(normalize_rel_path)
			else {
				return true;
			};
			// Its ancestors were all accepted, so the leaf's own decision is git's answer.
			if let Some(hit) = rules.decide(&rel_path, is_dir) {
				// Hidden either way. But a BUILT-IN default hit with nothing synced at or under it is
				// not a root: it is the `.DS_Store` every folder has, and carrying one per folder
				// through the pass costs every later filter its whole directory count. A user-level
				// or `.filenignore` line makes a root whether or not anything was synced there — the
				// user named that path, and the report says so.
				if hit.origin == Origin::Default && !baseline.tracked(&rel_path, is_dir) {
					ignored_default_untracked += 1;
				} else {
					ignored.insert(rel_path, hit.into());
				}
				return false;
			}
			if let Some(target) = alias_target(e, canonical_root.as_deref(), depth) {
				// Only targets INSIDE the root have a real path that wins. Two links to one directory
				// outside the root are both still walked, and upload it twice.
				aliased_dirs.insert(rel_path, target);
				return false;
			}
			if is_dir && rule_files.reads(&rel_path) {
				load_rule_file(
					root,
					&rel_path,
					e.path(),
					&mut rules,
					&mut ignore_blocked.borrow_mut(),
					&mut rule_errors,
				);
			}
			true
		});

	for entry in walker {
		let entry = match entry {
			Ok(entry) => entry,
			Err(err) => {
				let rel_path = err
					.path()
					.and_then(|p| p.strip_prefix(root).ok())
					.and_then(normalize_rel_path)
					.unwrap_or_default();
				// A directory the walk listed but could not descend into does NOT get the rule the
				// `metadata` and hash calls below use for an entry that vanished: its children were
				// never listed, so their absence is not something the walk observed. Trusting it
				// reads a directory moved or removed mid-walk as one deletion per file under it,
				// and the destination — listed before the move — is not in this scan either, so the
				// pass cannot fold the move: it would trash the remote copies and upload them again
				// next pass. Holding those deletions for one pass costs nothing instead.
				//
				// An unreadable directory or entry means the tree is partial. A symlink that leads
				// nowhere does not: there is nothing behind it to have missed — unless a DIRECTORY
				// was synced through it (a link to an unmounted drive), whose subtree would
				// otherwise read as deleted. A file synced as a link's copy is only that copy, and
				// its deletion propagates like any other.
				if !unfollowable_symlink(&err)
					|| rows
						.get(&rel_path)
						.is_some_and(|entry| entry.kind == NodeKind::Dir)
				{
					complete = false;
				}
				let source = match err.loop_ancestor() {
					Some(ancestor) => std::io::Error::other(format!(
						"symlink loops back onto {}",
						ancestor.display()
					)),
					None => err
						.into_io_error()
						.unwrap_or_else(|| std::io::Error::other("directory walk error")),
				};
				record(&mut errors, root, ScanError::Io { rel_path, source });
				continue;
			}
		};

		on_listed(entry.path());

		// The root itself is the pair's anchor, not a synced item. A subtree walk's start IS one.
		if start_depth + entry.depth() == 0 {
			continue;
		}

		let Ok(rel) = entry.path().strip_prefix(root) else {
			continue;
		};
		let Some(rel_path) = normalize_rel_path(rel) else {
			complete = false;
			record(
				&mut errors,
				root,
				ScanError::NonUtf8Name {
					lossy_path: rel.to_string_lossy().into_owned(),
				},
			);
			continue;
		};

		// `metadata()` follows symlinks (the walker has follow_links set), so a symlinked dir/file
		// is classified by its target.
		let metadata = match entry.metadata() {
			Ok(metadata) => metadata,
			Err(err) => {
				let source = err
					.into_io_error()
					.unwrap_or_else(|| std::io::Error::other("metadata error"));
				// Gone since the walk listed it: the walk observed this entry and something removed
				// it meanwhile, so nothing was missed and the scan still completed. Its absence is
				// real evidence, which is the whole point — a temp file that came and went during
				// the walk must not hold every deletion of the pair. Any other error (a permission
				// denial above all) IS missing evidence and still makes the scan partial.
				if source.kind() == ErrorKind::NotFound {
					tracing::debug!(
						"local scan of {}: {rel_path:?} vanished after the walk listed it",
						root.display()
					);
					continue;
				}
				complete = false;
				record(&mut errors, root, ScanError::Io { rel_path, source });
				continue;
			}
		};

		let kind = if metadata.is_dir() {
			NodeKind::Dir
		} else if metadata.is_file() {
			NodeKind::File
		} else {
			// Sockets, FIFOs, devices: not syncable, skip silently. (A broken symlink never gets
			// here: following it fails, and the walk hands it over as an error above.)
			continue;
		};

		// A name the remote would reject can never be pushed: report it once, at the top of the
		// subtree, and let the engine screen the whole subtree out of the plan rather than failing
		// an upload every pass. The node itself is still recorded — the scan reports what is on
		// disk, and an entry that exists but is omitted here reads downstream as a local DELETION,
		// so renaming a synced item into a rejected name would trash its remote copy. The scan also
		// still COMPLETED: this is a fully observed item that cannot be synced, not evidence the
		// walk missed anything, so `complete` stays true and the delete guard is unaffected.
		if !under_invalid_name(&invalid_names, &rel_path)
			&& let Some(reason) = name_rejection(&rel_path)
		{
			invalid_names.insert(rel_path.clone(), reason);
		}

		let key = collision_key(&rel_path);
		// Naming the twin means folding the paths already taken, which is why it happens on the
		// error path only. Finding none means the digests collided rather than the names, and a
		// pair is never refused over that.
		if !claimed.insert(collision_hash(&key))
			&& let Some(previous) = nodes
				.keys()
				.chain(bailed.iter())
				.find(|taken| collision_key(taken.as_str()) == key)
				.cloned()
		{
			complete = false;
			record(
				&mut errors,
				root,
				ScanError::DuplicateName {
					rel_path: format!("{previous} / {rel_path}"),
				},
			);
			continue;
		}

		let node = match kind {
			NodeKind::Dir => LocalNode {
				rel_path: rel_path.clone(),
				kind,
				size: 0,
				mtime_millis: FilenMetaExt::modified(&metadata).timestamp_millis(),
				content_hash: None,
			},
			NodeKind::File => {
				let size = metadata.len();
				let mtime = FilenMetaExt::modified(&metadata).timestamp_millis();
				let reused = fast_path_hash(rows.get(&rel_path).as_ref(), size, mtime);
				let content_hash = match reused {
					Some(hash) => Some(hash),
					None => match hash_file(entry.path()) {
						Ok(hash) => Some(hash),
						// A rule file the scan could not read has blocked its directory and been
						// reported already. Nothing is planned under a blocked directory, so the
						// missing node cannot read as a deletion.
						Err(_)
							if rule_files
								.reads(rel_path.rsplit_once('/').map_or("", |(dir, _)| dir))
								&& entry.file_name() == OsStr::new(FILENIGNORE)
								&& ignore_blocked.borrow().contains(
									rel_path.rsplit_once('/').map_or("", |(dir, _)| dir),
								) =>
						{
							bailed.push(rel_path);
							continue;
						}
						// Removed between the walk listing it and the hash reading it: the same
						// race as the `metadata` call above, and the same answer.
						Err(source) if source.kind() == ErrorKind::NotFound => {
							tracing::debug!(
								"local scan of {}: {rel_path:?} vanished after the walk listed it",
								root.display()
							);
							continue;
						}
						Err(source) => {
							complete = false;
							// It claimed the folded name above, so that name is still taken.
							bailed.push(rel_path.clone());
							record(&mut errors, root, ScanError::Io { rel_path, source });
							continue;
						}
					},
				};
				LocalNode {
					rel_path: rel_path.clone(),
					kind,
					size,
					mtime_millis: mtime,
					content_hash,
				}
			}
		};
		nodes.insert(rel_path, node);
	}

	// A directory that went away DURING the walk answers `NotFound` for every entry it had already
	// listed, and each of those was skipped just above as an entry that vanished — which on its own
	// is indistinguishable from the tree being emptied one file at a time. One stat tells them
	// apart: if what the walk started from is gone (a removed volume, a share that dropped, a lazily
	// unmounted tree), nothing the walk listed is evidence that anything was deleted.
	if let Err(source) = std::fs::metadata(&start_path) {
		complete = false;
		record(
			&mut errors,
			root,
			ScanError::Io {
				rel_path: start.to_owned(),
				source,
			},
		);
	}

	errors.extend(rule_errors);
	let scan = LocalScan {
		nodes,
		complete,
		errors,
		invalid_names,
		aliased_dirs,
		ignored,
		ignored_default_untracked,
		ignore_blocked: ignore_blocked.into_inner(),
	};
	(scan, rules)
}

/// [`scan_local`] started at the root-relative directory `start` instead of the pair root: the same
/// walk over one subtree, producing the same ROOT-relative keys — the directory-level
/// re-observation of a change-scoped pass (`observe::observe_local`).
///
/// `rules` must already carry the `.filenignore` files of `start`'s ancestors, and `start` must not
/// itself be hidden by them: what a whole-tree walk would have had in force when it reached `start`
/// is what decides the contents. `observe::observe_local` stats and loads the ancestors for exactly
/// that reason.
///
/// `start` is a node of the scan like any other. Only the pair root is skipped, and only because it
/// is the pair's anchor rather than a synced item.
pub(crate) fn scan_subtree(
	root: &Path,
	start: &str,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: &RuleFiles,
) -> (LocalScan, IgnoreRules) {
	scan_local_watched(root, start, baseline, rules, rule_files, &mut |_| {})
}

#[cfg(test)]
mod tests {
	use std::fs;

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

	fn sorted_paths(scan: &LocalScan) -> Vec<&str> {
		let mut paths: Vec<&str> = scan.nodes.keys().map(String::as_str).collect();
		paths.sort_unstable();
		paths
	}

	#[test]
	fn collision_key_folds_case() {
		assert_eq!(collision_key("A/B.txt"), "a/b.txt");
		assert_eq!(collision_key("already/low.txt"), "already/low.txt");
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

		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
		paths.sort();
		assert_eq!(paths, vec!["a.txt", "sub", "sub/b.txt"]);

		let a = &scan.nodes["a.txt"];
		assert_eq!(a.kind, NodeKind::File);
		assert_eq!(a.size, 5);
		assert!(
			a.content_hash.is_some(),
			"files are hashed without a baseline"
		);
		assert_eq!(scan.nodes["sub"].kind, NodeKind::Dir);
		assert!(
			scan.nodes["sub"].content_hash.is_none(),
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
			scan.nodes["a.txt"].content_hash,
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
			rescan.nodes["a.txt"].content_hash,
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
		assert!(scan.nodes.is_empty());
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
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
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
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
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
			scan.nodes["CON"].content_hash.is_some(),
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
		assert_eq!(scan.nodes.len(), 5);
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
		let paths: Vec<_> = scan.nodes.keys().cloned().collect();
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
			!untracked.nodes.contains_key("a/.DS_Store")
				&& !untracked.nodes.contains_key("a/.Trashes"),
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
		let (scan, _) = scan_local_watched(
			&root,
			"",
			&tree(&baseline),
			IgnoreRules::default(),
			&RuleFiles::Read,
			// Removed the moment the walker hands the entry over, before the scan stats it.
			&mut |path| {
				if path.ends_with("temp.txt") {
					fs::remove_file(path).unwrap();
				}
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

	/// A DIRECTORY the walk listed but could not descend into is the opposite case: its children
	/// were never listed, so their absence is not something the walk observed. Trusting it reads
	/// every file under a directory that was moved or removed mid-walk as a deletion — and the
	/// destination, listed before the move, is not in the scan either, so the pass cannot see the
	/// move. One held pass costs nothing; the next one folds it.
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
		let (scan, _) = scan_local_watched(
			&root,
			"",
			&tree(&baseline),
			IgnoreRules::default(),
			&RuleFiles::Read,
			// The first time a child of `outer` is listed, every child of `outer` goes — while the
			// walk still holds that directory's open stream and has yet to descend into any of them.
			&mut |path| {
				if !swept && path.parent() == Some(outer.as_path()) {
					swept = true;
					for child in fs::read_dir(&outer).unwrap() {
						fs::remove_dir_all(child.unwrap().path()).unwrap();
					}
				}
			},
		);

		assert!(
			!scan.complete,
			"the children of a directory the walk never descended into were not observed absent"
		);
		assert!(
			scan.errors
				.iter()
				.any(|error| matches!(error, ScanError::Io { .. })),
			"and the pass is told why: {:?}",
			scan.errors
		);

		fs::remove_dir_all(&root).ok();
	}

	/// A twin whose first spelling claimed the folded name and then bailed — a file the scan could
	/// not read — must still refuse the pass. The claim is what says the name is taken; forgetting
	/// it lets the second spelling through as an ordinary new file, and since the server folds case
	/// too, uploading it lands on top of the remote copy the first spelling is synced as.
	#[cfg(unix)]
	#[test]
	fn a_twin_whose_first_spelling_could_not_be_read_is_still_a_collision() {
		use std::os::unix::fs::PermissionsExt;

		let (upper, lower) = ("A.txt", "a.txt");
		assert_eq!(
			collision_key(upper),
			collision_key(lower),
			"the fixture needs two names that fold together"
		);
		let root = temp_root();
		let (upper, lower) = (root.join(upper), root.join(lower));
		fs::write(&upper, b"x").unwrap();
		fs::write(&lower, b"y").unwrap();
		// A filesystem that folds case holds one file, not two, and then there is no collision to
		// put in front of the scan — no pair of names it keeps apart folds together either, since
		// it folds the same way the key does. The case this guards is reachable wherever the
		// filesystem is case-sensitive, which is where the twin can exist at all.
		if fs::read_dir(&root).unwrap().count() < 2 {
			fs::remove_dir_all(&root).ok();
			return;
		}

		let baseline = HashMap::new();
		let mut blocked = false;
		let (scan, _) = scan_local_watched(
			&root,
			"",
			&tree(&baseline),
			IgnoreRules::default(),
			&RuleFiles::Read,
			// Whichever of the two the walk lists FIRST is made unreadable before the scan hashes
			// it, so it claims the folded name and then bails. Which one that is depends on the
			// directory's order, and the answer must not.
			&mut |path| {
				if !blocked && (path == upper || path == lower) {
					blocked = true;
					fs::set_permissions(path, fs::Permissions::from_mode(0o000)).unwrap();
				}
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
		let (scan, _) = scan_local_watched(
			&root,
			"",
			&tree(&baseline),
			IgnoreRules::default(),
			&RuleFiles::Read,
			&mut |path| {
				if !gone && path.parent() == Some(root.as_path()) {
					gone = true;
					fs::remove_dir_all(&root).unwrap();
				}
			},
		);

		assert!(
			!scan.complete,
			"the root itself is gone: nothing the walk listed is evidence of a deletion"
		);

		fs::remove_dir_all(&root).ok();
	}

	#[test]
	fn a_name_collision_is_not_reported_as_an_error_line() {
		let scan = LocalScan {
			nodes: HashMap::new(),
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
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
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
		let mut paths: Vec<_> = scan.nodes.keys().cloned().collect();
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
		assert!(scan.nodes.is_empty(), "{:?}", scan.nodes.keys());

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
		assert!(skipped.nodes.contains_key("build/deep/a.txt"));
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
		assert!(only.nodes.contains_key("build/deep/a.txt"));
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
			scan.nodes.contains_key("locked/a.txt"),
			"{:?}",
			scan.nodes.keys()
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
		);

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

	/// The rows a test spells as a path-keyed map, as the pass's resident baseline.
	fn tree(rows: &HashMap<String, BaselineEntry>) -> Baseline {
		Baseline::from_rows(rows.values().cloned())
	}
}
