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

use std::{
	collections::{BTreeSet, HashMap},
	fmt::Display,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, PoisonError},
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use uuid::Uuid;

use crate::cache::open_sqlite;
// Only the test rows connection opens an in-memory database.
#[cfg(test)]
use crate::cache::open_sqlite_in_memory;

use super::{
	engine::{BaselineSchemaMismatch, PendingKind},
	guard::DeleteGuard,
	ignore::FILENIGNORE,
	mode::SyncMode,
	rows::{Baseline, Snapshot},
	scan::collision_key,
};

/// The schema version this build writes and understands, stamped into `PRAGMA user_version`.
///
/// There is no migration chain and no released schema to migrate FROM: a DB stamped at anything
/// else — older or newer — was not written by this engine, and reading it under these rules would
/// misread its rows into deletes. Both directions are refused (see [`BaselineStore::init`]); the
/// first released schema is what a migration path would start from.
///
/// Moved to 2 when a `pending_writes` row of kind [`KIND_TRASHED`] gained a required `path`: a
/// version-1 DB wrote that column NULL there, which [`BaselineStore::row_to_pending`] now refuses,
/// and one such row fails the whole journal read and with it the engine's construction. A row's
/// MEANING changing is exactly what this constant is for — the refusal above says what to do about
/// the file, where the journal read would only say that a column was NULL.
///
/// Moved to 3 when a pass stopped reading the rows out of a resident copy and began reading them
/// out of the table itself (see [`rows`](super::rows)): the table gained the columns and indexes
/// those reads seek, and the counts table the triggers keep. A version-2 file has none of them, so
/// every one of those reads would fail on it — or, where a column happened to exist, read it as
/// something it never recorded.
///
/// Moved to 4 when a pair began carrying the last synced state of the filen-sync pair it replaced
/// (`legacy_synced`, see [`SCHEMA`] and [`SyncEngine::import_legacy_state`]). A version-3 file has
/// no table for it, so the import, and every pass that weighs what it imported, would fail on it.
///
/// Moved to 5 when a pair began recording whether a pass of it has completed cleanly
/// (`sync_pairs.first_pass_completed`, see [`PairRecord::first_pass_completed`]): from then on a
/// pass is a first sync until that is set, as well as whenever the pair has no rows, which alone
/// made one before. A version-4 file has no column for it, so every read of its pairs would fail
/// on it.
///
/// [`SyncEngine::import_legacy_state`]: super::SyncEngine::import_legacy_state
const SCHEMA_VERSION: i64 = 5;

/// Schema for the baseline DB, created whole on a fresh DB. `foreign_keys`, `synchronous` and the
/// busy timeout are applied per-connection in [`BaselineStore::init`] (they reset on every open);
/// the WAL journal mode set there persists in the file itself.
///
/// `pending_writes` is the journal of remote writes this engine has made that the cache has not
/// announced yet (see [`PendingWrites`](super::engine::PendingWrites)), keyed by uuid like the
/// in-memory journal it mirrors. `path_failures` counts how many times in a row applying one path
/// has failed, what the last failure said, and when it happened (unix millis) — the time is what
/// lets an exhausted streak expire and be retried. Both cascade with their pair.
///
/// `user_ignore` holds the device-wide user ignore patterns: one row at most, shared by every pair,
/// so it cascades with nothing.
///
/// `legacy_synced` is what the filen-sync pair a pair replaced last recorded as synced, imported
/// from that engine's state files (see [`legacy`](super::legacy)): per path, the remote version it
/// last listed there and the size and mtime (unix millis) the local file had then. It is evidence
/// for a path with no `baseline` row and nothing else; it cascades with its pair, and the
/// `AUTOINCREMENT` id below means it can never attach to a later pair.
///
/// `sync_pairs.id` is `AUTOINCREMENT` for one reason: a removed pair's id must never come back. A
/// watch loop finishes the pass it is in when its pair is removed, and that pass goes on writing
/// baseline and journal rows keyed by the id it started with — which a plain `INTEGER PRIMARY KEY`
/// hands straight to the next pair created, whose first sync is then reconciled against a stranger's
/// rows. With the id retired those writes hit the foreign key and fail, which is what they should do.
///
/// A pass reads its rows out of `baseline` itself (see [`rows`](super::rows)), so the table carries
/// what those reads seek on, each column computed in Rust by [`Derived::of`] from the row it is
/// written with — never by SQL, which could not agree with the engine's own rules:
///
/// - `folded_path` is [`collision_key`](super::scan::collision_key) of `rel_path`:
///   `char::to_lowercase` over the whole path. SQLite's `lower()` folds ASCII only, and the Ä/ä,
///   final-sigma and dotted-İ cases are exactly where the two differ. Indexed with `rel_path` so a
///   folded at-or-under question is a range over it and every row of it comes back in one order.
/// - `carryable` is [`BaselineEntry::carryable`], `rule_file` whether the row IS a `.filenignore`,
///   and `unconfirmed` [`BaselineEntry::awaits_confirmation`]. Each has a partial index, because
///   each is a whole-set question a pass asks before it reads either side and whose answer on a
///   converged pair is empty: the index is the set, and costs nothing while the set is.
/// - `baseline_dirs` holds the directory rows alone: a whole pass looks for the source of a
///   directory move among them, once per move it folds, and they are a few per cent of the rows.
///
/// `baseline` is a `WITHOUT ROWID` table: its rows live IN the primary key's b-tree, in
/// `(pair_id, rel_path)` order. A pass reads its rows by path — point lookups and ranges of that
/// key — and in a rowid table every row of a range is an index step and then a seek into a second
/// b-tree for the row itself; clustered, a range is a walk of consecutive leaves and a point lookup
/// one descent. SQLite advises the layout for rows under a twentieth of a page, which a row here
/// (two paths, three hashes, two uuids) passes only at [`PAGE_SIZE`].
///
/// `baseline_counts` is each pair's row count, and how many of those rows are uncarryable and how
/// many record a remote item — kept by the three triggers below on every insert, delete and
/// change of those two columns, so the whole-set counts a pass asks for are one row read and never
/// a `COUNT(*)`, which is a walk of the pair. The triggers count a row that `INSERT OR REPLACE` or
/// `UPDATE OR REPLACE` deletes to make room only because every connection turns
/// `recursive_triggers` on ([`BaselineStore::init`]): without it SQLite fires no delete trigger for
/// a row REPLACE removes, and the count would drift up by one per overwrite.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sync_pairs (
	id INTEGER PRIMARY KEY AUTOINCREMENT,
	local_root TEXT NOT NULL,
	remote_root BLOB NOT NULL,
	mode INTEGER NOT NULL,
	guard_floor INTEGER,
	guard_ratio REAL,
	paused INTEGER NOT NULL DEFAULT 0,
	first_pass_completed INTEGER NOT NULL DEFAULT 0,
	UNIQUE (local_root, remote_root)
);

CREATE TABLE IF NOT EXISTS baseline (
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rel_path TEXT NOT NULL,
	folded_path TEXT NOT NULL,
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
	carryable INTEGER NOT NULL,
	rule_file INTEGER NOT NULL,
	unconfirmed INTEGER NOT NULL,
	PRIMARY KEY (pair_id, rel_path)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS baseline_folded ON baseline (pair_id, folded_path, rel_path);
CREATE INDEX IF NOT EXISTS baseline_uncarryable ON baseline (pair_id, rel_path)
	WHERE carryable = 0;
CREATE INDEX IF NOT EXISTS baseline_rule_files ON baseline (pair_id, rel_path)
	WHERE rule_file = 1;
CREATE INDEX IF NOT EXISTS baseline_unconfirmed ON baseline (pair_id, rel_path)
	WHERE unconfirmed = 1;
CREATE INDEX IF NOT EXISTS baseline_dirs ON baseline (pair_id, rel_path) WHERE kind = 1;

CREATE TABLE IF NOT EXISTS baseline_counts (
	pair_id INTEGER PRIMARY KEY REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rows INTEGER NOT NULL,
	uncarryable INTEGER NOT NULL,
	remote_rows INTEGER NOT NULL
);

CREATE TRIGGER IF NOT EXISTS baseline_counted AFTER INSERT ON baseline BEGIN
	INSERT INTO baseline_counts (pair_id, rows, uncarryable, remote_rows)
	VALUES (NEW.pair_id, 1, NEW.carryable = 0, NEW.remote_uuid IS NOT NULL)
	ON CONFLICT (pair_id) DO UPDATE SET
		rows = rows + 1,
		uncarryable = uncarryable + (NEW.carryable = 0),
		remote_rows = remote_rows + (NEW.remote_uuid IS NOT NULL);
END;

CREATE TRIGGER IF NOT EXISTS baseline_uncounted AFTER DELETE ON baseline BEGIN
	UPDATE baseline_counts SET
		rows = rows - 1,
		uncarryable = uncarryable - (OLD.carryable = 0),
		remote_rows = remote_rows - (OLD.remote_uuid IS NOT NULL)
	WHERE pair_id = OLD.pair_id;
END;

CREATE TRIGGER IF NOT EXISTS baseline_recounted
	AFTER UPDATE OF carryable, remote_uuid ON baseline BEGIN
	UPDATE baseline_counts SET
		uncarryable = uncarryable - (OLD.carryable = 0) + (NEW.carryable = 0),
		remote_rows = remote_rows - (OLD.remote_uuid IS NOT NULL) + (NEW.remote_uuid IS NOT NULL)
	WHERE pair_id = NEW.pair_id;
END;

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
	last_failure_at INTEGER NOT NULL,
	PRIMARY KEY (pair_id, rel_path)
);

CREATE TABLE IF NOT EXISTS user_ignore (
	id INTEGER PRIMARY KEY CHECK (id = 0),
	patterns TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ignored_roots (
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rel_path TEXT NOT NULL,
	PRIMARY KEY (pair_id, rel_path)
);

CREATE TABLE IF NOT EXISTS legacy_synced (
	pair_id INTEGER NOT NULL REFERENCES sync_pairs (id) ON DELETE CASCADE,
	rel_path TEXT NOT NULL,
	remote_uuid BLOB NOT NULL,
	size INTEGER NOT NULL,
	local_mtime INTEGER NOT NULL,
	PRIMARY KEY (pair_id, rel_path)
) WITHOUT ROWID;
";

/// Objects added to the schema after version 1 was first written, created on EVERY open rather
/// than only on a fresh DB: [`SCHEMA`] runs once, when the file holds no table of ours, so an index
/// added to it alone would never reach a DB that already exists. `IF NOT EXISTS` makes the re-run
/// free, and an index changes no row's MEANING, so [`SCHEMA_VERSION`] does not move — an older
/// build opening the file just plans its own reads without it.
///
/// `baseline_state` serves the conflict read ([`BaselineStore::conflicts`]): the rows a pair
/// holds in conflict are a handful out of a whole tree, and finding them without the index is a
/// walk of every row of the pair — a walk that runs under the pair's store mutex. It carries
/// `rel_path` as well so that a pass's "is every row under this directory synced" is one seek per
/// unsynced state bounded to the directory's range, rather than a visit of every unsynced row of
/// the pair (see [`rows`](super::rows)).
const ADDITIVE_SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS baseline_state ON baseline (pair_id, state, rel_path);
CREATE INDEX IF NOT EXISTS baseline_remote_uuid ON baseline (pair_id, remote_uuid);
CREATE INDEX IF NOT EXISTS baseline_remote_stable_uuid ON baseline (pair_id, remote_stable_uuid);
";

/// The columns [`BaselineStore::row_to_entry`] reads, named once so the statements that hand back
/// rows cannot drift apart from each other or from it.
pub(super) const ENTRY_COLUMNS: &str =
	"rel_path, kind, remote_uuid, content_hash, size, local_mtime,
	 remote_modified, state, local_kind, remote_kind, remote_hash, remote_size,
	 remote_stable_uuid, agreed_hash";

/// The two statements [`BaselineStore::delete_subtrees`] runs per root: the row AT the path, and
/// the rows UNDER it.
///
/// Two seeking statements rather than the one `substr(rel_path, 1, length(?2) + 1) = ?2 || '/'`
/// predicate they replaced: a `substr` of the indexed column cannot seek, so that form scanned
/// every row of the pair once per root — G × N per pass, the largest single term of a pass on a
/// tree whose directories each hold a default-ignored file. The range is exact, not approximate:
/// `/` is 0x2F and `0` is 0x30, and TEXT compares bytewise under the default BINARY collation, so
/// `?2 || '/' < rel_path < ?2 || '0'` is precisely "under `?2`".
const DELETE_AT_PATH: &str = "DELETE FROM baseline WHERE pair_id = ?1 AND rel_path = ?2";
const DELETE_UNDER_PATH: &str =
	"DELETE FROM baseline WHERE pair_id = ?1 AND rel_path > ?2 || '/' AND rel_path < ?2 || '0'";

/// The two row statements a pass runs most: one per applied action, and one per row of the tree in
/// the single transactions a first sync's confirmation and a subtree re-key make.
///
/// Named so they can go through [`prepare_cached`](rusqlite::Connection::prepare_cached), which
/// keys its cache on the statement text. `execute` parses and plans afresh on every call, and a
/// 15-parameter upsert is not free to plan: a first sync pays that once per item of the tree, and
/// the one transaction over a million rows pays it a million times.
const UPSERT_ENTRY: &str = "INSERT OR REPLACE INTO baseline
	 (pair_id, rel_path, kind, remote_uuid, content_hash, size, local_mtime,
	  remote_modified, state, local_kind, remote_kind, remote_hash, remote_size,
	  remote_stable_uuid, agreed_hash, folded_path, carryable, rule_file, unconfirmed)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
	  ?19)";
const DELETE_ENTRY: &str = "DELETE FROM baseline WHERE pair_id = ?1 AND rel_path = ?2";

/// The two tables [`BaselineStore::replace_legacy_synced`] stages a filen-sync pair's trees in.
/// `TEMP`, so they are this connection's alone and live in SQLite's temporary database rather than
/// the baseline file: filling them takes no lock another pair's writes wait on, however large the
/// trees are. Dropped first, in case a failed drop left one behind.
const LEGACY_STAGE: &str = "
DROP TABLE IF EXISTS temp.legacy_local;
DROP TABLE IF EXISTS temp.legacy_remote;
CREATE TEMP TABLE legacy_local (
	rel_path TEXT PRIMARY KEY,
	raw TEXT NOT NULL,
	is_file INTEGER NOT NULL,
	size INTEGER,
	mtime INTEGER,
	ambiguous INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
CREATE TEMP TABLE legacy_remote (
	rel_path TEXT PRIMARY KEY,
	raw TEXT NOT NULL,
	is_file INTEGER NOT NULL,
	size INTEGER,
	uuid BLOB,
	ambiguous INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;
";

const LEGACY_UNSTAGE: &str = "
DROP TABLE IF EXISTS temp.legacy_local;
DROP TABLE IF EXISTS temp.legacy_remote;
";

/// One item into its tree's stage. The same raw path again replaces the item, as the old reader
/// let the last line for a path win; a different raw path at the same NFC path marks it ambiguous
/// for good. Every `SET` expression reads the row as it was before the update, so `ambiguous`
/// compares the spelling already held with the new one.
const LEGACY_PUT_LOCAL: &str = "INSERT INTO temp.legacy_local (rel_path, raw, is_file, size, mtime)
	 VALUES (?1, ?2, ?3, ?4, ?5)
	 ON CONFLICT (rel_path) DO UPDATE SET
	  ambiguous = ambiguous OR raw <> excluded.raw,
	  raw = excluded.raw, is_file = excluded.is_file, size = excluded.size, mtime = excluded.mtime";
const LEGACY_PUT_REMOTE: &str =
	"INSERT INTO temp.legacy_remote (rel_path, raw, is_file, size, uuid)
	 VALUES (?1, ?2, ?3, ?4, ?5)
	 ON CONFLICT (rel_path) DO UPDATE SET
	  ambiguous = ambiguous OR raw <> excluded.raw,
	  raw = excluded.raw, is_file = excluded.is_file, size = excluded.size, uuid = excluded.uuid";

/// The staged trees joined into `legacy_synced`: a path both trees list as the same file, the
/// remote with a version and the local with an mtime, at the same size in both. That both trees
/// agree is what "the old engine recorded it as synced" is read as here — stricter than any one
/// tree, never looser, so a path either tree is unsure of imports nothing.
///
/// The two trees also have to spell the path the same way, byte for byte. The old engine keyed and
/// compared both by the raw path, so a local NFD `café.txt` and a remote NFC one were two paths to
/// it, never one it had synced; the NFC key alone would pair them.
const LEGACY_JOIN: &str =
	"INSERT INTO legacy_synced (pair_id, rel_path, remote_uuid, size, local_mtime)
	 SELECT ?1, r.rel_path, r.uuid, r.size, l.mtime
	 FROM temp.legacy_remote r JOIN temp.legacy_local l ON l.rel_path = r.rel_path
	 WHERE l.raw = r.raw
	  AND r.is_file = 1 AND l.is_file = 1 AND r.ambiguous = 0 AND l.ambiguous = 0
	  AND r.uuid IS NOT NULL AND l.mtime IS NOT NULL AND r.size = l.size";

/// The id seek [`BaselineStore::synced_paths`] runs against `column`: one `IN` list of exactly
/// [`SYNCED_CHUNK`] placeholders, so the text is the same on every call and `prepare_cached` plans
/// it once. Two texts exist, one per indexed column, and building either is a handful of pushes
/// against the two statements a pass runs at most.
fn synced_by(column: &str) -> String {
	let binds = (2..=SYNCED_CHUNK + 1)
		.map(|at| format!("?{at}"))
		.collect::<Vec<_>>()
		.join(", ");
	// Named for the reason `conflict_rows_sql` gives: without statistics the planner would walk
	// the clustered primary key on `pair_id` instead.
	format!(
		"SELECT rel_path, remote_uuid, remote_stable_uuid FROM baseline INDEXED BY baseline_{column}
		 WHERE pair_id = ?1 AND {column} IN ({binds})"
	)
}

/// The two statements a directory move runs, in this order: the row AT `?2`, then every row under
/// it, each swapping that prefix for `?3` over the same bytewise ranges the deletes above use.
///
/// Two statements for the same reason the deletes are two. Asking one `UPDATE` for both — `rel_path
/// = ?2 OR (rel_path > ?2 || '/' AND rel_path < ?2 || '0')` — plans as `SEARCH baseline USING INDEX
/// PRIMARY KEY (pair_id=?)`: an `OR` of a point and a range is not sargable, so it
/// walks every row of the pair, which is the cost the whole-pair read it replaced had.
///
/// They read the STORE rather than the pass's baseline, which is deliberate: the plan re-keys its
/// own baseline for every directory move of the pass at once, so for a move with another nested
/// inside it, that copy already names the inner directory by the path the INNER move gives it.
/// Taken from the store, each move commits its own step only, and an inner move that then fails
/// leaves the rows where the directory really is.
///
/// Neither can collide with itself: the sources all lie under `?2`, the destinations under `?3`,
/// and a case-only rename has no rows at the new spelling because the key compares bytewise. They
/// can collide with rows that were ALREADY at the destination, and `OR REPLACE` answers that the
/// way the read-delete-upsert they replaced did — by overwriting. `plan::dir_move_from` refuses a
/// move onto an occupied destination, but `plan::next_case_only_dir_rename` in a pushing mode does
/// not check the local end, so rows under both spellings reach here; the directory has been renamed
/// on disk by then, and failing on the primary key would leave the store holding it under two names.
///
/// Each also carries the columns a key change moves with it: `folded_path` the same prefix swap in
/// its folded form (`?4` is `?2` folded, `?5` is `?3` folded — folding is per character, so the
/// folded path of `to/rest` IS the folded `to` followed by the folded `rest`, and `length` and
/// `substr` count characters on both), and `rule_file` for the one row whose LEAF the move
/// renames (`?6`: whether the new leaf is a `.filenignore`). A row under the source keeps its leaf,
/// so it keeps its flag.
const MOVE_AT_PATH: &str = "UPDATE OR REPLACE baseline
	 SET rel_path = ?3, folded_path = ?5, rule_file = (kind = 2 AND ?6)
	 WHERE pair_id = ?1 AND rel_path = ?2";
const MOVE_UNDER_PATH: &str = "UPDATE OR REPLACE baseline
	 SET rel_path = ?3 || substr(rel_path, length(?2) + 1),
	  folded_path = ?5 || substr(folded_path, length(?4) + 1)
	 WHERE pair_id = ?1 AND rel_path > ?2 || '/' AND rel_path < ?2 || '0'";

/// Write one row with the columns [`Derived`] computes from it — the statement every write of a row
/// goes through, on the store's connection and on the one a test builds its rows in alike.
pub(super) fn upsert_row(
	conn: &Connection,
	pair: PairId,
	entry: &BaselineEntry,
) -> rusqlite::Result<()> {
	let derived = Derived::of(entry);
	conn.prepare_cached(UPSERT_ENTRY)?.execute(params![
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
		derived.folded_path,
		derived.carryable,
		derived.rule_file,
		derived.unconfirmed,
	])?;
	Ok(())
}

/// `pending_writes.kind` discriminants — what the write did.
const KIND_CREATED: i64 = 1;
const KIND_MOVED: i64 = 2;
const KIND_TRASHED: i64 = 3;

/// One path's live failure streak, as `path_failures` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathFailure {
	/// How many attempts in a row failed.
	pub(crate) attempts: u32,
	/// What the most recent one said.
	pub(crate) last_error: String,
	/// When the most recent one happened, in unix millis.
	pub(crate) last_failure_at: i64,
}

/// Whether a baseline row describes a directory or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NodeKind {
	Dir,
	File,
}

impl NodeKind {
	pub(super) const fn as_i64(self) -> i64 {
		match self {
			Self::Dir => 1,
			Self::File => 2,
		}
	}

	pub(super) fn from_i64(value: i64) -> Option<Self> {
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
	pub(super) const fn as_i64(self) -> i64 {
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

impl BaselineEntry {
	/// Whether this row stands in for BOTH sides of a pass — the one rule that says what a
	/// change-scoped pass may carry forward instead of re-reading.
	///
	/// It lives here, on the row it is about, because THREE readers need the same answer and a
	/// second copy of the field list is how a new column comes to be carried by one of them and not
	/// another: [`derive::carried`](super::derive) builds the two nodes from a row that passes,
	/// the store writes it into the row's `carryable` column so a pass can find every row that fails
	/// through a partial index rather than a walk, and a carried side answers "does this side hold
	/// the path" straight off that column without building a node at all.
	///
	/// A row that fails this is not a defect. It is one the engine wrote one-sided on purpose — a
	/// `KeepLocal`/`KeepRemote` resolution that cleared a half, an [`Adopted`](BaselineState::Adopted)
	/// row, a divergence whose two halves describe a disagreement rather than an agreement — and a
	/// pass re-observes it rather than deriving either side from it.
	pub(crate) fn carryable(&self) -> bool {
		// Anything but `Synced` describes a divergence or a standing one-sided copy, so neither side
		// may be derived from it.
		self.state == BaselineState::Synced
			// Nothing on record for the remote item: there is no remote node to build.
			&& self.remote_uuid.is_some()
			&& match self.kind {
				// A directory has no content and no stamp any part of a pass reads.
				NodeKind::Dir => true,
				// A file's every field IS read — the hash classifies it, `(size, mtime)` is the
				// scanner's fast path, and the remote stamp is written into the rows an adopt writes.
				// A row missing any of them describes no side fully, so it describes neither.
				NodeKind::File => {
					self.content_hash.is_some()
						&& self.size.is_some()
						&& self.local_mtime.is_some()
						&& self.remote_modified.is_some()
				}
			}
	}

	/// Whether this row awaits confirmation of a push of ours: this side's content is on record and
	/// is not what the two sides last agreed on (see [`agreed_hash`](Self::agreed_hash)).
	pub(crate) fn awaits_confirmation(&self) -> bool {
		self.kind == NodeKind::File
			&& self.state == BaselineState::Synced
			&& self.content_hash.is_some()
			&& self.agreed_hash != self.content_hash
	}

	/// Whether this row IS a `.filenignore`: a file whose leaf name is exactly that.
	pub(crate) fn is_rule_file(&self) -> bool {
		self.kind == NodeKind::File && self.rel_path.rsplit('/').next() == Some(FILENIGNORE)
	}
}

/// The columns a row is written with beyond its own fields (see [`SCHEMA`]), computed here from the
/// row — the one place each rule lives — and never by SQL.
///
/// Computed ONCE, when the row is written, and trusted by every read after: nothing recomputes them
/// on open. So each rule is part of what a stored row MEANS, and changing one —
/// [`BaselineEntry::carryable`], [`BaselineEntry::is_rule_file`],
/// [`BaselineEntry::awaits_confirmation`], or [`collision_key`], whose `char::to_lowercase` is the
/// toolchain's Unicode tables — leaves every row written before the change answering by the old
/// rule: a row the new carry rule rejects still carried, a spelling the new tables fold together
/// missed by the collision check. Such a change bumps [`SCHEMA_VERSION`], so that a file written
/// under the old rules is refused rather than misread; `the_derived_columns_are_pinned_to_the_schema_version`
/// fails on each until it is.
pub(super) struct Derived {
	pub(super) folded_path: String,
	pub(super) carryable: bool,
	pub(super) rule_file: bool,
	pub(super) unconfirmed: bool,
}

impl Derived {
	pub(super) fn of(entry: &BaselineEntry) -> Self {
		Self {
			folded_path: collision_key(&entry.rel_path),
			carryable: entry.carryable(),
			rule_file: entry.is_rule_file(),
			unconfirmed: entry.awaits_confirmation(),
		}
	}
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
	/// A directory move: the row at `from` and every row under it are re-keyed to sit under `to`,
	/// in two seeking statements over the rows the store holds (see [`MOVE_AT_PATH`]).
	MoveSubtree {
		from: &'a str,
		to: &'a str,
	},
}

/// What drops one baseline row inside a commit, by its path: the visitor the commit of a directory
/// deletion hands the rows of the deletions it carried out along with its own
/// ([`BaselineStore::apply_changes_carrying`], [`BaselineStore::record_pending_carrying`]). They are
/// streamed through it rather than collected first: the deletion of the tree's top directory
/// carries a row per item of the tree.
pub(crate) type DropRow<'a> = dyn FnMut(&str) -> rusqlite::Result<()> + 'a;

/// What writes one baseline row inside a commit: the visitor [`BaselineStore::upsert_streamed`]
/// hands the rows of a pass's confirmations, a row per push the pass confirmed — on the pass after a
/// whole-tree push, a row per item of the tree.
pub(crate) type PutRow<'a> = dyn FnMut(&BaselineEntry) -> rusqlite::Result<()> + 'a;

/// One item of a filen-sync tree, as [`BaselineStore::replace_legacy_synced`] stages it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacyItem<'a> {
	/// The NFC path the item is at, keyed as the scan keys a local path.
	pub(crate) rel_path: &'a str,
	/// The path exactly as the tree spelled it. Two spellings of one `rel_path` (an NFC and an NFD
	/// name, say) make the path ambiguous, and an ambiguous path is imported from neither.
	pub(crate) raw: &'a str,
	pub(crate) is_file: bool,
	/// Bytes.
	pub(crate) size: Option<i64>,
	pub(crate) side: LegacySide,
}

/// Which of the two filen-sync trees a [`LegacyItem`] is from, with what only that tree records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacySide {
	/// `previousLocalTree`: the local file's mtime as that engine last saw it, in unix millis.
	Local { mtime: Option<i64> },
	/// `previousRemoteTree`: the remote version that engine last listed at the path.
	Remote { uuid: Option<Uuid> },
}

/// What a pair's imported filen-sync state recorded as synced at one path: the remote version that
/// engine last listed there, and the local file as it last saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacySynced {
	pub(crate) remote_uuid: Uuid,
	/// Bytes.
	pub(crate) size: u64,
	/// Unix millis.
	pub(crate) local_mtime: i64,
}

/// What stages one item of a filen-sync tree: the visitor
/// [`BaselineStore::replace_legacy_synced`] hands its caller, which streams a tree of any size
/// through it a line at a time.
pub(crate) type PutLegacy<'a> = dyn FnMut(&LegacyItem<'_>) -> rusqlite::Result<()> + 'a;

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
	/// Whether a pass of the pair has completed cleanly: it read both sides whole and could trust
	/// what each said was absent, nothing failed, refused, cancelled or interrupted it, and the
	/// guard held none of its deletions. Until one has, every pass is a first sync and the guard
	/// holds its deletions for approval, however many rows earlier passes left behind; so is a pass
	/// of a pair that tracks no row, whatever this says. Set by the engine, never by a caller.
	pub first_pass_completed: bool,
}

/// A registered pair's id, handed back by [`SyncEngine::add_pair`](super::SyncEngine::add_pair).
pub type PairId = i64;

/// Where the baseline records each of the remote items a pass is about to ask about.
///
/// This replaces two resident `HashMap`s the tree used to carry for the life of a pair — one
/// `Uuid -> row`, one `StableUuid -> row` — which at a million rows cost 73.5 MiB, a third of the
/// whole resident tree, to answer a handful of point lookups per pass. The same question is now a
/// seek of the store's own `(pair_id, remote_uuid)` index, asked ONCE for every id the pass can
/// name before it reads either side (see [`BaselineStore::synced_paths`]).
///
/// It distinguishes "asked, and no row records it" from "never asked": a pass that looked up an id
/// it did not resolve would read the answer as an absence, and an absence this side did not observe
/// is the one thing a change-scoped pass must never invent. So an unasked id is a BUG, and
/// [`path_by_uuid`](Self::path_by_uuid) says so rather than answering `None` quietly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SyncedPaths {
	by_uuid: HashMap<Uuid, Option<String>>,
	by_lineage: HashMap<StableUuid, Option<String>>,
}

impl SyncedPaths {
	/// The set of ids to resolve, every one of them still unanswered.
	pub(crate) fn asking(uuids: &[Uuid], lineages: &[StableUuid]) -> Self {
		Self {
			by_uuid: uuids.iter().map(|&uuid| (uuid, None)).collect(),
			by_lineage: lineages.iter().map(|&id| (id, None)).collect(),
		}
	}

	/// Record one row against whichever of the asked ids it carries.
	///
	/// The lexicographically first path wins where two rows claim one id. The store constrains
	/// neither id and no writer produces a duplicate — a move commits the delete of its source and
	/// the insert of its destination together — so a second claim is a bug and is logged. What it
	/// is NOT is the old map's behaviour: that was last-writer-wins over the load order, which
	/// could leave the id answering for whichever row happened to be rewritten last, or for
	/// nothing at all once that row lost the id. A seek finds every claimant, so this picks one
	/// deterministically instead.
	pub(crate) fn offer(
		&mut self,
		rel_path: &str,
		uuid: Option<Uuid>,
		lineage: Option<StableUuid>,
	) {
		if let Some(uuid) = uuid
			&& let Some(slot) = self.by_uuid.get_mut(&uuid)
		{
			keep_first(slot, rel_path, &format!("remote uuid {uuid}"));
		}
		if let Some(lineage) = lineage
			&& let Some(slot) = self.by_lineage.get_mut(&lineage)
		{
			keep_first(slot, rel_path, "a whole-life id");
		}
	}

	/// Where the row recording `uuid` sits, of the ids this was asked to resolve.
	pub(crate) fn path_by_uuid(&self, uuid: Uuid) -> Option<String> {
		self.answer(self.by_uuid.get(&uuid), &uuid)
	}

	/// Where the row recording the file with this whole-life id sits.
	pub(crate) fn path_by_lineage(&self, lineage: StableUuid) -> Option<String> {
		self.answer(self.by_lineage.get(&lineage), &lineage)
	}

	/// One resolved slot as an answer — and a loud refusal for an id nobody resolved.
	///
	/// `debug_assert` so the test suite fails outright on an enumeration that missed an id, and a
	/// logged error in release, where answering `None` silently is what would turn the gap into a
	/// fabricated absence.
	fn answer(&self, slot: Option<&Option<String>>, id: &dyn std::fmt::Display) -> Option<String> {
		match slot {
			Some(found) => found.clone(),
			None => {
				debug_assert!(
					false,
					"the baseline was asked where {id} sits without that id having been resolved"
				);
				tracing::error!(
					"baseline: asked where {id} sits, but that id was never resolved for this \
					 pass; answering that nothing records it"
				);
				None
			}
		}
	}
}

/// Keep the lexicographically first of two paths claiming one id, saying so when there are two.
fn keep_first(slot: &mut Option<String>, rel_path: &str, what: &str) {
	match slot {
		None => *slot = Some(rel_path.to_owned()),
		Some(held) => {
			tracing::error!(
				"baseline: {rel_path:?} and {held:?} both record {what}; answering with the first \
				 of the two by path"
			);
			if rel_path < held.as_str() {
				*held = rel_path.to_owned();
			}
		}
	}
}

/// How many ids one [`BaselineStore::synced_paths`] statement binds.
///
/// Fixed so the statement TEXT is fixed, which is what lets it go through `prepare_cached`: a
/// per-call `IN` list of the exact length would be a fresh parse and plan every time. The last
/// chunk is padded by repeating an id it already holds, which matches the same rows and costs one
/// extra seek rather than a second statement shape.
const SYNCED_CHUNK: usize = 64;

/// The staging tables of [`BaselineStore::replace_legacy_synced`], dropped with this however the
/// import ends. A failed drop is logged and costs only the temporary database's space until the
/// next import drops them first, or the connection closes.
struct LegacyStage<'c>(&'c Connection);

impl Drop for LegacyStage<'_> {
	fn drop(&mut self) {
		if let Err(error) = self.0.execute_batch(LEGACY_UNSTAGE) {
			tracing::warn!("baseline store: could not drop the filen-sync staging tables: {error}");
		}
	}
}

/// The baseline DB handle (sole owner / single writer).
///
/// It holds nothing of a pair's rows between passes: a pass reads them out of the table, through
/// one of [`readers`](Self::readers) (see [`BaselineStore::baseline`]).
pub(crate) struct BaselineStore {
	conn: Connection,
	/// The connections passes read on, shared with every [`Snapshot`] that has one out.
	readers: Arc<Readers>,
	/// The file an in-memory store stands on, removed with the store (see
	/// [`open_in_memory`](Self::open_in_memory)).
	#[cfg(test)]
	_file: Option<tempfile::TempPath>,
}

/// The DB's page size, in bytes. Twice SQLite's default, so that a `baseline` row — ~250 bytes
/// with two paths — stays inside the twentieth of a page `WITHOUT ROWID` is laid out for (see
/// [`SCHEMA`]).
const PAGE_SIZE: usize = 8192;

/// The page cache a READER connection keeps, in KiB (a negative `cache_size` is KiB, not pages).
///
/// Chosen, not defaulted, because this is now the one place a pair's rows are held in the process:
/// the resident tree this replaced was ~127 bytes a row for the life of the pair, and SQLite's own
/// cache would otherwise be the same cost moved somewhere nobody sized. What a pass's point lookups
/// touch on every read is the interior pages of two b-trees — the table, which IS its primary key,
/// and `baseline_folded` — which at a million rows come to a few MiB; the leaves they lead to are read
/// through the OS page cache, which is the kernel's to evict and is not this process's resident
/// set. 8 MiB keeps every interior page of a million-row pair resident with room for the leaves a
/// directory's worth of lookups revisits, and it is a CEILING: a pair smaller than that never
/// grows the cache to it. A whole pass pages the entire table through it and leaves it full, until
/// the pass is over (see [`BaselineStore::release_page_caches`]).
const READER_CACHE_KIB: i64 = 8 * 1024;

/// The page cache of the store's own WRITING connection, in KiB. SQLite's default, stated: the
/// writer reads a few pages per statement and a first sync's million-row transaction spills to the
/// WAL whatever its cache, so a larger one buys that transaction little and costs every pair the
/// difference for as long as a pass runs (see [`BaselineStore::release_page_caches`]).
const WRITER_CACHE_KIB: i64 = 2 * 1024;

/// What the `-wal` file is cut back to once a checkpoint has emptied it, in bytes.
///
/// SQLite never shrinks the file on its own: it rewinds to the start and keeps the size it grew to.
/// A first sync's one transaction over the whole tree grows it to hundreds of MiB, and without a
/// limit the file then stays that size for as long as the engine holds a connection open. Twice
/// what the default autocheckpoint lets it reach (1000 pages of [`PAGE_SIZE`]), so ordinary use is
/// never truncated and regrown.
const WAL_SIZE_LIMIT_BYTES: i64 = 16 * 1024 * 1024;

/// The connections a pair's passes read on: one per pass in flight, reused across passes so a pass
/// does not pay a connection open (a schema parse, a fresh statement cache) for its rows.
pub(crate) struct Readers {
	path: PathBuf,
	idle: Mutex<Vec<Connection>>,
}

impl Readers {
	fn take(&self) -> rusqlite::Result<Connection> {
		let idle = self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.pop();
		match idle {
			Some(conn) => Ok(conn),
			None => open_reader(&self.path),
		}
	}

	/// A connection whose read transaction has ended, back for the next pass — kept only when no
	/// other is idle. A pair runs one pass at a time: a second snapshot open at once (a dry run
	/// beside a pass that is still planning — a pass hands its own back once its plan is made) is
	/// a moment, not a steady state, and the connection it opened is closed rather than kept for
	/// the life of the store.
	///
	/// The one kept is parked with its page cache (up to [`READER_CACHE_KIB`]) still full, and gives
	/// it back once the pass is over (see [`BaselineStore::release_page_caches`]), not here: a pass
	/// parks its reader once its plan is made, with the apply still to run, and a release reaches
	/// every connection in the process (see [`release_page_cache`]), so one here would drop the
	/// pages that pass's own apply is still reading. The end spares only that pass: whatever else
	/// is reading when a release lands, another pair's pass in flight included, loses its cached
	/// pages all the same.
	pub(super) fn give(&self, conn: Connection) {
		let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
		if idle.is_empty() {
			idle.push(conn);
		}
	}

	/// Give back the page cache of every parked reader. The connections stay open, with their
	/// parsed schema and prepared statements; the next pass reads the interior pages it needs back
	/// from the OS page cache.
	fn release_idle(&self) {
		for conn in self
			.idle
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
		{
			release_page_cache(conn, "a parked reader");
		}
	}
}

/// A connection a pass reads on. Read-write at the file level — a WAL reader has to be able to
/// write the shared-memory index — but `query_only`, so nothing a pass does through it can write a
/// row; the busy timeout covers the rare moment a reader waits on WAL recovery.
fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
	let conn = open_sqlite(path)?;
	conn.busy_timeout(std::time::Duration::from_millis(30_000))?;
	conn.execute_batch(&format!(
		"PRAGMA query_only = ON; PRAGMA cache_size = -{READER_CACHE_KIB};"
	))?;
	// Every statement a pass reads with, with room: the default of 16 would evict on a pass that
	// uses all of them, and re-preparing is the cost the cache exists to remove.
	conn.set_prepared_statement_cache_capacity(32);
	Ok(conn)
}

/// Give back the pages `conn` caches. A failure is memory kept, not a pass lost, so it is logged.
///
/// It reaches further than `conn`. The bundled SQLite is built with
/// `SQLITE_ENABLE_MEMORY_MANAGEMENT`, which puts every connection's pages in one shared cache, and
/// a release empties all of it that no statement is using: the other pairs' connections and the
/// cache's lose their cached pages too, and read them back from the OS page cache. Only clean pages
/// are dropped. A pass calls this once it is over, when its own reads are done (see
/// [`BaselineStore::release_page_caches`]), but not once every pass is: another pair's pass still
/// in flight when it lands loses its cached pages all the same.
fn release_page_cache(conn: &Connection, whose: &str) {
	if let Err(error) = conn.release_memory() {
		tracing::warn!("baseline store: could not give back the page cache of {whose}: {error}");
	}
}

/// The states a row can hold other than `Synced`, as stored: what the reader's "is every row under
/// this directory synced" seeks on (see `baseline_state`). Written out so that a new state has to be
/// added here — the `match` below refuses to compile until it is.
pub(super) const UNSYNCED_STATES: [i64; 3] = [
	BaselineState::Conflicted.as_i64(),
	BaselineState::Adopted.as_i64(),
	BaselineState::Overwritten.as_i64(),
];

const _: () = {
	// Every state but `Synced` is in `UNSYNCED_STATES`: the match is exhaustive, so a new state
	// fails here until someone decides which side of that line it is on.
	const fn listed(state: BaselineState) -> bool {
		match state {
			BaselineState::Synced => false,
			BaselineState::Conflicted | BaselineState::Adopted | BaselineState::Overwritten => true,
		}
	}
	assert!(listed(BaselineState::Conflicted) && !listed(BaselineState::Synced));
};

/// A connection holding the schema and one pair, for a test to write rows into and read them back
/// through a [`Snapshot`] — the store's own statements on the store's own tables.
#[cfg(test)]
pub(super) fn test_rows_connection(pair: PairId) -> Connection {
	let conn = open_sqlite_in_memory().expect("opening a test connection");
	conn.execute_batch(&format!(
		"PRAGMA page_size = {PAGE_SIZE}; PRAGMA foreign_keys = ON; PRAGMA recursive_triggers = ON;
		 {SCHEMA} {ADDITIVE_SCHEMA}"
	))
	.expect("creating the test schema");
	conn.execute(
		"INSERT INTO sync_pairs (id, local_root, remote_root, mode) VALUES (?1, '/test', ?2, ?3)",
		params![pair, Uuid::nil(), SyncMode::TwoWay.as_i64()],
	)
	.expect("registering the test pair");
	conn
}

fn open_error(error: rusqlite::Error) -> crate::Error {
	super::engine::db_error(error, "opening the sync baseline DB")
}

fn corrupt(what: &str, value: impl Display) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(
		0,
		Type::Integer,
		format!("invalid {what} value in baseline DB: {value}").into(),
	)
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
	// Before anything is written: the page size of a file is fixed once it holds a page, and once it
	// is in WAL mode.
	conn.execute_batch(&format!("PRAGMA page_size = {PAGE_SIZE};"))?;
	let tx = conn.unchecked_transaction()?;
	conn.execute_batch(schema)?;
	conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
	tx.commit()
}

/// Whether `error` is SQLite saying another connection holds the single write lock.
fn is_write_lock_contention(error: &rusqlite::Error) -> bool {
	match error {
		rusqlite::Error::SqliteFailure(failure, _) => matches!(
			failure.code,
			rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
		),
		_ => false,
	}
}

/// Run a write that RECORDS an act already done, and make its failure loud.
///
/// ONE attempt, on purpose. The busy timeout [`init`](BaselineStore::init) sets makes SQLite itself
/// sleep and retry inside `sqlite3_step` for a full 30 s before it answers `SQLITE_BUSY`, so a
/// second attempt would wait out another 30 s to learn what the first one already established: the
/// connection holding the write lock is not making progress. The unbounded retry this replaces
/// turned that into a pass that hung for as long as the wedged writer lived, without a word.
///
/// How much margin those 30 s leave is measured, and it is not generous. The longest write one
/// connection makes is a whole-tree transaction, and with `ADDITIVE_SCHEMA`'s index in place a
/// million-row re-write measured 21.2 s at best and 26.4 s by median (15.8 s without it) on an
/// SSD, with a control write on another connection waiting 21.9 s behind one of them. A record
/// write that lands during a first sync's confirmation pass on a tree that size has single-digit
/// seconds to spare; on a phone's flash, or on a tree larger than a million rows, it is unmeasured
/// and may have none. `delete_pair`'s cascade — the other long write — is 4.1 s at a million rows
/// and is not the one to worry about. Where the
/// margin does run out this fails, loudly, which is the point; the lever if it happens in the
/// field is a longer timeout for these writes alone, not a retry.
///
/// So the record is lost and the act it describes stands without it. Loudly, not silently: the
/// error goes to the caller, this logs it with the pair, the path and which record failed, and a
/// pass carries it in [`SyncReport::errors`](super::SyncReport::errors) with `store_failed` set, so
/// a watch loop counts the pass as failed and backs off instead of running the next one into the
/// same wedged DB. What the next pass does about the lost record, per site:
///
/// - [`upsert_entry`](BaselineStore::upsert_entry) — the row after a LOCAL write (a download, a
///   local directory create, an adopt, a conflict hold). The row still describes the state before
///   it, so both sides read as changed and, holding the same bytes, the reconcile plans
///   `AdoptBaseline` and records it then (`plan::reconcile_two_way`'s converged arm): no transfer,
///   one pass late. A conflict hold that did not land is re-planned from the two sides and held
///   again — with ONE exception: the row that holds the version an upload of ours buried
///   (`apply::note_overwrote`). The remote head is then our own upload and the buried version
///   survives only in the file's version history, so no view can re-derive that row; the conflict
///   is reported and the pass fails, but the path is not HELD, and unless the caller acts on that
///   report the next pass finds both sides holding our bytes and adopts them. A remote edit
///   landing in between makes the two sides disagree, which is a conflict surfaced to the caller,
///   not an overwrite.
/// - [`delete_entry`](BaselineStore::delete_entry) — the row after a deletion (a local quarantine,
///   an adopt that drops the row). The row survives with both sides absent at its path, which is
///   what `apply::adopt_outcome` drops it for, under the absence gate every deletion already
///   passes. Neither side is touched meanwhile: both are already gone.
/// - [`record_pending`](BaselineStore::record_pending) — the journal row and the baseline rows a
///   REMOTE write produced (an upload, a remote directory create, a trash, a remote file move).
///   The remote write stands and nothing in this process knows it, since the in-memory journal is
///   published only once this has committed. The next pass therefore reconciles against whatever
///   the cache shows and can REDO the act: a second upload of the same bytes, which the server
///   takes as another version of the same file rather than a duplicate; a second `dir/create`,
///   which the name-hash dedup answers with the existing directory's uuid; a second trash of an
///   item already trashed. Duplicated work, not lost content — and a foreign edit at the same path
///   still reads as a divergence and is held.
/// - the same call from `apply::commit_dir_move` — a directory move's journal row and subtree
///   re-key. The directory has moved on the server with its rows still under the old path.
///   `plan::next_dir_move` matches a baseline directory row by its remote uuid wherever the remote
///   now lists it, so the next pass re-keys the subtree: the move is re-derived, not re-made. Where
///   the cache has not caught up either, that pass re-issues the same move. Neither side reads the
///   directory as deleted, because this write is one transaction — rows never end up at only one
///   of the two paths.
/// - [`delete_pending`](BaselineStore::delete_pending) — the journal retirement. The record memory
///   has dropped stays in the DB, so THIS process is unaffected; the next OPEN restores it and
///   folds our own write over the snapshot again, for what is left of the 180 s grace window
///   `load_pending` measures by the wall clock. For a record retired because the cache caught up
///   that fold is a no-op (`fold_create` sees the cache already showing our uuid); for one retired
///   because the server said our write was superseded, it delays the pull of the superseding
///   version by the rest of that window — the direction a delayed confirmation already takes.
///
/// A control verb's write is not wrapped at all: it records nothing that has happened, so it fails
/// and its caller can try again.
fn record_write<T>(
	pair: PairId,
	what: impl FnOnce() -> String,
	write: impl FnOnce() -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
	write().inspect_err(|error| {
		let why = if is_write_lock_contention(error) {
			"another connection has held SQLite's write lock for longer than the busy timeout"
		} else {
			"the write failed"
		};
		tracing::error!(
			"baseline store[pair {pair}]: {} could NOT be written ({why}): {error} — the act it \
			 records has already happened, so it now stands unrecorded and this pass reports a \
			 failure",
			what()
		);
	})
}

/// The changes a failed [`apply_changes`](BaselineStore::apply_changes) or
/// [`record_pending`](BaselineStore::record_pending) did not write, for the log line that says so.
/// Called on that failure only, so its allocations cost a pass nothing.
fn describe_changes(changes: &[BaselineChange<'_>]) -> String {
	let mut described: Vec<String> = changes
		.iter()
		.take(3)
		.map(|change| match change {
			BaselineChange::Upsert(entry) => format!("upsert {:?}", entry.rel_path),
			BaselineChange::Delete(rel_path) => format!("delete {rel_path:?}"),
			BaselineChange::MoveSubtree { from, to } => format!("move {from:?} -> {to:?}"),
		})
		.collect();
	if changes.len() > 3 {
		described.push(format!("and {} more", changes.len() - 3));
	}
	described.join(", ")
}

/// The conflict read's statement, named so the plan test explains exactly what the read runs
/// rather than one that merely looks like it. `(pair_id, state)` is what [`ADDITIVE_SCHEMA`]'s
/// index covers, and there is deliberately no `ORDER BY` here: the ordering
/// [`BaselineStore::conflicts`] promises is applied to the rows it hands back, for the reason
/// given there.
///
/// The index is NAMED (`INDEXED BY`): this DB is never `ANALYZE`d, and with no statistics the
/// planner prefers a walk of the clustered primary key on `pair_id` — every row of the pair — to a
/// seek of a narrower index it would then have to read each row back through. Named, the seek is
/// taken or the statement refuses to prepare.
fn conflict_rows_sql() -> String {
	format!(
		"SELECT {ENTRY_COLUMNS} FROM baseline INDEXED BY baseline_state
		 WHERE pair_id = ?1 AND state IN (?2, ?3)"
	)
}

/// [`conflict_rows_sql`] without the index named: the read as it runs in a file without the
/// index, for the probe's without-index phase and the plan test's control.
#[cfg(any(test, feature = "bench-internals"))]
fn conflict_rows_walked_sql() -> String {
	format!("SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 AND state IN (?2, ?3)")
}

impl BaselineStore {
	/// Open (creating if needed) the baseline DB at `path`.
	pub(crate) fn open(path: &Path) -> Result<Self, crate::Error> {
		Self::init(open_sqlite(path).map_err(open_error)?, path)
	}

	/// A store no other test sees, removed with it.
	///
	/// On a temporary FILE rather than SQLite's `:memory:`, because a pass reads its rows on a
	/// second connection inside a WAL read transaction (see [`baseline`](Self::baseline)), and an
	/// in-memory database has neither a second connection nor WAL.
	#[cfg(test)]
	pub(crate) fn open_in_memory() -> Result<Self, crate::Error> {
		let file = tempfile::Builder::new()
			.prefix("sync-baseline-")
			.suffix(".db")
			.tempfile()
			.map_err(|error| {
				crate::Error::custom(
					crate::ErrorKind::Internal,
					format!("creating a test baseline DB: {error}"),
				)
			})?
			.into_temp_path();
		let mut store = Self::open(&file)?;
		store._file = Some(file);
		Ok(store)
	}

	/// Take this connection's write lock and hold it until the returned transaction is dropped —
	/// the shape another pair's whole-tree commit has, for the tests that pin what a contended write
	/// does.
	#[cfg(test)]
	pub(super) fn hold_write_lock(
		&self,
		pair: PairId,
		entry: &BaselineEntry,
	) -> rusqlite::Result<rusqlite::Transaction<'_>> {
		let tx = self.conn.unchecked_transaction()?;
		// The INSERT is what takes the write lock; `BEGIN DEFERRED` alone takes nothing.
		self.upsert_entry_once(pair, entry)?;
		Ok(tx)
	}

	/// The writing connection, for a test that reads a page through it.
	#[cfg(test)]
	pub(super) fn writer(&self) -> &Connection {
		&self.conn
	}

	/// Shorten this connection's busy timeout, so a test reaches the contended answer in
	/// milliseconds instead of sitting out the 30 s a real open waits.
	#[cfg(test)]
	pub(super) fn busy_timeout_for_test(&self, timeout: std::time::Duration) {
		self.conn
			.busy_timeout(timeout)
			.expect("setting a busy timeout on an open connection");
	}

	/// Drop or re-create the conflict-state index, for the plan test and the probe phases that have
	/// to see both sides of it. Nothing in production calls it: every open creates it.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(super) fn set_state_index(&self, present: bool) -> rusqlite::Result<()> {
		self.conn.execute_batch(if present {
			ADDITIVE_SCHEMA
		} else {
			"DROP INDEX IF EXISTS baseline_state;"
		})
	}

	/// A fresh DB is created whole and stamped at [`SCHEMA_VERSION`]. An existing one is opened only
	/// when it carries exactly that version: anything else — older or newer — is refused with a
	/// [`BaselineSchemaMismatch`] rather than read, since there is no migration chain to bring it here
	/// and reading foreign rows under these rules would misplan them into deletes.
	fn init(conn: Connection, path: &Path) -> Result<Self, crate::Error> {
		// Both are per-connection, reset on every open, and neither writes to the file — so they are
		// safe to apply before this build knows whether the DB is even one it can read. The timeout
		// covers the version read below too: it retries a transient `SQLITE_BUSY` rather than
		// failing a pass the moment a second opener (another process, a backup tool) touches it.
		//
		// It has to outlast the longest write any ONE connection to this file makes, because the
		// engine keeps several: one per pair plus a control connection. WAL lets a reader through
		// during a write, but not a second writer, so a control write — persisting a paused flag,
		// storing the user ignore patterns — waits out whatever a pair is committing. The longest
		// of those is a first sync's single transaction over the whole tree: 21.2-26.4 s for a
		// million rows with `ADDITIVE_SCHEMA`'s index in place (15.8 s without it), and a control
		// write measured 21.9 s waiting behind one. So 30 s is enough at that size and not by much
		// — a timeout under it would turn a pause into a `SQLITE_BUSY` error instead of a slow
		// pause, and a tree well past a million rows would need more than 30 s. Waiting is the
		// right direction for every one of these callers:
		// the flag a pause fails to persist is the one that protects the pair on the next open.
		conn.busy_timeout(std::time::Duration::from_millis(30_000))
			.map_err(open_error)?;
		// `recursive_triggers` is what makes the counting triggers see a row that REPLACE deletes
		// (see [`SCHEMA`]); every connection that writes rows needs it, and this is the only one.
		conn.execute_batch(&format!(
			"PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL; \
			 PRAGMA recursive_triggers = ON; PRAGMA cache_size = -{WRITER_CACHE_KIB}; \
			 PRAGMA journal_size_limit = {WAL_SIZE_LIMIT_BYTES};"
		))
		.map_err(open_error)?;
		let version: i64 = conn
			.query_row("PRAGMA user_version", [], |row| row.get(0))
			.map_err(open_error)?;
		if is_unwritten(&conn).map_err(open_error)? {
			create_schema(&conn, SCHEMA).map_err(open_error)?;
		} else if version != SCHEMA_VERSION {
			return Err(crate::Error::custom_with_source(
				crate::ErrorKind::InvalidState,
				BaselineSchemaMismatch {
					path: path.to_path_buf(),
					found: version,
					expected: SCHEMA_VERSION,
				},
				None::<&str>,
			));
		}
		// Last, and only once the DB is one this build owns: `journal_mode` is the one pragma here
		// that persists in the file HEADER, so setting it earlier rewrote a DB that was then
		// refused — leaving a stranger's file in WAL with `-wal`/`-shm` sidecars — and failed on a
		// read-only file instead of refusing it with the message that says what to do about it.
		//
		// WAL with `synchronous = NORMAL` is the pairing the cache uses (`cache/sql/mod.rs`): one
		// fsync per checkpoint instead of one per commit, which is the difference between a quarter
		// of a millisecond and a few microseconds for the row a pass writes per applied action —
		// measured on this store, 331 µs against 14.7 µs. The trade is that a power loss can lose
		// the last committed transactions. It can never split one, and every write that has to be
		// atomic already is one — `record_pending` commits the journal row together with the
		// baseline row it describes — so a lost commit leaves the pair consistent and merely
		// re-reconciles that path on the next pass.
		//
		// It costs a BULK write, which the per-row figure hides: one transaction of 1M rows measured
		// 11.0 s here against 6.0 s under the rollback journal, because its pages go into the `-wal`
		// file and are copied into the DB again at the checkpoint, where a rollback journal over a
		// nearly empty table has almost nothing to undo. A first sync pays that once, against the
		// per-action writes of every pass after it being ~22x cheaper. Chunking the batch is not a
		// way out: the one transaction is what makes a subtree's rows land together.
		//
		// Answers with the mode it set, which `execute` refuses and `pragma_update` allows.
		conn.pragma_update(None, "journal_mode", "WAL")
			.map_err(open_error)?;
		// Then the additive objects, on every open: a DB written before one of them existed gains it
		// here, and a fresh one has just been created without them (see [`ADDITIVE_SCHEMA`]).
		conn.execute_batch(ADDITIVE_SCHEMA).map_err(open_error)?;
		Ok(Self {
			conn,
			readers: Arc::new(Readers {
				path: path.to_path_buf(),
				idle: Mutex::new(Vec::new()),
			}),
			#[cfg(test)]
			_file: None,
		})
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
				// The non-retrying form: these rows are inside the transaction above.
				self.upsert_entry_once(id, entry)?;
			}
		}
		tx.commit()?;
		Ok(changed)
	}

	pub(crate) fn pair(&self, id: PairId) -> rusqlite::Result<Option<PairRecord>> {
		self.conn
			.query_row(
				"SELECT id, local_root, remote_root, mode, guard_floor, guard_ratio, paused, first_pass_completed FROM sync_pairs WHERE id = ?1",
				params![id],
				Self::row_to_pair,
			)
			.optional()
	}

	pub(crate) fn list_pairs(&self) -> rusqlite::Result<Vec<PairRecord>> {
		self.conn
			.prepare("SELECT id, local_root, remote_root, mode, guard_floor, guard_ratio, paused, first_pass_completed FROM sync_pairs ORDER BY id")?
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

	/// Record that a pass of `pair` has completed cleanly (see
	/// [`PairRecord::first_pass_completed`]). Once set it stays set for the life of the pair.
	pub(crate) fn mark_first_pass_completed(&self, id: PairId) -> rusqlite::Result<()> {
		self.conn.execute(
			"UPDATE sync_pairs SET first_pass_completed = 1 WHERE id = ?1",
			params![id],
		)?;
		Ok(())
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

	/// The stored user ignore patterns, `""` when none were ever set.
	pub(crate) fn user_ignore(&self) -> rusqlite::Result<String> {
		Ok(self
			.conn
			.query_row("SELECT patterns FROM user_ignore WHERE id = 0", [], |row| {
				row.get(0)
			})
			.optional()?
			.unwrap_or_default())
	}

	/// Replace the user ignore patterns. The caller has already validated them.
	pub(crate) fn set_user_ignore(&self, patterns: &str) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT INTO user_ignore (id, patterns) VALUES (0, ?1)
			 ON CONFLICT (id) DO UPDATE SET patterns = excluded.patterns",
			params![patterns],
		)?;
		Ok(())
	}

	/// The ignored roots `pair`'s last pass recorded.
	pub(crate) fn ignored_roots(&self, pair: PairId) -> rusqlite::Result<BTreeSet<String>> {
		self.conn
			.prepare("SELECT rel_path FROM ignored_roots WHERE pair_id = ?1")?
			.query_map(params![pair], |row| row.get(0))?
			.collect()
	}

	/// Replace `pair`'s recorded ignored roots, in ONE transaction.
	pub(crate) fn set_ignored_roots(
		&self,
		pair: PairId,
		roots: &BTreeSet<String>,
	) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		self.conn.execute(
			"DELETE FROM ignored_roots WHERE pair_id = ?1",
			params![pair],
		)?;
		{
			let mut insert = self
				.conn
				.prepare("INSERT INTO ignored_roots (pair_id, rel_path) VALUES (?1, ?2)")?;
			for root in roots {
				insert.execute(params![pair, root])?;
			}
		}
		tx.commit()
	}

	fn row_to_pair(row: &Row<'_>) -> rusqlite::Result<PairRecord> {
		let mode_raw: i64 = row.get("mode")?;
		// Both NULL is the ordinary case (a pair registered before v3, or never reconfigured): the
		// default policy, which is exactly what those pairs already ran under.
		let delete_guard = match (
			row.get::<_, Option<i64>>("guard_floor")?,
			row.get::<_, Option<f64>>("guard_ratio")?,
		) {
			// `set_delete_guard` saturates `unlimited`'s `usize::MAX` floor to `i64::MAX`; it reads
			// back as `usize::MAX`, so an unlimited guard comes back equal to `unlimited()`. No
			// tracked-item count reaches either, so a floor really set to `i64::MAX` loses nothing.
			(Some(floor), Some(ratio)) => {
				let floor = if floor == i64::MAX {
					usize::MAX
				} else {
					usize::try_from(floor.max(0)).unwrap_or(usize::MAX)
				};
				DeleteGuard::new(floor, ratio).map_err(|_| corrupt("guard ratio", ratio))?
			}
			_ => DeleteGuard::default(),
		};
		Ok(PairRecord {
			id: row.get("id")?,
			local_root: row.get("local_root")?,
			remote_root: row.get("remote_root")?,
			mode: SyncMode::from_i64(mode_raw).ok_or_else(|| corrupt("mode", mode_raw))?,
			delete_guard,
			paused: row.get("paused")?,
			first_pass_completed: row.get("first_pass_completed")?,
		})
	}

	/// Insert or replace one baseline row.
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: a pass writes
	/// this row AFTER the transfer, rename or trash it records (see [`record_write`]).
	pub(crate) fn upsert_entry(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| format!("the baseline row for {:?}", entry.rel_path),
			|| self.upsert_entry_once(pair, entry),
		)
	}

	/// [`upsert_entry`](Self::upsert_entry) without the retry, for the callers already inside a
	/// transaction of this connection's — which holds the write lock, so there is nothing left to
	/// wait for and a retry of the statement alone would re-run it inside a transaction that has
	/// already failed.
	fn upsert_entry_once(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		upsert_row(&self.conn, pair, entry)
	}

	/// One baseline row by path (whole-pair snapshots go through `entries`).
	pub(crate) fn entry(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<BaselineEntry>> {
		self.conn
			.query_row(
				&format!(
					"SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 AND rel_path = ?2"
				),
				params![pair, rel_path],
				Self::row_to_entry,
			)
			.optional()
	}

	/// Every baseline row for `pair`, ordered by path (parent-before-child for same-prefix paths).
	///
	/// The whole pair as a `Vec`, which nothing a pass does needs any more —
	/// [`baseline`](Self::baseline) streams the rows into the tree instead, and that `Vec` at a
	/// million rows is the widest thing an idle process used to build. What is left is the tests'
	/// way of asking what the DB holds, and the probe's.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(crate) fn entries(&self, pair: PairId) -> rusqlite::Result<Vec<BaselineEntry>> {
		self.conn
			.prepare(&format!(
				"SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 ORDER BY rel_path"
			))?
			.query_map(params![pair], Self::row_to_entry)?
			.collect()
	}

	/// Where `pair` records each of `uuids` and `lineages` — the point lookups the resident tree
	/// used to answer from two `HashMap`s of its own (see [`SyncedPaths`]).
	///
	/// Index seeks, not a scan: `ADDITIVE_SCHEMA` carries `(pair_id, remote_uuid)` and
	/// `(pair_id, remote_stable_uuid)` for exactly this. The ids are deduplicated first, which is
	/// most of the work on a real delta — a directory holding a hundred changed children names one
	/// parent a hundred times — and then bound [`SYNCED_CHUNK`] at a time.
	///
	/// An empty ask runs no statement at all, which is the idle pass's shape.
	pub(crate) fn synced_paths(
		&self,
		pair: PairId,
		uuids: &[Uuid],
		lineages: &[StableUuid],
	) -> rusqlite::Result<SyncedPaths> {
		let mut out = SyncedPaths::asking(uuids, lineages);
		let uuids: Vec<Uuid> = out.by_uuid.keys().copied().collect();
		let lineages: Vec<StableUuid> = out.by_lineage.keys().copied().collect();
		self.seek_ids(pair, &uuids, &synced_by("remote_uuid"), &mut out)?;
		self.seek_ids(pair, &lineages, &synced_by("remote_stable_uuid"), &mut out)?;
		Ok(out)
	}

	/// Run `statement` over `ids`, [`SYNCED_CHUNK`] at a time, offering every row it finds.
	fn seek_ids<T>(
		&self,
		pair: PairId,
		ids: &[T],
		statement: &str,
		out: &mut SyncedPaths,
	) -> rusqlite::Result<()>
	where
		T: rusqlite::ToSql + Copy,
	{
		if ids.is_empty() {
			return Ok(());
		}
		let mut prepared = self.conn.prepare_cached(statement)?;
		for chunk in ids.chunks(SYNCED_CHUNK) {
			let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(SYNCED_CHUNK + 1);
			bound.push(&pair);
			for id in chunk {
				bound.push(id);
			}
			// The pad repeats a member of this chunk, so it matches rows the chunk already matches
			// and the statement keeps one shape. `chunk` is never empty.
			let pad = &chunk[0];
			for _ in chunk.len()..SYNCED_CHUNK {
				bound.push(pad);
			}
			let mut rows = prepared.query(bound.as_slice())?;
			while let Some(row) = rows.next()? {
				let rel_path: String = row.get("rel_path")?;
				out.offer(
					&rel_path,
					row.get("remote_uuid")?,
					row.get("remote_stable_uuid")?,
				);
			}
		}
		Ok(())
	}

	/// `pair`'s rows as a pass reads them: a [`Snapshot`] of the table, on a reader connection of
	/// the pass's own with a read transaction open (see [`rows`](super::rows)).
	///
	/// Nothing is read here but the pair's counts, which is the read that pins the snapshot: what a
	/// pass then asks, it asks of the table. The store keeps nothing of the pair between passes —
	/// the resident tree this replaced was the whole pair, ~127 bytes a row for the life of the
	/// process — and a pass that asks nothing (an idle wake) costs this one row read.
	///
	/// This process's store is assumed to be the only writer of the pair's rows — it is, for every
	/// path in the engine. The snapshot does not depend on it: a second writer's commits are simply
	/// not in a snapshot that began before them, exactly as this store's own are not.
	pub(crate) fn baseline(&self, pair: PairId) -> rusqlite::Result<Baseline> {
		let conn = self.readers.take()?;
		Ok(Baseline::stored(Snapshot::begin(
			conn,
			pair,
			Some(Arc::clone(&self.readers)),
		)?))
	}

	/// Give back the pages the store's connections cache, once a pass is over: the writing
	/// connection's, which hold what the pass wrote through it (up to [`WRITER_CACHE_KIB`]), and
	/// the parked reader's, which hold the rows the pass read (up to [`READER_CACHE_KIB`]). Both
	/// would otherwise stay cached while the pair idles.
	///
	/// At the end and not sooner: the reader is parked once the plan is made, but a release reaches
	/// every connection in the process (see [`release_page_cache`]), and one before the apply would
	/// drop the pages the pass's own apply is still reading. The end spares only that pass's reads:
	/// another pair's pass still in flight when the release lands loses its cached pages all the
	/// same.
	pub(crate) fn release_page_caches(&self) {
		release_page_cache(&self.conn, "the writing connection");
		self.readers.release_idle();
	}

	/// The rows `pair` is holding in conflict — both flavours — ordered by path.
	///
	/// A `WHERE` on the state rather than a read of the whole pair filtered in Rust: what a caller
	/// listing the conflicts wants is a handful of rows, and reading every row of the pair to find
	/// them is the same work a pass does. [`ADDITIVE_SCHEMA`]'s `(pair_id, state)` index is what
	/// makes it a seek of those rows rather than that same walk with the filtering moved into
	/// SQLite.
	pub(crate) fn conflicts(&self, pair: PairId) -> rusqlite::Result<Vec<BaselineEntry>> {
		self.conflicts_by(pair, &conflict_rows_sql())
	}

	/// [`conflicts`](Self::conflicts) in a file without the state index (see
	/// [`conflict_rows_walked_sql`]).
	#[cfg(feature = "bench-internals")]
	pub(super) fn conflicts_walked(&self, pair: PairId) -> rusqlite::Result<Vec<BaselineEntry>> {
		self.conflicts_by(pair, &conflict_rows_walked_sql())
	}

	fn conflicts_by(&self, pair: PairId, sql: &str) -> rusqlite::Result<Vec<BaselineEntry>> {
		let mut held: Vec<BaselineEntry> = self
			.conn
			.prepare(sql)?
			.query_map(
				params![
					pair,
					BaselineState::Conflicted.as_i64(),
					BaselineState::Overwritten.as_i64(),
				],
				Self::row_to_entry,
			)?
			.collect::<rusqlite::Result<_>>()?;
		// Ordered here rather than by the statement: an `ORDER BY rel_path` is satisfied for free
		// by the primary key, so the planner takes that walk over the state seek and the index buys
		// nothing. Measured at 100k (103,479 rows, none of them held): the ordered form read every
		// row of the pair in 71.6 ms, all of it under the pair's store mutex, against 0.03 ms for
		// the seek. What comes back is the handful of rows a pair holds in conflict, so sorting them
		// here costs nothing.
		held.sort_unstable_by(|a, b| a.rel_path.cmp(&b.rel_path));
		Ok(held)
	}

	/// Drop one baseline row. Fails loudly rather than waiting for another connection's write
	/// transaction, for the same reason [`upsert_entry`](Self::upsert_entry) does.
	pub(crate) fn delete_entry(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| format!("the deletion of the baseline row for {rel_path:?}"),
			|| self.delete_entry_once(pair, rel_path),
		)
	}

	/// [`delete_entry`](Self::delete_entry) without the retry, for a caller already inside one of
	/// this connection's transactions.
	fn delete_entry_once(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		self.conn
			.prepare_cached(DELETE_ENTRY)?
			.execute(params![pair, rel_path])?;
		Ok(())
	}

	/// Delete every row at or under each of `roots`, in ONE transaction.
	pub(crate) fn delete_subtrees(
		&self,
		pair: PairId,
		roots: &BTreeSet<String>,
	) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		{
			let mut delete_root = self.conn.prepare(DELETE_AT_PATH)?;
			let mut delete_under = self.conn.prepare(DELETE_UNDER_PATH)?;
			for root in roots {
				delete_root.execute(params![pair, root])?;
				delete_under.execute(params![pair, root])?;
			}
		}
		tx.commit()
	}

	/// Journal a remote write AND apply the baseline edits it produced, in ONE transaction.
	///
	/// The two have to land together: the journal row is what a reopened engine folds into its
	/// remote view, and the baseline row is the written state it folds. A crash between them leaves
	/// the next pass either re-doing the write or folding a row that describes nothing.
	///
	/// The same transaction retires `expired`, the journal rows the in-memory journal has just
	/// dropped as past their grace ([`PendingWrites::expire`](super::engine::PendingWrites::expire)),
	/// so the table holds the window memory does. Before the write's own row: a uuid written again
	/// once its earlier record expired is named there too.
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: the remote
	/// write this journals has already happened (see [`record_write`]).
	pub(crate) fn record_pending(
		&self,
		row: &PendingRow,
		changes: &[BaselineChange<'_>],
		expired: &[Uuid],
	) -> rusqlite::Result<()> {
		self.record_pending_carrying(row, changes, expired, |_| Ok(()))
	}

	/// [`record_pending`](Self::record_pending) for a directory trash that carries other deletions
	/// out along with its own: the same transaction also drops the row of every path `carried` hands
	/// the visitor it is given (see [`DropRow`]).
	pub(crate) fn record_pending_carrying(
		&self,
		row: &PendingRow,
		changes: &[BaselineChange<'_>],
		expired: &[Uuid],
		carried: impl FnOnce(&mut DropRow<'_>) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		record_write(
			row.pair,
			|| {
				let what = match &row.kind {
					PendingKind::Created { path, .. } => format!("create at {path:?}"),
					PendingKind::Moved { from, to } => format!("move {from:?} -> {to:?}"),
					PendingKind::Trashed { path } => format!("trash of {path:?}"),
				};
				format!(
					"the journal row for the {what} of {}, the baseline change(s) it commits with \
					 ({}), and the rows of any deletions it carried",
					row.uuid,
					describe_changes(changes)
				)
			},
			|| {
				let tx = self.conn.unchecked_transaction()?;
				self.write_pending(row.pair, row.uuid, &row.kind, row.recorded_at, expired)?;
				self.write_changes(row.pair, changes)?;
				carried(&mut |path| self.delete_entry_once(row.pair, path))?;
				tx.commit()
			},
		)
	}

	/// A remote write's journal row, after the retirement of `expired`: inside a transaction the
	/// caller opened, with the baseline changes it commits with.
	fn write_pending(
		&self,
		pair: PairId,
		uuid: Uuid,
		kind: &PendingKind,
		recorded_at: i64,
		expired: &[Uuid],
	) -> rusqlite::Result<()> {
		self.delete_pending_rows(expired)?;
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
			PendingKind::Trashed { path } => (KIND_TRASHED, Some(path.as_str()), None, None, None),
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
		Ok(())
	}

	/// Apply several baseline edits in ONE transaction — for a local write that re-keys a whole
	/// subtree, where a crash part-way would leave rows under both spellings.
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: the rename on
	/// disk this re-keys has already happened (see [`record_write`]).
	pub(crate) fn apply_changes(
		&self,
		pair: PairId,
		changes: &[BaselineChange<'_>],
	) -> rusqlite::Result<()> {
		self.apply_changes_carrying(pair, changes, |_| Ok(()))
	}

	/// Write every row `rows` hands the visitor it is given ([`PutRow`]) in ONE transaction, as
	/// [`apply_changes`](Self::apply_changes) writes its upserts: streamed through it rather than
	/// collected first. An error out of `rows` rolls back everything it wrote.
	pub(crate) fn upsert_streamed(
		&self,
		pair: PairId,
		rows: impl FnOnce(&mut PutRow<'_>) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| "the rows a pass confirmed".to_string(),
			|| {
				let tx = self.conn.unchecked_transaction()?;
				rows(&mut |entry| self.upsert_entry_once(pair, entry))?;
				tx.commit()
			},
		)
	}

	/// Write `rows` in ONE transaction: the rows a pass's seeding stage records as synced before it
	/// plans (see [`seed`](super::seed)).
	///
	/// Not wrapped in [`record_write`]: nothing has happened yet that the rows record an act of. A
	/// failure is the pass's error before it has applied anything, and the next pass weighs the
	/// same paths again.
	pub(crate) fn seed_rows(&self, pair: PairId, rows: &[BaselineEntry]) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		for row in rows {
			self.upsert_entry_once(pair, row)?;
		}
		tx.commit()
	}

	/// Replace what `pair` holds of the filen-sync state it was migrated from with the two trees
	/// `items` stages, and return how many paths that imported.
	///
	/// The trees are staged in two `TEMP` tables (see [`LEGACY_STAGE`]) in one transaction, which
	/// writes nothing to the baseline file, and then joined into `legacy_synced` in a second, whose
	/// first statement drops what the pair held before. So the baseline file's write lock is held
	/// for the join alone, and an error out of `items` leaves the pair's rows exactly as they were.
	/// Memory stays flat whatever the size of the trees: they are streamed through the visitor.
	pub(crate) fn replace_legacy_synced<E: From<rusqlite::Error>>(
		&self,
		pair: PairId,
		items: impl FnOnce(&mut PutLegacy<'_>) -> Result<(), E>,
	) -> Result<u64, E> {
		self.conn.execute_batch(LEGACY_STAGE)?;
		// Declared before either transaction, so it drops after both: a drop inside one that then
		// rolls back would bring the tables back.
		let _staged = LegacyStage(&self.conn);
		{
			let tx = self.conn.unchecked_transaction()?;
			let mut local = self.conn.prepare(LEGACY_PUT_LOCAL)?;
			let mut remote = self.conn.prepare(LEGACY_PUT_REMOTE)?;
			items(&mut |item| {
				match item.side {
					LegacySide::Local { mtime } => local.execute(params![
						item.rel_path,
						item.raw,
						item.is_file,
						item.size,
						mtime
					])?,
					LegacySide::Remote { uuid } => remote.execute(params![
						item.rel_path,
						item.raw,
						item.is_file,
						item.size,
						uuid
					])?,
				};
				Ok(())
			})?;
			drop((local, remote));
			tx.commit()?;
		}
		let tx = self.conn.unchecked_transaction()?;
		self.conn.execute(
			"DELETE FROM legacy_synced WHERE pair_id = ?1",
			params![pair],
		)?;
		let imported = self.conn.execute(LEGACY_JOIN, params![pair])?;
		tx.commit()?;
		Ok(u64::try_from(imported).expect("a row count fits in a u64 (should be impossible)"))
	}

	/// How many paths `pair` holds imported filen-sync state for, for the tests that read it back.
	#[cfg(test)]
	pub(crate) fn legacy_synced_count(&self, pair: PairId) -> rusqlite::Result<u64> {
		let count: i64 = self.conn.query_row(
			"SELECT COUNT(*) FROM legacy_synced WHERE pair_id = ?1",
			params![pair],
			|row| row.get(0),
		)?;
		u64::try_from(count).map_err(|_| corrupt("legacy_synced count", count))
	}

	/// Whether `pair` holds any imported filen-sync state: asked once per seeding, so a pair that
	/// imported none looks up none (see [`seed`](super::seed)).
	pub(crate) fn has_legacy_synced(&self, pair: PairId) -> rusqlite::Result<bool> {
		self.conn
			.prepare_cached("SELECT EXISTS (SELECT 1 FROM legacy_synced WHERE pair_id = ?1)")?
			.query_row(params![pair], |row| row.get(0))
	}

	/// What the imported filen-sync state of `pair` recorded as synced at `rel_path`, if anything.
	pub(crate) fn legacy_synced(
		&self,
		pair: PairId,
		rel_path: &str,
	) -> rusqlite::Result<Option<LegacySynced>> {
		self.conn
			.prepare_cached(
				"SELECT remote_uuid, size, local_mtime FROM legacy_synced
				 WHERE pair_id = ?1 AND rel_path = ?2",
			)?
			.query_row(params![pair, rel_path], |row| {
				let size: i64 = row.get(1)?;
				Ok(LegacySynced {
					remote_uuid: row.get(0)?,
					size: u64::try_from(size).map_err(|_| corrupt("legacy_synced size", size))?,
					local_mtime: row.get(2)?,
				})
			})
			.optional()
	}

	/// [`apply_changes`](Self::apply_changes) for a local directory deletion that carries other
	/// deletions out along with its own: the same transaction also drops the row of every path
	/// `carried` hands the visitor it is given (see [`DropRow`]).
	pub(crate) fn apply_changes_carrying(
		&self,
		pair: PairId,
		changes: &[BaselineChange<'_>],
		carried: impl FnOnce(&mut DropRow<'_>) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| {
				format!(
					"the baseline change(s) {} and the rows of any deletions they carried",
					describe_changes(changes)
				)
			},
			|| {
				let tx = self.conn.unchecked_transaction()?;
				self.write_changes(pair, changes)?;
				carried(&mut |path| self.delete_entry_once(pair, path))?;
				tx.commit()
			},
		)
	}

	/// Runs inside a transaction the caller opened, so every statement here is the non-retrying
	/// form: this connection already holds the write lock by the time the second one runs.
	fn write_changes(&self, pair: PairId, changes: &[BaselineChange<'_>]) -> rusqlite::Result<()> {
		for change in changes {
			match change {
				BaselineChange::Upsert(entry) => self.upsert_entry_once(pair, entry)?,
				BaselineChange::Delete(rel_path) => self.delete_entry_once(pair, rel_path)?,
				BaselineChange::MoveSubtree { from, to } => {
					let (folded_from, folded_to) = (collision_key(from), collision_key(to));
					let is_rule_file = to.rsplit('/').next() == Some(FILENIGNORE);
					self.conn.prepare_cached(MOVE_AT_PATH)?.execute(params![
						pair,
						from,
						to,
						folded_from,
						folded_to,
						is_rule_file
					])?;
					self.conn.prepare_cached(MOVE_UNDER_PATH)?.execute(params![
						pair,
						from,
						to,
						folded_from,
						folded_to
					])?;
				}
			}
		}
		Ok(())
	}

	/// Count one failed attempt at `rel_path`, made at `at` (unix millis), remembering what went
	/// wrong. Consecutive: a success or a [`clear_failure`](Self::clear_failure) resets the count to
	/// nothing.
	pub(crate) fn record_failure(
		&self,
		pair: PairId,
		rel_path: &str,
		error: &str,
		at: i64,
	) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT INTO path_failures (pair_id, rel_path, attempts, last_error, last_failure_at)
			 VALUES (?1, ?2, 1, ?3, ?4)
			 ON CONFLICT (pair_id, rel_path)
			 DO UPDATE SET attempts = attempts + 1, last_error = excluded.last_error,
			 last_failure_at = excluded.last_failure_at",
			params![pair, rel_path, error, at],
		)?;
		Ok(())
	}

	/// Count one failed attempt at `rel_path`, made at `at` (unix millis), as
	/// [`record_failure`](Self::record_failure) does — but only when the streak's last failure is
	/// at least `spacing` (millis) older, or there is no streak yet. A failure inside the window
	/// changes nothing, not even the remembered error; one dated before the last (the clock went
	/// back) is inside it.
	pub(crate) fn record_spaced_failure(
		&self,
		pair: PairId,
		rel_path: &str,
		error: &str,
		at: i64,
		spacing: i64,
	) -> rusqlite::Result<()> {
		self.conn.execute(
			"INSERT INTO path_failures (pair_id, rel_path, attempts, last_error, last_failure_at)
			 VALUES (?1, ?2, 1, ?3, ?4)
			 ON CONFLICT (pair_id, rel_path)
			 DO UPDATE SET attempts = attempts + 1, last_error = excluded.last_error,
			 last_failure_at = excluded.last_failure_at
			 WHERE path_failures.last_failure_at <= excluded.last_failure_at - ?5",
			params![pair, rel_path, error, at, spacing],
		)?;
		Ok(())
	}

	/// Forget `rel_path`'s failure streak — it succeeded, or the caller asked for a retry.
	pub(crate) fn clear_failure(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		self.clear_failures(pair, std::slice::from_ref(&rel_path))
	}

	/// Forget the failure streaks of every path in `rel_paths`, in ONE transaction: a pass clears
	/// one per streak its actions ended, and each on its own is a commit of its own.
	pub(crate) fn clear_failures(&self, pair: PairId, rel_paths: &[&str]) -> rusqlite::Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		{
			let mut clear = self
				.conn
				.prepare("DELETE FROM path_failures WHERE pair_id = ?1 AND rel_path = ?2")?;
			for rel_path in rel_paths {
				clear.execute(params![pair, rel_path])?;
			}
		}
		tx.commit()
	}

	/// Every path under `pair` with a live failure streak, by `rel_path`.
	pub(crate) fn failures(
		&self,
		pair: PairId,
	) -> rusqlite::Result<std::collections::HashMap<String, PathFailure>> {
		self.conn
			.prepare(
				"SELECT rel_path, attempts, last_error, last_failure_at FROM path_failures
				 WHERE pair_id = ?1",
			)?
			.query_map(params![pair], |row| {
				let attempts: i64 = row.get("attempts")?;
				Ok((
					row.get::<_, String>("rel_path")?,
					PathFailure {
						// A streak past `u32::MAX` failures is still past the threshold.
						attempts: u32::try_from(attempts.max(0)).unwrap_or(u32::MAX),
						last_error: row.get("last_error")?,
						last_failure_at: row.get("last_failure_at")?,
					},
				))
			})?
			.collect()
	}

	/// Retire journal rows the in-memory journal has dropped (the cache caught up, or the grace
	/// window ran out).
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: the caller has
	/// already retired these records in memory, and a record left standing in the DB alone comes
	/// back on the next open (see [`record_write`]).
	///
	/// `pair` is the pair whose connection this is, for that log line. The rows themselves are keyed
	/// by uuid alone, and a pass retires whatever the cache has caught up to whichever pair wrote it.
	pub(crate) fn delete_pending(&self, pair: PairId, uuids: &[Uuid]) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| format!("the retirement of {} journal row(s)", uuids.len()),
			|| {
				let tx = self.conn.unchecked_transaction()?;
				self.delete_pending_rows(uuids)?;
				tx.commit()
			},
		)
	}

	/// The DELETEs behind [`delete_pending`](Self::delete_pending), inside the caller's transaction.
	fn delete_pending_rows(&self, uuids: &[Uuid]) -> rusqlite::Result<()> {
		let mut stmt = self
			.conn
			.prepare_cached("DELETE FROM pending_writes WHERE uuid = ?1")?;
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
			KIND_TRASHED => PendingKind::Trashed {
				path: text("path")?,
			},
			_ => return Err(corrupt("pending write kind", kind_raw)),
		};
		Ok(PendingRow {
			pair: row.get("pair_id")?,
			uuid: row.get("uuid")?,
			kind,
			recorded_at: row.get("recorded_at")?,
		})
	}

	/// A row as [`ENTRY_COLUMNS`] lists its columns, read by POSITION: every row of every
	/// enumeration a pass makes comes through here, and a lookup by name is a walk of the column
	/// names per field. `the_entry_columns_are_read_in_the_order_they_are_listed` holds the two lists
	/// together.
	pub(super) fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<BaselineEntry> {
		let kind_raw: i64 = row.get(1)?;
		let state_raw: i64 = row.get(7)?;
		// Straight into the fixed-size array rusqlite decodes blobs into — no `Vec` per hash, and a
		// blob of the wrong length is refused by the column read itself rather than by a check of
		// ours. Three of these per row, on every row of every pass's baseline read.
		let hash = |column| -> rusqlite::Result<Option<Blake3Hash>> {
			Ok(row
				.get::<_, Option<[u8; 32]>>(column)?
				.map(Blake3Hash::from))
		};
		let side_kind = |raw: Option<i64>| match raw {
			None => Ok(None),
			Some(raw) => NodeKind::from_i64(raw)
				.map(Some)
				.ok_or_else(|| corrupt("kind", raw)),
		};
		Ok(BaselineEntry {
			rel_path: row.get(0)?,
			kind: NodeKind::from_i64(kind_raw).ok_or_else(|| corrupt("kind", kind_raw))?,
			remote_uuid: row.get(2)?,
			content_hash: hash(3)?,
			size: row.get::<_, Option<i64>>(4)?.map(|s| s as u64),
			local_mtime: row.get(5)?,
			remote_modified: row.get(6)?,
			state: BaselineState::from_i64(state_raw).ok_or_else(|| corrupt("state", state_raw))?,
			local_kind: side_kind(row.get(8)?)?,
			remote_kind: side_kind(row.get(9)?)?,
			remote_hash: hash(10)?,
			remote_size: row.get::<_, Option<i64>>(11)?.map(|s| s as u64),
			remote_stable_uuid: row.get(12)?,
			agreed_hash: hash(13)?,
		})
	}
}

#[cfg(test)]
mod tests;
