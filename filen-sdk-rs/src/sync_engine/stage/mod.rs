//! A pass's scratch database: an SQLite file of the pass's own for what it would otherwise hold in
//! memory while it plans and applies. A whole pass stages its remote snapshot ([`remote`]) and its
//! local scan ([`local`]) from the read to the end of the plan, both read as a [`StagedSide`]
//! ([`side`]), and reconciles them window by window ([`reconcile`]); every pass that plans anything
//! screens its plan where it is staged ([`screen`]), and stages what it applies ([`plan`]) from the
//! plan to the end of the apply. A pass that confirms more pushes of ours than a window holds stages
//! those as well ([`confirmed`]), from the confirmation to the end of the plan.
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
//! The snapshot, the scan and the plan of one pass take a number each, so a whole pass has three
//! files: the snapshot's and the scan's go once the plan is staged, before the apply begins, or
//! with the pass's inputs when it stops before that (a dry run, a refusal), and the plan's when the
//! apply is done. A pass that pulls alone numbers them in that order; one that pushes takes its
//! scan's first, as it walks the tree while it reads the snapshot, and when the remote read finds
//! a rule file that walk did not match with, it gives that walk and its file up and takes a number
//! for a second walk after the snapshot's. The plan's file is made when the pass reconciles — a
//! whole pass writes its plan there window by window, a change-scoped one once it has planned it in
//! memory — and the plan is screened there and staged for the apply in that same file. A pass that
//! confirms pushes takes a number for them too, after its snapshot's, but makes that file only
//! when it confirms more than a window of them; it goes with the pass's view of the rows, once the
//! plan is staged.
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
//! The reads the apply makes with transfers in flight answer with an error, since a panic would
//! drop those transfers between their network op and the row that records it. A page that fails to
//! read ([`Pager::turn`], and [`Pager::sweep`], which reads its pages alike) stops the apply
//! admitting anything more: the transfers in flight finish, and the pass fails. Where an upload
//! lands, failing to read ([`Pager::created_at`]), fails that upload and the pass with it. The read
//! inside a baseline commit is an error too, which rolls that commit back ([`Carried::each`]).
//!
//! What a deletion asks before it runs ([`Pager::shields_local`], [`Pager::shields_remote`],
//! [`Pager::carried`]) panics instead, as the baseline's rows do (see `rows::snapshot`): only the
//! runs the apply takes outside the transfers ask it, before the action, or the chunk it is in,
//! is sent, so nothing is in flight to drop.

mod confirmed;
mod local;
mod plan;
mod reconcile;
mod remote;
mod screen;
mod side;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::{
	ffi::OsString,
	fs,
	io::ErrorKind as IoErrorKind,
	path::{Path, PathBuf},
};

use filen_types::crypto::Blake3Hash;
use rusqlite::{Connection, Row, Statement, ToSql};

pub(super) use confirmed::{Confirmed, ConfirmedRows, StagedConfirmations};
pub(super) use local::StageSink;
pub(super) use plan::{Carried, Pager, Run, Staged, StagedPlan};
#[cfg(test)]
pub(super) use plan::{PAGE_ROWS, PlanWriter};
#[cfg(test)]
pub(super) use reconcile::test_support::windows_of;
pub(super) use reconcile::{WINDOW, reconcile_windows};
pub(super) use remote::RemoteSink;
pub(super) use screen::{Rows, Screen, ScreenError};
pub(super) use side::{Pages, Paging, StagedSide};
#[cfg(test)]
pub(super) use test_support::{assert_views_alike, paging_of, staged_sides, staged_view};

use super::baseline::{BaselineEntry, NodeKind, PairId};
use crate::{Error, ErrorKind, cache::open_sqlite};

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
		let conn = open_sqlite(&file.0)
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

	/// The stage's connection, for a read between the rows: it sees the rows of the open batch.
	pub(super) fn conn(&self) -> &Connection {
		&self.stage.conn
	}

	/// The stage, with every row written committed.
	pub(super) fn finish(self) -> rusqlite::Result<Stage> {
		if self.open > 0 {
			self.stage.conn.execute_batch("COMMIT")?;
		}
		Ok(self.stage)
	}
}

/// A content hash as the stage's `BLOB` column holds it.
fn hash_bytes(hash: Option<&Blake3Hash>) -> Option<&[u8]> {
	hash.map(|hash| hash.as_ref().as_slice())
}

/// Bind `row`'s columns, in `ENTRY_COLUMNS` order, to `statement`'s parameters from `first` on: what
/// a staged row of the baseline's own shape is written with, for `BaselineStore::row_to_entry` to
/// read back.
fn bind_entry(
	statement: &mut Statement<'_>,
	first: usize,
	row: &BaselineEntry,
) -> rusqlite::Result<()> {
	let columns: [&dyn ToSql; 14] = [
		&row.rel_path,
		&row.kind.as_i64(),
		&row.remote_uuid,
		&hash_bytes(row.content_hash.as_ref()),
		&row.size,
		&row.local_mtime,
		&row.remote_modified,
		&row.state.as_i64(),
		&row.local_kind.map(NodeKind::as_i64),
		&row.remote_kind.map(NodeKind::as_i64),
		&hash_bytes(row.remote_hash.as_ref()),
		&row.remote_size,
		&row.remote_stable_uuid,
		&hash_bytes(row.agreed_hash.as_ref()),
	];
	for (at, column) in columns.into_iter().enumerate() {
		statement.raw_bind_parameter(first + at, column)?;
	}
	Ok(())
}

/// The content hash in `row`'s `column`, as [`hash_bytes`] wrote it.
fn hash_at(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<Blake3Hash>> {
	Ok(row
		.get::<_, Option<[u8; 32]>>(column)?
		.map(Blake3Hash::from))
}
