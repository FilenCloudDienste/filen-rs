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

use super::mode::SyncMode;

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

impl BaselineStore {
	/// Open (creating if needed) the baseline DB at `path`.
	pub(crate) fn open(path: &Path) -> rusqlite::Result<Self> {
		Self::init(Connection::open(path)?)
	}

	#[cfg(test)]
	pub(crate) fn open_in_memory() -> rusqlite::Result<Self> {
		Self::init(Connection::open_in_memory()?)
	}

	fn init(conn: Connection) -> rusqlite::Result<Self> {
		conn.execute_batch("PRAGMA foreign_keys = ON;")?;
		conn.execute_batch(SCHEMA)?;
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

	// Pair-management API completed by the store and exercised by its unit tests; not yet called by
	// the engine, which currently registers pairs but never lists/removes them.
	#[allow(dead_code)]
	pub(crate) fn list_pairs(&self) -> rusqlite::Result<Vec<PairRecord>> {
		self.conn
			.prepare("SELECT id, local_root, remote_root, mode FROM sync_pairs ORDER BY id")?
			.query_map([], Self::row_to_pair)?
			.collect()
	}

	#[allow(dead_code)]
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

	// Single-row lookup; the engine reads whole-pair snapshots via `entries`, but the point lookup
	// is part of the store's query surface and is exercised by the unit tests.
	#[allow(dead_code)]
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
}
