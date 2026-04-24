use std::path::{Path, PathBuf};

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

	pub(crate) fn create_descendant_path<'a>(
		&'a self,
		descendants: impl Iterator<Item = &'a str> + Clone,
	) -> CanonicalPath {
		let new_path_len = self.0.as_os_str().len()
			+ descendants
				.clone()
				.map(|s| s.len() + std::path::MAIN_SEPARATOR.len_utf8())
				.sum::<usize>();
		let mut new_path = PathBuf::with_capacity(new_path_len);
		new_path.push(&self.0);
		for descendant in descendants {
			new_path.push(descendant);
		}
		CanonicalPath(new_path)
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
		let child = cp.create_descendant_path(["child"].iter().copied());
		assert_eq!(child.as_ref(), Path::new("/root/child"));
	}

	#[test]
	fn create_descendant_path_multiple() {
		let cp = make("/root");
		let deep = cp.create_descendant_path(["a", "b", "c"].iter().copied());
		assert_eq!(deep.as_ref(), Path::new("/root/a/b/c"));
	}

	#[test]
	fn create_descendant_path_empty_iterator() {
		let cp = make("/root");
		let same = cp.create_descendant_path(std::iter::empty());
		assert_eq!(same.as_ref(), Path::new("/root"));
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
		let deep = base.create_descendant_path(components.iter().copied());

		let result: &Path = deep.as_ref();
		assert!(result.starts_with("/base"));
		// 500 components + base
		assert_eq!(result.components().count(), 502);
		assert_eq!(deep.into_string().matches("deeply_nested_dir").count(), 500);
	}
}
