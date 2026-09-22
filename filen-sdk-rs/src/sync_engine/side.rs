//! The two sides a pass reconciles — the local tree and the remote view — behind the narrowest
//! access each consumer needs.
//!
//! A pass reads two path-keyed sides. A WHOLE pass materializes both (the local walk's nodes, the
//! view built out of the cache snapshot); a change-scoped pass has neither and derives them from
//! the resident baseline instead ([`derive::from_baseline`](super::derive::from_baseline)), which
//! at a million rows is the whole cost of such a pass. The two traits here are what lets a
//! consumer be handed that second shape without knowing it exists.
//!
//! The split between them is the point rather than a convenience. [`NodesAt`] answers about ONE
//! path, which a backing that derives its nodes can do without building anything else; [`Nodes`]
//! can be walked whole, which such a backing can only do by walking the tree. So a consumer that
//! asks about paths takes `NodesAt` and stays cheap whatever backs it, and one that takes `Nodes`
//! says in its own signature that it costs the tree.
//!
//! Nodes and paths come back as [`Cow`] for the same reason: a materialized side hands out a
//! reference to what it already holds — no allocation, which is what keeps a whole pass's cost
//! where it was — and a derived one hands out what it just built.

use std::{borrow::Cow, collections::HashMap};

use super::{plan::is_under, tree::at_or_under_folded};

/// One side of a pass, addressed by path.
pub(super) trait NodesAt {
	type Node: Clone;

	/// The node this side holds at `path`, or `None` when it holds nothing there.
	fn at(&self, path: &str) -> Option<Cow<'_, Self::Node>>;

	/// Whether this side holds `path` — the question that needs no node built to answer it.
	fn holds(&self, path: &str) -> bool;

	/// Whether this side holds `path` under any spelling, or anything under it: what a directory
	/// move asks of its destination, folded the way the server dedups names (see
	/// [`at_or_under_folded`]).
	fn occupied(&self, path: &str) -> bool;
}

/// A side whose every node can be enumerated — what a whole-tree read produces, and what a
/// consumer whose cost is the tree asks for.
pub(super) trait Nodes: NodesAt {
	fn len(&self) -> usize;

	fn is_empty(&self) -> bool;

	/// Every path this side holds, in no order.
	fn paths(&self) -> impl Iterator<Item = Cow<'_, str>>;

	/// Every path with its node, in no order.
	fn iter(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self::Node>)>;

	/// Everything STRICTLY under `dir` (`dir/...`), in no order.
	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self::Node>)>;
}

impl<V: Clone> NodesAt for HashMap<String, V> {
	type Node = V;

	fn at(&self, path: &str) -> Option<Cow<'_, V>> {
		self.get(path).map(Cow::Borrowed)
	}

	fn holds(&self, path: &str) -> bool {
		self.contains_key(path)
	}

	/// A scan of the keys, because a path-keyed map has no order to bisect — the baseline's own
	/// answer ([`Baseline::occupied`](super::tree::Baseline::occupied)) is a walk of one node's
	/// children, and this is the same question asked of a map that holds paths no row tracks.
	/// Folding each key into a [`collision_key`](super::scan::collision_key) first would allocate a
	/// `String` per key to learn — for all but a handful — that the first character already
	/// differs; [`at_or_under_folded`] stops there.
	fn occupied(&self, path: &str) -> bool {
		self.keys().any(|key| at_or_under_folded(key, path))
	}
}

impl<V: Clone> Nodes for HashMap<String, V> {
	fn len(&self) -> usize {
		HashMap::len(self)
	}

	fn is_empty(&self) -> bool {
		HashMap::is_empty(self)
	}

	fn paths(&self) -> impl Iterator<Item = Cow<'_, str>> {
		self.keys().map(|path| Cow::Borrowed(path.as_str()))
	}

	fn iter(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, V>)> {
		HashMap::iter(self).map(|(path, node)| (Cow::Borrowed(path.as_str()), Cow::Borrowed(node)))
	}

	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, V>)> {
		Nodes::iter(self).filter(move |(path, _)| is_under(path, dir))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The map shape both sides are, with a node type that is nothing but a marker.
	fn map(paths: &[&str]) -> HashMap<String, u32> {
		paths
			.iter()
			.enumerate()
			.map(|(at, path)| ((*path).to_owned(), at as u32))
			.collect()
	}

	#[test]
	fn a_map_answers_about_one_path() {
		let side = map(&["docs", "docs/a.txt", "docsx"]);

		assert_eq!(side.at("docs/a.txt").as_deref(), Some(&1));
		assert!(side.holds("docs/a.txt"));
		assert_eq!(side.at("docs/b.txt"), None);
		assert!(!side.holds("docs/b.txt"));
	}

	#[test]
	fn occupied_folds_the_case_and_takes_the_subtree_but_not_a_prefix() {
		let side = map(&["Docs/a.txt", "notes"]);

		assert!(
			side.occupied("docs"),
			"a directory nothing is keyed by is still occupied by what sits under it, folded"
		);
		assert!(side.occupied("DOCS/A.TXT"), "the node itself, folded");
		assert!(side.occupied("notes"));
		assert!(
			!side.occupied("note"),
			"a key that merely starts with the path does not occupy it"
		);
		assert!(!side.occupied("docs2"));
	}

	#[test]
	fn under_is_the_subtree_and_not_the_directory_itself() {
		let side = map(&["docs", "docs/a.txt", "docs/deep/b.txt", "docsx", "other"]);

		let mut under: Vec<String> = side
			.under("docs")
			.map(|(path, _)| path.into_owned())
			.collect();
		under.sort();
		assert_eq!(under, vec!["docs/a.txt", "docs/deep/b.txt"]);
	}

	#[test]
	fn a_map_enumerates_every_path_it_holds() {
		let side = map(&["a", "b/c"]);

		let mut paths: Vec<String> = side.paths().map(Cow::into_owned).collect();
		paths.sort();
		assert_eq!(paths, vec!["a", "b/c"]);
		assert_eq!(Nodes::len(&side), 2);
		assert!(!Nodes::is_empty(&side));
		assert_eq!(Nodes::iter(&side).count(), 2);
		assert!(Nodes::is_empty(&map(&[])));
	}
}
