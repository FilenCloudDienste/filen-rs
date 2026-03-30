use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};

// ── Constants ──

pub const FILENIGNORE: &str = ".filenignore";
pub const LOCAL_FILENIGNORE: &str = ".local.filenignore";

pub const DEFAULT_PATTERNS: &[&str] = &[
	// Local-only ignore file (must never sync)
	LOCAL_FILENIGNORE,
	// OS metadata
	".DS_Store",
	"._*",
	"Thumbs.db",
	"desktop.ini",
	"ehthumbs.db",
	"ehthumbs_vista.db",
	"$RECYCLE.BIN/",
	".Spotlight-V100/",
	".Trashes/",
	".fseventsd/",
	".TemporaryItems/",
	// Temporary files
	"*.tmp",
	"*.temp",
	"~$*",
	// Editor swap files
	"*.swp",
	"*.swo",
	"*.swn",
	"*~",
	".#*",
	"#*#",
	// Partial downloads
	"*.crdownload",
	"*.part",
	"*.partial",
];

// ── IgnoreStack ──

/// Immutable set of ignore rules for a single sync.
///
/// All 5 layers (default, global user, sync-specific, local folder, folder)
/// are compiled into a single `Gitignore` via add-order precedence.
/// `Send + Sync` for concurrent use. The integration crate manages access
/// (e.g. behind `ArcSwap` or `RwLock`) if concurrent reads during rebuilds
/// are needed.
#[derive(Debug)]
pub struct IgnoreStack(Gitignore);

impl IgnoreStack {
	/// Creates an empty ignore stack that matches nothing.
	pub fn empty() -> Self {
		Self(Gitignore::empty())
	}

	/// Returns `true` if the path should be excluded from sync.
	pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
		self.0.matched_path_or_any_parents(path, is_dir).is_ignore()
	}
}

// ── Pattern Rewriting ──
//
// Works around <https://github.com/BurntSushi/ripgrep/issues/1909>:
// the `ignore` crate's `from` parameter in `add_line` is metadata-only.
// All patterns compile against the builder's root, breaking patterns from
// subdirectory ignore files. We fix this by rewriting patterns to include
// their relative directory prefix before adding them to the builder.

/// Rewrites a single gitignore pattern line so that patterns from a
/// subdirectory ignore file are correctly scoped to that subdirectory.
///
/// Returns `None` for comment/empty lines (should be skipped).
fn rewrite_line(reldir: &str, line: &str) -> Option<String> {
	// Match the ignore crate's whitespace handling
	let line = if !line.ends_with("\\ ") {
		line.trim_end()
	} else {
		line
	};

	if line.is_empty() || line.starts_with('#') {
		return None;
	}

	let mut rest = line;
	let mut negation = "";

	// Handle \! and \# escapes — the char after \ is literal.
	// After rewriting, it ends up mid-pattern where it's already literal,
	// so we just strip the backslash and proceed.
	if rest.starts_with("\\!") || rest.starts_with("\\#") {
		rest = &rest[1..];
	} else if rest.starts_with('!') {
		negation = "!";
		rest = &rest[1..];
	}

	let anchored = rest.starts_with('/');
	if anchored {
		rest = &rest[1..];
	}

	let dir_suffix = if rest.ends_with('/') { "/" } else { "" };
	let body = rest.strip_suffix('/').unwrap_or(rest);

	let has_slash = body.contains('/');

	// Anchored and slash-containing patterns are positional (no **/),
	// so just prefix with reldir. Simple filename patterns need reldir/**/
	// to match at any depth within the subtree.
	let rewritten = if anchored || has_slash {
		format!("{negation}{reldir}/{body}{dir_suffix}")
	} else {
		format!("{negation}{reldir}/**/{body}{dir_suffix}")
	};

	Some(rewritten)
}

/// Reads a `.filenignore` file and adds its patterns to the builder,
/// rewriting patterns from subdirectory files to be correctly scoped.
///
/// Missing files are silently skipped (returns `Ok`).
fn add_file(
	builder: &mut GitignoreBuilder,
	sync_root: &Path,
	file_path: &Path,
) -> Result<(), ignore::Error> {
	// Compute relative directory of the ignore file's parent vs sync root
	let reldir = file_path
		.parent()
		.and_then(|d| d.strip_prefix(sync_root).ok())
		.filter(|r| !r.as_os_str().is_empty() && *r != Path::new("."));

	// Root-level files need no rewriting — delegate directly
	let Some(reldir) = reldir else {
		if let Some(e) = builder.add(file_path) {
			return Err(e);
		}
		return Ok(());
	};

	let reldir_str: String = reldir
		.components()
		.map(|c| c.as_os_str().to_string_lossy())
		.collect::<Vec<_>>()
		.join("/");

	let file = match std::fs::File::open(file_path) {
		Ok(f) => f,
		Err(_) => return Ok(()), // missing file is not an error
	};

	let reader = BufReader::new(file);
	for (i, line_result) in reader.lines().enumerate() {
		let line = match line_result {
			Ok(l) => l,
			Err(e) => return Err(ignore::Error::Io(e)),
		};

		// Handle BOM on first line
		const UTF8_BOM: &str = "\u{feff}";
		let line = if i == 0 {
			line.trim_start_matches(UTF8_BOM).to_owned()
		} else {
			line
		};

		let rewritten = match rewrite_line(&reldir_str, &line) {
			Some(r) => r,
			None => continue,
		};

		builder.add_line(Some(file_path.into()), &rewritten)?;
	}

	Ok(())
}

// ── IgnoreStackBuilder ──

/// Tracks all ignore sources for a single sync. Kept around by the
/// integration crate for hot-reload: modify sources, call `build()` again.
pub struct IgnoreStackBuilder {
	sync_root: PathBuf,
	default_patterns: Vec<String>,
	global_user_file: Option<PathBuf>,
	sync_specific_file: Option<PathBuf>,
	local_folder_files: Vec<PathBuf>,
	folder_files: Vec<PathBuf>,
}

impl IgnoreStackBuilder {
	/// Creates a new builder rooted at the given sync directory.
	/// Includes `DEFAULT_PATTERNS` automatically.
	pub fn new(sync_root: impl Into<PathBuf>) -> Self {
		Self {
			sync_root: sync_root.into(),
			default_patterns: DEFAULT_PATTERNS.iter().map(|s| (*s).to_owned()).collect(),
			global_user_file: None,
			sync_specific_file: None,
			local_folder_files: Vec::new(),
			folder_files: Vec::new(),
		}
	}

	pub fn set_global_user_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
		self.global_user_file = Some(path.into());
		self
	}

	pub fn set_sync_specific_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
		self.sync_specific_file = Some(path.into());
		self
	}

	pub fn add_folder_ignore_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
		self.folder_files.push(path.into());
		self
	}

	pub fn add_local_folder_ignore_file(&mut self, path: impl Into<PathBuf>) -> &mut Self {
		self.local_folder_files.push(path.into());
		self
	}

	pub fn remove_folder_ignore_file(&mut self, path: &Path) -> bool {
		let before = self.folder_files.len();
		self.folder_files.retain(|p| p != path);
		self.folder_files.len() < before
	}

	pub fn remove_local_folder_ignore_file(&mut self, path: &Path) -> bool {
		let before = self.local_folder_files.len();
		self.local_folder_files.retain(|p| p != path);
		self.local_folder_files.len() < before
	}

	pub fn clear_folder_ignore_files(&mut self) -> &mut Self {
		self.folder_files.clear();
		self
	}

	pub fn clear_local_folder_ignore_files(&mut self) -> &mut Self {
		self.local_folder_files.clear();
		self
	}

	pub fn build(&mut self) -> Result<IgnoreStack, ignore::Error> {
		let mut builder = GitignoreBuilder::new(&self.sync_root);

		// Layer 5 (lowest priority): defaults
		for pattern in &self.default_patterns {
			builder.add_line(None, pattern)?;
		}

		// Layer 4: global user file (root-level, no rewriting)
		if let Some(ref path) = self.global_user_file
			&& let Some(e) = builder.add(path)
		{
			return Err(e);
		}

		// Layer 3: sync-specific file (root-level, no rewriting)
		if let Some(ref path) = self.sync_specific_file
			&& let Some(e) = builder.add(path)
		{
			return Err(e);
		}

		// Layer 2: local folder files (sorted shallowest to deepest)
		self.local_folder_files
			.sort_by_key(|p| p.components().count());
		for path in &self.local_folder_files {
			add_file(&mut builder, &self.sync_root, path)?;
		}

		// Layer 1 (highest priority): folder .filenignore files
		self.folder_files.sort_by_key(|p| p.components().count());
		for path in &self.folder_files {
			add_file(&mut builder, &self.sync_root, path)?;
		}

		let gitignore = builder.build()?;
		Ok(IgnoreStack(gitignore))
	}
}

// ── Utility functions ──

/// Returns `true` if the file name is a `.filenignore` or `.local.filenignore`.
pub fn is_ignore_file(file_name: &str) -> bool {
	file_name == FILENIGNORE || file_name == LOCAL_FILENIGNORE
}

/// Returns `true` if the file name is a `.local.filenignore` (local-only, never synced).
pub fn is_local_only_ignore_file(file_name: &str) -> bool {
	file_name == LOCAL_FILENIGNORE
}

// ── Compile-time assertions ──

const _: () = {
	fn _assert_send_sync<T: Send + Sync>() {}
	fn _check() {
		_assert_send_sync::<IgnoreStack>();
	}
};

// ── Tests ──

#[cfg(test)]
mod tests {
	use super::*;

	// ── Pattern rewriting tests ──
	#[test]
	fn rewrite_anchored() {
		assert_eq!(rewrite_line("src", "/build"), Some("src/build".into()));
	}

	#[test]
	fn rewrite_slash_pattern() {
		assert_eq!(rewrite_line("src", "foo/bar"), Some("src/foo/bar".into()));
	}

	#[test]
	fn rewrite_simple() {
		assert_eq!(rewrite_line("src", "*.o"), Some("src/**/*.o".into()));
	}

	#[test]
	fn rewrite_dir_only() {
		assert_eq!(rewrite_line("b", "c/"), Some("b/**/c/".into()));
	}

	#[test]
	fn rewrite_negation() {
		assert_eq!(rewrite_line("src", "!*.o"), Some("!src/**/*.o".into()));
	}

	#[test]
	fn rewrite_negation_anchored() {
		assert_eq!(rewrite_line("src", "!/build"), Some("!src/build".into()));
	}

	#[test]
	fn rewrite_comment_skipped() {
		assert_eq!(rewrite_line("src", "# comment"), None);
	}

	#[test]
	fn rewrite_empty_skipped() {
		assert_eq!(rewrite_line("src", ""), None);
	}

	#[test]
	fn rewrite_escaped_bang() {
		assert_eq!(rewrite_line("src", "\\!foo"), Some("src/**/!foo".into()));
	}

	#[test]
	fn rewrite_escaped_hash() {
		assert_eq!(rewrite_line("src", "\\#foo"), Some("src/**/#foo".into()));
	}

	#[test]
	fn rewrite_doublestar() {
		assert_eq!(rewrite_line("src", "**/*.o"), Some("src/**/*.o".into()));
	}

	#[test]
	fn rewrite_deeply_nested() {
		assert_eq!(rewrite_line("a/b/c", "*.o"), Some("a/b/c/**/*.o".into()));
	}

	// ── IgnoreStack tests ──

	#[test]
	fn empty_stack_matches_nothing() {
		let stack = IgnoreStack::empty();
		assert!(!stack.is_ignored(Path::new("foo.txt"), false));
		assert!(!stack.is_ignored(Path::new("dir"), true));
	}

	#[test]
	fn default_patterns() {
		let mut builder = IgnoreStackBuilder::new("/root");
		let stack = builder.build().unwrap();

		assert!(stack.is_ignored(Path::new("/root/.DS_Store"), false));
		assert!(stack.is_ignored(Path::new("/root/foo.tmp"), false));
		assert!(stack.is_ignored(Path::new("/root/sub/bar.swp"), false));
		assert!(stack.is_ignored(Path::new("/root/.local.filenignore"), false));
		assert!(stack.is_ignored(Path::new("/root/Thumbs.db"), false));
	}

	#[test]
	fn default_does_not_match_filenignore() {
		let mut builder = IgnoreStackBuilder::new("/root");
		let stack = builder.build().unwrap();
		assert!(!stack.is_ignored(Path::new("/root/.filenignore"), false));
	}

	#[test]
	fn is_ignored_convenience() {
		let mut builder = GitignoreBuilder::new("/root");
		builder.add_line(None, "*.tmp").unwrap();
		let gi = builder.build().unwrap();
		let stack = IgnoreStack(gi);

		assert!(stack.is_ignored(Path::new("/root/foo.tmp"), false));
		assert!(!stack.is_ignored(Path::new("/root/foo.rs"), false));
	}

	// ── Utility function tests ──

	#[test]
	fn test_is_ignore_file() {
		assert!(is_ignore_file(".filenignore"));
		assert!(is_ignore_file(".local.filenignore"));
		assert!(!is_ignore_file(".gitignore"));
		assert!(!is_ignore_file("filenignore"));
	}

	#[test]
	fn test_is_local_only_ignore_file() {
		assert!(is_local_only_ignore_file(".local.filenignore"));
		assert!(!is_local_only_ignore_file(".filenignore"));
	}

	// ── End-to-end matching tests ──
	//
	// These test the full rewrite→compile→match pipeline by feeding
	// rewritten patterns into a GitignoreBuilder directly. No filesystem
	// access needed.

	/// Helper: build an IgnoreStack from raw add_line calls.
	fn stack_from_lines(root: &str, lines: &[&str]) -> IgnoreStack {
		let mut b = GitignoreBuilder::new(root);
		for line in lines {
			b.add_line(None, line).unwrap();
		}
		IgnoreStack(b.build().unwrap())
	}

	/// Helper: build an IgnoreStack with defaults + rewritten subdirectory patterns.
	fn stack_with_rewritten(root: &str, reldir: &str, patterns: &[&str]) -> IgnoreStack {
		let mut b = GitignoreBuilder::new(root);
		for p in DEFAULT_PATTERNS {
			b.add_line(None, p).unwrap();
		}
		for p in patterns {
			if let Some(rewritten) = rewrite_line(reldir, p) {
				b.add_line(None, &rewritten).unwrap();
			}
		}
		IgnoreStack(b.build().unwrap())
	}

	#[test]
	fn missing_file_skipped() {
		let mut builder = IgnoreStackBuilder::new("/nonexistent/root");
		builder.add_folder_ignore_file("/nonexistent/root/sub/.filenignore");
		let stack = builder.build().unwrap();
		assert!(!stack.is_ignored(Path::new("/nonexistent/root/anything"), false));
	}

	#[test]
	fn priority_last_added_wins() {
		// Simulate: global ignores *.log, sync-specific whitelists debug.log
		let stack = stack_from_lines("/root", &["*.log", "!debug.log"]);
		assert!(!stack.is_ignored(Path::new("/root/debug.log"), false));
		assert!(stack.is_ignored(Path::new("/root/error.log"), false));
	}

	#[test]
	fn folder_depth_ordering() {
		// Simulate: root ignores *.log, src/ whitelists *.log (deeper = later = wins)
		let stack = stack_from_lines(
			"/root",
			&[
				"*.log",
				// rewritten !*.log from src/.filenignore:
				"!src/**/*.log",
			],
		);
		assert!(!stack.is_ignored(Path::new("/root/src/debug.log"), false));
		assert!(stack.is_ignored(Path::new("/root/other/debug.log"), false));
	}

	#[test]
	fn subdirectory_anchored_correct() {
		let stack = stack_with_rewritten("/root", "src", &["/build"]);
		assert!(stack.is_ignored(Path::new("/root/src/build"), true));
		assert!(!stack.is_ignored(Path::new("/root/build"), true));
	}

	#[test]
	fn subdirectory_simple_scoped() {
		let stack = stack_with_rewritten("/root", "src", &["*.o"]);
		assert!(stack.is_ignored(Path::new("/root/src/foo.o"), false));
		assert!(stack.is_ignored(Path::new("/root/src/sub/bar.o"), false));
		assert!(!stack.is_ignored(Path::new("/root/other/foo.o"), false));
	}

	#[test]
	fn subdirectory_dir_recursive() {
		let stack = stack_with_rewritten("/root", "a/b", &["c/"]);
		assert!(stack.is_ignored(Path::new("/root/a/b/c"), true));
		assert!(stack.is_ignored(Path::new("/root/a/b/d/c"), true));
		assert!(stack.is_ignored(Path::new("/root/a/b/d/e/c"), true));
		assert!(!stack.is_ignored(Path::new("/root/a/c"), true));
		// c/ should not match files
		assert!(!stack.is_ignored(Path::new("/root/a/b/c"), false));
	}

	#[test]
	fn root_patterns_work_normally() {
		let stack = stack_from_lines("/root", &["/build", "*.o"]);
		assert!(stack.is_ignored(Path::new("/root/build"), true));
		assert!(stack.is_ignored(Path::new("/root/foo.o"), false));
		assert!(stack.is_ignored(Path::new("/root/sub/bar.o"), false));
	}

	#[test]
	fn remove_folder_file() {
		let mut builder = IgnoreStackBuilder::new("/root");
		let path = Path::new("/root/sub/.filenignore");
		builder.add_folder_ignore_file(path);
		assert!(builder.remove_folder_ignore_file(path));
		assert!(!builder.remove_folder_ignore_file(path)); // already removed
	}

	#[test]
	fn subdirectory_negation_scoped() {
		// Root ignores *.log, src/ whitelists *.log
		let stack = stack_from_lines(
			"/root",
			&[
				"*.log",
				// rewritten !*.log from src/.filenignore:
				"!src/**/*.log",
			],
		);
		assert!(!stack.is_ignored(Path::new("/root/src/debug.log"), false));
		assert!(stack.is_ignored(Path::new("/root/other/debug.log"), false));
	}
}
