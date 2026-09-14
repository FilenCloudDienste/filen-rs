//! The persisted 3-way baseline: the last-synced snapshot of every item under a sync pair.
//!
//! Each [`BaselineEntry`] records what the engine last reconciled at a given relative path — the
//! remote uuid, the content hash + size, the local mtime it actually observed AFTER writing (so
//! the mtime+size fast-path is stable), and the remote modified time. Reconciliation compares this
//! baseline against a fresh local scan and a fresh remote snapshot; the difference on each side
//! tells the engine what changed and which way to apply it.
//!
//! Stored in its own SQLite DB (one writer — the engine), separate from the cache DB so the
//! cache's single-writer worker model is untouched.

use std::path::Path;

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use uuid::Uuid;

use super::{engine::PendingKind, guard::DeleteGuard, mode::SyncMode};

/// The schema version this build writes and understands, stamped into `PRAGMA user_version`.
///
/// There is no migration chain and no released schema to migrate FROM: a DB stamped at anything
/// else — older or newer — was not written by this engine, and reading it under these rules would
/// misread its rows into deletes. Both directions are refused (see [`BaselineStore::init`]); the
/// first released schema is what a migration path would start from.
const SCHEMA_VERSION: i64 = 1;

/// Schema for the baseline DB, created whole on a fresh DB. `foreign_keys` is applied
/// per-connection in [`BaselineStore::init`] (it resets to off on every open). No WAL: a single
/// owner writes and reads this DB, so the default rollback journal is enough.
///
/// `pending_writes` is the journal of remote writes this engine has made that the cache has not
/// announced yet (see [`PendingWrites`](super::engine::PendingWrites)), keyed by uuid like the
/// in-memory journal it mirrors. `path_failures` counts how many times in a row applying one path
/// has failed, and what the last failure said. Both cascade with their pair.
///
/// `sync_pairs.id` is `AUTOINCREMENT` for one reason: a removed pair's id must never come back. A
/// watch loop finishes the pass it is in when its pair is removed, and that pass goes on writing
/// baseline and journal rows keyed by the id it started with — which a plain `INTEGER PRIMARY KEY`
/// hands straight to the next pair created, whose first sync is then reconciled against a stranger's
/// rows. With the id retired those writes hit the foreign key and fail, which is what they should do.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_pairs (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	local_root TEXT NOT NULL,
	remote_root BLOB NOT NULL,
	mode INTEGER NOT NULL,
	guard_floor INTEGER,
	guard_ratio REAL,
	paused INTEGER NOT NULL DEFAULT 0,
	UNIQUE (local_root, remote_root)
);

CREATE TABLE IF NOT EXISTS baseline (
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rel_path TEXT NOT NULL,
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
	agreed_hash BLOB,
	PRIMARY KEY (pair_id, rel_path)
);

CREATE TABLE IF NOT EXISTS pending_writes (
	uuid BLOB PRIMARY KEY,
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	kind INTEGER NOT NULL,
	path TEXT,
	replaced BLOB,
	from_path TEXT,
	to_path TEXT,
	recorded_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS path_failures (
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rel_path TEXT NOT NULL,
	attempts INTEGER NOT NULL,
	last_error TEXT NOT NULL,
	PRIMARY KEY (pair_id, rel_path)
);
";

/// `pending_writes.kind` discriminants — what the write did.
const KIND_CREATED: i64 = 1;
const KIND_MOVED: i64 = 2;
const KIND_TRASHED: i64 = 3;

/// Whether a baseline row describes a directory or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
	Dir,
	File,
}

impl NodeKind {
	fn as_i64(self) -> i64 {
		match self {
			Self::Dir => 1,
			Self::File => 2,
		}
	}

	fn from_i64(value: i64) -> Option<Self> {
		match value {
			1 => Some(Self::Dir),
			2 => Some(Self::File),
			_ => None,
		}
	}
}

/// Lifecycle state of a baseline row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BaselineState {
	/// The path is fully reconciled — local, remote, and baseline agree.
	Synced,
	/// A two-way conflict was surfaced for this path; it is excluded from further passes until the
	/// caller resolves it.
	Conflicted,
	/// A conflict of the same standing, from the other side of the same race: this engine's own
	/// upload landed on top of a version it never saw — another client's edit, made between this
	/// pass reading the remote and its upload landing — and buried it.
	///
	/// It is held exactly like [`Conflicted`](Self::Conflicted), and the row's remote half names
	/// the BURIED version rather than what the remote holds now (our own upload is the head). That
	/// difference is why it is a state of its own:
	/// [`resolve_conflict`](super::SyncEngine::resolve_conflict) has to restore the version to
	/// bring it back, where an ordinary conflict only has to stop pushing over it.
	Overwritten,
	/// The DESTINATION's copy of an item the source no longer has, adopted at a mode switch
	/// ([`Backlog::AdoptDestination`](super::mode::Backlog::AdoptDestination)). The row records
	/// what the destination held at that moment.
	///
	/// It is not a synced state and never classifies anything: the reconcile reads such a path as
	/// having no baseline at all, so the item reads as newly created on the side that has it. What
	/// the row adds is one thing — a one-way mode does NOT delete the destination's copy while the
	/// source is still empty there. The row retires the moment either side moves: the two sides
	/// converging adopts a `Synced` row over it, and both sides losing the path drops it.
	Adopted,
}

impl BaselineState {
	fn as_i64(self) -> i64 {
		match self {
			Self::Synced => 0,
			Self::Conflicted => 1,
			Self::Adopted => 2,
			Self::Overwritten => 3,
		}
	}

	fn from_i64(value: i64) -> Option<Self> {
		match value {
			0 => Some(Self::Synced),
			1 => Some(Self::Conflicted),
			2 => Some(Self::Adopted),
			3 => Some(Self::Overwritten),
			_ => None,
		}
	}

	/// Whether the row is a divergence being HELD for the caller to resolve — both flavours. The
	/// planner treats them identically: the path and its subtree are excluded until
	/// [`resolve_conflict`](super::SyncEngine::resolve_conflict) picks a winner.
	pub(crate) fn is_conflict(self) -> bool {
		matches!(self, Self::Conflicted | Self::Overwritten)
	}
}

/// One last-synced item under a pair, keyed by its path relative to both roots (already
/// NFC-normalized by the scanner / remote view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BaselineEntry {
	pub(crate) rel_path: String,
	pub(crate) kind: NodeKind,
	/// The remote item's uuid at last sync (`None` for a dir that exists only locally in a
	/// not-yet-pushed state, etc.).
	pub(crate) remote_uuid: Option<Uuid>,
	/// BLAKE3 of the file content at last sync (files only).
	pub(crate) content_hash: Option<Blake3Hash>,
	/// File size in bytes at last sync.
	pub(crate) size: Option<u64>,
	/// Local mtime in epoch millis, read back AFTER the last write so the fast-path does not
	/// thrash when the FS rounds or ignores a set-times.
	pub(crate) local_mtime: Option<i64>,
	/// Remote `last_modified` in epoch millis at last sync.
	pub(crate) remote_modified: Option<i64>,
	pub(crate) state: BaselineState,
	/// CONFLICTED ROWS ONLY (`None` on a `Synced` row, where both sides agree and the fields above
	/// describe both): what each side looked like when the conflict was surfaced, so a later
	/// [`resolve_conflict`](super::SyncEngine::resolve_conflict) can re-anchor the row to the
	/// winner and leave the loser reading as stale. `local_kind` / `remote_kind` are `None` when
	/// that side was ABSENT (a delete-vs-modify divergence). The local half is carried by
	/// `content_hash` / `size` / `local_mtime`; the remote half by the three fields here plus
	/// `remote_uuid` / `remote_modified`.
	pub(crate) local_kind: Option<NodeKind>,
	pub(crate) remote_kind: Option<NodeKind>,
	pub(crate) remote_hash: Option<Blake3Hash>,
	pub(crate) remote_size: Option<u64>,
	/// The server-minted whole-life id of the remote FILE this row last recorded — `None` for a
	/// directory (which has none: its uuid survives renames) and for a row written before this
	/// column existed.
	///
	/// `remote_uuid` is a VERSION id: every content edit and every version restore re-mints it. This
	/// one does not, so it is what tells a new version of the SAME file apart from a DIFFERENT file
	/// that has taken the path over.
	pub(crate) remote_stable_uuid: Option<StableUuid>,
	/// The last content BOTH sides were known to hold at this path.
	///
	/// `content_hash` is what THIS side recorded; this is what the two sides last AGREED on. A pull,
	/// an adopt of a converged path and a conflict resolution write the two the same — both sides
	/// demonstrably held that content at that moment. A PUSH does not: an upload proves the server
	/// took our bytes, not that the remote still held them when we next looked, so the marker stays
	/// on the previous agreed content until something confirms our version: a snapshot listing it at
	/// the path, or its having stood as the remote head for
	/// [`CONFIRM_TENURE`](super::engine::CONFIRM_TENURE).
	///
	/// That gap is what tells a remote edit made AFTER our push (pull it) from one made
	/// CONCURRENTLY with it (a conflict) — see `plan::reconcile_two_way`.
	///
	/// `None` = nothing is on record (a file this side created and pushed, and nothing has confirmed
	/// it since). That is an UNCONFIRMED push like any other, not consent: a foreign version of the
	/// same lineage landing on such a row is a conflict, which is what makes two clients creating
	/// the same name concurrently non-silent.
	pub(crate) agreed_hash: Option<Blake3Hash>,
}

/// One row of the persisted pending-write journal: a remote write this engine made, and when by
/// the WALL clock (unix millis — the in-memory journal's `Instant` does not outlive its process).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingRow {
	pub(crate) pair: PairId,
	pub(crate) uuid: Uuid,
	pub(crate) kind: PendingKind,
	pub(crate) recorded_at: i64,
}

/// A baseline edit a remote write produced, committed in the same transaction as that write's
/// journal row.
#[derive(Debug)]
pub(crate) enum BaselineChange<'a> {
	Upsert(&'a BaselineEntry),
	Delete(&'a str),
}

/// A registered sync pair, as returned by [`SyncEngine::list_pairs`](super::SyncEngine::list_pairs).
#[derive(Debug, Clone, PartialEq)]
pub struct PairRecord {
	pub id: PairId,
	/// The canonicalized local root the pair syncs.
	pub local_root: String,
	/// The remote folder the pair syncs against.
	pub remote_root: Uuid,
	pub mode: SyncMode,
	/// The pair's mass-delete threshold, as set by
	/// [`SyncEngine::set_delete_guard`](super::SyncEngine::set_delete_guard) (or the default it was
	/// registered with).
	pub delete_guard: DeleteGuard,
	/// Whether the pair is [`paused`](super::SyncEngine::pause_pair) — the same answer
	/// [`SyncEngine::is_paused`](super::SyncEngine::is_paused) gives, so a caller listing the pairs
	/// does not have to ask again per pair.
	pub paused: bool,
}

/// A registered pair's id, handed back by [`SyncEngine::add_pair`](super::SyncEngine::add_pair).
pub type PairId = i64;

/// The baseline DB handle (sole owner / single writer).
pub(crate) struct BaselineStore {
	conn: Connection,
}

fn open_error(error: rusqlite::Error) -> crate::Error {
	crate::Error::custom_with_source(
		crate::ErrorKind::Internal,
		error,
		Some("opening the sync baseline DB".to_string()),
	)
}

fn corrupt(what: &str, value: i64) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(
		0,
		Type::Integer,
		format!("invalid {what} value in baseline DB: {value}").into(),
	)
}

fn hash_from_blob(bytes: Vec<u8>) -> rusqlite::Result<Blake3Hash> {
	<[u8; 32]>::try_from(bytes.as_slice())
		.map(Blake3Hash::from)
		.map_err(|_| {
			rusqlite::Error::FromSqlConversionFailure(
				0,
				Type::Blob,
				format!("content_hash blob is {} bytes, expected 32", bytes.len()).into(),
			)
		})
}

/// Whether the DB has never been written to — no table of ours, and none of anybody else's. That,
/// not `user_version`, is what tells a brand-new file apart from a DB stamped at a version this
/// build does not know: version 0 is both "SQLite's default for an empty file" and "stamped by
/// something that did not use `user_version`".
fn is_unwritten(conn: &Connection) -> rusqlite::Result<bool> {
	let tables: i64 = conn.query_row(
		"SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
		[],
		|row| row.get(0),
	)?;
	Ok(tables == 0)
}

/// Create the schema and stamp its version as ONE transaction.
///
/// SQLite's DDL is transactional and `user_version` lives in the database header, so a failure
/// anywhere in the batch rolls the whole thing back and leaves the file exactly as it was found:
/// no tables, version 0 — which [`is_unwritten`] reads as a brand-new DB, so the next open creates
/// it from scratch. Without the transaction a failure halfway through leaves a partial set of
/// tables at version 0, and every later open refuses that as a stranger's DB.
///
/// Takes the batch as a parameter so the failure path is testable with a batch that cannot commit.
fn create_schema(conn: &Connection, schema: &str) -> rusqlite::Result<()> {
	let tx = conn.unchecked_transaction()?;
	conn.execute_batch(schema)?;
	conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
	tx.commit()
}

impl BaselineStore {
	/// Open (creating if needed) the baseline DB at `path`.
	pub(crate) fn open(path: &Path) -> Result<Self, crate::Error> {
		Self::init(Connection::open(path).map_err(open_error)?)
	}

	#[cfg(test)]
	pub(crate) fn open_in_memory() -> Result<Self, crate::Error> {
		Self::init(Connection::open_in_memory().map_err(open_error)?)
	}

	/// A fresh DB is created whole and stamped at [`SCHEMA_VERSION`]. An existing one is opened only
	/// when it carries exactly that version: anything else — older or newer — is refused rather than
	/// read, since there is no migration chain to bring it here and reading foreign rows under these
	/// rules would misplan them into deletes.
	fn init(conn: Connection) -> Result<Self, crate::Error> {
		conn.execute_batch("PRAGMA foreign_keys = ON;")
			.map_err(open_error)?;
		let version: i64 = conn
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.map_err(open_error)?;
		if is_unwritten(&conn).map_err(open_error)? {
			create_schema(&conn, SCHEMA).map_err(open_error)?;
		} else if version != SCHEMA_VERSION {
			return Err(crate::Error::custom(
				crate::ErrorKind::InvalidState,
				format!(
					"sync baseline DB is at schema version {version}, but this build reads only \
					 version {SCHEMA_VERSION}: refusing to open it (it was not written by this \
					 engine; remove the baseline DB to re-sync from scratch)"
				),
			));
		}
		Ok(Self { conn })
	}

	/// Register a pair, or find the existing one with the same `(local_root, remote_root)`.
	///
	/// Returns the pair's id AND the mode actually stored, which is the caller's `mode` only when
	/// the pair is new: an existing registration is left exactly as it is, so the caller can tell a
	/// genuine re-registration from an attempt to change an established pair's direction
	/// (see [`set_mode`](Self::set_mode)).
	pub(crate) fn create_pair(
		&self,
		local_root: &str,
		remote_root: Uuid,
		mode: SyncMode,
	) -> rusqlite::Result<(PairId, SyncMode)> {
		self.conn.execute(
			"INSERT OR IGNORE INTO sync_pairs (local_root, remote_root, mode) VALUES (?1, ?2, ?3)",
			params![local_root, remote_root, mode.as_i64()],
		)?;
		self.conn.query_row(
			"SELECT id, mode FROM sync_pairs WHERE local_root = ?1 AND remote_root = ?2",
			params![local_root, remote_root],
			|row| {
				let raw: i64 = row.get("mode")?;
				Ok((
					row.get("id")?,
					SyncMode::from_i64(raw).ok_or_else(|| corrupt("mode", raw))?,
				))
			},
		)
	}

	/// Change a registered pair's mode, returning how many rows it touched (0 = unknown pair).
	///
	/// `adopted` is the baseline re-seed a
	/// [`Backlog::AdoptDestination`](super::mode::Backlog::AdoptDestination) switch computed — empty
	/// for [`Propagate`](super::mode::Backlog::Propagate), where the rows are deliberately left
	/// alone. The rows and the mode land in ONE transaction: a switch that took effect without its
	/// re-seed is the propagating behaviour the caller explicitly did not ask for, and the very next
	/// pass would act on it.
	pub(crate) fn set_mode(
		&self,
		id: PairId,
		mode: SyncMode,
		adopted: &[BaselineEntry],
	) -> rusqlite::Result<usize> {
		let tx = self.conn.unchecked_transaction()?;
		let changed = self.conn.execute(
			"UPDATE sync_pairs SET mode = ?2 WHERE id = ?1",
			params![id, mode.as_i64()],
		)?;
		if changed > 0 {
			for entry in adopted {
				self.upsert_entry(id, entry)?;
			}
		}
		tx.commit()?;
		Ok(changed)
	}

	pub(crate) fn pair(&self, id: PairId) -> rusqlite::Result<Option<PairRecord>> {
		self.conn
			.query_row(
				"SELECT id, local_root, remote_root, mode, guard_floor, guard_ratio, paused FROM sync_pairs WHERE id = ?1",
				params![id],
				Self::row_to_pair,
			)
			.optional()
	}

	pub(crate) fn list_pairs(&self) -> rusqlite::Result<Vec<PairRecord>> {
		self.conn
			.prepare("SELECT id, local_root, remote_root, mode, guard_floor, guard_ratio, paused FROM sync_pairs ORDER BY id")?
			.query_map([], Self::row_to_pair)?
			.collect()
	}

	/// Persist `pair`'s paused flag. Returns whether the pair exists — an unknown id updates
	/// nothing, and the engine turns that into an error rather than a silent no-op.
	pub(crate) fn set_paused(&self, id: PairId, paused: bool) -> rusqlite::Result<bool> {
		let updated = self.conn.execute(
			"UPDATE sync_pairs SET paused = ?1 WHERE id = ?2",
			params![paused, id],
		)?;
		Ok(updated > 0)
	}

	/// Every pair currently paused. Read once when the engine opens, then tracked in memory.
	pub(crate) fn paused_pairs(&self) -> rusqlite::Result<Vec<PairId>> {
		self.conn
			.prepare("SELECT id FROM sync_pairs WHERE paused != 0")?
			.query_map([], |row| row.get(0))?
			.collect()
	}

	pub(crate) fn delete_pair(&self, id: PairId) -> rusqlite::Result<()> {
		// The `ON DELETE CASCADE` (with `foreign_keys = ON`) drops the pair's baseline rows.
		self.conn
			.execute("DELETE FROM sync_pairs WHERE id = ?1", params![id])?;
		Ok(())
	}

	/// Persist a pair's mass-delete threshold; every later pass screens against it.
	pub(crate) fn set_delete_guard(
		&self,
		id: PairId,
		guard: DeleteGuard,
	) -> rusqlite::Result<usize> {
		self.conn.execute(
			"UPDATE sync_pairs SET guard_floor = ?2, guard_ratio = ?3 WHERE id = ?1",
			// `DeleteGuard::unlimited`'s floor is `usize::MAX`, which no SQLite integer holds; it
			// saturates to `i64::MAX`, still far past any tracked-item count.
			params![
				id,
				i64::try_from(guard.floor()).unwrap_or(i64::MAX),
				guard.ratio()
			],
		)
	}

	fn row_to_pair(row: &Row<'_>) -> rusqlite::Result<PairRecord> {
		let mode_raw: i64 = row.get("mode")?;
		// Both NULL is the ordinary case (a pair registered before v3, or never reconfigured): the
		// default policy, which is exactly what those pairs already ran under.
		let delete_guard = match (
			row.get::<_, Option<i64>>("guard_floor")?,
			row.get::<_, Option<f64>>("guard_ratio")?,
		) {
			(Some(floor), Some(ratio)) => DeleteGuard::new(floor.max(0) as usize, ratio)
				.map_err(|_| corrupt("guard ratio", ratio as i64))?,
			_ => DeleteGuard::default(),
		};
		Ok(PairRecord {
			id: row.get("id")?,
			local_root: row.get("local_root")?,
			remote_root: row.get("remote_root")?,
			mode: SyncMode::from_i64(mode_raw).ok_or_else(|| corrupt("mode", mode_raw))?,
			delete_guard,
			paused: row.get("paused")?,
		})
	}

	/// Insert or replace one baseline row.
	pub(crate) fn upsert_entry(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT OR REPLACE INTO baseline
			 (pair_id, rel_path, kind, remote_uuid, content_hash, size, local_mtime,
			  remote_modified, state, local_kind, remote_kind, remote_hash, remote_size,
			  remote_stable_uuid, agreed_hash)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
			params![
				pair,
				entry.rel_path,
				entry.kind.as_i64(),
				entry.remote_uuid,
				entry.content_hash.as_ref().map(|h| h.as_ref().as_slice()),
				entry.size.map(|s| s as i64),
				entry.local_mtime,
				entry.remote_modified,
				entry.state.as_i64(),
				entry.local_kind.map(NodeKind::as_i64),
				entry.remote_kind.map(NodeKind::as_i64),
				entry.remote_hash.as_ref().map(|h| h.as_ref().as_slice()),
				entry.remote_size.map(|s| s as i64),
				entry.remote_stable_uuid,
				entry.agreed_hash.as_ref().map(|h| h.as_ref().as_slice()),
			],
		)?;
		Ok(())
	}

	/// One baseline row by path (whole-pair snapshots go through [`Self::entries`]).
	pub(crate) fn entry(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<BaselineEntry>> {
		self.conn
			.query_row(
				"SELECT rel_path, kind, remote_uuid, content_hash, size, local_mtime,
				        remote_modified, state, local_kind, remote_kind, remote_hash, remote_size,
				        remote_stable_uuid, agreed_hash
				 FROM baseline WHERE pair_id = ?1 AND rel_path = ?2",
				params![pair, rel_path],
				Self::row_to_entry,
			)
			.optional()
	}

	/// Every baseline row for `pair`, ordered by path (parent-before-child for same-prefix paths).
	pub(crate) fn entries(&self, pair: PairId) -> rusqlite::Result<Vec<BaselineEntry>> {
		self.conn
			.prepare(
				"SELECT rel_path, kind, remote_uuid, content_hash, size, local_mtime,
				        remote_modified, state, local_kind, remote_kind, remote_hash, remote_size,
				        remote_stable_uuid, agreed_hash
				 FROM baseline WHERE pair_id = ?1 ORDER BY rel_path",
			)?
			.query_map(params![pair], Self::row_to_entry)?
			.collect()
	}

	pub(crate) fn delete_entry(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		self.conn.execute(
			"DELETE FROM baseline WHERE pair_id = ?1 AND rel_path = ?2",
			params![pair, rel_path],
		)?;
		Ok(())
	}

	/// Journal a remote write AND apply the baseline edits it produced, in ONE transaction.
	///
	/// The two have to land together: the journal row is what a reopened engine folds into its
	/// remote view, and the baseline row is the written state it folds. A crash between them leaves
	/// the next pass either re-doing the write or folding a row that describes nothing.
	pub(crate) fn record_pending(
		&self,
		pair: PairId,
		uuid: Uuid,
		kind: &PendingKind,
		recorded_at: i64,
		changes: &[BaselineChange<'_>],
	) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		let (kind_id, path, replaced, from_path, to_path) = match kind {
			PendingKind::Created { path, replaced } => {
				(KIND_CREATED, Some(path.as_str()), *replaced, None, None)
			}
			PendingKind::Moved { from, to } => (
				KIND_MOVED,
				None,
				None,
				Some(from.as_str()),
				Some(to.as_str()),
			),
			PendingKind::Trashed => (KIND_TRASHED, None, None, None, None),
		};
		self.conn.execute(
			"INSERT OR REPLACE INTO pending_writes
			 (uuid, pair_id, kind, path, replaced, from_path, to_path, recorded_at)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
			params![
				uuid,
				pair,
				kind_id,
				path,
				replaced,
				from_path,
				to_path,
				recorded_at
			],
		)?;
		self.write_changes(pair, changes)?;
		tx.commit()
	}

	/// Apply several baseline edits in ONE transaction — for a local write that re-keys a whole
	/// subtree, where a crash part-way would leave rows under both spellings.
	pub(crate) fn apply_changes(
		&self,
		pair: PairId,
		changes: &[BaselineChange<'_>],
	) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		self.write_changes(pair, changes)?;
		tx.commit()
	}

	fn write_changes(&self, pair: PairId, changes: &[BaselineChange<'_>]) -> rusqlite::Result<()> {
		for change in changes {
			match change {
				BaselineChange::Upsert(entry) => self.upsert_entry(pair, entry)?,
				BaselineChange::Delete(rel_path) => self.delete_entry(pair, rel_path)?,
			}
		}
		Ok(())
	}

	/// Count one failed attempt at `rel_path`, remembering what went wrong. Consecutive: a success
	/// or a [`clear_failure`](Self::clear_failure) resets the count to nothing.
	pub(crate) fn record_failure(
		&self,
		pair: PairId,
		rel_path: &str,
		error: &str,
	) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT INTO path_failures (pair_id, rel_path, attempts, last_error)
			 VALUES (?1, ?2, 1, ?3)
			 ON CONFLICT (pair_id, rel_path)
			 DO UPDATE SET attempts = attempts + 1, last_error = excluded.last_error",
			params![pair, rel_path, error],
		)?;
		Ok(())
	}

	/// Forget `rel_path`'s failure streak — it succeeded, or the caller asked for a retry.
	pub(crate) fn clear_failure(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		self.conn.execute(
			"DELETE FROM path_failures WHERE pair_id = ?1 AND rel_path = ?2",
			params![pair, rel_path],
		)?;
		Ok(())
	}

	/// Every path under `pair` with a live failure streak: `rel_path -> (attempts, last error)`.
	pub(crate) fn failures(
		&self,
		pair: PairId,
	) -> rusqlite::Result<std::collections::HashMap<String, (u32, String)>> {
		self.conn
			.prepare("SELECT rel_path, attempts, last_error FROM path_failures WHERE pair_id = ?1")?
			.query_map(params![pair], |row| {
				let attempts: i64 = row.get("attempts")?;
				Ok((
					row.get::<_, String>("rel_path")?,
					(attempts.max(0) as u32, row.get::<_, String>("last_error")?),
				))
			})?
			.collect()
	}

	/// Retire journal rows the in-memory journal has dropped (the cache caught up, or the grace
	/// window ran out).
	pub(crate) fn delete_pending(&self, uuids: &[Uuid]) -> rusqlite::Result<()> {
		let mut stmt = self
			.conn
			.prepare("DELETE FROM pending_writes WHERE uuid = ?1")?;
		for uuid in uuids {
			stmt.execute(params![uuid])?;
		}
		Ok(())
	}

	/// The journal a previous engine left behind, minus the rows no engine may act on any more —
	/// those older than `grace_millis` (the same ceiling the in-memory journal applies) and those
	/// naming a pair that is gone. Both are DELETED here, so the journal cannot accumulate.
	pub(crate) fn load_pending(
		&self,
		now: i64,
		grace_millis: i64,
	) -> rusqlite::Result<Vec<PendingRow>> {
		self.conn.execute(
			"DELETE FROM pending_writes WHERE pair_id NOT IN (SELECT id FROM sync_pairs)",
			[],
		)?;
		self.conn.execute(
			"DELETE FROM pending_writes WHERE recorded_at <= ?1",
			params![now.saturating_sub(grace_millis)],
		)?;
		self.conn
			.prepare(
				"SELECT pair_id, uuid, kind, path, replaced, from_path, to_path, recorded_at
				 FROM pending_writes",
			)?
			.query_map([], Self::row_to_pending)?
			.collect()
	}

	fn row_to_pending(row: &Row<'_>) -> rusqlite::Result<PendingRow> {
		let kind_raw: i64 = row.get("kind")?;
		let missing = |what: &str| {
			rusqlite::Error::FromSqlConversionFailure(
				0,
				Type::Null,
				format!("pending_writes row of kind {kind_raw} has no {what}").into(),
			)
		};
		let text = |column: &str| -> rusqlite::Result<String> {
			row.get::<_, Option<String>>(column)?
				.ok_or_else(|| missing(column))
		};
		let kind = match kind_raw {
			KIND_CREATED => PendingKind::Created {
				path: text("path")?,
				replaced: row.get("replaced")?,
			},
			KIND_MOVED => PendingKind::Moved {
				from: text("from_path")?,
				to: text("to_path")?,
			},
			KIND_TRASHED => PendingKind::Trashed,
			_ => return Err(corrupt("pending write kind", kind_raw)),
		};
		Ok(PendingRow {
			pair: row.get("pair_id")?,
			uuid: row.get("uuid")?,
			kind,
			recorded_at: row.get("recorded_at")?,
		})
	}

	fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<BaselineEntry> {
		let kind_raw: i64 = row.get("kind")?;
		let state_raw: i64 = row.get("state")?;
		let content_hash = row
			.get::<_, Option<Vec<u8>>>("content_hash")?
			.map(hash_from_blob)
			.transpose()?;
		let remote_hash = row
			.get::<_, Option<Vec<u8>>>("remote_hash")?
			.map(hash_from_blob)
			.transpose()?;
		let agreed_hash = row
			.get::<_, Option<Vec<u8>>>("agreed_hash")?
			.map(hash_from_blob)
			.transpose()?;
		let side_kind = |raw: Option<i64>| match raw {
			None => Ok(None),
			Some(raw) => NodeKind::from_i64(raw)
				.map(Some)
				.ok_or_else(|| corrupt("kind", raw)),
		};
		Ok(BaselineEntry {
			rel_path: row.get("rel_path")?,
			kind: NodeKind::from_i64(kind_raw).ok_or_else(|| corrupt("kind", kind_raw))?,
			remote_uuid: row.get("remote_uuid")?,
			content_hash,
			size: row.get::<_, Option<i64>>("size")?.map(|s| s as u64),
			local_mtime: row.get("local_mtime")?,
			remote_modified: row.get("remote_modified")?,
			state: BaselineState::from_i64(state_raw).ok_or_else(|| corrupt("state", state_raw))?,
			local_kind: side_kind(row.get("local_kind")?)?,
			remote_kind: side_kind(row.get("remote_kind")?)?,
			remote_hash,
			remote_size: row.get::<_, Option<i64>>("remote_size")?.map(|s| s as u64),
			remote_stable_uuid: row.get("remote_stable_uuid")?,
			agreed_hash,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn file_entry(rel_path: &str, hash: [u8; 32], size: u64) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(Uuid::new_v4()),
			content_hash: Some(Blake3Hash::from(hash)),
			size: Some(size),
			local_mtime: Some(1_700_000_000_123),
			remote_modified: Some(1_700_000_000_456),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(StableUuid::new_for_test(Uuid::new_v4())),
			agreed_hash: Some(Blake3Hash::from([0xAB; 32])),
		}
	}

	fn dir_entry(rel_path: &str) -> BaselineEntry {
		BaselineEntry {
			rel_path: rel_path.to_string(),
			kind: NodeKind::Dir,
			remote_uuid: Some(Uuid::new_v4()),
			content_hash: None,
			size: None,
			local_mtime: None,
			remote_modified: None,
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: None,
			agreed_hash: None,
		}
	}

	/// A unique temp path for a store that has to be closed and reopened.
	fn temp_db_path(tag: &str) -> std::path::PathBuf {
		std::env::temp_dir().join(format!("filen_baseline_{tag}_{}.db", Uuid::new_v4()))
	}

	/// Wall-clock stamps for the journal tests, and the ceiling they are measured against.
	const NOW: i64 = 1_800_000_000_000;
	const GRACE: i64 = 180_000;

	fn pending_count(store: &BaselineStore) -> i64 {
		store
			.conn
			.query_row("SELECT COUNT(*) FROM pending_writes", [], |row| row.get(0))
			.unwrap()
	}

	fn created(path: &str) -> PendingKind {
		PendingKind::Created {
			path: path.to_string(),
			replaced: None,
		}
	}

	/// The `PRAGMA user_version` a store's connection reports.
	fn version_of(store: &BaselineStore) -> i64 {
		store
			.conn
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap()
	}

	#[test]
	fn a_fresh_db_is_stamped_at_the_current_schema_version() {
		let store = BaselineStore::open_in_memory().unwrap();
		assert_eq!(version_of(&store), SCHEMA_VERSION);
	}

	/// Both directions of a version mismatch are refused, and the DB is left exactly as it was.
	/// There is no migration chain: a stamp other than the current one means the DB was not written
	/// by this engine, whichever side of the current version it sits on.
	#[test]
	fn a_db_stamped_at_any_other_version_is_refused_not_read() {
		for planted in [SCHEMA_VERSION - 1, SCHEMA_VERSION + 1] {
			let path = temp_db_path(&format!("version_{planted}"));
			{
				let store = BaselineStore::open(&path).unwrap();
				store
					.conn
					.execute_batch(&format!("PRAGMA user_version = {planted};"))
					.unwrap();
			}
			let message = match BaselineStore::open(&path) {
				Ok(_) => panic!("schema version {planted} must be refused"),
				Err(error) => error.to_string(),
			};
			assert!(
				message.contains("schema version") && message.contains("refusing"),
				"the refusal must name the version problem: {message}"
			);
			// Fails CLOSED: the DB is left as it was, neither migrated nor re-stamped in place.
			let raw = Connection::open(&path).unwrap();
			let still: i64 = raw
				.query_row("PRAGMA user_version", [], |row| row.get(0))
				.unwrap();
			assert_eq!(still, planted, "the refused DB was rewritten");
			drop(raw);
			std::fs::remove_file(&path).ok();
		}
	}

	/// Creating the schema is all-or-nothing. A batch that fails partway (here: a statement SQLite
	/// rejects, standing in for a disk error) must leave the file exactly as it was found — no
	/// tables, version 0 — because that is the one state a later open reads as "brand new" and
	/// creates from scratch. A partial set of tables would be refused as a stranger's DB forever.
	#[test]
	fn a_schema_creation_that_fails_partway_leaves_no_tables_behind() {
		let path = temp_db_path("partial_schema");
		let broken = format!("{SCHEMA}\nCREATE TABLE oops (id INTEGER PRIMARY KEY;");
		{
			let conn = Connection::open(&path).unwrap();
			create_schema(&conn, &broken).expect_err("the broken batch must fail");
			assert!(
				is_unwritten(&conn).unwrap(),
				"the failed creation left tables behind"
			);
			let version: i64 = conn
				.query_row("PRAGMA user_version", [], |row| row.get(0))
				.unwrap();
			assert_eq!(version, 0, "the failed creation stamped a version");
		}

		// So the next open creates it from scratch rather than refusing it.
		let store = BaselineStore::open(&path).unwrap();
		assert_eq!(version_of(&store), SCHEMA_VERSION);
		assert!(store.list_pairs().unwrap().is_empty());
		drop(store);
		std::fs::remove_file(&path).ok();
	}

	/// A DB file that exists but has never been written to is a FRESH one, not a version-0 stranger:
	/// `user_version` reads 0 either way, so emptiness is what tells them apart.
	#[test]
	fn an_empty_db_file_is_created_from_scratch_rather_than_refused() {
		let path = temp_db_path("empty_file");
		drop(Connection::open(&path).unwrap());
		let store = BaselineStore::open(&path).unwrap();
		assert_eq!(version_of(&store), SCHEMA_VERSION);
		assert!(store.list_pairs().unwrap().is_empty());
		drop(store);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn a_configured_delete_guard_survives_a_reopen() {
		let path = temp_db_path("guard_roundtrip");
		let remote = Uuid::new_v4();
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let (pair, _) = store
				.create_pair("/root", remote, SyncMode::TwoWay)
				.unwrap();
			assert_eq!(
				store.pair(pair).unwrap().unwrap().delete_guard,
				DeleteGuard::default(),
				"a new pair starts on the default"
			);
			store
				.set_delete_guard(pair, DeleteGuard::new(3, 0.25).unwrap())
				.unwrap();
			pair
		};

		let store = BaselineStore::open(&path).unwrap();
		let guard = store.pair(pair).unwrap().unwrap().delete_guard;
		assert_eq!(guard.floor(), 3);
		assert_eq!(guard.ratio(), 0.25);
		assert_eq!(
			store.list_pairs().unwrap()[0].delete_guard,
			guard,
			"list_pairs reports it too"
		);

		// `unlimited`'s usize::MAX floor has no SQLite representation; it must still come back as a
		// threshold nothing can exceed rather than silently wrapping to a strict one.
		store
			.set_delete_guard(pair, DeleteGuard::unlimited())
			.unwrap();
		assert!(
			store.pair(pair).unwrap().unwrap().delete_guard.floor() > u32::MAX as usize,
			"an unlimited floor must not wrap on the way through the DB"
		);
		drop(store);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn path_failures_count_consecutively_and_reset_on_a_clear() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		assert!(store.failures(pair).unwrap().is_empty());

		store.record_failure(pair, "a.txt", "boom").unwrap();
		store.record_failure(pair, "a.txt", "boom again").unwrap();
		store.record_failure(pair, "b.txt", "other").unwrap();
		let failures = store.failures(pair).unwrap();
		assert_eq!(
			failures["a.txt"],
			(2, "boom again".to_string()),
			"the streak counts up and keeps the LAST error"
		);
		assert_eq!(failures["b.txt"].0, 1);

		// Clearing one path leaves the other alone, and a later failure starts from 1 again.
		store.clear_failure(pair, "a.txt").unwrap();
		assert!(!store.failures(pair).unwrap().contains_key("a.txt"));
		store.record_failure(pair, "a.txt", "fresh").unwrap();
		assert_eq!(store.failures(pair).unwrap()["a.txt"].0, 1);
		assert_eq!(store.failures(pair).unwrap()["b.txt"].0, 1);

		// Clearing a path with no streak is a no-op, not an error.
		store.clear_failure(pair, "never-failed.txt").unwrap();

		// The streak is pair-scoped state: removing the pair takes it with it.
		store.delete_pair(pair).unwrap();
		assert!(store.failures(pair).unwrap().is_empty());
	}

	/// A removed pair's id must never be handed to a later pair. A watch loop finishes the pass it
	/// is in when its pair is removed, so that pass still writes baseline and journal rows keyed by
	/// the old id: with the id reused, they land in the successor — whose first sync is supposed to
	/// see an EMPTY baseline — instead of failing against a pair that is gone.
	#[test]
	fn a_removed_pairs_id_is_never_handed_to_a_later_pair() {
		let store = BaselineStore::open_in_memory().unwrap();
		let remote = Uuid::new_v4();
		let (pair, _) = store
			.create_pair("/root", remote, SyncMode::TwoWay)
			.unwrap();
		store.delete_pair(pair).unwrap();

		let (readded, _) = store
			.create_pair("/root", remote, SyncMode::TwoWay)
			.unwrap();
		assert_ne!(readded, pair, "the removed pair's id came back");
		// So a write from the removed pair's in-flight pass has nowhere to land.
		assert!(
			store
				.upsert_entry(pair, &file_entry("f.txt", [1; 32], 3))
				.is_err(),
			"a baseline write for the removed pair was accepted"
		);
	}

	#[test]
	fn create_pair_is_idempotent_and_reports_the_stored_mode() {
		let store = BaselineStore::open_in_memory().unwrap();
		let remote = Uuid::new_v4();
		let (id, mode) = store
			.create_pair("/home/u/sync", remote, SyncMode::TwoWay)
			.unwrap();
		assert_eq!(
			mode,
			SyncMode::TwoWay,
			"a new pair takes the asked-for mode"
		);

		// Re-registering the same roots returns the same id and the mode ALREADY stored — an
		// existing pair is never silently re-pointed by a registration call.
		let (again, stored) = store
			.create_pair("/home/u/sync", remote, SyncMode::LocalToRemote)
			.unwrap();
		assert_eq!(id, again, "re-registering a pair returns the same id");
		assert_eq!(
			stored,
			SyncMode::TwoWay,
			"the stored mode is reported back, not overwritten"
		);
		assert_eq!(store.pair(id).unwrap().unwrap().mode, SyncMode::TwoWay);

		// Changing it is an explicit, separate operation.
		assert_eq!(store.set_mode(id, SyncMode::LocalToRemote, &[]).unwrap(), 1);
		assert_eq!(
			store.pair(id).unwrap().unwrap().mode,
			SyncMode::LocalToRemote
		);
		assert_eq!(store.set_mode(9999, SyncMode::TwoWay, &[]).unwrap(), 0);
		assert_eq!(store.list_pairs().unwrap().len(), 1);
	}

	/// A `Backlog::AdoptDestination` switch writes its re-seeded rows and the new mode together:
	/// a mode that took effect without them is the propagating behaviour the caller did not ask
	/// for, and the very next pass would act on it.
	#[test]
	fn a_mode_change_and_the_rows_it_adopts_land_together() {
		let path = temp_db_path("reconfigure");
		let remote = Uuid::new_v4();
		let adopted = BaselineEntry {
			state: BaselineState::Adopted,
			..file_entry("gone.txt", [4; 32], 7)
		};
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let (pair, _) = store
				.create_pair("/root", remote, SyncMode::LocalBackup)
				.unwrap();
			assert_eq!(
				store
					.set_mode(
						pair,
						SyncMode::LocalToRemote,
						std::slice::from_ref(&adopted)
					)
					.unwrap(),
				1
			);
			pair
		};

		let reopened = BaselineStore::open(&path).unwrap();
		assert_eq!(
			reopened.pair(pair).unwrap().unwrap().mode,
			SyncMode::LocalToRemote
		);
		assert_eq!(
			reopened.entry(pair, "gone.txt").unwrap().as_ref(),
			Some(&adopted),
			"the adopted row must survive with its state intact"
		);

		// An unknown pair changes nothing at all — neither a mode nor a stray baseline row.
		assert_eq!(
			reopened
				.set_mode(
					pair + 9_999,
					SyncMode::TwoWay,
					std::slice::from_ref(&adopted)
				)
				.unwrap(),
			0
		);
		assert_eq!(reopened.entries(pair + 9_999).unwrap().len(), 0);
		assert_eq!(
			reopened.pair(pair).unwrap().unwrap().mode,
			SyncMode::LocalToRemote
		);
		drop(reopened);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn entries_round_trip_with_full_fidelity() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		let f = file_entry("a/b.txt", [9u8; 32], 4242);
		let d = dir_entry("a");
		store.upsert_entry(pair, &f).unwrap();
		store.upsert_entry(pair, &d).unwrap();

		assert_eq!(store.entry(pair, "a/b.txt").unwrap().as_ref(), Some(&f));
		assert_eq!(store.entry(pair, "a").unwrap().as_ref(), Some(&d));
		assert_eq!(store.entry(pair, "missing").unwrap(), None);

		// Ordered by rel_path: "a" before "a/b.txt".
		let all = store.entries(pair).unwrap();
		assert_eq!(
			all.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>(),
			vec!["a", "a/b.txt"]
		);
	}

	#[test]
	fn upsert_replaces_and_delete_removes() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::RemoteToLocal)
			.unwrap();

		let mut f = file_entry("x.txt", [1u8; 32], 10);
		store.upsert_entry(pair, &f).unwrap();
		f.size = Some(20);
		f.content_hash = Some(Blake3Hash::from([2u8; 32]));
		store.upsert_entry(pair, &f).unwrap();
		assert_eq!(store.entry(pair, "x.txt").unwrap(), Some(f.clone()));
		assert_eq!(store.entries(pair).unwrap().len(), 1, "replace, not insert");

		store.delete_entry(pair, "x.txt").unwrap();
		assert_eq!(store.entry(pair, "x.txt").unwrap(), None);
	}

	#[test]
	fn deleting_a_pair_cascades_to_its_baseline_rows() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.upsert_entry(pair, &file_entry("keep.txt", [3u8; 32], 1))
			.unwrap();
		assert_eq!(store.entries(pair).unwrap().len(), 1);

		store.delete_pair(pair).unwrap();
		assert!(store.pair(pair).unwrap().is_none());
		assert!(
			store.entries(pair).unwrap().is_empty(),
			"ON DELETE CASCADE dropped the baseline rows"
		);
	}

	/// The whole point of persisting the journal: the engine that made the write is gone, and the
	/// one that reopens the DB has to find both halves — the record AND the baseline row that says
	/// what the write left behind, which is what a fold reads.
	#[test]
	fn a_pending_write_and_the_row_it_produced_survive_a_reopen() {
		let path = temp_db_path("pending_reopen");
		let entry = file_entry("a.txt", [7u8; 32], 5);
		let uuid = Uuid::new_v4();
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let (pair, _) = store
				.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
				.unwrap();
			store
				.record_pending(
					pair,
					uuid,
					&created("a.txt"),
					NOW,
					&[BaselineChange::Upsert(&entry)],
				)
				.unwrap();
			pair
		};

		let reopened = BaselineStore::open(&path).unwrap();
		assert_eq!(
			reopened.load_pending(NOW + 1_000, GRACE).unwrap(),
			vec![PendingRow {
				pair,
				uuid,
				kind: created("a.txt"),
				recorded_at: NOW,
			}]
		);
		assert_eq!(
			reopened.entry(pair, "a.txt").unwrap().as_ref(),
			Some(&entry),
			"the row the write produced is there to be folded"
		);

		// Retiring it is what the next pass does once the cache has caught up.
		reopened.delete_pending(&[uuid]).unwrap();
		assert!(
			reopened
				.load_pending(NOW + 1_000, GRACE)
				.unwrap()
				.is_empty()
		);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn every_write_kind_round_trips_through_the_journal() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let replaced = Uuid::new_v4();
		let kinds = [
			PendingKind::Created {
				path: "a.txt".to_string(),
				replaced: Some(replaced),
			},
			PendingKind::Moved {
				from: "a.txt".to_string(),
				to: "b.txt".to_string(),
			},
			PendingKind::Trashed,
		];
		let uuids: Vec<Uuid> = kinds.iter().map(|_| Uuid::new_v4()).collect();
		for (uuid, kind) in uuids.iter().zip(&kinds) {
			store.record_pending(pair, *uuid, kind, NOW, &[]).unwrap();
		}

		let mut loaded = store.load_pending(NOW, GRACE).unwrap();
		loaded.sort_by_key(|row| uuids.iter().position(|u| *u == row.uuid).unwrap());
		assert_eq!(
			loaded.into_iter().map(|row| row.kind).collect::<Vec<_>>(),
			kinds.to_vec()
		);
	}

	/// Past the grace ceiling the snapshot is believed again, so the row must not come back — and
	/// it is DELETED rather than filtered, or the journal would grow without bound.
	#[test]
	fn a_write_past_the_grace_window_is_dropped_on_load() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.record_pending(
				pair,
				Uuid::new_v4(),
				&created("old.txt"),
				NOW - GRACE - 1,
				&[],
			)
			.unwrap();
		let fresh = Uuid::new_v4();
		store
			.record_pending(pair, fresh, &created("new.txt"), NOW - 1_000, &[])
			.unwrap();

		let loaded = store.load_pending(NOW, GRACE).unwrap();
		assert_eq!(
			loaded.iter().map(|row| row.uuid).collect::<Vec<_>>(),
			vec![fresh],
			"only the write still inside the window is handed back"
		);
		assert_eq!(pending_count(&store), 1, "the expired row was deleted");
	}

	#[test]
	fn deleting_a_pair_takes_its_journal_rows_with_it() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.record_pending(pair, Uuid::new_v4(), &created("a.txt"), NOW, &[])
			.unwrap();
		let kept = Uuid::new_v4();
		store
			.record_pending(other, kept, &created("b.txt"), NOW, &[])
			.unwrap();

		store.delete_pair(pair).unwrap();
		assert_eq!(
			store
				.load_pending(NOW, GRACE)
				.unwrap()
				.iter()
				.map(|row| row.uuid)
				.collect::<Vec<_>>(),
			vec![kept],
			"the removed pair's journal went with it, the other pair's stayed"
		);
	}

	/// A pair removed while foreign keys were off (an older build, a repaired DB) leaves rows
	/// behind that name nothing. Loading must drop them rather than hand the engine a write it
	/// could only fold against a baseline that no longer exists.
	#[test]
	fn a_journal_row_whose_pair_is_gone_is_dropped_on_load() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.record_pending(pair, Uuid::new_v4(), &created("a.txt"), NOW, &[])
			.unwrap();

		store
			.conn
			.execute_batch("PRAGMA foreign_keys = OFF;")
			.unwrap();
		store.delete_pair(pair).unwrap();
		store
			.conn
			.execute_batch("PRAGMA foreign_keys = ON;")
			.unwrap();
		assert_eq!(
			pending_count(&store),
			1,
			"precondition: the row was orphaned"
		);

		assert!(store.load_pending(NOW, GRACE).unwrap().is_empty());
		assert_eq!(pending_count(&store), 0, "the orphan was deleted");
	}

	/// The listed record carries the flag, so a caller rendering the pairs does not have to ask per
	/// pair — and reads it from the row, not from a default the record was built with.
	#[test]
	fn a_listed_pair_reports_whether_it_is_paused() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (running, _) = store
			.create_pair("/home/u/running", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (paused, _) = store
			.create_pair("/home/u/paused", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		assert!(
			store.list_pairs().unwrap().iter().all(|pair| !pair.paused),
			"a new pair must list as running"
		);

		store.set_paused(paused, true).unwrap();
		let listed = store.list_pairs().unwrap();
		assert_eq!(
			listed
				.iter()
				.filter(|pair| pair.paused)
				.map(|pair| pair.id)
				.collect::<Vec<_>>(),
			vec![paused],
			"list_pairs did not report the paused pair"
		);
		assert!(store.pair(paused).unwrap().unwrap().paused);
		assert!(!store.pair(running).unwrap().unwrap().paused);

		store.set_paused(paused, false).unwrap();
		assert!(!store.pair(paused).unwrap().unwrap().paused, "the resume");
	}

	#[test]
	fn the_paused_flag_round_trips_and_survives_a_reopen() {
		let path = temp_db_path("paused");
		let remote = Uuid::new_v4();
		let (running, paused) = {
			let store = BaselineStore::open(&path).unwrap();
			let (running, _) = store
				.create_pair("/home/u/running", Uuid::new_v4(), SyncMode::TwoWay)
				.unwrap();
			let (paused, _) = store
				.create_pair("/home/u/paused", remote, SyncMode::TwoWay)
				.unwrap();
			assert!(store.paused_pairs().unwrap().is_empty(), "a new pair runs");

			assert!(store.set_paused(paused, true).unwrap());
			assert_eq!(store.paused_pairs().unwrap(), vec![paused]);
			// Idempotent, and an unknown pair is reported as unknown rather than silently ignored.
			assert!(store.set_paused(paused, true).unwrap());
			assert!(!store.set_paused(paused + 9_999, true).unwrap());

			// Re-adding the same roots must not un-pause the pair behind the caller's back.
			assert_eq!(
				store
					.create_pair("/home/u/paused", remote, SyncMode::LocalToRemote)
					.unwrap()
					.0,
				paused
			);
			assert_eq!(store.paused_pairs().unwrap(), vec![paused]);
			(running, paused)
		};

		let reopened = BaselineStore::open(&path).unwrap();
		assert_eq!(
			reopened.paused_pairs().unwrap(),
			vec![paused],
			"the paused flag did not survive the reopen"
		);
		assert!(reopened.set_paused(paused, false).unwrap());
		assert!(reopened.paused_pairs().unwrap().is_empty());
		// Removing a pair takes its flag with it.
		assert!(reopened.set_paused(running, true).unwrap());
		reopened.delete_pair(running).unwrap();
		assert!(reopened.paused_pairs().unwrap().is_empty());
		drop(reopened);
		std::fs::remove_file(&path).ok();
	}
}
