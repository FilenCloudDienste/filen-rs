//! A pass's own edits to the rows, kept BESIDE the shared tree instead of written into a copy of it.
//!
//! A pass edits its view of the rows twice over: a directory move it folds re-keys a subtree
//! (`plan::fold_dir_moves`), and a push the remote has confirmed advances a row's agreed-content
//! marker (the confirmation steps). Neither edit is the store's — a dry run persists neither, and a
//! real pass writes its own rows when it applies — so neither may land in the tree the store
//! shares with it. Writing them into that tree used to take `Arc::make_mut` on it, which COPIES THE
//! WHOLE TREE while the store holds the other handle: at a million rows, a second ~129 MiB for a
//! pass that moved one directory or confirmed one upload.
//!
//! [`Edits`] records what the pass changed and nothing else, and [`View`] answers every question the
//! tree answers against the tree AS EDITED:
//!
//! - [`Edits::written`](Edits) holds every row this pass wrote, keyed by the path it now sits at —
//!   the rows a move landed and the rows a confirmation advanced. A path it holds a row for
//!   REPLACES the shared tree's row there.
//! - [`Edits::vacated`](Edits) holds the shared tree's nodes this pass moved AWAY, each standing for
//!   itself and its whole subtree: every row of the shared tree at or under one is gone from this
//!   view, for the rest of the pass. That is exact, not an approximation — a move takes every row at
//!   or under its source, and a later move can only bring rows back as written ones.
//!
//! So a row of the edited view is a written row, or else a shared one that no vacated node sits at
//! or above. Every read walks the two trees side by side — each directory's children merged in the
//! tree's own sibling order, a vacated shared node skipped with its whole subtree — so the edited
//! view enumerates in EXACTLY the order a copy of the tree with the same edits applied would, and
//! the folded questions fold exactly as the tree folds (they are the tree's own sibling searches,
//! run on both halves).
//!
//! # The fold window
//!
//! This is load-bearing, not an optimisation. Between a pass's first folded move and the move's
//! commit, the pass's view and the database disagree on purpose, and `occupied(to)` in
//! `plan::next_dir_move` and `occupied(parent)` in `plan::parents_ready` are asked inside that
//! window. They must see the source vacated and the destination occupied, exactly as the copied tree
//! did; the tests in `rows.rs` hold every question to what a copy with the same edits answers.

use std::collections::{BTreeSet, HashSet};

use filen_types::crypto::Blake3Hash;

use super::{
	super::baseline::{BaselineEntry, BaselineState},
	NodeId, Tree, blank_row, sibling_cmp,
};

/// What a pass changed about the rows (see the module doc). Empty edits describe the shared tree
/// exactly; a pass that edits nothing never builds one.
#[derive(Debug, Clone)]
pub(in super::super) struct Edits {
	/// Every row this pass wrote, at the path it now sits at.
	written: Tree,
	/// The shared tree's nodes this pass moved away, each with its whole subtree.
	vacated: HashSet<NodeId>,
	/// The edited view's row count, and the two other counts the tree keeps: kept here so the
	/// whole-set questions a pass asks stay O(1) whatever it moved.
	rows: usize,
	carryable: usize,
	remote_rows: usize,
}

/// One path of the edited view: its node in the shared tree when the view still shows that node,
/// and its node among the written rows when the pass wrote at or under that path. At least one of
/// the two is set.
#[derive(Debug, Clone, Copy)]
pub(in super::super) struct At {
	shared: Option<NodeId>,
	written: Option<NodeId>,
}

impl At {
	const ROOT: Self = Self {
		shared: Some(NodeId::ROOT),
		written: Some(NodeId::ROOT),
	};
}

/// The shared tree as `edits` changed it: the read half of [`Edits`].
#[derive(Debug, Clone, Copy)]
pub(in super::super) struct View<'a> {
	shared: &'a Tree,
	edits: &'a Edits,
}

impl Edits {
	/// No edits yet, over `shared`.
	pub(in super::super) fn new(shared: &Tree) -> Self {
		Self {
			written: Tree::default(),
			vacated: HashSet::new(),
			rows: shared.len(),
			carryable: shared.carryable_rows(),
			remote_rows: shared.remote_rows,
		}
	}

	pub(in super::super) fn view<'a>(&'a self, shared: &'a Tree) -> View<'a> {
		View {
			shared,
			edits: self,
		}
	}

	/// [`Tree::move_subtree`], as an edit: every row at or under `from` re-keyed under `to`,
	/// overwriting whatever row sits at each destination and leaving the rest of the destination's
	/// subtree where it is. Every source leaves before any destination is written, exactly as there.
	pub(in super::super) fn move_subtree(&mut self, shared: &Tree, from: &str, to: &str) {
		if from.is_empty() || to.is_empty() {
			tracing::error!("baseline: refusing to move the pair root ({from:?} -> {to:?})");
			return;
		}
		let view = self.view(shared);
		let Some(at) = view.resolve(from) else {
			return;
		};
		let mut moving: Vec<BaselineEntry> = Vec::new();
		if view.is_row(at) {
			moving.push(view.entry_at(at, to.to_string()));
		}
		let mut walk = view.walk(at, String::new());
		while let Some(child) = walk.next_row() {
			moving.push(view.entry_at(child, format!("{to}/{}", walk.path)));
		}
		for row in &moving {
			self.uncount(row);
		}
		if let Some(id) = at.shared {
			self.vacated.insert(id);
		}
		self.written
			.remove_subtrees(&BTreeSet::from([from.to_string()]));
		for row in &moving {
			self.write(shared, row);
		}
	}

	/// [`Tree::set_agreed`], as an edit.
	pub(in super::super) fn set_agreed(
		&mut self,
		shared: &Tree,
		rel_path: &str,
		agreed_hash: Option<Blake3Hash>,
	) -> Option<BaselineEntry> {
		let mut row = self.view(shared).get(rel_path)?;
		// What the tree hands back after the same write: its `agreed` map holds the marker only
		// where it differs from the content hash, and reads the content hash back otherwise — which
		// is then this very value.
		row.agreed_hash = agreed_hash;
		self.write(shared, &row);
		Some(row)
	}

	/// Put `row` in the view, replacing whatever row it shows at that path.
	fn write(&mut self, shared: &Tree, row: &BaselineEntry) {
		if let Some(old) = self.view(shared).get(&row.rel_path) {
			self.uncount(&old);
		}
		self.rows += 1;
		self.carryable += usize::from(row.carryable());
		self.remote_rows += usize::from(row.remote_uuid.is_some());
		self.written.upsert(row);
	}

	/// Take `row` out of the counts. Checked in EVERY build: this crate's release profile leaves
	/// overflow checks off, and a count wrapped past zero would answer `has_remote_rows` or
	/// `carryable_rows` for a pair that has no such row — the silently wrong answer the tree's own
	/// counts are asserted against.
	fn uncount(&mut self, row: &BaselineEntry) {
		fn take(count: &mut usize, by: bool, what: &str, path: &str) {
			if by {
				*count = count.checked_sub(1).unwrap_or_else(|| {
					panic!("the edited view never counted {path:?} among {what}")
				});
			}
		}
		take(&mut self.rows, true, "its rows", &row.rel_path);
		take(
			&mut self.carryable,
			row.carryable(),
			"its carryable rows",
			&row.rel_path,
		);
		take(
			&mut self.remote_rows,
			row.remote_uuid.is_some(),
			"its remote rows",
			&row.rel_path,
		);
	}
}

impl<'a> View<'a> {
	fn vacated(&self, id: NodeId) -> bool {
		self.edits.vacated.contains(&id)
	}

	/// The child of `at` named exactly `name`.
	fn child(&self, at: At, name: &str) -> Option<At> {
		let child = At {
			shared: at
				.shared
				.and_then(|id| self.shared.child(id, name))
				.filter(|&id| !self.vacated(id)),
			written: at.written.and_then(|id| self.edits.written.child(id, name)),
		};
		(child.shared.is_some() || child.written.is_some()).then_some(child)
	}

	fn resolve(&self, rel_path: &str) -> Option<At> {
		let mut at = At::ROOT;
		if rel_path.is_empty() {
			return Some(at);
		}
		for part in rel_path.split('/') {
			at = self.child(at, part)?;
		}
		Some(at)
	}

	/// The written node, when the pass wrote a ROW at this path: it replaces the shared one.
	fn written_row(&self, at: At) -> Option<NodeId> {
		at.written.filter(|&id| self.edits.written.is_row(id))
	}

	fn is_row(&self, at: At) -> bool {
		self.written_row(at).is_some() || at.shared.is_some_and(|id| self.shared.is_row(id))
	}

	fn fill_row(&self, at: At, row: &mut BaselineEntry) {
		match (self.written_row(at), at.shared) {
			(Some(id), _) => self.edits.written.fill_row(id, row),
			(None, Some(id)) => self.shared.fill_row(id, row),
			(None, None) => unreachable!("a node of the edited view is in one tree or the other"),
		}
	}

	fn entry_at(&self, at: At, rel_path: String) -> BaselineEntry {
		let mut row = blank_row();
		row.rel_path = rel_path;
		self.fill_row(at, &mut row);
		row
	}

	fn state(&self, at: At) -> BaselineState {
		match (self.written_row(at), at.shared) {
			(Some(id), _) => self.edits.written.nodes[id.index()].state,
			(None, Some(id)) => self.shared.nodes[id.index()].state,
			(None, None) => unreachable!("a node of the edited view is in one tree or the other"),
		}
	}

	fn path_of(&self, at: At) -> String {
		match (at.written, at.shared) {
			(Some(id), _) => self.edits.written.path_of(id),
			(None, Some(id)) => self.shared.path_of(id),
			(None, None) => unreachable!("a node of the edited view is in one tree or the other"),
		}
	}

	/// The two trees' `siblings`, merged in the tree's sibling order: a node both trees hold is ONE
	/// node of the view, and a vacated shared node is not in it.
	fn merge(&self, shared: &[NodeId], written: &[NodeId], out: &mut Vec<At>) {
		let (mut s, mut w) = (0, 0);
		loop {
			while s < shared.len() && self.vacated(shared[s]) {
				s += 1;
			}
			let next = match (shared.get(s), written.get(w)) {
				(None, None) => return,
				(Some(&id), None) => (Some(id), None),
				(None, Some(&id)) => (None, Some(id)),
				(Some(&a), Some(&b)) => {
					match sibling_cmp(self.shared.name(a), self.edits.written.name(b)) {
						std::cmp::Ordering::Less => (Some(a), None),
						std::cmp::Ordering::Greater => (None, Some(b)),
						std::cmp::Ordering::Equal => (Some(a), Some(b)),
					}
				}
			};
			s += usize::from(next.0.is_some());
			w += usize::from(next.1.is_some());
			out.push(At {
				shared: next.0,
				written: next.1,
			});
		}
	}

	/// Every node whose path folds to `rel_path` (see [`Tree::folded_nodes`]).
	fn folded_nodes(&self, rel_path: &str) -> Vec<At> {
		let mut at = vec![At::ROOT];
		for part in rel_path.split('/') {
			let mut next = Vec::new();
			for parent in at {
				let shared = parent
					.shared
					.map_or(&[][..], |id| self.shared.folded_children(id, part));
				let written = parent
					.written
					.map_or(&[][..], |id| self.edits.written.folded_children(id, part));
				self.merge(shared, written, &mut next);
			}
			if next.is_empty() {
				return Vec::new();
			}
			at = next;
		}
		at
	}

	/// Whether the subtree at `at`, `at` included, holds a row of the view. Unlike the tree's, a
	/// node here can hold none: a shared directory whose every row the pass moved away.
	fn holds_rows(&self, at: At) -> bool {
		self.is_row(at) || self.walk(at, String::new()).next_row().is_some()
	}

	fn walk(&self, root: At, root_path: String) -> Walk<'a> {
		Walk {
			view: *self,
			stack: vec![Frame {
				at: root,
				kids: Vec::new(),
				next: 0,
				listed: false,
				before: root_path.len(),
			}],
			path: root_path,
		}
	}

	/// Whether the view shows the shared tree's row `id` at `rel_path`: no vacated node at or above
	/// it, and no written row over it.
	fn shows_shared(&self, id: NodeId, rel_path: &str) -> bool {
		let mut at = id;
		while at != NodeId::ROOT {
			if self.vacated(at) {
				return false;
			}
			at = self.shared.nodes[at.index()].parent;
		}
		!self.edits.written.contains_key(rel_path)
	}

	pub(in super::super) fn len(&self) -> usize {
		self.edits.rows
	}

	pub(in super::super) fn carryable_rows(&self) -> usize {
		self.edits.carryable
	}

	pub(in super::super) fn has_remote_rows(&self) -> bool {
		self.edits.remote_rows > 0
	}

	pub(in super::super) fn get(&self, rel_path: &str) -> Option<BaselineEntry> {
		let at = self.resolve(rel_path)?;
		self.is_row(at)
			.then(|| self.entry_at(at, rel_path.to_string()))
	}

	pub(in super::super) fn contains_key(&self, rel_path: &str) -> bool {
		self.resolve(rel_path).is_some_and(|at| self.is_row(at))
	}

	pub(in super::super) fn carryable(&self, rel_path: &str) -> bool {
		let Some(at) = self.resolve(rel_path) else {
			return false;
		};
		match (self.written_row(at), at.shared) {
			(Some(id), _) => !self.edits.written.uncarryable.contains(&id),
			(None, Some(id)) => self.shared.is_row(id) && !self.shared.uncarryable.contains(&id),
			(None, None) => false,
		}
	}

	pub(in super::super) fn tracked(&self, rel_path: &str, is_dir: bool) -> bool {
		let Some(at) = self.resolve(rel_path) else {
			return false;
		};
		self.is_row(at) || (is_dir && self.walk(at, String::new()).next_row().is_some())
	}

	pub(in super::super) fn iter(self) -> impl Iterator<Item = BaselineEntry> + 'a {
		self.walk(At::ROOT, String::new())
			.map(move |(at, path)| self.entry_at(at, path))
	}

	pub(in super::super) fn visit_rows(&self, mut visit: impl FnMut(&BaselineEntry)) {
		let mut walk = self.walk(At::ROOT, String::new());
		let mut row = blank_row();
		while let Some(at) = walk.next_row() {
			row.rel_path.clear();
			row.rel_path.push_str(&walk.path);
			self.fill_row(at, &mut row);
			visit(&row);
		}
	}

	#[cfg(feature = "bench-internals")]
	pub(in super::super) fn paths(self) -> impl Iterator<Item = String> + 'a {
		self.walk(At::ROOT, String::new()).map(|(_, path)| path)
	}

	pub(in super::super) fn visit_row_paths(&self, mut visit: impl FnMut(&str)) {
		let mut walk = self.walk(At::ROOT, String::new());
		while walk.next_row().is_some() {
			visit(&walk.path);
		}
	}

	pub(in super::super) fn subtree(self, root: &str) -> impl Iterator<Item = BaselineEntry> + 'a {
		let from = self.resolve(root);
		let prefix = root.to_string();
		from.into_iter()
			.flat_map(move |at| self.walk(at, prefix.clone()))
			.map(move |(at, path)| self.entry_at(at, path))
	}

	pub(in super::super) fn visit_subtree_paths(&self, root: &str, mut visit: impl FnMut(&str)) {
		let Some(at) = self.resolve(root) else {
			return;
		};
		let mut walk = self.walk(at, root.to_string());
		while walk.next_row().is_some() {
			visit(&walk.path);
		}
	}

	/// The rows awaiting confirmation: the shared tree's that the view still shows, then the
	/// written ones. In no order, like the tree's.
	pub(in super::super) fn unconfirmed(self) -> impl Iterator<Item = BaselineEntry> + 'a {
		self.shared
			.agreed
			.keys()
			.filter(move |&&id| self.shared.awaits_confirmation(id))
			.filter_map(move |&id| {
				let path = self.shared.path_of(id);
				self.shows_shared(id, &path)
					.then(|| self.shared.entry_at(id, path))
			})
			.chain(self.edits.written.unconfirmed())
	}

	pub(in super::super) fn any_unconfirmed(&self) -> bool {
		self.unconfirmed().next().is_some()
	}

	/// The shared tree's rows in `index` that the view still shows, then the written rows in the
	/// same index of the written tree. In no order, like the tree's.
	fn indexed(
		self,
		index: impl Fn(&Tree) -> &HashSet<NodeId>,
	) -> impl Iterator<Item = String> + 'a {
		let written = &self.edits.written;
		index(self.shared)
			.iter()
			.filter_map(move |&id| {
				let path = self.shared.path_of(id);
				self.shows_shared(id, &path).then_some(path)
			})
			.chain(index(written).iter().map(|&id| written.path_of(id)))
			.collect::<Vec<_>>()
			.into_iter()
	}

	pub(in super::super) fn rule_file_rows(self) -> impl Iterator<Item = String> + 'a {
		self.indexed(|tree| &tree.rule_files)
	}

	pub(in super::super) fn uncarryable_paths(self) -> impl Iterator<Item = String> + 'a {
		self.indexed(|tree| &tree.uncarryable)
	}

	pub(in super::super) fn occupied(&self, rel_path: &str) -> bool {
		if rel_path.is_empty() {
			return false;
		}
		self.folded_nodes(rel_path)
			.into_iter()
			.any(|at| self.holds_rows(at))
	}

	pub(in super::super) fn folded_row_paths(&self, rel_path: &str) -> Vec<String> {
		if rel_path.is_empty() {
			return Vec::new();
		}
		self.folded_nodes(rel_path)
			.into_iter()
			.filter(|&at| self.is_row(at))
			.map(|at| self.path_of(at))
			.collect()
	}

	pub(in super::super) fn any_folded_row_at_or_under(
		&self,
		rel_path: &str,
		held: &mut impl FnMut(&str) -> bool,
	) -> bool {
		if rel_path.is_empty() {
			return false;
		}
		for at in self.folded_nodes(rel_path) {
			let path = self.path_of(at);
			if self.is_row(at) && held(&path) {
				return true;
			}
			let mut walk = self.walk(at, path);
			while walk.next_row().is_some() {
				if held(&walk.path) {
					return true;
				}
			}
		}
		false
	}

	pub(in super::super) fn subtree_all_synced(&self, rel_path: &str) -> bool {
		let Some(at) = self.resolve(rel_path) else {
			return true;
		};
		let mut walk = self.walk(at, String::new());
		while let Some(child) = walk.next_row() {
			if self.state(child) != BaselineState::Synced {
				return false;
			}
		}
		true
	}
}

/// One directory of a [`Walk`]: its merged children, listed when the walk first descends past it.
struct Frame {
	at: At,
	kids: Vec<At>,
	next: usize,
	listed: bool,
	/// How long the walk's path was before this node's name.
	before: usize,
}

/// Every row of the view under one node, parent before child, siblings in the tree's order — the
/// edited view's form of the tree's own walk, with the same one reused path buffer.
struct Walk<'a> {
	view: View<'a>,
	stack: Vec<Frame>,
	path: String,
}

impl Walk<'_> {
	fn next_row(&mut self) -> Option<At> {
		loop {
			let view = self.view;
			let frame = self.stack.last_mut()?;
			if !frame.listed {
				let shared = frame.at.shared.map_or(&[][..], |id| view.shared.kids(id));
				let written = frame
					.at
					.written
					.map_or(&[][..], |id| view.edits.written.kids(id));
				view.merge(shared, written, &mut frame.kids);
				frame.listed = true;
			}
			let Some(&child) = frame.kids.get(frame.next) else {
				let frame = self.stack.pop().expect("the frame was just read");
				if self.stack.is_empty() {
					return None;
				}
				self.path.truncate(frame.before);
				continue;
			};
			frame.next += 1;
			let before = self.path.len();
			if !self.path.is_empty() {
				self.path.push('/');
			}
			let name = match (child.shared, child.written) {
				(Some(id), _) => view.shared.name(id),
				(None, Some(id)) => view.edits.written.name(id),
				(None, None) => unreachable!("a merged child is in one tree or the other"),
			};
			self.path.push_str(name);
			self.stack.push(Frame {
				at: child,
				kids: Vec::new(),
				next: 0,
				listed: false,
				before,
			});
			if view.is_row(child) {
				return Some(child);
			}
		}
	}
}

impl Iterator for Walk<'_> {
	type Item = (At, String);

	fn next(&mut self) -> Option<Self::Item> {
		let at = self.next_row()?;
		Some((at, self.path.clone()))
	}
}
