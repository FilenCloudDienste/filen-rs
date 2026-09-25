//! The resident baseline: one pair's rows as an id-keyed tree, held between passes.
//!
//! The rows a pass reconciles against used to be a `HashMap<String, BaselineEntry>` — 808 bytes a
//! row measured, every path materialized and stored whole, and a second copy of it in every
//! per-pass structure keyed the same way. This holds the same rows as a `Vec<Node>` addressed by a
//! [`NodeId`], where each node carries its LEAF NAME and its parent's id and nothing else about
//! where it sits. A path is walked out of the tree only where one is actually needed — the dirty
//! set, the report, a log line.
//!
//! What that buys, besides the bytes: the questions a pass asks about a SUBTREE stop being scans of
//! every row. "Is anything at or under this path, under any spelling" ([`Baseline::occupied`]),
//! "does the baseline still track anything here" ([`Baseline::tracked`]), "is every row under this
//! directory synced" ([`Baseline::subtree_all_synced`]) and "where is the row with this uuid"
//! ([`Baseline::path_by_uuid`]) are all answered from the node's own children or from an index.
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
//! # A path with no row of its own
//!
//! A row can sit under a directory that has no row (a destination-only item adopted at a mode
//! switch, a subtree whose directory rows were untracked). Such a path becomes a node with
//! [`PRESENT`] clear: it holds its children and nothing else. It is not a row — it is not counted
//! by [`Baseline::len`], never yielded by an iterator, and never answers [`Baseline::get`] — and it
//! is pruned the moment its last child leaves, so "this node has children" and "this node has a row
//! under it" mean the same thing.

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

/// A node's index in [`Baseline::nodes`]. The root is [`NodeId::ROOT`] and is never a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(super) struct NodeId(u32);

impl NodeId {
	/// The pair root: the one node with no name and no row, whose children are the top-level paths.
	pub(super) const ROOT: Self = Self(0);

	fn index(self) -> usize {
		self.0 as usize
	}

	/// A `u32` id is what makes the node 112 bytes rather than 120; a pair with four billion rows
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
	/// The leaf name — NFC, exactly as the row's path spells it. Never the path.
	name: Box<str>,
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

impl Node {
	fn empty(parent: NodeId, name: &str) -> Self {
		Self {
			parent,
			name: name.into(),
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

/// Whether `path` IS `prefix`, or lies under it, folded the way [`fold_cmp`] folds — and, like it,
/// without materializing either folded form.
///
/// The same question [`Baseline::occupied`] answers from a node's children, asked of a map that has
/// no such order: the two side maps a directory move checks its destination against. Folding each
/// key into a [`collision_key`](super::scan::collision_key) first allocates a `String` for every
/// key of the whole tree, to learn — for all but a handful of them — that the very first character
/// already differs. This stops at that character.
pub(super) fn at_or_under_folded(path: &str, prefix: &str) -> bool {
	// Same fast path, and same argument for it, as `fold_cmp`: on ASCII the two foldings are the
	// same answer rather than an approximation.
	if path.is_ascii() && prefix.is_ascii() {
		let (path, prefix) = (path.as_bytes(), prefix.as_bytes());
		return path.len() >= prefix.len()
			&& path[..prefix.len()].eq_ignore_ascii_case(prefix)
			&& path.get(prefix.len()).is_none_or(|&byte| byte == b'/');
	}
	// Comparing the two folded CHARACTER streams is comparing the two folded strings: a lowercase
	// expansion never yields `/`, so the separator can only be matched by a real one, and a prefix
	// that runs out mid-expansion leaves a character that is not `/` and is refused here.
	let mut folded = path.chars().flat_map(char::to_lowercase);
	for want in prefix.chars().flat_map(char::to_lowercase) {
		if folded.next() != Some(want) {
			return false;
		}
	}
	matches!(folded.next(), None | Some('/'))
}

/// The order a directory's children are kept in: by folded name, then by the raw one. The store's
/// primary key compares bytewise, so `A` and `a` are two rows of one directory; the tie-break is
/// what keeps both of them addressable.
fn sibling_cmp(a: &str, b: &str) -> Ordering {
	fold_cmp(a, b).then_with(|| a.cmp(b))
}

/// A row with nothing set and no path: the buffer [`Baseline::visit_rows`] refills, and the value
/// [`Baseline::fill_row`] writes every field of.
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
pub(super) struct Baseline {
	nodes: Vec<Node>,
	/// Slots whose node was removed, handed back out before the `Vec` grows again.
	free: Vec<NodeId>,
	/// Each directory's children, sorted by [`sibling_cmp`]. A node with no entry here has none.
	children: HashMap<NodeId, Vec<NodeId>>,
	by_uuid: HashMap<Uuid, NodeId>,
	by_lineage: HashMap<StableUuid, NodeId>,
	/// Conflicted rows only.
	side: HashMap<NodeId, ConflictSides>,
	/// Rows whose `agreed_hash` is NOT their `content_hash` — the shape an unconfirmed push leaves
	/// (see [`Baseline::awaits_confirmation`]). A row that is not here agrees with itself, which is
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
}

impl Default for Baseline {
	fn default() -> Self {
		Self {
			nodes: vec![Node::empty(NodeId::ROOT, "")],
			free: Vec::new(),
			children: HashMap::new(),
			by_uuid: HashMap::new(),
			by_lineage: HashMap::new(),
			side: HashMap::new(),
			agreed: HashMap::new(),
			uncarryable: HashSet::new(),
			rule_files: HashSet::new(),
			rows: 0,
		}
	}
}

/// What [`Baseline::resident_bytes`] is made of, one field per structure it counts.
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
	/// Every leaf name's heap allocation. Not the paths — no node holds one.
	pub(super) names: usize,
	/// The removed-slot free list.
	pub(super) free: usize,
	/// Every directory's children vector.
	pub(super) children_vecs: usize,
	/// The table those vectors are filed in, which is a separate cost from the vectors.
	pub(super) children_table: usize,
	pub(super) by_uuid: usize,
	pub(super) by_lineage: usize,
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
	pub(super) fn named(&self) -> [(&'static str, usize); 11] {
		[
			("nodes", self.nodes),
			("names", self.names),
			("free", self.free),
			("children_vecs", self.children_vecs),
			("children_table", self.children_table),
			("by_uuid", self.by_uuid),
			("by_lineage", self.by_lineage),
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

impl Baseline {
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
		self.by_uuid.shrink_to_fit();
		self.by_lineage.shrink_to_fit();
		self.side.shrink_to_fit();
		self.agreed.shrink_to_fit();
		self.uncarryable.shrink_to_fit();
		self.rule_files.shrink_to_fit();
	}

	/// How many rows the pair tracks.
	pub(super) fn len(&self) -> usize {
		self.rows
	}

	pub(super) fn is_empty(&self) -> bool {
		self.rows == 0
	}

	fn name(&self, id: NodeId) -> &str {
		&self.nodes[id.index()].name
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
	/// sides last agreed on (see [`Baseline::agreed`]). The whole map is the candidate set, so a
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
		!self.by_uuid.is_empty()
	}

	/// Where the row recording `uuid` sits.
	pub(super) fn path_by_uuid(&self, uuid: Uuid) -> Option<String> {
		self.by_uuid.get(&uuid).map(|&id| self.path_of(id))
	}

	/// Where the row recording the file with this whole-life id sits.
	pub(super) fn path_by_lineage(&self, lineage: StableUuid) -> Option<String> {
		self.by_lineage.get(&lineage).map(|&id| self.path_of(id))
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
	/// eleven structures to go after, and a hand estimate of the parts reached only ~175 of that
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
			names: self.nodes.iter().map(|node| heap(node.name.len())).sum(),
			free: heap(self.free.capacity() * size_of::<NodeId>()),
			children_vecs: self
				.children
				.values()
				.map(|kids| heap(kids.capacity() * size_of::<NodeId>()))
				.sum(),
			children_table: table::<NodeId, Vec<NodeId>>(self.children.capacity()),
			by_uuid: table::<Uuid, NodeId>(self.by_uuid.capacity()),
			by_lineage: table::<StableUuid, NodeId>(self.by_lineage.capacity()),
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
		// Each index answers for ONE row. The store constrains neither id, and a second row
		// claiming one leaves the first answering nothing as soon as the claimant is rewritten —
		// where the per-pass map these replaced would still have found it, and a path whose
		// deletion should have been withheld would not be. No writer produces that (a move deletes
		// the old row before it writes the new one), so a displaced claim is a bug, and `insert`
		// hands the previous one back anyway — saying so costs nothing.
		if let Some(uuid) = entry.remote_uuid
			&& let Some(prior) = self.by_uuid.insert(uuid, id)
			&& prior != id
		{
			tracing::error!(
				"baseline: {:?} claims remote uuid {uuid}, recorded at {:?}",
				entry.rel_path,
				self.path_of(prior)
			);
		}
		if let Some(lineage) = entry.remote_stable_uuid
			&& let Some(prior) = self.by_lineage.insert(lineage, id)
			&& prior != id
		{
			tracing::error!(
				"baseline: {:?} claims the whole-life id already recorded at {:?}",
				entry.rel_path,
				self.path_of(prior)
			);
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
		let node = Node::empty(parent, name);
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
		let kids = self.children.entry(parent).or_default();
		let at = kids
			.binary_search_by(|&other| sibling_cmp(&self.nodes[other.index()].name, name))
			.unwrap_or_else(|at| at);
		kids.insert(at, id);
		id
	}

	/// Take the row off `id`, leaving the node standing for whatever sits under it.
	fn clear_row(&mut self, id: NodeId) {
		if !self.is_row(id) {
			return;
		}
		self.clear_indexes(id);
		self.nodes[id.index()].flags = 0;
		self.rows -= 1;
	}

	/// Take `id` out of every index that names it — the uuid and lineage entries only when they
	/// still point AT it, since a later row may have taken the id over.
	fn clear_indexes(&mut self, id: NodeId) {
		let node = &self.nodes[id.index()];
		if node.has(HAS_REMOTE_UUID) {
			let uuid = node.remote_uuid;
			if self.by_uuid.get(&uuid) == Some(&id) {
				self.by_uuid.remove(&uuid);
			}
		}
		if let Some(lineage) = self.nodes[id.index()].stable_uuid
			&& self.by_lineage.get(&lineage) == Some(&id)
		{
			self.by_lineage.remove(&lineage);
		}
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
			self.free.push(at);
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
			self.free.push(at);
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
	baseline: &'a Baseline,
	/// The parent path `at` stands for; `""` is the pair root, which `at` starts on.
	dir: String,
	at: NodeId,
}

impl Cursor<'_> {
	/// The row at `rel_path`, as [`Baseline::get`] gives it.
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
	baseline: &'a Baseline,
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
mod tests {
	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use super::{
		super::{baseline::BaselineChange, plan::is_under, scan::collision_key},
		*,
	};

	/// The index's answer and the row's must be the same answer, for every row the tree holds.
	///
	/// The only place the two halves of the contract meet. [`Baseline::carryable`] reads the
	/// `uncarryable` set, recorded from the entry a caller handed [`Baseline::upsert`], while
	/// `derive::carried` asks [`BaselineEntry::carryable`] of the row [`Baseline::fill_row`]
	/// rebuilds out of the node's flag bits. They agree only while the store round-trips every
	/// field the rule reads: narrow one column or drop one flag and the index would call a row
	/// carryable that derives nothing — a path in neither map and in neither `dirty` nor `held`,
	/// which is a pass planning over an agreement nobody recorded.
	fn index_agrees_with_every_row(tree: &Baseline) {
		let mut rows = Vec::new();
		tree.visit_rows(|row| rows.push(row.clone()));
		for row in rows {
			assert_eq!(
				tree.carryable(&row.rel_path),
				row.carryable(),
				"{}: the index and the row the store rebuilds disagree",
				row.rel_path
			);
		}
	}

	/// Every term of [`ResidentTerms`] is in the total, and the total is what
	/// [`Baseline::resident_bytes`] answers.
	///
	/// The struct is destructured EXHAUSTIVELY here on purpose: a term added to it and left out of
	/// [`ResidentTerms::named`] would otherwise be missing from every total silently, and a
	/// resident figure that quietly stopped counting a structure is exactly the kind of number this
	/// round exists to stop anyone quoting. A new field fails this at COMPILE time, before it can
	/// fail an assertion.
	#[cfg(feature = "bench-internals")]
	#[test]
	fn every_resident_term_is_in_the_total() {
		let tree = Baseline::from_rows([
			file("docs/a.txt", Uuid::from_u128(1), [1; 32]),
			file("docs/b.txt", Uuid::from_u128(2), [2; 32]),
			dir("docs"),
		]);
		let terms = tree.resident_terms();
		let ResidentTerms {
			nodes,
			names,
			free,
			children_vecs,
			children_table,
			by_uuid,
			by_lineage,
			side,
			agreed,
			uncarryable,
			rule_files,
		} = terms;
		let by_hand = nodes
			+ names + free
			+ children_vecs
			+ children_table
			+ by_uuid + by_lineage
			+ side + agreed
			+ uncarryable
			+ rule_files;
		assert_eq!(
			by_hand,
			terms.total(),
			"a term of the resident tree is not in the total: {:?}",
			terms
		);
		assert_eq!(
			terms.total(),
			tree.resident_bytes(),
			"the split and the sum it is set against disagree"
		);
		// The terms a loaded tree must charge something for, so the split cannot be all zeroes and
		// still pass. `side` and `agreed` are deliberately absent: a converged pair has neither a
		// conflict nor an unconfirmed push, which is the steady state being accounted for.
		for (what, bytes) in [
			("nodes", nodes),
			("names", names),
			("children_vecs", children_vecs),
			("children_table", children_table),
			("by_uuid", by_uuid),
			("by_lineage", by_lineage),
		] {
			assert!(
				bytes > 0,
				"{what} costs nothing on a tree holding three rows"
			);
		}
	}

	/// The uncarryable index is what a change-scoped pass reads INSTEAD of walking the tree, so it
	/// has to follow every write path: a row rewritten one-sided joins it, one rewritten whole
	/// leaves it, a moved subtree takes its entries along, and a removed row stops answering for a
	/// path that no longer has one. An index that fell behind on any of those would leave a pass
	/// deriving both sides from a row that describes neither.
	#[test]
	fn the_uncarryable_index_follows_every_write_path() {
		let uuid = Uuid::from_u128(9);
		let whole = file("docs/a.txt", uuid, [1; 32]);
		let one_sided = BaselineEntry {
			state: BaselineState::Conflicted,
			..whole.clone()
		};

		let mut tree = Baseline::from_rows([whole.clone()]);
		assert_eq!(tree.carryable_rows(), 1);
		assert!(tree.carryable("docs/a.txt"));
		assert_eq!(tree.uncarryable_paths().count(), 0);
		assert!(
			!tree.carryable("docs"),
			"a path with no row of its own carries nothing"
		);
		assert!(!tree.carryable("docs/missing.txt"));
		index_agrees_with_every_row(&tree);

		// Rewritten one-sided: indexed, and carryable no longer.
		tree.upsert(&one_sided);
		assert_eq!(tree.carryable_rows(), 0);
		assert!(!tree.carryable("docs/a.txt"));
		assert_eq!(
			tree.uncarryable_paths().collect::<Vec<_>>(),
			vec!["docs/a.txt".to_string()]
		);
		index_agrees_with_every_row(&tree);

		// A move re-keys it with the row. Without this a pass would re-observe a path nothing is
		// keyed by any more, and carry the row that moved.
		tree.move_subtree("docs", "moved");
		assert_eq!(
			tree.uncarryable_paths().collect::<Vec<_>>(),
			vec!["moved/a.txt".to_string()]
		);
		assert!(!tree.carryable("moved/a.txt"));
		index_agrees_with_every_row(&tree);

		// Rewritten whole: out of the index again.
		tree.upsert(&BaselineEntry {
			rel_path: "moved/a.txt".to_string(),
			..whole.clone()
		});
		assert_eq!(tree.carryable_rows(), 1);
		assert_eq!(tree.uncarryable_paths().count(), 0);
		assert!(tree.carryable("moved/a.txt"));
		index_agrees_with_every_row(&tree);

		// A removed row leaves the index rather than answering for a path with no row.
		tree.upsert(&BaselineEntry {
			rel_path: "moved/b.txt".to_string(),
			state: BaselineState::Adopted,
			..whole.clone()
		});
		assert_eq!(
			tree.uncarryable_paths().collect::<Vec<_>>(),
			vec!["moved/b.txt".to_string()]
		);
		assert_eq!(tree.carryable_rows(), 1);
		tree.remove("moved/b.txt");
		assert_eq!(tree.uncarryable_paths().count(), 0);
		assert_eq!(tree.carryable_rows(), 1);
		index_agrees_with_every_row(&tree);

		// And a subtree removal takes its entries with it.
		tree.upsert(&BaselineEntry {
			rel_path: "moved/deep/c.txt".to_string(),
			state: BaselineState::Adopted,
			..whole.clone()
		});
		assert_eq!(tree.uncarryable_paths().count(), 1);
		tree.remove_subtrees(&BTreeSet::from(["moved/deep".to_string()]));
		assert_eq!(tree.uncarryable_paths().count(), 0);
		assert_eq!(tree.carryable_rows(), 1);
		index_agrees_with_every_row(&tree);
	}

	/// The folded subtree walk answers the question `occupied` asks, with the caller saying which
	/// rows count — so a carried side can ask it about rows the pass has observed away without the
	/// walk knowing anything about observations.
	#[test]
	fn the_folded_subtree_walk_lets_the_caller_choose_which_rows_count() {
		let tree = Baseline::from_rows([
			file("Docs/a.txt", Uuid::from_u128(1), [1; 32]),
			file("notes", Uuid::from_u128(2), [2; 32]),
		]);
		let mut anything = |_: &str| true;

		assert!(
			tree.any_folded_row_at_or_under("docs", &mut anything),
			"a directory no row is keyed by is still occupied by what sits under it, folded"
		);
		assert!(tree.any_folded_row_at_or_under("DOCS/A.TXT", &mut anything));
		assert!(tree.any_folded_row_at_or_under("notes", &mut anything));
		assert!(
			!tree.any_folded_row_at_or_under("note", &mut anything),
			"a path a key merely starts with is not occupied"
		);
		assert!(!tree.any_folded_row_at_or_under("docs2", &mut anything));
		assert!(
			!tree.any_folded_row_at_or_under("", &mut anything),
			"the pair root holds nothing, as `occupied` answers for it"
		);

		// The predicate is the point: refusing the one row under `docs` empties it.
		let mut nothing = |_: &str| false;
		assert!(!tree.any_folded_row_at_or_under("docs", &mut nothing));
		let mut only_notes = |path: &str| path == "notes";
		assert!(!tree.any_folded_row_at_or_under("docs", &mut only_notes));
		assert!(tree.any_folded_row_at_or_under("NOTES", &mut only_notes));
	}

	fn file(rel_path: &str, uuid: Uuid, hash: [u8; 32]) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(Blake3Hash::from(hash)),
			size: Some(7),
			local_mtime: Some(12),
			remote_modified: Some(34),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: Some(Blake3Hash::from(hash)),
		}
	}

	fn dir(rel_path: &str) -> BaselineEntry {
		BaselineEntry {
			kind: NodeKind::Dir,
			content_hash: None,
			size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
			..file(rel_path, Uuid::new_v4(), [0; 32])
		}
	}

	fn paths(baseline: &Baseline) -> Vec<String> {
		let mut all: Vec<String> = baseline.paths().collect();
		all.sort();
		all
	}

	/// The comparator decides which names are the SAME name for the engine, so it has to agree with
	/// [`collision_key`] exactly — including the non-ASCII cases that key exists for, which
	/// globset's own case-insensitive matching cannot fold.
	#[test]
	fn the_folded_comparator_agrees_with_the_collision_key() {
		let pairs = [
			("report.txt", "Report.TXT"),
			("Ärger.txt", "ärger.txt"),
			("ÅNGSTRÖM", "ångström"),
			("ΣΊΣΥΦΟΣ", "σίσυφοσ"),
			("ЖУРНАЛ", "журнал"),
			("İ", "i\u{307}"),
			("a", "b"),
			("ä", "b"),
			("z", "ä"),
		];
		for (a, b) in pairs {
			assert_eq!(
				fold_cmp(a, b) == Ordering::Equal,
				collision_key(a) == collision_key(b),
				"{a:?} vs {b:?}"
			);
			assert_eq!(
				fold_cmp(a, b),
				collision_key(a).cmp(&collision_key(b)),
				"{a:?} vs {b:?} order"
			);
		}
	}

	/// The streaming at-or-under test answers what folding both paths and asking [`is_under`]
	/// answers — the form it replaces — including where a lowercase expansion makes the folded
	/// path longer than the raw one, which is the case a byte-wise prefix test gets wrong.
	#[test]
	fn the_folded_at_or_under_test_agrees_with_folding_both_paths() {
		let cases = [
			("Docs", "docs"),
			("docs", "Docs"),
			("docs/a.txt", "Docs"),
			("Docs/sub/a.txt", "docs/SUB"),
			("docsx", "docs"),
			("docs", "docs/a"),
			("docs2/a", "docs"),
			("ÄÖ/x", "äö"),
			("ÄÖx", "äö"),
			("İ/x", "İ"),
			("İ/x", "i"),
			("İ", "i"),
			("ΣΊΣΥΦΟΣ/x", "σίσυφοσ"),
			("a", ""),
			("", ""),
		];
		for (path, prefix) in cases {
			let (folded_path, folded_prefix) = (collision_key(path), collision_key(prefix));
			assert_eq!(
				at_or_under_folded(path, prefix),
				folded_path == folded_prefix || is_under(&folded_path, &folded_prefix),
				"{path:?} at or under {prefix:?}"
			);
		}
	}

	/// A row goes in and comes back out whole: every field, including the halves kept beside the
	/// nodes (the conflict sides and an agreed marker that differs from this side's content).
	#[test]
	fn a_row_round_trips_with_full_fidelity() {
		let uuid = Uuid::new_v4();
		let held = BaselineEntry {
			state: BaselineState::Conflicted,
			local_kind: Some(NodeKind::File),
			remote_kind: Some(NodeKind::Dir),
			remote_hash: Some(Blake3Hash::from([9; 32])),
			remote_size: Some(42),
			agreed_hash: Some(Blake3Hash::from([3; 32])),
			..file("a/b/c.txt", uuid, [1; 32])
		};
		let unconfirmed = BaselineEntry {
			agreed_hash: None,
			..file("a/pushed.bin", Uuid::new_v4(), [2; 32])
		};
		let plain = dir("a");
		let baseline = Baseline::from_rows([held.clone(), unconfirmed.clone(), plain.clone()]);

		assert_eq!(baseline.get("a/b/c.txt"), Some(held));
		assert_eq!(baseline.get("a/pushed.bin"), Some(unconfirmed));
		assert_eq!(baseline.get("a"), Some(plain));
		assert_eq!(baseline.get("a/b"), None, "a path with no row of its own");
		assert_eq!(baseline.len(), 3, "the path-only node is not a row");
		assert_eq!(paths(&baseline), vec!["a", "a/b/c.txt", "a/pushed.bin"]);
	}

	/// The indexes name the row that holds the id NOW: a row rewritten with another uuid, a row
	/// deleted, and a row moved all have to leave the index describing where things are.
	#[test]
	fn the_uuid_and_lineage_indexes_follow_the_rows() {
		let (first, second) = (Uuid::new_v4(), Uuid::new_v4());
		let held_by_the_dir = Uuid::new_v4();
		let mut baseline = Baseline::from_rows([
			BaselineEntry {
				remote_uuid: Some(held_by_the_dir),
				..dir("d")
			},
			file("d/x.txt", first, [1; 32]),
		]);
		assert_eq!(baseline.path_by_uuid(first).as_deref(), Some("d/x.txt"));
		assert_eq!(
			baseline
				.path_by_lineage(StableUuid::new_for_test(first))
				.as_deref(),
			Some("d/x.txt")
		);

		// A new version at the same path: the old uuid names nothing any more.
		baseline.upsert(&file("d/x.txt", second, [2; 32]));
		assert_eq!(baseline.path_by_uuid(first), None);
		assert_eq!(baseline.path_by_uuid(second).as_deref(), Some("d/x.txt"));

		baseline.move_subtree("d", "e");
		assert_eq!(baseline.path_by_uuid(second).as_deref(), Some("e/x.txt"));

		baseline.remove("e/x.txt");
		assert_eq!(baseline.path_by_uuid(second), None);
		// The row that is still there still answers, so the index did not simply empty itself: the
		// directory's own claim was re-keyed by the move above.
		assert!(baseline.has_remote_rows());
		assert_eq!(baseline.path_by_uuid(held_by_the_dir).as_deref(), Some("e"));
	}

	/// `occupied` is the server's own name dedup asked of the baseline: any spelling, at the path or
	/// under it. `""` holds nothing, exactly as the path-keyed form it replaces answered.
	#[test]
	fn occupied_finds_any_spelling_at_or_under_a_path() {
		let baseline = Baseline::from_rows([
			dir("Photos"),
			file("Photos/Ärger.txt", Uuid::new_v4(), [1; 32]),
			dir("empty_parent/inner"),
		]);

		assert!(baseline.occupied("photos"));
		assert!(baseline.occupied("PHOTOS/ärger.txt"));
		assert!(
			baseline.occupied("EMPTY_PARENT"),
			"a path-only node with a row under it"
		);
		assert!(!baseline.occupied("photos/other.txt"));
		assert!(!baseline.occupied("pho"));
		assert!(
			!baseline.occupied(""),
			"nothing is at or under the pair root itself"
		);
	}

	/// The ignore probe is EXACT-spelling, unlike `occupied`: it asks whether the rows a rule is
	/// about to untrack exist, and those are keyed the way the store keys them.
	#[test]
	fn tracked_asks_about_the_exact_path_and_a_directorys_subtree() {
		let baseline = Baseline::from_rows([dir("a"), file("a/b/c.txt", Uuid::new_v4(), [1; 32])]);

		assert!(baseline.tracked("a", false));
		assert!(baseline.tracked("a/b", true), "a row under it");
		assert!(!baseline.tracked("a/b", false), "no row AT it");
		assert!(!baseline.tracked("A", true), "the row is spelled otherwise");
		assert!(!baseline.tracked("a/b/c.txt/deeper", true));
	}

	/// Two siblings the server would treat as one name can both be in the store (its key compares
	/// bytewise), so both have to stay addressable by their exact spelling.
	#[test]
	fn two_siblings_that_fold_together_are_both_addressable() {
		let (upper, lower) = (Uuid::new_v4(), Uuid::new_v4());
		let baseline =
			Baseline::from_rows([file("A.txt", upper, [1; 32]), file("a.txt", lower, [2; 32])]);

		assert_eq!(baseline.len(), 2);
		assert_eq!(baseline.get("A.txt").unwrap().remote_uuid, Some(upper));
		assert_eq!(baseline.get("a.txt").unwrap().remote_uuid, Some(lower));
		assert!(baseline.occupied("A.TXT"));
	}

	/// A path with no row of its own stands only while something is under it — and a row taken off
	/// a path that still holds children keeps the path reachable.
	#[test]
	fn a_path_only_node_lives_exactly_as_long_as_its_subtree() {
		let mut baseline =
			Baseline::from_rows([dir("a"), file("a/b/c.txt", Uuid::new_v4(), [1; 32])]);

		baseline.remove("a");
		assert_eq!(baseline.len(), 1);
		assert_eq!(baseline.get("a"), None);
		assert!(
			baseline.tracked("a", true),
			"the row under it is still there"
		);

		baseline.remove("a/b/c.txt");
		assert!(baseline.is_empty());
		assert!(!baseline.tracked("a", true));
		assert!(!baseline.occupied("a"));
		assert!(paths(&baseline).is_empty());
	}

	/// A directory move re-keys the row at the source and every row under it, overwrites what it
	/// lands on, and leaves the rest of the destination's subtree alone — what the store's two
	/// `UPDATE OR REPLACE` statements do. A case-only rename is one of these.
	#[test]
	fn a_subtree_move_re_keys_its_rows_and_overwrites_only_what_it_lands_on() {
		let moved = Uuid::new_v4();
		let mut baseline = Baseline::from_rows([
			dir("a"),
			file("a/x.txt", moved, [1; 32]),
			dir("b"),
			file("b/x.txt", Uuid::new_v4(), [2; 32]),
			file("b/kept.txt", Uuid::new_v4(), [3; 32]),
			file("ab.txt", Uuid::new_v4(), [4; 32]),
		]);

		baseline.move_subtree("a", "b");
		assert_eq!(
			paths(&baseline),
			vec!["ab.txt", "b", "b/kept.txt", "b/x.txt"],
			"the sibling sharing the prefix is untouched and b/kept.txt survives"
		);
		assert_eq!(baseline.get("b/x.txt").unwrap().remote_uuid, Some(moved));

		baseline.move_subtree("b", "B");
		assert_eq!(
			paths(&baseline),
			vec!["B", "B/kept.txt", "B/x.txt", "ab.txt"]
		);
		assert_eq!(baseline.get("B/x.txt").unwrap().remote_uuid, Some(moved));
	}

	/// Untracking an ignored root drops the row at it and every row under it, and nothing else.
	#[test]
	fn removing_a_subtree_drops_the_root_and_its_rows_only() {
		let (dropped, kept) = (Uuid::new_v4(), Uuid::new_v4());
		let mut baseline = Baseline::from_rows([
			dir("build"),
			file("build/out.bin", dropped, [1; 32]),
			file("builder.txt", kept, [2; 32]),
		]);

		baseline.remove_subtrees(&BTreeSet::from(["build".to_string()]));
		assert_eq!(paths(&baseline), vec!["builder.txt"]);
		assert_eq!(baseline.len(), 1);
		// The dropped rows leave the indexes with the subtree; the sibling's claim stays.
		assert_eq!(baseline.path_by_uuid(dropped), None);
		assert_eq!(baseline.path_by_uuid(kept).as_deref(), Some("builder.txt"));
		assert!(baseline.has_remote_rows());
	}

	/// The two shapes the tree cannot represent are refused rather than half-applied: the pair
	/// root's slot is not one to hand back (freeing it would make the root its own child), and an
	/// index answers for one row, so a second claim on an id displaces the first.
	#[test]
	fn the_pair_root_and_a_second_claim_on_one_uuid_are_refused() {
		let shared = Uuid::new_v4();
		let mut baseline = Baseline::from_rows([dir("d"), file("d/x.txt", shared, [1; 32])]);

		baseline.remove_subtrees(&BTreeSet::from([String::new()]));
		baseline.move_subtree("", "e");
		baseline.move_subtree("d", "");
		assert_eq!(
			paths(&baseline),
			vec!["d", "d/x.txt"],
			"the tree is untouched"
		);

		// The root's slot never reached the free list, so the next row written is a child OF the
		// root and not of itself — which a walk of the tree would otherwise never leave.
		baseline.upsert(&file("after.txt", Uuid::new_v4(), [3; 32]));
		assert_eq!(paths(&baseline), vec!["after.txt", "d", "d/x.txt"]);

		// A second row claiming a uuid the tree already records: the later row answers, exactly as
		// the per-pass map this index replaced answered, and the displacement is logged.
		baseline.upsert(&file("d/twin.txt", shared, [2; 32]));
		assert_eq!(baseline.path_by_uuid(shared).as_deref(), Some("d/twin.txt"));
	}

	/// The unconfirmed index holds exactly the rows a push left behind — a file, synced, with
	/// content on record that is not what the two sides last agreed on — and drops each as its
	/// marker advances.
	#[test]
	fn unconfirmed_holds_the_rows_awaiting_confirmation_only() {
		let pushed = Uuid::new_v4();
		let mut baseline = Baseline::from_rows([
			BaselineEntry {
				agreed_hash: None,
				..file("pushed.txt", pushed, [1; 32])
			},
			file("agreed.txt", Uuid::new_v4(), [2; 32]),
			BaselineEntry {
				agreed_hash: None,
				state: BaselineState::Conflicted,
				..file("held.txt", Uuid::new_v4(), [3; 32])
			},
			BaselineEntry {
				agreed_hash: None,
				..dir("d")
			},
		]);

		assert!(baseline.any_unconfirmed());
		assert_eq!(
			baseline
				.unconfirmed()
				.map(|entry| entry.rel_path)
				.collect::<Vec<_>>(),
			vec!["pushed.txt".to_string()]
		);

		let advanced = baseline
			.set_agreed("pushed.txt", Some(Blake3Hash::from([1; 32])))
			.expect("the row is there");
		assert_eq!(advanced.agreed_hash, advanced.content_hash);
		assert!(!baseline.any_unconfirmed());
		assert_eq!(baseline.unconfirmed().count(), 0);
	}

	/// The gate a directory move takes before it carries a subtree across as one move: a row under
	/// it that is not `Synced` is a divergence the per-path plan has to see.
	#[test]
	fn subtree_all_synced_sees_every_row_under_a_directory() {
		let mut baseline = Baseline::from_rows([
			dir("a"),
			file("a/x.txt", Uuid::new_v4(), [1; 32]),
			file("a/deep/y.txt", Uuid::new_v4(), [2; 32]),
		]);
		assert!(baseline.subtree_all_synced("a"));
		assert!(
			baseline.subtree_all_synced("missing"),
			"nothing is not a divergence"
		);

		baseline.upsert(&BaselineEntry {
			state: BaselineState::Conflicted,
			..file("a/deep/y.txt", Uuid::new_v4(), [2; 32])
		});
		assert!(!baseline.subtree_all_synced("a"));
	}

	/// The store hands its write paths to [`Baseline::apply`] as changes; each one has to land the
	/// way the statement it mirrors lands.
	#[test]
	fn applying_a_change_list_mirrors_each_statement() {
		let mut baseline =
			Baseline::from_rows([dir("a"), file("a/x.txt", Uuid::new_v4(), [1; 32])]);
		let created = file("b/new.txt", Uuid::new_v4(), [5; 32]);

		baseline.apply(&[
			BaselineChange::Upsert(&created),
			BaselineChange::Delete("a/x.txt"),
			BaselineChange::MoveSubtree { from: "a", to: "c" },
		]);

		assert_eq!(paths(&baseline), vec!["b/new.txt", "c"]);
		assert_eq!(baseline.len(), 2);
	}

	/// The cursor is a memo over [`Baseline::get`] and has to answer identically — for a path whose
	/// directory it has never seen, for one that is no row, for a sibling that folds onto another
	/// spelling, and after a miss, which must leave the directory it holds consistent.
	#[test]
	fn the_cursor_answers_exactly_what_a_fresh_lookup_answers() {
		let baseline = Baseline::from_rows([
			dir("a"),
			file("a/x.txt", Uuid::new_v4(), [1; 32]),
			file("a/X.txt", Uuid::new_v4(), [2; 32]),
			file("a/deep/y.txt", Uuid::new_v4(), [3; 32]),
			file("b.txt", Uuid::new_v4(), [4; 32]),
		]);

		let mut cursor = baseline.cursor();
		for path in [
			"a",
			"a/x.txt",
			"a/X.txt",
			"a/deep",
			"a/deep/y.txt",
			"b.txt",
			"missing",
			"missing/deeper.txt",
			"a/gone.txt",
			"a/x.txt",
		] {
			assert_eq!(cursor.get(path), baseline.get(path), "{path}");
		}
	}

	/// The borrowing walk is what a pass reads the tree with, so it has to hand out exactly the
	/// paths the owning one does — in the same order, over a tree that holds a path-only node, two
	/// siblings that fold together and nesting on both sides of them — and it has to do it again on a
	/// second walk, since it reuses one buffer.
	#[test]
	fn visiting_the_paths_hands_out_what_collecting_them_does() {
		let baseline = Baseline::from_rows([
			dir("a"),
			file("a/x.txt", Uuid::new_v4(), [1; 32]),
			file("a/X.txt", Uuid::new_v4(), [2; 32]),
			// Under `a/deep`, which is a path with no row of its own.
			file("a/deep/y.txt", Uuid::new_v4(), [3; 32]),
			file("a/deep/nested/z.txt", Uuid::new_v4(), [4; 32]),
			file("b.txt", Uuid::new_v4(), [5; 32]),
		]);
		let owned: Vec<String> = baseline.paths().collect();

		let mut visited = Vec::new();
		baseline.visit_row_paths(|path| visited.push(path.to_string()));
		assert_eq!(visited, owned);

		let mut again = Vec::new();
		baseline.visit_row_paths(|path| again.push(path.to_string()));
		assert_eq!(
			again, owned,
			"the reused buffer must not leak between walks"
		);

		let empty = Baseline::default();
		let mut none = Vec::new();
		empty.visit_row_paths(|path| none.push(path.to_string()));
		assert!(none.is_empty(), "a pair with no rows visits nothing");
	}

	/// A row cannot be written for the pair root itself: the store's key includes a path, and a
	/// nameless row would be a row nothing can address.
	#[test]
	fn a_row_with_no_path_is_refused() {
		let mut baseline = Baseline::default();
		baseline.upsert(&file("", Uuid::new_v4(), [1; 32]));
		assert!(baseline.is_empty());
	}
}
