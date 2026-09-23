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

use super::{
	plan::{is_under, moved_path},
	tree::at_or_under_folded,
};

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

/// One side of a pass, as the pass OWNS it: [`RemoteView::nodes`](super::plan::RemoteView::nodes),
/// [`LocalScan::nodes`](super::scan::LocalScan::nodes) and the two halves of
/// [`Derived`](super::derive::Derived).
///
/// Today it is the path-keyed map it replaces and nothing else, and it exists for what comes
/// next: every read goes through [`NodesAt`] / [`Nodes`] and every EDIT is a method here, so a
/// second backing — one that derives its nodes out of the resident baseline instead of holding
/// them — can be added behind this one API without a consumer changing again.
///
/// Nothing hands out a borrow of a stored node (`&T`) or of a stored path (`&str`), which is the
/// whole discipline: a backing that builds a node on demand has nothing to lend. Reads come back
/// as [`Cow`], so a materialized side still lends what it holds and allocates nothing.
#[derive(Clone)]
pub(crate) struct Side<T> {
	nodes: HashMap<String, T>,
}

impl<T> Side<T> {
	/// A side sized for `items` nodes — a map that never grows is one that never holds its old
	/// table and its new one at once.
	pub(crate) fn with_capacity(items: usize) -> Self {
		Self {
			nodes: HashMap::with_capacity(items),
		}
	}

	/// How many nodes this side has room for. Only the probe's memory accounting asks, and only
	/// a materialized side can answer with a table's capacity — which is why it is gated with the
	/// probe rather than sitting in a read trait every backing owes an answer to.
	#[cfg(feature = "bench-internals")]
	pub(crate) fn capacity(&self) -> usize {
		self.nodes.capacity()
	}

	/// Put `node` at `path`, returning whatever was there.
	pub(crate) fn insert(&mut self, path: String, node: T) -> Option<T> {
		self.nodes.insert(path, node)
	}

	/// Take the node at `path` out of this side, returning it.
	pub(crate) fn remove(&mut self, path: &str) -> Option<T> {
		self.nodes.remove(path)
	}

	/// Keep the nodes `keep` answers `true` for.
	///
	/// A WHOLE-side edit by construction — it asks about every node — so a backing that derives
	/// its nodes pays the tree for it, exactly as [`Nodes`] says in its own docs. A caller that
	/// knows which paths it is about should say so instead ([`remove`](Self::remove)).
	pub(crate) fn retain(&mut self, mut keep: impl FnMut(&str, &T) -> bool) {
		self.nodes.retain(|path, node| keep(path, node));
	}

	/// Every path STRICTLY under `dir` (`dir/...`), with no node built.
	///
	/// Owned and collected rather than borrowed and lazy because every caller is about to MUTATE
	/// this side with the answer — drop the subtree, re-key it, trash it — and the walk cannot
	/// still be borrowing it by then.
	pub(crate) fn subtree_paths(&self, dir: &str) -> Vec<String> {
		self.nodes
			.keys()
			.filter(|path| is_under(path, dir))
			.cloned()
			.collect()
	}

	/// Move the subtree at `from` onto `to` — the node at `from` itself included — telling each
	/// node its new path through `set_path`.
	///
	/// A method rather than a walk at the caller because the walk is the part a derived backing
	/// would do differently: re-keying is one edit stated in terms of two paths, and a backing
	/// that keeps its nodes by id can record it without visiting a node at all.
	pub(crate) fn rekey_subtree(&mut self, from: &str, to: &str, set_path: impl Fn(&mut T, &str)) {
		let moving: Vec<(String, String)> = self
			.nodes
			.keys()
			.filter_map(|key| Some((key.clone(), moved_path(key, from, to)?)))
			.collect();
		for (old, new) in moving {
			let Some(mut node) = self.nodes.remove(&old) else {
				continue;
			};
			set_path(&mut node, &new);
			self.nodes.insert(new, node);
		}
	}
}

impl<T> Default for Side<T> {
	fn default() -> Self {
		Self {
			nodes: HashMap::new(),
		}
	}
}

impl<T> From<HashMap<String, T>> for Side<T> {
	fn from(nodes: HashMap<String, T>) -> Self {
		Self { nodes }
	}
}

impl<T> FromIterator<(String, T)> for Side<T> {
	fn from_iter<I: IntoIterator<Item = (String, T)>>(nodes: I) -> Self {
		Self {
			nodes: nodes.into_iter().collect(),
		}
	}
}

/// This side's nodes, taken by value: the one read that consumes rather than borrows, so a
/// backing that derives its nodes hands over what it built instead of cloning it out from under
/// itself.
impl<T> IntoIterator for Side<T> {
	type Item = (String, T);
	type IntoIter = std::collections::hash_map::IntoIter<String, T>;

	fn into_iter(self) -> Self::IntoIter {
		self.nodes.into_iter()
	}
}

impl<T> Extend<(String, T)> for Side<T> {
	fn extend<I: IntoIterator<Item = (String, T)>>(&mut self, nodes: I) {
		self.nodes.extend(nodes);
	}
}

/// Node for node, whatever backs either side.
impl<T: Clone + PartialEq> PartialEq for Side<T> {
	fn eq(&self, other: &Self) -> bool {
		Nodes::len(self) == Nodes::len(other)
			&& self
				.iter()
				.all(|(path, node)| other.at(&path).is_some_and(|theirs| *theirs == *node))
	}
}

impl<T: Clone + std::fmt::Debug> std::fmt::Debug for Side<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_map().entries(Nodes::iter(self)).finish()
	}
}

impl<T: Clone> NodesAt for Side<T> {
	type Node = T;

	fn at(&self, path: &str) -> Option<Cow<'_, T>> {
		self.nodes.at(path)
	}

	fn holds(&self, path: &str) -> bool {
		self.nodes.holds(path)
	}

	fn occupied(&self, path: &str) -> bool {
		self.nodes.occupied(path)
	}
}

impl<T: Clone> Nodes for Side<T> {
	fn len(&self) -> usize {
		Nodes::len(&self.nodes)
	}

	fn is_empty(&self) -> bool {
		Nodes::is_empty(&self.nodes)
	}

	fn paths(&self) -> impl Iterator<Item = Cow<'_, str>> {
		self.nodes.paths()
	}

	fn iter(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		Nodes::iter(&self.nodes)
	}

	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		self.nodes.under(dir)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The map shape both sides are, with a node type that is nothing but a marker.
	/// The owning shape, over the same map: what it holds is what the traits answer for.
	#[test]
	fn a_side_is_the_map_it_holds() {
		let mut side: Side<u32> = Side::with_capacity(4);
		assert!(Nodes::is_empty(&side));
		side.insert("docs".to_owned(), 0);
		side.extend([("docs/a.txt".to_owned(), 1), ("docsx".to_owned(), 2)]);

		assert_eq!(Nodes::len(&side), 3);
		assert_eq!(side.at("docs/a.txt").as_deref(), Some(&1));
		assert!(side.holds("docs"));
		assert!(side.occupied("DOCS/A.TXT"));
		assert_eq!(side.subtree_paths("docs"), vec!["docs/a.txt".to_owned()]);
		assert_eq!(side.remove("docsx"), Some(2));
		assert_eq!(side.remove("docsx"), None);

		side.retain(|path, _| path != "docs/a.txt");
		assert_eq!(side.paths().collect::<Vec<_>>(), vec!["docs"]);
	}

	/// Re-keying takes the subtree AND the directory itself, and tells each node where it landed.
	#[test]
	fn rekeying_a_subtree_moves_the_root_with_it() {
		let mut side: Side<String> = ["docs", "docs/a.txt", "docs/deep/b.txt", "docsx", "other"]
			.into_iter()
			.map(|path| (path.to_owned(), path.to_owned()))
			.collect();

		side.rekey_subtree("docs", "notes", |node, path| *node = path.to_owned());

		let mut paths: Vec<String> = side.paths().map(Cow::into_owned).collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["docsx", "notes", "notes/a.txt", "notes/deep/b.txt", "other"]
		);
		assert!(
			side.iter().all(|(path, node)| *node == *path),
			"every moved node was told its new path"
		);
	}

	/// Two sides are equal when they hold the same nodes at the same paths — the property the
	/// probe compares a streamed view against a materialized one with.
	#[test]
	fn sides_compare_node_for_node() {
		let of = |paths: &[&str]| -> Side<u32> {
			paths
				.iter()
				.enumerate()
				.map(|(at, path)| ((*path).to_owned(), at as u32))
				.collect()
		};

		assert_eq!(of(&["a", "b"]), of(&["a", "b"]));
		assert_ne!(of(&["a", "b"]), of(&["a"]));
		assert_ne!(of(&["a", "b"]), of(&["a", "c"]));
	}

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
