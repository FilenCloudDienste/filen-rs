//! The resident baseline: one pair's rows as an id-keyed tree.
//!
//! The store used to hold one of these per pair for the life of the pair, and every pass read its
//! rows out of it. A pass now reads them out of the store's table through
//! [`rows::Baseline`](super::rows::Baseline), and this tree is compiled only into the tests —
//! where it is the ORACLE that reader is held to, real code answering every question the reader
//! answers — and into `bench-internals`, whose probe still sizes one. What follows is the design
//! as it stood when it was the backing.
//!
//! The rows a pass reconciles against used to be a `HashMap<String, BaselineEntry>` — 808 bytes a
//! row measured, every path materialized and stored whole, and a second copy of it in every
//! per-pass structure keyed the same way. This holds the same rows as a `Vec<Node>` addressed by a
//! [`NodeId`], where each node carries its LEAF NAME and its parent's id and nothing else about
//! where it sits. A path is walked out of the tree only where one is actually needed — the dirty
//! set, the report, a log line.
//!
//! What that buys, besides the bytes: the questions a pass asks about a SUBTREE stop being scans of
//! every row. "Is anything at or under this path, under any spelling" ([`Tree::occupied`]),
//! "does the baseline still track anything here" ([`Tree::tracked`]), "is every row under this
//! directory synced" ([`Tree::subtree_all_synced`]) are all answered from the node's own
//! children or from an index.
//!
//! "Where is the row with this uuid" is NOT one of them any more. It used to be a resident
//! `HashMap` per id — 73.5 MiB of the 211.6 a million-row pair cost, to answer a handful of point
//! lookups per pass — and it is now a seek of the store's own index
//! ([`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths)), resolved for
//! the items a pass names before it reads either side.
//!
//! # Siblings are ordered by their FOLDED name, folded on the fly
//!
//! Filen dedups names case-insensitively, so the engine keys collisions by
//! [`collision_key`](super::scan::collision_key) — `char::to_lowercase` over the whole path, which
//! globset's own case-insensitive flag cannot do (it folds ASCII only). Each directory's children
//! are kept sorted by that same folding, applied by [`fold_cmp`] as the comparison runs rather than
//! stored as a second copy of every name, and ties — two siblings that fold together, which the
//! store can hold because its primary key compares bytewise — are broken by the raw name. So a
//! lookup by exact name and a lookup by folded name are both a binary search of the one vector.
//!
//! # Every leaf name lives in ONE allocation
//!
//! A node does not own its name: it holds a `(offset, length)` into [`Tree::names`], one
//! `String` the whole tree's leaf names are appended to. That is 8 bytes a node instead of 16, and
//! — the part that costs more than the 8 — it is ONE heap block for a million names rather than a
//! million blocks an allocator has to keep pages resident for. Cloning the tree, which
//! `Arc::make_mut` does on the first write of every pass that writes, copies two buffers instead
//! of walking a million allocations.
//!
//! Names are never edited in place: a rename is a remove and an insert, so the old bytes are
//! simply left behind and [`Tree::maybe_compact_names`] rebuilds the arena once most of it is
//! dead. That is the one structure here whose size could otherwise follow the pair's HISTORY
//! rather than its tree.
//!
//! # A path with no row of its own
//!
//! A row can sit under a directory that has no row (a destination-only item adopted at a mode
//! switch, a subtree whose directory rows were untracked). Such a path becomes a node with
//! [`PRESENT`] clear: it holds its children and nothing else. It is not a row — it is not counted
//! by [`Tree::len`], never yielded by an iterator, and never answers [`Tree::get`] — and it
//! is pruned the moment its last child leaves, so "this node has children" and "this node has a row
//! under it" mean the same thing.

// Under `bench-internals` alone only the probe's handful of these is called; the rest is the
// oracle's, and the tests call every one. Allowed for the module rather than gated item by item:
// the gates would cover most of the file and say no more than this does.
#![cfg_attr(not(test), allow(dead_code))]

use std::{
	cmp::Ordering,
	collections::{BTreeSet, HashMap, HashSet},
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use uuid::Uuid;

use super::{
	baseline::{BaselineChange, BaselineEntry, BaselineState, NodeKind},
	ignore::FILENIGNORE,
};

/// A node's index in [`Tree::nodes`]. The root is [`NodeId::ROOT`] and is never a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct NodeId(u32);

impl NodeId {
	/// The pair root: the one node with no name and no row, whose children are the top-level paths.
	pub(super) const ROOT: Self = Self(0);

	fn index(self) -> usize {
		self.0 as usize
	}

	/// A `u32` id is what makes the node 104 bytes rather than 112; a pair with four billion rows
	/// has lost long before this panics (the store's own row cap is the disk).
	fn from_index(index: usize) -> Self {
		Self(u32::try_from(index).expect("a sync pair holds fewer than 4 billion baseline rows"))
	}
}

/// `Node::flags`: which of the optional halves of a [`BaselineEntry`] this node actually holds, so
/// the fields can be bare values instead of `Option`s (24 bytes saved a node at this width) without
/// a sentinel value that a real uuid or hash could collide with.
const PRESENT: u8 = 1 << 0;
const HAS_REMOTE_UUID: u8 = 1 << 1;
const HAS_CONTENT_HASH: u8 = 1 << 2;
const HAS_SIZE: u8 = 1 << 3;
const HAS_LOCAL_MTIME: u8 = 1 << 4;
const HAS_REMOTE_MODIFIED: u8 = 1 << 5;

/// One baseline row, or a path a row sits under (see the module doc).
///
/// `stable_uuid` is the one optional field that stays an `Option`: [`StableUuid`] is deliberately
/// not constructible outside its deserialization boundaries, so there is no value to use as the
/// absent one.
#[derive(Debug, Clone)]
struct Node {
	parent: NodeId,
	/// Where this node's leaf name sits in [`Tree::names`]: a byte offset and a byte length,
	/// NFC and exactly as the row's path spells it. Never the path, and never an allocation of its
	/// own (see the module doc).
	name_at: u32,
	name_len: u32,
	kind: NodeKind,
	state: BaselineState,
	flags: u8,
	remote_uuid: Uuid,
	stable_uuid: Option<StableUuid>,
	content_hash: [u8; 32],
	size: u64,
	local_mtime: i64,
	remote_modified: i64,
}

/// `node`'s leaf name in the arena `names`. The span is one whole name [`Tree::push_name`]
/// appended, so it starts and ends on a character boundary.
fn leaf_name<'n>(names: &'n str, node: &Node) -> &'n str {
	names
		.get(node.name_at as usize..)
		.and_then(|rest| rest.get(..node.name_len as usize))
		.expect("a node's span is a whole name the arena holds (should be impossible)")
}

impl Node {
	/// A node with no name yet: [`Tree::insert_node`] puts one in the arena and points the node
	/// at it, so the arena stays the only place a name is ever written.
	fn empty(parent: NodeId) -> Self {
		Self {
			parent,
			name_at: 0,
			name_len: 0,
			kind: NodeKind::Dir,
			state: BaselineState::Synced,
			flags: 0,
			remote_uuid: Uuid::nil(),
			stable_uuid: None,
			content_hash: [0; 32],
			size: 0,
			local_mtime: 0,
			remote_modified: 0,
		}
	}

	fn has(&self, flag: u8) -> bool {
		self.flags & flag != 0
	}
}

/// What each side held when a conflict was surfaced — the half of a [`BaselineEntry`] only a
/// conflicted row carries, kept beside the nodes so the other 99.99 % of rows do not pay for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ConflictSides {
	local_kind: Option<NodeKind>,
	remote_kind: Option<NodeKind>,
	remote_hash: Option<Blake3Hash>,
	remote_size: Option<u64>,
}

impl ConflictSides {
	fn of(entry: &BaselineEntry) -> Option<Self> {
		let sides = Self {
			local_kind: entry.local_kind,
			remote_kind: entry.remote_kind,
			remote_hash: entry.remote_hash,
			remote_size: entry.remote_size,
		};
		(sides != Self::default()).then_some(sides)
	}
}

/// Compare two names the way [`collision_key`](super::scan::collision_key) folds them — every
/// `char::to_lowercase` expansion, not just the ASCII range — without materializing either folded
/// form. Two names are `Equal` here exactly when their collision keys are equal.
pub(super) fn fold_cmp(a: &str, b: &str) -> Ordering {
	// Every comparison of a lookup runs through here, and almost every name on a real drive is
	// ASCII, where folding a byte is one instruction and `char::to_lowercase` is a table lookup
	// returning an iterator. The two agree on ASCII by construction — `to_lowercase` maps an ASCII
	// char to exactly one ASCII char — so the fast path is the same answer, not an approximation.
	if a.is_ascii() && b.is_ascii() {
		return a
			.as_bytes()
			.iter()
			.map(u8::to_ascii_lowercase)
			.cmp(b.as_bytes().iter().map(u8::to_ascii_lowercase));
	}
	a.chars()
		.flat_map(char::to_lowercase)
		.cmp(b.chars().flat_map(char::to_lowercase))
}

/// The order a directory's children are kept in: by folded name, then by the raw one. The store's
/// primary key compares bytewise, so `A` and `a` are two rows of one directory; the tie-break is
/// what keeps both of them addressable.
fn sibling_cmp(a: &str, b: &str) -> Ordering {
	fold_cmp(a, b).then_with(|| a.cmp(b))
}

/// A row with nothing set and no path: the buffer [`Tree::visit_rows`] refills, and the value
/// [`Tree::fill_row`] writes every field of.
fn blank_row() -> BaselineEntry {
	BaselineEntry {
		rel_path: String::new(),
		kind: NodeKind::Dir,
		remote_uuid: None,
		content_hash: None,
		size: None,
		local_mtime: None,
		remote_modified: None,
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// One pair's baseline rows, resident between passes (see the module doc).
#[derive(Debug, Clone)]
pub(super) struct Tree {
	nodes: Vec<Node>,
	/// Every leaf name in the tree, appended end to end; a node addresses its own by offset and
	/// length (see the module doc). Never indexed by anything but a node's own pair, so the
	/// boundaries between names are the nodes and this needs no separators.
	names: String,
	/// How many bytes of [`names`](Self::names) no node points at any more — what
	/// [`maybe_compact_names`](Self::maybe_compact_names) weighs against the live ones.
	dead_names: usize,
	/// Slots whose node was removed, handed back out before the `Vec` grows again.
	free: Vec<NodeId>,
	/// Each directory's children, sorted by [`sibling_cmp`]. A node with no entry here has none.
	children: HashMap<NodeId, Vec<NodeId>>,
	/// Conflicted rows only.
	side: HashMap<NodeId, ConflictSides>,
	/// Rows whose `agreed_hash` is NOT their `content_hash` — the shape an unconfirmed push leaves
	/// (see [`Tree::awaits_confirmation`]). A row that is not here agrees with itself, which is
	/// every row of a converged pair.
	agreed: HashMap<NodeId, Option<Blake3Hash>>,
	/// The rows that stand in for NEITHER side of a pass
	/// ([`BaselineEntry::carryable`]) — the ones a change-scoped pass has to re-observe instead of
	/// carrying forward.
	///
	/// Indexed, rather than found by walking, because a pass needs the WHOLE list of them before it
	/// reads anything: they go into its dirty set (re-observe) and its held set (plan nothing here).
	/// Finding them by walking the tree is the one whole-tree cost a change-scoped pass could not
	/// otherwise shed — at a million rows that walk IS the pass. Almost every row of a converged
	/// pair is carryable, so this is empty in the steady state.
	uncarryable: HashSet<NodeId>,
	/// The rows that ARE a `.filenignore` — a file whose leaf name is [`FILENIGNORE`].
	///
	/// Indexed for the same reason [`uncarryable`](Self::uncarryable) is: two steps of every
	/// change-scoped pass need the whole list of them before either side is read — the candidate
	/// set `load_remote_rules` reads the remote copies from, and the `RuleFiles::Only` set a mode
	/// that does not push hands the local scan. Both used to find them by walking every row, which
	/// at a million rows is two whole-tree scans to discover that a pair has one rule file or none.
	rule_files: HashSet<NodeId>,
	/// How many nodes are rows (see [`PRESENT`]).
	rows: usize,
	/// How many of those rows record a remote item — a `remote_uuid` on record.
	///
	/// A COUNT, where a `HashMap<Uuid, NodeId>` used to stand: the only whole-set question that map
	/// answered is [`has_remote_rows`](Self::has_remote_rows), and answering it needs how many
	/// rather than which. The per-item lookups it also served are the store's now.
	remote_rows: usize,
}

impl Default for Tree {
	fn default() -> Self {
		Self {
			nodes: vec![Node::empty(NodeId::ROOT)],
			names: String::new(),
			dead_names: 0,
			free: Vec::new(),
			children: HashMap::new(),
			side: HashMap::new(),
			agreed: HashMap::new(),
			uncarryable: HashSet::new(),
			rule_files: HashSet::new(),
			rows: 0,
			remote_rows: 0,
		}
	}
}

/// What [`Tree::resident_bytes`] is made of, one field per structure it counts.
///
/// Reported per term rather than summed because the sum cannot be acted on. Every field is in
/// BYTES and counted the way `resident_bytes` counts — a heap allocation rounded to the 16-byte
/// size class macOS and glibc both use, and a map's own `capacity` rather than its live entries,
/// which is where a `HashMap`'s slack lives.
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResidentTerms {
	/// The node array's own allocation: `capacity`, not `len`.
	pub(super) nodes: usize,
	/// The name arena: the ONE allocation every leaf name lives in, the bytes no node points at
	/// any more included (see [`Tree::maybe_compact_names`]). Not the paths — no node holds
	/// one.
	pub(super) names: usize,
	/// The removed-slot free list.
	pub(super) free: usize,
	/// Every directory's children vector.
	pub(super) children_vecs: usize,
	/// The table those vectors are filed in, which is a separate cost from the vectors.
	pub(super) children_table: usize,
	/// Conflicted rows only, so empty on a converged pair.
	pub(super) side: usize,
	/// Unconfirmed pushes only, so empty on a converged pair.
	pub(super) agreed: usize,
	pub(super) uncarryable: usize,
	pub(super) rule_files: usize,
}

#[cfg(feature = "bench-internals")]
impl ResidentTerms {
	/// Every term under its own name, in the order a table should print them.
	///
	/// The ONE place the field list lives for a reader: [`total`](Self::total) sums THIS, so a term
	/// added to the struct and forgotten here would be missing from the total as well — which the
	/// accounting would show as a remainder rather than hide, and which
	/// `every_resident_term_is_named` fails on outright.
	pub(super) fn named(&self) -> [(&'static str, usize); 9] {
		[
			("nodes", self.nodes),
			("names", self.names),
			("free", self.free),
			("children_vecs", self.children_vecs),
			("children_table", self.children_table),
			("side", self.side),
			("agreed", self.agreed),
			("uncarryable", self.uncarryable),
			("rule_files", self.rule_files),
		]
	}

	/// What the tree holds for the life of the pair: every term added up.
	pub(super) fn total(&self) -> usize {
		self.named().iter().map(|&(_, bytes)| bytes).sum()
	}
}

impl Tree {
	/// The rows as the store read them back, in any order.
	///
	/// The store's own read builds the tree row by row instead
	/// ([`BaselineStore::baseline`](super::baseline::BaselineStore::baseline)), so what is left
	/// here is the tests' way of writing a tree down and the probe's.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn from_rows(rows: impl IntoIterator<Item = BaselineEntry>) -> Self {
		let mut baseline = Self::default();
		for entry in rows {
			baseline.upsert(&entry);
		}
		baseline.shrink_after_load();
		baseline
	}

	/// Give back the room the load's doubling took but the tree will not use.
	///
	/// The end of a load is the one moment where the whole tree's size is known and nothing is
	/// about to grow. A `Vec` that doubled its way to a million nodes holds room for two million,
	/// and the resident copy keeps that slack for the life of the pair: 105 MiB of it at a million
	/// rows, which is a third of what the tree costs. The maps double the same way.
	///
	/// Called by every path that loads a pair — `from_rows` and the store's own
	/// row-at-a-time read — and by nothing else: on a tree a pass is still writing to, giving the
	/// room back only means taking it again.
	pub(super) fn shrink_after_load(&mut self) {
		self.nodes.shrink_to_fit();
		// The arena doubled its way up like the rest, and a load writes no dead bytes, so this is
		// the one moment it is exactly as big as the tree needs.
		self.names.shrink_to_fit();
		self.side.shrink_to_fit();
		self.agreed.shrink_to_fit();
		self.uncarryable.shrink_to_fit();
		self.rule_files.shrink_to_fit();
	}

	/// How many rows the pair tracks.
	pub(super) fn len(&self) -> usize {
		self.rows
	}

	#[cfg(test)]
	pub(super) fn is_empty(&self) -> bool {
		self.rows == 0
	}

	fn name(&self, id: NodeId) -> &str {
		leaf_name(&self.names, &self.nodes[id.index()])
	}

	fn is_row(&self, id: NodeId) -> bool {
		self.nodes[id.index()].has(PRESENT)
	}

	fn kids(&self, id: NodeId) -> &[NodeId] {
		self.children.get(&id).map_or(&[], Vec::as_slice)
	}

	/// The child of `parent` named exactly `name`.
	fn child(&self, parent: NodeId, name: &str) -> Option<NodeId> {
		let kids = self.children.get(&parent)?;
		let at = kids
			.binary_search_by(|&id| sibling_cmp(self.name(id), name))
			.ok()?;
		Some(kids[at])
	}

	/// The children of `parent` whose names FOLD to `name` — usually one, two when a directory
	/// holds both `A` and `a`.
	fn folded_children(&self, parent: NodeId, name: &str) -> &[NodeId] {
		let Some(kids) = self.children.get(&parent) else {
			return &[];
		};
		let start = kids.partition_point(|&id| fold_cmp(self.name(id), name) == Ordering::Less);
		let len =
			kids[start..].partition_point(|&id| fold_cmp(self.name(id), name) == Ordering::Equal);
		&kids[start..start + len]
	}

	/// The node at `rel_path`, row or not.
	fn resolve(&self, rel_path: &str) -> Option<NodeId> {
		if rel_path.is_empty() {
			return Some(NodeId::ROOT);
		}
		let mut at = NodeId::ROOT;
		for part in rel_path.split('/') {
			at = self.child(at, part)?;
		}
		Some(at)
	}

	/// Whether the subtree rooted at `id` holds a row. A node with children always does — a node
	/// with no row of its own is pruned as soon as its last child leaves — so this is one lookup.
	fn holds_rows(&self, id: NodeId) -> bool {
		self.is_row(id) || !self.kids(id).is_empty()
	}

	/// The path of `id`, walked out of its ancestors. `""` for the root.
	pub(super) fn path_of(&self, id: NodeId) -> String {
		let mut parts: Vec<&str> = Vec::new();
		let mut at = id;
		while at != NodeId::ROOT {
			parts.push(self.name(at));
			at = self.nodes[at.index()].parent;
		}
		parts.reverse();
		parts.join("/")
	}

	/// A lookup that remembers the directory it last looked in (see [`Cursor`]).
	pub(super) fn cursor(&self) -> Cursor<'_> {
		Cursor {
			baseline: self,
			dir: String::new(),
			at: NodeId::ROOT,
		}
	}

	/// The row at `rel_path`, rebuilt from its node.
	pub(super) fn get(&self, rel_path: &str) -> Option<BaselineEntry> {
		let id = self.resolve(rel_path)?;
		self.is_row(id)
			.then(|| self.entry_at(id, rel_path.to_string()))
	}

	pub(super) fn contains_key(&self, rel_path: &str) -> bool {
		self.resolve(rel_path).is_some_and(|id| self.is_row(id))
	}

	/// Every row, parent before child, each with its path walked out for it.
	pub(super) fn iter(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.walk(NodeId::ROOT, String::new())
			.map(|(id, path)| self.entry_at(id, path))
	}

	/// Every row, parent before child, handed to `visit` as one buffer this REFILLS per row — the
	/// borrowing form of [`iter`](Self::iter), which builds a fresh row and a fresh path each time.
	///
	/// It is what the whole-tree questions of a pass ask with, the ones whose answer for almost every
	/// row is "no": has this file's uuid moved, is this a synced directory. The predicate is then
	/// checked against a row that cost no allocation, and only a caller that KEEPS one pays for it.
	/// The buffer's `rel_path` is the row's own path throughout, so a row handed on is a correct row.
	pub(super) fn visit_rows(&self, mut visit: impl FnMut(&BaselineEntry)) {
		let mut walk = self.walk(NodeId::ROOT, String::new());
		let mut row = blank_row();
		while let Some(id) = walk.next_row() {
			row.rel_path.clear();
			row.rel_path.push_str(&walk.path);
			self.fill_row(id, &mut row);
			visit(&row);
		}
	}

	/// Every row's path, parent before child.
	///
	/// No pass asks for this: what a pass wants is a question answered per path, which
	/// [`visit_row_paths`](Self::visit_row_paths) does without the allocation. It stays for the probe,
	/// which measures what materializing them costs, and for the tests, which compare against it.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn paths(&self) -> impl Iterator<Item = String> + '_ {
		self.walk(NodeId::ROOT, String::new()).map(|(_, path)| path)
	}

	/// Every row's path, parent before child, handed to `visit` as a slice of the ONE buffer the
	/// walk reuses — the borrowing form of [`paths`](Self::paths).
	///
	/// It is what a caller that only asks a QUESTION about each path uses: a whole-tree question then
	/// costs no allocation at all, where `paths` costs one per row. A caller that needs to keep a
	/// path still has to copy it, which is the point — the buffer is gone by the next row.
	pub(super) fn visit_row_paths(&self, mut visit: impl FnMut(&str)) {
		let mut walk = self.walk(NodeId::ROOT, String::new());
		while walk.next_row().is_some() {
			visit(&walk.path);
		}
	}

	/// Every row STRICTLY under `root`.
	pub(super) fn subtree(&self, root: &str) -> impl Iterator<Item = BaselineEntry> + '_ {
		let from = self.resolve(root);
		let prefix = root.to_string();
		from.into_iter()
			.flat_map(move |id| self.walk(id, prefix.clone()))
			.map(|(id, path)| self.entry_at(id, path))
	}

	/// Every path STRICTLY under `root`, handed to `visit` as a slice of the ONE buffer the walk
	/// reuses — the subtree form of [`visit_row_paths`](Self::visit_row_paths).
	///
	/// What [`subtree`](Self::subtree) costs is a rebuilt row and a fresh `String` per row; a
	/// caller that only counts what is down there, or asks a question of each path, pays neither.
	pub(super) fn visit_subtree_paths(&self, root: &str, mut visit: impl FnMut(&str)) {
		let Some(id) = self.resolve(root) else {
			return;
		};
		let mut walk = self.walk(id, root.to_string());
		while walk.next_row().is_some() {
			visit(&walk.path);
		}
	}

	/// The rows awaiting confirmation: this side's content is on record and is not what the two
	/// sides last agreed on (see [`Tree::agreed`]). The whole map is the candidate set, so a
	/// converged pair answers "none" without touching a node.
	pub(super) fn unconfirmed(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.agreed
			.keys()
			.filter(|&&id| self.awaits_confirmation(id))
			.map(|&id| self.entry_at(id, self.path_of(id)))
	}

	/// Whether any row awaits confirmation — the gate a pass and the confirmation sweep take before
	/// they copy anything.
	pub(super) fn any_unconfirmed(&self) -> bool {
		self.agreed.keys().any(|&id| self.awaits_confirmation(id))
	}

	fn awaits_confirmation(&self, id: NodeId) -> bool {
		let node = &self.nodes[id.index()];
		node.has(PRESENT)
			&& node.kind == NodeKind::File
			&& node.state == BaselineState::Synced
			&& node.has(HAS_CONTENT_HASH)
	}

	/// Whether any row records a remote item: what tells a remote view that came back empty apart
	/// from a pair that never had anything on the remote.
	pub(super) fn has_remote_rows(&self) -> bool {
		self.remote_rows > 0
	}

	/// Where this tree records each of `uuids` and `lineages`, found by ONE walk of every row.
	///
	/// The test and probe stand-in for
	/// [`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths), which answers
	/// the same question off the DB's `(pair_id, remote_uuid)` index. No pass calls this: it is
	/// O(tree) by construction, which is the cost that moving the two resident uuid maps into SQL
	/// was about. Same tie-break as the store's — the lexicographically first path wins — so a
	/// baseline the tests build and one the store holds answer a duplicated id identically.
	#[cfg(test)]
	pub(super) fn synced_paths(
		&self,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> super::baseline::SyncedPaths {
		let mut out = super::baseline::SyncedPaths::asking(uuids, lineages);
		self.visit_rows(|row| {
			out.offer(&row.rel_path, row.remote_uuid, row.remote_stable_uuid);
		});
		out
	}

	/// Whether a row sits at `rel_path` under ANY spelling, or anywhere under it — the question the
	/// server's case-insensitive name dedup makes a directory move ask about its destination.
	///
	/// `""` holds nothing, exactly as the path-keyed form it replaces answered (nothing is `""` and
	/// nothing is strictly under it).
	pub(super) fn occupied(&self, rel_path: &str) -> bool {
		if rel_path.is_empty() {
			return false;
		}
		self.folded_nodes(rel_path)
			.into_iter()
			.any(|id| self.holds_rows(id))
	}

	/// Every node whose path folds to `rel_path` — more than one only where a directory on the way
	/// down holds two spellings of the same name.
	fn folded_nodes(&self, rel_path: &str) -> Vec<NodeId> {
		let mut at = vec![NodeId::ROOT];
		for part in rel_path.split('/') {
			let mut next = Vec::new();
			for parent in at {
				next.extend_from_slice(self.folded_children(parent, part));
			}
			if next.is_empty() {
				return Vec::new();
			}
			at = next;
		}
		at
	}

	/// The path of every ROW whose path folds to `rel_path`, under whatever spelling — the
	/// EXACT-key form of [`occupied`](Self::occupied), which answers for the subtree as well.
	///
	/// Normally empty or one path: more than one only where a directory on the way down holds two
	/// spellings of the same name. What asks is the narrowed collision check of a change-scoped
	/// pass, per path it decided, where scanning the view's every key is the cost being removed.
	pub(super) fn folded_row_paths(&self, rel_path: &str) -> Vec<String> {
		if rel_path.is_empty() {
			return Vec::new();
		}
		self.folded_nodes(rel_path)
			.into_iter()
			.filter(|&id| self.is_row(id))
			.map(|id| self.path_of(id))
			.collect()
	}

	/// Whether the baseline still tracks anything at `rel_path`, or — for a directory — under it.
	///
	/// This is what decides whether an ignore rule's hit is a ROOT the pass records, reports and
	/// untracks, or an entry the rules merely hide: the built-in defaults hide a `.DS_Store` in
	/// every folder, and recording one root per folder makes every later filter scale with the
	/// directory count for nothing. Exact spelling, like the row key it asks about.
	pub(super) fn tracked(&self, rel_path: &str, is_dir: bool) -> bool {
		let Some(id) = self.resolve(rel_path) else {
			return false;
		};
		self.is_row(id) || (is_dir && !self.kids(id).is_empty())
	}

	/// How many rows stand in for both sides — what a carried side holds before this pass's own
	/// observations correct it (see [`BaselineEntry::carryable`]).
	pub(super) fn carryable_rows(&self) -> usize {
		// Checked in EVERY build. A `debug_assert` beside a `saturating_sub` would state the
		// invariant and then mask its violation exactly where it matters: this is a capacity hint,
		// so an undercount silently reallocates a pass's way up instead of failing. A plain
		// subtraction would be no better — this crate's release profile leaves overflow checks off,
		// so the underflow would wrap to a capacity no allocation can serve.
		assert!(
			self.uncarryable.len() <= self.rows,
			"every uncarryable id is a row: {} of {}",
			self.uncarryable.len(),
			self.rows
		);
		self.rows - self.uncarryable.len()
	}

	/// Whether the row at `rel_path` stands in for both sides.
	///
	/// Answered without building a row, which is the whole reason a carried side can say it holds a
	/// path it has no node for: rebuilding the row to ask would allocate its path and both its
	/// halves, per lookup.
	pub(super) fn carryable(&self, rel_path: &str) -> bool {
		self.resolve(rel_path)
			.is_some_and(|id| self.is_row(id) && !self.uncarryable.contains(&id))
	}

	/// The path of every `.filenignore` row, in no order.
	///
	/// From the index, not from a walk. A pass asks this before it reads either side, and the
	/// answer for almost every pair is one path or none — which is exactly the shape that must not
	/// cost a visit of every row in the tree.
	pub(super) fn rule_file_rows(&self) -> impl Iterator<Item = String> + '_ {
		self.rule_files.iter().map(|&id| self.path_of(id))
	}

	/// The path of every row that stands in for NEITHER side — what a change-scoped pass adds to
	/// its dirty set and its held set before it reads anything.
	///
	/// From the index, not from a walk. This is the list that would otherwise cost a pass one visit
	/// of every row in the tree just to discover that a converged pair has none.
	pub(super) fn uncarryable_paths(&self) -> impl Iterator<Item = String> + '_ {
		self.uncarryable.iter().map(|&id| self.path_of(id))
	}

	/// Whether any row at or under `rel_path`, under any spelling, satisfies `held` — the folded
	/// subtree question [`occupied`](Self::occupied) asks, with the CALLER deciding which rows
	/// count.
	///
	/// A carried side cannot use `occupied` itself: that counts every row, and every path a row
	/// merely sits under, where such a side holds only the rows it can carry and has this pass's own
	/// observations over them. So the walk is shared and the predicate is not.
	///
	/// ROWS ONLY, which is the half a caller must not forget: the predicate can subtract from this
	/// walk and never add to it. A side that also holds paths NO row tracks — every create this
	/// pass has seen and not yet written back — has to scan those itself first and use this for the
	/// row half. Asked alone, it answers "nothing there" for a destination holding a brand-new
	/// file, and the directory move that asked would land on top of it.
	pub(super) fn any_folded_row_at_or_under(
		&self,
		rel_path: &str,
		held: &mut impl FnMut(&str) -> bool,
	) -> bool {
		// `""` holds nothing, exactly as `occupied` answers for it.
		if rel_path.is_empty() {
			return false;
		}
		for id in self.folded_nodes(rel_path) {
			let path = self.path_of(id);
			if self.is_row(id) && held(&path) {
				return true;
			}
			// The walk yields the node's DESCENDANTS only, so the node itself is asked above.
			let mut walk = self.walk(id, path);
			while walk.next_row().is_some() {
				if held(&walk.path) {
					return true;
				}
			}
		}
		false
	}

	/// Whether every row STRICTLY under `rel_path` is `Synced` — the gate a directory move takes
	/// before it carries a subtree across as one move.
	pub(super) fn subtree_all_synced(&self, rel_path: &str) -> bool {
		let Some(id) = self.resolve(rel_path) else {
			return true;
		};
		self.walk(id, String::new())
			.all(|(child, _)| self.nodes[child.index()].state == BaselineState::Synced)
	}

	/// What this tree holds for the life of the pair, summed from the structures themselves.
	///
	/// Computed rather than read off the process: `getrusage`'s high-water mark is process-wide and
	/// every earlier phase of a probe run has already moved it, so it cannot answer "what does the
	/// resident baseline cost". Heap allocations are counted at the 16-byte granularity macOS and
	/// glibc both round small blocks to; the map figures use the tables' own capacity, which is
	/// where a `HashMap`'s slack lives.
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_bytes(&self) -> usize {
		self.resident_terms().total()
	}

	/// The same figure [`resident_bytes`](Self::resident_bytes) sums, TERM BY TERM.
	///
	/// Same convention, same arithmetic — `resident_bytes` is this summed, so a reader cannot come
	/// to disagree with the total it is set against. It exists because the sum alone cannot be
	/// acted on: "the resident tree is 211.6 MiB at a million rows" says nothing about which of
	/// nine structures to go after, and a hand estimate of the parts reached only ~175 of that
	/// figure — so the split was not known, and guessing at it is how three rounds of this effort
	/// came to optimise the wrong structure.
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_terms(&self) -> ResidentTerms {
		fn heap(bytes: usize) -> usize {
			bytes.div_ceil(16) * 16
		}
		fn table<K, V>(capacity: usize) -> usize {
			// Key, value and one control byte per slot — hashbrown's layout.
			capacity * (size_of::<K>() + size_of::<V>() + 1)
		}
		ResidentTerms {
			nodes: self.nodes.capacity() * size_of::<Node>(),
			names: heap(self.names.capacity()),
			free: heap(self.free.capacity() * size_of::<NodeId>()),
			children_vecs: self
				.children
				.values()
				.map(|kids| heap(kids.capacity() * size_of::<NodeId>()))
				.sum(),
			children_table: table::<NodeId, Vec<NodeId>>(self.children.capacity()),
			side: table::<NodeId, ConflictSides>(self.side.capacity()),
			agreed: table::<NodeId, Option<Blake3Hash>>(self.agreed.capacity()),
			uncarryable: table::<NodeId, ()>(self.uncarryable.capacity()),
			rule_files: table::<NodeId, ()>(self.rule_files.capacity()),
		}
	}

	fn walk(&self, root: NodeId, root_path: String) -> Walk<'_> {
		Walk {
			baseline: self,
			stack: vec![(root, 0, root_path.len())],
			path: root_path,
		}
	}

	fn entry_at(&self, id: NodeId, rel_path: String) -> BaselineEntry {
		let mut row = blank_row();
		row.rel_path = rel_path;
		self.fill_row(id, &mut row);
		row
	}

	/// Write the row at `id` over `row`, leaving its `rel_path` alone — the caller owns that.
	///
	/// Split out of [`entry_at`](Self::entry_at) so [`visit_rows`](Self::visit_rows) can refill one
	/// buffer per walk instead of building a row per node, with the field list still in one place: a
	/// second copy of it is how a new column comes to be carried by one reader and not the other.
	fn fill_row(&self, id: NodeId, row: &mut BaselineEntry) {
		let node = &self.nodes[id.index()];
		let sides = self.side.get(&id);
		let content_hash = node
			.has(HAS_CONTENT_HASH)
			.then(|| Blake3Hash::from(node.content_hash));
		row.kind = node.kind;
		row.remote_uuid = node.has(HAS_REMOTE_UUID).then_some(node.remote_uuid);
		row.content_hash = content_hash;
		row.size = node.has(HAS_SIZE).then_some(node.size);
		row.local_mtime = node.has(HAS_LOCAL_MTIME).then_some(node.local_mtime);
		row.remote_modified = node
			.has(HAS_REMOTE_MODIFIED)
			.then_some(node.remote_modified);
		row.state = node.state;
		row.local_kind = sides.and_then(|s| s.local_kind);
		row.remote_kind = sides.and_then(|s| s.remote_kind);
		row.remote_hash = sides.and_then(|s| s.remote_hash);
		row.remote_size = sides.and_then(|s| s.remote_size);
		row.remote_stable_uuid = node.stable_uuid;
		// Not in the map means "the same as this side's content", which is every confirmed row.
		row.agreed_hash = self.agreed.get(&id).copied().unwrap_or(content_hash);
	}

	/// Apply exactly what [`BaselineStore::write_changes`](super::baseline::BaselineStore) applied
	/// to the DB, in the same order.
	pub(super) fn apply(&mut self, changes: &[BaselineChange<'_>]) {
		for change in changes {
			match change {
				BaselineChange::Upsert(entry) => self.upsert(entry),
				BaselineChange::Delete(rel_path) => self.remove(rel_path),
				BaselineChange::MoveSubtree { from, to } => self.move_subtree(from, to),
			}
		}
	}

	/// Write one row, creating the path it sits on where no row does.
	pub(super) fn upsert(&mut self, entry: &BaselineEntry) {
		if entry.rel_path.is_empty() {
			// The root is not a row: the store's primary key includes the path, and every writer
			// names a real one. Nothing can be done with such a row but refuse it loudly.
			tracing::error!("baseline: refusing a row with an empty path");
			return;
		}
		let id = self.ensure(&entry.rel_path);
		self.clear_indexes(id);
		// Read before the node is overwritten: what this row recorded a moment ago is what says
		// whether `remote_rows` moves.
		let had_remote_uuid = self.nodes[id.index()].has(HAS_REMOTE_UUID);
		let node = &mut self.nodes[id.index()];
		if !node.has(PRESENT) {
			self.rows += 1;
		}
		let mut flags = PRESENT;
		node.kind = entry.kind;
		node.state = entry.state;
		node.stable_uuid = entry.remote_stable_uuid;
		if let Some(uuid) = entry.remote_uuid {
			flags |= HAS_REMOTE_UUID;
			node.remote_uuid = uuid;
		}
		if let Some(hash) = entry.content_hash {
			flags |= HAS_CONTENT_HASH;
			node.content_hash = *hash.as_ref();
		}
		if let Some(size) = entry.size {
			flags |= HAS_SIZE;
			node.size = size;
		}
		if let Some(mtime) = entry.local_mtime {
			flags |= HAS_LOCAL_MTIME;
			node.local_mtime = mtime;
		}
		if let Some(modified) = entry.remote_modified {
			flags |= HAS_REMOTE_MODIFIED;
			node.remote_modified = modified;
		}
		node.flags = flags;
		// Checked in EVERY build, like `carryable_rows`: this crate's release profile leaves
		// overflow checks off, so a decrement that should never run would wrap to a count no
		// tree could have and `has_remote_rows` would answer yes for a pair with no remote row
		// at all — which reads as a vanished remote and holds a pass's deletions back forever.
		let has_remote_uuid = flags & HAS_REMOTE_UUID != 0;
		if has_remote_uuid && !had_remote_uuid {
			self.remote_rows += 1;
		} else if had_remote_uuid && !has_remote_uuid {
			assert!(
				self.remote_rows > 0,
				"a row is losing a remote uuid the tree never counted: {:?}",
				entry.rel_path
			);
			self.remote_rows -= 1;
		}
		if let Some(sides) = ConflictSides::of(entry) {
			self.side.insert(id, sides);
		}
		if entry.agreed_hash != entry.content_hash {
			self.agreed.insert(id, entry.agreed_hash);
		}
		// Asked of the ROW the store just wrote, which is the only place the whole field list is in
		// hand. `clear_indexes` above has already taken the previous answer out.
		if !entry.carryable() {
			self.uncarryable.insert(id);
		}
		// The leaf name, not the path: a row's node carries only its own name, and that is the
		// whole of what makes a path a rule file (see `ignore::rule_file_dir`).
		if entry.kind == NodeKind::File && self.name(id) == FILENIGNORE {
			self.rule_files.insert(id);
		}
	}

	/// Drop the row at `rel_path`. The node stays only while something under it does.
	pub(super) fn remove(&mut self, rel_path: &str) {
		let Some(id) = self.resolve(rel_path) else {
			return;
		};
		self.clear_row(id);
		self.prune(id);
		self.maybe_compact_names();
	}

	/// Drop every row at or under each of `roots` — the ignore untracking, mirrored.
	pub(super) fn remove_subtrees(&mut self, roots: &BTreeSet<String>) {
		for root in roots {
			if root.is_empty() {
				// The root is not a row and its slot is not one to hand back: freeing it would put
				// node 0 on the free list, and the next row written would take that slot and make
				// the root its own child. Refused loudly, like a row with no path.
				tracing::error!("baseline: refusing to drop the pair root's subtree");
				continue;
			}
			let Some(id) = self.resolve(root) else {
				continue;
			};
			let under: Vec<NodeId> = self
				.walk(id, String::new())
				.map(|(child, _)| child)
				.collect();
			for child in under {
				self.clear_row(child);
			}
			self.clear_row(id);
			self.detach(id);
			let parent = self.nodes[id.index()].parent;
			self.free_subtree(id);
			self.prune(parent);
		}
		self.maybe_compact_names();
	}

	/// Re-key the row at `from` and everything under it to sit under `to`, overwriting whatever
	/// sits at each destination and leaving the rest of the destination's subtree where it is —
	/// what the store's two `UPDATE OR REPLACE` statements do.
	///
	/// Every source leaves before any destination is written, so a rename that only changes case
	/// (`A` -> `a`, which is one node in the tree and two rows in the store) cannot drop the row it
	/// has just written.
	pub(super) fn move_subtree(&mut self, from: &str, to: &str) {
		if from.is_empty() || to.is_empty() {
			// Neither end can be the pair root: it holds no row, and moving the whole tree onto or
			// out of it is a request no writer makes (see `remove_subtrees`).
			tracing::error!("baseline: refusing to move the pair root ({from:?} -> {to:?})");
			return;
		}
		let Some(id) = self.resolve(from) else {
			return;
		};
		let mut moving: Vec<BaselineEntry> = Vec::new();
		if self.is_row(id) {
			moving.push(self.entry_at(id, to.to_string()));
		}
		moving.extend(self.walk(id, String::new()).map(|(child, path)| {
			let mut entry = self.entry_at(child, String::new());
			entry.rel_path = format!("{to}/{path}");
			entry
		}));
		self.remove_subtrees(&BTreeSet::from([from.to_string()]));
		for entry in &moving {
			self.upsert(entry);
		}
	}

	/// Advance the agreed-content marker of the row at `rel_path`, and hand back the row as it now
	/// stands. `None` when nothing is there.
	///
	pub(super) fn set_agreed(
		&mut self,
		rel_path: &str,
		agreed_hash: Option<Blake3Hash>,
	) -> Option<BaselineEntry> {
		let id = self.resolve(rel_path)?;
		if !self.is_row(id) {
			return None;
		}
		let content_hash = self.nodes[id.index()]
			.has(HAS_CONTENT_HASH)
			.then(|| Blake3Hash::from(self.nodes[id.index()].content_hash));
		if agreed_hash == content_hash {
			self.agreed.remove(&id);
		} else {
			self.agreed.insert(id, agreed_hash);
		}
		Some(self.entry_at(id, rel_path.to_string()))
	}

	/// The node at `rel_path`, creating it — and every path component above it that has no node
	/// yet — where it does not exist.
	fn ensure(&mut self, rel_path: &str) -> NodeId {
		let mut at = NodeId::ROOT;
		for part in rel_path.split('/') {
			at = match self.child(at, part) {
				Some(id) => id,
				None => self.insert_node(at, part),
			};
		}
		at
	}

	fn insert_node(&mut self, parent: NodeId, name: &str) -> NodeId {
		let mut node = Node::empty(parent);
		(node.name_at, node.name_len) = self.push_name(name);
		let id = match self.free.pop() {
			Some(id) => {
				self.nodes[id.index()] = node;
				id
			}
			None => {
				self.nodes.push(node);
				NodeId::from_index(self.nodes.len() - 1)
			}
		};
		// Three DISJOINT fields borrowed at once: the child list is grown while the arena and the
		// node array say what each sibling is called. Spelled out as field accesses for exactly
		// that reason — `self.name(other)` borrows the whole of `self`, which the entry below
		// already holds mutably.
		let (names, nodes) = (&self.names, &self.nodes);
		let kids = self.children.entry(parent).or_default();
		let at = kids
			.binary_search_by(|&other| sibling_cmp(leaf_name(names, &nodes[other.index()]), name))
			.unwrap_or_else(|at| at);
		kids.insert(at, id);
		id
	}

	/// Append `name` to the arena and say where it landed.
	///
	/// The cap is the arena's, not one name's: a `u32` offset puts 4 GiB of leaf names out of
	/// reach, which at the ~15 bytes a real name costs is a pair of a quarter of a billion rows —
	/// well past the point [`NodeId::from_index`]'s own cap and the disk have given up.
	fn push_name(&mut self, name: &str) -> (u32, u32) {
		let at = u32::try_from(self.names.len())
			.expect("a sync pair's leaf names come to under 4 GiB in total");
		let len = u32::try_from(name.len()).expect("a leaf name is under 4 GiB");
		self.names.push_str(name);
		(at, len)
	}

	/// Hand `id`'s slot back, and write off its name with it.
	///
	/// The bytes stay in the arena — nothing can be taken out of the middle of it — and are counted
	/// dead so that [`maybe_compact_names`](Self::maybe_compact_names) can weigh them.
	fn free_node(&mut self, id: NodeId) {
		self.dead_names += self.nodes[id.index()].name_len as usize;
		self.free.push(id);
	}

	/// Rebuild the name arena once most of it is names no node points at.
	///
	/// A rename is a remove and an insert, so a pair that renames all day appends a name for every
	/// one it orphans: without this the arena is the one structure here sized by the pair's
	/// HISTORY rather than by its tree, and a long-lived pair would grow without bound.
	///
	/// Amortised O(1) a name, by construction: a rebuild costs one walk of the live nodes and can
	/// only run once the dead bytes have caught up with the live ones, which takes as much
	/// removing again as the walk costs. The floor keeps a small tree from walking itself at all —
	/// half of a 64 KiB arena is not worth the visit, and a pair whose names never reach it never
	/// compacts.
	fn maybe_compact_names(&mut self) {
		/// Below this the arena is not worth a walk, however much of it is dead.
		const FLOOR: usize = 64 * 1024;
		// In EVERY build: this crate's release profile leaves overflow checks off, so a dead count
		// that had run past the arena would wrap the live figure below to something enormous and
		// this would never compact again — the unbounded growth it exists to stop, arrived at
		// silently.
		assert!(
			self.dead_names <= self.names.len(),
			"more of the name arena is written off ({}) than was ever written ({})",
			self.dead_names,
			self.names.len()
		);
		let live = self.names.len() - self.dead_names;
		if self.dead_names < FLOOR || self.dead_names < live {
			return;
		}
		let mut arena = String::with_capacity(live);
		// Every live node is reachable from the root through `children`: a node is only ever
		// created into its parent's list, and one taken out of it is freed in the same breath.
		let mut stack = vec![NodeId::ROOT];
		while let Some(id) = stack.pop() {
			let moved_to =
				u32::try_from(arena.len()).expect("the live names fit where they already fit");
			arena.push_str(leaf_name(&self.names, &self.nodes[id.index()]));
			self.nodes[id.index()].name_at = moved_to;
			if let Some(kids) = self.children.get(&id) {
				stack.extend(kids);
			}
		}
		// In EVERY build, for the same reason the dead count above is checked in every build: a
		// live node the walk did not reach keeps its old offset into an arena that no longer holds
		// it, and `name()` then slices some other node's bytes out as this node's leaf name — a
		// silently wrong path, which is the worst thing this tree can produce. One comparison per
		// compaction, against a compaction that is amortised O(1) a name.
		assert_eq!(
			arena.len(),
			live,
			"the walk reached every live name and no other"
		);
		self.names = arena;
		self.dead_names = 0;
	}

	/// Take the row off `id`, leaving the node standing for whatever sits under it.
	fn clear_row(&mut self, id: NodeId) {
		if !self.is_row(id) {
			return;
		}
		self.clear_indexes(id);
		if self.nodes[id.index()].has(HAS_REMOTE_UUID) {
			// In every build, for the reason `upsert` gives.
			assert!(
				self.remote_rows > 0,
				"a row recording a remote item was never counted as one"
			);
			self.remote_rows -= 1;
		}
		self.nodes[id.index()].flags = 0;
		self.rows -= 1;
	}

	/// Take `id` out of every index that names it.
	fn clear_indexes(&mut self, id: NodeId) {
		self.side.remove(&id);
		self.agreed.remove(&id);
		self.uncarryable.remove(&id);
		self.rule_files.remove(&id);
	}

	/// Drop `id` and every ancestor of it that is left holding neither a row nor a child.
	fn prune(&mut self, id: NodeId) {
		let mut at = id;
		while at != NodeId::ROOT && !self.is_row(at) && self.kids(at).is_empty() {
			let parent = self.nodes[at.index()].parent;
			self.detach(at);
			self.children.remove(&at);
			self.free_node(at);
			at = parent;
		}
	}

	/// Take `id` out of its parent's child list.
	fn detach(&mut self, id: NodeId) {
		let parent = self.nodes[id.index()].parent;
		let Some(kids) = self.children.get_mut(&parent) else {
			return;
		};
		if let Some(at) = kids.iter().position(|&other| other == id) {
			kids.remove(at);
		}
		if kids.is_empty() {
			self.children.remove(&parent);
		}
	}

	/// Hand `id`'s slot and every slot under it back, the row half already cleared.
	fn free_subtree(&mut self, id: NodeId) {
		let mut stack = vec![id];
		while let Some(at) = stack.pop() {
			if let Some(kids) = self.children.remove(&at) {
				stack.extend(kids);
			}
			self.free_node(at);
		}
	}
}

/// A lookup that remembers the directory it last resolved, for a caller that asks about paths in
/// tree order — the local scan, which walks a directory at a time, and the reconcile, which reads
/// its keys sorted. Resolving `a/b/c/x.txt` from the root is a binary search per level, each one
/// jumping to a random node of the arena; from the directory already in hand it is one.
///
/// A plain memo, not a cache: it holds the directory it was last asked for and nothing else, so it
/// cannot go stale within the borrow it holds (the baseline is immutable for as long as it lives)
/// and a miss costs one full resolve, exactly what every lookup used to cost.
pub(super) struct Cursor<'a> {
	baseline: &'a Tree,
	/// The parent path `at` stands for; `""` is the pair root, which `at` starts on.
	dir: String,
	at: NodeId,
}

impl Cursor<'_> {
	/// The row at `rel_path`, as [`Tree::get`] gives it.
	pub(super) fn get(&mut self, rel_path: &str) -> Option<BaselineEntry> {
		let (parent, name) = rel_path
			.rsplit_once('/')
			.map_or(("", rel_path), |(parent, name)| (parent, name));
		if parent != self.dir {
			self.at = self.baseline.resolve(parent)?;
			self.dir.clear();
			self.dir.push_str(parent);
		}
		let id = self.baseline.child(self.at, name)?;
		self.baseline
			.is_row(id)
			.then(|| self.baseline.entry_at(id, rel_path.to_string()))
	}
}

/// Every row under one node, parent before child, with each row's path built as the walk descends
/// (one `String` per row rather than one per node of the tree).
struct Walk<'a> {
	baseline: &'a Tree,
	/// `(node, how many of its children have been visited, how long the path was before its name)`.
	stack: Vec<(NodeId, usize, usize)>,
	path: String,
}

impl Walk<'_> {
	/// Advance to the next row, leaving its path in [`path`](Self::path): the borrowing form of
	/// [`Iterator::next`], which clones that path out. One buffer for the whole walk rather than a
	/// `String` per row, which is what lets a caller ask a question about every path for free.
	fn next_row(&mut self) -> Option<NodeId> {
		loop {
			let &(id, visited, _) = self.stack.last()?;
			let kids = self.baseline.kids(id);
			if visited >= kids.len() {
				let (_, _, before) = self.stack.pop().expect("the frame was just read");
				if self.stack.is_empty() {
					return None;
				}
				self.path.truncate(before);
				continue;
			}
			let child = kids[visited];
			self.stack.last_mut().expect("the frame was just read").1 += 1;
			let before = self.path.len();
			if !self.path.is_empty() {
				self.path.push('/');
			}
			self.path.push_str(self.baseline.name(child));
			self.stack.push((child, 0, before));
			if self.baseline.is_row(child) {
				return Some(child);
			}
		}
	}
}

impl Iterator for Walk<'_> {
	type Item = (NodeId, String);

	fn next(&mut self) -> Option<Self::Item> {
		let id = self.next_row()?;
		Some((id, self.path.clone()))
	}
}

#[cfg(test)]
mod tests;
