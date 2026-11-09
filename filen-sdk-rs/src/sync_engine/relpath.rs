//! Splitting the `/`-joined, root-relative paths the engine keys everything by.
//!
//! A path with no `/` sits at the pair root: its parent is `""` and its leaf is the whole path.

/// The parent part of a relative path (`""` at the top level).
pub(super) fn parent_path(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// The last component of a relative path.
pub(super) fn leaf(rel_path: &str) -> &str {
	rel_path.rsplit_once('/').map_or(rel_path, |(_, name)| name)
}

/// A relative path's parent part and last component, as [`parent_path`] and [`leaf`] give them.
pub(super) fn split_parent(rel_path: &str) -> (&str, &str) {
	match rel_path.rsplit_once('/') {
		Some((parent, name)) => (parent, name),
		None => ("", rel_path),
	}
}

#[cfg(test)]
mod tests {
	use super::{leaf, parent_path, split_parent};

	#[test]
	fn a_top_level_path_has_an_empty_parent_and_is_its_own_leaf() {
		assert_eq!(parent_path("a.txt"), "");
		assert_eq!(leaf("a.txt"), "a.txt");
		assert_eq!(split_parent("a.txt"), ("", "a.txt"));
	}

	#[test]
	fn a_path_one_level_down_splits_at_its_slash() {
		assert_eq!(parent_path("dir/a.txt"), "dir");
		assert_eq!(leaf("dir/a.txt"), "a.txt");
		assert_eq!(split_parent("dir/a.txt"), ("dir", "a.txt"));
	}

	#[test]
	fn a_nested_path_splits_at_its_last_slash() {
		assert_eq!(parent_path("a/b/c.txt"), "a/b");
		assert_eq!(leaf("a/b/c.txt"), "c.txt");
		assert_eq!(split_parent("a/b/c.txt"), ("a/b", "c.txt"));
	}
}
