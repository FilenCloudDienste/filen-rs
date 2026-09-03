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

use filen_types::crypto::Blake3Hash;
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use uuid::Uuid;

use super::{engine::PendingKind, mode::SyncMode};

/// The schema version this build writes and understands, stamped into `PRAGMA user_version`. A DB
/// carrying a HIGHER version was written by a newer engine and is REFUSED (never read under the
/// older rules, which would misread it into deletes); a LOWER one is migrated forward in place.
const SCHEMA_VERSION: i64 = 2;

/// Schema for the baseline DB. `foreign_keys` is applied per-connection in [`BaselineStore::init`]
/// (it resets to off on every open). No WAL: a single owner writes and reads this DB, so the
/// default rollback journal is enough.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_pairs (
	id INTEGER PRIMARY KEY,
	local_root TEXT NOT NULL,
	remote_root BLOB NOT NULL,
	mode INTEGER NOT NULL,
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
	PRIMARY KEY (pair_id, rel_path)
);
";

/// The v2 addition: the journal of remote writes this engine has made that the cache has not
/// announced yet (see [`PendingWrites`](super::engine::PendingWrites)). Keyed by uuid, like the
/// in-memory journal it mirrors, and cascaded with its pair.
const PENDING_WRITES_SCHEMA: &str = "
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
}

impl BaselineState {
	fn as_i64(self) -> i64 {
		match self {
			Self::Synced => 0,
			Self::Conflicted => 1,
		}
	}

	fn from_i64(value: i64) -> Option<Self> {
		match value {
			0 => Some(Self::Synced),
			1 => Some(Self::Conflicted),
			_ => None,
		}
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairRecord {
	pub id: PairId,
	/// The canonicalized local root the pair syncs.
	pub local_root: String,
	/// The remote folder the pair syncs against.
	pub remote_root: Uuid,
	pub mode: SyncMode,
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

/// Whether `table` already has a column named `column`.
fn has_column(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
	let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
	let mut rows = stmt.query_map([], |row| row.get::<_, String>("name"))?;
	rows.try_fold(false, |found, name| Ok(found || name? == column))
}

/// Bring a DB stamped at `from` up to [`SCHEMA_VERSION`], one version at a time. A fresh DB starts
/// at 0 and runs every step, so each one is idempotent against the tables [`SCHEMA`] just created.
fn migrate(conn: &Connection, from: i64) -> rusqlite::Result<()> {
	if from < 1 {
		migrate_to_v1(conn)?;
	}
	if from < 2 {
		migrate_to_v2(conn)?;
	}
	Ok(())
}

/// Bring a version-0 DB (written before `user_version` was stamped, so without the per-side
/// conflict-evidence columns) up to v1. A DB freshly created from [`SCHEMA`] already has them, so
/// this is a no-op there.
fn migrate_to_v1(conn: &Connection) -> rusqlite::Result<()> {
	for (column, ty) in [
		("local_kind", "INTEGER"),
		("remote_kind", "INTEGER"),
		("remote_hash", "BLOB"),
		("remote_size", "INTEGER"),
	] {
		if !has_column(conn, "baseline", column)? {
			conn.execute_batch(&format!("ALTER TABLE baseline ADD COLUMN {column} {ty};"))?;
		}
	}
	Ok(())
}

/// Bring a v1 DB up to v2: the pending-write journal. A v1 DB simply has none, so creating the
/// table is the whole migration — no existing row is read or rewritten.
fn migrate_to_v2(conn: &Connection) -> rusqlite::Result<()> {
	conn.execute_batch(PENDING_WRITES_SCHEMA)
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

	fn init(conn: Connection) -> Result<Self, crate::Error> {
		conn.execute_batch("PRAGMA foreign_keys = ON;")
			.map_err(open_error)?;
		let version: i64 = conn
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.map_err(open_error)?;
		if version > SCHEMA_VERSION {
			return Err(crate::Error::custom(
				crate::ErrorKind::InvalidState,
				format!(
					"sync baseline DB is at schema version {version}, but this build understands \
					 only up to {SCHEMA_VERSION}: refusing to open it (a newer engine wrote it; \
					 upgrade, or remove the baseline DB to re-sync from scratch)"
				),
			));
		}
		// Idempotent: creates the base tables on a fresh DB, no-ops on an existing one; the
		// migrations below add everything a later version introduced.
		conn.execute_batch(SCHEMA).map_err(open_error)?;
		if version < SCHEMA_VERSION {
			migrate(&conn, version).map_err(open_error)?;
			conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
				.map_err(open_error)?;
		}
		Ok(Self { conn })
	}

	/// Register a pair (or return the existing pair's id for the same `(local_root, remote_root)`).
	pub(crate) fn create_pair(
		&self,
		local_root: &str,
		remote_root: Uuid,
		mode: SyncMode,
	) -> rusqlite::Result<PairId> {
		self.conn.execute(
			"INSERT INTO sync_pairs (local_root, remote_root, mode) VALUES (?1, ?2, ?3)
			 ON CONFLICT (local_root, remote_root) DO UPDATE SET mode = excluded.mode",
			params![local_root, remote_root, mode.as_i64()],
		)?;
		self.conn.query_row(
			"SELECT id FROM sync_pairs WHERE local_root = ?1 AND remote_root = ?2",
			params![local_root, remote_root],
			|row| row.get(0),
		)
	}

	pub(crate) fn pair(&self, id: PairId) -> rusqlite::Result<Option<PairRecord>> {
		self.conn
			.query_row(
				"SELECT id, local_root, remote_root, mode FROM sync_pairs WHERE id = ?1",
				params![id],
				Self::row_to_pair,
			)
			.optional()
	}

	pub(crate) fn list_pairs(&self) -> rusqlite::Result<Vec<PairRecord>> {
		self.conn
			.prepare("SELECT id, local_root, remote_root, mode FROM sync_pairs ORDER BY id")?
			.query_map([], Self::row_to_pair)?
			.collect()
	}

	pub(crate) fn delete_pair(&self, id: PairId) -> rusqlite::Result<()> {
		// The `ON DELETE CASCADE` (with `foreign_keys = ON`) drops the pair's baseline rows.
		self.conn
			.execute("DELETE FROM sync_pairs WHERE id = ?1", params![id])?;
		Ok(())
	}

	fn row_to_pair(row: &Row<'_>) -> rusqlite::Result<PairRecord> {
		let mode_raw: i64 = row.get("mode")?;
		Ok(PairRecord {
			id: row.get("id")?,
			local_root: row.get("local_root")?,
			remote_root: row.get("remote_root")?,
			mode: SyncMode::from_i64(mode_raw).ok_or_else(|| corrupt("mode", mode_raw))?,
		})
	}

	/// Insert or replace one baseline row.
	pub(crate) fn upsert_entry(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT OR REPLACE INTO baseline
			 (pair_id, rel_path, kind, remote_uuid, content_hash, size, local_mtime,
			  remote_modified, state, local_kind, remote_kind, remote_hash, remote_size)
			 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
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
				        remote_modified, state, local_kind, remote_kind, remote_hash, remote_size
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
				        remote_modified, state, local_kind, remote_kind, remote_hash, remote_size
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
		for change in changes {
			match change {
				BaselineChange::Upsert(entry) => self.upsert_entry(pair, entry)?,
				BaselineChange::Delete(rel_path) => self.delete_entry(pair, rel_path)?,
			}
		}
		tx.commit()
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

	#[test]
	fn a_newer_than_supported_db_is_refused_not_degraded() {
		let path = std::env::temp_dir().join(format!("filen_baseline_v2_{}.db", Uuid::new_v4()));
		{
			let store = BaselineStore::open(&path).unwrap();
			store
				.conn
				.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
				.unwrap();
		}
		let message = match BaselineStore::open(&path) {
			Ok(_) => panic!("a newer schema must be refused"),
			Err(error) => error.to_string(),
		};
		assert!(
			message.contains("schema version") && message.contains("refusing"),
			"the refusal must name the version problem: {message}"
		);
		// Fails CLOSED: the newer DB is left exactly as it was, not downgraded in place.
		let raw = Connection::open(&path).unwrap();
		let still: i64 = raw
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.unwrap();
		assert_eq!(still, SCHEMA_VERSION + 1, "the refused DB was rewritten");
		drop(raw);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn a_pre_versioning_db_is_migrated_forward_in_place() {
		let path = std::env::temp_dir().join(format!("filen_baseline_v0_{}.db", Uuid::new_v4()));
		// A v0 DB: the pre-versioning schema (no per-side conflict-evidence columns, user_version 0)
		// carrying one row that must survive the migration.
		{
			let conn = Connection::open(&path).unwrap();
			conn.execute_batch(
				"CREATE TABLE sync_pairs (
					id INTEGER PRIMARY KEY,
					local_root TEXT NOT NULL,
					remote_root BLOB NOT NULL,
					mode INTEGER NOT NULL,
					UNIQUE (local_root, remote_root)
				);
				CREATE TABLE baseline (
					pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
					rel_path TEXT NOT NULL,
					kind INTEGER NOT NULL,
					remote_uuid BLOB,
					content_hash BLOB,
					size INTEGER,
					local_mtime INTEGER,
					remote_modified INTEGER,
					state INTEGER NOT NULL,
					PRIMARY KEY (pair_id, rel_path)
				);
				INSERT INTO sync_pairs (id, local_root, remote_root, mode)
					VALUES (1, '/old/root', X'00000000000000000000000000000001', 0);
				INSERT INTO baseline (pair_id, rel_path, kind, size, state)
					VALUES (1, 'kept.txt', 2, 7, 0);",
			)
			.unwrap();
		}

		let store = BaselineStore::open(&path).unwrap();
		assert_eq!(version_of(&store), SCHEMA_VERSION, "stamped in place");
		let entry = store.entry(1, "kept.txt").unwrap().expect("row survived");
		assert_eq!(entry.size, Some(7));
		assert_eq!(entry.local_kind, None, "the new columns read back as unset");
		assert_eq!(store.list_pairs().unwrap().len(), 1, "pair survived");
		drop(store);

		// Re-opening the migrated DB is a clean no-op (the migration runs exactly once).
		let again = BaselineStore::open(&path).unwrap();
		assert_eq!(version_of(&again), SCHEMA_VERSION);
		assert!(again.entry(1, "kept.txt").unwrap().is_some());
		drop(again);
		std::fs::remove_file(&path).ok();
	}

	#[test]
	fn create_pair_is_idempotent_and_round_trips() {
		let store = BaselineStore::open_in_memory().unwrap();
		let remote = Uuid::new_v4();
		let id = store
			.create_pair("/home/u/sync", remote, SyncMode::TwoWay)
			.unwrap();

		// Same (local_root, remote_root) returns the same id and updates the mode in place.
		let again = store
			.create_pair("/home/u/sync", remote, SyncMode::LocalToRemote)
			.unwrap();
		assert_eq!(id, again, "re-registering a pair returns the same id");

		let record = store.pair(id).unwrap().expect("pair exists");
		assert_eq!(record.local_root, "/home/u/sync");
		assert_eq!(record.remote_root, remote);
		assert_eq!(record.mode, SyncMode::LocalToRemote, "mode was updated");
		assert_eq!(store.list_pairs().unwrap().len(), 1);
	}

	#[test]
	fn entries_round_trip_with_full_fidelity() {
		let store = BaselineStore::open_in_memory().unwrap();
		let pair = store
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
		let pair = store
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
		let pair = store
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
			let pair = store
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
		let pair = store
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
		let pair = store
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
		let pair = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let other = store
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
		let pair = store
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

	/// The first real migration: a v1 DB (baseline rows, no journal) gains the table and keeps
	/// everything it had.
	#[test]
	fn a_v1_db_is_migrated_to_v2_keeping_its_rows() {
		let path = temp_db_path("v1");
		{
			let conn = Connection::open(&path).unwrap();
			conn.execute_batch(SCHEMA).unwrap();
			conn.execute_batch(
				"INSERT INTO sync_pairs (id, local_root, remote_root, mode)
					VALUES (1, '/old/root', X'00000000000000000000000000000001', 0);
				INSERT INTO baseline (pair_id, rel_path, kind, size, state)
					VALUES (1, 'kept.txt', 2, 7, 0);
				PRAGMA user_version = 1;",
			)
			.unwrap();
		}

		let store = BaselineStore::open(&path).unwrap();
		assert_eq!(version_of(&store), SCHEMA_VERSION, "stamped in place");
		assert_eq!(store.entry(1, "kept.txt").unwrap().unwrap().size, Some(7));
		assert_eq!(store.list_pairs().unwrap().len(), 1, "pair survived");
		// The journal the migration added is usable at once.
		store
			.record_pending(1, Uuid::new_v4(), &created("a.txt"), NOW, &[])
			.unwrap();
		assert_eq!(store.load_pending(NOW, GRACE).unwrap().len(), 1);
		drop(store);
		std::fs::remove_file(&path).ok();
	}
}
