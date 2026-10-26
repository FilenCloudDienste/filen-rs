//! The two sides a pass reconciles — the local tree and the remote view — behind the narrowest
//! access each consumer needs.
//!
//! A pass reads two path-keyed sides. A WHOLE pass materializes both (the local walk's nodes, the
//! view built out of the cache snapshot); a change-scoped pass has neither and DERIVES them from
//! the resident baseline. The two traits here are what lets a consumer be handed either shape
//! without knowing which it got.
//!
//! The split between them is the point rather than a convenience. [`NodesAt`] answers about ONE
//! path, which a backing that derives its nodes can do without building anything else; [`Nodes`]
//! can be walked whole, which such a backing can only do by walking the tree. So a consumer that
//! asks about paths takes `NodesAt` and stays cheap whatever backs it, and one that takes `Nodes`
//! says in its own signature that it costs the tree.
//!
//! Nodes and paths come back as [`Cow`] for the same reason: a materialized side hands out a
//! reference to what it already holds — no allocation, which is what keeps a whole pass's cost
//! where it was — and a derived one hands out what it just built, and lends what it holds.
//!
//! # The backings
//!
//! [`Side::Whole`] is the path-keyed map a whole read produces. [`Side::Carried`] is an
//! [`Overlay`] — one entry per path THIS pass read — over the rows the resident baseline already
//! describes: a carried row's local and remote nodes are both derived from that row on demand, so
//! a change-scoped pass holds per-PASS data instead of a second copy of the tree. [`Side::Staged`]
//! is what a whole pass's local walk and remote snapshot produce now: the map's nodes as rows of the
//! pass's stage, read a page at a time, so a whole pass holds no copy of either tree in memory
//! either.
//!
//! That is the whole of the change-scoped pass's cost. At a million rows, materializing the two
//! maps and then freeing them was 395 ms of a 397 ms pass, against 0.226 ms for every other step
//! put together.
//!
//! # Why the baseline is supplied at READ time
//!
//! A carried side does not hold the baseline it derives from — [`Side::of`] takes it per read.
//! It cannot hold one: a pass MUTATES the rows while both sides are live (`Baseline::set_agreed`
//! for a push confirmation, `Baseline::move_subtree` for a directory move), so a side owning its
//! own copy would force a clone of the whole tree and then go on answering from the pre-move rows.
//!
//! # Which mutations a derived backing cannot do the way a map does
//!
//! [`Side::rekey_subtree`] touches the OVERLAY only. A carried side's nodes come off rows, and
//! `Baseline::move_subtree` moves those rows, so the carried half re-keys ITSELF — re-keying it
//! here as well would move it twice. The order is therefore load-bearing: re-key the side, THEN
//! move the rows, while the source path still names them. The same rows are under the OTHER side
//! too, which a whole map would leave untouched and a carried side would let follow them, so
//! that side is told to stay where it is first ([`Side::stay_put`]).
//!
//! [`Side::retain`] is a whole-side edit by construction — it asks about every node — so a carried
//! side pays the tree for it, exactly as [`Nodes`] says in its own docs. A caller that knows which
//! paths it is about should say so instead ([`Side::remove`], [`Side::subtree_paths`]).

#[cfg(feature = "bench-internals")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
	borrow::Cow,
	collections::{HashMap, HashSet, hash_map},
};

use itertools::Either;
use uuid::Uuid;

use super::{
	baseline::{BaselineEntry, NodeKind},
	changes::{SUBTREE_CAP, past_cap},
	plan::{RemoteNode, is_under, moved_path},
	rows::Baseline,
	scan::{LocalNode, at_or_under_folded},
	stage::{Pages, StagedSide},
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

	/// Everything STRICTLY under `dir` (`dir/...`), in path order: what a directory's signature is
	/// read over, path after path (see `plan::fold_dir_moves`). A staged side reads it in that order;
	/// the others sort the subtree.
	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, Self::Node>)>;
}

/// A node that says what kind of item it is: what a walk of a side's directories keeps.
pub(super) trait Kinded {
	fn kind(&self) -> NodeKind;
}

impl Kinded for LocalNode {
	fn kind(&self) -> NodeKind {
		self.kind
	}
}

impl Kinded for RemoteNode {
	fn kind(&self) -> NodeKind {
		self.kind
	}
}

/// `nodes` in path order.
fn in_path_order<'a, T>(
	nodes: impl Iterator<Item = (Cow<'a, str>, Cow<'a, T>)>,
) -> std::vec::IntoIter<(Cow<'a, str>, Cow<'a, T>)>
where
	T: Clone + 'a,
{
	let mut nodes: Vec<_> = nodes.collect();
	nodes.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
	nodes.into_iter()
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
	/// answer ([`Baseline::occupied`](super::rows::Baseline::occupied)) is a walk of one node's
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
		in_path_order(Nodes::iter(self).filter(|(path, _)| is_under(path, dir)))
	}
}

/// A node one side of a pass DERIVES from a single baseline row.
///
/// Implemented in [`derive`](super::derive), next to the rule it rests on: a row stands in for
/// BOTH sides or for neither ([`derive::carried`](super::derive)), so the two implementations are
/// two halves of one answer and cannot come to disagree about which rows are carryable.
pub(super) trait FromRow: Clone {
	/// This side's half of `row`, or `None` when the row stands in for neither side.
	fn from_row(row: &BaselineEntry) -> Option<Self>;
}

/// What a change-scoped pass observed, over the side the baseline rows already describe.
///
/// One record per path the pass actually read: the node where it found something, a TOMBSTONE
/// where it found nothing. The tombstone is the only thing that can take a carried row's node out
/// of the side — invariant I1 as a data structure, since the only writer of one is an observation
/// about that very path.
///
/// A path with NO record is one the pass got no evidence for, and its row still speaks for it.
/// That is the whole difference between "observed absent" and "not looked at", so every write
/// keeps a path in at most one of the two collections, and [`get`](Self::get) answers all three.
///
/// Two collections where one map to `Option<T>` would say the same, because a tombstone in such a
/// map costs a node's whole slot — 104 bytes for a local node, against a path's 24 in a set — and
/// a directory renamed away tombstones every row under it. On a 50k-file directory rename that
/// was half the local overlay, and what doubled its table to 131,072 buckets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Overlay<T> {
	/// Where the pass found a node.
	nodes: HashMap<String, T>,
	/// Where it found nothing: the tombstones.
	gone: HashSet<String>,
}

impl<T> Default for Overlay<T> {
	fn default() -> Self {
		Self {
			nodes: HashMap::new(),
			gone: HashSet::new(),
		}
	}
}

/// What an [`Overlay`] recorded at one path.
enum Recorded<'o, T> {
	/// The pass found this node there.
	Node(&'o T),
	/// The pass found nothing there: a tombstone, which the row behind it does not get past.
	Tombstone,
	/// The pass recorded nothing there, so the path's row still speaks for it.
	Nothing,
}

impl<T> Overlay<T> {
	/// What the pass recorded at `path`.
	fn get(&self, path: &str) -> Recorded<'_, T> {
		match self.nodes.get(path) {
			Some(node) => Recorded::Node(node),
			None if self.gone.contains(path) => Recorded::Tombstone,
			None => Recorded::Nothing,
		}
	}

	/// Whether the pass recorded anything at `path`, a node or a tombstone.
	fn records(&self, path: &str) -> bool {
		!matches!(self.get(path), Recorded::Nothing)
	}

	/// Record `node` at `path`, over whatever was recorded there.
	fn put(&mut self, path: String, node: T) {
		self.gone.remove(&path);
		self.nodes.insert(path, node);
	}

	/// Record that nothing is at `path`, over whatever was recorded there.
	fn tombstone(&mut self, path: String) {
		self.nodes.remove(&path);
		self.gone.insert(path);
	}

	/// Every path the pass recorded anything at.
	fn paths(&self) -> impl Iterator<Item = &String> {
		self.nodes.keys().chain(&self.gone)
	}

	/// Whether the side this overlay corrects holds `path`: what the pass recorded there, or —
	/// where it recorded nothing — whether the baseline has a row it can carry.
	///
	/// The ONE implementation of that question, answered without building a node.
	fn holds(&self, baseline: &Baseline, path: &str) -> bool {
		match self.get(path) {
			Recorded::Node(_) => true,
			Recorded::Tombstone => false,
			Recorded::Nothing => baseline.carryable(path),
		}
	}
}

/// One side of a pass, as the pass OWNS it: [`RemoteView::nodes`](super::plan::RemoteView::nodes),
/// [`LocalScan::nodes`](super::scan::LocalScan::nodes) and the two halves of
/// [`Derived`](super::derive::Derived).
///
/// Every read goes through [`NodesAt`] / [`Nodes`] on [`SideRef`] and every EDIT is a method here,
/// so which backing a consumer was handed is not a thing it can ask.
///
/// Nothing hands out a borrow of a stored node (`&T`) or of a stored path (`&str`), which is the
/// whole discipline: a backing that builds a node on demand has nothing to lend. Reads come back
/// as [`Cow`], so a materialized side still lends what it holds and allocates nothing.
///
/// Compared only by the tests and the probe. Never cloned: a staged side is its pass's own table,
/// so a test that wants a second side to edit asks for a copy of its nodes
/// (`test_support::copied`).
#[derive(Debug)]
#[cfg_attr(any(test, feature = "bench-internals"), derive(PartialEq, Eq))]
pub(crate) enum Side<T> {
	/// Every node, as a whole read produced it.
	Whole(HashMap<String, T>),
	/// What this pass observed, over the rows the baseline carries.
	Carried(Overlay<T>),
	/// Every node, as a whole pass staged it: a row each in the pass's stage, read a page at a time
	/// (see [`StagedSide`]). What a whole read produces on either side, where a whole map held a
	/// node per item of the tree in memory for the rest of the plan.
	Staged(StagedSide<T>),
}

impl<T> Side<T> {
	/// A side that derives its nodes from the baseline's rows, holding only what a pass observes.
	pub(super) fn carried() -> Self {
		Self::Carried(Overlay::default())
	}

	/// How many nodes this side has room for in memory. Only the probe's memory accounting asks,
	/// and the tests. For a CARRIED side this is the per-pass data and not the tree, which is the
	/// whole point of it; a STAGED side holds its nodes in its stage, and none in memory.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(crate) fn capacity(&self) -> usize {
		match self {
			Self::Whole(map) => map.capacity(),
			Self::Carried(overlay) => overlay.nodes.capacity(),
			Self::Staged(_) => 0,
		}
	}

	/// How many tombstones this side has room for, beside [`capacity`](Self::capacity)'s nodes:
	/// a carried side keeps them in a set of paths of their own. None for a whole or a staged side,
	/// which record no absence. Only the probe's memory accounting asks.
	#[cfg(feature = "bench-internals")]
	pub(crate) fn tombstone_capacity(&self) -> usize {
		match self {
			Self::Whole(_) | Self::Staged(_) => 0,
			Self::Carried(overlay) => overlay.gone.capacity(),
		}
	}

	/// Read this side against the baseline its carried half derives from.
	///
	/// A `Whole` side never consults it, so passing a default baseline to one is correct rather
	/// than merely harmless.
	pub(super) fn of<'a>(&'a self, baseline: &'a Baseline) -> SideRef<'a, T> {
		SideRef {
			baseline,
			side: self,
		}
	}

	/// Read a WHOLE or a STAGED side against an empty tree, for a test that has no baseline to
	/// hand.
	///
	/// Exact rather than a stand-in: neither backing ever consults a baseline. It ASSERTS the
	/// backing, because a carried side read this way would answer "holds nothing" for every row it
	/// carries and turn the assertions that follow into ones that cannot fail.
	///
	/// The probe reads its whole-pass sides through this too, which is why it is not `cfg(test)`:
	/// that module is compiled into a binary built WITHOUT `cfg(test)`.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn whole(&self) -> SideRef<'_, T> {
		assert!(
			!matches!(self, Self::Carried(_)),
			"a carried side must be read against the baseline it derives from, not an empty one"
		);
		static EMPTY: std::sync::OnceLock<Baseline> = std::sync::OnceLock::new();
		self.of(EMPTY.get_or_init(Baseline::default))
	}

	/// The paths this side holds that no baseline row put there — what THIS pass's own producers
	/// added: the local observation, the remote delta, the fold of unacknowledged writes.
	///
	/// A whole or a staged side answers with every path it holds, which is all it has. A carried
	/// one answers with its OVERLAY's nodes alone, and that is exact rather than a shortcut:
	/// [`derive::from_baseline`](super::derive::from_baseline) places nothing, so every other path
	/// such a side holds IS a baseline row answering for itself. That is what lets the assembly
	/// check cost the change rather than the tree.
	pub(super) fn own_keys(&self) -> impl Iterator<Item = Cow<'_, str>> {
		match self {
			Self::Whole(map) => Either::Left(map.keys()),
			// A tombstone holds nothing, so it is not a key this side has.
			Self::Carried(overlay) => Either::Left(overlay.nodes.keys()),
			Self::Staged(staged) => Either::Right(staged.pages().map(|(path, _)| Cow::Owned(path))),
		}
		.map_left(|held| held.map(|path| Cow::Borrowed(path.as_str())))
	}

	/// Whether this pass recorded nothing at `path`, so that whatever this side holds there is the
	/// node its row carries. No row is read to say so. A whole or a staged side records everything
	/// it holds and never answers `true`.
	pub(super) fn untouched(&self, path: &str) -> bool {
		match self {
			Self::Whole(_) | Self::Staged(_) => false,
			Self::Carried(overlay) => !overlay.records(path),
		}
	}

	/// This side's nodes by value, for a test that owns a WHOLE or a STAGED one and wants to feed
	/// them somewhere. Asserts the backing for the reason [`whole`](Self::whole) does: a carried
	/// side's nodes are not held anywhere to be handed over.
	#[cfg(test)]
	pub(super) fn into_whole(self) -> HashMap<String, T> {
		match self {
			Self::Whole(map) => map,
			Self::Staged(staged) => staged.into_pages().collect(),
			Self::Carried(_) => panic!("a carried side holds no map to hand over"),
		}
	}

	/// Put `node` at `path`.
	///
	/// Returns nothing, where a map's `insert` hands back what it displaced: a carried side would
	/// have to DERIVE the displaced node, and the one caller that wanted it
	/// (`engine::place_node`) asks through [`NodesAt::at`] first, which is one lookup on the
	/// path it is already holding.
	///
	/// A staged side files the node under `path`, over whatever is there (see
	/// [`StagedSide::insert`]). The local walk's table holds one spelling of each folded name; the
	/// remote one does not, since two spellings of one name are what its collision check looks for.
	pub(crate) fn insert(&mut self, path: String, node: T) {
		match self {
			Self::Whole(map) => {
				map.insert(path, node);
			}
			Self::Carried(overlay) => overlay.put(path, node),
			Self::Staged(staged) => staged.insert(&path, &node),
		}
	}

	/// Make room for `additional` more nodes, for a caller that knows how many it is about to put.
	/// A staged side's room is its stage's file.
	pub(super) fn reserve(&mut self, additional: usize) {
		match self {
			Self::Whole(map) => map.reserve(additional),
			Self::Carried(overlay) => overlay.nodes.reserve(additional),
			Self::Staged(_) => {}
		}
	}
}

impl<T: FromRow> Side<T> {
	/// Take the node at `path` out of this side, returning it.
	///
	/// On a carried side this writes the TOMBSTONE that makes [`NodesAt::at`] answer `None` even
	/// where a row is carried there, and the node it hands back is the one that row described.
	pub(crate) fn remove(&mut self, baseline: &Baseline, path: &str) -> Option<T> {
		match self {
			Self::Whole(map) => map.remove(path),
			Self::Staged(staged) => staged.remove(path),
			Self::Carried(overlay) => {
				// Already a tombstone: nothing to take, and the tombstone stands.
				if overlay.gone.contains(path) {
					return None;
				}
				let was = overlay
					.nodes
					.remove(path)
					.or_else(|| baseline.get(path).as_ref().and_then(T::from_row));
				// Recorded either way: a `remove` is an observation about this path, and a path
				// the side was not describing is one nothing is carried at. Writing the tombstone
				// unconditionally keeps `holds` false there whatever the rows later say.
				overlay.gone.insert(path.to_owned());
				was
			}
		}
	}

	/// [`remove`](Self::remove), for a caller already holding the baseline's row at the path: it
	/// answers whether this side held a node there — what [`NodesAt::holds`] said before — and
	/// neither reads the row again nor builds the node to say so.
	///
	/// A caller dropping a subtree throws away the node `remove` hands back, and on a carried side
	/// that node costs a lookup of the row the caller already has plus the derivation of both its
	/// halves: once per row under a directory moved away.
	pub(super) fn forget(&mut self, row: &BaselineEntry) -> bool {
		match self {
			Self::Whole(map) => map.remove(&row.rel_path).is_some(),
			Self::Staged(staged) => staged.forget(&row.rel_path),
			Self::Carried(overlay) => {
				// Already a tombstone: nothing held, and the tombstone stands.
				if overlay.gone.contains(&row.rel_path) {
					return false;
				}
				// A node the pass put there, or else what the tree's carryable index answers at
				// the row's path, from the row.
				let held = overlay.nodes.remove(&row.rel_path).is_some() || row.carryable();
				// A key of its own: the caller goes on to record the row's path as decided.
				overlay.gone.insert(row.rel_path.clone());
				held
			}
		}
	}

	/// This side's nodes by value, for a caller done with it: what [`Nodes::iter`] yields, with a
	/// materialized side's paths and nodes handed over rather than copied out.
	pub(super) fn into_entries(self, baseline: &Baseline) -> impl Iterator<Item = (String, T)> {
		match self {
			Self::Whole(map) => Either::Left(map.into_iter()),
			Self::Staged(staged) => Either::Right(Either::Left(staged.into_pages())),
			// A carried side holds its rows' nodes nowhere to hand over, so they are built as a
			// walk of it builds them.
			Self::Carried(_) => Either::Right(Either::Right(
				self.of(baseline)
					.iter()
					.map(|(path, node)| (path.into_owned(), node.into_owned()))
					.collect::<Vec<_>>()
					.into_iter(),
			)),
		}
	}

	/// Keep the nodes `keep` answers `true` for.
	///
	/// A WHOLE-side edit by construction — it asks about every node — so a carried side pays the
	/// tree for it. A caller that knows which paths it is about should say so instead
	/// ([`remove`](Self::remove), [`subtree_paths`](Self::subtree_paths)).
	pub(crate) fn retain(&mut self, baseline: &Baseline, mut keep: impl FnMut(&str, &T) -> bool) {
		match self {
			Self::Whole(map) => map.retain(|path, node| keep(path, node)),
			Self::Staged(staged) => staged.retain(keep),
			Self::Carried(_) => {
				let dropping: Vec<String> = self
					.of(baseline)
					.entries()
					.filter(|(path, node)| !keep(path.as_ref(), node.as_ref()))
					.map(|(path, _)| path.into_owned())
					.collect();
				let Self::Carried(overlay) = self else {
					unreachable!("the backing was just matched as carried")
				};
				for path in dropping {
					overlay.tombstone(path);
				}
			}
		}
	}

	/// Every path STRICTLY under `dir` (`dir/...`), with no node built.
	///
	/// Owned and collected rather than borrowed and lazy because every caller is about to MUTATE
	/// this side with the answer — drop the subtree, re-key it, trash it — and the walk cannot
	/// still be borrowing it by then.
	pub(crate) fn subtree_paths(&self, baseline: &Baseline, dir: &str) -> Vec<String> {
		match self {
			Self::Whole(map) => map
				.keys()
				.filter(|path| is_under(path, dir))
				.cloned()
				.collect(),
			Self::Staged(staged) => staged.subtree_paths(dir),
			Self::Carried(overlay) => {
				let mut out: Vec<String> = Vec::new();
				// The rows, from the baseline's own subtree walk — the size of the directory
				// rather than of the tree — minus whatever this pass observed away.
				baseline.visit_subtree_paths(dir, |path| {
					if !overlay.records(path) && baseline.carryable(path) {
						out.push(path.to_owned());
					}
				});
				// What the pass PUT somewhere under `dir`, which no row need name.
				out.extend(
					overlay
						.nodes
						.keys()
						.filter(|path| is_under(path, dir))
						.cloned(),
				);
				out
			}
		}
	}

	/// Move the subtree at `from` onto `to` — the node at `from` itself included — telling each
	/// node its new path through `set_path`.
	///
	/// On a CARRIED side this re-keys the overlay and nothing else. The carried half re-keys
	/// itself, because its nodes are derived from rows that
	/// [`Baseline::move_subtree`](super::rows::Baseline::move_subtree) moves — so this must run
	/// BEFORE the rows move, while `from` still names them, and doing both would move it twice.
	///
	/// What it must also do is drop what this pass recorded at the paths the move WRITES A ROW TO
	/// — `to` itself, and the path each row under `from` lands on. Those are the paths
	/// [`Baseline::move_subtree`](super::rows::Baseline::move_subtree) overwrites, so the arriving
	/// row is what a whole read shows there and whatever the overlay said about the row that used
	/// to sit there is superseded: a tombstone left standing would answer "absent" over a row that
	/// has just arrived, and a node a producer placed there is one the whole backing's own re-key
	/// overwrites. The tombstones are real — `hide`, `resolve_collisions` and `merge_local`'s
	/// `drop_rows` all write them before `fold_dir_moves` runs — so the destination of a fold is
	/// exactly where one can be found.
	///
	/// Everything ELSE under `to` is left alone, because the move does not touch it: `move_subtree`
	/// leaves the rest of the destination's subtree where it is, so a tombstone there is still an
	/// observation about the row still sitting there, and a node placed beside the landing paths
	/// was never this move's to drop. Sweeping the whole destination subtree would resurrect the
	/// first and lose the second.
	///
	/// A STAGED side re-keys its rows in its stage, where a node's path is its row's, so there is
	/// no node for `set_path` to tell. Its destination must be free under every spelling, which a
	/// directory move's is (see [`StagedSide::rekey`]).
	pub(crate) fn rekey_subtree(
		&mut self,
		baseline: &Baseline,
		from: &str,
		to: &str,
		set_path: impl Fn(&mut T, &str),
	) {
		match self {
			Self::Whole(map) => {
				let moving: Vec<(String, String)> = map
					.keys()
					.filter_map(|key| Some((key.clone(), moved_path(key, from, to)?)))
					.collect();
				for (old, new) in moving {
					let Some(mut node) = map.remove(&old) else {
						continue;
					};
					set_path(&mut node, &new);
					map.insert(new, node);
				}
			}
			Self::Staged(staged) => staged.rekey(from, to),
			Self::Carried(overlay) => {
				let moving: Vec<(String, String)> = overlay
					.paths()
					.filter_map(|key| Some((key.clone(), moved_path(key, from, to)?)))
					.collect();
				// The landing paths first — the destinations a row arrives at, and no other path
				// under `to` (see the note above). Found among the overlay's OWN keys, as those at
				// or under `to` with a row at the same place under `from`: a lookup per key the
				// pass observed there, rather than a walk of the rows about to move, which is the
				// size of the subtree and would make a folded move of a large directory cost it.
				let landing: Vec<String> = overlay
					.paths()
					.filter(|key| {
						moved_path(key, to, from)
							.is_some_and(|source| baseline.contains_key(&source))
					})
					.cloned()
					.collect();
				for key in landing {
					overlay.nodes.remove(&key);
					overlay.gone.remove(&key);
				}
				for (old, new) in moving {
					if let Some(mut node) = overlay.nodes.remove(&old) {
						set_path(&mut node, &new);
						overlay.put(new, node);
					} else if overlay.gone.remove(&old) {
						overlay.tombstone(new);
					}
				}
			}
		}
	}

	/// Keep this side where it is while the rows at and under `from` move onto `to` beneath it:
	/// the half of a directory move [`rekey_subtree`](Self::rekey_subtree) is not, for the side
	/// the move does NOT carry.
	///
	/// A whole or a staged side derives nothing from the rows, so moving them leaves it as it was.
	/// A carried side reads its nodes off those very rows and would follow them: it would stop
	/// holding what it carried at each source path, and start holding a node at each destination
	/// the row lands on — a node no observation of this side ever found there. So before the rows
	/// move, every path they leave or land on that this side recorded nothing at gets what the side
	/// answers there NOW: the node the row at it carries, or — where a row lands on a path that
	/// carried nothing — a tombstone.
	///
	/// Hands back those paths. Each is one where the side no longer holds what the row there
	/// carries once the rows have moved, which is what a change-scoped pass has to decide (see
	/// [`PassPaths::Changed`](super::plan::PassPaths::Changed)).
	///
	/// A walk of the rows under `from`, which is why a move whose match already proved this side
	/// holds nothing at the source and a recorded node at every destination does not ask for it.
	///
	/// `None`, with nothing recorded, where the rows strictly under the two ends number more than
	/// [`SUBTREE_CAP`]: a record at each of them is the subtree's worth of memory a change-scoped
	/// pass reads both sides whole rather than hold.
	pub(crate) fn stay_put(
		&mut self,
		baseline: &Baseline,
		from: &str,
		to: &str,
	) -> Option<Vec<String>> {
		let Self::Carried(overlay) = self else {
			return Some(Vec::new());
		};
		if past_cap(
			baseline
				.count_subtree(from)
				.saturating_add(baseline.count_subtree(to)),
			SUBTREE_CAP,
		) {
			return None;
		}
		// The rows the move overwrites, by the path it overwrites them at: none where the
		// destination was free, which is every move but one onto rows it has to merge with. Keyed
		// by a copy of the path the row holds, as the lookups below are by path.
		let mut landed_on: HashMap<String, BaselineEntry> = baseline
			.get(to)
			.into_iter()
			.chain(baseline.subtree(to))
			.map(|row| (row.rel_path.clone(), row))
			.collect();
		let mut recorded = Vec::new();
		for row in baseline.get(from).into_iter().chain(baseline.subtree(from)) {
			let Some(landing) = moved_path(&row.rel_path, from, to) else {
				continue;
			};
			// Each path recorded is handed back as well, hence the copies of it below.
			if !overlay.records(&landing) {
				match landed_on.remove(&landing).as_ref().and_then(T::from_row) {
					Some(node) => overlay.put(landing.clone(), node),
					None => overlay.tombstone(landing.clone()),
				}
				recorded.push(landing);
			}
			// A source path is left with no row at all, so only a node the row carried is lost.
			if !overlay.records(&row.rel_path)
				&& let Some(node) = T::from_row(&row)
			{
				overlay.put(row.rel_path.clone(), node);
				recorded.push(row.rel_path);
			}
		}
		Some(recorded)
	}
}

impl<T> Default for Side<T> {
	fn default() -> Self {
		Self::Whole(HashMap::new())
	}
}

impl<T> From<HashMap<String, T>> for Side<T> {
	fn from(nodes: HashMap<String, T>) -> Self {
		Self::Whole(nodes)
	}
}

impl<T> FromIterator<(String, T)> for Side<T> {
	fn from_iter<I: IntoIterator<Item = (String, T)>>(nodes: I) -> Self {
		Self::Whole(nodes.into_iter().collect())
	}
}

impl<T> Extend<(String, T)> for Side<T> {
	fn extend<I: IntoIterator<Item = (String, T)>>(&mut self, nodes: I) {
		match self {
			Self::Whole(map) => map.extend(nodes),
			Self::Carried(overlay) => {
				for (path, node) in nodes {
					overlay.put(path, node);
				}
			}
			Self::Staged(staged) => {
				for (path, node) in nodes {
					staged.insert(&path, &node);
				}
			}
		}
	}
}

/// How often a CARRIED side has been walked whole, and over how many rows.
///
/// A carried side exists so that a change-scoped pass holds per-pass data instead of a second copy
/// of the tree. [`SideRef::entries`] gives that up: on a carried backing it walks every row and
/// builds a `Vec` sized to [`Baseline::carryable_rows`], so one call on a converged million-row
/// pair materializes the very tree the backing exists not to materialize. Whether a scoped pass
/// reaches it is not a thing a reading of ten thousand lines of engine settles — every whole-set
/// caller is supposed to be gated on `PassPaths::Whole`, and "supposed to" is what a counter is
/// for.
///
/// Process-global and never reset by itself: a memory child runs one pass, so its figures are that
/// pass's. Counted under `bench-internals` only — a shipping build has neither the counter nor the
/// `fetch_add`.
#[cfg(feature = "bench-internals")]
static CARRIED_WHOLE_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "bench-internals")]
static CARRIED_WHOLE_ROWS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "bench-internals")]
static CARRIED_SUBTREE_CALLS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "bench-internals")]
static CARRIED_SUBTREE_ROWS: AtomicU64 = AtomicU64::new(0);

/// What a carried side was asked to materialize (see [`CARRIED_WHOLE_CALLS`]).
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CarriedWalks {
	/// Calls to [`SideRef::entries`] on a carried side — each one O(tree).
	pub(super) whole_calls: u64,
	/// The carryable rows those calls walked, summed — each call sizes its `Vec` to exactly this,
	/// so it is an UPPER bound on what was pushed rather than a count of it: a row that derives
	/// nothing for this side, or one the pass already observed, is walked and skipped. It is the
	/// figure that says whether a pass materialized a tree, which is a question an upper bound
	/// answers: zero means it did not.
	pub(super) whole_rows: u64,
	/// Calls to [`SideRef::entries_under`] on a carried side — each one O(subtree), which is what
	/// a scoped pass is entitled to.
	pub(super) subtree_calls: u64,
	pub(super) subtree_rows: u64,
}

/// What carried sides have materialized in this process so far.
#[cfg(feature = "bench-internals")]
pub(super) fn carried_walks() -> CarriedWalks {
	CarriedWalks {
		whole_calls: CARRIED_WHOLE_CALLS.load(Ordering::Relaxed),
		whole_rows: CARRIED_WHOLE_ROWS.load(Ordering::Relaxed),
		subtree_calls: CARRIED_SUBTREE_CALLS.load(Ordering::Relaxed),
		subtree_rows: CARRIED_SUBTREE_ROWS.load(Ordering::Relaxed),
	}
}

/// Zero the counters, so what follows is measured on its own.
#[cfg(feature = "bench-internals")]
pub(super) fn reset_carried_walks() {
	for counter in [
		&CARRIED_WHOLE_CALLS,
		&CARRIED_WHOLE_ROWS,
		&CARRIED_SUBTREE_CALLS,
		&CARRIED_SUBTREE_ROWS,
	] {
		counter.store(0, Ordering::Relaxed);
	}
}

/// A side plus the baseline its carried half derives from — what every consumer actually reads
/// (see [`Side::of`]).
#[derive(Clone, Copy)]
pub(super) struct SideRef<'a, T> {
	baseline: &'a Baseline,
	side: &'a Side<T>,
}

impl<T: FromRow> SideRef<'_, T> {
	/// Every path this side holds with its node: borrowed pairs for what the side holds — a
	/// materialized map, a carried side's overlay — and owned ones for the nodes a carried side
	/// derives from its rows.
	///
	/// O(tree) on a carried side, which is what [`Nodes`] means by its own existence: a backing
	/// that derives its nodes can only enumerate them by walking the rows.
	fn entries(&self) -> Entries<'_, T> {
		match self.side {
			Side::Whole(map) => Entries::Whole(map.iter()),
			Side::Staged(staged) => Entries::Staged(staged.pages()),
			Side::Carried(overlay) => {
				let mut out: Vec<(String, T)> = Vec::with_capacity(self.baseline.carryable_rows());
				#[cfg(feature = "bench-internals")]
				{
					CARRIED_WHOLE_CALLS.fetch_add(1, Ordering::Relaxed);
					CARRIED_WHOLE_ROWS
						.fetch_add(self.baseline.carryable_rows() as u64, Ordering::Relaxed);
				}
				self.baseline.visit_rows(|row| {
					// A path the pass observed is answered from the overlay below, whichever way.
					if overlay.records(&row.rel_path) {
						return;
					}
					if let Some(node) = T::from_row(row) {
						out.push((row.rel_path.clone(), node));
					}
				});
				Entries::Carried(out.into_iter(), overlay.nodes.iter())
			}
		}
	}

	/// What [`NodesAt::at`] answers at `row`'s path, for a caller holding `row` as this side's
	/// baseline hands it out there: the carried half is derived from the row in hand rather than
	/// read again. `row` must be the baseline's row at its path in the view this side reads — a
	/// row of any other view answers for rows the pass does not have.
	pub(super) fn at_row(&self, row: &BaselineEntry) -> Option<Cow<'_, T>> {
		match self.side {
			Side::Whole(map) => map.get(&row.rel_path).map(Cow::Borrowed),
			Side::Staged(staged) => staged.at(&row.rel_path).map(Cow::Owned),
			Side::Carried(overlay) => match overlay.get(&row.rel_path) {
				Recorded::Node(node) => Some(Cow::Borrowed(node)),
				Recorded::Tombstone => None,
				Recorded::Nothing => T::from_row(row).map(Cow::Owned),
			},
		}
	}

	/// The candidates for [`Nodes::under`]: the subtree rather than the tree, so a carried side
	/// answers a directory question in the size of the directory.
	///
	/// The overlay's nodes are lent whole, as a materialized map's are, for `under` to narrow: a
	/// directory the pass walked — a renamed directory's destination — is all overlay, and copying
	/// it out to hand it over was a second copy of every node the walk found.
	fn entries_under(&self, dir: &str) -> Entries<'_, T> {
		match self.side {
			Side::Whole(map) => Entries::Whole(map.iter()),
			// The subtree's own range of the table, rather than a walk of the whole of it.
			Side::Staged(staged) => Entries::Staged(staged.pages_under(dir)),
			Side::Carried(overlay) => {
				let mut out: Vec<(String, T)> = Vec::new();
				#[cfg(feature = "bench-internals")]
				CARRIED_SUBTREE_CALLS.fetch_add(1, Ordering::Relaxed);
				for row in self.baseline.subtree(dir) {
					if overlay.records(&row.rel_path) {
						continue;
					}
					if let Some(node) = T::from_row(&row) {
						out.push((row.rel_path.clone(), node));
					}
				}
				#[cfg(feature = "bench-internals")]
				CARRIED_SUBTREE_ROWS.fetch_add(out.len() as u64, Ordering::Relaxed);
				Entries::Carried(out.into_iter(), overlay.nodes.iter())
			}
		}
	}
}

impl SideRef<'_, RemoteNode> {
	/// Where this side holds each of `uuids` it holds: what a pass's journal of its own writes asks
	/// of the view (see `PendingWrites::settle`). A staged side seeks its uuid index once per uuid;
	/// the others are walked, keeping only the nodes asked about.
	pub(super) fn paths_of(&self, uuids: &HashSet<Uuid>) -> HashMap<Uuid, Cow<'_, str>> {
		if let Side::Staged(staged) = self.side {
			return staged
				.paths_of(uuids)
				.into_iter()
				.map(|(uuid, path)| (uuid, Cow::Owned(path)))
				.collect();
		}
		self.entries()
			.filter(|(_, node)| uuids.contains(&node.remote_uuid))
			.map(|(path, node)| (node.remote_uuid, path))
			.collect()
	}

	/// Every FILE this side holds with no content hash, with its path: the seeding stage's
	/// candidates (see `seed`), which leaves out the paths that have a row. A staged side reads them
	/// off an index of its own and nothing else; a whole side filters its map.
	///
	/// A carried side answers from its overlay alone. What it leaves out is every node it derives,
	/// and each of those is at a row's path, which the stage leaves out anyway.
	pub(super) fn unhashed_files(
		&self,
	) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, RemoteNode>)> {
		let unhashed =
			|node: &RemoteNode| node.kind == NodeKind::File && node.content_hash.is_none();
		match self.side {
			Side::Whole(map) => Either::Left(Either::Left(
				map.iter().filter(move |(_, node)| unhashed(node)).map(lent),
			)),
			Side::Carried(overlay) => Either::Left(Either::Right(
				overlay
					.nodes
					.iter()
					.filter(move |(_, node)| unhashed(node))
					.map(lent),
			)),
			Side::Staged(staged) => Either::Right(
				staged
					.unhashed_files()
					.map(|node| (Cow::Owned(node.rel_path.clone()), Cow::Owned(node))),
			),
		}
	}
}

/// The backings' entries as one iterator, so [`Nodes`] can return a single type without the
/// materialized side paying an allocation it did not pay before.
enum Entries<'a, T> {
	Whole(hash_map::Iter<'a, String, T>),
	/// The nodes a carried side derives from its rows, built for this walk, then the ones its
	/// overlay holds.
	Carried(
		std::vec::IntoIter<(String, T)>,
		hash_map::Iter<'a, String, T>,
	),
	/// The nodes a staged side reads out of its stage, a page at a time.
	Staged(Pages<'a, T>),
}

impl<'a, T: Clone> Iterator for Entries<'a, T> {
	type Item = (Cow<'a, str>, Cow<'a, T>);

	fn next(&mut self) -> Option<Self::Item> {
		match self {
			Self::Whole(held) => held.next().map(lent),
			Self::Carried(built, held) => built
				.next()
				.map(|(path, node)| (Cow::Owned(path), Cow::Owned(node)))
				.or_else(|| held.next().map(lent)),
			Self::Staged(read) => read
				.next()
				.map(|(path, node)| (Cow::Owned(path), Cow::Owned(node))),
		}
	}
}

/// A node a side holds, lent with its path.
fn lent<'a, T: Clone>((path, node): (&'a String, &'a T)) -> (Cow<'a, str>, Cow<'a, T>) {
	(Cow::Borrowed(path.as_str()), Cow::Borrowed(node))
}

impl<T: FromRow> NodesAt for SideRef<'_, T> {
	type Node = T;

	fn at(&self, path: &str) -> Option<Cow<'_, T>> {
		match self.side {
			Side::Whole(map) => map.get(path).map(Cow::Borrowed),
			Side::Staged(staged) => staged.at(path).map(Cow::Owned),
			Side::Carried(overlay) => match overlay.get(path) {
				Recorded::Node(node) => Some(Cow::Borrowed(node)),
				// Observed absent: the tombstone answers, and the row behind it does not.
				Recorded::Tombstone => None,
				Recorded::Nothing => self
					.baseline
					.get(path)
					.as_ref()
					.and_then(T::from_row)
					.map(Cow::Owned),
			},
		}
	}

	fn holds(&self, path: &str) -> bool {
		match self.side {
			Side::Whole(map) => map.contains_key(path),
			Side::Staged(staged) => staged.holds(path),
			// Answered WITHOUT building the node, which is the whole reason `holds` exists apart
			// from `at().is_some()`.
			Side::Carried(overlay) => overlay.holds(self.baseline, path),
		}
	}

	fn occupied(&self, path: &str) -> bool {
		match self.side {
			Side::Whole(map) => map.keys().any(|key| at_or_under_folded(key, path)),
			// Two seeks of the folded index, where a map folds its keys one by one.
			Side::Staged(staged) => staged.occupied(path),
			Side::Carried(overlay) => {
				// What the pass observed is the only part of this side that is not a row, so it is
				// the only part folded key by key — and it is the per-pass data, not the tree.
				if overlay
					.nodes
					.keys()
					.any(|key| at_or_under_folded(key, path))
				{
					return true;
				}
				// The rows, through the baseline's own folded walk: to the subtree without a scan,
				// and then only as far as the first row this side still holds. It answers `false`
				// for `""` itself, which is the pair root and holds no row, exactly as
				// [`at_or_under_folded`] answers `false` there for every key but `""`.
				self.baseline
					.any_folded_row_at_or_under(path, &mut |row_path| self.holds(row_path))
			}
		}
	}
}

impl<T: FromRow> Nodes for SideRef<'_, T> {
	fn len(&self) -> usize {
		match self.side {
			Side::Whole(map) => map.len(),
			Side::Staged(staged) => staged.len(),
			Side::Carried(overlay) => {
				// The rows this side carries, corrected by what the pass recorded over them. Each
				// overlay key is distinct, so a tombstone can only cancel a row that was counted.
				// Asked in path order: the baseline then reads the rows a page at a time, where the
				// map's own order is a row per question.
				let mut edits: Vec<(&String, bool)> = overlay
					.nodes
					.keys()
					.map(|path| (path, true))
					.chain(overlay.gone.iter().map(|path| (path, false)))
					.collect();
				edits.sort_unstable_by_key(|&(path, _)| path);
				let mut added = 0usize;
				let mut removed = 0usize;
				for (path, present) in edits {
					match (present, self.baseline.carryable(path)) {
						(true, false) => added += 1,
						(false, true) => removed += 1,
						_ => {}
					}
				}
				let rows = self.baseline.carryable_rows();
				// Checked in EVERY build: this crate's release profile leaves overflow checks off,
				// so a bare subtraction would wrap to a length no caller could act on rather than
				// fail. Each tombstone counted above sits on a distinct carryable row.
				assert!(
					removed <= rows,
					"a carried side dropped {removed} of {rows} carryable row(s)"
				);
				rows + added - removed
			}
		}
	}

	fn is_empty(&self) -> bool {
		match self.side {
			Side::Whole(map) => map.is_empty(),
			Side::Carried(_) | Side::Staged(_) => Nodes::len(self) == 0,
		}
	}

	fn paths(&self) -> impl Iterator<Item = Cow<'_, str>> {
		self.entries().map(|(path, _)| path)
	}

	fn iter(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		self.entries()
	}

	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		let dir = dir.to_owned();
		let under = self
			.entries_under(&dir)
			.filter(move |(path, _)| is_under(path, &dir));
		match self.side {
			// Read off the stage in path order, a page at a time.
			Side::Staged(_) => Either::Left(under),
			Side::Whole(_) | Side::Carried(_) => Either::Right(in_path_order(under)),
		}
	}
}

impl<T: FromRow + Kinded> SideRef<'_, T> {
	/// Every directory this side holds, with its node, in path order: what the directory-move fold
	/// walks the two sides and the rows side by side in (see `plan::fold_dir_moves`). A staged side
	/// reads them off its stage's index of directories, in that order and reading none of its
	/// files; the others walk every node and sort the directories.
	pub(super) fn dirs(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		let is_dir = |(_, node): &(Cow<'_, str>, Cow<'_, T>)| node.kind() == NodeKind::Dir;
		match self.side {
			Side::Staged(staged) => {
				Either::Left(Entries::Staged(staged.dir_pages()).filter(is_dir))
			}
			Side::Whole(_) | Side::Carried(_) => {
				Either::Right(in_path_order(self.entries().filter(is_dir)))
			}
		}
	}
}

/// What the tests copy and read sides with.
#[cfg(test)]
pub(super) mod test_support;

#[cfg(test)]
mod tests;
