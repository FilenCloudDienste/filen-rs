//! The baseline rows a pass reads, behind the one type every consumer reads them through.
//!
//! [`Baseline`] is a BOUNDARY: every consumer reads the rows through it, and none names what backs
//! it. What backs it is the store's own table — a [`Snapshot`], one read transaction on a reader
//! connection of the pass's own (see `rows/snapshot.rs`) — plus the pass's own [`Edits`] once it
//! makes any. The store keeps nothing of a pair's rows in memory between passes; the resident tree
//! that used to (`tree.rs`, ~127 bytes a row for the life of the pair) is compiled only into the
//! tests, where it is the ORACLE every answer here is held to.
//!
//! Every question is ONE indexed statement or one keyset-paged range, off `baseline`'s columns and
//! indexes (see `baseline::SCHEMA`):
//!
//! | Method | Shape | Answered by |
//! |---|---|---|
//! | [`get`](Baseline::get) | point, exact path | the primary key — or the last page any enumeration read, where it spans the path (see `snapshot::Page`) |
//! | [`contains_key`](Baseline::contains_key), [`carryable`](Baseline::carryable) | point | the primary key, reading the `carryable` column alone |
//! | [`cursor`](Baseline::cursor) | point, asked a directory at a time | a page of the directory's range, then pages from the path asked, sized to how many of the last page's rows were asked for |
//! | [`tracked`](Baseline::tracked) | point, or anything strictly under | the primary key, then one row of the range `(p/, p0)` |
//! | [`occupied`](Baseline::occupied) | folded, at or under | `baseline_folded`: `folded_path` equality, then one row of the range `(f/, f0)` |
//! | [`folded_row_paths`](Baseline::folded_row_paths) | folded, exact | `folded_path` equality |
//! | [`any_folded_row_at_or_under`](Baseline::any_folded_row_at_or_under) | folded, at or under, first match | the two above, paged, stopping at the first row the predicate takes |
//! | [`subtree_all_synced`](Baseline::subtree_all_synced) | subtree predicate | `baseline_state (pair_id, state, rel_path)`: one seek per unsynced state, bounded to the range |
//! | [`subtree`](Baseline::subtree), [`visit_subtree_paths`](Baseline::visit_subtree_paths) | subtree | the primary key's range `(p/, p0)`, paged |
//! | [`iter`](Baseline::iter), [`visit_rows`](Baseline::visit_rows), [`visit_row_paths`](Baseline::visit_row_paths) | whole pair | the primary key, paged |
//! | [`len`](Baseline::len), [`carryable_rows`](Baseline::carryable_rows), [`has_remote_rows`](Baseline::has_remote_rows) | count | `baseline_counts`, read once when the snapshot begins |
//! | [`any_unconfirmed`](Baseline::any_unconfirmed), [`unconfirmed`](Baseline::unconfirmed) | small set | the partial index `baseline_unconfirmed` |
//! | [`uncarryable_paths`](Baseline::uncarryable_paths) | small set | the partial index `baseline_uncarryable` |
//! | [`rule_file_rows`](Baseline::rule_file_rows) | small set | the partial index `baseline_rule_files` |
//! | [`set_agreed`](Baseline::set_agreed), [`move_subtree`](Baseline::move_subtree) | pass-private WRITE | an [`Edits`] entry, never a store write (see below) |
//!
//! The whole-set questions a pass asks before it reads either side — the counts, whether anything
//! is unconfirmed, which rows are uncarryable or rule files — are a counts row and three partial
//! indexes, each empty on a converged pair. No pass walks the table to answer one.
//!
//! # Order
//!
//! Every enumeration here comes in BYTEWISE path order, the primary key's: a parent before every
//! row under it (a path sorts before any extension of itself), and nothing else a caller may rely
//! on. It is not the order the resident tree walked — siblings by folded name, each subtree
//! contiguous — and no consumer depends on that: the baseline was a `HashMap`, in no order at all,
//! before the tree gave it one, and the consumers whose output could reach a plan
//! (`plan::visit_move_sources`, `SideRef::entries`, `plan::adopt_destination_rows`) key what they
//! build by path or uuid. The folded answers ([`folded_row_paths`](Baseline::folded_row_paths),
//! [`any_folded_row_at_or_under`](Baseline::any_folded_row_at_or_under)) are sets.
//!
//! # The folded questions
//!
//! `folded_path` is [`collision_key`] of the row's path, computed in Rust when the row is written
//! and never by SQLite's `lower()` (ASCII only): the Ä/ä, final-sigma and dotted-İ cases are where
//! the two differ. Folding is per character and never yields `/`, so the folded path of `a/b` is the
//! folded `a`, `/`, the folded `b` — the componentwise folding the tree applied — and "at or under
//! `p`, under any spelling" is `folded_path = f OR folded_path` in `(f/, f0)`. The range's bounds
//! are what keep `docsx` out of `docs`: `/` is 0x2F and `0` 0x30, so only a separator continues the
//! prefix.
//!
//! # The two writes, and the fold window
//!
//! [`move_subtree`](Baseline::move_subtree) (`plan::fold_dir_moves`) and
//! [`set_agreed`](Baseline::set_agreed) (the three confirmation steps) edit the PASS's view of the
//! rows, never the store's: a dry run persists neither, and a real pass persists the confirmations
//! itself and commits the move when it applies it. Each is an entry in the pass's [`Edits`] — the
//! rows it wrote, the store paths it moved away, the markers it advanced — the size of what the
//! pass changed.
//!
//! That overlay is load-bearing. Between the first `move_subtree` of a pass and the move's commit,
//! the pass's view and the database DISAGREE on purpose: the moved subtree is keyed under its
//! destination here and under its source in the table. `occupied(to)` in `plan::next_dir_move` and
//! `occupied(parent)` in `plan::parents_ready` are asked INSIDE that window, and so is every read
//! after it — the reconcile, the apply layer's `get`s, a carried side's rows. Answered from the
//! table alone, they would consult rows the fold has already re-keyed away and approve a directory
//! move onto an occupied destination. So every method answers against the edited view: a row the
//! pass wrote first, then the table's row where no moved-away path sits at or above it, carrying
//! the pass's advanced marker where it has one. The tests hold every method to what a COPY of the
//! resident tree with the same edits written into it answers.
//!
//! # Read consistency and threads
//!
//! The snapshot is one read transaction, so a pass reads one state of the table throughout, its
//! own commits included (see `rows/snapshot.rs`). A pass clones its baseline into the local scan's
//! blocking thread; the connection sits behind a mutex, so a clone is readable from any thread and
//! two readers take turns.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	ops::Bound,
	sync::Arc,
};

use filen_types::crypto::Blake3Hash;
#[cfg(test)]
use filen_types::fs::StableUuid;
#[cfg(test)]
use uuid::Uuid;

// The pass's rows as the store holds them. A child module so the store's statements stay beside
// the one type that runs them.
mod snapshot;

pub(super) use self::snapshot::Snapshot;
use self::snapshot::{Counts, PAGE, Page};
use super::{
	baseline::{BaselineEntry, BaselineState},
	scan::collision_key,
};

/// One pair's baseline rows as a pass reads them (see the module doc).
///
/// Cloning shares the snapshot, and the pass's edits with it: it is what a pass does to hand its
/// view to a blocking thread. `Default` is a pair with no rows, which reads nothing.
#[derive(Debug, Clone, Default)]
pub(super) struct Baseline {
	/// The table, as the pass's read transaction sees it. `None`: no rows at all.
	rows: Option<Arc<Snapshot>>,
	/// What this pass changed about `rows`; `None` until it changes anything, which is almost every
	/// pass, and every read then goes straight to the table.
	edits: Option<Arc<Edits>>,
}

/// What a pass changed about the rows (see the module doc). Keyed by PATH throughout, because the
/// table is: a row of the edited view is a written row, or else a table row that no vacated path
/// sits at or above, carrying its advanced marker where it has one.
#[derive(Debug, Clone)]
struct Edits {
	/// Every row this pass wrote, at the path it now sits at. Replaces the table's row there.
	written: BTreeMap<String, BaselineEntry>,
	/// `(collision_key, path)` of every row in [`written`](Self::written): the folded questions'
	/// index over the rows the pass wrote, as `baseline_folded` is over the table's.
	folded: BTreeSet<(String, String)>,
	/// The table paths this pass moved away, each standing for itself and its whole subtree: no
	/// table row at or under one is in the view for the rest of the pass. Exact, not approximate —
	/// a move takes every row at or under its source, and a later move can only bring rows back as
	/// written ones.
	vacated: BTreeSet<String>,
	/// The marker a confirmation advanced on a table row the pass left in place — the one field a
	/// confirmation changes, so a confirmed push costs an entry here rather than a written row.
	agreed: HashMap<String, Agreed>,
	/// The edited view's counts, kept so the whole-set questions stay O(1) whatever the pass moved.
	rows: usize,
	carryable: usize,
	remote_rows: usize,
}

/// A marker the pass advanced on a table row, and whether the row, carrying it, still awaits
/// confirmation — decided when the marker is set, from the row in hand, so that the question "is
/// anything unconfirmed" never has to read the row back.
#[derive(Debug, Clone, Copy)]
struct Agreed {
	hash: Option<Blake3Hash>,
	awaits: bool,
}

impl Edits {
	fn new(counts: Counts) -> Self {
		Self {
			written: BTreeMap::new(),
			folded: BTreeSet::new(),
			vacated: BTreeSet::new(),
			agreed: HashMap::new(),
			rows: counts.rows,
			carryable: carryable_count(counts),
			remote_rows: counts.remote_rows,
		}
	}

	/// The vacated path at or above `rel_path`, if one hides it.
	fn vacated_over<'e>(&'e self, rel_path: &str) -> Option<&'e str> {
		if self.vacated.is_empty() {
			return None;
		}
		rel_path
			.match_indices('/')
			.map(|(at, _)| &rel_path[..at])
			.chain([rel_path])
			.find_map(|at| self.vacated.get(at).map(String::as_str))
	}

	/// Whether the view shows the TABLE's row at `rel_path`: nothing vacated over it, and no row the
	/// pass wrote in its place.
	fn shows_stored(&self, rel_path: &str) -> bool {
		self.vacated_over(rel_path).is_none() && !self.written.contains_key(rel_path)
	}

	/// Put `row` in the view as written by this pass, replacing `old` (what the view showed there).
	fn write(&mut self, row: BaselineEntry, old: Option<&BaselineEntry>) {
		if let Some(old) = old {
			self.uncount(old);
		}
		self.rows += 1;
		self.carryable += usize::from(row.carryable());
		self.remote_rows += usize::from(row.remote_uuid.is_some());
		self.folded
			.insert((collision_key(&row.rel_path), row.rel_path.clone()));
		self.written.insert(row.rel_path.clone(), row);
	}

	/// Take `row` out of the counts. Checked in EVERY build: this crate's release profile leaves
	/// overflow checks off, and a count wrapped past zero would answer `has_remote_rows` or
	/// `carryable_rows` for a pair that has no such row — a silently wrong answer.
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

	/// Drop every written row at or under `rel_path`.
	fn unwrite_subtree(&mut self, rel_path: &str) {
		// `p`, then everything continuing `p` with a byte below `0` — a superset of the paths at or
		// under it, which the filter narrows to the ones that continue it with a separator.
		let end = format!("{rel_path}0");
		let under: Vec<String> = self
			.written
			.range::<str, _>((Bound::Included(rel_path), Bound::Excluded(end.as_str())))
			.map(|(path, _)| path.clone())
			.filter(|path| is_at_or_under(path, rel_path))
			.collect();
		for path in under {
			self.written.remove(&path);
			self.folded.remove(&(collision_key(&path), path));
		}
	}

	/// The written rows STRICTLY under `root` (`""`: every one), in path order.
	fn written_under<'e>(&'e self, root: &str) -> impl Iterator<Item = &'e BaselineEntry> + 'e {
		let range = if root.is_empty() {
			(Bound::Unbounded, Bound::Unbounded)
		} else {
			(
				Bound::Excluded(format!("{root}/")),
				Bound::Excluded(format!("{root}0")),
			)
		};
		self.written.range::<String, _>(range).map(|(_, row)| row)
	}

	/// The written paths whose folded path is `folded`, then those strictly under it.
	fn written_folded<'e>(&'e self, folded: &'e str) -> impl Iterator<Item = &'e str> + 'e {
		let at = self
			.folded
			.range((folded.to_string(), String::new())..)
			.take_while(move |(key, _)| key == folded);
		let under = self
			.folded
			.range((format!("{folded}/"), String::new())..(format!("{folded}0"), String::new()));
		at.chain(under).map(|(_, path)| path.as_str())
	}
}

fn is_at_or_under(path: &str, rel_path: &str) -> bool {
	path.strip_prefix(rel_path)
		.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// How many of `counts.rows` stand in for both sides. Checked in EVERY build, for the reason
/// [`Edits::uncount`] gives: a subtraction that wrapped would size a pass's maps to a capacity no
/// allocation can serve.
fn carryable_count(counts: Counts) -> usize {
	assert!(
		counts.uncarryable <= counts.rows,
		"every uncarryable row is a row: {} of {}",
		counts.uncarryable,
		counts.rows
	);
	counts.rows - counts.uncarryable
}

/// A lookup for a caller that asks about paths a directory at a time — the scan walks one
/// directory's entries (in whatever order the filesystem lists them), the reconcile reads its keys
/// sorted. Answers exactly what [`Baseline::get`] answers.
///
/// It answers the table's half out of a [`Page`] wherever the page spans the path asked, and the
/// pass's own edits over it exactly as `get` does. On the first path it is asked in a directory it
/// reads that directory's range from its start, which covers an ordinary directory whatever order
/// its entries are asked in; past that page it reads from the path asked. Both page sizes follow the
/// caller: a page most of whose rows were asked for makes the next one twice as large, one that
/// answered a single lookup halves it — so a walk of the table reads it in large pages, and a
/// caller asking about scattered paths pays about a point lookup each.
pub(super) struct Cursor<'a> {
	baseline: &'a Baseline,
	page: Option<Arc<Page>>,
	/// Whether `page` was read from a directory's start rather than from a path.
	page_is_dir: bool,
	/// The directory the last directory page was read for.
	dir: Option<String>,
	/// How many rows the next page of each kind reads, and how many lookups the current page has
	/// answered.
	want: usize,
	dir_want: usize,
	answered: usize,
}

impl Cursor<'_> {
	/// The most a page reads, however well the caller keeps to it.
	const MAX_PAGE: usize = 1024;

	fn adapt(want: &mut usize, rows: usize, answered: usize) {
		if answered * 2 >= rows.max(1) {
			*want = (*want * 2).min(Self::MAX_PAGE);
		} else if answered <= 1 {
			*want = (*want / 2).max(1);
		}
	}

	pub(super) fn get(&mut self, rel_path: &str) -> Option<BaselineEntry> {
		let baseline = self.baseline;
		if let Some(edits) = baseline.edits.as_deref() {
			if let Some(row) = edits.written.get(rel_path) {
				return Some(row.clone());
			}
			if edits.vacated_over(rel_path).is_some() {
				return None;
			}
		}
		let snapshot = baseline.rows.as_deref()?;
		let stored = self.stored(snapshot, rel_path)?;
		Some(baseline.marked(stored))
	}

	/// The table's row at `rel_path`, out of the current page or a new one.
	fn stored(&mut self, snapshot: &Snapshot, rel_path: &str) -> Option<BaselineEntry> {
		if let Some(answer) = self.page.as_deref().and_then(|page| page.answer(rel_path)) {
			self.answered += 1;
			return answer.cloned();
		}
		if let Some(page) = self.page.as_deref() {
			let want = if self.page_is_dir {
				&mut self.dir_want
			} else {
				&mut self.want
			};
			Self::adapt(want, page.rows.len(), self.answered);
		}
		self.answered = 1;
		let dir = rel_path.rsplit_once('/').map_or("", |(dir, _)| dir);
		if self.dir.as_deref() != Some(dir) {
			self.dir = Some(dir.to_string());
			let (from, to) = if dir.is_empty() {
				(String::new(), None)
			} else {
				(format!("{dir}/"), Some(format!("{dir}0")))
			};
			let page = snapshot.page(&from, false, to.as_deref(), self.dir_want);
			let answer = page.answer(rel_path).map(|row| row.cloned());
			self.page = Some(page);
			self.page_is_dir = true;
			if let Some(answer) = answer {
				return answer;
			}
		}
		let page = snapshot.page(rel_path, true, None, self.want);
		let answer = page
			.answer(rel_path)
			.expect("a page read from a path answers for that path")
			.cloned();
		self.page = Some(page);
		self.page_is_dir = false;
		answer
	}
}

/// Every row of the view STRICTLY under one path, in path order: the table's range, paged, merged
/// with the rows the pass wrote under the same path.
pub(super) struct Under<'a> {
	baseline: &'a Baseline,
	/// Where the next page of the table starts, and where the range ends.
	from: String,
	inclusive: bool,
	to: Option<String>,
	exhausted: bool,
	page: Option<Arc<Page>>,
	next: usize,
	stored: Option<BaselineEntry>,
	written: Box<dyn Iterator<Item = &'a BaselineEntry> + 'a>,
	next_written: Option<&'a BaselineEntry>,
}

impl<'a> Under<'a> {
	fn new(baseline: &'a Baseline, root: &str) -> Self {
		let (from, to) = if root.is_empty() {
			(String::new(), None)
		} else {
			(format!("{root}/"), Some(format!("{root}0")))
		};
		let mut written: Box<dyn Iterator<Item = &'a BaselineEntry> + 'a> =
			match baseline.edits.as_deref() {
				Some(edits) => Box::new(edits.written_under(root)),
				None => Box::new(std::iter::empty()),
			};
		let next_written = written.next();
		Self {
			baseline,
			from,
			inclusive: false,
			to,
			exhausted: baseline.rows.is_none(),
			page: None,
			next: 0,
			stored: None,
			written,
			next_written,
		}
	}

	/// The next table row the view shows, with the pass's marker on it.
	fn next_stored(&mut self) -> Option<BaselineEntry> {
		loop {
			let Some(row) = self
				.page
				.as_deref()
				.and_then(|page| page.rows.get(self.next))
				.cloned()
			else {
				if self.exhausted {
					return None;
				}
				let snapshot = self.baseline.rows.as_deref()?;
				let page = snapshot.page(&self.from, self.inclusive, self.to.as_deref(), PAGE);
				self.exhausted = !page.is_full();
				if let Some(last) = page.rows.last() {
					self.from.clone_from(&last.rel_path);
					self.inclusive = false;
				}
				self.page = Some(page);
				self.next = 0;
				continue;
			};
			self.next += 1;
			let Some(edits) = self.baseline.edits.as_deref() else {
				return Some(row);
			};
			if let Some(vacated) = edits.vacated_over(&row.rel_path) {
				// Everything under a vacated path is out of the view, and it is one contiguous range
				// of the table: skip to its end rather than read it. The vacated row ITSELF is not
				// followed by its subtree (`a b` sorts between `a` and `a/`), so it is skipped alone.
				if row.rel_path.len() > vacated.len() {
					let past = format!("{vacated}0");
					self.page = None;
					if self.to.as_deref().is_some_and(|to| past.as_str() >= to) {
						self.exhausted = true;
						return None;
					}
					self.from = past;
					self.inclusive = true;
					self.exhausted = false;
				}
				continue;
			}
			if edits.written.contains_key(&row.rel_path) {
				continue;
			}
			return Some(self.baseline.marked(row));
		}
	}
}

impl Iterator for Under<'_> {
	type Item = BaselineEntry;

	fn next(&mut self) -> Option<BaselineEntry> {
		if self.stored.is_none() {
			self.stored = self.next_stored();
		}
		let take_written = match (&self.stored, self.next_written) {
			(None, None) => return None,
			(Some(_), None) => false,
			(None, Some(_)) => true,
			(Some(stored), Some(written)) => written.rel_path < stored.rel_path,
		};
		if take_written {
			let row = self.next_written.take().cloned();
			self.next_written = self.written.next();
			row
		} else {
			self.stored.take()
		}
	}
}

impl Baseline {
	/// The table's rows as `snapshot` reads them.
	pub(super) fn stored(snapshot: Snapshot) -> Self {
		Self {
			rows: Some(Arc::new(snapshot)),
			edits: None,
		}
	}

	/// The rows as the store would hold them, in any order: the tests' way of writing a baseline
	/// down. They go through the store's own write statement into a connection of their own, so
	/// every derived column and count is the one the store would have written.
	#[cfg(test)]
	pub(super) fn from_rows(rows: impl IntoIterator<Item = BaselineEntry>) -> Self {
		Self::stored(snapshot::standalone(rows))
	}

	fn counts(&self) -> Counts {
		self.rows
			.as_deref()
			.map_or_else(Counts::default, |snapshot| snapshot.counts)
	}

	/// `row` as this pass sees it: with the marker it advanced there, if it did.
	fn marked(&self, mut row: BaselineEntry) -> BaselineEntry {
		if let Some(agreed) = self
			.edits
			.as_deref()
			.and_then(|edits| edits.agreed.get(&row.rel_path))
		{
			row.agreed_hash = agreed.hash;
		}
		row
	}

	/// The pass's edits, begun on the first write.
	fn edits_mut(&mut self) -> &mut Edits {
		let counts = self.counts();
		Arc::make_mut(
			self.edits
				.get_or_insert_with(|| Arc::new(Edits::new(counts))),
		)
	}

	/// How many rows the pair tracks.
	pub(super) fn len(&self) -> usize {
		self.edits
			.as_deref()
			.map_or_else(|| self.counts().rows, |edits| edits.rows)
	}

	pub(super) fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// The row at exactly `rel_path`.
	pub(super) fn get(&self, rel_path: &str) -> Option<BaselineEntry> {
		if let Some(edits) = self.edits.as_deref() {
			if let Some(row) = edits.written.get(rel_path) {
				return Some(row.clone());
			}
			if edits.vacated_over(rel_path).is_some() {
				return None;
			}
		}
		let row = self.rows.as_deref()?.row(rel_path)?;
		Some(self.marked(row))
	}

	/// Whether a row sits at exactly `rel_path`, and whether it is carryable.
	fn carry_flag(&self, rel_path: &str) -> Option<bool> {
		if let Some(edits) = self.edits.as_deref() {
			if let Some(row) = edits.written.get(rel_path) {
				return Some(row.carryable());
			}
			if edits.vacated_over(rel_path).is_some() {
				return None;
			}
		}
		self.rows.as_deref()?.carryable_at(rel_path)
	}

	pub(super) fn contains_key(&self, rel_path: &str) -> bool {
		self.carry_flag(rel_path).is_some()
	}

	/// A lookup for a caller that asks about paths in path order (see [`Cursor`]). Answers exactly
	/// what [`get`](Self::get) answers.
	pub(super) fn cursor(&self) -> Cursor<'_> {
		Cursor {
			baseline: self,
			page: None,
			page_is_dir: false,
			dir: None,
			want: 1,
			dir_want: PAGE,
			answered: 0,
		}
	}

	/// Every row, parent before child, in path order (see the module doc).
	pub(super) fn iter(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		Under::new(self, "")
	}

	/// Every row, parent before child.
	pub(super) fn visit_rows(&self, mut visit: impl FnMut(&BaselineEntry)) {
		for row in self.iter() {
			visit(&row);
		}
	}

	/// Every row's path, parent before child.
	#[cfg(feature = "bench-internals")]
	pub(super) fn paths(&self) -> impl Iterator<Item = String> + '_ {
		self.iter().map(|row| row.rel_path)
	}

	/// Every row's path, parent before child.
	pub(super) fn visit_row_paths(&self, mut visit: impl FnMut(&str)) {
		for row in self.iter() {
			visit(&row.rel_path);
		}
	}

	/// Every row STRICTLY under `root` (`""` is every row), parent before child.
	pub(super) fn subtree(&self, root: &str) -> impl Iterator<Item = BaselineEntry> + '_ {
		Under::new(self, root)
	}

	/// Every path STRICTLY under `root` (`""` is every path), parent before child.
	pub(super) fn visit_subtree_paths(&self, root: &str, mut visit: impl FnMut(&str)) {
		for row in self.subtree(root) {
			visit(&row.rel_path);
		}
	}

	/// The rows awaiting confirmation of a push of ours. Empty on a converged pair.
	pub(super) fn unconfirmed(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		let stored = self
			.rows
			.as_deref()
			.map(Snapshot::unconfirmed)
			.unwrap_or_default();
		let Some(edits) = self.edits.as_deref() else {
			return stored.into_iter();
		};
		// A table row's candidacy is decided by its EFFECTIVE marker: one the pass advanced leaves
		// the set, or — advanced to something other than its content — joins it.
		let listed: BTreeSet<String> = stored.iter().map(|row| row.rel_path.clone()).collect();
		let joined: Vec<BaselineEntry> = edits
			.agreed
			.iter()
			.filter(|(path, agreed)| agreed.awaits && !listed.contains(*path))
			.filter_map(|(path, _)| self.rows.as_deref()?.row(path))
			.collect();
		let mut out: Vec<BaselineEntry> = stored
			.into_iter()
			.chain(joined)
			.filter(|row| {
				edits.shows_stored(&row.rel_path)
					&& edits
						.agreed
						.get(&row.rel_path)
						.is_none_or(|agreed| agreed.awaits)
			})
			.map(|row| self.marked(row))
			.collect();
		out.extend(
			edits
				.written
				.values()
				.filter(|row| row.awaits_confirmation())
				.cloned(),
		);
		out.into_iter()
	}

	pub(super) fn any_unconfirmed(&self) -> bool {
		let Some(edits) = self.edits.as_deref() else {
			return self.rows.as_deref().is_some_and(Snapshot::any_unconfirmed);
		};
		edits
			.written
			.values()
			.any(BaselineEntry::awaits_confirmation)
			|| edits
				.agreed
				.iter()
				.any(|(path, agreed)| agreed.awaits && edits.shows_stored(path))
			|| self.rows.as_deref().is_some_and(|snapshot| {
				snapshot.unconfirmed_paths().iter().any(|path| {
					edits.shows_stored(path) && !edits.agreed.contains_key(path.as_str())
				})
			})
	}

	/// Advance the marker of every row awaiting confirmation that `confirms` accepts to the content
	/// the row records — what a confirmation IS — and hand back each row as it now stands, for the
	/// caller to persist. An edit to THIS view, like [`set_agreed`](Self::set_agreed), which it
	/// is for a set of rows at once: the rows come from this view's own
	/// [`unconfirmed`](Self::unconfirmed), so none is read back to be advanced.
	pub(super) fn confirm_where(
		&mut self,
		mut confirms: impl FnMut(&BaselineEntry) -> bool,
	) -> Vec<BaselineEntry> {
		let rows: Vec<BaselineEntry> = self.unconfirmed().filter(|row| confirms(row)).collect();
		if rows.is_empty() {
			return rows;
		}
		let edits = self.edits_mut();
		rows.into_iter()
			.map(|row| {
				let agreed = row.content_hash;
				if let Some(written) = edits.written.get_mut(&row.rel_path) {
					written.agreed_hash = agreed;
					return written.clone();
				}
				let row = BaselineEntry {
					agreed_hash: agreed,
					..row
				};
				edits.agreed.insert(
					row.rel_path.clone(),
					Agreed {
						hash: agreed,
						awaits: row.awaits_confirmation(),
					},
				);
				row
			})
			.collect()
	}

	/// Whether any row records a remote item.
	pub(super) fn has_remote_rows(&self) -> bool {
		self.edits
			.as_deref()
			.map_or_else(|| self.counts().remote_rows, |edits| edits.remote_rows)
			> 0
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

	/// Every row at or under `rel_path` under ANY spelling, handed to `visit` until it answers
	/// `true`. The table's rows the view shows, then the pass's own. Each page is read before any
	/// of it is handed out, so `visit` may ask this baseline anything.
	fn visit_folded(&self, rel_path: &str, visit: &mut impl FnMut(&str) -> bool) -> bool {
		if rel_path.is_empty() {
			// `""` holds nothing: nothing is `""`, and nothing is strictly under it.
			return false;
		}
		let folded = collision_key(rel_path);
		let edits = self.edits.as_deref();
		let shown = |path: &str| edits.is_none_or(|edits| edits.shows_stored(path));
		if let Some(snapshot) = self.rows.as_deref() {
			for path in snapshot.folded_at(&folded) {
				if shown(&path) && visit(&path) {
					return true;
				}
			}
			let mut after: Option<(String, String)> = None;
			loop {
				let page = snapshot.folded_under(
					&folded,
					after.as_ref().map(|(f, p)| (f.as_str(), p.as_str())),
					PAGE,
				);
				let last = page.len() < PAGE;
				for (_, path) in &page {
					if shown(path) && visit(path) {
						return true;
					}
				}
				if last {
					break;
				}
				after = page.into_iter().next_back();
			}
		}
		edits.is_some_and(|edits| edits.written_folded(&folded).any(&mut *visit))
	}

	/// Whether a row sits at `rel_path` under ANY spelling, or anywhere under it.
	pub(super) fn occupied(&self, rel_path: &str) -> bool {
		self.visit_folded(rel_path, &mut |_| true)
	}

	/// The path of every row whose path folds to `rel_path`, in path order.
	pub(super) fn folded_row_paths(&self, rel_path: &str) -> Vec<String> {
		if rel_path.is_empty() {
			return Vec::new();
		}
		let folded = collision_key(rel_path);
		let edits = self.edits.as_deref();
		let mut out: Vec<String> = self
			.rows
			.as_deref()
			.map(|snapshot| snapshot.folded_at(&folded))
			.unwrap_or_default()
			.into_iter()
			.filter(|path| edits.is_none_or(|edits| edits.shows_stored(path)))
			.collect();
		if let Some(edits) = edits {
			out.extend(
				edits
					.written_folded(&folded)
					.filter(|path| collision_key(path) == folded)
					.map(str::to_string),
			);
		}
		out.sort_unstable();
		out
	}

	/// Whether the rows still track anything at `rel_path`, or — for a directory — under it.
	pub(super) fn tracked(&self, rel_path: &str, is_dir: bool) -> bool {
		self.contains_key(rel_path) || (is_dir && self.subtree(rel_path).next().is_some())
	}

	/// How many rows stand in for both sides.
	pub(super) fn carryable_rows(&self) -> usize {
		self.edits
			.as_deref()
			.map_or_else(|| carryable_count(self.counts()), |edits| edits.carryable)
	}

	/// Whether the row at `rel_path` stands in for both sides.
	pub(super) fn carryable(&self, rel_path: &str) -> bool {
		self.carry_flag(rel_path) == Some(true)
	}

	/// The table's paths in one of its small indexes that the view still shows, then the pass's own
	/// rows the same rule picks. In no order.
	fn indexed(
		&self,
		stored: impl FnOnce(&Snapshot) -> Vec<String>,
		written: impl Fn(&BaselineEntry) -> bool,
	) -> std::vec::IntoIter<String> {
		let mut out = self.rows.as_deref().map(stored).unwrap_or_default();
		if let Some(edits) = self.edits.as_deref() {
			out.retain(|path| edits.shows_stored(path));
			out.extend(
				edits
					.written
					.values()
					.filter(|row| written(row))
					.map(|row| row.rel_path.clone()),
			);
		}
		out.into_iter()
	}

	/// The path of every `.filenignore` row, in no order.
	pub(super) fn rule_file_rows(&self) -> impl Iterator<Item = String> + '_ {
		self.indexed(Snapshot::rule_file_paths, BaselineEntry::is_rule_file)
	}

	/// The path of every row that stands in for neither side, in no order.
	pub(super) fn uncarryable_paths(&self) -> impl Iterator<Item = String> + '_ {
		self.indexed(Snapshot::uncarryable_paths, |row| !row.carryable())
	}

	/// Whether any row at or under `rel_path`, under any spelling, satisfies `held`. ROWS ONLY: a
	/// caller that also holds paths no row tracks scans those itself.
	pub(super) fn any_folded_row_at_or_under(
		&self,
		rel_path: &str,
		held: &mut impl FnMut(&str) -> bool,
	) -> bool {
		self.visit_folded(rel_path, held)
	}

	/// Whether every row STRICTLY under `rel_path` is `Synced`.
	pub(super) fn subtree_all_synced(&self, rel_path: &str) -> bool {
		let Some(edits) = self.edits.as_deref() else {
			return self
				.rows
				.as_deref()
				.is_none_or(|snapshot| snapshot.unsynced_under(rel_path).is_empty());
		};
		let stored = self
			.rows
			.as_deref()
			.map(|snapshot| snapshot.unsynced_under(rel_path))
			.unwrap_or_default();
		!stored.iter().any(|path| edits.shows_stored(path))
			&& edits
				.written_under(rel_path)
				.all(|row| row.state == BaselineState::Synced)
	}

	/// Re-key the row at `from` and everything under it to sit under `to`, in THIS view only:
	/// every row at or under `from` rewritten under `to`, overwriting whatever row the view shows at
	/// each destination and leaving the rest of the destination's subtree where it is. Every source
	/// leaves before any destination is written, so a rename by case alone keeps what it moved.
	pub(super) fn move_subtree(&mut self, from: &str, to: &str) {
		if from.is_empty() || to.is_empty() {
			// Neither end can be the pair root: it holds no row, and moving the whole pair onto or
			// out of it is a request no writer makes.
			tracing::error!("baseline: refusing to move the pair root ({from:?} -> {to:?})");
			return;
		}
		let moving: Vec<BaselineEntry> = self
			.get(from)
			.map(|row| BaselineEntry {
				rel_path: to.to_string(),
				..row
			})
			.into_iter()
			.chain(self.subtree(from).map(|row| BaselineEntry {
				rel_path: format!("{to}{}", &row.rel_path[from.len()..]),
				..row
			}))
			.collect();
		if moving.is_empty() {
			return;
		}
		let edits = self.edits_mut();
		for row in &moving {
			edits.uncount(row);
		}
		edits.vacated.insert(from.to_string());
		edits.unwrite_subtree(from);
		// What the view shows at or under the destination NOW, with every source gone: the rows the
		// writes below replace, read as one range rather than a lookup per moved row. The
		// destinations are distinct, so no write below changes another's.
		let mut replaced: HashMap<String, BaselineEntry> = self
			.get(to)
			.into_iter()
			.chain(self.subtree(to))
			.map(|row| (row.rel_path.clone(), row))
			.collect();
		let edits = self.edits_mut();
		for row in moving {
			let old = replaced.remove(&row.rel_path);
			edits.write(row, old.as_ref());
		}
	}

	/// Advance the agreed-content marker of the row at `rel_path` in THIS view, and hand back the
	/// row as it now stands. `None` when nothing is there. An edit, like
	/// [`move_subtree`](Self::move_subtree). The engine confirms through
	/// [`confirm_where`](Self::confirm_where); this is the tests' way of setting ANY marker, which is
	/// what reaches the shapes a confirmation alone never makes.
	#[cfg(test)]
	pub(super) fn set_agreed(
		&mut self,
		rel_path: &str,
		agreed_hash: Option<Blake3Hash>,
	) -> Option<BaselineEntry> {
		let current = self.get(rel_path)?;
		let edits = self.edits_mut();
		if let Some(row) = edits.written.get_mut(rel_path) {
			row.agreed_hash = agreed_hash;
			return Some(row.clone());
		}
		let row = BaselineEntry {
			agreed_hash,
			..current
		};
		edits.agreed.insert(
			rel_path.to_string(),
			Agreed {
				hash: agreed_hash,
				awaits: row.awaits_confirmation(),
			},
		);
		Some(row)
	}

	/// Write `entry` into the rows a test built this baseline from. Only before the pass has edited
	/// them. A baseline a clone still shares is copied first, so the clone keeps the rows it had.
	#[cfg(test)]
	pub(super) fn upsert_for_test(&mut self, entry: &BaselineEntry) {
		assert!(
			self.edits.is_none(),
			"the rows behind a baseline this pass has already edited"
		);
		let unshared = self
			.rows
			.as_mut()
			.is_some_and(|rows| Arc::get_mut(rows).is_some());
		if !unshared {
			*self = Self::from_rows(self.iter().collect::<Vec<_>>());
		}
		Arc::get_mut(self.rows.as_mut().expect("a test's rows"))
			.expect("a copy nothing shares")
			.upsert_for_test(entry);
	}

	/// What the pair holds in memory for the life of the pair, in bytes: the handle a pass reads
	/// the table through, and nothing per row. What SQLite caches for it is its reader's page
	/// cache, bounded by `baseline::READER_CACHE_KIB` and measured as a resident set, not here.
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_bytes(&self) -> usize {
		self.resident_terms().total()
	}

	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_terms(&self) -> ResidentTerms {
		ResidentTerms {
			handle: self.rows.as_ref().map_or(0, |_| size_of::<Snapshot>()),
		}
	}
}

/// What [`Baseline::resident_bytes`] is made of, one field per structure it counts.
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResidentTerms {
	/// The snapshot handle: a connection, its pair and its counts.
	pub(super) handle: usize,
}

#[cfg(feature = "bench-internals")]
impl ResidentTerms {
	/// Every term under its own name, in the order a table should print them.
	pub(super) fn named(&self) -> [(&'static str, usize); 1] {
		[("handle", self.handle)]
	}

	pub(super) fn total(&self) -> usize {
		self.named().iter().map(|&(_, bytes)| bytes).sum()
	}
}

#[cfg(test)]
pub(super) mod tests {
	use std::collections::BTreeSet;

	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use rand::{Rng, SeedableRng, rngs::StdRng};
	use uuid::Uuid;

	use super::{
		super::{
			baseline::{BaselineEntry, BaselineState, NodeKind},
			tree::Tree,
		},
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

	/// A synced file row with contents `hash`, for another module's tests.
	pub(in super::super) fn file_for_test(hash: u8) -> BaselineEntry {
		file("unnamed", hash)
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
	/// (Ä, Greek sigma, Cyrillic, and the dotted İ whose lowering is LONGER than it), a name that
	/// sorts between a directory and its own subtree (`docs b`, `docs.txt`), and a row of every
	/// index — unconfirmed, conflicted, uncarryable, rule file.
	fn corpus() -> Vec<BaselineEntry> {
		vec![
			dir("docs"),
			file("docs/a.txt", 1),
			dir("docs/sub"),
			file("docs/sub/deep.txt", 2),
			file("docs/.filenignore", 3),
			file("docsx", 4),
			file("docs b", 18),
			file("docs.txt", 19),
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
	pub(in super::super) fn probes(paths: impl IntoIterator<Item = String>) -> Vec<String> {
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

	fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
		items.sort();
		items
	}

	fn by_path(mut rows: Vec<BaselineEntry>) -> Vec<BaselineEntry> {
		rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
		rows
	}

	/// The contract every enumeration keeps in place of the tree's walk order: strictly ascending
	/// by path, so a parent before everything under it and no row twice.
	fn assert_path_order(context: &str, paths: &[String]) {
		for pair in paths.windows(2) {
			assert!(
				pair[0] < pair[1],
				"{context}: {:?} came before {:?}, out of path order",
				pair[0],
				pair[1]
			);
		}
	}

	/// Every question the boundary answers, asked of `view` and of `oracle` — the resident TREE
	/// holding the same rows with the same edits written into it — and required to be the same
	/// answer. The tree is real code, not an expectation written down: a question the two answer
	/// differently fails whatever either answer is.
	///
	/// Where the tree has an order the reader does not keep (its walk: siblings by folded name,
	/// each subtree contiguous), the answers are compared as sets and the reader's own order is
	/// held to its contract instead (see the module doc).
	pub(in super::super) fn assert_alike(
		context: &str,
		view: &Baseline,
		oracle: &Tree,
		probes: &[String],
	) {
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
		let rows: Vec<BaselineEntry> = view.iter().collect();
		assert_path_order(
			&format!("{context}: iter"),
			&rows
				.iter()
				.map(|row| row.rel_path.clone())
				.collect::<Vec<_>>(),
		);
		assert_eq!(rows, by_path(oracle.iter().collect()), "{context}: iter");
		let mut visited = Vec::new();
		view.visit_rows(|row| visited.push(row.clone()));
		assert_eq!(visited, rows, "{context}: visit_rows");
		let mut paths = Vec::new();
		view.visit_row_paths(|path| paths.push(path.to_string()));
		assert_eq!(
			paths,
			sorted(oracle.paths().collect()),
			"{context}: visit_row_paths"
		);
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
		// The cursor asked in path order, which is what it pages for, and then backwards, which
		// defeats every page it reads: both must answer as `get` does.
		for order in [false, true] {
			let mut cursor = view.cursor();
			let asked: Vec<&String> = if order {
				probes.iter().rev().collect()
			} else {
				probes.iter().collect()
			};
			for path in asked {
				assert_eq!(
					cursor.get(path),
					oracle.get(path),
					"{context}: at {path:?}: cursor (reversed: {order})"
				);
			}
		}
		for path in probes {
			let at = format!("{context}: at {path:?}");
			assert_eq!(view.get(path), oracle.get(path), "{at}: get");
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
				sorted(oracle.folded_row_paths(path)),
				"{at}: folded_row_paths"
			);
			// Every row the folded walk offers, as a set: offered in full when nothing is taken.
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
				(view_any, sorted(offered.clone())),
				(oracle_any, sorted(oracle_offered)),
				"{at}: any_folded_row_at_or_under"
			);
			// And it stops at the first row taken, whichever that is.
			if let Some(last) = offered.last().cloned() {
				let mut after = 0;
				assert!(
					view.any_folded_row_at_or_under(path, &mut |row: &str| {
						after += 1;
						row == last
					}),
					"{at}: any_folded_row_at_or_under takes a row it offered"
				);
				assert_eq!(after, offered.len(), "{at}: offered past the row it took");
			}
			assert_eq!(
				view.subtree_all_synced(path),
				oracle.subtree_all_synced(path),
				"{at}: subtree_all_synced"
			);
			let under: Vec<BaselineEntry> = view.subtree(path).collect();
			assert_path_order(
				&format!("{at}: subtree"),
				&under
					.iter()
					.map(|row| row.rel_path.clone())
					.collect::<Vec<_>>(),
			);
			assert_eq!(
				under,
				by_path(oracle.subtree(path).collect()),
				"{at}: subtree"
			);
			let mut under_paths = Vec::new();
			view.visit_subtree_paths(path, |p| under_paths.push(p.to_string()));
			let mut oracle_under = Vec::new();
			oracle.visit_subtree_paths(path, |p| oracle_under.push(p.to_string()));
			assert_eq!(
				under_paths,
				sorted(oracle_under),
				"{at}: visit_subtree_paths"
			);
		}
	}

	/// The fold window. A folded directory move re-keys a subtree the store still holds under its
	/// source, and `plan::next_dir_move` then asks `occupied(to)` and `plan::parents_ready` asks
	/// `occupied(parent)` of the pass's view — which must answer with the source vacated and the
	/// destination occupied, exactly as a copy of the tree with the move written into it does,
	/// while the TABLE still holds the rows under the source.
	#[test]
	fn a_folded_move_answers_the_fold_windows_questions_as_a_moved_copy_does() {
		let rows = corpus();
		let mut view = Baseline::from_rows(rows.clone());
		let mut oracle = Tree::from_rows(rows);
		view.move_subtree("docs", "archive/docs");
		oracle.move_subtree("docs", "archive/docs");
		assert!(view.edits.is_some(), "the move is an edit beside the table");
		assert!(
			view.rows
				.as_deref()
				.is_some_and(|snapshot| snapshot.row("docs/a.txt").is_some()),
			"the table still holds the source: this is the window, not a committed move"
		);

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

	/// Apply one random edit — a move or a confirmation — to both the view and the oracle, and say
	/// what it was. The sequences this draws chain moves through each other's sources and
	/// destinations, move a subtree back onto where it came from, rename by case alone, move a
	/// directory into its own subtree, and advance the markers of rows that have and have not
	/// moved.
	pub(in super::super) fn random_edit(
		rng: &mut StdRng,
		step: usize,
		view: &mut Baseline,
		oracle: &mut Tree,
		probes: &[String],
	) -> (String, bool) {
		// A path among those the copy holds now, or any probe at all — which reaches paths no row
		// names, folded spellings and nothing at all.
		let held = oracle.paths().collect::<Vec<_>>();
		let pick =
			|rng: &mut StdRng, from: &[String]| from[rng.random_range(0..from.len())].clone();
		if rng.random_range(0..6) == 0 {
			// A confirmation as the engine makes one: every unconfirmed row a predicate takes,
			// advanced to its own content. The tree's form of it is one `set_agreed` per row.
			let salt = rng.random_range(0..3_usize);
			let takes = |row: &BaselineEntry| !(row.rel_path.len() + salt).is_multiple_of(3);
			let mut confirmed = view.confirm_where(takes);
			let mut expected: Vec<BaselineEntry> = oracle
				.unconfirmed()
				.filter(takes)
				.collect::<Vec<_>>()
				.into_iter()
				.filter_map(|row| oracle.set_agreed(&row.rel_path, row.content_hash))
				.collect();
			confirmed.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
			expected.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
			assert_eq!(
				confirmed, expected,
				"step {step}: confirm_where confirmed different rows"
			);
			return (format!("confirm_where(salt {salt})"), false);
		}
		if rng.random_range(0..4) == 0 {
			let path = if held.is_empty() || rng.random_range(0..3) == 0 {
				pick(rng, probes)
			} else {
				pick(rng, &held)
			};
			let hash = match rng.random_range(0..3) {
				0 => None,
				1 => oracle.get(&path).and_then(|row| row.content_hash),
				_ => Some(Blake3Hash::from([rng.random_range(0..4); 32])),
			};
			assert_eq!(
				view.set_agreed(&path, hash),
				oracle.set_agreed(&path, hash),
				"step {step}: set_agreed({path:?}) handed back different rows"
			);
			(format!("set_agreed({path:?}, {hash:?})"), false)
		} else {
			let from = if held.is_empty() || rng.random_range(0..4) == 0 {
				pick(rng, probes)
			} else {
				pick(rng, &held)
			};
			let to = match rng.random_range(0..5) {
				// A destination under a path the pair holds, or a fresh one.
				0 => format!("{}/moved{step}", pick(rng, probes)),
				1 => format!("moved{step}"),
				// The source by case alone.
				2 => from.to_uppercase(),
				// Anything the pair holds now: a move onto an occupied destination.
				_ if !held.is_empty() => pick(rng, &held),
				_ => pick(rng, probes),
			};
			let to = to.trim_start_matches('/').to_string();
			view.move_subtree(&from, &to);
			oracle.move_subtree(&from, &to);
			(format!("move_subtree({from:?} -> {to:?})"), true)
		}
	}

	/// Every edit sequence a pass can write, and many it cannot, answered alike by the view and by
	/// the tree with the same edits written into it — after every single edit.
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
				let (what, moved) = random_edit(&mut rng, step, &mut view, &mut oracle, &probes);
				if moved {
					moves += 1;
				} else {
					confirms += 1;
				}
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

	/// A pass that edits nothing reads the table itself, and one that edits never writes into it: a
	/// clone taken before the edits still reads the rows as they were.
	#[test]
	fn an_edit_never_reaches_the_table() {
		let before = Baseline::from_rows(corpus());
		let mut pass = before.clone();
		assert!(
			pass.set_agreed("nothing/here", None).is_none() && pass.edits.is_none(),
			"a confirmation that finds no row begins no edits"
		);
		let rows: Vec<BaselineEntry> = before.iter().collect();
		pass.move_subtree("docs", "moved");
		pass.set_agreed("pushed-top.txt", None);
		pass.set_agreed("moved/pushed.txt", Some(Blake3Hash::from([14; 32])));
		assert!(
			Arc::ptr_eq(
				pass.rows.as_ref().expect("rows"),
				before.rows.as_ref().expect("rows")
			),
			"the pass still reads the same snapshot"
		);
		assert_eq!(before.iter().collect::<Vec<_>>(), rows);
		assert!(!pass.contains_key("docs/a.txt") && pass.contains_key("moved/a.txt"));
	}

	/// A page boundary falls inside every range the reader pages over: more rows than a page under
	/// one directory, under one folded spelling, and in the whole pair — with a moved-away subtree
	/// in the middle of them for the enumeration to skip past.
	#[test]
	fn every_paged_read_answers_across_its_page_boundaries() {
		let mut rows = vec![dir("big"), dir("BIG"), dir("big/moved")];
		for n in 0..(PAGE * 2 + 7) {
			rows.push(file(&format!("big/{n:04}.txt"), 1));
			rows.push(file(&format!("BIG/{n:04}.txt"), 2));
		}
		for n in 0..(PAGE + 3) {
			rows.push(file(&format!("big/moved/{n:04}.txt"), 3));
		}
		let mut view = Baseline::from_rows(rows.clone());
		let mut oracle = Tree::from_rows(rows);
		let probes = probes(["big/0001.txt", "big/moved/0002.txt", "elsewhere"].map(String::from));
		assert_alike("unedited", &view, &oracle, &probes);
		view.move_subtree("big/moved", "elsewhere");
		oracle.move_subtree("big/moved", "elsewhere");
		assert_alike("with a paged subtree moved away", &view, &oracle, &probes);
	}
}
