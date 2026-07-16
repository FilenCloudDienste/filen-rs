use std::path::{Component, Path, PathBuf};

/// Returns `true` only if `name` is a single, non-traversing path component that
/// is safe to join onto a confined root: it must be a lone `Normal` component —
/// never empty, `.`, `..`, separator-bearing, absolute, or otherwise rooted.
///
/// The check goes through [`std::path::Component`] so "separator" and "absolute"
/// are interpreted exactly as the host filesystem these paths are materialized
/// on would interpret them.
fn is_safe_descendant(name: &str) -> bool {
	let mut components = Path::new(name).components();
	matches!(
		(components.next(), components.next()),
		(Some(Component::Normal(c)), None) if c == name
	)
}

#[derive(Clone)]
pub(crate) struct CanonicalPath(PathBuf);

impl CanonicalPath {
	pub(crate) fn new(path: &Path) -> Result<Self, std::io::Error> {
		if let (Some(parent), Some(name)) = (path.parent(), path.file_name()) {
			std::fs::canonicalize(parent)
				.map(|canonical_parent| CanonicalPath(canonical_parent.join(name)))
		} else {
			std::fs::canonicalize(path).map(CanonicalPath)
		}
	}
}

impl CanonicalPath {
	pub(crate) fn into_string(self) -> String {
		self.0
			.into_os_string()
			.into_string()
			.unwrap_or_else(|e| e.to_string_lossy().into_owned())
	}

	/// Joins `descendants` (server-supplied, decrypted remote names) onto this
	/// confined root, one component at a time.
	///
	/// Each descendant is validated with [`is_safe_descendant`] before being
	/// pushed: a name of `..`, `/etc/x`, or one containing a path separator
	/// would otherwise escape the root, because [`PathBuf::push`] treats an
	/// absolute component as *replacing* the whole path and `..` as a real
	/// parent hop. An unsafe component is rejected outright rather than
	/// sanitized, so no path outside the root can ever be produced.
	pub(crate) fn create_descendant_path<'a>(
		&'a self,
		descendants: impl Iterator<Item = &'a str> + Clone,
	) -> Result<CanonicalPath, std::io::Error> {
		let new_path_len = self.0.as_os_str().len()
			+ descendants
				.clone()
				.map(|s| s.len() + std::path::MAIN_SEPARATOR.len_utf8())
				.sum::<usize>();
		let mut new_path = PathBuf::with_capacity(new_path_len);
		new_path.push(&self.0);
		for descendant in descendants {
			if !is_safe_descendant(descendant) {
				return Err(std::io::Error::new(
					std::io::ErrorKind::InvalidInput,
					format!("refusing to build path with unsafe component {descendant:?}"),
				));
			}
			new_path.push(descendant);
		}
		Ok(CanonicalPath(new_path))
	}
}

impl AsRef<Path> for CanonicalPath {
	fn as_ref(&self) -> &Path {
		self.0.as_path()
	}
}

impl std::fmt::Debug for CanonicalPath {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{:?}", self.0)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn make(path: &str) -> CanonicalPath {
		CanonicalPath(PathBuf::from(path))
	}

	#[test]
	fn new_canonicalizes_existing_path() {
		let cp = CanonicalPath::new(Path::new(".")).unwrap();
		let as_path: &Path = cp.as_ref();
		assert!(as_path.is_absolute());
	}

	#[test]
	fn new_fails_on_nonexistent_path() {
		let result = CanonicalPath::new(Path::new("/nonexistent/path/that/does/not/exist"));
		assert!(result.is_err());
	}

	#[test]
	fn into_string_returns_path_string() {
		let cp = make("/some/test/path");
		assert_eq!(cp.into_string(), "/some/test/path");
	}

	#[test]
	fn create_descendant_path_single() {
		let cp = make("/root");
		let child = cp
			.create_descendant_path(["child"].iter().copied())
			.unwrap();
		assert_eq!(child.as_ref(), Path::new("/root/child"));
	}

	#[test]
	fn create_descendant_path_multiple() {
		let cp = make("/root");
		let deep = cp
			.create_descendant_path(["a", "b", "c"].iter().copied())
			.unwrap();
		assert_eq!(deep.as_ref(), Path::new("/root/a/b/c"));
	}

	#[test]
	fn create_descendant_path_empty_iterator() {
		let cp = make("/root");
		let same = cp.create_descendant_path(std::iter::empty()).unwrap();
		assert_eq!(same.as_ref(), Path::new("/root"));
	}

	#[test]
	fn create_descendant_path_rejects_parent_dir_traversal() {
		let cp = make("/root");
		// `PathBuf::push("..")` would produce `/root/..`, a real parent hop.
		let result = cp.create_descendant_path([".."].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_rejects_absolute_component() {
		let cp = make("/root");
		// `PathBuf::push("/etc/x")` would *replace* the whole path with `/etc/x`.
		let result = cp.create_descendant_path(["/etc/x"].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_rejects_embedded_separator() {
		let cp = make("/root");
		let result = cp.create_descendant_path(["a/b"].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_rejects_current_dir() {
		let cp = make("/root");
		let result = cp.create_descendant_path(["."].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_rejects_empty_component() {
		let cp = make("/root");
		let result = cp.create_descendant_path([""].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_rejects_unsafe_component_after_safe_ones() {
		let cp = make("/root");
		// A `..` reached only after descending through valid directories must
		// still be rejected, and nothing under `/root` must be produced.
		let result = cp.create_descendant_path(["a", "b", ".."].iter().copied());
		assert!(result.is_err());
	}

	#[test]
	fn create_descendant_path_accepts_names_with_dots_that_are_not_traversal() {
		let cp = make("/root");
		let ok = cp
			.create_descendant_path(["..evil", "file.txt", "...", "a..b"].iter().copied())
			.unwrap();
		assert_eq!(ok.as_ref(), Path::new("/root/..evil/file.txt/.../a..b"));
	}

	#[test]
	fn as_ref_returns_inner_path() {
		let cp = make("/foo/bar");
		let p: &Path = cp.as_ref();
		assert_eq!(p, Path::new("/foo/bar"));
	}

	#[test]
	fn debug_shows_inner_path() {
		let cp = make("/debug/test");
		let debug_str = format!("{:?}", cp);
		assert_eq!(debug_str, format!("{:?}", PathBuf::from("/debug/test")));
	}

	#[test]
	fn clone_produces_equal_path() {
		let cp = make("/clone/me");
		let cloned = cp.clone();
		assert_eq!(cloned.as_ref(), cp.as_ref());
	}

	#[test]
	fn create_descendant_path_very_long() {
		let base = make("/base");
		let components: Vec<&str> = (0..500).map(|_| "deeply_nested_dir").collect();
		let deep = base
			.create_descendant_path(components.iter().copied())
			.unwrap();

		let result: &Path = deep.as_ref();
		assert!(result.starts_with("/base"));
		// 500 components + base
		assert_eq!(result.components().count(), 502);
		assert_eq!(deep.into_string().matches("deeply_nested_dir").count(), 500);
	}
}
