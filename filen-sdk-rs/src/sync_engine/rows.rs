//! The baseline rows a pass reads, behind the one type every consumer reads them through.
//!
//! [`Baseline`] is a BOUNDARY, not a structure: today it is backed by the store's resident
//! [`Tree`], shared as an `Arc`, and every method forwards. It exists so that the backing can
//! change — a per-pass overlay of edits, then rows read from SQLite — without touching a consumer.
//! No consumer names [`Tree`], a `NodeId` or an `Arc` of either; the store hands out `Baseline`s
//! and keeps the trees.
//!
//! Every method is shaped so that a SQLite backing can answer it with ONE indexed query or ONE
//! streamed cursor. The table below is what the backing swap is designed from: each question, how
//! often the engine asks it, and what answers it off the `baseline` table. The counts are call
//! sites as the compiler found them when this boundary went in (every method marked
//! `#[deprecated]` once, the warnings classified): "prod" is the engine, "bench" the benchmark
//! harness, the probe and the engine's `bench_*` seams, "test" the `#[cfg(test)]` modules. The
//! tree's own tests call [`Tree`] directly and are not in them.
//!
//! | Method | Prod | Bench | Test | Shape | A SQLite backing answers it with |
//! |---|---:|---:|---:|---|---|
//! | [`get`](Baseline::get) | 13 | 2 | 6 | point, exact path | the primary key |
//! | [`contains_key`](Baseline::contains_key) | 10 | 1 | 2 | point | the primary key |
//! | [`cursor`](Baseline::cursor) | 4 | 1 | 0 | point, asked in path order | the primary key per `get`; the directory memo is the tree's, not the contract's |
//! | [`carryable`](Baseline::carryable) | 3 | 0 | 1 | point | the primary key, then [`BaselineEntry::carryable`] on the row |
//! | [`tracked`](Baseline::tracked) | 4 | 0 | 0 | point, or anything strictly under | the primary key, then one `rel_path` range probe (`> p/`, `< p0`, `LIMIT 1`) |
//! | [`occupied`](Baseline::occupied) | 2 | 0 | 0 | folded, at or under | a `folded_path` column (`collision_key(rel_path)`) indexed `(pair_id, folded_path)`: equality, then a range probe (`> f/`, `< f0`) |
//! | [`folded_row_paths`](Baseline::folded_row_paths) | 1 | 0 | 0 | folded, exact | `folded_path` equality |
//! | [`any_folded_row_at_or_under`](Baseline::any_folded_row_at_or_under) | 1 | 0 | 0 | folded, at or under, first match | the `occupied` range streamed, stopping at the first row `held` accepts |
//! | [`subtree_all_synced`](Baseline::subtree_all_synced) | 1 | 0 | 0 | subtree predicate | `NOT EXISTS` over the `rel_path` range with `state <> 'synced'`, on an index leading `(pair_id, state, rel_path)` |
//! | [`subtree`](Baseline::subtree) | 3 | 0 | 0 | subtree, rows | the `rel_path` range, streamed (keyset-paged, so the iterator owns no statement) |
//! | [`visit_subtree_paths`](Baseline::visit_subtree_paths) | 4 | 0 | 0 | subtree, paths | the `rel_path` range, streamed |
//! | [`iter`](Baseline::iter) | 1 | 1 | 4 | whole, rows | every row of the pair, keyset-paged |
//! | [`visit_rows`](Baseline::visit_rows) | 2 | 0 | 2 | whole, rows | every row of the pair, streamed |
//! | [`visit_row_paths`](Baseline::visit_row_paths) | 1 | 2 | 0 | whole, paths | every path of the pair, streamed |
//! | [`len`](Baseline::len) / [`is_empty`](Baseline::is_empty) | 9 | 11 | 10 | count | a RESIDENT counter the store's writes keep (`COUNT(*)` is a scan) |
//! | [`carryable_rows`](Baseline::carryable_rows) | 3 | 0 | 1 | count | that counter minus the uncarryable set's size |
//! | [`has_remote_rows`](Baseline::has_remote_rows) | 2 | 0 | 0 | existence | a seek of `(pair_id, remote_uuid)` past the NULLs, or a resident counter |
//! | [`any_unconfirmed`](Baseline::any_unconfirmed) | 3 | 0 | 0 | small set, existence | a partial index on the unconfirmed-push predicate, `LIMIT 1` |
//! | [`unconfirmed`](Baseline::unconfirmed) | 4 | 1 | 0 | small set, rows | the same partial index |
//! | [`uncarryable_paths`](Baseline::uncarryable_paths) | 1 | 0 | 0 | small set, paths | a partial index on the negated carry rule |
//! | [`rule_file_rows`](Baseline::rule_file_rows) | 3 | 2 | 0 | small set, paths | a partial index on the leaf name being `.filenignore` (a stored leaf or flag column) |
//! | [`set_agreed`](Baseline::set_agreed) | 2 | 0 | 0 | pass-private WRITE | see below |
//! | [`move_subtree`](Baseline::move_subtree) | 1 | 0 | 2 | pass-private WRITE | see below |
//!
//! Test and bench only: `from_rows` (24 test), `synced_paths` (4 test), `tree_mut` (1 bench, 1
//! test), `paths` (1 bench), `resident_bytes` (5 bench), `resident_terms` (1 bench).
//!
//! The uuid and lineage lookups a pass makes are NOT on this type, because they already are store
//! seeks:
//! [`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths) resolves them off
//! `(pair_id, remote_uuid)` / `(pair_id, remote_stable_uuid)` before the pass reads either side,
//! and the pass reads the [`SyncedPaths`](super::baseline::SyncedPaths) it got back. The
//! `synced_paths` on this type is the test stand-in for that seek and walks every row.
//!
//! # Nothing here needs a resident tree
//!
//! Every read above has an indexed answer once the table carries a folded-path column, a
//! leaf-name (or rule-file) column and the partial indexes named — all free: the branch's schema
//! is unpushed, so the swap adds them, writes no migration and bumps `SCHEMA_VERSION`. The
//! folded column must be [`collision_key`](super::scan::collision_key) computed in Rust at write
//! time, never SQLite's `lower()`, which folds ASCII only: the Å/å, final-sigma and dotted-İ cases
//! `tree.rs` tests are exactly where the two differ. A string prefix that is not a path prefix
//! (`docsx` under `docs`) is excluded by the range bounds being `f/` and `f0` rather than `f`.
//!
//! What a SQLite backing does NOT reproduce is ORDER. The tree walks parent before child, siblings
//! in folded-name order, each subtree contiguous; a `rel_path` index yields bytewise order, which
//! still puts a parent before its children but NOT a subtree contiguously (`a b` sorts between `a`
//! and `a/b`). The contract of every enumeration here is "parent before child, nothing else", and a
//! consumer that needs more must sort. The whole-tree ones whose output order could reach a plan
//! are `visit_rows` in `plan::visit_move_sources` (a whole pass's move detection) and in
//! `SideRef::entries` (a carried side enumerated whole), and `iter` in
//! `plan::adopt_destination_rows` (the rows a mode switch adopts); the swap has to show those are
//! order-insensitive or sort them.
//!
//! # The two writes, and the fold window
//!
//! [`move_subtree`](Baseline::move_subtree) (`plan::fold_dir_moves`) and
//! [`set_agreed`](Baseline::set_agreed) (the three confirmation steps) edit the PASS's view of the
//! rows, never the store's: a dry run persists neither, and a real pass persists the confirmations
//! itself and commits the move when it applies it. Today each takes `Arc::make_mut` on the shared
//! tree while the store still holds its own `Arc`, so the first one of a pass COPIES THE WHOLE
//! TREE. The overlay step replaces that copy with a per-pass map of edits over the backing's rows —
//! the pattern [`Side`](super::side::Side)'s overlay already uses.
//!
//! That overlay is not an optimisation, it is load-bearing. Between the first `move_subtree` of a
//! pass and the move's commit, the pass's view and the database DISAGREE on purpose: the moved
//! subtree is keyed under its destination here and under its source in the table. `occupied(to)`
//! in `plan::next_dir_move` and `occupied(parent)` in `plan::parents_ready` are asked INSIDE that
//! window, and so is every read after it — the reconcile, the apply layer's `get`s, a carried
//! side's rows. Answered from the table, they would consult rows the fold has already re-keyed
//! away, and approve a directory move onto an occupied destination. Every method above must
//! answer against the edited view: the overlay first, the backing only for paths it does not
//! cover, and — for the at-or-under questions — the backing's rows MINUS the subtrees the overlay
//! moved out, PLUS the ones it moved in.
//!
//! # Read consistency
//!
//! A pass reads one immutable snapshot: the `Arc` it was handed. The store's writes during that
//! pass land in the store's own copy (`BaselineStore::note_written` makes its own `Arc` unique
//! first), so the apply layer's `get`s keep reading the rows the plan was made from while the pass
//! writes new ones. A SQLite backing loses that for free — the apply layer's reads would see the
//! pass's own commits. The pass gate and the reading lock serialize a pair's passes and block
//! `resolve_conflict`, which settles everyone ELSE, but not the pass against itself; the swap has
//! to either hold a read transaction (a second connection in WAL mode) for the life of the pass,
//! or route the pass's own writes into the same overlay the fold uses so its reads never touch
//! a row it has written.
//!
//! One more constraint on the swap: a pass shares its baseline with the local scan's blocking
//! thread (`engine::prepare_scoped` / `prepare_whole` clone it into `spawn_blocking`), so whatever
//! backs it has to be readable from a second thread — a `Connection` is `Send` but not `Sync`.

use std::sync::Arc;

use filen_types::crypto::Blake3Hash;
#[cfg(test)]
use filen_types::fs::StableUuid;
#[cfg(test)]
use uuid::Uuid;

#[cfg(feature = "bench-internals")]
use super::tree::ResidentTerms;
use super::{
	baseline::BaselineEntry,
	tree::{Cursor, Tree},
};

/// One pair's baseline rows as a pass reads them (see the module doc).
///
/// Cloning shares the rows: it is what a pass does to hand its snapshot to a blocking thread.
#[derive(Debug, Clone, Default)]
pub(super) struct Baseline(Arc<Tree>);

impl Baseline {
	/// The store's resident tree, as the pass it is handed to reads it.
	pub(super) fn resident(tree: Arc<Tree>) -> Self {
		Self(tree)
	}

	/// The rows as the store would read them back, in any order: the tests' way of writing a
	/// baseline down.
	#[cfg(test)]
	pub(super) fn from_rows(rows: impl IntoIterator<Item = BaselineEntry>) -> Self {
		Self(Arc::new(Tree::from_rows(rows)))
	}

	/// How many rows the pair tracks.
	pub(super) fn len(&self) -> usize {
		self.0.len()
	}

	pub(super) fn is_empty(&self) -> bool {
		self.0.is_empty()
	}

	/// The row at exactly `rel_path`.
	pub(super) fn get(&self, rel_path: &str) -> Option<BaselineEntry> {
		self.0.get(rel_path)
	}

	pub(super) fn contains_key(&self, rel_path: &str) -> bool {
		self.0.contains_key(rel_path)
	}

	/// A lookup for a caller that asks about paths in path order (see [`Cursor`]). Answers exactly
	/// what [`get`](Self::get) answers.
	pub(super) fn cursor(&self) -> Cursor<'_> {
		self.0.cursor()
	}

	/// Every row, parent before child — and in no other order a caller may rely on (see the
	/// module doc).
	pub(super) fn iter(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.0.iter()
	}

	/// Every row, parent before child, handed to `visit` as one buffer refilled per row.
	pub(super) fn visit_rows(&self, visit: impl FnMut(&BaselineEntry)) {
		self.0.visit_rows(visit);
	}

	/// Every row's path, parent before child.
	#[cfg(feature = "bench-internals")]
	pub(super) fn paths(&self) -> impl Iterator<Item = String> + '_ {
		self.0.paths()
	}

	/// Every row's path, parent before child, as a slice of one reused buffer.
	pub(super) fn visit_row_paths(&self, visit: impl FnMut(&str)) {
		self.0.visit_row_paths(visit);
	}

	/// Every row STRICTLY under `root` (`""` is every row), parent before child.
	pub(super) fn subtree(&self, root: &str) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.0.subtree(root)
	}

	/// Every path STRICTLY under `root` (`""` is every path), as a slice of one reused buffer.
	pub(super) fn visit_subtree_paths(&self, root: &str, visit: impl FnMut(&str)) {
		self.0.visit_subtree_paths(root, visit);
	}

	/// The rows awaiting confirmation of a push of ours. Empty on a converged pair.
	pub(super) fn unconfirmed(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.0.unconfirmed()
	}

	pub(super) fn any_unconfirmed(&self) -> bool {
		self.0.any_unconfirmed()
	}

	/// Whether any row records a remote item.
	pub(super) fn has_remote_rows(&self) -> bool {
		self.0.has_remote_rows()
	}

	/// Where the rows record each of `uuids` and `lineages`, found by walking every row: the test
	/// stand-in for [`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths).
	#[cfg(test)]
	pub(super) fn synced_paths(
		&self,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> super::baseline::SyncedPaths {
		self.0.synced_paths(uuids, lineages)
	}

	/// Whether a row sits at `rel_path` under ANY spelling, or anywhere under it.
	pub(super) fn occupied(&self, rel_path: &str) -> bool {
		self.0.occupied(rel_path)
	}

	/// The path of every row whose path folds to `rel_path`.
	pub(super) fn folded_row_paths(&self, rel_path: &str) -> Vec<String> {
		self.0.folded_row_paths(rel_path)
	}

	/// Whether the rows still track anything at `rel_path`, or — for a directory — under it.
	pub(super) fn tracked(&self, rel_path: &str, is_dir: bool) -> bool {
		self.0.tracked(rel_path, is_dir)
	}

	/// How many rows stand in for both sides.
	pub(super) fn carryable_rows(&self) -> usize {
		self.0.carryable_rows()
	}

	/// Whether the row at `rel_path` stands in for both sides.
	pub(super) fn carryable(&self, rel_path: &str) -> bool {
		self.0.carryable(rel_path)
	}

	/// The path of every `.filenignore` row, in no order.
	pub(super) fn rule_file_rows(&self) -> impl Iterator<Item = String> + '_ {
		self.0.rule_file_rows()
	}

	/// The path of every row that stands in for neither side, in no order.
	pub(super) fn uncarryable_paths(&self) -> impl Iterator<Item = String> + '_ {
		self.0.uncarryable_paths()
	}

	/// Whether any row at or under `rel_path`, under any spelling, satisfies `held`. ROWS ONLY: a
	/// caller that also holds paths no row tracks scans those itself.
	pub(super) fn any_folded_row_at_or_under(
		&self,
		rel_path: &str,
		held: &mut impl FnMut(&str) -> bool,
	) -> bool {
		self.0.any_folded_row_at_or_under(rel_path, held)
	}

	/// Whether every row STRICTLY under `rel_path` is `Synced`.
	pub(super) fn subtree_all_synced(&self, rel_path: &str) -> bool {
		self.0.subtree_all_synced(rel_path)
	}

	/// Re-key the row at `from` and everything under it to sit under `to`, in THIS view only.
	///
	/// Copies the whole tree the first time a pass writes, while the store still shares it — the
	/// cost the overlay step removes (see the module doc).
	pub(super) fn move_subtree(&mut self, from: &str, to: &str) {
		Arc::make_mut(&mut self.0).move_subtree(from, to);
	}

	/// Advance the agreed-content marker of the row at `rel_path` in THIS view, and hand back the
	/// row as it now stands. `None` when nothing is there. Copies like
	/// [`move_subtree`](Self::move_subtree).
	pub(super) fn set_agreed(
		&mut self,
		rel_path: &str,
		agreed_hash: Option<Blake3Hash>,
	) -> Option<BaselineEntry> {
		Arc::make_mut(&mut self.0).set_agreed(rel_path, agreed_hash)
	}

	/// The resident tree behind this view, for a test or the probe that edits one in place.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn tree_mut(&mut self) -> &mut Tree {
		Arc::make_mut(&mut self.0)
	}

	/// What the backing holds for the life of the pair, in bytes (see [`Tree::resident_bytes`]).
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_bytes(&self) -> usize {
		self.0.resident_bytes()
	}

	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_terms(&self) -> ResidentTerms {
		self.0.resident_terms()
	}
}
