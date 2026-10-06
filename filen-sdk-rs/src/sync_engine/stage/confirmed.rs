//! The pushes of ours a pass confirmed, staged once there are more of them than a window holds.
//!
//! A confirmation advances a row's agreed-content marker to the row's own content (see
//! `plan::confirm_agreed`), and the pass needs the rows it advanced twice: its view of the rows
//! reads every one of them with the marker advanced for as long as it plans (`rows::Marks`), and a
//! real pass writes them all to the store before it reconciles. Held in memory, both were a row and
//! a marker per push: on the pass after a whole-tree push that is the tree, twice.
//!
//! So [`ConfirmedRows`] holds what a confirmation accepts up to a window of rows, and past that moves
//! them into a stage of the pass's own, a row each in `confirmed` keyed by the path the row had when
//! the pass began — the one the table holds it at. Each row is written as it now stands, in the
//! baseline's own columns, so the store's write reads them back as they are and the view's marker
//! is the row's `agreed_hash`.
//!
//! Its view reads the markers two ways (see [`StagedConfirmations`]): a walk of the rows in path
//! order reads the stretch of `confirmed` it walks beside them, a page at a time, and a lookup of
//! one path reads a page from that path on, as large as the lookups before it walked in order —
//! one row for a lookup out of order — with the read-ahead a staged side's point questions read
//! with (see `page`).
//!
//! A read that fails while the pass reads its rows panics, as a read of the rows themselves does
//! (see `rows::snapshot`): a marker read as absent is a confirmation the plan does not know about.
//! The reads the store's write makes return their error, which rolls that write back.

use std::{
	num::NonZeroUsize,
	sync::{Arc, LazyLock, Mutex, PoisonError},
};

use filen_types::crypto::Blake3Hash;
use rusqlite::{Row, params};

use super::{
	super::{
		baseline::{BaselineEntry, BaselineStore, ENTRY_COLUMNS},
		page::{Page, ReadAhead},
	},
	BatchWriter, Stage, bind_entry, hash_at,
};
use crate::Error;

/// WITHOUT ROWID: the rows are only ever looked up and walked by path, and arrive in path order
/// within a page, so the primary key's b-tree is the table and nothing is ever re-keyed.
const SCHEMA: &str = "
CREATE TABLE confirmed (
	rel_path TEXT NOT NULL PRIMARY KEY,
	kind INTEGER NOT NULL,
	remote_uuid BLOB,
	content_hash BLOB,
	size INTEGER,
	local_mtime INTEGER,
	remote_modified INTEGER,
	state INTEGER NOT NULL,
	local_kind INTEGER,
	remote_kind INTEGER,
	remote_hash BLOB,
	remote_size INTEGER,
	remote_stable_uuid BLOB,
	agreed_hash BLOB
) WITHOUT ROWID;
";

static INSERT: LazyLock<String> = LazyLock::new(|| {
	format!(
		"INSERT INTO confirmed ({ENTRY_COLUMNS})
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
	)
});
/// Every row after `?1`, in path order: what the store's write reads, a page at a time. Stepped and
/// stopped rather than `LIMIT`ed, as the baseline's own ranges are (see `rows::snapshot`).
static ROWS_AFTER: LazyLock<String> = LazyLock::new(|| {
	format!("SELECT {ENTRY_COLUMNS} FROM confirmed WHERE rel_path > ?1 ORDER BY rel_path")
});
const HOLDS: &str = "SELECT 1 FROM confirmed WHERE rel_path = ?1";
/// The markers from `?1` on, in path order: a lookup's page.
const MARKS_FROM: &str =
	"SELECT rel_path, agreed_hash FROM confirmed WHERE rel_path >= ?1 ORDER BY rel_path";
/// The markers of a stretch of paths, in path order, for each shape of its bounds: after or from
/// `?1`, before `?2` where there is one.
static MARKS_IN: LazyLock<[[String; 2]; 2]> = LazyLock::new(|| {
	let sql = |inclusive: bool, bounded: bool| {
		format!(
			"SELECT rel_path, agreed_hash FROM confirmed WHERE rel_path {} ?1{} ORDER BY rel_path",
			if inclusive { ">=" } else { ">" },
			if bounded { " AND rel_path < ?2" } else { "" },
		)
	};
	[
		[sql(false, false), sql(false, true)],
		[sql(true, false), sql(true, true)],
	]
});

/// How many rows one read of the stage hands back: the store's write reads a page of rows at a
/// time, a walk of the markers a page of them, and a lookup's read-ahead at most a page.
const PAGE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 is not zero");

/// The rows a confirmation advanced, as they now stand: held while they fit a window, staged past
/// it (see the module docs).
#[derive(Debug)]
pub(in super::super) enum Confirmed {
	Held(Vec<BaselineEntry>),
	/// Shared with the pass's view of the rows, whose markers are these rows'.
	Staged(Arc<StagedConfirmations>),
}

impl Default for Confirmed {
	fn default() -> Self {
		Self::Held(Vec::new())
	}
}

impl Confirmed {
	pub(in super::super) fn len(&self) -> usize {
		match self {
			Self::Held(rows) => rows.len(),
			Self::Staged(staged) => staged.rows,
		}
	}

	pub(in super::super) fn is_empty(&self) -> bool {
		self.len() == 0
	}

	/// Every row, handed to `visit` a page at a time, in no order a caller may rely on.
	pub(in super::super) fn each_page(
		&self,
		mut visit: impl FnMut(&[BaselineEntry]) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		match self {
			Self::Held(rows) => visit(rows),
			Self::Staged(staged) => {
				let mut after = String::new();
				loop {
					let page: Vec<BaselineEntry> = {
						let inner = staged.lock();
						inner
							.stage
							.conn
							.prepare_cached(&ROWS_AFTER)?
							.query_map([&after], BaselineStore::row_to_entry)?
							.take(PAGE.get())
							.collect::<rusqlite::Result<_>>()?
					};
					let Some(last) = page.last() else {
						return Ok(());
					};
					after.clone_from(&last.rel_path);
					visit(&page)?;
					if page.len() < PAGE.get() {
						return Ok(());
					}
				}
			}
		}
	}
}

/// What a confirmation accepted so far (see the module docs).
pub(in super::super) struct ConfirmedRows {
	held: Vec<BaselineEntry>,
	staged: Option<Staging>,
	/// How many rows are held before they all move to a stage.
	window: usize,
}

struct Staging {
	batch: BatchWriter,
	rows: usize,
	/// For the errors, which name the file.
	at: String,
}

impl ConfirmedRows {
	pub(in super::super) fn new(window: usize) -> Self {
		Self {
			held: Vec::new(),
			staged: None,
			window,
		}
	}

	/// Add `row`, as it stands once confirmed. The row past the window moves every row into
	/// `stage()`, made then; every row after it goes straight there.
	pub(in super::super) fn push(
		&mut self,
		row: BaselineEntry,
		stage: impl FnOnce() -> Result<Stage, Error>,
	) -> Result<(), Error> {
		if self.staged.is_none() {
			if self.held.len() < self.window {
				self.held.push(row);
				return Ok(());
			}
			let stage = stage()?;
			let at = stage.path().display().to_string();
			stage
				.conn
				.execute_batch(SCHEMA)
				.map_err(|e| staging_error(e, &at))?;
			self.staged = Some(Staging {
				batch: BatchWriter::new(stage),
				rows: 0,
				at,
			});
			for held in std::mem::take(&mut self.held) {
				self.stage_row(&held)?;
			}
		}
		self.stage_row(&row)
	}

	fn stage_row(&mut self, row: &BaselineEntry) -> Result<(), Error> {
		let staging = self
			.staged
			.as_mut()
			.expect("a row is staged only once there is a stage (should be impossible)");
		staging
			.batch
			.row(|conn| {
				let mut statement = conn.prepare_cached(&INSERT)?;
				bind_entry(&mut statement, 1, row)?;
				statement.raw_execute().map(drop)
			})
			.map_err(|e| staging_error(e, &staging.at))?;
		staging.rows += 1;
		Ok(())
	}

	/// Whether a row at `rel_path` was added.
	pub(in super::super) fn holds(&self, rel_path: &str) -> Result<bool, Error> {
		match &self.staged {
			None => Ok(self.held.iter().any(|row| row.rel_path == rel_path)),
			Some(staging) => staging
				.batch
				.conn()
				.prepare_cached(HOLDS)
				.and_then(|mut statement| statement.exists([rel_path]))
				.map_err(|e| staging_error(e, &staging.at)),
		}
	}

	pub(in super::super) fn finish(self) -> Result<Confirmed, Error> {
		let Some(Staging { batch, rows, at }) = self.staged else {
			return Ok(Confirmed::Held(self.held));
		};
		let stage = batch.finish().map_err(|e| staging_error(e, &at))?;
		Ok(Confirmed::Staged(Arc::new(StagedConfirmations {
			rows,
			inner: Mutex::new(Inner {
				stage,
				lookups: ReadAhead::default(),
			}),
		})))
	}
}

fn staging_error(error: rusqlite::Error, at: &str) -> Error {
	super::super::engine::db_error(error, &format!("staging the pass's confirmations in {at}"))
}

/// A pass's confirmations in their stage, as its view of the rows reads the markers (see the module
/// docs). The connection sits behind a mutex, taken for one read at a time and never while a caller's
/// code runs, as the baseline's own is.
pub(in super::super) struct StagedConfirmations {
	rows: usize,
	inner: Mutex<Inner>,
}

struct Inner {
	stage: Stage,
	/// The page the lookups read last (see the module docs).
	lookups: ReadAhead<Page<(String, Marker)>>,
}

/// The marker a confirmation advanced on a row: the row's `agreed_hash` as it now stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) struct Marker {
	pub(in super::super) agreed_hash: Option<Blake3Hash>,
}

impl std::fmt::Debug for StagedConfirmations {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("StagedConfirmations")
			.field("rows", &self.rows)
			.finish_non_exhaustive()
	}
}

impl StagedConfirmations {
	fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
		self.inner.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// The marker the confirmation advanced on the row at `rel_path`: `None` where it confirmed no
	/// row there.
	pub(in super::super) fn marker_at(&self, rel_path: &str) -> Option<Marker> {
		let mut inner = self.lock();
		let Inner { stage, lookups } = &mut *inner;
		lookups.ask(
			rel_path,
			PAGE,
			|page| {
				page.answer(rel_path)
					.map(|found| found.map(|&(_, marker)| marker))
			},
			|want| {
				let marks: Vec<(String, Marker)> = stage
					.conn
					.prepare_cached(MARKS_FROM)
					.and_then(|mut statement| {
						statement
							.query_map([rel_path], marker_of)?
							.take(want)
							.collect()
					})
					.unwrap_or_else(|error| fail(&error, "a page of the confirmed markers"));
				Page::read(rel_path, true, None, want, marks, |(path, _)| path)
			},
		)
	}

	/// `rows` — in path order, inside the stretch after `lo` (or from it, when `inclusive`) and
	/// before `hi` — each with the marker the confirmation advanced on it, read beside them a page
	/// at a time.
	pub(in super::super) fn mark_in_order<'a>(
		&'a self,
		lo: String,
		inclusive: bool,
		hi: Option<String>,
		rows: impl Iterator<Item = BaselineEntry> + 'a,
	) -> impl Iterator<Item = BaselineEntry> + 'a {
		let mut marks = MarksInOrder {
			staged: self,
			lo,
			inclusive,
			hi,
			page: Vec::new().into_iter(),
			done: false,
		}
		.peekable();
		rows.map(move |mut row| {
			while marks
				.next_if(|(path, _)| path.as_str() < row.rel_path.as_str())
				.is_some()
			{}
			if let Some((_, marker)) = marks.next_if(|(path, _)| *path == row.rel_path) {
				row.agreed_hash = marker.agreed_hash;
			}
			row
		})
	}

	/// What these confirmations hold in memory, in bytes: the handle and the page the lookups read
	/// last, which the read-ahead boxes. The rows themselves are in the stage.
	#[cfg(feature = "bench-internals")]
	pub(in super::super) fn bytes(&self) -> usize {
		let inner = self.lock();
		size_of::<Self>()
			+ inner.lookups.page().map_or(0, |page| {
				size_of_val(page) + page.bytes(|(path, _)| path.capacity())
			})
	}
}

/// The markers of a stretch of paths, in path order, a page at a time.
struct MarksInOrder<'a> {
	staged: &'a StagedConfirmations,
	/// Where the next page starts: after it, or at it when `inclusive`.
	lo: String,
	inclusive: bool,
	hi: Option<String>,
	page: std::vec::IntoIter<(String, Marker)>,
	done: bool,
}

impl Iterator for MarksInOrder<'_> {
	type Item = (String, Marker);

	fn next(&mut self) -> Option<Self::Item> {
		if let Some(mark) = self.page.next() {
			return Some(mark);
		}
		if self.done {
			return None;
		}
		let page: Vec<(String, Marker)> = {
			let inner = self.staged.lock();
			let sql = &MARKS_IN[usize::from(self.inclusive)][usize::from(self.hi.is_some())];
			inner
				.stage
				.conn
				.prepare_cached(sql)
				.and_then(|mut statement| match &self.hi {
					Some(hi) => statement
						.query_map(params![self.lo, hi], marker_of)?
						.take(PAGE.get())
						.collect(),
					None => statement
						.query_map([&self.lo], marker_of)?
						.take(PAGE.get())
						.collect(),
				})
				.unwrap_or_else(|error| fail(&error, "a stretch of the confirmed markers"))
		};
		self.done = page.len() < PAGE.get();
		if let Some((last, _)) = page.last() {
			self.lo.clone_from(last);
			self.inclusive = false;
		}
		self.page = page.into_iter();
		self.page.next()
	}
}

/// A `(path, marker)` row of [`MARKS_FROM`] or [`MARKS_IN`].
fn marker_of(row: &Row<'_>) -> rusqlite::Result<(String, Marker)> {
	Ok((
		row.get(0)?,
		Marker {
			agreed_hash: hash_at(row, 1)?,
		},
	))
}

/// A read of the markers that failed while the pass reads its rows (see the module docs).
fn fail(error: &rusqlite::Error, what: &str) -> ! {
	panic!(
		"reading the pass's staged confirmations ({what}) failed: {error} — refusing to read a \
		 failed read as a marker no confirmation advanced"
	)
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::sync_engine::rows::tests::file_for_test;

	fn confirmed_row(rel_path: &str, hash: u8) -> BaselineEntry {
		let row = BaselineEntry {
			rel_path: rel_path.to_owned(),
			..file_for_test(hash)
		};
		BaselineEntry {
			agreed_hash: row.content_hash,
			..row
		}
	}

	/// Rows past the window move to a stage, and read back as they went in — the store's write reads
	/// every one of them, and a lookup and a walk read each one's marker — whatever order the
	/// lookups come in.
	#[test]
	fn rows_past_the_window_are_staged_and_read_back_as_they_went_in() {
		let (_dir, stage) = Stage::in_temp_dir();
		let mut stage = Some(stage);
		let rows: Vec<BaselineEntry> = (0..(PAGE.get() * 2 + 5))
			.map(|n| confirmed_row(&format!("d/{n:05}"), u8::try_from(n % 200).unwrap()))
			.collect();
		let mut confirmed = ConfirmedRows::new(3);
		for row in &rows {
			confirmed
				.push(row.clone(), || Ok(stage.take().expect("one stage")))
				.unwrap();
		}
		assert!(confirmed.holds("d/00004").unwrap());
		assert!(!confirmed.holds("d/00004x").unwrap());
		let confirmed = confirmed.finish().unwrap();
		let Confirmed::Staged(staged) = &confirmed else {
			panic!("{} rows past a window of 3 are held", confirmed.len())
		};
		assert_eq!(confirmed.len(), rows.len());

		let mut read = Vec::new();
		confirmed
			.each_page(|page| {
				read.extend_from_slice(page);
				Ok(())
			})
			.unwrap();
		assert_eq!(read, rows);

		// Out of order, in order, and at paths it holds no row at.
		for n in [7, 3, 900, 2, (PAGE.get() * 2 + 4)]
			.into_iter()
			.chain(0..(PAGE.get() * 2 + 5))
		{
			let path = format!("d/{n:05}");
			assert_eq!(
				staged.marker_at(&path),
				Some(Marker {
					agreed_hash: rows[n].agreed_hash
				}),
				"{path}"
			);
			assert_eq!(staged.marker_at(&format!("{path}x")), None, "{path}x");
		}
		assert_eq!(staged.marker_at("a"), None);
		assert_eq!(staged.marker_at("z"), None);

		// A walk of every other row and a row no confirmation advanced, beside the markers.
		let unmarked = BaselineEntry {
			rel_path: "e".to_owned(),
			agreed_hash: None,
			..file_for_test(1)
		};
		let walked = rows
			.iter()
			.step_by(2)
			.skip(1)
			.map(|row| BaselineEntry {
				agreed_hash: None,
				..row.clone()
			})
			.chain([unmarked.clone()]);
		let marked: Vec<BaselineEntry> = staged
			.mark_in_order("d/00002".to_owned(), true, None, walked)
			.collect();
		let expected: Vec<BaselineEntry> = rows
			.iter()
			.step_by(2)
			.skip(1)
			.cloned()
			.chain([unmarked])
			.collect();
		assert_eq!(marked, expected);
	}

	/// Up to the window, nothing is staged and no stage is made.
	#[test]
	fn rows_up_to_the_window_are_held() {
		let mut confirmed = ConfirmedRows::new(2);
		for n in 0..2 {
			confirmed
				.push(confirmed_row(&format!("{n}"), 1), || {
					panic!("no stage is made for rows the window holds")
				})
				.unwrap();
		}
		assert!(matches!(confirmed.finish().unwrap(), Confirmed::Held(rows) if rows.len() == 2));
	}
}
