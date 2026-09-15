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
//! [`IgnoreRules::is_ignored_with_ancestors`].

use std::{
	collections::HashMap,
	fmt,
	path::{Path, PathBuf},
	sync::LazyLock,
};

use ignore::{
	Match,
	gitignore::{Gitignore, GitignoreBuilder, Glob},
};
use unicode_normalization::UnicodeNormalization;

use super::scan::collision_key;

/// The name of a per-directory rule file.
pub(crate) const FILENIGNORE: &str = ".filenignore";

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

/// Why a path is ignored: the deciding line and its source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IgnoreHit<'a> {
	pub(crate) origin: Origin<'a>,
	/// The line as written (trimmed, NFC).
	pub(crate) pattern: &'a str,
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

	/// Whether `rel_path` or any directory above it is ignored (git's rule: nothing under an
	/// ignored directory can be re-included). `memo` caches per-directory answers across calls on
	/// the same rules.
	pub(crate) fn is_ignored_with_ancestors(
		&self,
		rel_path: &str,
		is_dir: bool,
		memo: &mut HashMap<String, bool>,
	) -> bool {
		for (i, _) in rel_path.match_indices('/') {
			let ancestor = &rel_path[..i];
			let ignored = match memo.get(ancestor) {
				Some(&ignored) => ignored,
				None => {
					let ignored = self.decide(ancestor, true).is_some();
					memo.insert(ancestor.to_owned(), ignored);
					ignored
				}
			};
			if ignored {
				return true;
			}
		}
		self.decide(rel_path, is_dir).is_some()
	}
}

/// The matched line before case folding (`Glob::original` is the folded one).
fn as_written(glob: &Glob) -> &str {
	glob.from()
		.and_then(Path::to_str)
		.map_or(glob.original(), str::trim_end)
}

fn parent(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

#[cfg(test)]
mod tests {
	use super::*;

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
		rules.is_ignored_with_ancestors(rel_path, is_dir, &mut HashMap::new())
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

	#[test]
	fn memo_serves_later_paths_under_an_ignored_directory() {
		let rules = root_rules("build/");
		let mut memo = HashMap::new();
		assert!(rules.is_ignored_with_ancestors("build/a/b", F, &mut memo));
		assert_eq!(memo.get("build"), Some(&true));
		assert!(rules.is_ignored_with_ancestors("build/c", F, &mut memo));
		assert!(!rules.is_ignored_with_ancestors("src/c", F, &mut memo));
		assert_eq!(memo.get("src"), Some(&false));
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

	#[test]
	fn defaults_parse_cleanly() {
		let (_, errors) =
			IgnoreSource::parse(&DEFAULT_IGNORE_PATTERNS.join("\n"), Origin::Default).unwrap();
		assert!(errors.is_empty(), "{errors:?}");
	}
}
