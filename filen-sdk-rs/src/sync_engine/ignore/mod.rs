//! `.filenignore` rules: gitignore(5) syntax and precedence, evaluated against root-relative paths.
//!
//! Sources, highest precedence first: the `.filenignore` in the item's own directory, the
//! shallower `.filenignore` files up to the pair root (where a pair that only pulls reads its local
//! root file before the remote one, see below), the device-wide user level, then
//! [`DEFAULT_IGNORE_PATTERNS`]. Within one source the last matching line decides. The engine's
//! own internals (the quarantine directory, partial downloads, the watch staging filter),
//! filen-sync's trash folder (`LEGACY_TRASH_DIR`, at any depth) and, in a pair rooted at the drive,
//! the account's data directory (`ACCOUNT_DATA_DIR`) are not patterns and stay outside every
//! level.
//!
//! A pair that only pulls takes its `.filenignore` files from the remote, and can also hold a
//! LOCAL root file (see [`IgnoreRules::insert_local_root`]): the pair root's `.filenignore` on disk
//! when no baseline row says it was synced and its bytes differ from the remote root file's (an
//! identical one is that file's copy, synced as such). It is read at the root level, before the remote root
//! file — as if its lines followed that file's — so it sits below every nested file and above the
//! remote root file, the user level and the defaults.
//!
//! Every line is NFC-normalised, because every engine path is NFC, and patterns match
//! case-insensitively, because the server dedups names that way. Both the pattern and the path are
//! folded with [`collision_key`]: globset's own case-insensitive flag folds ASCII only (it
//! byte-escapes every non-ASCII literal), so `Ä*` would miss `ä.txt`. A backslash in a name is one
//! character on every platform, never a separator (see [`backslash`]).
//!
//! An item under an ignored directory can never be re-included, as in git. `ignore`'s own
//! `matched_path_or_any_parents` gets that wrong (it returns the leaf's whitelist before looking
//! at the parents), so callers that see paths out of tree order go through
//! [`IgnoreRules::ignored_root`].

use std::{
	borrow::Cow,
	collections::{BTreeSet, HashMap},
	fmt,
	path::{Path, PathBuf},
	sync::{Arc, LazyLock},
};

use filen_macros::js_type;
use ignore::{
	Match,
	gitignore::{Gitignore, GitignoreBuilder, Glob},
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use self::backslash::{path_key, pattern_line};
use super::{
	SyncMode,
	baseline::NodeKind,
	plan::{RemoteNode, RemoteView, ancestors},
	rows::Baseline,
	scan::collision_key,
	side::{Nodes, NodesAt},
};
use crate::Error;

mod backslash;

/// The name of a per-directory rule file.
pub(crate) const FILENIGNORE: &str = ".filenignore";

/// The largest `.filenignore` a pass reads, on disk or on the remote. A bigger one blocks its
/// directory.
pub(crate) const MAX_RULE_FILE_BYTES: u64 = 1024 * 1024;

/// The built-in patterns, applied below every other level: a user-level or `.filenignore` line
/// can re-include an entry with `!`.
pub const DEFAULT_IGNORE_PATTERNS: &[&str] = &[
	// macOS
	".DS_Store",
	"._*",
	".Spotlight-V100/",
	".fseventsd/",
	".TemporaryItems/",
	".DocumentRevisions-V100/",
	".Trashes/",
	// Windows
	"Thumbs.db",
	"ehthumbs.db",
	"ehthumbs_vista.db",
	"desktop.ini",
	"$RECYCLE.BIN/",
	"System Volume Information/",
	// Office and LibreOffice lock files
	"~$*",
	".~lock.*#",
	// Vim swap files, Emacs lock links
	"*.swp",
	"*.swo",
	"*.swn",
	".#*",
	// Chrome partial downloads
	"*.crdownload",
];

static DEFAULTS: LazyLock<IgnoreSource> = LazyLock::new(|| {
	let (source, errors) =
		IgnoreSource::parse(&DEFAULT_IGNORE_PATTERNS.join("\n"), Origin::Default)
			.expect("the built-in ignore patterns compile");
	debug_assert!(errors.is_empty(), "{errors:?}");
	source
});

/// Where a rule comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Origin<'a> {
	Default,
	User,
	/// The `.filenignore` in this root-relative directory (`""` is the pair root).
	File {
		dir: &'a str,
	},
}

impl fmt::Display for Origin<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Default => f.write_str("default ignore patterns"),
			Self::User => f.write_str("user ignore patterns"),
			Self::File { dir: "" } => f.write_str(FILENIGNORE),
			Self::File { dir } => write!(f, "{dir}/{FILENIGNORE}"),
		}
	}
}

/// The level the rule that ignores a path comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, tagged, napi)]
pub enum IgnoreLevel {
	/// [`DEFAULT_IGNORE_PATTERNS`].
	Default,
	/// The device-wide user patterns.
	User,
	/// The `.filenignore` in this root-relative directory (`""` is the pair root). In a pair that
	/// only pulls, `""` also stands for the pair root's file on disk that no pass has synced and
	/// that differs from the remote root file, which such a pair reads as local configuration beside
	/// the remote root file.
	File { dir: String },
}

impl fmt::Display for IgnoreLevel {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Default => Origin::Default.fmt(f),
			Self::User => Origin::User.fmt(f),
			Self::File { dir } => Origin::File { dir }.fmt(f),
		}
	}
}

/// The top of a subtree the ignore rules hide: nothing at or under it is synced in either direction,
/// and neither copy is touched.
#[derive(Debug, Clone, PartialEq, Eq)]
#[js_type(export, no_deser, no_default, napi)]
pub struct IgnoredPath {
	pub rel_path: String,
	pub level: IgnoreLevel,
	/// The deciding line, as the user wrote it: NFC, before the case folding the matcher does, so a
	/// rule written `Ä*` reports `Ä*`, and one that escapes its trailing space keeps it. Where an
	/// ancestor directory's rule hides the path, `rel_path` is that ancestor, so the line names the
	/// reported item; where both sides hide it, the local side's line is the one reported. The line
	/// is reported as it matched, against the path it matched: a folded directory move re-keys the
	/// entry, so a path-shaped line (`docs/build/`) can still name the pre-move path until the next
	/// pass decides again.
	pub pattern: String,
	/// A baseline row sat at or under the path when the pass read it: it was synced before, and this
	/// pass stopped tracking it. Removing the rule later syncs it like a first sync. Only the pass
	/// that drops the rows reports it so; a dry run reports it until a pass has run.
	pub tracked: bool,
}

impl fmt::Display for IgnoredPath {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(
			f,
			"ignored {:?} (by {}: {})",
			self.rel_path, self.level, self.pattern
		)?;
		if self.tracked {
			f.write_str(", no longer synced")?;
		}
		Ok(())
	}
}

impl From<Origin<'_>> for IgnoreLevel {
	fn from(origin: Origin<'_>) -> Self {
		match origin {
			Origin::Default => Self::Default,
			Origin::User => Self::User,
			Origin::File { dir } => Self::File {
				dir: dir.to_owned(),
			},
		}
	}
}

/// A pattern text that could not be used. `line` is 1-based; `None` means the source as a whole
/// failed to compile, so none of its rules can be trusted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{origin}{}: {reason}", line.map(|l| format!(":{l}")).unwrap_or_default())]
pub(crate) struct IgnoreParseError {
	/// The displayed [`Origin`], e.g. `src/.filenignore`.
	pub(crate) origin: String,
	pub(crate) line: Option<usize>,
	pub(crate) reason: String,
}

/// The compiled patterns of one source.
#[derive(Debug)]
pub(crate) struct IgnoreSource(Gitignore);

impl IgnoreSource {
	/// Compiles a pattern text. A bad line is skipped and reported, the rest still applies (git
	/// skips it silently). `Err` when the source as a whole does not compile.
	pub(crate) fn parse(
		text: &str,
		origin: Origin<'_>,
	) -> Result<(Self, Vec<IgnoreParseError>), IgnoreParseError> {
		// Rooted at "." so `Gitignore::matched` never strips anything: callers pass the path
		// already relative to the source's directory.
		let mut builder = GitignoreBuilder::new(".");
		let mut errors = Vec::new();
		// `lines` also drops the `\r` of a CRLF ending.
		for (i, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
			let line = line.nfc().collect::<String>();
			// `from` is only carried back on a match, so it holds the unfolded line for reporting.
			let folded = collision_key(&line);
			if let Err(e) = builder.add_line(Some(PathBuf::from(&line)), &pattern_line(&folded)) {
				errors.push(IgnoreParseError {
					origin: origin.to_string(),
					line: Some(i + 1),
					reason: e.to_string(),
				});
			}
		}
		let matcher = builder.build().map_err(|e| IgnoreParseError {
			origin: origin.to_string(),
			line: None,
			reason: e.to_string(),
		})?;
		Ok((Self(matcher), errors))
	}

	fn matched(&self, rel_path: &str, is_dir: bool) -> Match<&str> {
		match self.0.matched(Path::new(&path_key(rel_path)), is_dir) {
			Match::None => Match::None,
			Match::Ignore(glob) => Match::Ignore(as_written(glob)),
			Match::Whitelist(glob) => Match::Whitelist(as_written(glob)),
		}
	}
}

/// Compiles the device-wide user level. Unlike a `.filenignore` it is all or nothing: the first bad
/// line refuses the whole text, so what was set is exactly what applies.
pub(crate) fn parse_user_ignore(text: &str) -> Result<IgnoreSource, IgnoreParseError> {
	let (source, errors) = IgnoreSource::parse(text, Origin::User)?;
	match errors.into_iter().next() {
		Some(error) => Err(error),
		None => Ok(source),
	}
}

/// Why a path is ignored: the deciding line and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IgnoreHit<'a> {
	pub(crate) origin: Origin<'a>,
	/// The line as written (trimmed, NFC).
	pub(crate) pattern: &'a str,
}

/// An [`IgnoreHit`] a pass keeps: the level and the deciding line, owned. The line rides BESIDE the
/// level, never inside it, because two levels that differ only by which line matched must still
/// compare equal — the withheld-delete rule in `engine.rs` reads them by value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IgnoreDecision {
	pub(crate) level: IgnoreLevel,
	/// The line as written (see [`IgnoredPath::pattern`]).
	pub(crate) pattern: String,
}

impl From<IgnoreHit<'_>> for IgnoreDecision {
	fn from(hit: IgnoreHit<'_>) -> Self {
		Self {
			level: hit.origin.into(),
			pattern: hit.pattern.to_owned(),
		}
	}
}

/// Every rule a pass evaluates.
#[derive(Debug, Default)]
pub(crate) struct IgnoreRules {
	user: Option<IgnoreSource>,
	/// Keyed by root-relative directory, `""` for the pair root.
	files: HashMap<String, IgnoreSource>,
	/// The local root file of a pair that only pulls (see [`Self::insert_local_root`]).
	local_root: Option<IgnoreSource>,
}

impl IgnoreRules {
	pub(crate) fn new(user: Option<IgnoreSource>) -> Self {
		Self {
			user,
			files: HashMap::new(),
			local_root: None,
		}
	}

	/// Whether any `.filenignore` has joined the user's rules.
	pub(crate) fn has_files(&self) -> bool {
		!self.files.is_empty() || self.local_root.is_some()
	}

	/// Adds the `.filenignore` of `dir`, replacing any earlier one.
	pub(crate) fn insert_file(&mut self, dir: String, source: IgnoreSource) {
		self.files.insert(dir, source);
	}

	/// Adds the pair root's `.filenignore` on disk as LOCAL configuration of a pair that only pulls,
	/// beside any remote root file rather than in its place. The caller adds it only while no
	/// baseline row records that path, and only where its bytes differ from the remote root file's:
	/// a row, or identical bytes, mean the file is the synced copy of the remote's.
	///
	/// Its rules decide at the root level before the remote root file's (see the module docs), and
	/// the file itself is kept out of the sync on both sides while it is here: never deleted or
	/// binned as an extra, never uploaded, and the remote root file never downloaded over it. Kept
	/// out the way a built-in default keeps out an untracked `.DS_Store` — hidden, not reported —
	/// which is exact because no row sits at its path. Its own lines cannot re-include it, as no
	/// pattern can re-include the engine's own files.
	pub(crate) fn insert_local_root(&mut self, source: IgnoreSource) {
		self.local_root = Some(source);
	}

	/// The decision for `rel_path` alone, nearest source first. Ancestors are not consulted: use
	/// [`Self::is_ignored_with_ancestors`] unless every ancestor is already known to be synced.
	pub(crate) fn decide(&self, rel_path: &str, is_dir: bool) -> Option<IgnoreHit<'_>> {
		if self.local_root.is_some() && !is_dir && rel_path == FILENIGNORE {
			return Some(IgnoreHit {
				origin: Origin::Default,
				pattern: FILENIGNORE,
			});
		}
		let mut dir = parent(rel_path);
		loop {
			if dir.is_empty()
				&& let Some(source) = &self.local_root
			{
				match source.matched(rel_path, is_dir) {
					Match::Whitelist(_) => return None,
					Match::Ignore(pattern) => {
						return Some(IgnoreHit {
							origin: Origin::File { dir: "" },
							pattern,
						});
					}
					Match::None => {}
				}
			}
			if let Some((key, source)) = self.files.get_key_value(dir) {
				// A pattern in `dir`'s file sees the path below `dir`.
				let rel = match dir {
					"" => rel_path,
					dir => rel_path
						.strip_prefix(dir)
						.and_then(|rest| rest.strip_prefix('/'))
						.expect("`dir` is an ancestor of `rel_path` (should be impossible)"),
				};
				let origin = Origin::File { dir: key };
				match source.matched(rel, is_dir) {
					Match::Whitelist(_) => return None,
					Match::Ignore(pattern) => return Some(IgnoreHit { origin, pattern }),
					Match::None => {}
				}
			}
			if dir.is_empty() {
				break;
			}
			dir = parent(dir);
		}
		for (source, origin) in [
			(self.user.as_ref(), Origin::User),
			(Some(&*DEFAULTS), Origin::Default),
		] {
			match source.map(|s| s.matched(rel_path, is_dir)) {
				Some(Match::Whitelist(_)) => return None,
				Some(Match::Ignore(pattern)) => return Some(IgnoreHit { origin, pattern }),
				Some(Match::None) | None => {}
			}
		}
		None
	}

	/// The top-most ignored path at or above `rel_path`, with the rule that hides it: git's rule that
	/// nothing under an ignored directory can be re-included. `memo` caches per-directory answers
	/// across calls on the same rules.
	pub(crate) fn ignored_root<'p>(
		&self,
		rel_path: &'p str,
		is_dir: bool,
		memo: &mut HashMap<String, Option<IgnoreDecision>>,
	) -> Option<(&'p str, IgnoreDecision)> {
		// The pair root itself is never ignored.
		if rel_path.is_empty() {
			return None;
		}
		for ancestor in ancestors(rel_path) {
			if !memo.contains_key(ancestor) {
				let decision = self.decide(ancestor, true).map(IgnoreDecision::from);
				memo.insert(ancestor.to_owned(), decision);
			}
			if let Some(Some(decision)) = memo.get(ancestor) {
				// Owned: the caller keeps the decision past the memo. The memo saves the matching, not
				// this copy — a caller that only asks whether a path is hidden still copies the line
				// once per call.
				return Some((ancestor, decision.clone()));
			}
		}
		self.decide(rel_path, is_dir)
			.map(|hit| (rel_path, hit.into()))
	}
}

/// The remote `.filenignore` files a pass reads, and what it could not read.
#[derive(Debug, Default)]
pub(crate) struct RemoteRules {
	pub(crate) rules: IgnoreRules,
	/// Directories whose remote rules could not be used: blocked for the pass.
	pub(crate) blocked: BTreeSet<String>,
	/// One report line per rule file that could not be used, or per bad line in one.
	pub(crate) errors: Vec<String>,
	/// Every body read this pass, by remote uuid: the cache the next pass starts from. A content edit
	/// mints a new uuid, so a cached body is never stale.
	pub(crate) bodies: HashMap<Uuid, Arc<str>>,
	/// The body of the remote root `.filenignore` read this pass, if one was: what tells a pair that
	/// only pulls a copy of it on disk from local configuration (see
	/// [`UnrecordedRoot`](super::scan::UnrecordedRoot)).
	pub(crate) root_body: Option<Arc<str>>,
}

/// Reads the remote `.filenignore` files whose rules `mode` takes from the remote, on top of the
/// `user` level.
///
/// A mode that only pulls reads every one, shallowest first, and skips a file under a directory the
/// rules read so far ignore. The other modes take a directory's rules from disk, and read a remote
/// file only where `local_has_file` says the directory has no copy there: a two-way pair wherever
/// that is so (a remote-only rule file applies before it is downloaded); a mode that only pushes only
/// where `tracked` says the file has a baseline row, since a synced rule file the disk has lost still
/// governs until that loss propagates, and a directory deleted locally has to keep what those rules
/// hide on the remote. Neither skips a file under an ignored directory, because a local file the scan
/// has not read yet may re-include that directory.
///
/// A body is taken from `cached` by uuid, or else fetched. A file larger than
/// [`MAX_RULE_FILE_BYTES`], one that cannot be fetched, is not UTF-8 or does not compile blocks its
/// directory for the pass: guessing its rules could sync what the user meant to hide. So does one
/// the view holds back as listed twice, unless a mode that only pulls ignores its directory anyway.
///
/// A pair that only pulls may also have a local root file (see [`IgnoreRules::insert_local_root`]),
/// which the scan reads after this, comparing it with [`RemoteRules::root_body`]: a remote file under a directory only that file ignores is still
/// read here, and a failure to read it still blocks that directory.
pub(crate) async fn load_remote_rules<Fetch, Fut>(
	mode: SyncMode,
	candidates: RuleCandidates<'_>,
	user: Option<IgnoreSource>,
	local_has_file: impl Fn(&str) -> bool,
	tracked: impl Fn(&str) -> bool,
	cached: &HashMap<Uuid, Arc<str>>,
	mut fetch: Fetch,
) -> RemoteRules
where
	Fetch: FnMut(Uuid) -> Fut,
	Fut: Future<Output = Result<Vec<u8>, Error>>,
{
	let view = candidates.view;
	// Captured before `candidates.paths` is moved out below, which partially moves `candidates`.
	let rule_baseline = candidates.baseline;
	let mut out = RemoteRules {
		rules: IgnoreRules::new(user),
		..RemoteRules::default()
	};
	let reads_remote = |dir: &str| match mode {
		SyncMode::TwoWay => !local_has_file(dir),
		_ if mode.pushes() => tracked(dir) && !local_has_file(dir),
		_ => true,
	};
	// Only a mode that pulls alone has every rule above a file before the scan: in the others a file
	// on disk may still re-include what the remote rules ignore.
	let under_ignored = |rules: &IgnoreRules, dir: &str| {
		!mode.pushes() && rules.ignored_root(dir, true, &mut HashMap::new()).is_some()
	};
	// Deduplicated and ordered by the set, so a caller may name a path twice — a change-scoped
	// pass offers the resident index AND the paths its delta moved, and a rule file that moved is
	// in both — without the file being read, reported or blocked twice.
	let named: BTreeSet<String> = candidates.paths.into_iter().collect();
	// BOUND, not read through a temporary: each node comes back as a `Cow` borrowed from the
	// reader, and a carried side builds those on the spot.
	let view_nodes = view.nodes.of(rule_baseline);
	let mut candidates: Vec<(&str, Cow<'_, RemoteNode>)> = named
		.iter()
		.filter_map(|rel_path| {
			let node = view_nodes.at(rel_path)?;
			(node.kind == NodeKind::File).then_some((rule_file_dir(rel_path)?, node))
		})
		.collect();
	candidates.sort_by_key(|(dir, _)| dir.matches('/').count() + usize::from(!dir.is_empty()));
	for (dir, node) in candidates {
		if !reads_remote(dir) || under_ignored(&out.rules, dir) {
			continue;
		}
		let origin = Origin::File { dir };
		let body = match cached.get(&node.remote_uuid) {
			Some(body) => Arc::clone(body),
			None => match fetch_rule_file(&node, &mut fetch).await {
				Ok(body) => body,
				Err(reason) => {
					out.blocked.insert(dir.to_owned());
					out.errors.push(format!("remote {origin}: {reason}"));
					continue;
				}
			},
		};
		match IgnoreSource::parse(&body, origin) {
			Ok((source, line_errors)) => {
				out.rules.insert_file(dir.to_owned(), source);
				out.errors
					.extend(line_errors.iter().map(|error| format!("remote {error}")));
			}
			Err(error) => {
				out.blocked.insert(dir.to_owned());
				out.errors.push(format!("remote {error}"));
			}
		}
		if dir.is_empty() {
			out.root_body = Some(Arc::clone(&body));
		}
		out.bodies.insert(node.remote_uuid, body);
	}
	// After the rules above them are read, so a mode that pulls alone does not report one it ignores.
	for dir in view
		.held_paths
		.iter()
		.filter_map(|path| rule_file_dir(path))
	{
		if reads_remote(dir) && !under_ignored(&out.rules, dir) {
			out.blocked.insert(dir.to_owned());
			out.errors.push(format!(
				"remote {}: listed twice while the cache catches up, not read this pass",
				Origin::File { dir }
			));
		}
	}
	out
}

/// The body of the remote rule file `node`, or why it cannot be used.
async fn fetch_rule_file<Fetch, Fut>(
	node: &RemoteNode,
	fetch: &mut Fetch,
) -> Result<Arc<str>, String>
where
	Fetch: FnMut(Uuid) -> Fut,
	Fut: Future<Output = Result<Vec<u8>, Error>>,
{
	if node.size > MAX_RULE_FILE_BYTES {
		return Err(too_large());
	}
	let bytes = fetch(node.remote_uuid)
		.await
		.map_err(|e| format!("could not be downloaded: {e}"))?;
	rule_file_text(bytes).map(Arc::from)
}

/// The text of a `.filenignore` body, or why it cannot be used. The copy on disk and the remote copy
/// are read by this one rule, so no device applies a file another device refuses.
pub(crate) fn rule_file_text(bytes: Vec<u8>) -> Result<String, String> {
	if u64::try_from(bytes.len()).map_or(true, |len| len > MAX_RULE_FILE_BYTES) {
		return Err(too_large());
	}
	String::from_utf8(bytes).map_err(|_| "not valid UTF-8, not read".to_owned())
}

fn too_large() -> String {
	format!("larger than {MAX_RULE_FILE_BYTES} bytes, not read")
}

/// The root-relative directory a `.filenignore` at `rel_path` belongs to, or `None` for any other
/// path.
/// The `.filenignore` files a pass offers [`load_remote_rules`], and the view they sit in.
///
/// The two travel together because neither answers alone: the paths say WHICH nodes to consider,
/// and only the caller knows the cheap way to name them; the view is what says whether each is
/// still there, and what else the pass is holding back.
pub(crate) struct RuleCandidates<'a> {
	pub(crate) view: &'a RemoteView,
	/// The tree the view's carried half derives its nodes from (see
	/// [`Side::of`](super::side::Side::of)).
	pub(crate) baseline: &'a Baseline,
	pub(crate) paths: Vec<String>,
}

/// Every path in `view` that IS a `.filenignore`, which is what a pass that read the remote WHOLE
/// offers [`load_remote_rules`] as its candidate set: it has the map in hand and no index of it, so
/// finding them costs the scan of a map it just built. A change-scoped pass has the opposite — no
/// map worth scanning and an index that answers — and names them from
/// [`Baseline::rule_file_rows`](super::rows::Baseline::rule_file_rows) instead.
pub(crate) fn rule_file_paths(view: &RemoteView, baseline: &Baseline) -> Vec<String> {
	view.nodes
		.of(baseline)
		.iter()
		.filter(|(rel_path, node)| node.kind == NodeKind::File && rule_file_dir(rel_path).is_some())
		.map(|(rel_path, _)| rel_path.into_owned())
		.collect()
}

pub(crate) fn rule_file_dir(rel_path: &str) -> Option<&str> {
	match rel_path.strip_suffix(FILENIGNORE)? {
		"" => Some(""),
		dir => dir.strip_suffix('/'),
	}
}

/// The matched line before case folding (`Glob::original` is the folded one). Trailing whitespace is
/// dropped exactly where the parser drops it, so a line that escapes its trailing space keeps both
/// the escape and the space instead of reporting a dangling `\`.
fn as_written(glob: &Glob) -> &str {
	glob.from()
		.and_then(Path::to_str)
		.map_or(glob.original(), |line| {
			if line.ends_with("\\ ") {
				line
			} else {
				line.trim_end()
			}
		})
}

fn parent(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

#[cfg(test)]
mod tests;
