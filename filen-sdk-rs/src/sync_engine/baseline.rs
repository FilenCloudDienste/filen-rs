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
	cell::RefCell,
	collections::{BTreeSet, HashMap},
	path::Path,
	sync::Arc,
};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use uuid::Uuid;

use super::{engine::PendingKind, guard::DeleteGuard, mode::SyncMode, rows::Baseline, tree::Tree};

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
const SCHEMA_VERSION: i64 = 2;

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
";

/// The columns [`BaselineStore::row_to_entry`] reads, named once so the statements that hand back
/// rows cannot drift apart from each other or from it.
/// Objects added to the schema after version 1 was first written, created on EVERY open rather
/// than only on a fresh DB: [`SCHEMA`] runs once, when the file holds no table of ours, so an index
/// added to it alone would never reach a DB that already exists. `IF NOT EXISTS` makes the re-run
/// free, and an index changes no row's MEANING, so [`SCHEMA_VERSION`] does not move — an older
/// build opening the file just plans its own reads without it.
///
/// The one object here serves the conflict read ([`BaselineStore::conflicts`]): the rows a pair
/// holds in conflict are a handful out of a whole tree, and finding them without the index is a
/// walk of every row of the pair — a walk that runs under the pair's store mutex.
const ADDITIVE_SCHEMA: &str = "
CREATE INDEX IF NOT EXISTS baseline_state ON baseline (pair_id, state);
CREATE INDEX IF NOT EXISTS baseline_remote_uuid ON baseline (pair_id, remote_uuid);
CREATE INDEX IF NOT EXISTS baseline_remote_stable_uuid ON baseline (pair_id, remote_stable_uuid);
";

const ENTRY_COLUMNS: &str = "rel_path, kind, remote_uuid, content_hash, size, local_mtime,
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
	  remote_stable_uuid, agreed_hash)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)";
const DELETE_ENTRY: &str = "DELETE FROM baseline WHERE pair_id = ?1 AND rel_path = ?2";

/// The id seek [`BaselineStore::synced_paths`] runs against `column`: one `IN` list of exactly
/// [`SYNCED_CHUNK`] placeholders, so the text is the same on every call and `prepare_cached` plans
/// it once. Two texts exist, one per indexed column, and building either is a handful of pushes
/// against the two statements a pass runs at most.
fn synced_by(column: &str) -> String {
	let binds = (2..=SYNCED_CHUNK + 1)
		.map(|at| format!("?{at}"))
		.collect::<Vec<_>>()
		.join(", ");
	format!(
		"SELECT rel_path, remote_uuid, remote_stable_uuid FROM baseline
		 WHERE pair_id = ?1 AND {column} IN ({binds})"
	)
}

/// The two statements a directory move runs, in this order: the row AT `?2`, then every row under
/// it, each swapping that prefix for `?3` over the same bytewise ranges the deletes above use.
///
/// Two statements for the same reason the deletes are two. Asking one `UPDATE` for both — `rel_path
/// = ?2 OR (rel_path > ?2 || '/' AND rel_path < ?2 || '0')` — plans as `SEARCH baseline USING INDEX
/// sqlite_autoindex_baseline_1 (pair_id=?)`: an `OR` of a point and a range is not sargable, so it
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
const MOVE_AT_PATH: &str =
	"UPDATE OR REPLACE baseline SET rel_path = ?3 WHERE pair_id = ?1 AND rel_path = ?2";
const MOVE_UNDER_PATH: &str =
	"UPDATE OR REPLACE baseline SET rel_path = ?3 || substr(rel_path, length(?2) + 1)
	 WHERE pair_id = ?1 AND rel_path > ?2 || '/' AND rel_path < ?2 || '0'";

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

impl BaselineEntry {
	/// Whether this row stands in for BOTH sides of a pass — the one rule that says what a
	/// change-scoped pass may carry forward instead of re-reading.
	///
	/// It lives here, on the row it is about, because THREE readers need the same answer and a
	/// second copy of the field list is how a new column comes to be carried by one of them and not
	/// another: [`derive::carried`](super::derive) builds the two nodes from a row that passes,
	/// [`Tree`](super::tree::Tree) indexes the rows that fail so a pass can find them all
	/// without walking the tree, and a carried side answers "does this side hold the path" straight
	/// off that index without building a node at all.
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

/// A registered sync pair, as returned by [`SyncEngine::list_pairs`](super::SyncEngine::list_pairs).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
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

/// The baseline DB handle (sole owner / single writer).
pub(crate) struct BaselineStore {
	conn: Connection,
	/// Each pair's rows as they stand, read whole from the DB once and kept in step by every write
	/// path below (see [`BaselineStore::baseline`]). A pass reads its baseline from here instead of
	/// paying the whole-tree `SELECT` and the map build every time.
	///
	/// A [`RefCell`] because every write path takes `&self` — the connection is already the single
	/// writer, serialized by the pair's store mutex, so there is no second borrower to race. The
	/// store is `Send` and never `Sync`, exactly as the `Connection` inside it already makes it.
	resident: RefCell<HashMap<PairId, Arc<Tree>>>,
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
fn conflict_rows_sql() -> String {
	format!("SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 AND state IN (?2, ?3)")
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
	/// when it carries exactly that version: anything else — older or newer — is refused rather than
	/// read, since there is no migration chain to bring it here and reading foreign rows under these
	/// rules would misplan them into deletes.
	fn init(conn: Connection) -> Result<Self, crate::Error> {
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
		conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")
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
			resident: RefCell::new(HashMap::new()),
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
				// The non-mirroring form: these rows are inside the transaction below, and a copy
				// updated per row would keep the rows a rollback took back (see `note_written`).
				self.upsert_entry_once(id, entry)?;
			}
		}
		tx.commit()?;
		if changed > 0 {
			self.note_written(id, |rows| {
				for entry in adopted {
					rows.upsert(entry);
				}
			});
		}
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
		self.forget_resident(id);
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
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: a pass writes
	/// this row AFTER the transfer, rename or trash it records (see [`record_write`]).
	pub(crate) fn upsert_entry(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| format!("the baseline row for {:?}", entry.rel_path),
			|| self.upsert_entry_once(pair, entry),
		)
		.inspect(|()| {
			self.note_written(pair, |rows| rows.upsert(entry));
		})
	}

	/// [`upsert_entry`](Self::upsert_entry) without the retry, for the callers already inside a
	/// transaction of this connection's — which holds the write lock, so there is nothing left to
	/// wait for and a retry of the statement alone would re-run it inside a transaction that has
	/// already failed.
	fn upsert_entry_once(&self, pair: PairId, entry: &BaselineEntry) -> rusqlite::Result<()> {
		self.conn.prepare_cached(UPSERT_ENTRY)?.execute(params![
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
		])?;
		Ok(())
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

	/// `pair`'s rows as a [`Baseline`] over the resident [`Tree`], read from the DB on the first call and kept
	/// in step by every write path here afterwards.
	///
	/// This is what a pass reconciles against, so it is read once per PAIR rather than once per
	/// pass: at 100k rows the `SELECT` and the tree build it replaces cost ~180 ms, which an idle
	/// pass would otherwise pay to find out that nothing changed.
	///
	/// This process's store is assumed to be the only writer of the pair's rows — it is, for every
	/// path in the engine — and a second writer on the same file (another engine, another process)
	/// would leave the copy describing rows that connection has since changed. Nothing enforces it.
	///
	/// Handed out as an [`Arc`], and every write below applies itself through
	/// [`Arc::make_mut`]: while a pass holds a copy, that pass's view stays exactly the one it read
	/// — the pass reconciles against a fixed baseline and writes the rows it advances — and the
	/// FIRST write of such a pass clones the tree once, after which the store owns it alone again
	/// and the rest of that pass's writes land in place. A pass that writes nothing clones nothing.
	pub(crate) fn baseline(&self, pair: PairId) -> rusqlite::Result<Baseline> {
		if let Some(rows) = self.resident.borrow().get(&pair) {
			return Ok(Baseline::resident(Arc::clone(rows)));
		}
		let rows = Arc::new(self.read_tree(pair)?);
		self.resident.borrow_mut().insert(pair, Arc::clone(&rows));
		Ok(Baseline::resident(rows))
	}

	/// `pair`'s rows as a [`Tree`], built from them AS THEY ARRIVE.
	///
	/// Not `Tree::from_rows(self.entries(pair)?)`, which is the same tree by way of a `Vec` of
	/// every row the pair has. That `Vec` is the widest thing a process holding an idle pair ever
	/// builds: at a million rows it is ~300 MiB of `BaselineEntry` and their paths, alive beside
	/// the tree being built out of it — and once freed, the pages an allocator has not returned to
	/// the kernel are still resident, so a pair loaded that way costs its own size twice over for
	/// the life of the process. One row at a time costs one row.
	///
	/// The `ORDER BY` is `entries`'s, kept because the tree is built by `upsert`
	/// per row: parent before child for same-prefix paths, which is the order a load is cheapest
	/// in and the order both read paths agree on.
	fn read_tree(&self, pair: PairId) -> rusqlite::Result<Tree> {
		let mut statement = self.conn.prepare(&format!(
			"SELECT {ENTRY_COLUMNS} FROM baseline WHERE pair_id = ?1 ORDER BY rel_path"
		))?;
		let mut rows = statement.query(params![pair])?;
		let mut tree = Tree::default();
		while let Some(row) = rows.next()? {
			tree.upsert(&Self::row_to_entry(row)?);
		}
		tree.shrink_after_load();
		Ok(tree)
	}

	/// Apply to the resident copy what a write that has just COMMITTED did to the DB. Called on the
	/// success path only: a write that failed changed no row, and mirroring it would make the
	/// resident copy describe a DB that does not exist.
	///
	/// A pair nothing has read yet has no resident copy, and gains one from the DB when something
	/// asks: there is nothing to keep in step until then.
	fn note_written(&self, pair: PairId, apply: impl FnOnce(&mut Tree)) {
		if let Some(rows) = self.resident.borrow_mut().get_mut(&pair) {
			apply(Arc::make_mut(rows));
		}
	}

	/// Drop `pair`'s resident copy: its rows are gone, or are about to be re-read from scratch.
	fn forget_resident(&self, pair: PairId) {
		self.resident.borrow_mut().remove(&pair);
	}

	/// The rows `pair` is holding in conflict — both flavours — ordered by path.
	///
	/// A `WHERE` on the state rather than a read of the whole pair filtered in Rust: what a caller
	/// listing the conflicts wants is a handful of rows, and reading every row of the pair to find
	/// them is the same work a pass does. [`ADDITIVE_SCHEMA`]'s `(pair_id, state)` index is what
	/// makes it a seek of those rows rather than that same walk with the filtering moved into
	/// SQLite.
	pub(crate) fn conflicts(&self, pair: PairId) -> rusqlite::Result<Vec<BaselineEntry>> {
		let mut held: Vec<BaselineEntry> = self
			.conn
			.prepare(&conflict_rows_sql())?
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
		.inspect(|()| {
			self.note_written(pair, |rows| {
				rows.remove(rel_path);
			});
		})
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
		tx.commit().inspect(|()| {
			self.note_written(pair, |rows| rows.remove_subtrees(roots));
		})
	}

	/// Journal a remote write AND apply the baseline edits it produced, in ONE transaction.
	///
	/// The two have to land together: the journal row is what a reopened engine folds into its
	/// remote view, and the baseline row is the written state it folds. A crash between them leaves
	/// the next pass either re-doing the write or folding a row that describes nothing.
	///
	/// Fails loudly rather than waiting when another connection holds the write lock: the remote
	/// write this journals has already happened (see [`record_write`]).
	pub(crate) fn record_pending(
		&self,
		pair: PairId,
		uuid: Uuid,
		kind: &PendingKind,
		recorded_at: i64,
		changes: &[BaselineChange<'_>],
	) -> rusqlite::Result<()> {
		record_write(
			pair,
			|| {
				let what = match kind {
					PendingKind::Created { path, .. } => format!("create at {path:?}"),
					PendingKind::Moved { from, to } => format!("move {from:?} -> {to:?}"),
					PendingKind::Trashed { path } => format!("trash of {path:?}"),
				};
				format!(
					"the journal row for the {what} of {uuid}, and the baseline change(s) it commits \
					 with ({})",
					describe_changes(changes)
				)
			},
			|| self.record_pending_once(pair, uuid, kind, recorded_at, changes),
		)
		.inspect(|()| self.note_written(pair, |rows| rows.apply(changes)))
	}

	fn record_pending_once(
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
		self.write_changes(pair, changes)?;
		tx.commit()
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
		record_write(
			pair,
			|| format!("the baseline change(s) {}", describe_changes(changes)),
			|| {
				let tx = self.conn.unchecked_transaction()?;
				self.write_changes(pair, changes)?;
				tx.commit()
			},
		)
		.inspect(|()| self.note_written(pair, |rows| rows.apply(changes)))
	}

	/// Runs inside a transaction the caller opened, so every statement here is the non-retrying
	/// form: this connection already holds the write lock by the time the second one runs.
	fn write_changes(&self, pair: PairId, changes: &[BaselineChange<'_>]) -> rusqlite::Result<()> {
		for change in changes {
			match change {
				BaselineChange::Upsert(entry) => self.upsert_entry_once(pair, entry)?,
				BaselineChange::Delete(rel_path) => self.delete_entry_once(pair, rel_path)?,
				BaselineChange::MoveSubtree { from, to } => {
					self.conn
						.prepare_cached(MOVE_AT_PATH)?
						.execute(params![pair, from, to])?;
					self.conn
						.prepare_cached(MOVE_UNDER_PATH)?
						.execute(params![pair, from, to])?;
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

	/// Forget `rel_path`'s failure streak — it succeeded, or the caller asked for a retry.
	pub(crate) fn clear_failure(&self, pair: PairId, rel_path: &str) -> rusqlite::Result<()> {
		self.clear_failures(pair, std::slice::from_ref(&rel_path))
	}

	/// Forget the failure streaks of every path in `rel_paths`, in ONE transaction: a pass clears
	/// one per path it applied, and each on its own is a commit of its own.
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
						attempts: attempts.max(0) as u32,
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
				{
					let mut stmt = self
						.conn
						.prepare_cached("DELETE FROM pending_writes WHERE uuid = ?1")?;
					for uuid in uuids {
						stmt.execute(params![uuid])?;
					}
				}
				tx.commit()
			},
		)
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

	fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<BaselineEntry> {
		let kind_raw: i64 = row.get("kind")?;
		let state_raw: i64 = row.get("state")?;
		// Straight into the fixed-size array rusqlite decodes blobs into — no `Vec` per hash, and a
		// blob of the wrong length is refused by the column read itself rather than by a check of
		// ours. Three of these per row, on every row of every pass's baseline read.
		let hash = |column| -> rusqlite::Result<Option<Blake3Hash>> {
			Ok(row
				.get::<_, Option<[u8; 32]>>(column)?
				.map(Blake3Hash::from))
		};
		let content_hash = hash("content_hash")?;
		let remote_hash = hash("remote_hash")?;
		let agreed_hash = hash("agreed_hash")?;
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

	/// A pragma's value as this connection reports it.
	fn pragma<T: rusqlite::types::FromSql>(store: &BaselineStore, name: &str) -> T {
		store
			.conn
			.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))
			.unwrap()
	}

	/// A write a pass makes AFTER the act it records must FAIL once another pair's connection has
	/// held the write lock past the busy timeout, rather than waiting it out: a wedged writer must not
	/// hang a pass. The act then stands unrecorded, which the log and the pass's report say — and
	/// which every site recovers from on a later pass (see [`record_write`]).
	#[test]
	fn a_journal_write_under_a_held_write_lock_fails_instead_of_waiting() {
		let path = temp_db_path("busy_journal");
		let holder = BaselineStore::open(&path).unwrap();
		let (pair, _) = holder
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();

		let writer = BaselineStore::open(&path).unwrap();
		// A real open waits 30 s, which no test can sit out; 50 ms reaches the same answer.
		writer.busy_timeout_for_test(std::time::Duration::from_millis(50));
		{
			let _held = holder
				.hold_write_lock(pair, &file_entry("a.txt", [1; 32], 3))
				.unwrap();
			let started = std::time::Instant::now();
			let error = writer
				.record_pending(pair, Uuid::new_v4(), &created("a.txt"), NOW, &[])
				.expect_err("the contended journal write must fail rather than wait for the lock");
			assert!(
				is_write_lock_contention(&error),
				"the failure must be the write lock and not something else: {error}"
			);
			assert!(
				started.elapsed() < std::time::Duration::from_secs(5),
				"it waited {:?}: the bound is the busy timeout, not the holder's lifetime",
				started.elapsed()
			);
			assert_eq!(pending_count(&writer), 0, "nothing was recorded");
		}

		// With the lock free the same write lands, so the failure above was the contention and not a
		// connection this test broke.
		writer
			.record_pending(pair, Uuid::new_v4(), &created("a.txt"), NOW, &[])
			.expect("the write must land once the lock is free");
		assert_eq!(pending_count(&writer), 1);

		drop(writer);
		drop(holder);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
		}
	}

	/// Every open runs in WAL with `synchronous = NORMAL` and a busy timeout — the reopen too, which
	/// takes the early-returning path through `init`. Two of the three are per-connection, so an
	/// open that skipped them would quietly go back to an fsync per commit, which is what made the
	/// row-per-action writes of a first sync cost a quarter of a millisecond each.
	#[test]
	fn every_open_runs_in_wal_with_normal_sync_and_a_busy_timeout() {
		let path = temp_db_path("wal");
		let assert_pragmas = |store: &BaselineStore| {
			assert_eq!(pragma::<String>(store, "journal_mode"), "wal");
			assert_eq!(pragma::<i64>(store, "synchronous"), 1, "NORMAL is 1");
			assert_eq!(pragma::<i64>(store, "busy_timeout"), 30_000);
		};
		let entry = file_entry("a.txt", [1u8; 32], 3);
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			assert_pragmas(&store);
			let (pair, _) = store
				.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
				.unwrap();
			store.upsert_entry(pair, &entry).unwrap();
			pair
		};

		let reopened = BaselineStore::open(&path).unwrap();
		assert_pragmas(&reopened);
		assert_eq!(
			reopened.entry(pair, "a.txt").unwrap().as_ref(),
			Some(&entry),
			"the row written under WAL must read back after the reopen"
		);
		drop(reopened);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
		}
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

	/// A DB this build refuses is not written to on the way out. The journal mode is the one pragma
	/// that persists in the file HEADER, so setting it before the version check switched a stranger's
	/// DB — one a newer build wrote — to WAL and left `-wal`/`-shm` sidecars beside it, and a
	/// read-only file failed at the pragma instead of being refused with the message that explains
	/// what to do about it.
	#[test]
	fn a_refused_db_keeps_the_journal_mode_it_arrived_in() {
		let path = temp_db_path("refused_journal_mode");
		let journal_mode = |conn: &Connection| -> String {
			conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))
				.unwrap()
		};
		{
			// Not opened through `BaselineStore`: this file has never been in WAL, and it has a
			// table, so it reads as written rather than brand new.
			let raw = Connection::open(&path).unwrap();
			raw.execute_batch(&format!(
				"CREATE TABLE something (id INTEGER PRIMARY KEY);
				 PRAGMA user_version = {};",
				SCHEMA_VERSION + 1
			))
			.unwrap();
			assert_eq!(
				journal_mode(&raw),
				"delete",
				"the fixture starts in rollback"
			);
		}

		match BaselineStore::open(&path) {
			Ok(_) => panic!("a foreign schema version must be refused"),
			Err(error) => assert!(
				error.to_string().contains("refusing"),
				"the refusal must name the version problem: {error}"
			),
		}

		let raw = Connection::open(&path).unwrap();
		assert_eq!(
			journal_mode(&raw),
			"delete",
			"the refused DB was switched to WAL on the way to the refusal"
		);
		drop(raw);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
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

	/// A fresh DB carries the user ignore table, reads it as empty, and keeps what is set across a
	/// reopen.
	#[test]
	fn user_ignore_patterns_start_empty_and_survive_a_reopen() {
		let path = temp_db_path("user_ignore");
		{
			let store = BaselineStore::open(&path).unwrap();
			let tables: i64 = store
				.conn
				.query_row(
					"SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'user_ignore'",
					[],
					|row| row.get(0),
				)
				.unwrap();
			assert_eq!(tables, 1, "a fresh DB has no user_ignore table");
			assert_eq!(store.user_ignore().unwrap(), "");
			store.set_user_ignore("*.psd").unwrap();
			store.set_user_ignore("*.psd\n!keep.psd").unwrap();
		}
		let store = BaselineStore::open(&path).unwrap();
		assert_eq!(store.user_ignore().unwrap(), "*.psd\n!keep.psd");
		drop(store);
		std::fs::remove_file(&path).ok();
	}

	/// A pair's recorded ignored roots are replaced whole, kept apart from another pair's, and go
	/// with the pair.
	#[test]
	fn ignored_roots_are_replaced_per_pair_and_go_with_it() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/a", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/b", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		assert!(store.ignored_roots(pair).unwrap().is_empty());
		let roots = |list: &[&str]| -> BTreeSet<String> {
			list.iter().map(|root| (*root).to_string()).collect()
		};
		store
			.set_ignored_roots(pair, &roots(&["build", "ä/.DS_Store"]))
			.unwrap();
		store.set_ignored_roots(other, &roots(&["cache"])).unwrap();
		store.set_ignored_roots(pair, &roots(&["build"])).unwrap();
		assert_eq!(store.ignored_roots(pair).unwrap(), roots(&["build"]));
		assert_eq!(store.ignored_roots(other).unwrap(), roots(&["cache"]));
		store.delete_pair(pair).unwrap();
		assert!(store.ignored_roots(pair).unwrap().is_empty());
		assert_eq!(store.ignored_roots(other).unwrap(), roots(&["cache"]));
	}

	#[test]
	fn path_failures_count_consecutively_and_reset_on_a_clear() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		assert!(store.failures(pair).unwrap().is_empty());

		store.record_failure(pair, "a.txt", "boom", 1_000).unwrap();
		store
			.record_failure(pair, "a.txt", "boom again", 2_000)
			.unwrap();
		store.record_failure(pair, "b.txt", "other", 1_500).unwrap();
		let failures = store.failures(pair).unwrap();
		assert_eq!(
			failures["a.txt"],
			PathFailure {
				attempts: 2,
				last_error: "boom again".to_string(),
				last_failure_at: 2_000,
			},
			"the streak counts up and keeps the LAST error and the LAST failure's time"
		);
		assert_eq!(failures["b.txt"].attempts, 1);
		assert_eq!(failures["b.txt"].last_failure_at, 1_500);

		// Clearing one path leaves the other alone, and a later failure starts from 1 again.
		store.clear_failure(pair, "a.txt").unwrap();
		assert!(!store.failures(pair).unwrap().contains_key("a.txt"));
		store.record_failure(pair, "a.txt", "fresh", 3_000).unwrap();
		assert_eq!(store.failures(pair).unwrap()["a.txt"].attempts, 1);
		assert_eq!(store.failures(pair).unwrap()["b.txt"].attempts, 1);

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
	/// Clearing a pass's applied paths in one go has to write what clearing them one at a time
	/// wrote: those streaks and no others, a path with no streak still a no-op.
	#[test]
	fn clearing_several_paths_at_once_leaves_every_other_streak_alone() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		for path in ["a.txt", "b.txt", "c.txt"] {
			store.record_failure(pair, path, "boom", 1_000).unwrap();
			store.record_failure(other, path, "boom", 1_000).unwrap();
		}

		store
			.clear_failures(pair, &["a.txt", "c.txt", "never-failed.txt"])
			.unwrap();
		assert_eq!(
			store
				.failures(pair)
				.unwrap()
				.into_keys()
				.collect::<BTreeSet<_>>(),
			BTreeSet::from(["b.txt".to_string()])
		);
		assert_eq!(
			store.failures(other).unwrap().len(),
			3,
			"another pair's streaks are not this pair's to clear"
		);
	}

	/// Retiring a batch of journal rows retires exactly the named ones.
	#[test]
	fn delete_pending_retires_exactly_the_named_writes() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let uuids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();
		for uuid in &uuids {
			store
				.record_pending(pair, *uuid, &created("a.txt"), NOW, &[])
				.unwrap();
		}

		store.delete_pending(pair, &uuids[..3]).unwrap();
		assert_eq!(pending_count(&store), 1);
		assert_eq!(
			store
				.load_pending(NOW, GRACE)
				.unwrap()
				.iter()
				.map(|row| row.uuid)
				.collect::<Vec<_>>(),
			vec![uuids[3]]
		);
	}

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

	/// The resident tree is built from the rows as they arrive; the materialized read is the same
	/// rows by way of a `Vec`. The two must be the same tree — same rows, same order, same
	/// parents — or the read that a pass actually uses is not the one the tests cover.
	///
	/// Handed the rows in OPPOSITE orders on purpose. Both reads issue the same `ORDER BY
	/// rel_path`, so comparing them as they come is comparing a loop with itself; reversing one
	/// side puts every child before its own directory and every sibling backwards, which is the
	/// only way to ask whether the tree a pass loads depends on the order its rows arrived in.
	#[test]
	fn the_streamed_load_builds_the_tree_the_materialized_read_builds() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		assert_eq!(
			store.baseline(pair).unwrap().len(),
			0,
			"a pair with no rows loads an empty tree"
		);

		// Written out of path order, and with a child before its own directory — though the reads
		// below both sort, so this is the DB's fidelity and not yet the question.
		for entry in [
			file_entry("a/deep/b.txt", [9u8; 32], 4242),
			dir_entry("a"),
			file_entry("c.txt", [1u8; 32], 7),
			dir_entry("a/deep"),
		] {
			store.upsert_entry(pair, &entry).unwrap();
		}
		// The upserts above mirrored themselves into the copy the first read made, and this is a
		// test of the READ: make the store go back to the DB for it.
		store.forget_resident(pair);

		let streamed = store.baseline(pair).unwrap();
		let mut rows = store.entries(pair).unwrap();
		rows.reverse();
		let materialized = Baseline::from_rows(rows);
		assert_eq!(streamed.len(), materialized.len(), "same row count");
		assert_eq!(
			streamed.iter().collect::<Vec<_>>(),
			materialized.iter().collect::<Vec<_>>(),
			"same rows, in the same walk order, from opposite arrival orders"
		);
		assert_eq!(
			streamed.len(),
			4,
			"and it is the four rows that were written, not an empty tree both paths agree on"
		);
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

	/// Both conflict states come back, in path order, and nothing else does: a synced or adopted row
	/// is not a conflict, and another pair's conflict is not this pair's.
	#[test]
	fn conflicts_are_read_by_state_in_path_order() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let with_state = |rel_path: &str, state| BaselineEntry {
			state,
			..file_entry(rel_path, [1u8; 32], 1)
		};
		for (rel_path, state) in [
			("b.txt", BaselineState::Conflicted),
			("a.txt", BaselineState::Overwritten),
			("c.txt", BaselineState::Synced),
			("d.txt", BaselineState::Adopted),
		] {
			store
				.upsert_entry(pair, &with_state(rel_path, state))
				.unwrap();
		}
		store
			.upsert_entry(other, &with_state("z.txt", BaselineState::Conflicted))
			.unwrap();

		assert_eq!(
			store
				.conflicts(pair)
				.unwrap()
				.iter()
				.map(|entry| (entry.rel_path.as_str(), entry.state))
				.collect::<Vec<_>>(),
			vec![
				("a.txt", BaselineState::Overwritten),
				("b.txt", BaselineState::Conflicted)
			]
		);
		assert_eq!(store.conflicts(other).unwrap().len(), 1);
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

	/// The resident copy a pass reconciles against has to describe the DB after EVERY write path,
	/// or a pass reads rows that are not there (a fabricated absence) or misses rows that are.
	/// Every path that writes a baseline row is driven here and the copy compared against a fresh
	/// read of the file.
	#[test]
	fn every_write_path_keeps_the_resident_copy_equal_to_the_db() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		// From the DB: the copy exists from here on, so every write below has one to keep in step.
		assert!(store.baseline(pair).unwrap().is_empty());

		let agrees = |store: &BaselineStore, what: &str| {
			let from_db: HashMap<String, BaselineEntry> = store
				.entries(pair)
				.unwrap()
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect();
			let resident: HashMap<String, BaselineEntry> = store
				.baseline(pair)
				.unwrap()
				.iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect();
			assert_eq!(resident, from_db, "after {what}");
		};

		store
			.upsert_entry(pair, &file_entry("a/x.txt", [1; 32], 1))
			.unwrap();
		store.upsert_entry(pair, &dir_entry("a")).unwrap();
		store.upsert_entry(pair, &dir_entry("a/sub")).unwrap();
		store
			.upsert_entry(pair, &file_entry("a/sub/y.txt", [2; 32], 2))
			.unwrap();
		store.upsert_entry(pair, &dir_entry("ab")).unwrap();
		agrees(&store, "upsert_entry");

		store.delete_entry(pair, "ab").unwrap();
		agrees(&store, "delete_entry");

		let replaced = file_entry("a/x.txt", [3; 32], 3);
		store
			.apply_changes(
				pair,
				&[
					BaselineChange::Upsert(&replaced),
					BaselineChange::Upsert(&dir_entry("b")),
				],
			)
			.unwrap();
		agrees(&store, "apply_changes(Upsert)");

		// Onto an OCCUPIED destination, which the two `UPDATE OR REPLACE` statements overwrite.
		store
			.apply_changes(pair, &[BaselineChange::MoveSubtree { from: "a", to: "b" }])
			.unwrap();
		agrees(&store, "apply_changes(MoveSubtree)");

		// A case-only rename shares no key with itself under a bytewise comparison.
		store
			.apply_changes(pair, &[BaselineChange::MoveSubtree { from: "b", to: "B" }])
			.unwrap();
		agrees(&store, "apply_changes(MoveSubtree), case only");

		store
			.record_pending(
				pair,
				Uuid::new_v4(),
				&created("B/x.txt"),
				NOW,
				&[
					BaselineChange::Upsert(&file_entry("B/x.txt", [4; 32], 4)),
					BaselineChange::Delete("B/sub/y.txt"),
				],
			)
			.unwrap();
		agrees(&store, "record_pending");

		store
			.set_mode(
				pair,
				SyncMode::LocalToRemote,
				&[
					dir_entry("adopted"),
					file_entry("adopted/z.txt", [5; 32], 5),
				],
			)
			.unwrap();
		agrees(&store, "set_mode");

		store
			.delete_subtrees(pair, &BTreeSet::from(["B".to_string()]))
			.unwrap();
		agrees(&store, "delete_subtrees");
	}

	/// A write that FAILED changed no row, so the copy must still describe the DB as it still is.
	/// Triggers refuse one path per write shape, which is the only way to fail a write on purpose:
	/// contention fails a write before it has run a statement, and what has to be covered here is a
	/// write that fails HALF WAY — the second row of an adoption, whose rows land inside the mode
	/// switch's own transaction, so a rollback takes back the row the loop already ran.
	#[test]
	fn a_failed_write_leaves_the_resident_copy_describing_the_db() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		for entry in [
			dir_entry("guard"),
			file_entry("guard/x.txt", [1; 32], 1),
			file_entry("kept.txt", [2; 32], 2),
		] {
			store.upsert_entry(pair, &entry).unwrap();
		}
		let rows = |store: &BaselineStore| -> HashMap<String, BaselineEntry> {
			store
				.baseline(pair)
				.unwrap()
				.iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect()
		};
		let before = rows(&store);
		assert_eq!(before.len(), 3);

		store
			.conn
			.execute_batch(
				"CREATE TRIGGER no_insert BEFORE INSERT ON baseline WHEN NEW.rel_path LIKE 'boom%'
				 BEGIN SELECT RAISE(ABORT, 'insert refused'); END;
				 CREATE TRIGGER no_update BEFORE UPDATE ON baseline WHEN NEW.rel_path LIKE 'boom%'
				 BEGIN SELECT RAISE(ABORT, 'update refused'); END;
				 CREATE TRIGGER no_delete BEFORE DELETE ON baseline WHEN OLD.rel_path LIKE 'guard%'
				 BEGIN SELECT RAISE(ABORT, 'delete refused'); END;",
			)
			.unwrap();

		let agrees = |what: &str| {
			let from_db: HashMap<String, BaselineEntry> = store
				.entries(pair)
				.unwrap()
				.into_iter()
				.map(|entry| (entry.rel_path.clone(), entry))
				.collect();
			assert_eq!(rows(&store), from_db, "after a failed {what}");
			assert_eq!(rows(&store), before, "after a failed {what}");
		};

		let boom = file_entry("boom.txt", [3; 32], 3);
		store
			.upsert_entry(pair, &boom)
			.expect_err("the insert trigger must refuse this row");
		agrees("upsert_entry");

		store
			.delete_entry(pair, "guard/x.txt")
			.expect_err("the delete trigger must refuse this row");
		agrees("delete_entry");

		// The refused row is the SECOND of the batch: the first one ran.
		store
			.apply_changes(
				pair,
				&[
					BaselineChange::Upsert(&file_entry("kept.txt", [4; 32], 4)),
					BaselineChange::Upsert(&boom),
				],
			)
			.expect_err("the insert trigger must refuse the second change");
		agrees("apply_changes");

		store
			.apply_changes(
				pair,
				&[BaselineChange::MoveSubtree {
					from: "kept.txt",
					to: "boom/moved",
				}],
			)
			.expect_err("the update trigger must refuse the destination");
		agrees("apply_changes(MoveSubtree)");

		store
			.record_pending(
				pair,
				Uuid::new_v4(),
				&created("boom.txt"),
				NOW,
				&[BaselineChange::Upsert(&boom)],
			)
			.expect_err("the insert trigger must refuse the row this journals");
		agrees("record_pending");

		store
			.delete_subtrees(pair, &BTreeSet::from(["guard".to_string()]))
			.expect_err("the delete trigger must refuse the root");
		agrees("delete_subtrees");

		// The adoption re-seed: the first row runs, the second is refused, and the rollback takes
		// the mode change AND the first row back with it.
		store
			.set_mode(
				pair,
				SyncMode::LocalToRemote,
				&[dir_entry("adopted"), boom.clone()],
			)
			.expect_err("the insert trigger must refuse the second adopted row");
		assert_eq!(
			store.pair(pair).unwrap().unwrap().mode,
			SyncMode::TwoWay,
			"the mode switch rolled back with its rows"
		);
		agrees("set_mode");
	}

	/// A pass holds the copy it read while it writes the rows it advances: its own view must not
	/// change underneath it, and the store's must.
	#[test]
	fn a_write_leaves_the_copy_a_pass_already_took_alone() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.upsert_entry(pair, &file_entry("x.txt", [1; 32], 1))
			.unwrap();

		let read_by_the_pass = store.baseline(pair).unwrap();
		store
			.upsert_entry(pair, &file_entry("later.txt", [2; 32], 2))
			.unwrap();

		assert_eq!(read_by_the_pass.len(), 1, "the pass's own view moved");
		assert_eq!(
			store.baseline(pair).unwrap().len(),
			2,
			"the store's did not"
		);
	}

	/// A pair that is gone takes its resident rows with it: sqlite never hands its id out again,
	/// but a copy left behind would be a whole tree of rows nothing can reach.
	#[test]
	fn deleting_a_pair_drops_its_resident_rows() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/local", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		store
			.upsert_entry(pair, &file_entry("x.txt", [1; 32], 1))
			.unwrap();
		assert_eq!(store.baseline(pair).unwrap().len(), 1);

		store.delete_pair(pair).unwrap();
		assert!(store.resident.borrow().is_empty());
		assert!(store.baseline(pair).unwrap().is_empty());
	}

	/// A directory move re-keys the row at the source and every row under it, keeps everything else
	/// those rows hold, and touches nothing outside: not a sibling that merely shares the prefix,
	/// not another pair. A case-only rename is a move like any other — the key compares bytewise,
	/// so the new spelling is free — and each step re-keys from wherever the rows are now.
	#[test]
	fn a_subtree_move_re_keys_the_source_and_everything_under_it_only() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		for rel in ["a", "a/x", "b"] {
			store.upsert_entry(pair, &dir_entry(rel)).unwrap();
		}
		let deep = file_entry("a/x/y.txt", [7u8; 32], 11);
		store.upsert_entry(pair, &deep).unwrap();
		for rel in ["ab", "a0", "a."] {
			store
				.upsert_entry(pair, &file_entry(rel, [1u8; 32], 1))
				.unwrap();
		}
		store.upsert_entry(other, &dir_entry("a")).unwrap();
		let paths = |pair: PairId| -> Vec<String> {
			store
				.entries(pair)
				.unwrap()
				.into_iter()
				.map(|entry| entry.rel_path)
				.collect()
		};
		let move_subtree = |from, to| {
			store
				.apply_changes(pair, &[BaselineChange::MoveSubtree { from, to }])
				.unwrap();
		};

		move_subtree("a", "b/moved");
		assert_eq!(
			paths(pair),
			[
				"a.",
				"a0",
				"ab",
				"b",
				"b/moved",
				"b/moved/x",
				"b/moved/x/y.txt"
			]
		);
		assert_eq!(
			store.entry(pair, "b/moved/x/y.txt").unwrap(),
			Some(BaselineEntry {
				rel_path: "b/moved/x/y.txt".to_string(),
				..deep
			}),
			"a moved row must keep everything but its path"
		);
		assert_eq!(paths(other), ["a"], "another pair's rows do not move");

		move_subtree("b/moved", "b/MOVED");
		assert_eq!(
			paths(pair),
			[
				"a.",
				"a0",
				"ab",
				"b",
				"b/MOVED",
				"b/MOVED/x",
				"b/MOVED/x/y.txt"
			]
		);
		move_subtree("b/MOVED/x", "b/MOVED/x2");
		assert_eq!(
			paths(pair),
			[
				"a.",
				"a0",
				"ab",
				"b",
				"b/MOVED",
				"b/MOVED/x2",
				"b/MOVED/x2/y.txt"
			]
		);
	}

	/// A move onto a destination that already holds rows overwrites them, which is what the
	/// read-delete-upsert this replaced did. One caller can still ask for it: the case-only rename
	/// of a pushing mode emits its move without checking the local end, so rows left under both
	/// spellings by an earlier half-finished rename arrive here — after the directory has already
	/// been renamed on disk. Failing there would leave the store describing one directory under two
	/// names.
	#[test]
	fn a_subtree_move_overwrites_rows_sitting_at_the_destination() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		for rel in ["Docs", "docs"] {
			store.upsert_entry(pair, &dir_entry(rel)).unwrap();
		}
		store
			.upsert_entry(pair, &file_entry("Docs/a.txt", [1u8; 32], 1))
			.unwrap();
		store
			.upsert_entry(pair, &file_entry("docs/a.txt", [2u8; 32], 2))
			.unwrap();

		store
			.apply_changes(
				pair,
				&[BaselineChange::MoveSubtree {
					from: "Docs",
					to: "docs",
				}],
			)
			.unwrap();

		assert_eq!(
			store
				.entries(pair)
				.unwrap()
				.iter()
				.map(|e| e.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["docs", "docs/a.txt"],
			"one spelling is left, and it is the one the move named"
		);
		assert_eq!(
			store
				.entry(pair, "docs/a.txt")
				.unwrap()
				.and_then(|e| e.size),
			Some(1),
			"the row that moved is the one that survived"
		);
	}

	/// A row with a CHOSEN uuid and whole-life id, so a lookup can be aimed at it.
	fn identified(rel_path: &str, uuid: Uuid) -> BaselineEntry {
		BaselineEntry {
			remote_uuid: Some(uuid),
			remote_stable_uuid: Some(StableUuid::new_for_test(uuid)),
			..file_entry(rel_path, [7u8; 32], 3)
		}
	}

	/// The id lookup answers off the store's own indexes, for both columns, for more ids than one
	/// statement binds, and for this pair only.
	///
	/// Past [`SYNCED_CHUNK`] on purpose: the statement is a fixed-width `IN` list whose last chunk
	/// is padded by repeating one of its own members, so a padding bug either loses the tail or
	/// answers for a row nobody asked about. The pair filter matters just as much — an id is unique
	/// to the server, not to a pair's rows, and a stranger's row answering here would place an item
	/// at a path in a tree it has nothing to do with.
	#[test]
	fn synced_paths_answers_by_uuid_and_lineage_past_one_chunk() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let ids: Vec<Uuid> = (0..SYNCED_CHUNK + 5)
			.map(|n| Uuid::from_u128(n as u128 + 1))
			.collect();
		for (n, &uuid) in ids.iter().enumerate() {
			store
				.upsert_entry(pair, &identified(&format!("f{n}.txt"), uuid))
				.unwrap();
		}
		let stranger = Uuid::from_u128(9_000);
		store
			.upsert_entry(other, &identified("elsewhere.txt", stranger))
			.unwrap();

		let lineages: Vec<StableUuid> = ids.iter().copied().map(StableUuid::new_for_test).collect();
		let mut asked = ids.clone();
		asked.push(stranger);
		let found = store.synced_paths(pair, &asked, &lineages).unwrap();

		for (n, &uuid) in ids.iter().enumerate() {
			let want = format!("f{n}.txt");
			assert_eq!(found.path_by_uuid(uuid).as_deref(), Some(want.as_str()));
			assert_eq!(
				found
					.path_by_lineage(StableUuid::new_for_test(uuid))
					.as_deref(),
				Some(want.as_str())
			);
		}
		assert_eq!(
			found.path_by_uuid(stranger),
			None,
			"another pair's row must answer for nothing here"
		);
	}

	/// Two rows claiming one uuid: the seek finds BOTH and answers with the first by path.
	///
	/// This is a deliberate change from the resident map it replaces. That map was
	/// last-writer-wins over the load order, so the id answered for whichever row was rewritten
	/// last — and answered `None` as soon as that row lost the id, even though another row still
	/// carried it. `None` is the dangerous answer here: `unknown_remote_paths` reads it as an item
	/// that was never synced, which drops the protection that keeps a pass from deleting the local
	/// copy of a remote item that is still there. Deterministic, and never absent while some row
	/// carries the id, is what this pins.
	#[test]
	fn two_rows_claiming_one_uuid_answer_with_the_first_by_path() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let shared = Uuid::from_u128(77);
		store
			.upsert_entry(pair, &identified("b.txt", shared))
			.unwrap();
		store
			.upsert_entry(pair, &identified("a.txt", shared))
			.unwrap();

		let found = store.synced_paths(pair, &[shared], &[]).unwrap();
		assert_eq!(
			found.path_by_uuid(shared).as_deref(),
			Some("a.txt"),
			"of two claimants the first by path answers, rather than neither"
		);
	}

	/// An id nobody resolved is a BUG, not an absence.
	///
	/// The whole safety of resolving ids up front rests on the enumeration being exhaustive
	/// (`remote::delta_uuids`, `plan::skipped_ids`). An id that slipped through it would otherwise
	/// read as "no row records this", and on the remote side that is a move whose source is never
	/// vacated — the item left sitting at two paths in a view the pass then plans against.
	#[test]
	#[should_panic(expected = "without that id having been resolved")]
	fn asking_where_an_unresolved_id_sits_is_refused() {
		let _ = SyncedPaths::default().path_by_uuid(Uuid::from_u128(1));
	}

	#[test]
	fn delete_subtrees_takes_each_root_and_everything_under_it_only() {
		let store = BaselineStore::open_in_memory().unwrap();
		let (pair, _) = store
			.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		let (other, _) = store
			.create_pair("/other", Uuid::new_v4(), SyncMode::TwoWay)
			.unwrap();
		for rel in ["docs", "docs/sub", "ä", "other.txt", "a", "a/b"] {
			store.upsert_entry(pair, &dir_entry(rel)).unwrap();
		}
		for rel in [
			"docs/a.txt",
			"docs/sub/b.txt",
			"docsx",
			"docs.txt",
			"ä/b.txt",
			"äb.txt",
			// The neighbours of the subtree range: `.` (0x2E) sorts just below the `/` the range
			// starts at and `0` (0x30) just above it, so a range that is off by one byte at either
			// end takes one of these with it.
			"a/b/c",
			"ab",
			"a0",
			"a.",
		] {
			store
				.upsert_entry(pair, &file_entry(rel, [1u8; 32], 1))
				.unwrap();
		}
		store.upsert_entry(other, &dir_entry("docs")).unwrap();

		store
			.delete_subtrees(
				pair,
				&BTreeSet::from(["docs".to_string(), "ä".to_string(), "a".to_string()]),
			)
			.unwrap();
		assert_eq!(
			store
				.entries(pair)
				.unwrap()
				.iter()
				.map(|e| e.rel_path.as_str())
				.collect::<Vec<_>>(),
			vec!["a.", "a0", "ab", "docs.txt", "docsx", "other.txt", "äb.txt"]
		);
		assert!(
			store.entry(other, "docs").unwrap().is_some(),
			"another pair's rows stay"
		);
	}

	/// Whether the conflict-state index is in the file.
	fn has_state_index(store: &BaselineStore) -> bool {
		store
			.conn
			.query_row(
				"SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'baseline_state'",
				[],
				|row| row.get::<_, i64>(0),
			)
			.unwrap() == 1
	}

	/// The conflict read must SEEK the `(pair_id, state)` index rather than walk the pair's rows, on
	/// the very statement the read runs. The plan with the index dropped is asserted too, so the test
	/// cannot pass on a plan that never mentions an index at all.
	#[test]
	fn the_conflict_read_seeks_the_state_index() {
		let store = BaselineStore::open_in_memory().unwrap();
		let sql = conflict_rows_sql();
		let plan = |store: &BaselineStore| -> Vec<String> {
			store
				.conn
				.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
				.unwrap()
				.query_map(
					params![
						1_i64,
						BaselineState::Conflicted.as_i64(),
						BaselineState::Overwritten.as_i64()
					],
					|row| row.get("detail"),
				)
				.unwrap()
				.collect::<rusqlite::Result<_>>()
				.unwrap()
		};

		let indexed = plan(&store);
		assert!(
			indexed
				.iter()
				.any(|step| step.starts_with("SEARCH baseline USING INDEX baseline_state")),
			"the conflict read must seek the state index: {indexed:?}"
		);

		store.set_state_index(false).unwrap();
		let walked = plan(&store);
		assert!(
			!walked.iter().any(|step| step.contains("baseline_state")),
			"the control plan must not name an index that is gone: {walked:?}"
		);
		// Without it the read is a walk of every row the pair holds, whichever index carries it
		// there. The primary key is no longer the only candidate: `baseline_remote_uuid` and
		// `baseline_remote_stable_uuid` are `(pair_id, ...)` indexes too, so the planner may seek
		// one of THEM on `pair_id` alone and filter the state out of the rows it finds — the same
		// row count the primary-key walk had, and the cost `baseline_state` exists to remove. What
		// must not survive is a seek that narrows by state.
		assert!(
			walked.iter().any(|step| step.contains("(pair_id=?)")),
			"without it the read walks the pair's rows on pair_id alone: {walked:?}"
		);
	}

	/// A DB written before the index existed gains it on the next open and is NOT refused for it: the
	/// version stamp does not move, so the index is all that separates the two files.
	#[test]
	fn a_db_without_the_state_index_gains_it_on_open() {
		let path = temp_db_path("state_index");
		let entry = file_entry("a.txt", [5; 32], 9);
		let pair = {
			let store = BaselineStore::open(&path).unwrap();
			let (pair, _) = store
				.create_pair("/root", Uuid::new_v4(), SyncMode::TwoWay)
				.unwrap();
			store.upsert_entry(pair, &entry).unwrap();
			// The shape a build from before the index left behind.
			store.set_state_index(false).unwrap();
			assert!(!has_state_index(&store));
			pair
		};

		let reopened = BaselineStore::open(&path).unwrap();
		assert!(
			has_state_index(&reopened),
			"the open must create the index the file was missing"
		);
		assert_eq!(
			version_of(&reopened),
			SCHEMA_VERSION,
			"and must not restamp the file for it"
		);
		assert_eq!(
			reopened.entry(pair, "a.txt").unwrap().as_ref(),
			Some(&entry),
			"the rows it arrived with must read back"
		);
		drop(reopened);
		for suffix in ["", "-wal", "-shm"] {
			std::fs::remove_file(format!("{}{suffix}", path.display())).ok();
		}
	}

	/// Every statement that works on a whole subtree must SEEK the primary-key index, and the plan
	/// has to name the columns it seeks on.
	///
	/// `SEARCH baseline USING INDEX sqlite_autoindex_baseline_1 (pair_id=?)` — no `rel_path` term —
	/// is what an unsargable predicate produces: a walk of every index entry of the pair, with no
	/// `SCAN` step to give it away. Both the `substr` form these replaced and an `OR` of the two
	/// ranges plan exactly that, so asserting only "SEARCH, and not SCAN" passes for the very shape
	/// this guards against.
	#[test]
	fn every_subtree_statement_seeks_the_index_instead_of_scanning_the_pair() {
		let store = BaselineStore::open_in_memory().unwrap();
		let assert_seeks = |sql: &str, args: &[&dyn rusqlite::ToSql], seek: &str| {
			let plan: Vec<String> = store
				.conn
				.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
				.unwrap()
				.query_map(args, |row| row.get("detail"))
				.unwrap()
				.collect::<rusqlite::Result<_>>()
				.unwrap();
			assert!(
				// A DELETE reports the same seek as `SEARCH ... USING COVERING INDEX ...`.
				plan.iter()
					.any(|step| step.starts_with("SEARCH baseline USING")
						&& step.contains("sqlite_autoindex_baseline_1")
						&& step.contains(seek)),
				"{sql}\nmust seek on {seek}: {plan:?}"
			);
			assert!(
				!plan.iter().any(|step| step.contains("SCAN")),
				"{sql}\nstill scans: {plan:?}"
			);
		};
		let at_path = "(pair_id=? AND rel_path=?)";
		let under_path = "(pair_id=? AND rel_path>? AND rel_path<?)";

		assert_seeks(DELETE_AT_PATH, &[&1_i64, &"docs"], at_path);
		assert_seeks(DELETE_UNDER_PATH, &[&1_i64, &"docs"], under_path);
		assert_seeks(MOVE_AT_PATH, &[&1_i64, &"docs", &"moved"], at_path);
		assert_seeks(MOVE_UNDER_PATH, &[&1_i64, &"docs", &"moved"], under_path);
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
		reopened.delete_pending(pair, &[uuid]).unwrap();
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
			PendingKind::Trashed {
				path: "c.txt".to_string(),
			},
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
