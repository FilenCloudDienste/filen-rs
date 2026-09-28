//! `.filenignore` rules: gitignore(5) syntax and precedence, evaluated against root-relative paths.
//!
//! Sources, highest precedence first: the `.filenignore` in the item's own directory, the
//! shallower `.filenignore` files up to the pair root, the device-wide user level, then
//! [`DEFAULT_IGNORE_PATTERNS`]. Within one source the last matching line decides. The engine's
//! own internals (the quarantine directory, partial downloads, the watch staging filter) are not
//! patterns and stay outside every level.
//!
//! Every line is NFC-normalised, because every engine path is NFC, and patterns match
//! case-insensitively, because the server dedups names that way. Both the pattern and the path are
//! folded with [`collision_key`]: globset's own case-insensitive flag folds ASCII only (it
//! byte-escapes every non-ASCII literal), so `Ä*` would miss `ä.txt`.
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

use ignore::{
	Match,
	gitignore::{Gitignore, GitignoreBuilder, Glob},
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	SyncMode,
	baseline::NodeKind,
	plan::{RemoteNode, RemoteView},
	rows::Baseline,
	scan::collision_key,
	side::{Nodes, NodesAt},
};
use crate::Error;

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
#[non_exhaustive]
pub enum IgnoreLevel {
	/// [`DEFAULT_IGNORE_PATTERNS`].
	Default,
	/// The device-wide user patterns.
	User,
	/// The `.filenignore` in this root-relative directory (`""` is the pair root).
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
#[non_exhaustive]
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

impl IgnoredPath {
	/// Build one. The struct is `#[non_exhaustive]`, so this is the only way to write one from
	/// outside the crate — the engine produces these, a caller compares against them.
	///
	/// Hidden from the rendered docs because comparing is all a caller should be doing with it:
	/// it exists so the test suite, an external crate, can say what a pass should have reported
	/// without a wildcard that would hide the next field. Code that MINTS these takes on the break
	/// `#[non_exhaustive]` was added to prevent — a later field changes this signature.
	#[doc(hidden)]
	pub fn new(
		rel_path: impl Into<String>,
		level: IgnoreLevel,
		pattern: impl Into<String>,
		tracked: bool,
	) -> Self {
		Self {
			rel_path: rel_path.into(),
			level,
			pattern: pattern.into(),
			tracked,
		}
	}
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
			if let Err(e) = builder.add_line(Some(PathBuf::from(&line)), &collision_key(&line)) {
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
		match self.0.matched(Path::new(&collision_key(rel_path)), is_dir) {
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
}

impl IgnoreRules {
	pub(crate) fn new(user: Option<IgnoreSource>) -> Self {
		Self {
			user,
			files: HashMap::new(),
		}
	}

	/// Adds the `.filenignore` of `dir`, replacing any earlier one.
	pub(crate) fn insert_file(&mut self, dir: String, source: IgnoreSource) {
		self.files.insert(dir, source);
	}

	/// The decision for `rel_path` alone, nearest source first. Ancestors are not consulted: use
	/// [`Self::is_ignored_with_ancestors`] unless every ancestor is already known to be synced.
	pub(crate) fn decide(&self, rel_path: &str, is_dir: bool) -> Option<IgnoreHit<'_>> {
		let mut dir = parent(rel_path);
		loop {
			if let Some((key, source)) = self.files.get_key_value(dir) {
				let rel = if dir.is_empty() {
					rel_path
				} else {
					&rel_path[dir.len() + 1..]
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
		for (i, _) in rel_path.match_indices('/') {
			let ancestor = &rel_path[..i];
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
mod tests {
	use std::collections::BTreeMap;

	use super::*;
	use crate::ErrorKind;

	fn source(text: &str, origin: Origin<'_>) -> IgnoreSource {
		let (source, errors) = IgnoreSource::parse(text, origin).unwrap();
		assert!(errors.is_empty(), "{errors:?}");
		source
	}

	fn root_rules(text: &str) -> IgnoreRules {
		let mut rules = IgnoreRules::default();
		rules.insert_file(String::new(), source(text, Origin::File { dir: "" }));
		rules
	}

	fn ignored(rules: &IgnoreRules, rel_path: &str, is_dir: bool) -> bool {
		rules
			.ignored_root(rel_path, is_dir, &mut HashMap::new())
			.is_some()
	}

	/// The decision hiding `rel_path`, which must be hidden.
	fn decision(rules: &IgnoreRules, rel_path: &str) -> IgnoreDecision {
		rules
			.ignored_root(rel_path, F, &mut HashMap::new())
			.unwrap_or_else(|| panic!("{rel_path} is not ignored"))
			.1
	}

	const F: bool = false;
	const D: bool = true;

	#[test]
	fn gitignore_conformance() {
		#[rustfmt::skip]
		let probes: &[(&str, &str, &str, bool, bool)] = &[
			("1", "months", "months", F, true),
			("2", "months", "amonths", F, false),
			("3", "months", "monthsa", F, false),
			("4", "*.lock", "Cargo.lock", F, true),
			("5", "*.rs", "src/main.rs", F, true),
			("6", "src/*.rs", "src/grep/src/main.rs", F, false),
			("7", "/*.c", "cat-file.c", F, true),
			("8", "/*.c", "mozilla-sha1/sha1.c", F, false),
			("9", "/src/*.rs", "src/grep/src/main.rs", F, false),
			("10", "!src/main.rs\n*.rs", "src/main.rs", F, true),
			("11", "*.rs\n!src/main.rs", "src/main.rs", F, false),
			("12", "foo/", "foo", D, true),
			("13", "foo/", "foo", F, false),
			("14", "**/foo", "src/foo", F, true),
			("15", "**/foo/**", "wat/src/foo/bar/baz", F, true),
			("16", "**/foo/**", "wat/src/afoo/bar/baz", F, false),
			("17", "abc/**", "abc", D, false),
			("18", "a/**/b", "a/x/y/b", F, true),
			("19", "\\!xy", "!xy", F, true),
			("20", "\\#foo", "#foo", F, true),
			("21", "#foo", "#foo", F, false),
			("22", "node_modules/ ", "node_modules", D, true),
			("23", "*.html", "foo.HTML", F, true),
			("24", "path1/*", "path2/path1/foo", F, false),
			("25", "dir/\n!dir/keep", "dir/keep", F, true),
			("25b", "dir\n!dir/keep", "dir/keep", F, true),
			("26", "dir/*\n!dir/keep", "dir/keep", F, false),
		];
		for &(n, patterns, path, is_dir, expect) in probes {
			let rules = root_rules(patterns);
			assert_eq!(
				ignored(&rules, path, is_dir),
				expect,
				"probe {n}: {patterns:?} vs {path:?}"
			);
		}
		// The trap 25/25b guard against: the crate alone re-includes under an excluded parent.
		for patterns in ["dir/\n!dir/keep", "dir\n!dir/keep"] {
			let mut builder = GitignoreBuilder::new(".");
			for line in patterns.lines() {
				builder.add_line(None, line).unwrap();
			}
			let matcher = builder.build().unwrap();
			assert!(
				matcher
					.matched_path_or_any_parents("dir/keep", false)
					.is_whitelist()
			);
		}
	}

	#[test]
	fn nested_file_is_scoped_to_its_directory() {
		let mut rules = IgnoreRules::default();
		rules.insert_file(
			"src".into(),
			source("/build\n*.o", Origin::File { dir: "src" }),
		);
		assert!(ignored(&rules, "src/build", D));
		assert!(!ignored(&rules, "build", D));
		assert!(ignored(&rules, "src/a/b.o", F));
		assert!(!ignored(&rules, "lib/b.o", F));
		assert_eq!(
			rules.decide("src/a/b.o", F),
			Some(IgnoreHit {
				origin: Origin::File { dir: "src" },
				pattern: "*.o"
			})
		);
	}

	#[test]
	fn deeper_file_beats_shallower() {
		let mut rules = root_rules("*.log");
		rules.insert_file("src".into(), source("!*.log", Origin::File { dir: "src" }));
		assert!(!ignored(&rules, "src/a.log", F));
		assert!(!ignored(&rules, "src/deep/a.log", F));
		assert!(ignored(&rules, "b.log", F));
	}

	#[test]
	fn nested_whitelist_cannot_reinclude_under_an_ignored_directory() {
		let mut rules = root_rules("dir/");
		rules.insert_file("dir".into(), source("!keep", Origin::File { dir: "dir" }));
		assert!(rules.decide("dir/keep", F).is_none());
		assert!(ignored(&rules, "dir/keep", F));
	}

	#[test]
	fn file_beats_user_beats_defaults() {
		let mut rules = IgnoreRules::new(Some(source("*.psd\n!.DS_Store", Origin::User)));
		assert!(ignored(&rules, "a.psd", F));
		assert!(!ignored(&rules, "a/.DS_Store", F));
		assert_eq!(
			rules.decide("a.psd", F).map(|hit| hit.origin),
			Some(Origin::User)
		);
		rules.insert_file(String::new(), source("!*.psd", Origin::File { dir: "" }));
		assert!(!ignored(&rules, "a.psd", F));

		let bare = IgnoreRules::default();
		assert_eq!(
			bare.decide("a/.DS_Store", F),
			Some(IgnoreHit {
				origin: Origin::Default,
				pattern: ".DS_Store"
			})
		);
	}

	#[test]
	fn matching_is_case_insensitive() {
		let rules = IgnoreRules::new(Some(source("*.JPG\nÄ*", Origin::User)));
		assert!(ignored(&rules, "img.jpg", F));
		assert!(ignored(&rules, "ä.txt", F));
		assert!(ignored(&rules, "Ä.TXT", F));
		assert_eq!(
			rules.decide("img.jpg", F).map(|hit| hit.pattern),
			Some("*.JPG")
		);
	}

	#[test]
	fn nfd_pattern_matches_nfc_path() {
		let rules = root_rules("e\u{301}t\u{e9}.txt");
		assert!(ignored(&rules, "\u{e9}t\u{e9}.txt", F));
	}

	#[test]
	fn brace_alternation_and_double_star_pins() {
		// Divergence from git, which reads braces literally: globset alternates.
		let rules = root_rules("*.{jpg,png}");
		assert!(ignored(&rules, "a.png", F));
		assert!(!ignored(&rules, "a.{jpg,png}", F));
		assert!(ignored(&root_rules("\\{a\\}"), "{a}", F));
		// `**` away from a separator is two `*`, as in git.
		let rules = root_rules("a**b");
		assert!(ignored(&rules, "axyzb", F));
		assert!(!ignored(&rules, "ax/yb", F));
	}

	#[test]
	fn bad_line_is_reported_and_the_rest_applies() {
		let text = "\u{feff}*.log\r\n[z-a]\r\n!keep.log\r\n";
		let (source, errors) = IgnoreSource::parse(text, Origin::File { dir: "src" }).unwrap();
		assert_eq!(errors.len(), 1);
		assert_eq!(errors[0].line, Some(2));
		assert!(
			errors[0].to_string().starts_with("src/.filenignore:2: "),
			"{}",
			errors[0]
		);
		let mut rules = IgnoreRules::default();
		rules.insert_file("src".into(), source);
		assert!(ignored(&rules, "src/a.log", F));
		assert!(!ignored(&rules, "src/keep.log", F));

		let (_, errors) = IgnoreSource::parse("{a", Origin::User).unwrap();
		assert!(
			errors[0]
				.to_string()
				.starts_with("user ignore patterns:1: ")
		);
	}

	/// The user level is all or nothing, unlike a `.filenignore`: one bad line refuses the whole text,
	/// naming it, so what the engine stores is exactly what applies.
	#[test]
	fn a_bad_line_refuses_the_whole_user_level() {
		let error = parse_user_ignore("*.psd\n[z-a]\n!keep.psd\n")
			.expect_err("a bad line refuses the whole text");
		assert_eq!(error.line, Some(2));
		assert!(
			error.to_string().starts_with("user ignore patterns:2: "),
			"{error}"
		);

		let rules = IgnoreRules::new(Some(
			parse_user_ignore("*.psd\n!keep.psd\n").expect("a clean text compiles"),
		));
		assert!(ignored(&rules, "a.psd", F));
		assert!(!ignored(&rules, "keep.psd", F));
	}

	#[test]
	fn memo_serves_later_paths_under_an_ignored_directory() {
		let rules = root_rules("build/");
		let mut memo = HashMap::new();
		let by_root = IgnoreDecision {
			level: IgnoreLevel::File { dir: String::new() },
			pattern: "build/".to_string(),
		};
		assert_eq!(
			rules.ignored_root("build/a/b", F, &mut memo),
			Some(("build", by_root.clone()))
		);
		assert_eq!(memo.get("build"), Some(&Some(by_root.clone())));
		assert_eq!(
			rules.ignored_root("build/c", F, &mut memo),
			Some(("build", by_root))
		);
		assert_eq!(rules.ignored_root("src/c", F, &mut memo), None);
		assert_eq!(memo.get("src"), Some(&None));
		assert_eq!(rules.ignored_root("", D, &mut memo), None);
	}

	/// A path an ancestor directory's rule hides is reported at that ancestor, so the line that comes
	/// back is the line for the reported path — the leaf's own name matched nothing.
	#[test]
	fn an_ancestor_rule_comes_back_with_the_ancestor() {
		let rules = root_rules("*.log\nbuild/");
		// A leaf the `*.log` line also matches, so consulting the leaf first would come back with a
		// different path AND a different line.
		assert_eq!(
			rules.ignored_root("build/deep/a.log", F, &mut HashMap::new()),
			Some((
				"build",
				IgnoreDecision {
					level: IgnoreLevel::File { dir: String::new() },
					pattern: "build/".to_string(),
				}
			))
		);
	}

	/// The line comes back as the user wrote it, not as the case-folded text the matcher holds.
	#[test]
	fn the_reported_line_is_the_one_as_written() {
		let rules = IgnoreRules::new(Some(source("Ä*\n*.JPG", Origin::User)));
		assert_eq!(decision(&rules, "ä.txt").pattern, "Ä*");
		assert_eq!(decision(&rules, "sub/IMG.jpg").pattern, "*.JPG");
	}

	/// A line that escapes its trailing space keeps it. Trimming it off would report a dangling `\`,
	/// which is not a line anyone wrote and not one that would match if pasted back.
	#[test]
	fn an_escaped_trailing_space_is_reported_with_the_space() {
		let rules = root_rules("node_modules\\ ");
		assert_eq!(decision(&rules, "node_modules ").pattern, "node_modules\\ ");
	}

	/// The line rides beside the level, never inside it: two paths one source hides with different
	/// lines keep equal levels, which is what the rule that keeps a deleted directory compares.
	#[test]
	fn two_decisions_differing_only_in_the_line_keep_equal_levels() {
		let rules = root_rules("*.log\n*.tmp");
		let one = decision(&rules, "a.log");
		let other = decision(&rules, "b.tmp");
		assert_ne!(one.pattern, other.pattern);
		assert_eq!(one.level, other.level);
		assert_ne!(one, other);
	}

	#[test]
	fn every_default_matches_its_example_and_misses_a_near_miss() {
		#[rustfmt::skip]
		let cases: &[(&str, bool, &str, bool)] = &[
			("a/.DS_Store", F, "a/DS_Store", F),
			("._photo.jpg", F, "_photo.jpg", F),
			(".Spotlight-V100", D, ".Spotlight-V100", F),
			(".fseventsd", D, ".fseventsd", F),
			(".TemporaryItems", D, ".TemporaryItems", F),
			(".DocumentRevisions-V100", D, ".DocumentRevisions-V100", F),
			(".Trashes", D, ".Trashes", F),
			("a/Thumbs.db", F, "a/Thumbs.dbx", F),
			("ehthumbs.db", F, "myehthumbs.db", F),
			("ehthumbs_vista.db", F, "ehthumbs_vista.db.bak", F),
			("desktop.ini", F, "desktop.ini.txt", F),
			("$RECYCLE.BIN", D, "$RECYCLE.BIN", F),
			("System Volume Information", D, "System Volume Information", F),
			("~$budget.xlsx", F, "budget~$.xlsx", F),
			(".~lock.doc.odt#", F, ".~lock.doc.odt", F),
			("a.swp", F, "a.swpx", F),
			("a.swo", F, "swo", F),
			("a.swn", F, "a.sw", F),
			(".#notes.org", F, "#notes.org", F),
			("x.crdownload", F, "x.crdownload.txt", F),
		];
		assert_eq!(cases.len(), DEFAULT_IGNORE_PATTERNS.len());
		let rules = IgnoreRules::default();
		for &(hit, hit_dir, miss, miss_dir) in cases {
			assert!(rules.decide(hit, hit_dir).is_some(), "{hit}");
			assert!(rules.decide(miss, miss_dir).is_none(), "{miss}");
		}
	}

	fn rule_node(rel_path: &str, size: u64) -> RemoteNode {
		RemoteNode {
			rel_path: rel_path.to_owned(),
			kind: NodeKind::File,
			remote_uuid: Uuid::new_v4(),
			stable_uuid: None,
			content_hash: None,
			size,
			modified_millis: 0,
		}
	}

	fn rule_view(nodes: Vec<RemoteNode>, held: &[&str]) -> RemoteView {
		RemoteView {
			nodes: nodes
				.into_iter()
				.map(|node| (node.rel_path.clone(), node))
				.collect(),
			has_collisions: false,
			held_paths: held.iter().map(|path| (*path).to_owned()).collect(),
			skipped: Vec::new(),
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		}
	}

	/// The remote rules `mode` reads from `view`, where a download serves `bodies` by path (a path
	/// without one fails) and `on_disk` lists the directories holding a local rule file. Also returns
	/// the paths downloaded, in order.
	async fn load(
		mode: SyncMode,
		view: &RemoteView,
		on_disk: &[&str],
		bodies: &HashMap<&str, Vec<u8>>,
		cached: &HashMap<Uuid, Arc<str>>,
	) -> (RemoteRules, Vec<String>) {
		load_tracked(mode, view, on_disk, &[], bodies, cached).await
	}

	/// [`load`], where `tracked` lists the directories whose rule file has a baseline row.
	async fn load_tracked(
		mode: SyncMode,
		view: &RemoteView,
		on_disk: &[&str],
		tracked: &[&str],
		bodies: &HashMap<&str, Vec<u8>>,
		cached: &HashMap<Uuid, Arc<str>>,
	) -> (RemoteRules, Vec<String>) {
		// A whole-read view: its backing never consults a baseline, so an empty one is exact.
		let no_baseline = Baseline::default();
		let paths: HashMap<Uuid, String> = view
			.nodes
			.of(&no_baseline)
			.iter()
			.map(|(_, node)| (node.remote_uuid, node.rel_path.clone()))
			.collect();
		let mut fetched = Vec::new();
		let rules = load_remote_rules(
			mode,
			RuleCandidates {
				view,
				baseline: &no_baseline,
				paths: rule_file_paths(view, &no_baseline),
			},
			None,
			|dir| on_disk.contains(&dir),
			|dir| tracked.contains(&dir),
			cached,
			|uuid| {
				let path = paths[&uuid].as_str();
				fetched.push(path.to_owned());
				std::future::ready(
					bodies
						.get(path)
						.cloned()
						.ok_or_else(|| Error::custom(ErrorKind::IO, "offline")),
				)
			},
		)
		.await;
		(rules, fetched)
	}

	fn remote_ignored(rules: &RemoteRules, rel_path: &str) -> bool {
		ignored(&rules.rules, rel_path, F)
	}

	#[tokio::test]
	async fn a_mode_that_only_pushes_reads_no_remote_rules() {
		let view = rule_view(vec![rule_node(FILENIGNORE, 2)], &[".filenignore"]);
		let bodies = HashMap::from([(FILENIGNORE, b"*\n".to_vec())]);
		for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
			let (rules, fetched) = load(mode, &view, &[], &bodies, &HashMap::new()).await;
			assert!(fetched.is_empty(), "{mode:?}: {fetched:?}");
			assert!(!remote_ignored(&rules, "a.txt"), "{mode:?}");
			assert!(
				rules.blocked.is_empty() && rules.errors.is_empty(),
				"{mode:?}"
			);
		}
	}

	/// A mode that only pushes takes its rules from disk, but a synced rule file the disk has lost (its
	/// row still stands) is read from the remote until the loss propagates: a directory deleted
	/// locally is still read with the rules that keep its ignored remote content.
	#[tokio::test]
	async fn a_mode_that_only_pushes_reads_a_synced_rule_file_the_disk_has_lost() {
		let view = rule_view(
			vec![
				rule_node(FILENIGNORE, 2),
				rule_node("proj/.filenignore", 14),
			],
			&[],
		);
		let bodies = HashMap::from([
			(FILENIGNORE, b"*\n".to_vec()),
			("proj/.filenignore", b"node_modules/\n".to_vec()),
		]);
		for mode in [SyncMode::LocalToRemote, SyncMode::LocalBackup] {
			let (rules, fetched) =
				load_tracked(mode, &view, &[""], &["", "proj"], &bodies, &HashMap::new()).await;
			assert_eq!(fetched, vec!["proj/.filenignore"], "{mode:?}");
			assert!(remote_ignored(&rules, "proj/node_modules/m.js"), "{mode:?}");
			assert!(!remote_ignored(&rules, "a.txt"), "{mode:?}");
			assert!(
				rules.blocked.is_empty() && rules.errors.is_empty(),
				"{mode:?}"
			);
		}
	}

	/// A pulling mode does not report a rule file held back under a directory the rules already
	/// ignore: nothing under that directory syncs anyway.
	#[tokio::test]
	async fn a_held_rule_file_under_an_ignored_directory_is_not_reported() {
		let view = rule_view(vec![rule_node(FILENIGNORE, 7)], &["cache/sub/.filenignore"]);
		let bodies = HashMap::from([(FILENIGNORE, b"cache/\n".to_vec())]);
		for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
			let (rules, _) = load(mode, &view, &[], &bodies, &HashMap::new()).await;
			assert!(
				rules.errors.is_empty() && rules.blocked.is_empty(),
				"{mode:?}: {:?}",
				rules.errors
			);
		}
	}

	/// A pulling mode reads its rules shallowest first, and never reads a file whose directory the
	/// shallower rules already ignore: git never looks inside an ignored directory either.
	#[tokio::test]
	async fn a_mode_that_pulls_reads_shallowest_first_and_skips_ignored_directories() {
		let view = rule_view(
			vec![
				rule_node("src/deep/.filenignore", 9),
				rule_node("cache/.filenignore", 3),
				rule_node("src/.filenignore", 4),
				rule_node(FILENIGNORE, 7),
			],
			&[],
		);
		let bodies = HashMap::from([
			(FILENIGNORE, b"cache/\n".to_vec()),
			("cache/.filenignore", b"!x\n".to_vec()),
			("src/.filenignore", b"*.o\n".to_vec()),
			("src/deep/.filenignore", b"!keep.o\n".to_vec()),
		]);
		for mode in [SyncMode::RemoteToLocal, SyncMode::RemoteBackup] {
			let (rules, fetched) = load(mode, &view, &[""], &bodies, &HashMap::new()).await;
			assert_eq!(
				fetched,
				vec![".filenignore", "src/.filenignore", "src/deep/.filenignore"],
				"{mode:?}"
			);
			assert!(remote_ignored(&rules, "cache/x"), "{mode:?}");
			assert!(remote_ignored(&rules, "src/a.o"), "{mode:?}");
			assert!(!remote_ignored(&rules, "src/deep/keep.o"), "{mode:?}");
			assert!(
				rules.blocked.is_empty() && rules.errors.is_empty(),
				"{mode:?}"
			);
			assert_eq!(rules.bodies.len(), 3, "{mode:?}");
		}
	}

	/// A two-way pair takes a directory's rules from the copy on disk when there is one (the scan
	/// reads it), and reads the remote copy only where there is none, under ignored directories too.
	#[tokio::test]
	async fn a_two_way_pair_reads_a_remote_rule_file_only_where_none_is_on_disk() {
		let view = rule_view(
			vec![
				rule_node(FILENIGNORE, 7),
				rule_node("cache/.filenignore", 3),
				rule_node("src/.filenignore", 4),
			],
			&[],
		);
		let bodies = HashMap::from([
			(FILENIGNORE, b"cache/\n".to_vec()),
			("cache/.filenignore", b"!x\n".to_vec()),
			("src/.filenignore", b"*.o\n".to_vec()),
		]);
		let (rules, mut fetched) =
			load(SyncMode::TwoWay, &view, &[""], &bodies, &HashMap::new()).await;
		fetched.sort();
		assert_eq!(fetched, vec!["cache/.filenignore", "src/.filenignore"]);
		assert!(
			!remote_ignored(&rules, "cache/y"),
			"the root rules are the scan's to read"
		);
		assert!(remote_ignored(&rules, "src/a.o"));

		let (rules, fetched) = load(SyncMode::TwoWay, &view, &[], &bodies, &HashMap::new()).await;
		assert_eq!(fetched.len(), 3, "{fetched:?}");
		assert!(remote_ignored(&rules, "cache/y"));
	}

	/// Rules that cannot be read block their directory for the pass, each with its own report line.
	/// A bad line only reports, and the rest of that file applies.
	#[tokio::test]
	async fn an_unusable_remote_rule_file_blocks_its_directory() {
		let too_large = usize::try_from(MAX_RULE_FILE_BYTES).unwrap() + 1;
		let view = rule_view(
			vec![
				rule_node("offline/.filenignore", 4),
				rule_node("huge/.filenignore", MAX_RULE_FILE_BYTES + 1),
				rule_node("binary/.filenignore", 1),
				rule_node("understated/.filenignore", 4),
				rule_node("fine/.filenignore", 6),
				rule_node("badline/.filenignore", 12),
			],
			&["twice/.filenignore"],
		);
		let bodies = HashMap::from([
			("huge/.filenignore", b"*\n".to_vec()),
			("binary/.filenignore", vec![0xff]),
			("understated/.filenignore", vec![b'#'; too_large]),
			("fine/.filenignore", b"*.tmp\n".to_vec()),
			("badline/.filenignore", b"[z-a]\n*.bak\n".to_vec()),
		]);
		let (rules, fetched) = load(
			SyncMode::RemoteToLocal,
			&view,
			&[],
			&bodies,
			&HashMap::new(),
		)
		.await;
		assert!(
			!fetched.iter().any(|path| path == "huge/.filenignore"),
			"an oversized file is not downloaded: {fetched:?}"
		);
		let blocked = ["binary", "huge", "offline", "twice", "understated"];
		assert_eq!(
			rules.blocked,
			blocked.iter().map(|dir| (*dir).to_owned()).collect()
		);
		assert_eq!(rules.errors.len(), 6, "{:?}", rules.errors);
		for dir in blocked.iter().chain(&["badline"]) {
			let prefix = format!("remote {dir}/.filenignore");
			assert!(
				rules.errors.iter().any(|line| line.starts_with(&prefix)),
				"{prefix}: {:?}",
				rules.errors
			);
		}
		assert!(remote_ignored(&rules, "fine/a.tmp"));
		assert!(remote_ignored(&rules, "badline/a.bak"));

		// On a two-way pair a copy on disk governs, so the held remote one blocks nothing.
		let held = rule_view(Vec::new(), &["twice/.filenignore"]);
		let (rules, _) = load(
			SyncMode::TwoWay,
			&held,
			&["twice"],
			&bodies,
			&HashMap::new(),
		)
		.await;
		assert!(rules.blocked.is_empty(), "{:?}", rules.blocked);
	}

	#[tokio::test]
	async fn a_cached_body_is_not_downloaded_again() {
		let root = rule_node(FILENIGNORE, 6);
		let sub = rule_node("sub/.filenignore", 6);
		let stale = Uuid::new_v4();
		let cached = HashMap::from([
			(root.remote_uuid, Arc::from("*.log\n")),
			(stale, Arc::from("*\n")),
		]);
		let view = rule_view(vec![root.clone(), sub.clone()], &[]);
		let bodies = HashMap::from([("sub/.filenignore", b"*.tmp\n".to_vec())]);
		let (rules, fetched) = load(SyncMode::RemoteToLocal, &view, &[], &bodies, &cached).await;
		assert_eq!(fetched, vec!["sub/.filenignore"]);
		assert!(remote_ignored(&rules, "a.log"));
		assert!(remote_ignored(&rules, "sub/a.tmp"));
		let mut kept: Vec<Uuid> = rules.bodies.keys().copied().collect();
		kept.sort();
		let mut expected = vec![root.remote_uuid, sub.remote_uuid];
		expected.sort();
		assert_eq!(kept, expected, "only the bodies this pass read are kept");
	}

	#[test]
	fn defaults_parse_cleanly() {
		let (_, errors) =
			IgnoreSource::parse(&DEFAULT_IGNORE_PATTERNS.join("\n"), Origin::Default).unwrap();
		assert!(errors.is_empty(), "{errors:?}");
	}
}
