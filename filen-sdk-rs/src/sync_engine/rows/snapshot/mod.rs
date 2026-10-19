//! One pass's rows as the store holds them: a read transaction on a connection of the pass's own.
//!
//! The store hands a pass a [`Snapshot`] instead of a copy of its rows. It is one of the store's
//! reader connections (see [`Readers`]) with a read transaction open on it, and every question the
//! pass asks becomes an indexed statement against the table as that transaction sees it.
//!
//! # One consistent state for the whole plan
//!
//! The file is in WAL mode, so the transaction pins the snapshot it first read — the counts, read
//! by [`Snapshot::begin`] itself — for as long as it stays open, whatever the store's own
//! connection commits meanwhile. That is what a pass used to get from the immutable `Arc` of the
//! resident tree, and it matters for more than other writers: a pass WRITES while it plans (the
//! confirmations it persists, the conflicts it records), and its reads have to keep answering from
//! the rows the plan is being made from rather than from the ones the pass has just committed. The
//! pass gate and the reading lock serialize a pair's passes and keep `resolve_conflict` out; this
//! transaction is what keeps the plan consistent with itself.
//!
//! It ends when the plan does. An open reader is a mark no checkpoint can pass, so held through the
//! apply it would grow the `-wal` file by every commit the apply made — one per action — and by every
//! other pair's, for as long as the apply ran, a suspended one included. So each of the pass's
//! actions takes the rows its apply reads out of it (`apply::PlannedEntry::stage`) — the apply reads
//! nothing else, and the plan's rows are what it has to read — and the last clone of the snapshot
//! is dropped there ([`Baseline::close`](super::Baseline::close)), handing the connection back to
//! the store for the next pass.
//!
//! # A failed read is a panic, never an absence
//!
//! Every read here answers "no row" as a real answer: the pass then reads the path as untracked,
//! and a path that reads as untracked on one side and gone on the other is a deletion. So a read
//! that FAILS — an I/O error, a corrupt page — must not come back as that answer. It panics with
//! what failed instead: the pass dies and plans nothing, which is the only safe thing a pass can
//! do without its rows.
//!
//! # Re-entrancy
//!
//! A caller's closure can ask this snapshot something while it is being handed rows — the folded
//! walk's predicate asks whether a row is carryable. The connection sits behind a mutex, which is
//! not re-entrant, so no statement is ever stepped while a caller's code runs: every enumeration
//! reads a PAGE of rows, lets the connection go, and only then hands them out.

use std::{
	num::NonZeroUsize,
	sync::{Arc, LazyLock, Mutex, PoisonError},
};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::{
	baseline::{BaselineEntry, BaselineStore, ENTRY_COLUMNS, NodeKind, PairId, Readers},
	page::{self, AheadPage, ReadAhead},
};

/// How many statements the pass's reads ran, and how many rows they handed back, in this process —
/// the benchmark's count of what a pass asked the store (see `bench::MemAnswer`).
#[cfg(feature = "bench-internals")]
static STATEMENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(feature = "bench-internals")]
static ROWS_READ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(statements, rows)` read so far (see [`STATEMENTS`]).
#[cfg(feature = "bench-internals")]
pub(in super::super) fn reads() -> (u64, u64) {
	use std::sync::atomic::Ordering;
	(
		STATEMENTS.load(Ordering::Relaxed),
		ROWS_READ.load(Ordering::Relaxed),
	)
}

/// Zero the counters, so what follows is measured on its own.
#[cfg(feature = "bench-internals")]
pub(in super::super) fn reset_reads() {
	use std::sync::atomic::Ordering;
	STATEMENTS.store(0, Ordering::Relaxed);
	ROWS_READ.store(0, Ordering::Relaxed);
}

/// What one statement handed back, counted in rows for [`ROWS_READ`].
trait Answer {
	// Counted only where there is a counter to count into (tests and `bench-internals`). Allowed
	// rather than gated: a gate here would have to be repeated on every implementation below.
	#[cfg_attr(not(any(test, feature = "bench-internals")), allow(dead_code))]
	fn rows(&self) -> usize;
}

impl<T> Answer for Vec<T> {
	fn rows(&self) -> usize {
		self.len()
	}
}

impl<T> Answer for Option<T> {
	fn rows(&self) -> usize {
		usize::from(self.is_some())
	}
}

impl Answer for bool {
	fn rows(&self) -> usize {
		usize::from(*self)
	}
}

/// A `COUNT`: the one row it hands back, whatever it counted.
impl Answer for usize {
	fn rows(&self) -> usize {
		1
	}
}

/// How many rows one enumerating statement reads before it lets the connection go.
///
/// Large enough that a whole-pair walk is a few thousand statements rather than a million, small
/// enough that what a page holds is noise beside what the walk itself builds.
pub(super) const PAGE: usize = 256;

/// The most rows a page read for a caller that keeps to it holds, however well it keeps to it: the
/// cursor's pages and the point questions' read-ahead both double up to this.
pub(super) const MAX_PAGE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 is not zero");

/// The whole-set counts a pass asks for, as `baseline_counts` recorded them when the snapshot
/// began.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Counts {
	pub(super) rows: usize,
	pub(super) uncarryable: usize,
	pub(super) remote_rows: usize,
}

/// One pair's rows as one read transaction sees them (see the module doc).
pub(in super::super) struct Snapshot {
	/// `None` only once dropped.
	conn: Mutex<Option<Connection>>,
	/// Where the connection goes back to. `None` for a snapshot over rows a test wrote into a
	/// connection of its own, which nothing else writes to and which needs no transaction.
	readers: Option<Arc<Readers>>,
	pair: PairId,
	pub(super) counts: Counts,
	/// The last page any enumeration or cursor read, kept so that a point question about a path it
	/// spans is answered out of it (see [`Page`]).
	last_page: Mutex<Option<Arc<Page>>>,
	/// The last page the point questions read for themselves (see [`Snapshot::point`]). Apart from
	/// `last_page`, so that a question about some other path, asked while an enumeration is being
	/// handed out, does not take away the page the questions about the enumerated rows answer from.
	ahead: Mutex<ReadAhead<Page>>,
	/// The same for the questions about one folded path (see [`Snapshot::folded_at`]).
	folded_ahead: Mutex<ReadAhead<FoldedPage>>,
	/// `(statements, rows)` read through THIS snapshot, where the benchmark's counters count every
	/// one in the process: what a test holds the reads of one pass over its rows to.
	#[cfg(test)]
	reads: Mutex<(usize, usize)>,
}

/// A contiguous stretch of the table as the snapshot reads it (see [`page::Page`]): authoritative
/// within it because the transaction the page was read in has not moved.
///
/// That is what lets a point question about a path the pass is enumerating past cost a binary
/// search instead of a statement. A whole pass visits every row and, for each, asks the carried
/// side whether it holds that path — which asks here, about the row just handed out.
pub(super) type Page = page::Page<BaselineEntry>;

/// The path a row is filed at, which every page of rows is read in the order of.
fn row_path(row: &BaselineEntry) -> &str {
	&row.rel_path
}

/// A stretch of `baseline_folded` as the folded questions read it: every `(folded_path, rel_path)`
/// from the folded path `from` on, in the index's order, up to the last one read — or to the end of
/// the pair when the read ran out before its limit. AUTHORITATIVE for every folded path it spans, as
/// [`Page`] is for paths, but for the last one of a read that stopped at its limit: two rows can
/// share a folded path, so more of that one may follow.
#[derive(Debug)]
struct FoldedPage {
	from: String,
	complete: bool,
	rows: Vec<(String, String)>,
}

impl FoldedPage {
	/// Whether this page answers for `folded`.
	fn spans(&self, folded: &str) -> bool {
		folded >= self.from.as_str()
			&& (self.complete
				|| self
					.rows
					.last()
					.is_some_and(|(last, _)| folded < last.as_str()))
	}

	/// `Some(the path of every row folded to `folded`)` where this page answers for it.
	fn answer(&self, folded: &str) -> Option<Vec<String>> {
		self.spans(folded).then(|| {
			let at = self.rows.partition_point(|(key, _)| key.as_str() < folded);
			self.rows[at..]
				.iter()
				.take_while(|(key, _)| key == folded)
				.map(|(_, path)| path.clone())
				.collect()
		})
	}
}

impl AheadPage for FoldedPage {
	fn len(&self) -> usize {
		self.rows.len()
	}

	/// Whether every folded path this page can answer for sorts before `folded`: the question lies
	/// past the page, where a walk in order goes on.
	fn passed_by(&self, folded: &str) -> bool {
		self.rows
			.last()
			.is_some_and(|(last, _)| last.as_str() <= folded)
	}
}

impl std::fmt::Debug for Snapshot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Snapshot")
			.field("pair", &self.pair)
			.field("counts", &self.counts)
			.finish_non_exhaustive()
	}
}

impl Drop for Snapshot {
	fn drop(&mut self) {
		let Some(conn) = self
			.conn
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner)
			.take()
		else {
			return;
		};
		let Some(readers) = &self.readers else {
			return;
		};
		// Ending the read transaction is what lets a checkpoint reach past it. A connection that
		// cannot end it is not handed to the next pass, which would begin inside it.
		if conn.execute_batch("COMMIT").is_ok() && conn.is_autocommit() {
			readers.give(conn);
		}
	}
}

/// The row `pair_id = ?1 AND rel_path` range statement, for each shape of bound.
///
/// No `LIMIT`: a page is read by stepping the statement and stopping. A BOUND limit (`LIMIT ?`)
/// marks the plan as depending on the value bound to it — this build of SQLite has STAT4 — so
/// every page with a different limit or bound would re-prepare the statement inside its first
/// step, which measured as most of what a whole pass's scan spent reading its rows. The plan is an
/// index walk in `rel_path` order either way, so stopping early reads no row past the page.
fn range_sql(inclusive: bool, bounded: bool) -> String {
	format!(
		"SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 AND rel_path {} ?2{} \
		 ORDER BY rel_path",
		if inclusive { ">=" } else { ">" },
		if bounded { " AND rel_path < ?3" } else { "" },
	)
}

// The statements that read a secondary index name it (`INDEXED BY`). This DB is never `ANALYZE`d,
// so the planner has no statistics, and with none it will as soon seek `baseline_remote_uuid` on
// `pair_id` alone — a walk of the whole pair — as the partial index the question was written for.
// Named, the index is used or the statement fails to PREPARE, which is loud rather than slow.
/// Stepped and stopped rather than `LIMIT`ed, for the reason [`range_sql`] gives.
const FOLDED_FROM: &str = "SELECT folded_path, rel_path FROM baseline INDEXED BY baseline_folded
	 WHERE pair_id = ?1 AND folded_path >= ?2 ORDER BY folded_path, rel_path";
/// Keyset-paged on `(folded_path, rel_path)`, which is what `baseline_folded` orders by: two rows
/// can share a folded path, so the folded path alone is not a position to resume from. Stepped and
/// stopped rather than `LIMIT`ed, for the reason [`range_sql`] gives.
const FOLDED_UNDER: &str = "SELECT folded_path, rel_path FROM baseline INDEXED BY baseline_folded
	 WHERE pair_id = ?1 AND (folded_path, rel_path) > (?2, ?3) AND folded_path < ?4
	 ORDER BY folded_path, rel_path";
const UNCARRYABLE: &str = "SELECT rel_path FROM baseline INDEXED BY baseline_uncarryable
	 WHERE pair_id = ?1 AND carryable = 0";
const RULE_FILES: &str = "SELECT rel_path FROM baseline INDEXED BY baseline_rule_files
	 WHERE pair_id = ?1 AND rule_file = 1";
const UNCONFIRMED_PATHS: &str = "SELECT rel_path FROM baseline INDEXED BY baseline_unconfirmed
	 WHERE pair_id = ?1 AND unconfirmed = 1";
const ANY_UNCONFIRMED: &str = "SELECT 1 FROM baseline INDEXED BY baseline_unconfirmed
	 WHERE pair_id = ?1 AND unconfirmed = 1 LIMIT 1";
const COUNTS: &str =
	"SELECT rows, uncarryable, remote_rows FROM baseline_counts WHERE pair_id = ?1";

/// Each statement text built once: a lookup is a `prepare_cached` keyed on the text, and building
/// the text per call would be an allocation per row a pass asks for.
static UNCONFIRMED: LazyLock<String> = LazyLock::new(unconfirmed_sql);
static RANGE: LazyLock<[[String; 2]; 2]> = LazyLock::new(|| {
	[
		[range_sql(false, false), range_sql(false, true)],
		[range_sql(true, false), range_sql(true, true)],
	]
});
static DIRS: LazyLock<[[String; 2]; 2]> = LazyLock::new(|| {
	[
		[dirs_sql(false, false), dirs_sql(false, true)],
		[dirs_sql(true, false), dirs_sql(true, true)],
	]
});
static UNSYNCED: LazyLock<[String; 2]> =
	LazyLock::new(|| [unsynced_sql(false), unsynced_sql(true)]);
static COUNT_UNDER: LazyLock<[String; 2]> =
	LazyLock::new(|| [count_under_sql(false), count_under_sql(true)]);

/// How many rows lie after a path — strictly below a bound when `bounded` — off the primary key's
/// range: SQLite counts the range's cells and hands back one number, where reading the rows out of
/// it decodes and copies every one.
fn count_under_sql(bounded: bool) -> String {
	format!(
		"SELECT COUNT(*) FROM baseline WHERE pair_id = ?1 AND rel_path > ?2{}",
		if bounded { " AND rel_path < ?3" } else { "" },
	)
}

/// The directory rows from a path on — strictly below a bound when `bounded` — in path order, off
/// the partial index that holds nothing else.
fn dirs_sql(inclusive: bool, bounded: bool) -> String {
	format!(
		"SELECT {ENTRY_COLUMNS} FROM baseline INDEXED BY baseline_dirs
		 WHERE pair_id = ?1 AND kind = {} AND rel_path {} ?2{} ORDER BY rel_path",
		NodeKind::Dir.as_i64(),
		if inclusive { ">=" } else { ">" },
		if bounded { " AND rel_path < ?3" } else { "" },
	)
}

/// The unconfirmed rows after a path, in path order, off the partial index that holds nothing else.
/// Stepped and stopped rather than `LIMIT`ed, for the reason [`range_sql`] gives.
fn unconfirmed_sql() -> String {
	format!(
		"SELECT {ENTRY_COLUMNS} FROM baseline INDEXED BY baseline_unconfirmed
		 WHERE pair_id = ?1 AND unconfirmed = 1 AND rel_path > ?2 ORDER BY rel_path"
	)
}

/// The unsynced rows strictly under a directory: one seek per unsynced state, bounded to the
/// directory's range, on `baseline_state (pair_id, state, rel_path)`. The states are spelled out
/// rather than written `state <> 0`, which is not a term an index can seek on.
fn unsynced_sql(bounded: bool) -> String {
	let states = super::super::baseline::UNSYNCED_STATES
		.iter()
		.map(ToString::to_string)
		.collect::<Vec<_>>()
		.join(", ");
	format!(
		"SELECT rel_path FROM baseline INDEXED BY baseline_state
		 WHERE pair_id = ?1 AND state IN ({states}){}",
		if bounded {
			" AND rel_path > ?2 AND rel_path < ?3"
		} else {
			""
		}
	)
}

/// A count read back from SQLite, which stores it signed. Checked in EVERY build: a negative count
/// is a trigger that fired wrong, and read as a `usize` it would wrap to a pair of absurd size —
/// the kind of silently wrong figure a mass-delete guard divides by.
fn count(value: i64, what: &str) -> usize {
	usize::try_from(value)
		.unwrap_or_else(|_| panic!("baseline_counts holds a negative {what}: {value}"))
}

impl Snapshot {
	/// Open the pass's view on `conn`: a read transaction when the connection is one of the store's,
	/// and the counts read INSIDE it, which is the read that pins the snapshot.
	pub(in super::super) fn begin(
		conn: Connection,
		pair: PairId,
		readers: Option<Arc<Readers>>,
	) -> rusqlite::Result<Self> {
		if readers.is_some() {
			conn.execute_batch("BEGIN")?;
		}
		let counts = read_counts(&conn, pair)?;
		Ok(Self {
			conn: Mutex::new(Some(conn)),
			readers,
			pair,
			counts,
			last_page: Mutex::new(None),
			ahead: Mutex::new(ReadAhead::default()),
			folded_ahead: Mutex::new(ReadAhead::default()),
			#[cfg(test)]
			reads: Mutex::new((0, 0)),
		})
	}

	fn with<T: Answer>(
		&self,
		what: &str,
		read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
	) -> T {
		#[cfg(feature = "bench-internals")]
		STATEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
		#[cfg(test)]
		let read = |conn: &Connection| {
			read(conn).inspect(|answer| {
				let mut reads = self.reads.lock().unwrap_or_else(PoisonError::into_inner);
				reads.0 += 1;
				reads.1 += answer.rows();
			})
		};
		let guard = self.conn.lock().unwrap_or_else(PoisonError::into_inner);
		let conn = guard
			.as_ref()
			.expect("a snapshot holds its connection until it is dropped");
		let answer = read(conn).unwrap_or_else(|error| {
			panic!(
				"reading pair {}'s baseline rows ({what}) failed: {error} — refusing to read a \
				 failed read as an absent row",
				self.pair
			)
		});
		#[cfg(feature = "bench-internals")]
		ROWS_READ.fetch_add(answer.rows() as u64, std::sync::atomic::Ordering::Relaxed);
		answer
	}

	/// What the last page read says about `rel_path`, where it can say.
	fn remembered<T>(
		&self,
		rel_path: &str,
		answer: impl FnOnce(Option<&BaselineEntry>) -> T,
	) -> Option<T> {
		let last = self
			.last_page
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()?;
		last.answer(rel_path).map(answer)
	}

	/// The row at exactly `rel_path`.
	pub(super) fn row(&self, rel_path: &str) -> Option<BaselineEntry> {
		self.point(rel_path, |row| row.cloned())
	}

	/// Whether a row sits at exactly `rel_path`, and if so whether it is carryable.
	pub(super) fn carryable_at(&self, rel_path: &str) -> Option<bool> {
		self.point(rel_path, |row| row.map(BaselineEntry::carryable))
	}

	/// What the table holds at exactly `rel_path`, handed to `answer`: out of the last page an
	/// enumeration read where it spans the path, else out of the last page these questions read,
	/// else out of a new page read FROM the path.
	///
	/// The new page is sized to how the last one was used (see [`ReadAhead`]): a pass asking about
	/// its decided set, which it holds sorted, walks in path order. So a walk over a moved
	/// directory's fifty thousand paths is a few hundred statements rather than fifty thousand, and
	/// the pages it keeps are never larger than [`MAX_PAGE`] rows.
	fn point<T>(&self, rel_path: &str, answer: impl Fn(Option<&BaselineEntry>) -> T) -> T {
		if let Some(found) = self.remembered(rel_path, &answer) {
			return found;
		}
		self.ahead
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.ask(
				rel_path,
				MAX_PAGE,
				|page| page.answer(rel_path).map(&answer),
				|want| self.read_page(rel_path, true, None, want),
			)
	}

	/// Up to `limit` rows from `from` on (`inclusive` or not), strictly below `to` when there is
	/// one, in path order — as a [`Page`], which the snapshot also keeps for the point questions
	/// that follow.
	pub(super) fn page(
		&self,
		from: &str,
		inclusive: bool,
		to: Option<&str>,
		limit: usize,
	) -> Arc<Page> {
		let page = Arc::new(self.read_page(from, inclusive, to, limit));
		*self
			.last_page
			.lock()
			.unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&page));
		page
	}

	/// [`page`](Self::page), without keeping it.
	fn read_page(&self, from: &str, inclusive: bool, to: Option<&str>, limit: usize) -> Page {
		let rows = self.rows_from(from, inclusive, to, limit);
		Page::read(from, inclusive, to, limit, rows, row_path)
	}

	/// Up to `limit` DIRECTORY rows from `from` on, strictly below `to` when there is one, in path
	/// order. Not a contiguous stretch of the table — the files between them are skipped — so it
	/// answers no point question and is not kept as the last page.
	pub(super) fn dir_page(
		&self,
		from: &str,
		inclusive: bool,
		to: Option<&str>,
		limit: usize,
	) -> Arc<Page> {
		let rows: Vec<BaselineEntry> = self.with("a range of directory rows", |conn| {
			let mut statement =
				conn.prepare_cached(&DIRS[usize::from(inclusive)][usize::from(to.is_some())])?;
			let rows = match to {
				Some(to) => statement
					.query_map(params![self.pair, from, to], BaselineStore::row_to_entry)?,
				None => {
					statement.query_map(params![self.pair, from], BaselineStore::row_to_entry)?
				}
			};
			rows.take(limit).collect()
		});
		Arc::new(Page::read(from, inclusive, to, limit, rows, row_path))
	}

	fn rows_from(
		&self,
		from: &str,
		inclusive: bool,
		to: Option<&str>,
		limit: usize,
	) -> Vec<BaselineEntry> {
		self.with("a range of rows", |conn| {
			let mut statement =
				conn.prepare_cached(&RANGE[usize::from(inclusive)][usize::from(to.is_some())])?;
			let rows = match to {
				Some(to) => statement
					.query_map(params![self.pair, from, to], BaselineStore::row_to_entry)?,
				None => {
					statement.query_map(params![self.pair, from], BaselineStore::row_to_entry)?
				}
			};
			rows.take(limit).collect()
		})
	}

	/// The path of every row whose folded path is exactly `folded`: out of the page these questions
	/// read last where it spans `folded`, else out of a new one read from `folded` on and sized as a
	/// point question's is ([`point`](Self::point)) — so a pass folding its decided set in order reads
	/// the folded index a page at a time rather than a statement a path. A question counts once
	/// however many rows fold to its path: a walk visiting every folded path still covers half of
	/// each page while no more than two rows fold to each path on average.
	pub(super) fn folded_at(&self, folded: &str) -> Vec<String> {
		self.folded_ahead
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.ask(
				folded,
				MAX_PAGE,
				|page| page.answer(folded),
				|want| {
					// Never one row: a page that stops at its limit cannot answer for its last folded
					// path, and a one-row page read from a folded path some row holds stops on that
					// very path — so it would take a second statement to answer what the one after
					// it can.
					let mut want = want.max(2);
					loop {
						let rows: Vec<(String, String)> =
							self.with("the rows from a folded path", |conn| {
								conn.prepare_cached(FOLDED_FROM)?
									.query_map(params![self.pair, folded], |row| {
										Ok((row.get(0)?, row.get(1)?))
									})?
									.take(want)
									.collect()
							});
						let page = FoldedPage {
							from: folded.to_string(),
							complete: rows.len() < want,
							rows,
						};
						if page.spans(folded) {
							return page;
						}
						// Every row it read is folded to `folded` itself, and more may follow: read
						// past them.
						want *= 2;
					}
				},
			)
	}

	/// Up to `limit` rows STRICTLY under the folded path `folded`, as `(folded_path, rel_path)`,
	/// after `after` (the last pair a previous page ended on). The bounds are `folded/` and
	/// `folded0`, never `folded` itself: `/` is 0x2F and `0` is 0x30, so the range is exactly the
	/// paths continuing `folded` with a separator, and `docsx` is not under `docs`.
	pub(super) fn folded_under(
		&self,
		folded: &str,
		after: Option<(&str, &str)>,
		limit: usize,
	) -> Vec<(String, String)> {
		let (from, to) = (format!("{folded}/"), format!("{folded}0"));
		let (after_folded, after_path) = after.unwrap_or((&from, ""));
		self.with("the rows under a folded path", |conn| {
			conn.prepare_cached(FOLDED_UNDER)?
				.query_map(params![self.pair, after_folded, after_path, to], |row| {
					Ok((row.get(0)?, row.get(1)?))
				})?
				.take(limit)
				.collect()
		})
	}

	fn paths(&self, what: &str, sql: &str) -> Vec<String> {
		self.with(what, |conn| {
			conn.prepare_cached(sql)?
				.query_map(params![self.pair], |row| row.get(0))?
				.collect()
		})
	}

	pub(super) fn uncarryable_paths(&self) -> Vec<String> {
		self.paths("the uncarryable rows", UNCARRYABLE)
	}

	pub(super) fn rule_file_paths(&self) -> Vec<String> {
		self.paths("the rule-file rows", RULE_FILES)
	}

	/// Up to `limit` of the unconfirmed rows strictly after `after` (`""`: from the first), in path
	/// order: the set is a page at a time, because on the pass after a whole-tree push it is the tree.
	pub(super) fn unconfirmed_after(&self, after: &str, limit: usize) -> Vec<BaselineEntry> {
		self.with("a page of the unconfirmed rows", |conn| {
			conn.prepare_cached(&UNCONFIRMED)?
				.query_map(params![self.pair, after], BaselineStore::row_to_entry)?
				.take(limit)
				.collect()
		})
	}

	pub(super) fn unconfirmed_paths(&self) -> Vec<String> {
		self.paths("the unconfirmed rows' paths", UNCONFIRMED_PATHS)
	}

	pub(super) fn any_unconfirmed(&self) -> bool {
		self.with("whether any row is unconfirmed", |conn| {
			conn.prepare_cached(ANY_UNCONFIRMED)?
				.exists(params![self.pair])
		})
	}

	/// The path of every row strictly under `root` (`""`: every row) that is not `Synced`.
	pub(super) fn unsynced_under(&self, root: &str) -> Vec<String> {
		self.with("the unsynced rows under a directory", |conn| {
			if root.is_empty() {
				conn.prepare_cached(&UNSYNCED[0])?
					.query_map(params![self.pair], |row| row.get(0))?
					.collect()
			} else {
				conn.prepare_cached(&UNSYNCED[1])?
					.query_map(
						params![self.pair, format!("{root}/"), format!("{root}0")],
						|row| row.get(0),
					)?
					.collect()
			}
		})
	}

	/// How many rows lie strictly under `root` (`""`: every row), counted rather than read.
	pub(super) fn count_under(&self, root: &str) -> usize {
		self.with("the rows under a directory, counted", |conn| {
			if root.is_empty() {
				conn.prepare_cached(&COUNT_UNDER[0])?
					.query_row(params![self.pair, root], |row| row.get(0))
			} else {
				conn.prepare_cached(&COUNT_UNDER[1])?.query_row(
					params![self.pair, format!("{root}/"), format!("{root}0")],
					|row| row.get(0),
				)
			}
		})
	}

	/// What [`reads`](Self::reads) has counted so far.
	#[cfg(test)]
	pub(super) fn reads_for_test(&self) -> (usize, usize) {
		*self.reads.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Write `entry` into the rows a test built this snapshot over, and re-read the counts.
	#[cfg(test)]
	pub(super) fn upsert_for_test(&mut self, entry: &BaselineEntry) {
		assert!(
			self.readers.is_none(),
			"only a test's own rows are written through a snapshot"
		);
		let conn = self
			.conn
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner)
			.as_ref()
			.expect("a snapshot holds its connection until it is dropped");
		super::super::baseline::upsert_row(conn, self.pair, entry).expect("writing a test row");
		self.counts = read_counts(conn, self.pair).expect("reading a test pair's counts");
		// The one write a snapshot ever sees: a page read before it no longer describes the table.
		*self
			.last_page
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner) = None;
		*self.ahead.get_mut().unwrap_or_else(PoisonError::into_inner) = ReadAhead::default();
		*self
			.folded_ahead
			.get_mut()
			.unwrap_or_else(PoisonError::into_inner) = ReadAhead::default();
	}
}

fn read_counts(conn: &Connection, pair: PairId) -> rusqlite::Result<Counts> {
	Ok(conn
		.prepare_cached(COUNTS)?
		.query_row(params![pair], |row| {
			Ok(Counts {
				rows: count(row.get(0)?, "row count"),
				uncarryable: count(row.get(1)?, "uncarryable count"),
				remote_rows: count(row.get(2)?, "remote row count"),
			})
		})
		.optional()?
		// A pair no row was ever written for has no counts row: the triggers make one on the
		// first insert.
		.unwrap_or_default())
}

/// `rows` in a connection of their own, as the store would hold them: the tests' way of writing a
/// baseline down.
#[cfg(test)]
pub(super) fn standalone(rows: impl IntoIterator<Item = BaselineEntry>) -> Snapshot {
	const PAIR: PairId = 1;
	let conn = super::super::baseline::test_rows_connection(PAIR);
	for entry in rows {
		super::super::baseline::upsert_row(&conn, PAIR, &entry).expect("writing a test row");
	}
	Snapshot::begin(conn, PAIR, None).expect("reading a test pair's counts")
}

#[cfg(test)]
mod tests;
