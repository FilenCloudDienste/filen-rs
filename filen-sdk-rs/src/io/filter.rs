//! Which items inside a transferred directory a directory transfer takes.

use std::{fmt, sync::Arc};

/// A set of path patterns a [`TransferFilter`] asks about each item.
pub trait PathMatcher: Send + Sync {
	/// Whether the item at `rel_path` matches: its path relative to the transferred directory,
	/// its components joined with `/`, with no leading or trailing `/`. `is_dir` tells a directory
	/// from a file.
	fn matches(&self, rel_path: &str, is_dir: bool) -> bool;
}

/// Which items inside a transferred directory a directory transfer takes. The default takes all
/// of them.
#[derive(Clone, Default)]
pub struct TransferFilter {
	/// When set, only the files it matches are taken, along with everything inside a directory it
	/// matches, and any other directory is only created on the way to a file that is taken.
	///
	/// `Arc`: the local scan runs on a blocking thread, which needs its own handle.
	pub include: Option<Arc<dyn PathMatcher>>,
	/// When set, the items it matches are left out, a directory with everything inside it, even
	/// where `include` matches them too.
	pub exclude: Option<Arc<dyn PathMatcher>>,
}

impl TransferFilter {
	/// Whether the filter takes every item.
	pub(crate) fn takes_all(&self) -> bool {
		self.include.is_none() && self.exclude.is_none()
	}

	pub(crate) fn excludes(&self, rel_path: &str, is_dir: bool) -> bool {
		self.exclude
			.as_ref()
			.is_some_and(|exclude| exclude.matches(rel_path, is_dir))
	}
}

impl fmt::Debug for TransferFilter {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("TransferFilter")
			.field("include", &self.include.is_some())
			.field("exclude", &self.exclude.is_some())
			.finish()
	}
}
