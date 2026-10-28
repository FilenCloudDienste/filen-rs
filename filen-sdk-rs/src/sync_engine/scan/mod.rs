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
//! the engine never writes one); the walk's loop detection guards against cycles. A symlink that
//! cannot be followed (dangling, or part of a loop) is recorded as an error but does not make the
//! scan incomplete, unless the baseline tracks a directory at that path (see
//! [`LocalScan::complete`]). Only a directory the scan admits is checked for a loop, as only one it
//! admits is opened: a link back onto a directory inside the root is the alias below and never
//! checked, and a link the rules ignore is never opened, so a loop is an error only through a link
//! leading outside the root.
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
	path::{Component, Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, Ordering},
		mpsc,
	},
	time::{Duration, SystemTime},
};

use chrono::{DateTime, Utc};
use filen_types::crypto::Blake3Hash;
use unicode_normalization::{UnicodeNormalization, is_nfc};
use uuid::Uuid;

use super::{
	SyncMode,
	baseline::{BaselineEntry, NodeKind},
	ignore::{
		FILENIGNORE, IgnoreDecision, IgnoreParseError, IgnoreRules, IgnoreSource,
		MAX_RULE_FILE_BYTES, Origin, rule_file_dir, rule_file_text,
	},
	plan::RemoteView,
	rows::{Baseline, Cursor},
	side::{NodesAt, Side},
	stage::{Paging, Stage, StageSink},
	walk::{self, Walk, WalkError},
};
use crate::{
	fs::name::ValidatedName,
	io::{DOWNLOAD_TMP_EXT, FilenMetaExt},
};

/// The per-pair local quarantine directory (where remote-propagated deletions are moved instead of
/// being destroyed). Always excluded from the scan so it is never itself synced back up.
pub(crate) const QUARANTINE_DIR: &str = ".filen-sync-trash";

/// The folder filen-sync, the engine this one replaces, kept a pair's local deletions in: one at
/// the pair's local root, holding each deleted item by its base name. filen-sync never synced a
/// path with a component of that name, on either side, and neither does this engine (see
/// [`is_legacy_trash`]), so a pair moved over from it does not upload its old trash as content.
pub(crate) const LEGACY_TRASH_DIR: &str = ".filen.trash.local";

/// Whether an entry named `name` is filen-sync's trash folder ([`LEGACY_TRASH_DIR`]), at whatever
/// depth: never read by the local scan or the watcher, never placed in the remote view, so nothing
/// at or under it is uploaded, downloaded or deleted on its own account. A deleted ancestor still
/// takes it along, as it takes any content: a local deletion of `Photos` trashes the remote
/// `Photos` with its `.filen.trash.local`, and a remote one moves the local `Photos`, reserved
/// folder and all, into the bin. The match is exact and case-sensitive, as filen-sync's was:
/// `.Filen.Trash.Local` and `.filen.trash.local.txt` are the user's items.
///
/// The one place the name is matched; a path is reserved when any of its components is.
pub(crate) fn is_legacy_trash(name: &OsStr) -> bool {
	name == OsStr::new(LEGACY_TRASH_DIR)
}

/// Whether `name` is a case variant of filen-sync's trash folder's name (see [`is_legacy_trash`]),
/// such as `.Filen.Trash.Local`: the user's item, synced like any other, but one that a
/// case-insensitive filesystem or the server's case-insensitive name dedup can resolve onto a
/// reserved folder the other side never shows. The reserved folder itself is not one.
pub(crate) fn folds_onto_legacy_trash(name: &str) -> bool {
	name != LEGACY_TRASH_DIR && collision_key(name) == LEGACY_TRASH_DIR
}

/// The directory Filen's apps keep the account's own data in (chat uploads among it), at the top of
/// the drive. Only a pair whose remote root is the drive root has it in its tree (see
/// [`is_account_data`]).
pub(crate) const ACCOUNT_DATA_DIR: &str = ".filen";

/// Where a pair's remote root is, which decides whether its tree holds the account's own data
/// directory (see [`is_account_data`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PairRoot {
	/// The drive root, whose top level holds [`ACCOUNT_DATA_DIR`].
	Drive,
	/// A directory in the drive: everything under it is the user's.
	Directory,
}

/// Whether `rel_path` is the account's data directory ([`ACCOUNT_DATA_DIR`]) or under it, in a pair
/// rooted at `root`: in a pair rooted at the drive, a top-level entry of that name, matched as the
/// server matches names, case-insensitively. Never read by the local scan or the watcher, never
/// placed in the remote view, so nothing at or under it is uploaded, downloaded or deleted, and a
/// local write that resolves into it is refused. A pair rooted at a directory syncs a `.filen` in
/// it like anything else.
///
/// A case variant is reserved too, on both sides, where filen-sync's trash folder reserves only its
/// exact name ([`is_legacy_trash`]): the server dedups the drive root's names case-insensitively,
/// so `.Filen` there IS the account's directory however the apps spelled it, and a local `.Filen`
/// could only ever be merged into it.
pub(crate) fn is_account_data(rel_path: &str, root: PairRoot) -> bool {
	root == PairRoot::Drive && at_or_under_folded(rel_path, ACCOUNT_DATA_DIR)
}

/// The local root a walk reads, and where the pair's remote root is: together, what the walk keys
/// its entries against and which of them it never enters (see [`is_account_data`]).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanRoot<'a> {
	pub(crate) path: &'a Path,
	pub(crate) pair_root: PairRoot,
}

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
	/// How many nanoseconds the file system's mtime lies past [`mtime_millis`](Self::mtime_millis),
	/// the precision that reading of it drops: it rounds to the nearest millisecond on Unix and
	/// truncates on Windows, so this is negative where it rounded up. The seeding stage weighs a
	/// remote stamp written in whole seconds or by filen-sync by it (see `seed::evidence`). 0 for a
	/// directory, and on a node built from a row or read back from a staged plan, which record
	/// milliseconds only: the seeding weighs neither.
	pub(crate) mtime_nanos: i32,
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
	/// Download temp files the walk passed that no download can still be writing (see
	/// [`is_stale_download`]): what a download killed before its cleanup ran left behind. Like every
	/// `.filendl` file they are not in `nodes`; the pass removes them
	/// ([`remove_stale_downloads`]).
	pub(crate) stale_downloads: Vec<PathBuf>,
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

/// How long a download temp file goes unmodified before the scan takes it for one a killed
/// download left behind. A download writes into its temp file as each chunk arrives, so one still
/// in flight touches it far more often than this; a day leaves room for a transfer stalled on the
/// network or the memory budget.
const STALE_DOWNLOAD_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Whether the `.filendl` file at `path` is a download temp file nothing is writing any more: named
/// `<uuid>.filendl` as a download names it (a user's `notes.filendl` is left alone, though the scan
/// never syncs it), a plain file rather than a link, and unmodified for [`STALE_DOWNLOAD_AGE`] as
/// of `now`.
///
/// The one window this misreads: a download stamps its temp file with the remote's mtime just
/// before renaming it into place, so a file swept in between fails that download at its rename.
/// The download is retried by the next pass and its destination never holds a partial file.
pub(super) fn is_stale_download(path: &Path, now: SystemTime) -> bool {
	let named_by_a_download = path
		.file_stem()
		.and_then(OsStr::to_str)
		.is_some_and(|stem| Uuid::try_parse(stem).is_ok())
		&& path.extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT));
	named_by_a_download
		&& std::fs::symlink_metadata(path)
			.and_then(|meta| {
				if meta.is_file() {
					meta.modified().map(Some)
				} else {
					Ok(None)
				}
			})
			.ok()
			.flatten()
			.and_then(|modified| now.duration_since(modified).ok())
			.is_some_and(|age| age > STALE_DOWNLOAD_AGE)
}

/// Remove the download temp files a scan found stale ([`LocalScan::stale_downloads`]), each asked
/// again first, since the pass's read and this removal are apart. Returns how many went; a removal
/// that fails is logged with its cause and the file is looked at again by the next scan.
pub(super) fn remove_stale_downloads(paths: &[PathBuf], now: SystemTime) -> usize {
	let mut removed = 0;
	for path in paths {
		if !is_stale_download(path, now) {
			continue;
		}
		match std::fs::remove_file(path) {
			Ok(()) => removed += 1,
			Err(e) if e.kind() == ErrorKind::NotFound => {}
			Err(e) => {
				tracing::warn!("cannot remove the stale download temp file {path:?}: {e}");
			}
		}
	}
	removed
}

/// Log `error` and collect it.
pub(super) fn record(errors: &mut Vec<ScanError>, root: &Path, error: ScanError) {
	tracing::warn!("local scan of {}: {error}", root.display());
	errors.push(error);
}

/// Whether a walk error is a symlink that cannot be followed: one looping back onto an ancestor, or
/// one whose target does not resolve (dangling, or a link to itself). A symlink to a directory that
/// merely cannot be READ is not one of these — its target exists and was not read.
fn unfollowable_symlink(err: &WalkError) -> bool {
	if matches!(err, WalkError::Loop { .. }) {
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
	entry: &walk::Entry,
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
	/// What a mode that only pulls reads: the rules come from the remote copies, so only these files
	/// are read and the rest left alone.
	Only {
		/// The root-relative directories (`""` is the root) of the synced rule files the remote has
		/// lost: each is read in place of the remote copy, replacing any rules given for it.
		synced: BTreeSet<String>,
		/// Set while no baseline row records the pair root's file, so never together with `""` in
		/// `synced`: that file, where there is one, is read as local configuration (see
		/// [`IgnoreRules::insert_local_root`]) unless it is a copy of the remote root file.
		local_root: Option<UnrecordedRoot>,
	},
}

/// The pair root's `.filenignore` in a pass that only pulls, while no baseline row records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnrecordedRoot {
	/// The body of the remote root `.filenignore` the pass read, if it read one. A file on disk
	/// holding exactly these bytes is that file's copy — what a pair migrated from filen-sync, or
	/// removed and added again, holds — so it is synced as one and is no local configuration:
	/// otherwise it would hide the remote file for good, and a later remote edit of the rules would
	/// never reach this device.
	pub(crate) remote_copy: Option<Arc<str>>,
}

impl RuleFiles {
	/// What a pass in `mode` reads on disk, with `baseline` its rows and `remote` the view its
	/// remote rules were read from.
	///
	/// A mode that pushes reads every `.filenignore` on disk, over any remote copy read for the same
	/// directory. A mode that pulls alone reads on disk only the synced ones the remote has lost:
	/// they govern until that loss propagates, so a directory trashed on the remote keeps what they
	/// hide here. It also reads the pair root's file as local configuration while no row records it,
	/// so a rule a device keeps for itself applies without the remote holding it, unless the file is
	/// a copy of `remote_root`, the body of the remote root file this pass read.
	pub(super) fn for_pass(
		mode: SyncMode,
		baseline: &Baseline,
		remote: &RemoteView,
		remote_root: Option<&Arc<str>>,
	) -> Self {
		if mode.pushes() {
			return Self::Read;
		}
		Self::Only {
			synced: baseline
				.rule_file_rows()
				.filter(|rel_path| !remote.nodes.of(baseline).holds(rel_path))
				.filter_map(|rel_path| rule_file_dir(&rel_path).map(str::to_owned))
				.collect(),
			local_root: (!baseline.contains_key(FILENIGNORE)).then(|| UnrecordedRoot {
				remote_copy: remote_root.map(Arc::clone),
			}),
		}
	}

	pub(super) fn reads(&self, dir: &str) -> bool {
		match self {
			Self::Read => true,
			Self::Only { synced, local_root } => {
				synced.contains(dir) || (local_root.is_some() && dir.is_empty())
			}
		}
	}

	/// How `dir`'s file, reading `text`, joins the rules.
	fn reads_as(&self, dir: &str, text: &str) -> ReadAs {
		match self {
			Self::Only {
				local_root: Some(root),
				..
			} if dir.is_empty() => {
				if root.remote_copy.as_deref() == Some(text) {
					ReadAs::RemoteCopy
				} else {
					ReadAs::LocalRoot
				}
			}
			Self::Read | Self::Only { .. } => ReadAs::Own,
		}
	}
}

/// How [`load_rule_file`] reads a file [`RuleFiles::reads`].
enum ReadAs {
	/// As its directory's own rules, replacing any given for it.
	Own,
	/// As the local root file (see [`IgnoreRules::insert_local_root`]).
	LocalRoot,
	/// Not at all: it is a copy of the remote root file, whose rules already joined (and whose bad
	/// lines were already reported). It stays an ordinary item, which the pass records as synced.
	RemoteCopy,
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

/// Loads the `.filenignore` of the root-relative directory `dir` at `dir_path` into `rules`, as the
/// local root file where `rule_files` says so, and not at all where it is a copy of the remote root
/// file (see [`UnrecordedRoot`]). No such file is no rules. A file that cannot be read,
/// that the remote side would refuse (see [`rule_file_text`]) or that does not compile at all
/// blocks `dir`: guessing could push what the user meant to hide, or pull what they meant to keep
/// out. A bad line is only reported.
pub(super) fn load_rule_file(
	root: &Path,
	dir: &str,
	dir_path: &Path,
	rule_files: &RuleFiles,
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
	let as_local_root = match rule_files.reads_as(dir, &text) {
		ReadAs::Own => false,
		ReadAs::LocalRoot => true,
		ReadAs::RemoteCopy => return,
	};
	match IgnoreSource::parse(&text, Origin::File { dir }) {
		Ok((source, line_errors)) => {
			if as_local_root {
				rules.insert_local_root(source);
			} else {
				rules.insert_file(dir.to_owned(), source);
			}
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
	root: ScanRoot<'_>,
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
/// matched. The rules come back as the scan used them, for the rest of the pass to match with. The
/// engine's reserved entries are left out before any rule, unrecorded: the bin, filen-sync's
/// trash and, in a pair `root` says is rooted at the drive, the account's data directory (see
/// [`is_account_data`]).
///
/// Once `stop` is set the walk lists and hands over nothing more, and comes back incomplete once each
/// worker has finished the read it is in, to its end (a hash read cannot be interrupted). That is
/// how a pass gives up a walk it started before it knew the rules to walk with.
///
/// `Err` when the stage cannot be written — a full disk — which fails the pass before it has
/// planned anything.
pub(crate) fn scan_local_staged(
	root: ScanRoot<'_>,
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
		mtime_nanos: i32,
		/// The baseline's hash when the fast path took it (or `moved_hash` answered), read otherwise.
		hash: std::io::Result<Blake3Hash>,
	},
	/// A socket, FIFO or device: not syncable, and skipped silently. (A broken symlink never gets
	/// here: following it fails, and the walk hands it over as an error.)
	Other,
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
			mtime_nanos: mtime_nanos(metadata, mtime_millis),
			hash: hash(size, mtime_millis),
		}
	} else {
		Reading::Other
	}
}

/// How many nanoseconds `metadata`'s mtime lies past `mtime_millis`, the milliseconds
/// [`FilenMetaExt::modified`] reads it as (see [`LocalNode::mtime_nanos`]); 0 where the file system
/// gives no mtime, or where the two readings are further apart than an `i32` of nanoseconds (an
/// mtime past the range of either).
pub(crate) fn mtime_nanos(metadata: &std::fs::Metadata, mtime_millis: i64) -> i32 {
	let (Ok(modified), Some(read)) = (
		metadata.modified(),
		DateTime::from_timestamp_millis(mtime_millis),
	) else {
		return 0;
	};
	DateTime::<Utc>::from(modified)
		.signed_duration_since(read)
		.num_nanoseconds()
		.and_then(|nanos| i32::try_from(nanos).ok())
		.unwrap_or(0)
}

/// `read`, or, when it is a file whose hash read failed and `stat` now finds something other than a
/// file, a reading of that instead, so [`Window::settle`] holds the new type against the listed one
/// and takes the entry as caught mid-change.
///
/// The stat a reading is made from is taken before the hash read, so a file replaced by a directory
/// in between still reads as a file, and only opening or reading it finds the directory (on
/// Windows, failing as access denied). A failed read of what is still a file keeps its error.
fn restat_failed_read(
	read: Reading,
	stat: impl FnOnce() -> std::io::Result<std::fs::Metadata>,
) -> Reading {
	match read {
		Reading::File {
			size,
			mtime_millis,
			mtime_nanos,
			hash: Err(source),
		} => match stat() {
			Ok(now) if !now.is_file() => reading(&now, |_, _| Err(source)),
			_ => Reading::File {
				size,
				mtime_millis,
				mtime_nanos,
				hash: Err(source),
			},
		},
		read => read,
	}
}

/// Stat `entry` and, for a file, take the hash `row` holds when the fast path allows it and read the
/// file otherwise.
fn read_entry(entry: &walk::Entry, row: Option<&BaselineEntry>) -> std::io::Result<Reading> {
	let read = reading(&entry.metadata()?, |size, mtime_millis| {
		fast_path_hash(row, size, mtime_millis).map_or_else(|| hash_file(entry.path()), Ok)
	});
	Ok(restat_failed_read(read, || entry.metadata()))
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
		let (kind, size, mtime_millis, mtime_nanos, hash) = match read {
			Ok(Reading::Dir { mtime_millis }) => (NodeKind::Dir, 0, mtime_millis, 0, None),
			Ok(Reading::File {
				size,
				mtime_millis,
				mtime_nanos,
				hash,
			}) => (NodeKind::File, size, mtime_millis, mtime_nanos, Some(hash)),
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
			mtime_nanos,
			content_hash,
		}))
	}

	fn vanished(&self, rel_path: &str) {
		tracing::debug!(
			"local scan of {}: {rel_path:?} vanished after the walk listed it",
			self.root.display()
		);
	}

	/// Record what the walk could not read, with `rows` asked about the baseline row at its path.
	fn walk_error(&mut self, err: WalkError, rows: &mut Cursor<'_>) -> Result<(), S::Error> {
		let root = self.root;
		let rel_path = err
			.path()
			.and_then(|p| p.strip_prefix(root).ok())
			.and_then(normalize_rel_path)
			.unwrap_or_default();
		// A directory the walk listed but could not descend into does NOT get the rule the stat and
		// the hash read use for an entry that vanished (see `settle`): its children were never
		// listed, so their absence is not something the walk observed. Trusting it reads a
		// directory moved or removed mid-walk as one deletion per file under it, and the
		// destination — listed before the move — is not in this scan either, so the pass cannot
		// fold the move: it would trash the remote copies and upload them again next pass. Holding
		// those deletions for one pass costs nothing instead.
		//
		// An unreadable directory or entry means the tree is partial. A symlink that leads nowhere
		// does not: there is nothing behind it to have missed — unless a DIRECTORY was synced
		// through it (a link to an unmounted drive), whose subtree would otherwise read as deleted.
		// A file synced as a link's copy is only that copy, and its deletion propagates like any
		// other.
		let unfollowable = unfollowable_symlink(&err);
		if !unfollowable
			|| rows
				.get(&rel_path)
				.is_some_and(|entry| entry.kind == NodeKind::Dir)
		{
			self.complete = false;
		}
		let below_root = err.path().is_some_and(|path| path != root);
		let source = match err {
			WalkError::Loop { ancestor, .. } => {
				std::io::Error::other(format!("symlink loops back onto {}", ancestor.display()))
			}
			WalkError::Io { source, .. } => source,
		};
		// An entry gone or replaced since its directory was listed, or a directory gone or replaced
		// by a file before its listing was read, changed under the walk: partial as above, but
		// nothing is wrong with the tree, so nothing is reported. A missing pair root is reported,
		// and so is an error that names no path, such as an ancestor's handle the loop check could
		// not open: nothing places it below the root.
		if below_root && !unfollowable && changed_under_walk(&source) {
			self.caught_mid_change(&rel_path, source);
			return Ok(());
		}
		self.push(Listed::Error(ScanError::Io { rel_path, source }))?;
		Ok(())
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
type ReadJob = (usize, walk::Entry, Option<BaselineEntry>);

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
	ScanRoot {
		path: root,
		pair_root,
	}: ScanRoot<'_>,
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
	let mut stale_downloads = Vec::new();
	// One instant for the walk, so each temp file's age is measured against the same now.
	let now = SystemTime::now();
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
			rule_files,
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

		// An entry is keyed once for the filter and the read alike. A refused directory is skipped,
		// so nothing under it is listed.
		let mut walker = Walk::new(&start_path);
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
					window.walk_error(err, &mut rows)?;
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
					// Nor into filen-sync's trash, at any depth (its contents are the old engine's
					// deletions, never content to sync).
					if is_legacy_trash(entry.file_name()) {
						break 'filter false;
					}
					// Nor into the account's own data, in a pair rooted at the drive.
					if depth == 1
						&& rel_path
							.as_deref()
							.is_some_and(|rel_path| is_account_data(rel_path, pair_root))
					{
						break 'filter false;
					}
					// A `<uuid>.filendl` FILE is the temp file a download in flight is writing into this
					// very tree (see `Client::download_file_to_path`): partial bytes that must never be
					// read as a local item to upload. A directory with that suffix is a legitimate user
					// item.
					if !is_dir && entry.path().extension() == Some(OsStr::new(DOWNLOAD_TMP_EXT)) {
						// One a killed download left behind is collected for the pass to remove: no
						// cleanup of its own will ever run, and nothing else would see it.
						if !entry.path_is_symlink() && is_stale_download(entry.path(), now) {
							stale_downloads.push(entry.path().to_path_buf());
						}
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
					true
				};
			if !admitted {
				if is_dir {
					walker.skip_current_dir();
				}
				continue;
			}
			// Entered before it is recorded or its rule file read, so a directory that loops back
			// onto an ancestor is never recorded: in place of its entry is the loop's error, as if
			// the walk had found the loop before handing the directory over. Recorded, it would be
			// an empty directory with nothing under it. Only an admitted directory is entered: one
			// the filter refused is never opened.
			if is_dir && let Err(err) = walker.enter() {
				window.walk_error(err, &mut rows)?;
				continue;
			}
			if is_dir
				&& let Some(rel_path) = rel_path.as_deref()
				&& rule_files.reads(rel_path)
			{
				load_rule_file(
					root,
					rel_path,
					entry.path(),
					rule_files,
					&mut rules,
					&mut ignore_blocked,
					&mut rule_errors,
				);
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
				match entry.metadata() {
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
							let read = reading(&metadata, |_, _| {
								known.map_or_else(|| hash_file(entry.path()), Ok)
							});
							Next::Read(Ok(restat_failed_read(read, || entry.metadata())))
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
		stale_downloads,
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
	root: ScanRoot<'_>,
	start: &str,
	baseline: &Baseline,
	rules: IgnoreRules,
	rule_files: &RuleFiles,
	moved_hash: &mut dyn FnMut(&str, u64, i64) -> Option<Blake3Hash>,
	stop: &dyn Fn(usize) -> bool,
) -> Option<(LocalScan, IgnoreRules)> {
	let start_path = root.path.join(start);
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
mod tests;
