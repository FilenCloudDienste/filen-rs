//! A pass's scratch database: an SQLite file of the pass's own for what it would otherwise hold in
//! memory between making its plan and finishing its apply (see [`plan`] for what it stages today).
//!
//! # One file per pass
//!
//! The file sits beside the baseline DB and is named after it, the pair and the pass:
//! `<baseline>-stage-<pair>-<pass>`, the pass being numbered by the engine (see
//! `SyncEngine::stage`). A name per PASS rather than per pair, because `plan_pair` takes no pass
//! gate and reads alongside a pair's pass: a dry run that staged its work under the pair's name
//! would truncate the plan a live pass is applying. Named after the baseline FILE rather than kept
//! in a directory of stages, because baselines share directories — every test's sits in the temp
//! dir — and two engines number their pairs and passes alike, from 1 and 0.
//!
//! The file is removed when the [`Stage`] is dropped, once its connection has closed. One a crash
//! left behind is removed when an engine next opens the baseline ([`sweep`]).
//!
//! # Pragmas
//!
//! Every one is set explicitly ([`PRAGMAS`]), none left to the build's defaults, since the point of
//! the file is to keep what it holds out of memory:
//!
//! - `page_size = 8192`, the baseline's own: a staged row carries a path or two and three hashes.
//! - `journal_mode = OFF` and `synchronous = OFF`: nothing in the file outlives the pass, and a stage
//!   that fails is dropped whole, so there is nothing to roll back to or to make durable.
//! - `locking_mode = EXCLUSIVE`: one connection ever opens the file, and holds its lock from its
//!   first write instead of taking it per transaction.
//! - `cache_size = -2048`: a 2 MiB page cache.
//! - `cache_spill = ON`: a transaction whose dirty pages outgrow that cache writes them to the file
//!   as it goes instead of holding them until it commits. SQLite's default, but one a build can
//!   change, and ordering a large plan writes far more than the cache holds in one statement.
//! - `temp_store = FILE`: a sort or a transient index that outgrows the cache spills to a file of
//!   SQLite's own rather than to memory (see `cache::sql::bound_memory`, which explains where those
//!   files go and why Android keeps them in memory whatever this says).
//! - `mmap_size = 0`: no mapped page of the file counts in the process's resident set.
//!
//! # A failed read
//!
//! A staged plan is the only copy of what the pass is applying, so a read that fails is never the
//! end of the plan. A write that fails — a full disk while the plan is staged — is an error the pass
//! returns, before it has applied anything.
//!
//! A read that fails is an error as well: the apply reads the next page with transfers in flight,
//! and a panic would drop those transfers between their network op and the row that records it. A
//! page that fails to read ([`Pager::peek_in`], and [`Pager::sweep`], which reads its pages alike)
//! stops the apply admitting anything more: the transfers in flight finish, and the pass fails.

mod plan;
#[cfg(test)]
mod tests;

use std::{
	ffi::OsString,
	fs,
	io::ErrorKind as IoErrorKind,
	path::{Path, PathBuf},
};

use rusqlite::Connection;

#[cfg(test)]
pub(super) use plan::PAGE_ROWS;
pub(super) use plan::{Pager, PlanWriter, Run, Staged, StagedPlan};

use super::baseline::PairId;
use crate::{Error, ErrorKind};

/// Every pragma of a stage's connection, set before its first page is written (see the module
/// docs for why each).
const PRAGMAS: &str = "PRAGMA page_size = 8192; PRAGMA journal_mode = OFF; \
	 PRAGMA synchronous = OFF; PRAGMA locking_mode = EXCLUSIVE; PRAGMA cache_size = -2048; \
	 PRAGMA cache_spill = ON; PRAGMA temp_store = FILE; PRAGMA mmap_size = 0;";

/// How many rows a [`BatchWriter`] writes per transaction.
const BATCH_ROWS: usize = 8192;

/// One pass's scratch database (see the module docs).
pub(super) struct Stage {
	/// Declared before `file`, so it is dropped first: the file is removed once nothing has it open.
	conn: Connection,
	file: StageFile,
}

/// The path of a stage's file, which removing it goes with.
struct StageFile(PathBuf);

impl Drop for StageFile {
	fn drop(&mut self) {
		if let Err(error) = fs::remove_file(&self.0)
			&& error.kind() != IoErrorKind::NotFound
		{
			tracing::warn!(
				"sync engine: could not remove the pass's stage {}: {error}; the next open of its \
				 baseline removes it",
				self.0.display()
			);
		}
	}
}

impl Stage {
	/// A new, empty stage at `path`.
	///
	/// The file is created here and refused if anything is at the path already: that is a leftover
	/// [`sweep`] could not remove or, in the cases the baseline's lock does not cover (see there), a
	/// live pass of another engine, and a stage must neither remove the one nor open the other.
	pub(super) fn create(path: PathBuf) -> Result<Self, Error> {
		if let Err(error) = fs::OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
		{
			return Err(Error::custom_with_source(
				ErrorKind::IO,
				error,
				Some(format!("creating the pass's stage {}", path.display())),
			));
		}
		let file = StageFile(path);
		let conn = Connection::open(&file.0)
			.and_then(|conn| conn.execute_batch(PRAGMAS).map(|()| conn))
			.map_err(|error| {
				super::engine::db_error(
					error,
					&format!("opening the pass's stage {}", file.0.display()),
				)
			})?;
		Ok(Self { conn, file })
	}

	/// Where the stage's file is.
	pub(super) fn path(&self) -> &Path {
		&self.file.0
	}

	/// A stage beside a baseline in a directory of its own, which goes with the directory.
	#[cfg(test)]
	pub(super) fn in_temp_dir() -> (tempfile::TempDir, Self) {
		let dir = tempfile::tempdir().expect("a temp dir for the stage");
		let stage = Self::create(stage_path(&dir.path().join("baseline.db"), 1, 0))
			.expect("creating a stage");
		(dir, stage)
	}
}

/// Where the stage of pass `pass` of `pair` goes, for the baseline at `db_path`.
pub(super) fn stage_path(db_path: &Path, pair: PairId, pass: u64) -> PathBuf {
	let mut name = stage_prefix(db_path);
	name.push(format!("{pair}-{pass}"));
	db_path.with_file_name(name)
}

/// The name every stage of the baseline at `db_path` starts with.
fn stage_prefix(db_path: &Path) -> OsString {
	let mut prefix = db_path.file_name().unwrap_or_default().to_os_string();
	prefix.push("-stage-");
	prefix
}

/// Remove every stage the engines before this one left beside the baseline at `db_path`: a pass
/// that crashed, or whose stage could not be removed when it ended.
///
/// Run when an engine opens the baseline, before it has run a pass, so no stage of its own is among
/// them. A file that cannot be removed is logged and left; its name is never handed out again,
/// since [`Stage::create`] refuses a path that is taken.
///
/// Nor, but in the two cases below, is any of them a stage another engine is running a pass on,
/// though nothing in its name would tell: [`SyncEngine::open`](super::SyncEngine::open) sweeps
/// while it holds the lock that keeps every other engine, in this process or another, off the
/// baseline, and an engine holds that lock for as long as it lives. The lock does not cover these:
/// where the platform has no file locks there is none to hold; and a pass cancelled while its work
/// runs on a blocking thread can still hold its stage there after its engine is dropped, so an
/// engine that opens the baseline in the same process before that work ends sweeps it. Where an
/// open file can be removed, a pass whose name is removed runs on in the file it holds open; where
/// it cannot, as on Windows, the stage is left, and the next pass given its name fails to create
/// one.
pub(super) fn sweep(db_path: &Path) {
	let prefix = stage_prefix(db_path);
	let dir = match db_path.parent() {
		Some(dir) if !dir.as_os_str().is_empty() => dir,
		_ => Path::new("."),
	};
	let entries = match fs::read_dir(dir) {
		Ok(entries) => entries,
		Err(error) => {
			tracing::warn!(
				"sync engine: could not list {} for stages left behind: {error}",
				dir.display()
			);
			return;
		}
	};
	for entry in entries {
		let entry = match entry {
			Ok(entry) => entry,
			Err(error) => {
				tracing::warn!(
					"sync engine: could not list {} for stages left behind: {error}",
					dir.display()
				);
				continue;
			}
		};
		if !entry
			.file_name()
			.as_encoded_bytes()
			.starts_with(prefix.as_encoded_bytes())
		{
			continue;
		}
		match fs::remove_file(entry.path()) {
			Ok(()) => tracing::debug!(
				"sync engine: removed the stage {} a previous engine left behind",
				entry.path().display()
			),
			Err(error) => tracing::warn!(
				"sync engine: could not remove the stage {} a previous engine left behind: {error}",
				entry.path().display()
			),
		}
	}
}

/// Rows into a [`Stage`], committed every [`BATCH_ROWS`]: a transaction per row would write the
/// pages each row touches once per row, and one for the lot would make the transaction as large as
/// the plan.
pub(super) struct BatchWriter {
	stage: Stage,
	/// Rows written since the open transaction began; none is open at 0.
	open: usize,
}

impl BatchWriter {
	pub(super) fn new(stage: Stage) -> Self {
		Self { stage, open: 0 }
	}

	/// Write one row with `write`, inside the batch's transaction.
	pub(super) fn row(
		&mut self,
		write: impl FnOnce(&Connection) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		if self.open == 0 {
			self.stage.conn.execute_batch("BEGIN")?;
		}
		write(&self.stage.conn)?;
		self.open += 1;
		if self.open == BATCH_ROWS {
			self.stage.conn.execute_batch("COMMIT")?;
			self.open = 0;
		}
		Ok(())
	}

	/// The stage, with every row written committed.
	pub(super) fn finish(self) -> rusqlite::Result<Stage> {
		if self.open > 0 {
			self.stage.conn.execute_batch("COMMIT")?;
		}
		Ok(self.stage)
	}
}
