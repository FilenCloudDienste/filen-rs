//! The baseline rows a pass reads, behind the one type every consumer reads them through.
//!
//! [`Baseline`] is a BOUNDARY: every consumer reads the rows through it, and none names what backs
//! it. What backs it is the store's own table — a [`Snapshot`], one read transaction on a reader
//! connection of the pass's own (see `rows/snapshot.rs`) — plus the pass's own [`Edits`] once it
//! makes any. The store keeps nothing of a pair's rows in memory between passes; the resident tree
//! that used to (`tree.rs`, ~127 bytes a row for the life of the pair) is compiled only into the
//! tests, where it is the ORACLE every answer here is held to.
//!
//! Every question is an indexed statement or a keyset-paged range off `baseline`'s columns and
//! indexes (see `baseline::SCHEMA`) — or no statement at all, where a page already read spans it:
//!
//! | Method | Shape | Answered by |
//! |---|---|---|
//! | [`get`](Baseline::get), [`contains_key`](Baseline::contains_key), [`carryable`](Baseline::carryable) | point, exact path | the last page any enumeration read, where it spans the path (see `snapshot::Page`) — else a page of the primary key's range read from the path, one row long unless the questions before it walked in path order (see `Snapshot::point`) |
//! | [`cursor`](Baseline::cursor) | point, asked a directory at a time | on a directory's first miss the one row asked for, while no directory before it needed more ([`Cursor`]), else a page of the directory's range; then pages from the path asked, sized to how many of the last page's rows were asked for |
//! | [`tracked`](Baseline::tracked) | point, or anything strictly under | the primary key, then one row of the range `(p/, p0)` |
//! | [`occupied`](Baseline::occupied) | folded, at or under | `baseline_folded`: a page from the folded path on, unless the last one spans it (see `Snapshot::folded_at`), then one row of the range `(f/, f0)` |
//! | [`folded_row_paths`](Baseline::folded_row_paths) | folded, exact | `baseline_folded`: a page from the folded path on, as above |
//! | [`any_folded_row_at_or_under`](Baseline::any_folded_row_at_or_under) | folded, at or under, first match | the two above, paged, stopping at the first row the predicate takes |
//! | [`subtree_all_synced`](Baseline::subtree_all_synced) | subtree predicate | `baseline_state (pair_id, state, rel_path)`: one seek per unsynced state, bounded to the range |
//! | [`subtree`](Baseline::subtree), [`visit_subtree_paths`](Baseline::visit_subtree_paths) | subtree | the primary key's range `(p/, p0)`, paged |
//! | [`iter`](Baseline::iter), [`visit_rows`](Baseline::visit_rows), [`visit_row_paths`](Baseline::visit_row_paths) | whole pair | the primary key, paged |
//! | [`len`](Baseline::len), [`carryable_rows`](Baseline::carryable_rows), [`has_remote_rows`](Baseline::has_remote_rows) | count | `baseline_counts`, read once when the snapshot begins |
//! | [`any_unconfirmed`](Baseline::any_unconfirmed), [`unconfirmed`](Baseline::unconfirmed) | small set | the partial index `baseline_unconfirmed` |
//! | [`uncarryable_paths`](Baseline::uncarryable_paths) | small set | the partial index `baseline_uncarryable` |
//! | [`rule_file_rows`](Baseline::rule_file_rows) | small set | the partial index `baseline_rule_files` |
//! | [`confirm_where`](Baseline::confirm_where), [`move_subtree`](Baseline::move_subtree) | pass-private WRITE | a layer of [`Edits`], never a store write (see below) |
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
//! [`confirm_where`](Baseline::confirm_where) (the three confirmation steps) edit the PASS's view of
//! the rows, never the store's: a dry run persists neither, and a real pass persists the
//! confirmations itself and commits the move when it applies it. Each is a LAYER of the pass's
//! [`Edits`], read over the table and the layers beneath it, and each costs what the pass changed:
//!
//! - a confirmation is a marker entry keyed by the path it advanced ([`Block`]);
//! - a folded directory move is the MAPPING it is ([`Layer::Moved`]) — two paths, whatever the moved
//!   directory holds: a path under the destination is read as the same path under the source, one
//!   step per layer. That holds because the destination held nothing once the source had left it,
//!   which every move the fold makes satisfies (it asks `occupied(to)` first). A move onto rows it
//!   has to merge with — which no fold makes, and the tests do — writes the rows it moves instead,
//!   and vacates their source ([`Block`] again).
//!
//! That overlay is load-bearing. Between the first `move_subtree` of a pass and the move's commit,
//! the pass's view and the database DISAGREE on purpose: the moved subtree is keyed under its
//! destination here and under its source in the table. `occupied(to)` in `plan::next_dir_move` and
//! `occupied(parent)` in `plan::parents_ready` are asked INSIDE that window, and so is every read
//! after it — the reconcile, the apply layer's `get`s, a carried side's rows. Answered from the
//! table alone, they would consult rows the fold has already re-keyed away and approve a directory
//! move onto an occupied destination. So every method answers against the edited view, the folded
//! questions included: folding is per character, so the folded path of `to/rest` is the folded `to`
//! followed by the folded `/rest`, and a folded question under a mapped destination is the same
//! question under the folded source. The tests hold every method to what a COPY of the resident
//! tree with the same edits written into it answers.
//!
//! # Read consistency and threads
//!
//! The snapshot is one read transaction, so a pass's PLAN reads one state of the table throughout
//! (see `rows/snapshot.rs`) — the state before anything the pass itself commits. The transaction
//! ends with the plan: [`freeze`](Baseline::freeze) reads the rows at the paths the plan's actions
//! name into a baseline of their own and drops the snapshot, so the apply layer answers from the
//! plan's rows while the WAL checkpoints behind it. A pass clones its baseline into the local scan's
//! blocking thread; the connection sits behind a mutex, so a clone is readable from any thread and
//! two readers take turns.

use std::{
	borrow::Cow,
	collections::{BTreeMap, BTreeSet, HashMap},
	iter::Peekable,
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
use self::snapshot::{Counts, MAX_PAGE, PAGE, Page};
#[cfg(feature = "bench-internals")]
pub(super) use self::snapshot::{reads, reset_reads};
use super::{
	baseline::{BaselineEntry, BaselineState, NodeKind},
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
	/// The rows at the paths the pass's plan names, and nothing else — set once the plan is made
	/// (see [`freeze`](Baseline::freeze)), when `rows` and `edits` are gone. A path it does not
	/// hold is a question the apply layer was never meant to ask, and asking it panics rather than
	/// answer "no row".
	frozen: Option<Arc<HashMap<String, Option<BaselineEntry>>>>,
}

/// What a pass changed about the rows (see the module doc): LAYERS over the table, bottom first,
/// each read over the view the table and the layers beneath it make — plus the edited view's
/// counts, so the whole-set questions stay O(1) whatever the pass moved.
#[derive(Debug, Clone)]
struct Edits {
	layers: Vec<Layer>,
	rows: usize,
	carryable: usize,
	remote_rows: usize,
}

#[derive(Debug, Clone)]
enum Layer {
	/// Rows written, paths vacated and markers advanced, over the view below.
	Block(Block),
	/// Every row the view below holds at or under `from`, read at the same place under `to`
	/// instead — a directory move folded as the MAPPING it is, never as a copy of the rows it
	/// moves. Only ever a move onto a destination the view below holds nothing at or under once
	/// `from`'s own rows have left it: that is what lets a path under `to` be answered by `from`
	/// alone, with nothing of the destination's to fall back on.
	Moved { from: String, to: String },
}

/// Rows written, paths vacated and markers advanced over the view below (see [`Layer::Block`]).
/// Keyed by PATH throughout, because the table is: a row of the view it makes is a written row, or
/// else a row below that no vacated path sits at or above, carrying the marker advanced on it here
/// where there is one.
#[derive(Debug, Clone, Default)]
struct Block {
	/// Every row written here, at the path it now sits at. Replaces the row below there.
	written: BTreeMap<String, BaselineEntry>,
	/// `(collision_key, path)` of every row in [`written`](Self::written): the folded questions'
	/// index over the rows written here, as `baseline_folded` is over the table's.
	folded: BTreeSet<(String, String)>,
	/// The paths below moved away, each standing for itself and its whole subtree: no row below at
	/// or under one is in the view.
	vacated: BTreeSet<String>,
	/// The marker a confirmation advanced on a row below — the one field a confirmation changes, so
	/// a confirmed push costs an entry here rather than a written row.
	agreed: HashMap<String, Agreed>,
}

/// A marker the pass advanced on a row, and whether the row, carrying it, still awaits
/// confirmation — decided when the marker is set, from the row in hand, so that the question "is
/// anything unconfirmed" never has to read the row back.
#[derive(Debug, Clone, Copy)]
struct Agreed {
	hash: Option<Blake3Hash>,
	awaits: bool,
}

impl Block {
	/// The vacated path at or above `rel_path`, if one hides it.
	fn vacated_over<'b>(&'b self, rel_path: &str) -> Option<&'b str> {
		if self.vacated.is_empty() {
			return None;
		}
		rel_path
			.match_indices('/')
			.map(|(at, _)| &rel_path[..at])
			.chain([rel_path])
			.find_map(|at| self.vacated.get(at).map(String::as_str))
	}

	/// Whether the view shows the row BELOW at `rel_path`: nothing vacated over it, and no row
	/// written here in its place.
	fn shows_below(&self, rel_path: &str) -> bool {
		self.vacated_over(rel_path).is_none() && !self.written.contains_key(rel_path)
	}

	/// `row`, a row below, with the marker advanced on it here.
	fn mark(&self, mut row: BaselineEntry) -> BaselineEntry {
		if let Some(agreed) = self.agreed.get(&row.rel_path) {
			row.agreed_hash = agreed.hash;
		}
		row
	}

	fn write(&mut self, row: BaselineEntry) {
		self.folded
			.insert((collision_key(&row.rel_path), row.rel_path.clone()));
		self.written.insert(row.rel_path.clone(), row);
	}

	/// Drop every row written here at or under `rel_path`.
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

	/// The rows written here inside `span`, in path order.
	fn written_in<'b>(&'b self, span: &Span) -> impl Iterator<Item = &'b BaselineEntry> + 'b {
		let lo = if span.inclusive {
			Bound::Included(span.lo.clone())
		} else {
			Bound::Excluded(span.lo.clone())
		};
		let hi = span.hi.clone().map_or(Bound::Unbounded, Bound::Excluded);
		self.written
			.range::<String, _>((lo, hi))
			.map(|(_, row)| row)
	}

	/// The paths written here whose folded path is `folded` — and, unless `exact`, those strictly
	/// under it.
	fn written_folded<'b>(
		&'b self,
		folded: &'b str,
		exact: bool,
	) -> impl Iterator<Item = &'b str> + 'b {
		let at = self
			.folded
			.range((folded.to_string(), String::new())..)
			.take_while(move |(key, _)| key == folded);
		let under = self
			.folded
			.range((format!("{folded}/"), String::new())..(format!("{folded}0"), String::new()))
			.take_while(move |_| !exact);
		at.chain(under).map(|(_, path)| path.as_str())
	}
}

impl Edits {
	fn new(counts: Counts) -> Self {
		Self {
			layers: Vec::new(),
			rows: counts.rows,
			carryable: carryable_count(counts),
			remote_rows: counts.remote_rows,
		}
	}

	/// The block an edit is written into: the top layer, or a new one over a move.
	fn top_block(&mut self) -> &mut Block {
		if !matches!(self.layers.last(), Some(Layer::Block(_))) {
			self.layers.push(Layer::Block(Block::default()));
		}
		match self.layers.last_mut() {
			Some(Layer::Block(block)) => block,
			_ => unreachable!("a block was just pushed"),
		}
	}

	fn count(&mut self, row: &BaselineEntry) {
		self.rows += 1;
		self.carryable += usize::from(row.carryable());
		self.remote_rows += usize::from(row.remote_uuid.is_some());
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

	/// What these edits hold on the heap and in place, in bytes, counted the way the probe counts
	/// every other structure: capacities, not lengths.
	#[cfg(feature = "bench-internals")]
	fn bytes(&self) -> usize {
		let string = |s: &String| size_of::<String>() + s.capacity();
		let row = |row: &BaselineEntry| size_of::<BaselineEntry>() + row.rel_path.capacity();
		size_of::<Self>()
			+ self.layers.capacity() * size_of::<Layer>()
			+ self
				.layers
				.iter()
				.map(|layer| match layer {
					Layer::Block(block) => {
						block
							.written
							.iter()
							.map(|(path, entry)| string(path) + row(entry))
							.sum::<usize>() + block
							.folded
							.iter()
							.map(|(key, path)| string(key) + string(path))
							.sum::<usize>() + block.vacated.iter().map(string).sum::<usize>()
							+ block.agreed.capacity() * size_of::<(String, Agreed)>()
							+ block.agreed.keys().map(String::capacity).sum::<usize>()
					}
					Layer::Moved { from, to } => from.capacity() + to.capacity(),
				})
				.sum::<usize>()
	}
}

fn is_at_or_under(path: &str, rel_path: &str) -> bool {
	path.strip_prefix(rel_path)
		.is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// What follows `rel_path` in `path` when `path` is at or under it: `""` at it, `/...` under it.
fn at_or_under<'p>(path: &'p str, rel_path: &str) -> Option<&'p str> {
	path.strip_prefix(rel_path)
		.filter(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `row`, read at `rel_path`, with `mark` advanced on it where a layer above advanced one.
fn placed(
	mut row: BaselineEntry,
	rel_path: &str,
	mark: Option<Option<Blake3Hash>>,
) -> BaselineEntry {
	if row.rel_path != rel_path {
		rel_path.clone_into(&mut row.rel_path);
	}
	if let Some(hash) = mark {
		row.agreed_hash = hash;
	}
	row
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

/// A range of paths: every path after `lo` (or at it, when `inclusive`), strictly before `hi`.
#[derive(Debug, Clone)]
struct Span {
	lo: String,
	inclusive: bool,
	hi: Option<String>,
}

impl Span {
	fn all() -> Self {
		Self {
			lo: String::new(),
			inclusive: false,
			hi: None,
		}
	}

	/// Every path STRICTLY under `root` (`""`: every path). `/` is 0x2F and `0` 0x30, so the range
	/// is exactly the paths continuing `root` with a separator.
	fn under(root: &str) -> Self {
		if root.is_empty() {
			return Self::all();
		}
		Self {
			lo: format!("{root}/"),
			inclusive: false,
			hi: Some(format!("{root}0")),
		}
	}

	fn contains(&self, path: &str) -> bool {
		let past_lo = if self.inclusive {
			path >= self.lo.as_str()
		} else {
			path > self.lo.as_str()
		};
		past_lo && self.hi.as_deref().is_none_or(|hi| path < hi)
	}

	/// Whether no path can be in it. Conservative: `false` can still mean empty.
	fn is_empty(&self) -> bool {
		self.hi.as_deref().is_some_and(|hi| hi <= self.lo.as_str())
	}

	/// The paths in both.
	fn and(&self, other: &Self) -> Self {
		let (lo, inclusive) = match self.lo.cmp(&other.lo) {
			std::cmp::Ordering::Less => (other.lo.clone(), other.inclusive),
			std::cmp::Ordering::Greater => (self.lo.clone(), self.inclusive),
			std::cmp::Ordering::Equal => (self.lo.clone(), self.inclusive && other.inclusive),
		};
		let hi = match (&self.hi, &other.hi) {
			(Some(a), Some(b)) => Some(a.min(b).clone()),
			(Some(a), None) => Some(a.clone()),
			(None, b) => b.clone(),
		};
		Self { lo, inclusive, hi }
	}

	/// The paths before `hi`.
	fn below(hi: String) -> Self {
		Self {
			hi: Some(hi),
			..Self::all()
		}
	}

	/// The paths from `lo` on.
	fn from_on(lo: String) -> Self {
		Self {
			lo,
			inclusive: true,
			hi: None,
		}
	}

	/// This span — non-empty and inside `under(to)` — at the same place under `from`. A prefix swap
	/// keeps the order of everything it applies to, so the rows in the result, re-keyed back under
	/// `to`, are this span's in the same order.
	fn moved_back(&self, to: &str, from: &str) -> Self {
		let back = |bound: &str| {
			let rest = bound
				.strip_prefix(to)
				.expect("a bound of a span under the destination continues the destination");
			format!("{from}{rest}")
		};
		Self {
			lo: back(&self.lo),
			inclusive: self.inclusive,
			hi: self.hi.as_deref().map(back),
		}
	}
}

/// Rows in path order.
type Rows<'a> = Box<dyn Iterator<Item = BaselineEntry> + 'a>;

/// Two iterators of DISJOINT paths, each in path order, as one in path order.
struct Merge<'a> {
	a: Peekable<Rows<'a>>,
	b: Peekable<Rows<'a>>,
}

impl Iterator for Merge<'_> {
	type Item = BaselineEntry;

	fn next(&mut self) -> Option<BaselineEntry> {
		let take_b = match (self.a.peek(), self.b.peek()) {
			(Some(a), Some(b)) => b.rel_path < a.rel_path,
			(None, _) => true,
			(Some(_), None) => false,
		};
		if take_b { self.b.next() } else { self.a.next() }
	}
}

fn merge<'a>(a: Rows<'a>, b: Rows<'a>) -> Rows<'a> {
	Box::new(Merge {
		a: a.peekable(),
		b: b.peekable(),
	})
}

/// The table's rows inside a span, in path order, a page at a time.
struct TableRows<'a> {
	snapshot: Option<&'a Snapshot>,
	span: Span,
	/// Directory rows only, off `baseline_dirs`.
	dirs: bool,
	exhausted: bool,
	page: Option<Arc<Page>>,
	next: usize,
}

impl TableRows<'_> {
	/// The span's next page, or `None` once it is read through.
	fn next_page(&mut self) -> Option<Arc<Page>> {
		if self.exhausted {
			return None;
		}
		let snapshot = self.snapshot?;
		let Span { lo, inclusive, hi } = &self.span;
		let page = if self.dirs {
			snapshot.dir_page(lo, *inclusive, hi.as_deref(), PAGE)
		} else {
			snapshot.page(lo, *inclusive, hi.as_deref(), PAGE)
		};
		self.exhausted = !page.is_full();
		if let Some(last) = page.rows.last() {
			self.span.lo.clone_from(&last.rel_path);
			self.span.inclusive = false;
		}
		Some(page)
	}
}

impl Iterator for TableRows<'_> {
	type Item = BaselineEntry;

	fn next(&mut self) -> Option<BaselineEntry> {
		loop {
			if let Some(row) = self
				.page
				.as_deref()
				.and_then(|page| page.rows.get(self.next))
			{
				self.next += 1;
				return Some(row.clone());
			}
			self.page = Some(self.next_page()?);
			self.next = 0;
		}
	}
}

/// Where the view `depth` layers deep answers for a path.
enum Lookup<'a> {
	/// A row a layer wrote, as it holds it — at its own path there.
	Written(&'a BaselineEntry),
	/// Nothing: a move took it away.
	Absent,
	/// The table's row at this path — the one asked, translated through every move above it.
	Table(Cow<'a, str>),
}

/// The set a pass asks for as paths, off one of the table's small indexes.
#[derive(Debug, Clone, Copy)]
enum Indexed {
	Uncarryable,
	RuleFiles,
}

impl Indexed {
	fn of_row(self, row: &BaselineEntry) -> bool {
		match self {
			Self::Uncarryable => !row.carryable(),
			Self::RuleFiles => row.is_rule_file(),
		}
	}
}

/// A lookup for a caller that asks about paths a directory at a time — the scan walks one
/// directory's entries (in whatever order the filesystem lists them), the reconcile reads its keys
/// sorted. Answers exactly what [`Baseline::get`] answers.
///
/// It answers the table's half out of a [`Page`] wherever the page spans the path asked, and the
/// pass's own edits over it exactly as `get` does. In a directory it reads that directory's range
/// from its start, which covers an ordinary directory whatever order its entries are asked in; past
/// that page it reads from the path asked. Both page sizes follow the caller: a page most of whose
/// rows were asked for makes the next one twice as large, one that answered a single lookup halves
/// it — so a walk of the table reads it in large pages, and a caller asking about scattered paths
/// pays about a point lookup each.
///
/// It starts out PROBING: a directory's first miss reads the one row asked for, as a point lookup
/// does ([`Read::Probe`]), and its range is read on the SECOND path asked there that the page in
/// hand cannot answer. The first directory that needs more than its probe stops the probing for
/// good. A pass deciding one changed file reads one row of its directory rather than all of it,
/// where a walk of the tree probes one directory, learns from it, and reads every directory after
/// it whole from the start.
pub(super) struct Cursor<'a> {
	baseline: &'a Baseline,
	page: Option<Arc<Page>>,
	/// How `page` was read, which says which of the two sizes its use adapts.
	read: Read,
	/// The directory the last page was read in.
	dir: Option<String>,
	/// Whether a directory's first miss reads the one row asked for, rather than its range: while
	/// every directory has needed no more than that.
	probe_first: bool,
	/// How many rows the next page of each kind reads, and how many lookups the current page has
	/// answered.
	want: usize,
	dir_want: usize,
	answered: usize,
}

/// How a [`Cursor`]'s page was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Read {
	/// The one row asked for, a directory's first miss.
	Probe,
	/// The directory's range from its start.
	Dir,
	/// From the path asked, on.
	Path,
}

impl Cursor<'_> {
	fn adapt(want: &mut usize, rows: usize, answered: usize) {
		if answered * 2 >= rows.max(1) {
			*want = (*want * 2).min(MAX_PAGE);
		} else if answered <= 1 {
			*want = (*want / 2).max(1);
		}
	}

	pub(super) fn get(&mut self, rel_path: &str) -> Option<BaselineEntry> {
		let baseline = self.baseline;
		if let Some(frozen) = baseline.frozen.as_deref() {
			return frozen_row(frozen, rel_path);
		}
		let (at, mark) = baseline.lookup(baseline.depth(), rel_path);
		let row = match at {
			Lookup::Written(row) => row.clone(),
			Lookup::Absent => return None,
			Lookup::Table(path) => {
				let snapshot = baseline.table()?;
				self.stored(snapshot, &path)?
			}
		};
		Some(placed(row, rel_path, mark))
	}

	/// The table's row at `rel_path`, out of the current page or a new one.
	fn stored(&mut self, snapshot: &Snapshot, rel_path: &str) -> Option<BaselineEntry> {
		if let Some(answer) = self.page.as_deref().and_then(|page| page.answer(rel_path)) {
			self.answered += 1;
			return answer.cloned();
		}
		if let Some(page) = self.page.as_deref() {
			match self.read {
				Read::Dir => Self::adapt(&mut self.dir_want, page.rows.len(), self.answered),
				Read::Path => Self::adapt(&mut self.want, page.rows.len(), self.answered),
				Read::Probe => {}
			}
		}
		self.answered = 1;
		let dir = rel_path.rsplit_once('/').map_or("", |(dir, _)| dir);
		let entered = self.dir.as_deref() != Some(dir);
		if entered {
			// The directory being left needed its probe and nothing more, or it did not.
			if self.dir.is_some() {
				self.probe_first = self.read == Read::Probe;
			}
			self.dir = Some(dir.to_string());
		}
		if entered && self.probe_first {
			return self.read_from(snapshot, rel_path, 1, Read::Probe);
		}
		if entered || self.read == Read::Probe {
			let (from, to) = if dir.is_empty() {
				(String::new(), None)
			} else {
				(format!("{dir}/"), Some(format!("{dir}0")))
			};
			let page = snapshot.page(&from, false, to.as_deref(), self.dir_want);
			let answer = page.answer(rel_path).map(|row| row.cloned());
			self.page = Some(page);
			self.read = Read::Dir;
			if let Some(answer) = answer {
				return answer;
			}
		}
		self.read_from(snapshot, rel_path, self.want, Read::Path)
	}

	/// A page of `limit` rows from `rel_path` on, kept as `read`, and its answer there.
	fn read_from(
		&mut self,
		snapshot: &Snapshot,
		rel_path: &str,
		limit: usize,
		read: Read,
	) -> Option<BaselineEntry> {
		let page = snapshot.page(rel_path, true, None, limit);
		let answer = page
			.answer(rel_path)
			.expect("a page read from a path answers for that path")
			.cloned();
		self.page = Some(page);
		self.read = read;
		answer
	}
}

/// The row the frozen rows hold at `rel_path` — and a panic where they hold nothing, because a path
/// the plan did not name is a question nothing after the plan was meant to ask. Answering "no row"
/// there would read a tracked path as untracked (see the module doc).
fn frozen_row(
	frozen: &HashMap<String, Option<BaselineEntry>>,
	rel_path: &str,
) -> Option<BaselineEntry> {
	frozen
		.get(rel_path)
		.unwrap_or_else(|| {
			panic!(
				"the apply layer asked the baseline about {rel_path:?}, which no planned action \
				 names — refusing to answer from rows the pass no longer reads"
			)
		})
		.clone()
}

impl Baseline {
	/// The table's rows as `snapshot` reads them.
	pub(super) fn stored(snapshot: Snapshot) -> Self {
		Self {
			rows: Some(Arc::new(snapshot)),
			edits: None,
			frozen: None,
		}
	}

	/// The rows as the store would hold them, in any order: the tests' way of writing a baseline
	/// down. They go through the store's own write statement into a connection of their own, so
	/// every derived column and count is the one the store would have written.
	#[cfg(test)]
	pub(super) fn from_rows(rows: impl IntoIterator<Item = BaselineEntry>) -> Self {
		Self::stored(snapshot::standalone(rows))
	}

	/// The table, for every read but a point lookup — which a FROZEN baseline cannot serve.
	fn table(&self) -> Option<&Snapshot> {
		assert!(
			self.frozen.is_none(),
			"a baseline frozen for the apply layer answers point lookups at the paths its plan \
			 names, and nothing else"
		);
		self.rows.as_deref()
	}

	fn counts(&self) -> Counts {
		self.table()
			.map_or_else(Counts::default, |snapshot| snapshot.counts)
	}

	/// How many layers of edits the view has.
	fn depth(&self) -> usize {
		self.edits.as_deref().map_or(0, |edits| edits.layers.len())
	}

	/// The layer `depth` counts up to (1 is the bottom one).
	fn layer(&self, depth: usize) -> &Layer {
		&self
			.edits
			.as_deref()
			.expect("a view with layers has edits")
			.layers[depth - 1]
	}

	/// The pass's edits, begun on the first write.
	fn edits_mut(&mut self) -> &mut Edits {
		let counts = self.counts();
		Arc::make_mut(
			self.edits
				.get_or_insert_with(|| Arc::new(Edits::new(counts))),
		)
	}

	/// Where the view `depth` layers deep answers for `rel_path`, and the marker the top-most layer
	/// that advanced one there advanced. One step per layer and no more: a move maps a path to ONE
	/// path below it, because its destination held nothing of its own.
	fn lookup<'a>(
		&'a self,
		depth: usize,
		rel_path: &'a str,
	) -> (Lookup<'a>, Option<Option<Blake3Hash>>) {
		let mut path: Cow<'a, str> = Cow::Borrowed(rel_path);
		let mut mark = None;
		let Some(edits) = self.edits.as_deref() else {
			return (Lookup::Table(path), mark);
		};
		for layer in edits.layers[..depth].iter().rev() {
			match layer {
				Layer::Block(block) => {
					if let Some(row) = block.written.get(path.as_ref()) {
						return (Lookup::Written(row), mark);
					}
					if block.vacated_over(&path).is_some() {
						return (Lookup::Absent, mark);
					}
					if mark.is_none() {
						mark = block.agreed.get(path.as_ref()).map(|agreed| agreed.hash);
					}
				}
				Layer::Moved { from, to } => {
					if let Some(rest) = at_or_under(&path, to) {
						let below = format!("{from}{rest}");
						path = Cow::Owned(below);
					} else if is_at_or_under(&path, from) {
						return (Lookup::Absent, mark);
					}
				}
			}
		}
		(Lookup::Table(path), mark)
	}

	fn get_at(&self, depth: usize, rel_path: &str) -> Option<BaselineEntry> {
		let (at, mark) = self.lookup(depth, rel_path);
		let row = match at {
			Lookup::Written(row) => row.clone(),
			Lookup::Absent => return None,
			Lookup::Table(path) => self.table()?.row(&path)?,
		};
		Some(placed(row, rel_path, mark))
	}

	/// Whether a row sits at exactly `rel_path` in the view `depth` layers deep, and whether it is
	/// carryable.
	fn carry_flag_at(&self, depth: usize, rel_path: &str) -> Option<bool> {
		match self.lookup(depth, rel_path).0 {
			Lookup::Written(row) => Some(row.carryable()),
			Lookup::Absent => None,
			Lookup::Table(path) => self.table()?.carryable_at(&path),
		}
	}

	/// The rows of the view `depth` layers deep inside `span`, in path order.
	fn rows_in(&self, depth: usize, span: Span, dirs: bool) -> Rows<'_> {
		if span.is_empty() {
			return Box::new(std::iter::empty());
		}
		if depth == 0 {
			return Box::new(self.table_rows(span, dirs));
		}
		match self.layer(depth) {
			Layer::Block(block) => {
				let written: Rows<'_> = Box::new(
					block
						.written_in(&span)
						.filter(move |row| !dirs || row.kind == NodeKind::Dir)
						.cloned(),
				);
				let below: Rows<'_> = Box::new(
					self.rows_in(depth - 1, span, dirs)
						.filter(move |row| block.shows_below(&row.rel_path))
						.map(move |row| block.mark(row)),
				);
				merge(below, written)
			}
			Layer::Moved { from, to } => {
				// What stays where it is: everything but `from` and its subtree, which is the point
				// `from` and the one range `(from/, from0)` — read around rather than read and dropped.
				let before = span.and(&Span::below(format!("{from}/")));
				let after = span.and(&Span::from_on(format!("{from}0")));
				let kept: Rows<'_> = Box::new(
					self.rows_in(depth - 1, before, dirs)
						.filter(move |row| row.rel_path != *from)
						.chain(self.rows_in(depth - 1, after, dirs)),
				);
				// What moved: the row at `from`, now at `to`, then `to`'s subtree, read as `from`'s.
				let point = span
					.contains(to)
					.then(|| self.get_at(depth - 1, from))
					.flatten()
					.filter(|row| !dirs || row.kind == NodeKind::Dir)
					.map(|row| placed(row, to, None));
				let under_to = span.and(&Span::under(to));
				let subtree: Rows<'_> = if under_to.is_empty() {
					Box::new(std::iter::empty())
				} else {
					Box::new(
						self.rows_in(depth - 1, under_to.moved_back(to, from), dirs)
							.map(move |row| {
								let path = format!("{to}{}", &row.rel_path[from.len()..]);
								BaselineEntry {
									rel_path: path,
									..row
								}
							}),
					)
				};
				merge(kept, Box::new(point.into_iter().chain(subtree)))
			}
		}
	}

	fn table_rows(&self, span: Span, dirs: bool) -> TableRows<'_> {
		let snapshot = self.table();
		TableRows {
			exhausted: snapshot.is_none(),
			snapshot,
			span,
			dirs,
			page: None,
			next: 0,
		}
	}

	/// Every row of the view inside `span` (directory rows alone when `dirs`), in path order, handed
	/// to `visit` by reference. A view with no edits — every pass that has folded no move and
	/// confirmed nothing yet — hands out the table's own pages as it reads them, so a walk of the
	/// pair decodes each row once and copies none. Each page is read before any of it is handed out,
	/// so `visit` may ask this baseline anything.
	fn visit_in(&self, span: Span, dirs: bool, visit: &mut dyn FnMut(&BaselineEntry)) {
		if self.depth() > 0 {
			for row in self.rows_in(self.depth(), span, dirs) {
				visit(&row);
			}
			return;
		}
		if span.is_empty() {
			return;
		}
		let mut rows = self.table_rows(span, dirs);
		while let Some(page) = rows.next_page() {
			for row in &page.rows {
				visit(row);
			}
		}
	}

	/// Offer the path of every row of the view `depth` layers deep whose folded path is `folded` —
	/// and, unless `exact`, every one strictly under it — to `visit` until it answers `true`; answer
	/// whether it did. Each table page is read before any of it is handed out, so `visit` may ask
	/// this baseline anything.
	fn visit_folded_in(
		&self,
		depth: usize,
		folded: &str,
		exact: bool,
		visit: &mut dyn FnMut(&str) -> bool,
	) -> bool {
		if depth == 0 {
			let Some(snapshot) = self.table() else {
				return false;
			};
			if snapshot.folded_at(folded).iter().any(|path| visit(path)) {
				return true;
			}
			if exact {
				return false;
			}
			let mut after: Option<(String, String)> = None;
			loop {
				let page = snapshot.folded_under(
					folded,
					after.as_ref().map(|(f, p)| (f.as_str(), p.as_str())),
					PAGE,
				);
				let last = page.len() < PAGE;
				if page.iter().any(|(_, path)| visit(path)) {
					return true;
				}
				if last {
					return false;
				}
				after = page.into_iter().next_back();
			}
		}
		match self.layer(depth) {
			Layer::Block(block) => {
				self.visit_folded_in(depth - 1, folded, exact, &mut |path| {
					block.shows_below(path) && visit(path)
				}) || block.written_folded(folded, exact).any(visit)
			}
			Layer::Moved { from, to } => {
				// What stayed: nothing of it is at or under `to` (see `Layer::Moved`).
				if self.visit_folded_in(depth - 1, folded, exact, &mut |path| {
					!is_at_or_under(path, from) && visit(path)
				}) {
					return true;
				}
				// What moved. Folding is per character, so the folded path of `to/rest` is the
				// folded `to` followed by the folded `/rest` — and `from/rest`'s is the folded `from`
				// followed by the same.
				let folded_to = collision_key(to);
				if !exact && is_at_or_under(&folded_to, folded) {
					// Every moved row is at or under `folded`.
					return (self.carry_flag_at(depth - 1, from).is_some() && visit(to))
						|| self
							.rows_in(depth - 1, Span::under(from), false)
							.any(|row| visit(&format!("{to}{}", &row.rel_path[from.len()..])));
				}
				match at_or_under(folded, &folded_to) {
					// `folded` IS the destination's folded path, and only an exact question gets here.
					Some("") => self.carry_flag_at(depth - 1, from).is_some() && visit(to),
					Some(rest) => {
						let source = format!("{}{rest}", collision_key(from));
						self.visit_folded_in(depth - 1, &source, exact, &mut |path| {
							at_or_under(path, from)
								.is_some_and(|rest| visit(&format!("{to}{rest}")))
						})
					}
					None => false,
				}
			}
		}
	}

	/// The rows of the view `depth` layers deep awaiting confirmation of a push of ours.
	fn unconfirmed_in(&self, depth: usize) -> Vec<BaselineEntry> {
		if depth == 0 {
			return self.table().map(Snapshot::unconfirmed).unwrap_or_default();
		}
		let below = self.unconfirmed_in(depth - 1);
		match self.layer(depth) {
			Layer::Block(block) => {
				// A row's candidacy is decided by its EFFECTIVE marker: one advanced here leaves the
				// set, or — advanced to something other than its content — joins it.
				let listed: BTreeSet<&str> =
					below.iter().map(|row| row.rel_path.as_str()).collect();
				let joined: Vec<BaselineEntry> = block
					.agreed
					.iter()
					.filter(|(path, agreed)| agreed.awaits && !listed.contains(path.as_str()))
					.filter_map(|(path, _)| self.get_at(depth - 1, path))
					.collect();
				below
					.into_iter()
					.chain(joined)
					.filter(|row| {
						block.shows_below(&row.rel_path)
							&& block
								.agreed
								.get(&row.rel_path)
								.is_none_or(|agreed| agreed.awaits)
					})
					.map(|row| block.mark(row))
					.chain(
						block
							.written
							.values()
							.filter(|row| row.awaits_confirmation())
							.cloned(),
					)
					.collect()
			}
			Layer::Moved { from, to } => below
				.into_iter()
				.map(|row| match at_or_under(&row.rel_path, from) {
					Some(rest) => BaselineEntry {
						rel_path: format!("{to}{rest}"),
						..row
					},
					None => row,
				})
				.collect(),
		}
	}

	fn any_unconfirmed_in(&self, depth: usize) -> bool {
		if depth == 0 {
			return self.table().is_some_and(Snapshot::any_unconfirmed);
		}
		match self.layer(depth) {
			// A move takes no row out of the view and adds none.
			Layer::Moved { .. } => self.any_unconfirmed_in(depth - 1),
			Layer::Block(block) => {
				block
					.written
					.values()
					.any(BaselineEntry::awaits_confirmation)
					|| block
						.agreed
						.iter()
						.any(|(path, agreed)| agreed.awaits && block.shows_below(path))
					|| {
						let below: Vec<String> = if depth == 1 {
							self.table()
								.map(Snapshot::unconfirmed_paths)
								.unwrap_or_default()
						} else {
							self.unconfirmed_in(depth - 1)
								.into_iter()
								.map(|row| row.rel_path)
								.collect()
						};
						below.iter().any(|path| {
							block.shows_below(path) && !block.agreed.contains_key(path.as_str())
						})
					}
			}
		}
	}

	/// The paths of the view `depth` layers deep in one of the table's small indexes, in no order.
	fn indexed_in(&self, depth: usize, which: Indexed) -> Vec<String> {
		if depth == 0 {
			return self
				.table()
				.map(|snapshot| match which {
					Indexed::Uncarryable => snapshot.uncarryable_paths(),
					Indexed::RuleFiles => snapshot.rule_file_paths(),
				})
				.unwrap_or_default();
		}
		let mut below = self.indexed_in(depth - 1, which);
		match self.layer(depth) {
			Layer::Block(block) => {
				below.retain(|path| block.shows_below(path));
				below.extend(
					block
						.written
						.values()
						.filter(|row| which.of_row(row))
						.map(|row| row.rel_path.clone()),
				);
				below
			}
			Layer::Moved { from, to } => {
				let mut out: Vec<String> = below
					.into_iter()
					.filter(|path| path != from)
					.map(|path| match at_or_under(&path, from) {
						Some(rest) => format!("{to}{rest}"),
						None => path,
					})
					.collect();
				// The row at `from` itself changes its NAME, and whether a row is a rule file is a
				// question about its name: asked again of it at `to`.
				if let Some(row) = self.get_at(depth - 1, from)
					&& which.of_row(&placed(row, to, None))
				{
					out.push(to.clone());
				}
				out
			}
		}
	}

	/// The path of every row of the view `depth` layers deep STRICTLY under `root` (`""`: every
	/// row) that is not `Synced`.
	fn unsynced_in(&self, depth: usize, root: &str) -> Vec<String> {
		if depth == 0 {
			return self
				.table()
				.map(|snapshot| snapshot.unsynced_under(root))
				.unwrap_or_default();
		}
		match self.layer(depth) {
			Layer::Block(block) => {
				let mut out = self.unsynced_in(depth - 1, root);
				out.retain(|path| block.shows_below(path));
				out.extend(
					block
						.written_in(&Span::under(root))
						.filter(|row| row.state != BaselineState::Synced)
						.map(|row| row.rel_path.clone()),
				);
				out
			}
			Layer::Moved { from, to } => {
				let mut out = self.unsynced_in(depth - 1, root);
				out.retain(|path| !is_at_or_under(path, from));
				let moved = |rest: &str| format!("{to}{rest}");
				if root.is_empty() || at_or_under(to, root).is_some_and(|rest| !rest.is_empty()) {
					// `to` is strictly under `root`: every moved row is.
					if self
						.get_at(depth - 1, from)
						.is_some_and(|row| row.state != BaselineState::Synced)
					{
						out.push(to.clone());
					}
					out.extend(
						self.unsynced_in(depth - 1, from)
							.iter()
							.map(|path| moved(&path[from.len()..])),
					);
				} else if let Some(rest) = at_or_under(root, to) {
					// `root` is `to` or under it: the moved rows under it are those under the same
					// place under `from`.
					out.extend(
						self.unsynced_in(depth - 1, &format!("{from}{rest}"))
							.iter()
							.map(|path| moved(&path[from.len()..])),
					);
				}
				out
			}
		}
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
		if let Some(frozen) = self.frozen.as_deref() {
			return frozen_row(frozen, rel_path);
		}
		self.get_at(self.depth(), rel_path)
	}

	/// Whether a row sits at exactly `rel_path`, and whether it is carryable.
	fn carry_flag(&self, rel_path: &str) -> Option<bool> {
		if let Some(frozen) = self.frozen.as_deref() {
			return frozen_row(frozen, rel_path).map(|row| row.carryable());
		}
		self.carry_flag_at(self.depth(), rel_path)
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
			read: Read::Probe,
			dir: None,
			probe_first: true,
			want: 1,
			dir_want: PAGE,
			answered: 0,
		}
	}

	/// The rows at `paths` and at every ancestor of each — what the apply layer reads after the
	/// plan is made, and nothing else — as a baseline that holds only those.
	///
	/// This is what ENDS the pass's read transaction: the snapshot this baseline read through is
	/// dropped here, before the first action commits. Held through the apply, it would pin the
	/// store's WAL for as long as the apply runs — a suspended pass included — so that no
	/// checkpoint could reach past it and every commit meanwhile, this pass's and every other
	/// pair's, would grow the `-wal` file (see `rows/snapshot.rs`). The frozen rows still answer as
	/// the plan's rows did: the pass's own commits are in none of them.
	pub(super) fn freeze<'p>(self, paths: impl IntoIterator<Item = &'p str>) -> Self {
		// `""` too: the pair root holds no row, and a parent lookup of a top-level path asks it.
		let mut asked: BTreeSet<&str> = BTreeSet::from([""]);
		for path in paths {
			asked.extend(path.match_indices('/').map(|(at, _)| &path[..at]));
			asked.insert(path);
		}
		let frozen: HashMap<String, Option<BaselineEntry>> = {
			let mut cursor = self.cursor();
			asked
				.into_iter()
				.map(|path| (path.to_string(), cursor.get(path)))
				.collect()
		};
		if let Some(rows) = &self.rows
			&& Arc::strong_count(rows) > 1
		{
			// Not a wrong answer, so not a panic in a shipped build: the apply still reads the rows
			// frozen here. But the transaction stays open for as long as that clone lives.
			tracing::error!(
				"baseline: a clone of the pass's snapshot outlives its plan — its read transaction \
				 stays open through the apply and pins the WAL"
			);
			debug_assert!(false, "a clone of the pass's snapshot outlives its plan");
		}
		Self {
			rows: None,
			edits: None,
			frozen: Some(Arc::new(frozen)),
		}
	}

	/// Whether this is the apply layer's frozen baseline (see [`freeze`](Self::freeze)).
	pub(super) fn is_frozen(&self) -> bool {
		self.frozen.is_some()
	}

	/// Every row, parent before child, in path order (see the module doc).
	pub(super) fn iter(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.rows_in(self.depth(), Span::all(), false)
	}

	/// Every row, parent before child.
	pub(super) fn visit_rows(&self, mut visit: impl FnMut(&BaselineEntry)) {
		self.visit_in(Span::all(), false, &mut visit);
	}

	/// Every DIRECTORY row, parent before child: the rows a directory move can start from, read off
	/// an index that holds nothing else instead of out of a walk of every row.
	pub(super) fn visit_dir_rows(&self, mut visit: impl FnMut(&BaselineEntry)) {
		self.visit_in(Span::all(), true, &mut visit);
	}

	/// Every row's path, parent before child.
	#[cfg(feature = "bench-internals")]
	pub(super) fn paths(&self) -> impl Iterator<Item = String> + '_ {
		self.iter().map(|row| row.rel_path)
	}

	/// Every row's path, parent before child.
	pub(super) fn visit_row_paths(&self, mut visit: impl FnMut(&str)) {
		self.visit_in(Span::all(), false, &mut |row| visit(&row.rel_path));
	}

	/// Every row STRICTLY under `root` (`""` is every row), parent before child.
	pub(super) fn subtree(&self, root: &str) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.rows_in(self.depth(), Span::under(root), false)
	}

	/// Every path STRICTLY under `root` (`""` is every path), parent before child.
	pub(super) fn visit_subtree_paths(&self, root: &str, mut visit: impl FnMut(&str)) {
		self.visit_in(Span::under(root), false, &mut |row| visit(&row.rel_path));
	}

	/// The rows awaiting confirmation of a push of ours. Empty on a converged pair.
	pub(super) fn unconfirmed(&self) -> impl Iterator<Item = BaselineEntry> + '_ {
		self.unconfirmed_in(self.depth()).into_iter()
	}

	pub(super) fn any_unconfirmed(&self) -> bool {
		self.any_unconfirmed_in(self.depth())
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
		let block = self.edits_mut().top_block();
		rows.into_iter()
			.map(|row| {
				let agreed = row.content_hash;
				if let Some(written) = block.written.get_mut(&row.rel_path) {
					written.agreed_hash = agreed;
					return written.clone();
				}
				let row = BaselineEntry {
					agreed_hash: agreed,
					..row
				};
				block.agreed.insert(
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
	/// `true`.
	fn visit_folded(&self, rel_path: &str, visit: &mut dyn FnMut(&str) -> bool) -> bool {
		if rel_path.is_empty() {
			// `""` holds nothing: nothing is `""`, and nothing is strictly under it.
			return false;
		}
		self.visit_folded_in(self.depth(), &collision_key(rel_path), false, visit)
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
		let mut out = Vec::new();
		self.visit_folded_in(self.depth(), &collision_key(rel_path), true, &mut |path| {
			out.push(path.to_string());
			false
		});
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

	/// The path of every `.filenignore` row, in no order.
	pub(super) fn rule_file_rows(&self) -> impl Iterator<Item = String> + '_ {
		self.indexed_in(self.depth(), Indexed::RuleFiles)
			.into_iter()
	}

	/// The path of every row that stands in for neither side, in no order.
	pub(super) fn uncarryable_paths(&self) -> impl Iterator<Item = String> + '_ {
		self.indexed_in(self.depth(), Indexed::Uncarryable)
			.into_iter()
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
		self.unsynced_in(self.depth(), rel_path).is_empty()
	}

	/// Re-key the row at `from` and everything under it to sit under `to`, in THIS view only:
	/// every row at or under `from` rewritten under `to`, overwriting whatever row the view shows at
	/// each destination and leaving the rest of the destination's subtree where it is. Every source
	/// leaves before any destination is written, so a rename by case alone keeps what it moved.
	///
	/// A move onto a destination that holds nothing once the source has left it — every move
	/// `plan::fold_dir_moves` folds, since it asks `occupied(to)` first — is recorded as the
	/// MAPPING it is ([`Layer::Moved`]): two paths, whatever the subtree holds, and every read
	/// translates through it. Only a move onto rows it has to merge with writes the rows it moves.
	pub(super) fn move_subtree(&mut self, from: &str, to: &str) {
		if from.is_empty() || to.is_empty() {
			// Neither end can be the pair root: it holds no row, and moving the whole pair onto or
			// out of it is a request no writer makes.
			tracing::error!("baseline: refusing to move the pair root ({from:?} -> {to:?})");
			return;
		}
		if from == to || (!self.contains_key(from) && self.subtree(from).next().is_none()) {
			return;
		}
		let occupied = (self.contains_key(to) && !is_at_or_under(to, from))
			|| self
				.subtree(to)
				.any(|row| !is_at_or_under(&row.rel_path, from));
		if !occupied {
			self.edits_mut().layers.push(Layer::Moved {
				from: from.to_string(),
				to: to.to_string(),
			});
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
		let edits = self.edits_mut();
		for row in &moving {
			edits.uncount(row);
		}
		let block = edits.top_block();
		block.vacated.insert(from.to_string());
		block.unwrite_subtree(from);
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
			if let Some(old) = replaced.remove(&row.rel_path) {
				edits.uncount(&old);
			}
			edits.count(&row);
			edits.top_block().write(row);
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
		let block = self.edits_mut().top_block();
		if let Some(row) = block.written.get_mut(rel_path) {
			row.agreed_hash = agreed_hash;
			return Some(row.clone());
		}
		let row = BaselineEntry {
			agreed_hash,
			..current
		};
		block.agreed.insert(
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

	/// What this baseline holds in memory, in bytes: the handle a pass reads the table through,
	/// and the pass's own edits over it — nothing per row it did not edit. What SQLite caches for
	/// it is its reader's page cache, bounded by `baseline::READER_CACHE_KIB` and measured as a
	/// resident set, not here.
	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_bytes(&self) -> usize {
		self.resident_terms().total()
	}

	#[cfg(feature = "bench-internals")]
	pub(super) fn resident_terms(&self) -> ResidentTerms {
		ResidentTerms {
			handle: self.rows.as_ref().map_or(0, |_| size_of::<Snapshot>()),
			edits: self.edits.as_deref().map_or(0, Edits::bytes)
				+ self.frozen.as_deref().map_or(0, |frozen| {
					frozen.capacity() * size_of::<(String, Option<BaselineEntry>)>()
						+ frozen
							.iter()
							.map(|(path, row)| {
								path.capacity()
									+ row.as_ref().map_or(0, |row| row.rel_path.capacity())
							})
							.sum::<usize>()
				}),
		}
	}
}

/// What [`Baseline::resident_bytes`] is made of, one field per structure it counts.
#[cfg(feature = "bench-internals")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResidentTerms {
	/// The snapshot handle: a connection, its pair and its counts. Not the pages it keeps to answer
	/// point and folded questions out of — up to [`MAX_PAGE`] rows each, dropped with the pass.
	pub(super) handle: usize,
	/// The pass's edits over the table — what it moved and confirmed — or the rows frozen for its
	/// apply. Zero on a pair between passes.
	pub(super) edits: usize,
}

#[cfg(feature = "bench-internals")]
impl ResidentTerms {
	/// Every term under its own name, in the order a table should print them.
	pub(super) fn named(&self) -> [(&'static str, usize); 2] {
		[("handle", self.handle), ("edits", self.edits)]
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
		let mut dirs = Vec::new();
		view.visit_dir_rows(|row| dirs.push(row.clone()));
		assert_eq!(
			dirs,
			rows.iter()
				.filter(|row| row.kind == NodeKind::Dir)
				.cloned()
				.collect::<Vec<_>>(),
			"{context}: visit_dir_rows"
		);
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

	/// How many layers the view's edits have, and whether the top one is a mapped move.
	fn layers(view: &Baseline) -> (usize, bool) {
		view.edits.as_deref().map_or((0, false), |edits| {
			(
				edits.layers.len(),
				matches!(edits.layers.last(), Some(Layer::Moved { .. })),
			)
		})
	}

	/// Every edit sequence a pass can write, and many it cannot, answered alike by the view and by
	/// the tree with the same edits written into it — after every single edit. Short sequences
	/// from many seeds, then long ones that stack mapped moves many layers deep: moves through each
	/// other's destinations, back onto their sources, into their own subtrees, with rows written and
	/// markers advanced between them.
	#[test]
	fn every_edit_sequence_answers_as_a_copy_with_the_same_edits_does() {
		let rows = corpus();
		let everything = probes(
			rows.iter()
				.map(|row| row.rel_path.clone())
				.chain(["new", "new/inner", "archive/new", "DOCS"].map(String::from)),
		);
		let (mut mapped, mut written, mut confirms, mut deepest) = (0, 0, 0, 0);
		for (seeds, steps) in [(0..60, 8), (1000..1020, 24)] {
			for seed in seeds {
				let mut rng = StdRng::seed_from_u64(seed);
				let mut view = Baseline::from_rows(rows.clone());
				let mut oracle = Tree::from_rows(rows.clone());
				let mut probes = everything.clone();
				for step in 0..steps {
					let (before, _) = layers(&view);
					let (what, moved) =
						random_edit(&mut rng, step, &mut view, &mut oracle, &probes);
					let (after, top_mapped) = layers(&view);
					if !moved {
						confirms += 1;
					} else if after > before && top_mapped {
						mapped += 1;
					} else if after > 0 && !top_mapped {
						written += 1;
					}
					deepest = deepest.max(after);
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
		}
		// Both kinds of move, and views many layers deep, or the comparison above proved less than it
		// says.
		assert!(
			mapped > 150 && written > 100 && confirms > 100 && deepest >= 8,
			"{mapped} mapped move(s), {written} written, {confirms} confirmation(s), {deepest} \
			 layer(s) at most"
		);
	}

	/// A folded move of a directory holding many rows is a MAPPING: its edits hold two paths,
	/// whatever the subtree holds, and nothing of the rows it carries — while every question asked
	/// of the moved subtree, at its new place and its old one and under any spelling, answers as a
	/// copy with the move written into it does.
	#[test]
	fn a_folded_move_of_a_large_directory_holds_none_of_its_rows() {
		let mut rows = vec![
			dir("big"),
			dir("big/inner"),
			dir("Other"),
			file("Other/x", 9),
		];
		for n in 0..(PAGE * 8) {
			rows.push(file(&format!("big/{n:05}.txt"), 1));
			rows.push(file(&format!("big/inner/{n:05}.txt"), 2));
		}
		let mut view = Baseline::from_rows(rows.clone());
		let mut oracle = Tree::from_rows(rows);
		view.move_subtree("big", "Renamed/Big");
		oracle.move_subtree("big", "Renamed/Big");
		let edits = view.edits.as_deref().expect("the move is an edit");
		assert!(
			edits
				.layers
				.iter()
				.all(|layer| matches!(layer, Layer::Moved { .. })),
			"the move wrote rows: {} layer(s), {} written",
			edits.layers.len(),
			edits
				.layers
				.iter()
				.map(|layer| match layer {
					Layer::Block(block) => block.written.len(),
					Layer::Moved { .. } => 0,
				})
				.sum::<usize>()
		);
		// The pass then confirms beside it and moves a directory inside the moved one, the way a
		// fold of nested moves stacks them.
		view.move_subtree("Renamed/Big/inner", "Renamed/inner2");
		oracle.move_subtree("Renamed/Big/inner", "Renamed/inner2");
		let probes = probes(
			[
				"big/00001.txt",
				"big/inner/00002.txt",
				"Renamed/Big/00003.txt",
				"Renamed/inner2/00004.txt",
				"RENAMED/BIG/INNER",
				"Other/x",
			]
			.map(String::from),
		);
		assert_alike("big -> Renamed/Big, inner out", &view, &oracle, &probes);
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
