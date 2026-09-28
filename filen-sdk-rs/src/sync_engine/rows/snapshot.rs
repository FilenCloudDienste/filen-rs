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
//! other pair's, for as long as the apply ran, a suspended one included. So the pass freezes the rows
//! at the paths its actions name out of it ([`Baseline::freeze`](super::Baseline::freeze)) — the
//! apply reads nothing else, and the plan's rows are what it has to read — and the last clone of the
//! snapshot is dropped there, handing the connection back to the store for the next pass.
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

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use rusqlite::{Connection, OptionalExtension, params};

use super::super::baseline::{
	BaselineEntry, BaselineStore, ENTRY_COLUMNS, NodeKind, PairId, Readers,
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
	// Counted only where there is a counter to count into.
	#[cfg_attr(not(feature = "bench-internals"), allow(dead_code))]
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

/// How many rows one enumerating statement reads before it lets the connection go.
///
/// Large enough that a whole-pair walk is a few thousand statements rather than a million, small
/// enough that what a page holds is noise beside what the walk itself builds.
pub(super) const PAGE: usize = 256;

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
}

/// A contiguous stretch of the table as the snapshot reads it: every row from `from` on, up to the
/// last row read — or, when the read ran out of rows before its limit, up to the end of the range
/// it asked for. Within that stretch the page is AUTHORITATIVE: a path it holds no row for has no
/// row in the table either, because the transaction the page was read in has not moved.
///
/// That is what lets a point question about a path the pass is enumerating past cost a binary
/// search instead of a statement. A whole pass visits every row and, for each, asks the carried
/// side whether it holds that path — which asks here, about the row just handed out.
#[derive(Debug)]
pub(super) struct Page {
	from: String,
	inclusive: bool,
	to: Option<String>,
	/// Whether the read reached the end of its range rather than its limit.
	complete: bool,
	pub(super) rows: Vec<BaselineEntry>,
}

impl Page {
	/// `Some(row or none)` where this page answers for `rel_path`; `None` where it cannot say.
	pub(super) fn answer(&self, rel_path: &str) -> Option<Option<&BaselineEntry>> {
		let past_from = if self.inclusive {
			rel_path >= self.from.as_str()
		} else {
			rel_path > self.from.as_str()
		};
		let before_end = match self.rows.last() {
			Some(last) if rel_path <= last.rel_path.as_str() => true,
			_ => self.complete && self.to.as_deref().is_none_or(|to| rel_path < to),
		};
		(past_from && before_end).then(|| {
			self.rows
				.binary_search_by(|row| row.rel_path.as_str().cmp(rel_path))
				.ok()
				.map(|at| &self.rows[at])
		})
	}

	/// Whether the read stopped at its limit, so the range goes on past the last row.
	pub(super) fn is_full(&self) -> bool {
		!self.complete
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
const ROW: &str = "SELECT carryable FROM baseline WHERE pair_id = ?1 AND rel_path = ?2";
const FOLDED_AT: &str = "SELECT rel_path FROM baseline INDEXED BY baseline_folded WHERE pair_id = ?1 AND folded_path = ?2";
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
static AT: LazyLock<String> = LazyLock::new(at_sql);
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

fn unconfirmed_sql() -> String {
	format!(
		"SELECT {ENTRY_COLUMNS} FROM baseline INDEXED BY baseline_unconfirmed
		 WHERE pair_id = ?1 AND unconfirmed = 1"
	)
}

fn at_sql() -> String {
	format!("SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 AND rel_path = ?2")
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
		})
	}

	fn with<T: Answer>(
		&self,
		what: &str,
		read: impl FnOnce(&Connection) -> rusqlite::Result<T>,
	) -> T {
		#[cfg(feature = "bench-internals")]
		STATEMENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
		if let Some(row) = self.remembered(rel_path, |row| row.cloned()) {
			return row;
		}
		self.with("a row", |conn| {
			conn.prepare_cached(&AT)?
				.query_row(params![self.pair, rel_path], BaselineStore::row_to_entry)
				.optional()
		})
	}

	/// Whether a row sits at exactly `rel_path`, and if so whether it is carryable — the two point
	/// questions that need no more of the row than that.
	pub(super) fn carryable_at(&self, rel_path: &str) -> Option<bool> {
		if let Some(flag) = self.remembered(rel_path, |row| row.map(BaselineEntry::carryable)) {
			return flag;
		}
		self.with("a row's carry flag", |conn| {
			conn.prepare_cached(ROW)?
				.query_row(params![self.pair, rel_path], |row| row.get(0))
				.optional()
		})
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
		let rows = self.rows_from(from, inclusive, to, limit);
		let page = Arc::new(Page {
			from: from.to_string(),
			inclusive,
			to: to.map(str::to_string),
			complete: rows.len() < limit,
			rows,
		});
		*self
			.last_page
			.lock()
			.unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(&page));
		page
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
		Arc::new(Page {
			from: from.to_string(),
			inclusive,
			to: to.map(str::to_string),
			complete: rows.len() < limit,
			rows,
		})
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

	/// The path of every row whose folded path is exactly `folded`.
	pub(super) fn folded_at(&self, folded: &str) -> Vec<String> {
		self.with("the rows at a folded path", |conn| {
			conn.prepare_cached(FOLDED_AT)?
				.query_map(params![self.pair, folded], |row| row.get(0))?
				.collect()
		})
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

	pub(super) fn unconfirmed(&self) -> Vec<BaselineEntry> {
		self.with("the unconfirmed rows", |conn| {
			conn.prepare_cached(&UNCONFIRMED)?
				.query_map(params![self.pair], BaselineStore::row_to_entry)?
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
mod tests {
	use super::*;

	/// Every statement a pass reads with SEEKS the index it was written for, and none scans: at a
	/// million rows a scan is the whole pair per question, which is the cost the resident tree
	/// existed to avoid and the one this reader must not bring back. Asserted on the plan, naming
	/// the index and the terms it seeks on — "SEARCH, and not SCAN" alone passes for a seek on
	/// `pair_id` that then walks every row of the pair.
	#[test]
	fn every_statement_a_pass_reads_with_seeks_its_index() {
		let conn = super::super::super::baseline::test_rows_connection(1);
		let plan = |sql: &str, args: &[&dyn rusqlite::ToSql]| -> Vec<String> {
			conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
				.unwrap()
				.query_map(args, |row| row.get("detail"))
				.unwrap()
				.collect::<rusqlite::Result<_>>()
				.unwrap()
		};
		let primary = "PRIMARY KEY";
		let cases: [(String, Vec<&dyn rusqlite::ToSql>, &str, &str); 19] = [
			(
				dirs_sql(false, false),
				vec![&1_i64, &"a"],
				"baseline_dirs",
				"(pair_id=? AND rel_path>?)",
			),
			(
				dirs_sql(true, false),
				vec![&1_i64, &"a"],
				"baseline_dirs",
				"(pair_id=? AND rel_path>?)",
			),
			(
				dirs_sql(false, true),
				vec![&1_i64, &"a/", &"a0"],
				"baseline_dirs",
				"(pair_id=? AND rel_path>? AND rel_path<?)",
			),
			(
				dirs_sql(true, true),
				vec![&1_i64, &"a/", &"a0"],
				"baseline_dirs",
				"(pair_id=? AND rel_path>? AND rel_path<?)",
			),
			(
				UNCONFIRMED_PATHS.to_string(),
				vec![&1_i64],
				"baseline_unconfirmed",
				"(pair_id=?)",
			),
			(
				at_sql(),
				vec![&1_i64, &"a"],
				primary,
				"(pair_id=? AND rel_path=?)",
			),
			(
				ROW.to_string(),
				vec![&1_i64, &"a"],
				primary,
				"(pair_id=? AND rel_path=?)",
			),
			(
				range_sql(true, false),
				vec![&1_i64, &"a"],
				primary,
				"(pair_id=? AND rel_path>?)",
			),
			(
				range_sql(false, true),
				vec![&1_i64, &"a/", &"a0"],
				primary,
				"(pair_id=? AND rel_path>? AND rel_path<?)",
			),
			(
				FOLDED_AT.to_string(),
				vec![&1_i64, &"a"],
				"baseline_folded",
				"(pair_id=? AND folded_path=?)",
			),
			(
				FOLDED_UNDER.to_string(),
				vec![&1_i64, &"a/", &"", &"a0"],
				"baseline_folded",
				"(pair_id=? AND (folded_path,rel_path)>(?,?) AND folded_path<?)",
			),
			(
				UNCARRYABLE.to_string(),
				vec![&1_i64],
				"baseline_uncarryable",
				"(pair_id=?)",
			),
			(
				RULE_FILES.to_string(),
				vec![&1_i64],
				"baseline_rule_files",
				"(pair_id=?)",
			),
			(
				ANY_UNCONFIRMED.to_string(),
				vec![&1_i64],
				"baseline_unconfirmed",
				"(pair_id=?)",
			),
			(
				unconfirmed_sql(),
				vec![&1_i64],
				"baseline_unconfirmed",
				"(pair_id=?)",
			),
			(
				unsynced_sql(false),
				vec![&1_i64],
				"baseline_state",
				"(pair_id=? AND state=?)",
			),
			(
				unsynced_sql(true),
				vec![&1_i64, &"a/", &"a0"],
				"baseline_state",
				"(pair_id=? AND state=? AND rel_path>? AND rel_path<?)",
			),
			(
				COUNTS.to_string(),
				vec![&1_i64],
				"baseline_counts",
				"INTEGER PRIMARY KEY (rowid=?)",
			),
			(
				// The one shape a caller never sends — `""` bounded — is still a seek.
				range_sql(false, false),
				vec![&1_i64, &""],
				primary,
				"(pair_id=? AND rel_path>?)",
			),
		];
		for (sql, args, index, seek) in &cases {
			let steps = plan(sql, args);
			assert!(
				steps.iter().any(|step| step.starts_with("SEARCH")
					&& step.contains(index)
					&& step.contains(seek)),
				"{sql}\nmust seek {index} on {seek}: {steps:?}"
			);
			assert!(
				!steps
					.iter()
					.any(|step| step.contains("SCAN") || step.contains("TEMP B-TREE")),
				"{sql}\nscans or sorts: {steps:?}"
			);
		}
	}

	/// No statement a pass reads with is RE-PREPARED when it is asked again with other values.
	///
	/// This build of SQLite has STAT4, under which a statement whose plan could depend on a bound
	/// value — a bound `LIMIT` is one — is expired whenever that binding changes and re-prepared
	/// inside its next step: a parse and a plan per page, invisible in every other test and most
	/// of a whole pass's cost when it happened. Each read runs twice with different values, and
	/// SQLite's own counter for the statement it ran has to still read zero.
	#[test]
	fn no_statement_a_pass_reads_with_is_prepared_again_per_call() {
		let rows = (0..40).map(|n| BaselineEntry {
			rel_path: format!("d{}/f{n:02}", n % 3),
			..crate::sync_engine::rows::tests::file_for_test(n)
		});
		let snapshot = standalone(rows);
		for (from, to) in [("d0", Some("d1")), ("d1/f", None)] {
			for inclusive in [false, true] {
				snapshot.page(from, inclusive, to, 3);
			}
		}
		snapshot.row("d0/f00");
		snapshot.row("d1/f01");
		snapshot.carryable_at("d0/f00");
		snapshot.carryable_at("d2/f02");
		snapshot.folded_at("d0");
		snapshot.folded_at("d1");
		snapshot.folded_under("d0", None, 2);
		snapshot.folded_under("d1", Some(("d1/f01", "d1/f01")), 2);
		snapshot.unsynced_under("");
		snapshot.unsynced_under("d0");
		snapshot.unsynced_under("d1");
		let guard = snapshot.conn.lock().unwrap();
		let conn = guard.as_ref().unwrap();
		for sql in [
			AT.as_str(),
			ROW,
			&RANGE[0][0],
			&RANGE[0][1],
			&RANGE[1][0],
			&RANGE[1][1],
			FOLDED_AT,
			FOLDED_UNDER,
			&UNSYNCED[1],
		] {
			let statement = conn.prepare_cached(sql).unwrap();
			assert_eq!(
				statement.get_status(rusqlite::StatementStatus::RePrepare),
				0,
				"re-prepared on a new binding: {sql}"
			);
			assert!(
				statement.get_status(rusqlite::StatementStatus::VmStep) > 0,
				"the read never ran this statement, so its counter says nothing: {sql}"
			);
		}
	}
}
