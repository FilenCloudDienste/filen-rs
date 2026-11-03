use std::collections::VecDeque;

use crate::io::{
	filter::TransferFilter,
	fs_tree::{WalkError, entry::DFSWalkerEntry},
};

/// A directory on the path to the entry being looked at.
struct Ancestor<E> {
	name: String,
	/// The directory's own entry while it is held back: under an include filter, a directory is
	/// only passed on once something inside it is.
	held: Option<E>,
	/// Whether the include filter matched it or a directory above it, which takes everything
	/// inside it that the exclude filter does not leave out.
	included: bool,
}

/// The entries of a depth-first walk below a transferred directory (depth 1 for its children)
/// that `filter` takes, still depth-first and each directory before what is inside it.
///
/// An excluded directory is dropped with everything below it. Under an include filter a file is
/// taken when the filter matches it or a directory above it, and a directory when the filter
/// matches it or once something inside it is taken; a directory nothing is taken from is dropped.
/// An entry whose name cannot be read is passed on, with the directories above it, for the tree
/// builder to report.
pub(super) struct FilteredWalk<'f, I, E> {
	inner: I,
	filter: &'f TransferFilter,
	/// The directories above the entry looked at last, shallowest first.
	ancestors: Vec<Ancestor<E>>,
	/// The depth of an excluded directory whose descendants are still being skipped.
	skipping_below: Option<usize>,
	/// Entries taken and not yet handed out: a directory held back, then the entry inside it that
	/// let it through.
	ready: VecDeque<Result<E, WalkError>>,
}

impl<'f, I, E> FilteredWalk<'f, I, E>
where
	I: Iterator<Item = Result<E, WalkError>>,
	E: DFSWalkerEntry,
{
	pub(super) fn new(inner: I, filter: &'f TransferFilter) -> Self {
		Self {
			inner,
			filter,
			ancestors: Vec::new(),
			skipping_below: None,
			ready: VecDeque::new(),
		}
	}

	/// Hands out the directories held back above an entry that is taken, then the entry.
	fn take(&mut self, entry: E) {
		for ancestor in &mut self.ancestors {
			if let Some(held) = ancestor.held.take() {
				self.ready.push_back(Ok(held));
			}
		}
		self.ready.push_back(Ok(entry));
	}

	/// Looks at the next entry of the walk, putting what it takes into `ready`.
	fn look_at(&mut self, entry: E) {
		let depth = entry.depth();
		if let Some(excluded) = self.skipping_below {
			if depth > excluded {
				return;
			}
			self.skipping_below = None;
		}
		// The directories at `depth` and below hold nothing more of the walk; one still held back
		// had nothing taken from it and is dropped.
		self.ancestors.truncate(depth.saturating_sub(1));
		let is_dir = entry.is_dir();
		let Ok(name) = entry.name().map(str::to_owned) else {
			if is_dir {
				self.ancestors.push(Ancestor {
					name: String::new(),
					held: None,
					included: true,
				});
			}
			self.take(entry);
			return;
		};
		let rel_path = self
			.ancestors
			.iter()
			.map(|ancestor| ancestor.name.as_str())
			.chain(std::iter::once(name.as_str()))
			.collect::<Vec<_>>()
			.join("/");
		if self.filter.excludes(&rel_path, is_dir) {
			if is_dir {
				self.skipping_below = Some(depth);
			}
			return;
		}
		let included = match &self.filter.include {
			None => true,
			Some(include) => {
				self.ancestors.last().is_some_and(|parent| parent.included)
					|| include.matches(&rel_path, is_dir)
			}
		};
		match (is_dir, included) {
			(true, true) => {
				self.take(entry);
				self.ancestors.push(Ancestor {
					name,
					held: None,
					included: true,
				});
			}
			(true, false) => self.ancestors.push(Ancestor {
				name,
				held: Some(entry),
				included: false,
			}),
			(false, true) => self.take(entry),
			(false, false) => {}
		}
	}
}

impl<I, E> Iterator for FilteredWalk<'_, I, E>
where
	I: Iterator<Item = Result<E, WalkError>>,
	E: DFSWalkerEntry,
{
	type Item = Result<E, WalkError>;

	fn next(&mut self) -> Option<Self::Item> {
		loop {
			if let Some(item) = self.ready.pop_front() {
				return Some(item);
			}
			match self.inner.next()? {
				Ok(entry) => self.look_at(entry),
				Err(e) => return Some(Err(e)),
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use std::sync::Arc;

	use super::FilteredWalk;
	use crate::io::{
		filter::{PathMatcher, TransferFilter},
		fs_tree::{
			WalkError,
			entry::{DFSWalkerEntry, SecondPassDirEntry, SecondPassEntry, SecondPassFileEntry},
		},
	};

	/// Matches the paths it lists, a directory's with a trailing `/`.
	struct Paths(&'static [&'static str]);

	impl PathMatcher for Paths {
		fn matches(&self, rel_path: &str, is_dir: bool) -> bool {
			let path = if is_dir {
				format!("{rel_path}/")
			} else {
				rel_path.to_owned()
			};
			self.0.contains(&path.as_str())
		}
	}

	/// The walk of a tree written one entry a line, indented two spaces a level, a directory's name
	/// ending in `/`.
	fn walk(tree: &'static str) -> Vec<Result<SecondPassEntry<'static, (), ()>, WalkError>> {
		tree.lines()
			.filter(|line| !line.trim().is_empty())
			.map(|line| {
				let name = line.trim_start();
				let depth = (line.len() - name.len()) / 2 + 1;
				Ok(match name.strip_suffix('/') {
					Some(dir) => SecondPassEntry::dir(SecondPassDirEntry::new(dir, ()), depth),
					None => SecondPassEntry::file(SecondPassFileEntry::new(name, (), 1), depth),
				})
			})
			.collect()
	}

	const TREE: &str = "
docs/
  a.txt
  b.jpg
  drafts/
    c.jpg
empty/
photos/
  d.jpg
  raw/
    e.cr2
top.jpg
";

	/// What `filter` takes of [`TREE`], as written there but unindented, each prefixed with its
	/// depth.
	fn taken(filter: &TransferFilter) -> Vec<String> {
		FilteredWalk::new(walk(TREE).into_iter(), filter)
			.map(|entry| {
				let entry = entry.expect("the test walk has no errors");
				let suffix = if entry.is_dir() { "/" } else { "" };
				format!("{}:{}{}", entry.depth(), entry.name().unwrap(), suffix)
			})
			.collect()
	}

	fn matcher(paths: &'static [&'static str]) -> Option<Arc<dyn PathMatcher>> {
		Some(Arc::new(Paths(paths)))
	}

	#[test]
	fn no_filter_takes_everything() {
		assert_eq!(
			taken(&TransferFilter::default()),
			[
				"1:docs/",
				"2:a.txt",
				"2:b.jpg",
				"2:drafts/",
				"3:c.jpg",
				"1:empty/",
				"1:photos/",
				"2:d.jpg",
				"2:raw/",
				"3:e.cr2",
				"1:top.jpg",
			]
		);
	}

	#[test]
	fn an_excluded_directory_is_left_out_with_everything_inside_it() {
		let filter = TransferFilter {
			include: None,
			exclude: matcher(&["docs/drafts/", "photos/", "docs/a.txt"]),
		};
		assert_eq!(
			taken(&filter),
			["1:docs/", "2:b.jpg", "1:empty/", "1:top.jpg"]
		);
	}

	#[test]
	fn an_include_takes_the_matched_files_and_the_directories_leading_to_them() {
		let filter = TransferFilter {
			include: matcher(&["docs/drafts/c.jpg", "top.jpg"]),
			exclude: None,
		};
		assert_eq!(
			taken(&filter),
			["1:docs/", "2:drafts/", "3:c.jpg", "1:top.jpg"]
		);
	}

	#[test]
	fn an_included_directory_takes_everything_inside_it() {
		let filter = TransferFilter {
			include: matcher(&["photos/", "empty/"]),
			exclude: None,
		};
		assert_eq!(
			taken(&filter),
			["1:empty/", "1:photos/", "2:d.jpg", "2:raw/", "3:e.cr2"]
		);
	}

	#[test]
	fn an_exclude_wins_over_an_include() {
		let filter = TransferFilter {
			include: matcher(&["photos/"]),
			exclude: matcher(&["photos/raw/"]),
		};
		assert_eq!(taken(&filter), ["1:photos/", "2:d.jpg"]);
	}

	#[test]
	fn an_include_matching_nothing_takes_nothing() {
		let filter = TransferFilter {
			include: matcher(&["missing.txt"]),
			exclude: None,
		};
		assert_eq!(taken(&filter), Vec::<String>::new());
	}
}
