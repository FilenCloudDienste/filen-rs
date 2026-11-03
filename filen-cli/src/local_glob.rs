//! Wildcards in local paths, expanded the way a Unix shell expands them before the CLI sees its
//! arguments. cmd.exe and PowerShell pass `*.jpg` through unexpanded, so on Windows the CLI expands
//! the local paths it uploads itself.
//!
//! As in a POSIX shell: each path component is matched on its own, so `*` never matches across a
//! separator; `*`, `?` and `[...]` (`[!...]` negated) are the wildcards; a wildcard does not match
//! a leading `.` unless the component starts with one; the matches come sorted; and a path that
//! matches nothing, or is not a valid pattern, is kept as it was. Unlike in a shell, a path that
//! names an existing item is kept as it is too, since a backslash escapes nothing, being the
//! separator on Windows, and quotes reach the program: `[[]` matches a literal `[`.

use std::{
	ffi::OsString,
	fs, io,
	path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result};
use globset::{GlobBuilder, GlobMatcher};

/// Whether matching tells upper from lower case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Case {
	Sensitive,
	Insensitive,
}

impl Case {
	/// How the platform's file names compare: Windows file systems ignore case.
	pub(crate) fn of_platform() -> Self {
		if cfg!(windows) {
			Self::Insensitive
		} else {
			Self::Sensitive
		}
	}
}

/// Every path in `paths` with its wildcards expanded, in order.
pub(crate) fn expand_all(paths: Vec<String>, case: Case) -> Result<Vec<String>> {
	let mut expanded = Vec::with_capacity(paths.len());
	for path in paths {
		expanded.extend(expand(path, case)?);
	}
	Ok(expanded)
}

/// The existing paths `pattern` matches, sorted, or `pattern` itself when it has no wildcard,
/// names an existing item as it is, or matches nothing.
pub(crate) fn expand(pattern: String, case: Case) -> Result<Vec<String>> {
	// A `[` is valid in a file name, and on Windows nothing can escape it, so an item named as
	// given wins over what the pattern would match
	if !has_wildcard(&pattern) || fs::symlink_metadata(&pattern).is_ok() {
		return Ok(vec![pattern]);
	}
	let mut candidates = vec![PathBuf::new()];
	for component in Path::new(&pattern).components() {
		let matcher = match component {
			Component::Normal(name) => name
				.to_str()
				.and_then(|name| ComponentMatcher::new(name, case)),
			_ => None,
		};
		candidates = match matcher {
			Some(matcher) => {
				let mut matched = Vec::new();
				for candidate in &candidates {
					matched.extend(matcher.entries_of(candidate)?);
				}
				matched
			}
			None => candidates
				.into_iter()
				.map(|candidate| candidate.join(component))
				.filter(|candidate| fs::symlink_metadata(candidate).is_ok())
				.collect(),
		};
		if candidates.is_empty() {
			return Ok(vec![pattern]);
		}
	}
	candidates
		.into_iter()
		.map(|path| {
			path.into_os_string().into_string().map_err(|path| {
				anyhow::anyhow!("{path:?} matches {pattern:?} but is not valid UTF-8")
			})
		})
		.collect()
}

fn has_wildcard(component: &str) -> bool {
	component.contains(['*', '?', '['])
}

/// A path component holding a wildcard.
struct ComponentMatcher {
	matcher: GlobMatcher,
	/// Whether names that start with `.` can match.
	matches_hidden: bool,
}

impl ComponentMatcher {
	/// The matcher for `component`, or `None` when it holds no wildcard or is not a valid pattern
	/// (such as an unclosed `[`), which a shell takes literally.
	fn new(component: &str, case: Case) -> Option<Self> {
		if !has_wildcard(component) {
			return None;
		}
		// `{a,b}` is not a wildcard in a POSIX shell's matching, nor in cmd.exe; `[{]` keeps
		// globset from reading one
		let escaped = component.replace('{', "[{]").replace('}', "[}]");
		let glob = GlobBuilder::new(&escaped)
			.literal_separator(true)
			.backslash_escape(false)
			.case_insensitive(case == Case::Insensitive)
			.build()
			.ok()?;
		Some(Self {
			matcher: glob.compile_matcher(),
			matches_hidden: component.starts_with('.'),
		})
	}

	/// The entries of the directory `dir` (the current directory when empty) whose names match,
	/// sorted. A `dir` that is not a directory has none.
	fn entries_of(&self, dir: &Path) -> Result<Vec<PathBuf>> {
		let listed = if dir.as_os_str().is_empty() {
			Path::new(".")
		} else {
			dir
		};
		if !fs::metadata(listed).is_ok_and(|metadata| metadata.is_dir()) {
			return Ok(Vec::new());
		}
		let mut names = fs::read_dir(listed)
			.and_then(|entries| {
				entries
					.map(|entry| entry.map(|entry| entry.file_name()))
					.collect::<io::Result<Vec<OsString>>>()
			})
			.with_context(|| format!("Failed to list {}", listed.display()))?;
		names.retain(|name| {
			(self.matches_hidden || !name.as_encoded_bytes().starts_with(b"."))
				&& self.matcher.is_match(Path::new(name))
		});
		names.sort();
		Ok(names.into_iter().map(|name| dir.join(name)).collect())
	}
}

#[cfg(test)]
mod tests {
	use std::path::MAIN_SEPARATOR_STR as SEP;

	use tempfile::TempDir;

	use super::*;

	/// A directory holding `files` (paths with `/`), empty files created with their parents.
	fn tree(files: &[&str]) -> TempDir {
		let dir = tempfile::tempdir().unwrap();
		for file in files {
			let path = dir.path().join(file);
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, b"").unwrap();
		}
		dir
	}

	/// What `pattern`, below `dir`, expands to, relative to `dir` and with `/` separators.
	fn expanded(dir: &TempDir, pattern: &str, case: Case) -> Vec<String> {
		let root = format!("{}{SEP}", dir.path().to_str().unwrap());
		let pattern = format!("{root}{}", pattern.replace('/', SEP));
		expand(pattern, case)
			.unwrap()
			.into_iter()
			.map(|path| path.strip_prefix(&root).unwrap().replace(SEP, "/"))
			.collect()
	}

	#[test]
	fn a_path_without_a_wildcard_is_kept_even_when_missing() {
		assert_eq!(
			expand("missing/a.jpg".to_owned(), Case::Sensitive).unwrap(),
			["missing/a.jpg"]
		);
	}

	#[test]
	fn a_star_matches_the_files_of_a_directory_sorted() {
		let dir = tree(&["b.jpg", "a.jpg", "c.png", "sub/d.jpg"]);
		assert_eq!(expanded(&dir, "*.jpg", Case::Sensitive), ["a.jpg", "b.jpg"]);
	}

	#[test]
	fn a_question_mark_and_a_class_match_one_character() {
		let dir = tree(&["a1", "a2", "a3", "a10"]);
		assert_eq!(expanded(&dir, "a?", Case::Sensitive), ["a1", "a2", "a3"]);
		assert_eq!(expanded(&dir, "a[13]", Case::Sensitive), ["a1", "a3"]);
		assert_eq!(expanded(&dir, "a[!13]", Case::Sensitive), ["a2"]);
	}

	#[test]
	fn a_star_does_not_match_a_hidden_name() {
		let dir = tree(&[".hidden.jpg", "shown.jpg"]);
		assert_eq!(expanded(&dir, "*.jpg", Case::Sensitive), ["shown.jpg"]);
		assert_eq!(expanded(&dir, ".*.jpg", Case::Sensitive), [".hidden.jpg"]);
	}

	#[test]
	fn a_star_does_not_cross_a_separator() {
		let dir = tree(&["x/a.jpg", "x/y/b.jpg"]);
		assert_eq!(expanded(&dir, "*.jpg", Case::Sensitive), ["*.jpg"]);
		assert_eq!(expanded(&dir, "x/*.jpg", Case::Sensitive), ["x/a.jpg"]);
	}

	#[test]
	fn a_wildcard_directory_is_matched_component_by_component() {
		let dir = tree(&["a/1.txt", "b/1.txt", "b/2.txt", "c/2.txt", "file"]);
		assert_eq!(
			expanded(&dir, "*/1.txt", Case::Sensitive),
			["a/1.txt", "b/1.txt"]
		);
		assert_eq!(
			expanded(&dir, "*/*.txt", Case::Sensitive),
			["a/1.txt", "b/1.txt", "b/2.txt", "c/2.txt"]
		);
	}

	#[test]
	fn a_pattern_matching_nothing_is_kept() {
		let dir = tree(&["a.png"]);
		assert_eq!(expanded(&dir, "*.jpg", Case::Sensitive), ["*.jpg"]);
	}

	#[test]
	fn an_invalid_pattern_is_taken_literally() {
		let dir = tree(&["a[b"]);
		assert_eq!(expanded(&dir, "a[b", Case::Sensitive), ["a[b"]);
	}

	#[test]
	fn a_class_matches_a_wildcard_character_literally() {
		// `*` and `?` cannot be in a file name on Windows, `[` can
		let dir = tree(&["a[b", "axb"]);
		assert_eq!(expanded(&dir, "a[[]b", Case::Sensitive), ["a[b"]);
	}

	#[test]
	fn an_existing_path_is_kept_rather_than_matched() {
		let dir = tree(&["a[b]", "ab"]);
		assert_eq!(expanded(&dir, "a[b]", Case::Sensitive), ["a[b]"]);
	}

	#[test]
	fn a_pattern_naming_no_existing_path_is_matched() {
		let dir = tree(&["ab"]);
		assert_eq!(expanded(&dir, "a[b]", Case::Sensitive), ["ab"]);
	}

	#[test]
	fn braces_are_not_alternatives() {
		let dir = tree(&["a", "b", "{a,b}x"]);
		assert_eq!(expanded(&dir, "{a,b}*", Case::Sensitive), ["{a,b}x"]);
	}

	#[test]
	fn case_insensitive_matching_ignores_case() {
		let dir = tree(&["a.JPG", "b.jpg"]);
		assert_eq!(expanded(&dir, "*.jpg", Case::Sensitive), ["b.jpg"]);
		assert_eq!(
			expanded(&dir, "*.jpg", Case::Insensitive),
			["a.JPG", "b.jpg"]
		);
	}

	#[test]
	fn every_path_is_expanded_in_order() {
		let dir = tree(&["a.jpg", "b.png", "c.jpg"]);
		let root = dir.path().to_str().unwrap();
		let expanded = expand_all(
			vec![
				format!("{root}{SEP}*.png"),
				"-".to_owned(),
				format!("{root}{SEP}*.jpg"),
			],
			Case::Sensitive,
		)
		.unwrap();
		assert_eq!(
			expanded,
			[
				format!("{root}{SEP}b.png"),
				"-".to_owned(),
				format!("{root}{SEP}a.jpg"),
				format!("{root}{SEP}c.jpg"),
			]
		);
	}
}
