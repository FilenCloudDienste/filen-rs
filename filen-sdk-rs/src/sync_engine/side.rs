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
//! where it was — and a derived one hands out what it just built.
//!
//! # The two backings
//!
//! [`Side::Whole`] is the path-keyed map a whole read produces. [`Side::Carried`] is an
//! [`Overlay`] — one entry per path THIS pass read — over the rows the resident baseline already
//! describes: a carried row's local and remote nodes are both derived from that row on demand, so
//! a change-scoped pass holds per-PASS data instead of a second copy of the tree.
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
//! move the rows, while the source path still names them.
//!
//! [`Side::retain`] is a whole-side edit by construction — it asks about every node — so a carried
//! side pays the tree for it, exactly as [`Nodes`] says in its own docs. A caller that knows which
//! paths it is about should say so instead ([`Side::remove`], [`Side::subtree_paths`]).

#[cfg(feature = "bench-internals")]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
	borrow::Cow,
	collections::{HashMap, hash_map},
};

use super::{
	baseline::BaselineEntry,
	plan::{is_under, moved_path},
	rows::Baseline,
	scan::at_or_under_folded,
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
		Nodes::iter(self).filter(move |(path, _)| is_under(path, dir))
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
/// One entry per path the pass actually read: `Some(node)` where it found something, `None` where
/// it found nothing. That `None` is a TOMBSTONE, and it is the only thing that can take a carried
/// row's node out of the side — invariant I1 as a data structure, since the only writer of one is
/// an observation about that very path.
///
/// A path with NO entry is one the pass got no evidence for, and its row still speaks for it.
/// That is the whole difference between "observed absent" and "not looked at", which is why this
/// is a map to `Option<T>` rather than a map plus a set of removals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Overlay<T> {
	edits: HashMap<String, Option<T>>,
}

impl<T> Default for Overlay<T> {
	fn default() -> Self {
		Self {
			edits: HashMap::new(),
		}
	}
}

impl<T> Overlay<T> {
	/// What the pass recorded at `path`; `None` when it recorded nothing there at all.
	fn get(&self, path: &str) -> Option<&Option<T>> {
		self.edits.get(path)
	}

	/// Whether the side this overlay corrects holds `path`: what the pass recorded there, or —
	/// where it recorded nothing — whether the baseline has a row it can carry.
	///
	/// The ONE implementation of that question, answered without building a node.
	fn holds(&self, baseline: &Baseline, path: &str) -> bool {
		match self.get(path) {
			Some(edit) => edit.is_some(),
			None => baseline.carryable(path),
		}
	}

	fn iter(&self) -> impl Iterator<Item = (&String, &Option<T>)> {
		self.edits.iter()
	}

	/// How many paths the pass recorded — the size of the per-pass data, never of the side.
	#[cfg(feature = "bench-internals")]
	fn capacity(&self) -> usize {
		self.edits.capacity()
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Side<T> {
	/// Every node, as a whole read produced it.
	Whole(HashMap<String, T>),
	/// What this pass observed, over the rows the baseline carries.
	Carried(Overlay<T>),
}

impl<T> Side<T> {
	/// A side that derives its nodes from the baseline's rows, holding only what a pass observes.
	pub(super) fn carried() -> Self {
		Self::Carried(Overlay::default())
	}

	/// How many nodes this side has room for. Only the probe's memory accounting asks. For a
	/// CARRIED side this is the per-pass data and not the tree, which is the whole point of it.
	#[cfg(feature = "bench-internals")]
	pub(crate) fn capacity(&self) -> usize {
		match self {
			Self::Whole(map) => map.capacity(),
			Self::Carried(overlay) => overlay.capacity(),
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

	/// Read a WHOLE side against an empty tree, for a test that has no baseline to hand.
	///
	/// Exact rather than a stand-in: a whole side's backing never consults a baseline. It ASSERTS
	/// the backing, because a carried side read this way would answer "holds nothing" for every
	/// row it carries and turn the assertions that follow into ones that cannot fail.
	///
	/// The probe reads its whole-pass sides through this too, which is why it is not `cfg(test)`:
	/// that module is compiled into a binary built WITHOUT `cfg(test)`.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn whole(&self) -> SideRef<'_, T> {
		assert!(
			matches!(self, Self::Whole(_)),
			"a carried side must be read against the baseline it derives from, not an empty one"
		);
		static EMPTY: std::sync::OnceLock<Baseline> = std::sync::OnceLock::new();
		self.of(EMPTY.get_or_init(Baseline::default))
	}

	/// The paths this side holds that no baseline row put there — what THIS pass's own producers
	/// added: the local observation, the remote delta, the fold of unacknowledged writes.
	///
	/// A whole side answers with every path it holds, which is all it has. A carried one answers
	/// with its OVERLAY alone, and that is exact rather than a shortcut:
	/// [`derive::from_baseline`](super::derive::from_baseline) places nothing, so every other path
	/// such a side holds IS a baseline row answering for itself. That is what lets the assembly
	/// check cost the change rather than the tree.
	pub(super) fn own_keys(&self) -> OwnKeys<'_, T> {
		match self {
			Self::Whole(map) => OwnKeys::Whole(map.keys()),
			Self::Carried(overlay) => OwnKeys::Carried(overlay.edits.iter()),
		}
	}

	/// This side's nodes by value, for a test that owns a WHOLE one and wants to feed them
	/// somewhere. Asserts the backing for the reason [`whole`](Self::whole) does: a carried side's
	/// nodes are not held anywhere to be handed over.
	#[cfg(test)]
	pub(super) fn into_whole(self) -> HashMap<String, T> {
		match self {
			Self::Whole(map) => map,
			Self::Carried(_) => panic!("a carried side holds no map to hand over"),
		}
	}

	/// Put `node` at `path`.
	///
	/// Returns nothing, where a map's `insert` hands back what it displaced: a carried side would
	/// have to DERIVE the displaced node, and the one caller that wanted it
	/// (`engine::place_node`) asks through [`NodesAt::at`] first, which is one lookup on the
	/// path it is already holding.
	pub(crate) fn insert(&mut self, path: String, node: T) {
		match self {
			Self::Whole(map) => {
				map.insert(path, node);
			}
			Self::Carried(overlay) => {
				overlay.edits.insert(path, Some(node));
			}
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
			Self::Carried(overlay) => {
				let was = match overlay.edits.get(path) {
					// Already a tombstone: nothing to take, and the tombstone stands.
					Some(None) => return None,
					Some(Some(node)) => Some(node.clone()),
					None => baseline.get(path).as_ref().and_then(T::from_row),
				};
				// Recorded either way: a `remove` is an observation about this path, and a path
				// the side was not describing is one nothing is carried at. Writing the tombstone
				// unconditionally keeps `holds` false there whatever the rows later say.
				overlay.edits.insert(path.to_owned(), None);
				was
			}
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
					overlay.edits.insert(path, None);
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
			Self::Carried(overlay) => {
				let mut out: Vec<String> = Vec::new();
				// The rows, from the baseline's own subtree walk — the size of the directory
				// rather than of the tree — minus whatever this pass observed away.
				baseline.visit_subtree_paths(dir, |path| {
					if overlay.get(path).is_none() && baseline.carryable(path) {
						out.push(path.to_owned());
					}
				});
				// What the pass PUT somewhere under `dir`, which no row need name.
				out.extend(
					overlay
						.iter()
						.filter(|(path, edit)| edit.is_some() && is_under(path, dir))
						.map(|(path, _)| path.clone()),
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
			Self::Carried(overlay) => {
				let moving: Vec<(String, String)> = overlay
					.edits
					.keys()
					.filter_map(|key| Some((key.clone(), moved_path(key, from, to)?)))
					.collect();
				// The landing paths first — the destinations a row arrives at, and no other path
				// under `to` (see the note above). Taken from the rows about to move rather than
				// from the destination's subtree, so it is the size of what moves.
				let mut landing: Vec<String> = Vec::new();
				if baseline.contains_key(from) {
					landing.push(to.to_owned());
				}
				baseline.visit_subtree_paths(from, |path| {
					if let Some(dest) = moved_path(path, from, to) {
						landing.push(dest);
					}
				});
				for key in landing {
					overlay.edits.remove(&key);
				}
				for (old, new) in moving {
					let Some(mut edit) = overlay.edits.remove(&old) else {
						continue;
					};
					if let Some(node) = &mut edit {
						set_path(node, &new);
					}
					overlay.edits.insert(new, edit);
				}
			}
		}
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
			Self::Carried(overlay) => overlay
				.edits
				.extend(nodes.into_iter().map(|(path, node)| (path, Some(node)))),
		}
	}
}

/// The keys a side holds that no baseline row placed there (see [`Side::own_keys`]).
pub(super) enum OwnKeys<'a, T> {
	Whole(hash_map::Keys<'a, String, T>),
	Carried(hash_map::Iter<'a, String, Option<T>>),
}

impl<'a, T> Iterator for OwnKeys<'a, T> {
	type Item = &'a str;

	fn next(&mut self) -> Option<Self::Item> {
		match self {
			Self::Whole(keys) => keys.next().map(String::as_str),
			// A tombstone holds nothing, so it is not a key this side has.
			Self::Carried(edits) => {
				for (path, edit) in edits.by_ref() {
					if edit.is_some() {
						return Some(path.as_str());
					}
				}
				None
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
	/// Every path this side holds with its node, as borrowed pairs for a materialized map and
	/// owned ones for a carried side.
	///
	/// O(tree) on a carried side, which is what [`Nodes`] means by its own existence: a backing
	/// that derives its nodes can only enumerate them by walking the rows.
	fn entries(&self) -> Entries<'_, T> {
		match self.side {
			Side::Whole(map) => Entries::Whole(map.iter()),
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
					if overlay.get(&row.rel_path).is_some() {
						return;
					}
					if let Some(node) = T::from_row(row) {
						out.push((row.rel_path.clone(), node));
					}
				});
				out.extend(overlay.iter().filter_map(|(path, edit)| {
					edit.as_ref().map(|node| (path.clone(), node.clone()))
				}));
				Entries::Carried(out.into_iter())
			}
		}
	}

	/// The candidates for [`Nodes::under`]: the subtree rather than the tree, so a carried side
	/// answers a directory question in the size of the directory.
	fn entries_under(&self, dir: &str) -> Entries<'_, T> {
		match self.side {
			Side::Whole(map) => Entries::Whole(map.iter()),
			Side::Carried(overlay) => {
				let mut out: Vec<(String, T)> = Vec::new();
				#[cfg(feature = "bench-internals")]
				CARRIED_SUBTREE_CALLS.fetch_add(1, Ordering::Relaxed);
				for row in self.baseline.subtree(dir) {
					if overlay.get(&row.rel_path).is_some() {
						continue;
					}
					if let Some(node) = T::from_row(&row) {
						out.push((row.rel_path.clone(), node));
					}
				}
				#[cfg(feature = "bench-internals")]
				CARRIED_SUBTREE_ROWS.fetch_add(out.len() as u64, Ordering::Relaxed);
				out.extend(overlay.iter().filter_map(|(path, edit)| {
					(is_under(path, dir))
						.then(|| edit.as_ref().map(|node| (path.clone(), node.clone())))
						.flatten()
				}));
				Entries::Carried(out.into_iter())
			}
		}
	}
}

/// The two backings' entries as one iterator, so [`Nodes`] can return a single type without the
/// materialized side paying an allocation it did not pay before.
enum Entries<'a, T> {
	Whole(hash_map::Iter<'a, String, T>),
	Carried(std::vec::IntoIter<(String, T)>),
}

impl<'a, T: Clone> Iterator for Entries<'a, T> {
	type Item = (Cow<'a, str>, Cow<'a, T>);

	fn next(&mut self) -> Option<Self::Item> {
		match self {
			Self::Whole(entries) => entries
				.next()
				.map(|(path, node)| (Cow::Borrowed(path.as_str()), Cow::Borrowed(node))),
			Self::Carried(entries) => entries
				.next()
				.map(|(path, node)| (Cow::Owned(path), Cow::Owned(node))),
		}
	}
}

impl<T: FromRow> NodesAt for SideRef<'_, T> {
	type Node = T;

	fn at(&self, path: &str) -> Option<Cow<'_, T>> {
		match self.side {
			Side::Whole(map) => map.get(path).map(Cow::Borrowed),
			Side::Carried(overlay) => match overlay.get(path) {
				Some(Some(node)) => Some(Cow::Borrowed(node)),
				// Observed absent: the tombstone answers, and the row behind it does not.
				Some(None) => None,
				None => self
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
			// Answered WITHOUT building the node, which is the whole reason `holds` exists apart
			// from `at().is_some()`.
			Side::Carried(overlay) => overlay.holds(self.baseline, path),
		}
	}

	fn occupied(&self, path: &str) -> bool {
		match self.side {
			Side::Whole(map) => map.keys().any(|key| at_or_under_folded(key, path)),
			Side::Carried(overlay) => {
				// What the pass observed is the only part of this side that is not a row, so it is
				// the only part folded key by key — and it is the per-pass data, not the tree.
				if overlay
					.iter()
					.any(|(key, edit)| edit.is_some() && at_or_under_folded(key, path))
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
			Side::Carried(overlay) => {
				// The rows this side carries, corrected by what the pass recorded over them. Each
				// overlay key is distinct, so a tombstone can only cancel a row that was counted.
				let mut added = 0usize;
				let mut removed = 0usize;
				for (path, edit) in overlay.iter() {
					match (edit.is_some(), self.baseline.carryable(path)) {
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
			Side::Carried(_) => Nodes::len(self) == 0,
		}
	}

	fn paths(&self) -> impl Iterator<Item = Cow<'_, str>> {
		self.entries().map(|(path, _)| path)
	}

	fn iter(&self) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		self.entries()
	}

	fn under(&self, dir: &str) -> impl Iterator<Item = (Cow<'_, str>, Cow<'_, T>)> {
		let under = dir.to_owned();
		self.entries_under(dir)
			.filter(move |(path, _)| is_under(path, &under))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A marker node is never derived from a row — it is not either side of a pass — so a carried
	/// side of one holds exactly its overlay, which is what the overlay tests below want.
	impl FromRow for u32 {
		fn from_row(_: &BaselineEntry) -> Option<Self> {
			None
		}
	}

	impl FromRow for String {
		fn from_row(_: &BaselineEntry) -> Option<Self> {
			None
		}
	}

	/// The owning shape, over a materialized map: what it holds is what the traits answer for.
	#[test]
	fn a_side_is_the_map_it_holds() {
		let rows = Baseline::default();
		let mut side: Side<u32> = Side::default();
		assert!(Nodes::is_empty(&side.whole()));
		side.insert("docs".to_owned(), 0);
		side.extend([("docs/a.txt".to_owned(), 1), ("docsx".to_owned(), 2)]);

		assert_eq!(Nodes::len(&side.whole()), 3);
		assert_eq!(side.whole().at("docs/a.txt").as_deref(), Some(&1));
		assert!(side.whole().holds("docs"));
		assert!(side.whole().occupied("DOCS/A.TXT"));
		assert_eq!(
			side.subtree_paths(&rows, "docs"),
			vec!["docs/a.txt".to_owned()]
		);
		assert_eq!(side.remove(&rows, "docsx"), Some(2));
		assert_eq!(side.remove(&rows, "docsx"), None);

		side.retain(&rows, |path, _| path != "docs/a.txt");
		assert_eq!(side.whole().paths().collect::<Vec<_>>(), vec!["docs"]);
	}

	/// Re-keying takes the subtree AND the directory itself, and tells each node where it landed.
	#[test]
	fn rekeying_a_subtree_moves_the_root_with_it() {
		let rows = Baseline::default();
		let mut side: Side<String> = ["docs", "docs/a.txt", "docs/deep/b.txt", "docsx", "other"]
			.into_iter()
			.map(|path| (path.to_owned(), path.to_owned()))
			.collect();

		side.rekey_subtree(&rows, "docs", "notes", |node, path| *node = path.to_owned());

		let mut paths: Vec<String> = side.whole().paths().map(Cow::into_owned).collect();
		paths.sort();
		assert_eq!(
			paths,
			vec!["docsx", "notes", "notes/a.txt", "notes/deep/b.txt", "other"]
		);
		assert!(
			side.whole().iter().all(|(path, node)| *node == *path),
			"every moved node was told its new path"
		);
	}

	/// Two sides are equal when they hold the same nodes at the same paths.
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

	/// A carried side read against an empty tree would answer "holds nothing" for every row it
	/// carries, so the test-only reader refuses it rather than letting the assertions go quiet.
	#[test]
	#[should_panic(expected = "must be read against the baseline it derives from")]
	fn the_test_reader_refuses_a_carried_side() {
		let side: Side<u32> = Side::carried();
		let _ = side.whole();
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

	// ------------------------------------------------------------------------------------------
	// The oracle: the materialised backing is the expectation the derived one is held to.
	//
	// Both backings are built over the SAME generated tree, handed the SAME operations in the same
	// order, and compared after every one of them. The expectation is not written down here — it is
	// whatever `Side::Whole` answers, which is the backing a whole read produces. It is an oracle
	// for the CONTAINER: what the two answer given the same per-row derivation (see
	// [`materialised`]), not whether that derivation is right.
	// ------------------------------------------------------------------------------------------

	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use rand::{Rng, SeedableRng, rngs::StdRng};
	use uuid::Uuid;

	use super::super::baseline::{BaselineState, NodeKind};
	use super::super::plan::RemoteNode;

	const CASES: u64 = 200;

	/// The paths a case is drawn from, `(path, is_dir)`: nesting, two case-only sibling pairs
	/// (`a`/`A`, `top.txt`/`Top.txt`), a pair an ASCII-only fold keeps apart (`Ä`/`ä`), a name that
	/// is a string prefix of a sibling's (`a/b` and `a/b.txt`) and of another directory's (`a`,
	/// `ab`), a file and a directory sharing a stem (`z`, `z.txt`), and a childless directory for a
	/// re-key to carry. A deep path whose ancestors draw `NoRow` leaves PATH-ONLY nodes in the
	/// tree, which is a shape a pass meets and neither side holds a node for.
	const PATHS: &[(&str, bool)] = &[
		("a", true),
		("a/b", true),
		("a/b/c.txt", false),
		("a/b.txt", false),
		("a/B.txt", false),
		("ab", true),
		("ab/x.txt", false),
		("A", true),
		("A/y.txt", false),
		("d", true),
		("d/e", true),
		("d/e/f.txt", false),
		("top.txt", false),
		("Top.txt", false),
		("\u{c4}.txt", false),
		("\u{e4}.txt", false),
		("z", true),
		("z/1.txt", false),
		("z.txt", false),
		("empty", true),
	];

	/// Every row state the baseline can hold, plus the two half-recorded shapes a resolution writes
	/// on purpose and a file row missing a field its local half needs.
	#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
	enum Shape {
		/// No row at all: the path is in the tree only if a descendant put it there.
		NoRow,
		Synced,
		/// A push no snapshot has confirmed yet — still `Synced`, so still carryable.
		Unconfirmed,
		/// What a `KeepLocal` resolution leaves: the remote half cleared.
		NoRemoteHalf,
		/// What a `KeepRemote` resolution leaves: the local half cleared.
		NoLocalHalf,
		Conflicted,
		Overwritten,
		Adopted,
		/// A file row that records no size: it describes no side fully, so it describes neither.
		FileMissingSize,
	}

	const SHAPES: [Shape; 9] = [
		Shape::NoRow,
		Shape::Synced,
		Shape::Unconfirmed,
		Shape::NoRemoteHalf,
		Shape::NoLocalHalf,
		Shape::Conflicted,
		Shape::Overwritten,
		Shape::Adopted,
		Shape::FileMissingSize,
	];

	fn uuid_at(index: usize) -> Uuid {
		Uuid::from_u128(1000 + index as u128)
	}

	fn node_at(index: usize, version: u8) -> RemoteNode {
		let (path, is_dir) = PATHS[index % PATHS.len()];
		RemoteNode {
			rel_path: path.to_owned(),
			kind: if is_dir {
				NodeKind::Dir
			} else {
				NodeKind::File
			},
			remote_uuid: uuid_at(index),
			stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid_at(index))),
			content_hash: (!is_dir).then(|| Blake3Hash::from([version; 32])),
			size: index as u64,
			modified_millis: 1_700_000_000_000 + index as i64,
		}
	}

	fn row_at(index: usize, shape: Shape) -> Option<BaselineEntry> {
		if shape == Shape::NoRow {
			return None;
		}
		let (path, is_dir) = PATHS[index];
		let uuid = uuid_at(index);
		let hash = Blake3Hash::from([(index % 251) as u8; 32]);
		let mut row = BaselineEntry {
			rel_path: path.to_owned(),
			kind: if is_dir {
				NodeKind::Dir
			} else {
				NodeKind::File
			},
			remote_uuid: Some(uuid),
			content_hash: (!is_dir).then_some(hash),
			size: (!is_dir).then_some(index as u64),
			local_mtime: Some(1_700_000_000_000 + index as i64),
			remote_modified: Some(1_700_000_000_500 + index as i64),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
			agreed_hash: (!is_dir).then_some(hash),
		};
		match shape {
			Shape::NoRow => unreachable!("returned above"),
			Shape::Synced => {}
			Shape::Unconfirmed => row.agreed_hash = None,
			Shape::NoRemoteHalf => row.remote_uuid = None,
			Shape::NoLocalHalf => {
				row.content_hash = None;
				row.size = None;
				row.local_mtime = None;
			}
			Shape::Conflicted => row.state = BaselineState::Conflicted,
			Shape::Overwritten => row.state = BaselineState::Overwritten,
			Shape::Adopted => row.state = BaselineState::Adopted,
			Shape::FileMissingSize => row.size = None,
		}
		Some(row)
	}

	/// The side a whole read would have produced over `baseline`: one node per row that stands in
	/// for both sides.
	///
	/// WHAT THIS PINS, exactly: both backings are filled through the same per-row derivation
	/// (`derive::carried`, which is what `RemoteNode::from_row` is), so the oracle below compares
	/// the two CONTAINERS and not the derivation. A per-row bug — a field carried wrong, a
	/// carryability gate wrong — would produce the same wrong node on both sides and pass here.
	/// What checks the derivation itself is `derive`'s `assert_same_maps`, which holds a derived
	/// side against a real local scan and a real remote view.
	fn materialised(baseline: &Baseline) -> Side<RemoteNode> {
		let mut map: HashMap<String, RemoteNode> = HashMap::new();
		baseline.visit_rows(|row| {
			if let Some(node) = RemoteNode::from_row(row) {
				map.insert(row.rel_path.clone(), node);
			}
		});
		Side::Whole(map)
	}

	/// Every path the two backings are questioned about: the corpus, spellings that fold onto it,
	/// paths no row ever held, and the empty path.
	fn probe_paths() -> Vec<String> {
		let mut out: Vec<String> = PATHS.iter().map(|(path, _)| (*path).to_owned()).collect();
		out.extend(
			[
				"",
				"A/Y.TXT",
				"TOP.TXT",
				"\u{c4}.TXT",
				"a/b/c.TXT",
				"nope",
				"a/nope",
				"z.tx",
				"zz",
				"moved0",
				"moved1",
				"moved0/1.txt",
			]
			.into_iter()
			.map(str::to_owned),
		);
		out
	}

	fn sorted_paths(side: &SideRef<'_, RemoteNode>) -> Vec<String> {
		let mut out: Vec<String> = side.paths().map(Cow::into_owned).collect();
		out.sort();
		out
	}

	fn sorted_entries(side: &SideRef<'_, RemoteNode>) -> Vec<(String, RemoteNode)> {
		let mut out: Vec<(String, RemoteNode)> = side
			.iter()
			.map(|(path, node)| (path.into_owned(), node.into_owned()))
			.collect();
		out.sort_by(|left, right| left.0.cmp(&right.0));
		out
	}

	/// Every read the two traits expose, asked of both backings.
	///
	/// ITERATION ORDER is deliberately not compared: a map has the hash order of its table and a
	/// derived side has the tree's, so every enumeration is sorted first. A consumer whose output
	/// depended on the raw order was depending on an accident.
	fn assert_answers_alike(
		seed: u64,
		step: &str,
		baseline: &Baseline,
		whole: &Side<RemoteNode>,
		carried: &Side<RemoteNode>,
	) {
		let (w, c) = (whole.of(baseline), carried.of(baseline));
		let where_ = format!("seed {seed} after {step}");
		let (pw, pc) = (sorted_paths(&w), sorted_paths(&c));
		if pw != pc {
			let overlay_at = |path: &str| match carried {
				Side::Whole(_) => "<whole>".to_owned(),
				Side::Carried(overlay) => match overlay.get(path) {
					None => "no-entry".to_owned(),
					Some(None) => "TOMBSTONE".to_owned(),
					Some(Some(_)) => "live".to_owned(),
				},
			};
			for path in pw.iter().chain(pc.iter()) {
				if pw.contains(path) != pc.contains(path) {
					let row = baseline.get(path);
					eprintln!(
						"  DIVERGED at {path:?}: whole={} carried={} | index.carryable={} contains_key={} overlay={}",
						pw.contains(path),
						pc.contains(path),
						baseline.carryable(path),
						baseline.contains_key(path),
						overlay_at(path),
					);
					eprintln!(
						"      row: present={} row.carryable()={:?} state={:?} kind={:?} uuid={} size={:?} mtime={:?} hash={} rmod={:?}",
						row.is_some(),
						row.as_ref().map(BaselineEntry::carryable),
						row.as_ref().map(|r| r.state),
						row.as_ref().map(|r| r.kind),
						row.as_ref().is_some_and(|r| r.remote_uuid.is_some()),
						row.as_ref().and_then(|r| r.size),
						row.as_ref().and_then(|r| r.local_mtime),
						row.as_ref().is_some_and(|r| r.content_hash.is_some()),
						row.as_ref().and_then(|r| r.remote_modified),
					);
				}
			}
		}
		assert_eq!(pw, pc, "{where_}: paths");
		assert_eq!(Nodes::len(&w), Nodes::len(&c), "{where_}: len");
		assert_eq!(
			Nodes::is_empty(&w),
			Nodes::is_empty(&c),
			"{where_}: is_empty"
		);
		assert_eq!(sorted_entries(&w), sorted_entries(&c), "{where_}: iter");
		for path in probe_paths() {
			assert_eq!(
				w.at(&path).map(Cow::into_owned),
				c.at(&path).map(Cow::into_owned),
				"{where_}: at({path:?})"
			);
			assert_eq!(w.holds(&path), c.holds(&path), "{where_}: holds({path:?})");
			assert_eq!(
				w.occupied(&path),
				c.occupied(&path),
				"{where_}: occupied({path:?})"
			);
		}
		for (dir, _) in PATHS.iter().filter(|(_, is_dir)| *is_dir) {
			let mut under_w: Vec<String> =
				w.under(dir).map(|(path, _)| path.into_owned()).collect();
			let mut under_c: Vec<String> =
				c.under(dir).map(|(path, _)| path.into_owned()).collect();
			under_w.sort();
			under_c.sort();
			assert_eq!(under_w, under_c, "{where_}: under({dir:?})");
			let (mut sub_w, mut sub_c) = (
				whole.subtree_paths(baseline, dir),
				carried.subtree_paths(baseline, dir),
			);
			sub_w.sort();
			sub_c.sort();
			assert_eq!(sub_w, sub_c, "{where_}: subtree_paths({dir:?})");
		}
	}

	/// Both backings answer alike over every row shape, through every mutation the type exposes.
	///
	/// The directory move is the one step whose two forms differ on purpose: a materialised side
	/// re-keys its own nodes, where a carried side re-keys its OVERLAY and lets
	/// `Baseline::move_subtree` carry the rows it derives from. Running them in that order, against
	/// one shared tree, is what pins the contract `Side::rekey_subtree` documents.
	#[test]
	fn a_carried_side_answers_exactly_as_the_materialised_one() {
		let mut shapes_seen: std::collections::BTreeSet<Shape> = std::collections::BTreeSet::new();
		let mut carried_rows = 0usize;
		let mut uncarried_rows = 0usize;
		let mut moves = 0usize;
		for seed in 0..CASES {
			let mut rng = StdRng::seed_from_u64(seed);
			let rows: Vec<BaselineEntry> = (0..PATHS.len())
				.filter_map(|index| {
					let shape = SHAPES[rng.random_range(0..SHAPES.len())];
					shapes_seen.insert(shape);
					row_at(index, shape)
				})
				.collect();
			let mut baseline = Baseline::from_rows(rows);
			baseline.visit_rows(|row| {
				if row.carryable() {
					carried_rows += 1;
				} else {
					uncarried_rows += 1;
				}
			});

			let mut whole = materialised(&baseline);
			let mut carried = Side::carried();
			assert_answers_alike(seed, "assembly", &baseline, &whole, &carried);

			for step in 0..8u32 {
				let index = rng.random_range(0..PATHS.len());
				let path = PATHS[index].0;
				let what;
				match rng.random_range(0..5) {
					0 => {
						what = format!("insert({path:?})");
						let node = node_at(index, step as u8 + 1);
						whole.insert(path.to_owned(), node.clone());
						carried.insert(path.to_owned(), node);
					}
					1 => {
						// A path NO row tracks, which only the overlay can hold.
						let fresh = format!("{path}/fresh{step}");
						what = format!("insert-fresh({fresh:?})");
						let node = node_at(index, step as u8 + 1);
						whole.insert(fresh.clone(), node.clone());
						carried.insert(fresh, node);
					}
					2 => {
						what = format!("remove({path:?})");
						assert_eq!(
							whole.remove(&baseline, path),
							carried.remove(&baseline, path),
							"seed {seed} step {step}: remove({path:?}) handed back different nodes"
						);
					}
					3 => {
						let keep_files = rng.random_range(0..2) == 0;
						what = format!("retain(files={keep_files})");
						whole.retain(&baseline, |_, node| {
							(node.kind == NodeKind::File) == keep_files
						});
						carried.retain(&baseline, |_, node| {
							(node.kind == NodeKind::File) == keep_files
						});
					}
					_ => {
						// A destination nothing holds, which is the only kind a pass ever folds
						// onto: `next_dir_move` and `next_case_only_dir_rename` both refuse an
						// occupied one. Moving onto an occupied destination OVERWRITES the row
						// sitting there (`move_subtree` is `remove_subtrees` + `upsert`), and a
						// side assembled before that cannot see it — an input no pass produces,
						// so generating it would compare the two backings on an unreachable
						// state rather than on anything the engine can reach.
						let to = format!("moved{step}");
						assert!(
							!baseline.contains_key(&to)
								&& !whole.of(&baseline).occupied(&to)
								&& !carried.of(&baseline).occupied(&to),
							"seed {seed} step {step}: destination {to:?} is occupied, which is \
							 the one shape a fold never produces"
						);
						what = format!("rekey({path:?} -> {to:?})");
						// The SIDES first, then the rows: a carried side re-keys only what this
						// pass observed, and the rows it derives from move underneath it.
						whole.rekey_subtree(&baseline, path, &to, |node, at| {
							node.rel_path = at.to_owned();
						});
						carried.rekey_subtree(&baseline, path, &to, |node, at| {
							node.rel_path = at.to_owned();
						});
						baseline.move_subtree(path, &to);
						moves += 1;
					}
				}
				assert_answers_alike(
					seed,
					&format!("step {step} {what}"),
					&baseline,
					&whole,
					&carried,
				);
			}
		}
		// A corpus that stopped producing these would let the properties above pass by proving
		// nothing about the shapes the two backings can actually disagree on.
		assert_eq!(shapes_seen.len(), SHAPES.len(), "{shapes_seen:?}");
		assert!(carried_rows > 500, "only {carried_rows} carryable row(s)");
		assert!(
			uncarried_rows > 500,
			"only {uncarried_rows} uncarryable row(s)"
		);
		assert!(moves > 100, "only {moves} directory move(s)");
	}

	/// A row at an arbitrary path, in the one shape both sides carry.
	fn row_named(index: usize, path: &str) -> BaselineEntry {
		let mut row = row_at(index, Shape::Synced).expect("a synced row");
		row.rel_path = path.to_owned();
		row
	}

	/// The carried-walk counters move when a carried side really is walked whole.
	///
	/// Every benchmark row publishes zero for these, and so would a counter that is never
	/// incremented at all: a WHOLE pass takes the [`Side::Whole`] arm, so no scenario in the
	/// harness can tell a correct zero from a mis-wired one. The published zeros are evidence that
	/// a change-scoped pass holds no second copy of the tree ONLY if this fires.
	///
	/// Asserted as DELTAS rather than absolutes: the counters are process-global and the tests
	/// around this one walk carried sides of their own, on other threads, in the same process. A
	/// delta cannot be fooled in the direction that matters — a counter that never moves fails this
	/// however the rest of the suite is scheduled — where an equality would be flaky, which is its
	/// own bug.
	#[cfg(feature = "bench-internals")]
	#[test]
	fn a_carried_side_walked_whole_is_counted() {
		let baseline =
			Baseline::from_rows([row_named(0, "a"), row_named(1, "b"), row_named(2, "c")]);
		let rows = baseline.carryable_rows() as u64;
		assert!(
			rows > 0,
			"the fixture carries no rows, so walking it would count nothing and this test could \
			 not fail"
		);
		let mut side: Side<u32> = Side::carried();
		side.insert("placed.txt".to_owned(), 7);

		let before = carried_walks();
		let walked: Vec<String> = side.of(&baseline).paths().map(Cow::into_owned).collect();
		let after = carried_walks();

		assert!(
			after.whole_calls > before.whole_calls,
			"a carried side was walked whole and the call went uncounted"
		);
		assert!(
			after.whole_rows >= before.whole_rows + rows,
			"a carried side walked {rows} row(s) and the row counter did not move by them"
		);
		// And the walk actually ran to the end: a marker node derives nothing from a row, so the
		// overlay entry is the one path such a side holds.
		assert_eq!(walked, vec!["placed.txt".to_string()]);
	}

	/// A subtree moving onto a path this pass already recorded something at: the TOMBSTONES there
	/// go, and what a producer PLACED there stays.
	///
	/// Directed because the generated oracle cannot reach either half — every destination it folds
	/// onto is a fresh `moved{step}`, and nothing is ever keyed under that prefix, so the sweep
	/// `rekey_subtree` runs over the destination is dead code in that corpus. Both halves are real:
	/// a tombstone left standing answers "absent" over the rows that have just arrived, and a live
	/// entry swept away is a node the whole backing keeps. The expectation is the materialised
	/// backing, as it is above.
	#[test]
	fn a_move_supersedes_the_destination_tombstones_and_spares_the_rest() {
		let mut baseline = Baseline::from_rows([
			row_named(0, "dst"),
			row_named(2, "dst/keep.txt"),
			row_named(1, "src"),
			row_named(3, "src/x.txt"),
		]);
		let mut whole = materialised(&baseline);
		let mut carried = Side::carried();
		assert_answers_alike(0, "assembly", &baseline, &whole, &carried);

		// What `hide`, `resolve_collisions` and `merge_local`'s `drop_rows` leave at a destination:
		// an observation that nothing is there.
		for path in ["dst", "dst/keep.txt"] {
			assert_eq!(
				whole.remove(&baseline, path),
				carried.remove(&baseline, path),
				"remove({path:?}) handed back different nodes"
			);
		}
		// And what a producer placed there: one beside the landing paths, which no move of `src`
		// touches, and one ON a landing path, which the arriving row supersedes.
		for (at, index) in [("dst/live.txt", 4), ("dst/x.txt", 6)] {
			let mut live = node_at(index, 1);
			live.rel_path = at.to_owned();
			whole.insert(at.to_owned(), live.clone());
			carried.insert(at.to_owned(), live);
		}
		assert_answers_alike(0, "the destination recorded", &baseline, &whole, &carried);

		// The sides first, then the rows, as `fold_dir_moves` runs them.
		whole.rekey_subtree(&baseline, "src", "dst", |node, at| {
			node.rel_path = at.to_owned()
		});
		carried.rekey_subtree(&baseline, "src", "dst", |node, at| {
			node.rel_path = at.to_owned()
		});
		baseline.move_subtree("src", "dst");
		assert_answers_alike(0, "src -> dst", &baseline, &whole, &carried);

		assert_eq!(
			sorted_paths(&carried.of(&baseline)),
			["dst", "dst/live.txt", "dst/x.txt"],
			"the moved subtree answers over the tombstones at the paths it LANDS on; the node a \
			 producer placed beside them survives, and the tombstone over the row the move never \
			 touched (`dst/keep.txt`, which `move_subtree` leaves where it is) still stands"
		);
		assert_eq!(
			carried
				.of(&baseline)
				.at("dst/x.txt")
				.expect("the moved row answers")
				.remote_uuid,
			uuid_at(3),
			"the row that MOVED onto the path answers there, not the node a producer had put on it"
		);
	}

	/// The pair root is a path like any other to ASK about, and a side holding a node there is
	/// occupied at it whichever backing it has.
	///
	/// The generator never places one, so the empty path the oracle probes every step is a
	/// comparison neither backing can fail there; this is the state that makes it discriminate.
	#[test]
	fn both_backings_answer_alike_at_the_pair_root() {
		let baseline = Baseline::from_rows([row_named(0, "a")]);
		let mut whole = materialised(&baseline);
		let mut carried = Side::carried();
		let mut node = node_at(2, 1);
		node.rel_path = String::new();
		whole.insert(String::new(), node.clone());
		carried.insert(String::new(), node);

		assert_answers_alike(0, "a node at the pair root", &baseline, &whole, &carried);
		assert!(
			carried.of(&baseline).occupied(""),
			"a side holding a node at the pair root is occupied there"
		);
	}
}
