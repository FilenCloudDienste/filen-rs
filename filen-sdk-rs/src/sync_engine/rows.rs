//! The baseline rows a pass reads, behind the one type every consumer reads them through.
//!
//! [`Baseline`] is a BOUNDARY, not a structure: today it is backed by the store's resident
//! [`Tree`], shared as an `Arc`, plus the pass's own [`Edits`] once it makes any; with none, every
//! method forwards to the tree. It exists so that the backing can change — rows read from SQLite
//! next — without touching a consumer. No consumer names [`Tree`], a `NodeId` or an `Arc` of
//! either; the store hands out `Baseline`s and keeps the trees.
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
//! | [`cursor`](Baseline::cursor) | 4 | 1 | 0 | point, asked in path order | the primary key per `get`; the directory memo is the tree's, not the contract's ([`Cursor`] is an enum over the backings) |
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
//! | [`set_agreed`](Baseline::set_agreed) | 2 | 0 | 0 | pass-private WRITE | an [`Edits`] entry, never a store write (see below) |
//! | [`move_subtree`](Baseline::move_subtree) | 1 | 0 | 2 | pass-private WRITE | an [`Edits`] entry, never a store write (see below) |
//!
//! Test and bench only: `from_rows` (24 test), `synced_paths` (4 test), `tree_mut` (1 test),
//! `paths` (1 bench), `resident_bytes` (5 bench), `resident_terms` (1 bench).
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
//! itself and commits the move when it applies it. Each used to take `Arc::make_mut` on the shared
//! tree while the store still held its own `Arc`, so the first one of a pass COPIED THE WHOLE
//! TREE. Each is now an entry in the pass's [`Edits`] — the rows it wrote, and the shared nodes it
//! moved away — the size of what the pass changed, beside a tree it never writes to (see
//! `tree/edits.rs`). The same pattern [`Side`](super::side::Side)'s overlay uses.
//!
//! That overlay is not an optimisation, it is load-bearing. Between the first `move_subtree` of a
//! pass and the move's commit, the pass's view and the database DISAGREE on purpose: the moved
//! subtree is keyed under its destination here and under its source in the table. `occupied(to)`
//! in `plan::next_dir_move` and `occupied(parent)` in `plan::parents_ready` are asked INSIDE that
//! window, and so is every read after it — the reconcile, the apply layer's `get`s, a carried
//! side's rows. Answered from the table, they would consult rows the fold has already re-keyed
//! away, and approve a directory move onto an occupied destination. Every method above answers
//! against the edited view: the written rows first, the shared tree only for paths they do not
//! cover, and — for the at-or-under questions and every enumeration — the shared rows MINUS the
//! subtrees the pass moved out, PLUS the ones it moved in, in the order a copy of the tree with
//! the same edits would give. The tests below hold every method to exactly that copy's answers.
//! A SQLite backing has to keep the same shape: its edits over the table, never into it.
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
use itertools::Either;
#[cfg(test)]
use uuid::Uuid;

#[cfg(feature = "bench-internals")]
use super::tree::ResidentTerms;
use super::{
	baseline::BaselineEntry,
	tree::{self, Edits, Tree, View},
};

/// One pair's baseline rows as a pass reads them (see the module doc).
///
/// Cloning shares the rows, and the pass's edits with them: it is what a pass does to hand its view
/// to a blocking thread.
#[derive(Debug, Clone, Default)]
pub(super) struct Baseline {
	/// The store's resident tree. Never written through this handle.
	rows: Arc<Tree>,
	/// What this pass changed about `rows`; `None` until it changes anything, which is almost every
	/// pass, and every read then goes straight to the tree.
	edits: Option<Arc<Edits>>,
}

/// A lookup for a caller that asks about paths in path order (see [`tree::Cursor`]). Answers exactly
/// what [`Baseline::get`] answers.
pub(super) enum Cursor<'a> {
	Rows(tree::Cursor<'a>),
	Edited(View<'a>),
}

impl Cursor<'_> {
	pub(super) fn get(&mut self, rel_path: &str) -> Option<BaselineEntry> {
		match self {
			Self::Rows(cursor) => cursor.get(rel_path),
			Self::Edited(view) => view.get(rel_path),
		}
	}
}

impl Baseline {
	/// The store's resident tree, as the pass it is handed to reads it.
	pub(super) fn resident(tree: Arc<Tree>) -> Self {
		Self {
			rows: tree,
			edits: None,
		}
	}

	/// The rows as the store would read them back, in any order: the tests' way of writing a
	/// baseline down.
	#[cfg(test)]
	pub(super) fn from_rows(rows: impl IntoIterator<Item = BaselineEntry>) -> Self {
		Self::resident(Arc::new(Tree::from_rows(rows)))
	}

	/// The rows as this pass has edited them, when it has.
	fn edited(&self) -> Option<View<'_>> {
		self.edits.as_deref().map(|edits| edits.view(&self.rows))
	}

	/// The pass's edits, begun on the first write.
	fn edits_mut(&mut self) -> (&Tree, &mut Edits) {
		let edits = self
			.edits
			.get_or_insert_with(|| Arc::new(Edits::new(&self.rows)));
		(&self.rows, Arc::make_mut(edits))
	}

	/// How many rows the pair tracks.
	pub(super) fn len(&self) -> usize {
		self.edited()
			.map_or_else(|| self.rows.len(), |view| view.len())
	}

	pub(super) fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// The row at exactly `rel_path`.
	pub(super) fn get(&self, rel_path: &str) -> Option<BaselineEntry> {
		match self.edited() {
			Some(view) => view.get(rel_path),
			None => self.rows.get(rel_path),
		}
	}

	pub(super) fn contains_key(&self, rel_path: &str) -> bool {
		match self.edited() {
			Some(view) => view.contains_key(rel_path),
			None => self.rows.contains_key(rel_path),
		}
	}

	/// A lookup for a caller that asks about paths in path order (see [`Cursor`]). Answers exactly
	/// what [`get`](Self::get) answers.
	pub(super) fn cursor(&self) -> Cursor<'_> {
		match self.edited() {
			Some(view) => Cursor::Edited(view),
			None => Cursor::Rows(self.rows.cursor()),
		}
	}

	/// Every row, parent before child — and in no other order a caller may rely on (see the
	/// module doc).
	pub(super) fn iter(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.iter()),
			None => Either::Right(self.rows.iter()),
		}
	}

	/// Every row, parent before child, handed to `visit` as one buffer refilled per row.
	pub(super) fn visit_rows(&self, visit: impl FnMut(&BaselineEntry)) {
		match self.edited() {
			Some(view) => view.visit_rows(visit),
			None => self.rows.visit_rows(visit),
		}
	}

	/// Every row's path, parent before child.
	#[cfg(feature = "bench-internals")]
	pub(super) fn paths(&self) -> impl Iterator<Item = String> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.paths()),
			None => Either::Right(self.rows.paths()),
		}
	}

	/// Every row's path, parent before child, as a slice of one reused buffer.
	pub(super) fn visit_row_paths(&self, visit: impl FnMut(&str)) {
		match self.edited() {
			Some(view) => view.visit_row_paths(visit),
			None => self.rows.visit_row_paths(visit),
		}
	}

	/// Every row STRICTLY under `root` (`""` is every row), parent before child.
	pub(super) fn subtree(&self, root: &str) -> impl Iterator<Item = BaselineEntry> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.subtree(root)),
			None => Either::Right(self.rows.subtree(root)),
		}
	}

	/// Every path STRICTLY under `root` (`""` is every path), as a slice of one reused buffer.
	pub(super) fn visit_subtree_paths(&self, root: &str, visit: impl FnMut(&str)) {
		match self.edited() {
			Some(view) => view.visit_subtree_paths(root, visit),
			None => self.rows.visit_subtree_paths(root, visit),
		}
	}

	/// The rows awaiting confirmation of a push of ours. Empty on a converged pair.
	pub(super) fn unconfirmed(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.unconfirmed()),
			None => Either::Right(self.rows.unconfirmed()),
		}
	}

	pub(super) fn any_unconfirmed(&self) -> bool {
		match self.edited() {
			Some(view) => view.any_unconfirmed(),
			None => self.rows.any_unconfirmed(),
		}
	}

	/// Whether any row records a remote item.
	pub(super) fn has_remote_rows(&self) -> bool {
		match self.edited() {
			Some(view) => view.has_remote_rows(),
			None => self.rows.has_remote_rows(),
		}
	}

	/// Where the rows record each of `uuids` and `lineages`, found by walking every row: the test
	/// stand-in for [`BaselineStore::synced_paths`](super::baseline::BaselineStore::synced_paths).
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

	/// Whether a row sits at `rel_path` under ANY spelling, or anywhere under it.
	pub(super) fn occupied(&self, rel_path: &str) -> bool {
		match self.edited() {
			Some(view) => view.occupied(rel_path),
			None => self.rows.occupied(rel_path),
		}
	}

	/// The path of every row whose path folds to `rel_path`.
	pub(super) fn folded_row_paths(&self, rel_path: &str) -> Vec<String> {
		match self.edited() {
			Some(view) => view.folded_row_paths(rel_path),
			None => self.rows.folded_row_paths(rel_path),
		}
	}

	/// Whether the rows still track anything at `rel_path`, or — for a directory — under it.
	pub(super) fn tracked(&self, rel_path: &str, is_dir: bool) -> bool {
		match self.edited() {
			Some(view) => view.tracked(rel_path, is_dir),
			None => self.rows.tracked(rel_path, is_dir),
		}
	}

	/// How many rows stand in for both sides.
	pub(super) fn carryable_rows(&self) -> usize {
		match self.edited() {
			Some(view) => view.carryable_rows(),
			None => self.rows.carryable_rows(),
		}
	}

	/// Whether the row at `rel_path` stands in for both sides.
	pub(super) fn carryable(&self, rel_path: &str) -> bool {
		match self.edited() {
			Some(view) => view.carryable(rel_path),
			None => self.rows.carryable(rel_path),
		}
	}

	/// The path of every `.filenignore` row, in no order.
	pub(super) fn rule_file_rows(&self) -> impl Iterator<Item = String> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.rule_file_rows()),
			None => Either::Right(self.rows.rule_file_rows()),
		}
	}

	/// The path of every row that stands in for neither side, in no order.
	pub(super) fn uncarryable_paths(&self) -> impl Iterator<Item = String> + '_ {
		match self.edited() {
			Some(view) => Either::Left(view.uncarryable_paths()),
			None => Either::Right(self.rows.uncarryable_paths()),
		}
	}

	/// Whether any row at or under `rel_path`, under any spelling, satisfies `held`. ROWS ONLY: a
	/// caller that also holds paths no row tracks scans those itself.
	pub(super) fn any_folded_row_at_or_under(
		&self,
		rel_path: &str,
		held: &mut impl FnMut(&str) -> bool,
	) -> bool {
		match self.edited() {
			Some(view) => view.any_folded_row_at_or_under(rel_path, held),
			None => self.rows.any_folded_row_at_or_under(rel_path, held),
		}
	}

	/// Whether every row STRICTLY under `rel_path` is `Synced`.
	pub(super) fn subtree_all_synced(&self, rel_path: &str) -> bool {
		match self.edited() {
			Some(view) => view.subtree_all_synced(rel_path),
			None => self.rows.subtree_all_synced(rel_path),
		}
	}

	/// Re-key the row at `from` and everything under it to sit under `to`, in THIS view only: an
	/// edit recorded beside the shared rows, the size of what moved (see the module doc).
	pub(super) fn move_subtree(&mut self, from: &str, to: &str) {
		let (rows, edits) = self.edits_mut();
		edits.move_subtree(rows, from, to);
	}

	/// Advance the agreed-content marker of the row at `rel_path` in THIS view, and hand back the
	/// row as it now stands. `None` when nothing is there. An edit, like
	/// [`move_subtree`](Self::move_subtree).
	pub(super) fn set_agreed(
		&mut self,
		rel_path: &str,
		agreed_hash: Option<Blake3Hash>,
	) -> Option<BaselineEntry> {
		if self.edits.is_none() && !self.contains_key(rel_path) {
			// Nothing to advance, so nothing to begin edits for.
			return None;
		}
		let (rows, edits) = self.edits_mut();
		edits.set_agreed(rows, rel_path, agreed_hash)
	}

	/// The resident tree behind this view, for a test that edits one in place. Only before the pass
	/// has edited it: an edit already made lives beside the tree, and writing under it would change
	/// what that edit sits over.
	#[cfg(test)]
	pub(super) fn tree_mut(&mut self) -> &mut Tree {
		assert!(
			self.edits.is_none(),
			"the tree behind a baseline this pass has already edited"
		);
		Arc::make_mut(&mut self.rows)
	}

	/// What the store holds for the life of the pair, in bytes (see [`Tree::resident_bytes`]). A
	/// pass's own edits are not in it: they live and die with the pass.
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_bytes(&self) -> usize {
		self.rows.resident_bytes()
	}

	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_terms(&self) -> ResidentTerms {
		self.rows.resident_terms()
	}
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use rand::{Rng, SeedableRng, rngs::StdRng};
	use uuid::Uuid;

	use super::{
		super::baseline::{BaselineEntry, BaselineState, NodeKind},
		*,
	};

	fn file(rel_path: &str, hash: u8) -> BaselineEntry {
		let uuid = Uuid::new_v4();
		BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(uuid),
			content_hash: Some(Blake3Hash::from([hash; 32])),
			size: Some(7),
			local_mtime: Some(12),
			remote_modified: Some(34),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			agreed_hash: Some(Blake3Hash::from([hash; 32])),
		}
	}

	fn dir(rel_path: &str) -> BaselineEntry {
		BaselineEntry {
			kind: NodeKind::Dir,
			content_hash: None,
			size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
			..file(rel_path, 0)
		}
	}

	/// Every shape the view has to carry across an edit: a destination a move lands on in part, a
	/// sibling that is a string prefix and not a path prefix, rows under a path no row names, two
	/// siblings that fold together, the foldings `collision_key` and ASCII lowering disagree on
	/// (Ä, Greek sigma, Cyrillic, and the dotted İ whose lowering is LONGER than it), and a row of
	/// every index — unconfirmed, conflicted, uncarryable, rule file.
	fn corpus() -> Vec<BaselineEntry> {
		vec![
			dir("docs"),
			file("docs/a.txt", 1),
			dir("docs/sub"),
			file("docs/sub/deep.txt", 2),
			file("docs/.filenignore", 3),
			file("docsx", 4),
			dir("archive"),
			dir("archive/docs"),
			file("archive/docs/a.txt", 5),
			file("archive/docs/keep.txt", 6),
			file("orphan/deep/leaf.txt", 7),
			dir("Ärger"),
			file("Ärger/ä.txt", 8),
			dir("ärger"),
			file("ärger/b.txt", 9),
			dir("ΣΑΣ"),
			file("ΣΑΣ/σ.txt", 10),
			dir("σας"),
			dir("Документы"),
			file("Документы/файл.txt", 11),
			dir("İ"),
			file("İ/x.txt", 12),
			file("i̇x", 13),
			BaselineEntry {
				agreed_hash: None,
				..file("docs/pushed.txt", 14)
			},
			BaselineEntry {
				agreed_hash: None,
				..file("pushed-top.txt", 15)
			},
			BaselineEntry {
				state: BaselineState::Conflicted,
				remote_hash: Some(Blake3Hash::from([16; 32])),
				..file("docs/sub/held.txt", 16)
			},
			BaselineEntry {
				remote_uuid: None,
				..file("archive/local-only.txt", 17)
			},
		]
	}

	/// The paths every question is asked at: each path either side has ever held, each ancestor of
	/// one, a folded spelling of each, and each with a character appended — the string prefix
	/// that is not a path prefix.
	fn probes(paths: impl IntoIterator<Item = String>) -> Vec<String> {
		let mut out = BTreeSet::from([String::new()]);
		for path in paths {
			let mut at = path.as_str();
			loop {
				out.insert(at.to_string());
				out.insert(at.to_uppercase());
				out.insert(at.to_lowercase());
				out.insert(format!("{at}x"));
				match at.rsplit_once('/') {
					Some((parent, _)) => at = parent,
					None => break,
				}
			}
		}
		out.into_iter().collect()
	}

	fn sorted(mut paths: Vec<String>) -> Vec<String> {
		paths.sort();
		paths
	}

	/// Every question the boundary answers, asked of `view` and of `oracle` — a COPY of the tree
	/// with the same edits written into it, which is what a pass read before the edits moved beside
	/// the tree — and required to be the same answer, in the same order wherever the tree has one.
	fn assert_alike(context: &str, view: &Baseline, oracle: &Tree, probes: &[String]) {
		assert_eq!(view.len(), oracle.len(), "{context}: len");
		assert_eq!(
			view.carryable_rows(),
			oracle.carryable_rows(),
			"{context}: carryable_rows"
		);
		assert_eq!(
			view.has_remote_rows(),
			oracle.has_remote_rows(),
			"{context}: has_remote_rows"
		);
		assert_eq!(
			view.any_unconfirmed(),
			oracle.any_unconfirmed(),
			"{context}: any_unconfirmed"
		);
		assert_eq!(
			view.iter().collect::<Vec<_>>(),
			oracle.iter().collect::<Vec<_>>(),
			"{context}: iter"
		);
		let mut rows = Vec::new();
		view.visit_rows(|row| rows.push(row.clone()));
		assert_eq!(
			rows,
			oracle.iter().collect::<Vec<_>>(),
			"{context}: visit_rows"
		);
		let mut paths = Vec::new();
		view.visit_row_paths(|path| paths.push(path.to_string()));
		assert_eq!(
			paths,
			oracle.paths().collect::<Vec<_>>(),
			"{context}: visit_row_paths"
		);
		let by_path = |rows: Vec<BaselineEntry>| {
			let mut rows = rows;
			rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
			rows
		};
		assert_eq!(
			by_path(view.unconfirmed().collect()),
			by_path(oracle.unconfirmed().collect()),
			"{context}: unconfirmed"
		);
		assert_eq!(
			sorted(view.uncarryable_paths().collect()),
			sorted(oracle.uncarryable_paths().collect()),
			"{context}: uncarryable_paths"
		);
		assert_eq!(
			sorted(view.rule_file_rows().collect()),
			sorted(oracle.rule_file_rows().collect()),
			"{context}: rule_file_rows"
		);
		let (mut cursor, mut oracle_cursor) = (view.cursor(), oracle.cursor());
		for path in probes {
			let at = format!("{context}: at {path:?}");
			assert_eq!(view.get(path), oracle.get(path), "{at}: get");
			if !path.is_empty() {
				assert_eq!(cursor.get(path), oracle_cursor.get(path), "{at}: cursor");
			}
			assert_eq!(
				view.contains_key(path),
				oracle.contains_key(path),
				"{at}: contains_key"
			);
			assert_eq!(
				view.carryable(path),
				oracle.carryable(path),
				"{at}: carryable"
			);
			for is_dir in [false, true] {
				assert_eq!(
					view.tracked(path, is_dir),
					oracle.tracked(path, is_dir),
					"{at}: tracked({is_dir})"
				);
			}
			assert_eq!(view.occupied(path), oracle.occupied(path), "{at}: occupied");
			assert_eq!(
				view.folded_row_paths(path),
				oracle.folded_row_paths(path),
				"{at}: folded_row_paths"
			);
			// Every row the folded walk offers, in the order it offers them.
			let (mut offered, mut oracle_offered) = (Vec::new(), Vec::new());
			let view_any = view.any_folded_row_at_or_under(path, &mut |row: &str| {
				offered.push(row.to_string());
				false
			});
			let oracle_any = oracle.any_folded_row_at_or_under(path, &mut |row: &str| {
				oracle_offered.push(row.to_string());
				false
			});
			assert_eq!(
				(view_any, offered),
				(oracle_any, oracle_offered),
				"{at}: any_folded_row_at_or_under"
			);
			assert_eq!(
				view.subtree_all_synced(path),
				oracle.subtree_all_synced(path),
				"{at}: subtree_all_synced"
			);
			assert_eq!(
				view.subtree(path).collect::<Vec<_>>(),
				oracle.subtree(path).collect::<Vec<_>>(),
				"{at}: subtree"
			);
			let (mut under, mut oracle_under) = (Vec::new(), Vec::new());
			view.visit_subtree_paths(path, |p| under.push(p.to_string()));
			oracle.visit_subtree_paths(path, |p| oracle_under.push(p.to_string()));
			assert_eq!(under, oracle_under, "{at}: visit_subtree_paths");
		}
	}

	/// The fold window. A folded directory move re-keys a subtree the store still holds under its
	/// source, and `plan::next_dir_move` then asks `occupied(to)` and `plan::parents_ready` asks
	/// `occupied(parent)` of the pass's view — which must answer with the source vacated and the
	/// destination occupied, exactly as a copy of the tree with the move written into it did.
	///
	/// The answers are not written down here: the copy is the oracle, so a question the two answer
	/// differently fails whatever either answer is.
	#[test]
	fn a_folded_move_answers_the_fold_windows_questions_as_a_moved_copy_does() {
		let rows = corpus();
		let mut view = Baseline::from_rows(rows.clone());
		let mut oracle = Tree::from_rows(rows);
		view.move_subtree("docs", "archive/docs");
		oracle.move_subtree("docs", "archive/docs");
		assert!(view.edits.is_some(), "the move is an edit beside the tree");

		let asked = [
			// The source, a folded spelling of it, and a string prefix of it that is no path prefix.
			"docs",
			"DOCS",
			"Docs",
			"docsx",
			"doc",
			// The destination, its folded spellings, its parent, and the same prefix shapes.
			"archive/docs",
			"ARCHIVE/DOCS",
			"Archive/Docs",
			"archive/docsx",
			"archive/doc",
			"archive",
			"ARCHIVE",
			// A row the move landed on, one it did not, and one it brought.
			"archive/docs/a.txt",
			"archive/docs/keep.txt",
			"archive/docs/sub/deep.txt",
			"ARCHIVE/DOCS/SUB",
		];
		let mut answers = BTreeSet::new();
		for path in asked {
			let answer = view.occupied(path);
			assert_eq!(answer, oracle.occupied(path), "occupied({path:?})");
			answers.insert(answer);
		}
		assert_eq!(
			answers.len(),
			2,
			"the questions asked have to include both answers, or they prove nothing"
		);
		assert_alike(
			"docs -> archive/docs",
			&view,
			&oracle,
			&probes(asked.iter().map(|path| path.to_string())),
		);
	}

	/// Every edit sequence a pass can write, and many it cannot, answered alike by the view and by
	/// a copy with the same edits written into it — after every single edit. The sequences chain
	/// moves through each other's sources and destinations, move a subtree back onto where it
	/// came from, rename by case alone, move a directory into its own subtree, and advance the
	/// markers of rows that have and have not moved.
	#[test]
	fn every_edit_sequence_answers_as_a_copy_with_the_same_edits_does() {
		let rows = corpus();
		let everything = probes(
			rows.iter()
				.map(|row| row.rel_path.clone())
				.chain(["new", "new/inner", "archive/new", "DOCS"].map(String::from)),
		);
		let (mut moves, mut confirms) = (0, 0);
		for seed in 0..60 {
			let mut rng = StdRng::seed_from_u64(seed);
			let mut view = Baseline::from_rows(rows.clone());
			let mut oracle = Tree::from_rows(rows.clone());
			let mut probes = everything.clone();
			for step in 0..8 {
				// A source among the paths the copy holds now, or any probe at all — which reaches
				// paths no row names, folded spellings and nothing at all.
				let held = oracle.paths().collect::<Vec<_>>();
				let pick = |rng: &mut StdRng, from: &[String]| {
					from[rng.random_range(0..from.len())].clone()
				};
				let what = if rng.random_range(0..4) == 0 {
					let path = if held.is_empty() || rng.random_range(0..3) == 0 {
						pick(&mut rng, &probes)
					} else {
						pick(&mut rng, &held)
					};
					let hash = match rng.random_range(0..3) {
						0 => None,
						1 => oracle.get(&path).and_then(|row| row.content_hash),
						_ => Some(Blake3Hash::from([rng.random_range(0..4); 32])),
					};
					assert_eq!(
						view.set_agreed(&path, hash),
						oracle.set_agreed(&path, hash),
						"seed {seed} step {step}: set_agreed({path:?}) handed back different rows"
					);
					confirms += 1;
					format!("set_agreed({path:?}, {hash:?})")
				} else {
					let from = if held.is_empty() || rng.random_range(0..4) == 0 {
						pick(&mut rng, &probes)
					} else {
						pick(&mut rng, &held)
					};
					let to = match rng.random_range(0..5) {
						// A destination under a path the pair holds, or a fresh one.
						0 => format!("{}/moved{step}", pick(&mut rng, &probes)),
						1 => format!("moved{step}"),
						// The source by case alone.
						2 => from.to_uppercase(),
						// Anything the pair holds now: a move onto an occupied destination.
						_ if !held.is_empty() => pick(&mut rng, &held),
						_ => pick(&mut rng, &probes),
					};
					let to = to.trim_start_matches('/').to_string();
					view.move_subtree(&from, &to);
					oracle.move_subtree(&from, &to);
					moves += 1;
					format!("move_subtree({from:?} -> {to:?})")
				};
				probes.extend(oracle.paths());
				probes.sort();
				probes.dedup();
				assert_alike(
					&format!("seed {seed} step {step} {what}"),
					&view,
					&oracle,
					&probes,
				);
			}
		}
		assert!(
			moves > 300 && confirms > 60,
			"{moves} move(s), {confirms} confirmation(s)"
		);
	}

	/// A pass that edits nothing reads the store's tree itself, and one that edits never writes
	/// into it: the store's handle sees the rows it loaded whatever the pass did.
	#[test]
	fn an_edit_never_reaches_the_shared_tree() {
		let shared = Arc::new(Tree::from_rows(corpus()));
		let mut pass = Baseline::resident(Arc::clone(&shared));
		assert!(
			pass.set_agreed("nothing/here", None).is_none() && pass.edits.is_none(),
			"a confirmation that finds no row begins no edits"
		);
		let before: Vec<BaselineEntry> = shared.iter().collect();
		pass.move_subtree("docs", "moved");
		pass.set_agreed("pushed-top.txt", None);
		pass.set_agreed("moved/pushed.txt", Some(Blake3Hash::from([14; 32])));
		assert!(
			Arc::ptr_eq(&pass.rows, &shared),
			"the pass still shares the store's tree"
		);
		assert_eq!(shared.iter().collect::<Vec<_>>(), before);
		assert!(!pass.contains_key("docs/a.txt") && pass.contains_key("moved/a.txt"));
	}
}
