//! A pass's plan, staged: every action with what its apply reads for it, and the order the apply
//! runs them in — written once the plan is made, then read back a page at a time while the apply
//! runs, so the apply holds a page of its plan rather than all of it.
//!
//! # Tables
//!
//! - `plan`: one row per action, keyed by `seq`, its position in the plan as the reconcile ordered
//!   it, with the local and the remote node its [`PassStart`] carries, and — once the apply has run
//!   it — the uuid of the directory a remote directory create made (`result_uuid`), which the
//!   actions under that directory land in ([`Pager::created_at`]).
//! - `plan_base`: the baseline rows its [`PassStart`] carries — at the action's path (`endpoint`
//!   [`AT_PATH`]) and at the path a move starts from ([`AT_SOURCE`]).
//! - `exec`: the order the apply runs the plan in (`ord`, from 0), and how far each action ties the
//!   drive lock (`tie_until`).
//! - `written`: what the apply has written so far, when the plan deletes anything (see below).
//! - `carried`: the deletions each directory deletion carries out along with its own (see
//!   `Screen::fold_subsumed`), by the deletion's `op` and the directory's path. They are in no run:
//!   the apply runs the directory's deletion, and drops their rows with its own
//!   ([`Pager::carried`]).
//!
//! # The order
//!
//! The apply runs the plan in three [`Run`]s. First, in plan order, everything but the transfers
//! and the deletions — the moves and creates the transfers land in — and the deletion half of each
//! type flip (a deletion whose path a create of the same pass fills again), which has to clear the
//! path before that create runs: one at a time, but a run of remote directory creates or of remote
//! trashes a chunk at a time. Then the transfers, concurrently. Then the other deletions, in plan
//! order, which is children first: a local one at a time, a run of remote trashes a chunk at a
//! time.
//!
//! The transfers are independent of one another, so their order is free, and it is the one that
//! lets the pass hold the drive lock least: first those that finish a compound step of the first
//! run — the create half of a type flip, anything at or under a moved directory — so that step ends
//! as early as it can; then the downloads, which need no lock; then the uploads, which do.
//!
//! # Where the drive lock may be let go
//!
//! Between any two actions, except where an action already run is still waiting on one at or after
//! it ([`Staged::releasable`]). Two shapes tie actions together. A type flip's deletion leaves its
//! path empty until the create at it runs, and another client let in between would find the path
//! free: it ties up to the last action creating, transferring or moving from or to that path. A
//! directory move carries its subtree, and what the plan names under the destination assumes the
//! move has just happened: it ties up to the last such action under it. Any other deletion names
//! its item by uuid and ties nothing. `tie_until` holds where each action's tie ends, and the pager
//! carries the furthest one reached so far as it hands the plan out.
//!
//! # What the pass wrote
//!
//! The server dedups a create against an existing item case-insensitively and hands back that
//! item, and a case-insensitive filesystem resolves two spellings to one entry, so a deletion
//! planned for one path can land on what another action of the pass just created, moved or renamed.
//! The planner does not plan that shape (see `plan::fold_dir_moves`); what the apply records here
//! is the net under it: a remote trash or local quarantine whose target is, or holds, something the
//! pass already wrote is refused ([`Pager::shields_remote`], [`Pager::shields_local`]), and the
//! next pass re-plans from what is really there.
//!
//! Only a deletion asks, so a plan that deletes nothing — a first sync, every upload of it —
//! records nothing. One that does records, as each write lands, the collision key of the path it
//! wrote on the side it wrote it (`written`), and the remote item a remote move moved; the
//! directory a create made, or the existing one the dedup answered it with, is its `result_uuid`,
//! which the plan records anyway.

use std::{
	collections::VecDeque,
	sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError},
};

use filen_types::fs::StableUuid;
use rusqlite::{Connection, OptionalExtension, Row, ffi, params, types::Type};
use uuid::Uuid;

use super::{
	super::{
		apply::{PassStart, PlannedEntry, StartInputs},
		baseline::{BaselineEntry, BaselineStore, DropRow, ENTRY_COLUMNS, NodeKind},
		plan::{RemoteNode, SyncAction},
		scan::{LocalNode, collision_key},
	},
	BatchWriter, Stage, bind_entry, hash_at, hash_bytes,
};

const SCHEMA: &str = "
CREATE TABLE plan (
	seq INTEGER PRIMARY KEY,
	op INTEGER NOT NULL,
	kind INTEGER,
	path TEXT NOT NULL,
	from_path TEXT,
	remote_uuid BLOB,
	run INTEGER NOT NULL,
	completes INTEGER NOT NULL DEFAULT 0,
	l_path TEXT,
	l_kind INTEGER,
	l_size INTEGER,
	l_mtime INTEGER,
	l_hash BLOB,
	r_path TEXT,
	r_kind INTEGER,
	r_uuid BLOB,
	r_stable BLOB,
	r_hash BLOB,
	r_size INTEGER,
	r_modified INTEGER,
	parent_uuid BLOB,
	result_uuid BLOB
);
-- The remote directory creates, by path: op 3 is OP_CREATE_REMOTE_DIR.
CREATE INDEX plan_created ON plan (path) WHERE op = 3;
-- The directories those creates made, by uuid, for a deletion to ask about.
CREATE INDEX plan_result ON plan (result_uuid) WHERE result_uuid IS NOT NULL;
CREATE TABLE plan_base (
	seq INTEGER NOT NULL,
	endpoint INTEGER NOT NULL,
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
	PRIMARY KEY (seq, endpoint)
) WITHOUT ROWID;
CREATE TABLE exec (ord INTEGER PRIMARY KEY, seq INTEGER NOT NULL, tie_until INTEGER);
CREATE TABLE written (side INTEGER NOT NULL, key TEXT NOT NULL, uuid BLOB);
CREATE INDEX written_key ON written (side, key);
CREATE INDEX written_uuid ON written (uuid) WHERE uuid IS NOT NULL;
CREATE TABLE carried (op INTEGER NOT NULL, dir TEXT NOT NULL, path TEXT NOT NULL);
";

/// `plan.op`: which [`SyncAction`] the row is.
pub(super) const OP_CREATE_LOCAL_DIR: i64 = 0;
pub(super) const OP_DOWNLOAD: i64 = 1;
pub(super) const OP_DELETE_LOCAL: i64 = 2;
pub(super) const OP_CREATE_REMOTE_DIR: i64 = 3;
pub(super) const OP_UPLOAD: i64 = 4;
pub(super) const OP_TRASH_REMOTE: i64 = 5;
pub(super) const OP_CONFLICT: i64 = 6;
pub(super) const OP_ADOPT: i64 = 7;
pub(super) const OP_MOVE_REMOTE: i64 = 8;
pub(super) const OP_MOVE_LOCAL: i64 = 9;

// `plan_created` in SCHEMA spells this op out: with any other value every created-directory
// lookup would scan the plan instead of seeking the index.
const _: () = assert!(OP_CREATE_REMOTE_DIR == 3);

/// The ops of [`SyncAction::is_delete`] and of [`SyncAction::is_create`], for the statements below.
pub(super) const DELETES: &str = "(2, 5)";
pub(super) const CREATES: &str = "(0, 1, 3, 4)";
pub(super) const MOVES: &str = "(8, 9)";

/// `plan_base.endpoint`: the row at the action's path, and the row at a move's source.
const AT_PATH: i64 = 0;
const AT_SOURCE: i64 = 1;

/// How many actions a page of the plan holds.
pub(in super::super) const PAGE_ROWS: usize = 512;

const INSERT_PLAN: &str = "INSERT INTO plan (seq, op, kind, path, from_path, remote_uuid, run,
	 l_path, l_kind, l_size, l_mtime, l_hash,
	 r_path, r_kind, r_uuid, r_stable, r_hash, r_size, r_modified, parent_uuid)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
	  ?20)";

/// The columns a `plan_base` row carries are the baseline's own, so [`BaselineStore::row_to_entry`]
/// reads them back.
static INSERT_BASE: LazyLock<String> = LazyLock::new(|| {
	format!(
		"INSERT INTO plan_base (seq, endpoint, {ENTRY_COLUMNS})
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)"
	)
});
static READ_BASES: LazyLock<String> =
	LazyLock::new(|| format!("SELECT {ENTRY_COLUMNS}, endpoint FROM plan_base WHERE seq = ?1"));

/// Where a remote directory create's result is recorded and read, both seeking on `plan_created`,
/// whose condition each repeats.
static RECORD_CREATED: LazyLock<String> = LazyLock::new(|| {
	format!("UPDATE plan SET result_uuid = ?2 WHERE op = {OP_CREATE_REMOTE_DIR} AND path = ?1")
});
static READ_CREATED: LazyLock<String> = LazyLock::new(|| {
	format!("SELECT result_uuid FROM plan WHERE op = {OP_CREATE_REMOTE_DIR} AND path = ?1")
});

/// `written.side`: the side a write landed on.
const WROTE_LOCAL: i64 = 0;
const WROTE_REMOTE: i64 = 1;

const INSERT_WRITTEN: &str = "INSERT INTO written (side, key, uuid) VALUES (?1, ?2, ?3)";

/// Whether the pass wrote the remote item `?2` (none for a local deletion), or anything on side
/// `?1` whose collision key is `?3` or lies under it. One `EXISTS` per index it seeks: an `OR` of a
/// point and a range would not seek.
const SHIELDS: &str = "SELECT EXISTS (SELECT 1 FROM plan WHERE result_uuid = ?2)
	 OR EXISTS (SELECT 1 FROM written WHERE uuid = ?2)
	 OR EXISTS (SELECT 1 FROM written WHERE side = ?1 AND key = ?3)
	 OR EXISTS (SELECT 1 FROM written WHERE side = ?1 AND key >= ?3 || '/' AND key < ?3 || '0')";

/// Whether the deletion `?1` of the directory at `?2` carries anything, and what: both seek
/// `carried_by`.
const CARRIES: &str = "SELECT EXISTS (SELECT 1 FROM carried WHERE op = ?1 AND dir = ?2)";
const READ_CARRIED: &str = "SELECT path FROM carried WHERE op = ?1 AND dir = ?2";

/// A page of the plan in apply order, with what [`entry_of`] reads by position.
const READ_PAGE: &str = "SELECT x.ord, x.tie_until, p.seq, p.op, p.kind, p.path, p.from_path,
	 p.remote_uuid, p.l_path, p.l_kind, p.l_size, p.l_mtime, p.l_hash,
	 p.r_path, p.r_kind, p.r_uuid, p.r_stable, p.r_hash, p.r_size, p.r_modified, p.parent_uuid
	 FROM exec AS x JOIN plan AS p ON p.seq = x.seq
	 WHERE x.ord >= ?1 AND x.ord < ?2 ORDER BY x.ord LIMIT ?3";

/// Which part of the apply an action runs in (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum Run {
	/// Before any transfer, in plan order: one at a time, but a run of remote directory creates or
	/// of remote trashes a chunk at a time.
	Before,
	/// The file transfers, concurrently.
	Transfers,
	/// After every transfer, in plan order: the deletions that are not half of a type flip, a local
	/// one at a time, a run of remote trashes a chunk at a time.
	After,
}

impl Run {
	const fn code(self) -> i64 {
		match self {
			Self::Before => 0,
			Self::Transfers => 1,
			Self::After => 2,
		}
	}

	/// The run `action` is in, but for a type flip's deletion, which the order moves to
	/// [`Before`](Self::Before) once it knows the plan creates at its path.
	fn of(action: &SyncAction) -> Self {
		match action {
			SyncAction::UploadFile { .. } | SyncAction::DownloadFile { .. } => Self::Transfers,
			_ if action.is_delete() => Self::After,
			_ => Self::Before,
		}
	}
}

/// Writes a pass's plan into its stage, one entry at a time in plan order, and orders it
/// ([`finish`](Self::finish)).
pub(in super::super) struct PlanWriter {
	batch: BatchWriter,
	/// How many entries each [`Run`] holds as written, before the order moves the deletion half of
	/// each type flip into [`Run::Before`].
	before: usize,
	transfers: usize,
	after: usize,
	/// How many entries create at their path, and how many move a directory.
	creates: usize,
	dir_moves: usize,
	/// How many deletions the directory deletions carry.
	carried: usize,
}

impl PlanWriter {
	pub(in super::super) fn new(stage: Stage) -> rusqlite::Result<Self> {
		stage.conn.execute_batch(SCHEMA)?;
		Ok(Self {
			batch: BatchWriter::new(stage),
			before: 0,
			transfers: 0,
			after: 0,
			creates: 0,
			dir_moves: 0,
			carried: 0,
		})
	}

	/// The stage's connection, for a read of what else the stage holds between two entries.
	pub(super) fn conn(&self) -> &Connection {
		self.batch.conn()
	}

	/// Stage what the screened plan in the same stage folded into its directory deletions (see
	/// `Screen::fold_subsumed`): each carried deletion by its side's `op`, which is the directory
	/// deletion's own, and the directory's path.
	pub(super) fn carry_screened(&mut self) -> rusqlite::Result<()> {
		self.carried += self.batch.conn().execute(
			"INSERT INTO carried (op, dir, path)
			 SELECT op, carried_by, path FROM screen WHERE carried_by IS NOT NULL",
			[],
		)?;
		Ok(())
	}

	/// Stage `entry`, next in plan order.
	pub(in super::super) fn push(&mut self, entry: &PlannedEntry) -> rusqlite::Result<()> {
		let seq = self.before + self.transfers + self.after;
		let run = Run::of(&entry.action);
		self.batch.row(|conn| write_entry(conn, seq, run, entry))?;
		*match run {
			Run::Before => &mut self.before,
			Run::Transfers => &mut self.transfers,
			Run::After => &mut self.after,
		} += 1;
		self.creates += usize::from(entry.action.is_create());
		self.dir_moves += usize::from(moved_dir(&entry.action).is_some());
		Ok(())
	}

	/// The plan staged, in the order the apply runs it.
	pub(in super::super) fn finish(self) -> rusqlite::Result<StagedPlan> {
		let Self {
			batch,
			before,
			transfers,
			after,
			creates,
			dir_moves,
			carried,
		} = self;
		let stage = batch.finish()?;
		let conn = &stage.conn;
		if carried > 0 {
			conn.execute_batch("CREATE INDEX carried_by ON carried (op, dir)")?;
		}
		let deletes_flip = after > 0 && creates > 0;
		// The paths the statements below seek on — only when there is something to seek: a first
		// sync, all creates, and a mass deletion, all deletions, neither flips a type nor moves a
		// directory.
		if deletes_flip || dir_moves > 0 {
			conn.execute_batch(
				"CREATE INDEX plan_path ON plan (path);
				 CREATE INDEX plan_from ON plan (from_path) WHERE from_path IS NOT NULL;",
			)?;
		}
		let flips = if deletes_flip {
			conn.execute(
				&format!(
					"UPDATE plan SET run = {before_run} WHERE op IN {DELETES} AND EXISTS
					 (SELECT 1 FROM plan AS created WHERE created.path = plan.path
					  AND created.op IN {CREATES})",
					before_run = Run::Before.code(),
				),
				[],
			)?
		} else {
			0
		};
		if flips > 0 {
			conn.execute(
				&format!(
					"UPDATE plan SET completes = 1 WHERE run = {transfers_run} AND EXISTS
					 (SELECT 1 FROM plan AS flip WHERE flip.path = plan.path
					  AND flip.run = {before_run} AND flip.op IN {DELETES})",
					transfers_run = Run::Transfers.code(),
					before_run = Run::Before.code(),
				),
				[],
			)?;
		}
		let moved = if dir_moves > 0 {
			moved_dirs(conn)?
		} else {
			Vec::new()
		};
		// Two statements per directory rather than one: an `OR` of a point and a range does not seek.
		for dir in &moved {
			for under in [
				"UPDATE plan SET completes = 1 WHERE run = ?2 AND path = ?1",
				"UPDATE plan SET completes = 1 WHERE run = ?2
				 AND path >= ?1 || '/' AND path < ?1 || '0'",
			] {
				conn.prepare_cached(under)?
					.execute(params![dir, Run::Transfers.code()])?;
			}
		}
		// `completes` is set on transfers only, so it orders nothing in the other runs; nor does
		// `op`, since only a transfer is an upload.
		conn.execute(
			&format!(
				"INSERT INTO exec (ord, seq) SELECT ROW_NUMBER() OVER
				 (ORDER BY run, completes DESC, op = {OP_UPLOAD}, seq) - 1, seq FROM plan"
			),
			[],
		)?;
		if flips > 0 || !moved.is_empty() {
			tie(conn)?;
		}
		Ok(StagedPlan {
			stage: SharedStage(Arc::new(Mutex::new(stage))),
			before: before + flips,
			transfers,
			len: before + transfers + after,
			deletes: after > 0,
			carries: carried > 0,
		})
	}
}

/// Where a directory move is headed, for a directory move.
fn moved_dir(action: &SyncAction) -> Option<&str> {
	match action {
		SyncAction::MoveRemote {
			to_path,
			kind: NodeKind::Dir,
			..
		}
		| SyncAction::MoveLocal {
			to_path,
			kind: NodeKind::Dir,
			..
		} => Some(to_path),
		_ => None,
	}
}

/// Where every directory move of the plan is headed. As many as the plan moves directories, which
/// the fold makes one per moved subtree.
fn moved_dirs(conn: &Connection) -> rusqlite::Result<Vec<String>> {
	conn.prepare(&format!(
		"SELECT path FROM plan WHERE op IN {MOVES} AND kind = {dir}",
		dir = NodeKind::Dir.as_i64()
	))?
	.query_map([], |row| row.get(0))?
	.collect()
}

/// Set `tie_until` on every action that ties the drive lock (see the module docs): a type flip's
/// deletion, to the last position that creates, transfers or moves from or to its path; a directory
/// move, to the last position that does under it.
fn tie(conn: &Connection) -> rusqlite::Result<()> {
	conn.execute_batch("CREATE INDEX exec_seq ON exec (seq)")?;
	let tying: Vec<(i64, String, bool)> = conn
		.prepare(&format!(
			"SELECT seq, path, op IN {MOVES} FROM plan
			 WHERE (run = {before_run} AND op IN {DELETES}) OR (op IN {MOVES} AND kind = {dir})",
			before_run = Run::Before.code(),
			dir = NodeKind::Dir.as_i64(),
		))?
		.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
		.collect::<rusqlite::Result<_>>()?;
	// The last position of an action that is no deletion and starts or ends where `at` says of each
	// endpoint, one endpoint per statement: an `OR` of the two columns would not seek.
	let last = |at: fn(&str) -> String| {
		let positions = |endpoint| {
			format!(
				"SELECT x.ord FROM plan AS p JOIN exec AS x ON x.seq = p.seq
				 WHERE p.op NOT IN {DELETES} AND {}",
				at(endpoint)
			)
		};
		format!(
			"SELECT MAX(ord) FROM ({} UNION ALL {})",
			positions("p.path"),
			positions("p.from_path")
		)
	};
	let at_path = last(|endpoint| format!("{endpoint} = ?1"));
	let under_path = last(|endpoint| format!("{endpoint} >= ?1 || '/' AND {endpoint} < ?1 || '0'"));
	for (seq, path, is_move) in tying {
		let until: Option<i64> = conn
			.prepare_cached(if is_move { &under_path } else { &at_path })?
			.query_row([&path], |row| row.get(0))?;
		conn.prepare_cached("UPDATE exec SET tie_until = ?2 WHERE seq = ?1")?
			.execute(params![seq, until])?;
	}
	Ok(())
}

fn write_entry(
	conn: &Connection,
	seq: usize,
	run: Run,
	PlannedEntry { action, start }: &PlannedEntry,
) -> rusqlite::Result<()> {
	let path = action.rel_path();
	let (op, kind, from_path, remote_uuid) = action_columns(action);
	let Columns {
		local,
		remote,
		base,
		base_from,
		parent_uuid,
	} = Columns::of(start);
	// A node's own path is written only where it is not the action's: the two are one path for
	// every action the plan makes today.
	conn.prepare_cached(INSERT_PLAN)?.execute(params![
		seq,
		op,
		kind.map(NodeKind::as_i64),
		path,
		from_path,
		remote_uuid,
		run.code(),
		local
			.map(|node| node.rel_path.as_str())
			.filter(|&node_path| node_path != path),
		local.map(|node| node.kind.as_i64()),
		local.map(|node| node.size),
		local.map(|node| node.mtime_millis),
		local.and_then(|node| hash_bytes(node.content_hash.as_ref())),
		remote
			.map(|node| node.rel_path.as_str())
			.filter(|&node_path| node_path != path),
		remote.map(|node| node.kind.as_i64()),
		remote.map(|node| node.remote_uuid),
		remote.and_then(|node| node.stable_uuid),
		remote.and_then(|node| hash_bytes(node.content_hash.as_ref())),
		remote.map(|node| node.size),
		remote.map(|node| node.modified_millis),
		parent_uuid,
	])?;
	for (endpoint, row) in [(AT_PATH, base), (AT_SOURCE, base_from)] {
		if let Some(row) = row {
			write_base(conn, seq, endpoint, row)?;
		}
	}
	Ok(())
}

/// What a [`PassStart`] carries, in the columns a `plan` row and its `plan_base` rows hold every
/// kind's in: whatever its kind does not read is `None`.
#[derive(Default)]
struct Columns<'a> {
	local: Option<&'a LocalNode>,
	remote: Option<&'a RemoteNode>,
	base: Option<&'a BaselineEntry>,
	base_from: Option<&'a BaselineEntry>,
	parent_uuid: Option<Uuid>,
}

impl<'a> Columns<'a> {
	fn of(start: &'a PassStart) -> Self {
		match start {
			PassStart::Remote { remote } => Self {
				remote: remote.as_deref(),
				..Self::default()
			},
			PassStart::Download { remote, base } => Self {
				remote: remote.as_deref(),
				base: base.as_deref(),
				..Self::default()
			},
			PassStart::Upload {
				local,
				remote,
				base,
				parent_uuid,
			} => Self {
				local: local.as_deref(),
				remote: remote.as_deref(),
				base: base.as_deref(),
				parent_uuid: *parent_uuid,
				..Self::default()
			},
			PassStart::CreateRemoteDir {
				local,
				remote,
				parent_uuid,
			} => Self {
				local: local.as_deref(),
				remote: remote.as_deref(),
				parent_uuid: *parent_uuid,
				..Self::default()
			},
			PassStart::MoveRemoteFile {
				local,
				base_from,
				parent_uuid,
			} => Self {
				local: local.as_deref(),
				base_from: base_from.as_deref(),
				parent_uuid: *parent_uuid,
				..Self::default()
			},
			PassStart::MoveRemoteDir {
				remote,
				parent_uuid,
			} => Self {
				remote: remote.as_deref(),
				parent_uuid: *parent_uuid,
				..Self::default()
			},
			PassStart::MoveLocalFile {
				remote,
				base,
				base_from,
			} => Self {
				remote: remote.as_deref(),
				base: base.as_deref(),
				base_from: base_from.as_deref(),
				..Self::default()
			},
			PassStart::Sides { local, remote } => Self {
				local: local.as_deref(),
				remote: remote.as_deref(),
				..Self::default()
			},
			PassStart::Nothing => Self::default(),
		}
	}
}

/// The inputs a staged entry's rows hold, read back: [`PassStart::of`] takes those its action's kind
/// reads, which are the ones [`Columns`] wrote, each the pass's answer at the path it asks for.
pub(super) struct StagedInputs {
	pub(super) local: Option<Box<LocalNode>>,
	pub(super) remote: Option<Box<RemoteNode>>,
	pub(super) base: Option<Box<BaselineEntry>>,
	pub(super) base_from: Option<Box<BaselineEntry>>,
	pub(super) parent_uuid: Option<Uuid>,
}

impl StartInputs for StagedInputs {
	fn local(&mut self, _: &str) -> Option<Box<LocalNode>> {
		self.local.take()
	}

	fn remote(&mut self, _: &str) -> Option<Box<RemoteNode>> {
		self.remote.take()
	}

	fn base(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		self.base.take()
	}

	fn base_from(&mut self, _: &str) -> Option<Box<BaselineEntry>> {
		self.base_from.take()
	}

	fn parent_uuid(&mut self, _: &str) -> Option<Uuid> {
		self.parent_uuid.take()
	}
}

fn write_base(
	conn: &Connection,
	seq: usize,
	endpoint: i64,
	row: &BaselineEntry,
) -> rusqlite::Result<()> {
	let mut statement = conn.prepare_cached(&INSERT_BASE)?;
	statement.raw_bind_parameter(1, seq)?;
	statement.raw_bind_parameter(2, endpoint)?;
	bind_entry(&mut statement, 3, row)?;
	statement.raw_execute()?;
	Ok(())
}

/// `(op, kind, from_path, remote_uuid)` of `action`, whose path is [`SyncAction::rel_path`].
pub(super) fn action_columns(
	action: &SyncAction,
) -> (i64, Option<NodeKind>, Option<&str>, Option<Uuid>) {
	match action {
		SyncAction::CreateLocalDir { .. } => (OP_CREATE_LOCAL_DIR, None, None, None),
		SyncAction::DownloadFile { remote_uuid, .. } => {
			(OP_DOWNLOAD, None, None, Some(*remote_uuid))
		}
		SyncAction::DeleteLocal { kind, .. } => (OP_DELETE_LOCAL, Some(*kind), None, None),
		SyncAction::CreateRemoteDir { .. } => (OP_CREATE_REMOTE_DIR, None, None, None),
		SyncAction::UploadFile { .. } => (OP_UPLOAD, None, None, None),
		SyncAction::TrashRemote {
			kind, remote_uuid, ..
		} => (OP_TRASH_REMOTE, Some(*kind), None, Some(*remote_uuid)),
		SyncAction::Conflict { .. } => (OP_CONFLICT, None, None, None),
		SyncAction::AdoptBaseline { .. } => (OP_ADOPT, None, None, None),
		SyncAction::MoveRemote {
			from_path,
			kind,
			remote_uuid,
			..
		} => (
			OP_MOVE_REMOTE,
			Some(*kind),
			Some(from_path),
			Some(*remote_uuid),
		),
		SyncAction::MoveLocal {
			from_path, kind, ..
		} => (OP_MOVE_LOCAL, Some(*kind), Some(from_path), None),
	}
}

/// A pass's plan, staged and ordered (see the module docs).
pub(in super::super) struct StagedPlan {
	stage: SharedStage,
	/// How many actions run in [`Run::Before`], and in [`Run::Transfers`] after them.
	before: usize,
	transfers: usize,
	len: usize,
	/// Whether the plan deletes anything: only then does the apply record what it writes, which only
	/// a deletion asks about (see the module docs).
	deletes: bool,
	/// Whether any directory deletion of the plan carries other deletions.
	carries: bool,
}

/// A staged plan's stage, shared with the blocking thread a directory deletion commits on, which
/// reads the deletions it carries out of the stage inside that commit (see [`Carried`]): an `Arc`
/// for that thread's `'static` bound, a `Mutex` because a connection is not `Sync`. Nothing waits on
/// it: the apply reads its stage between actions, and awaits that commit before it reads again.
#[derive(Clone)]
struct SharedStage(Arc<Mutex<Stage>>);

impl SharedStage {
	/// The stage. One a panic left locked — a read of what a deletion asks that failed, which panics
	/// (see the module docs of [`stage`](super)) — is taken all the same: every write to it is a
	/// single statement, so a panic leaves none half made.
	fn lock(&self) -> MutexGuard<'_, Stage> {
		self.0.lock().unwrap_or_else(PoisonError::into_inner)
	}
}

/// The deletions a directory deletion carries out along with its own, staged with the plan (see
/// [`Pager::carried`]), read out of the stage a row at a time by [`each`](Self::each). Owned, with a
/// handle on the stage, for the blocking thread the deletion's commit runs on.
pub(in super::super) struct Carried {
	stage: SharedStage,
	op: i64,
	dir: String,
}

impl Carried {
	/// Hand `visit` the path of every deletion carried, stopping at the first error.
	///
	/// A read that fails here is an error, not a panic: `visit` drops a baseline row per path inside
	/// the deletion's commit, and an error rolls that commit back, which fails the deletion as a
	/// record that could not be written. A failed step is never read as the end of the rows.
	pub(in super::super) fn each(self, visit: &mut DropRow<'_>) -> rusqlite::Result<()> {
		let stage = self.stage.lock();
		let mut read = stage.conn.prepare_cached(READ_CARRIED)?;
		let mut rows = read.query(params![self.op, self.dir])?;
		while let Some(row) = rows.next()? {
			visit(row.get_ref(0)?.as_str()?)?;
		}
		Ok(())
	}
}

impl StagedPlan {
	/// The plan, read back a page at a time in apply order.
	pub(in super::super) fn pager(self) -> Pager {
		Pager {
			plan: self,
			page: VecDeque::new(),
			next: 0,
			reach: None,
		}
	}
}

/// One action of a staged plan, as the apply takes it.
#[derive(Debug)]
pub(in super::super) struct Staged {
	pub(in super::super) run: Run,
	/// Whether the drive lock may be let go just before this action: no action handed out before
	/// it ties the lock to it or past it (see the module docs).
	pub(in super::super) releasable: bool,
	pub(in super::super) entry: PlannedEntry,
}

/// A staged plan, handed out in apply order a page at a time: what the apply holds of its plan is
/// the page it is in.
pub(in super::super) struct Pager {
	plan: StagedPlan,
	/// What is left of the page read last.
	page: VecDeque<Staged>,
	/// Where the next page starts.
	next: usize,
	/// How far the actions handed out so far tie the drive lock: the furthest `tie_until` among them.
	reach: Option<usize>,
}

impl Pager {
	/// How many actions the plan holds.
	pub(in super::super) fn len(&self) -> usize {
		self.plan.len
	}

	/// Read the next page once the one in hand is used up, and hand back the actions it holds. Every
	/// action [`peek_in`](Self::peek_in) and [`next_in`](Self::next_in) hand out comes from a page
	/// read here, so a caller that turns before it takes anything can ready what a page acts on
	/// before any of it runs. `None` while the page in hand has actions left, and at the end of the
	/// plan.
	///
	/// A read that fails is an error, not a panic (see the module docs of [`stage`](super)): the
	/// apply turns pages with transfers in flight, and a panic would drop them between their network
	/// op and the row that records it. The page in hand stays used up, so the next call reads the
	/// same page again.
	pub(in super::super) fn turn(
		&mut self,
	) -> rusqlite::Result<Option<impl Iterator<Item = &PlannedEntry>>> {
		if !self.page.is_empty() || self.next >= self.plan.len {
			return Ok(None);
		}
		self.read_page()?;
		Ok(Some(self.page.iter().map(|staged| &staged.entry)))
	}

	/// The next action, when it is in `run`, turning to the next page if need be (see
	/// [`turn`](Self::turn)).
	pub(in super::super) fn peek_in(&mut self, run: Run) -> rusqlite::Result<Option<&Staged>> {
		self.turn()?;
		Ok(self.page.front().filter(|staged| staged.run == run))
	}

	/// Take the next action, when it is in `run`.
	pub(in super::super) fn next_in(&mut self, run: Run) -> rusqlite::Result<Option<Staged>> {
		if self.peek_in(run)?.is_none() {
			return Ok(None);
		}
		Ok(self.page.pop_front())
	}

	/// Every action of `run` — of the whole plan for `None` — in apply order, a page at a time,
	/// without moving the pager, stopping at a page that fails to read.
	pub(in super::super) fn sweep(
		&self,
		run: Option<Run>,
		mut each: impl FnMut(PlannedEntry),
	) -> rusqlite::Result<()> {
		let (mut at, end) = match run {
			None => (0, self.plan.len),
			Some(Run::Before) => (0, self.plan.before),
			Some(Run::Transfers) => (self.plan.before, self.plan.before + self.plan.transfers),
			Some(Run::After) => (self.plan.before + self.plan.transfers, self.plan.len),
		};
		while at < end {
			// Read under the lock and visited without it, so `each` may ask the stage itself.
			let page = read_page(&self.plan.stage.lock(), at, end)?;
			at += page.len();
			for (_, _, entry) in page {
				each(entry);
			}
		}
		Ok(())
	}

	/// Record that the remote directory create at `path` made — or merged into — the directory
	/// `uuid`.
	pub(in super::super) fn record_created(
		&mut self,
		path: &str,
		uuid: Uuid,
	) -> rusqlite::Result<()> {
		self.plan
			.stage
			.lock()
			.conn
			.prepare_cached(&RECORD_CREATED)?
			.execute(params![path, uuid])?;
		Ok(())
	}

	/// The remote directory this pass's create at `path` made (see [`record_created`]), if the plan
	/// creates one there and the create has run and landed.
	///
	/// A read that fails is an error, not a panic (see the module docs of [`stage`](super)): the
	/// apply asks where an upload lands with other transfers in flight, and a panic would drop them
	/// between their network op and the row that records it.
	///
	/// [`record_created`]: Self::record_created
	pub(in super::super) fn created_at(&self, path: &str) -> rusqlite::Result<Option<Uuid>> {
		Ok(self
			.plan
			.stage
			.lock()
			.conn
			.prepare_cached(&READ_CREATED)?
			.query_row([path], |row| row.get::<_, Option<Uuid>>(0))
			.optional()?
			.flatten())
	}

	/// Record that `action` has landed, for the deletions after it to ask about (see the module
	/// docs). A plan that deletes nothing has nobody to ask, and records nothing.
	///
	/// A remote directory create is recorded where its `result_uuid` is ([`record_created`]): that
	/// is the item the deletions must not take down, and the key here is where it sits.
	///
	/// [`record_created`]: Self::record_created
	pub(in super::super) fn note_written(&self, action: &SyncAction) -> rusqlite::Result<()> {
		if !self.plan.deletes {
			return Ok(());
		}
		let (side, path, uuid) = match action {
			SyncAction::CreateLocalDir { rel_path } | SyncAction::DownloadFile { rel_path, .. } => {
				(WROTE_LOCAL, rel_path, None)
			}
			SyncAction::MoveLocal { to_path, .. } => (WROTE_LOCAL, to_path, None),
			SyncAction::CreateRemoteDir { rel_path } | SyncAction::UploadFile { rel_path } => {
				(WROTE_REMOTE, rel_path, None)
			}
			SyncAction::MoveRemote {
				to_path,
				remote_uuid,
				..
			} => (WROTE_REMOTE, to_path, Some(*remote_uuid)),
			SyncAction::DeleteLocal { .. }
			| SyncAction::TrashRemote { .. }
			| SyncAction::Conflict { .. }
			| SyncAction::AdoptBaseline { .. } => return Ok(()),
		};
		self.plan
			.stage
			.lock()
			.conn
			.prepare_cached(INSERT_WRITTEN)?
			.execute(params![side, collision_key(path), uuid])?;
		Ok(())
	}

	/// Whether quarantining the local item at `rel_path` would take down something this pass wrote:
	/// a local write at that path or under it, under any spelling.
	///
	/// A read that fails panics: only a deletion asks, in a run the apply takes one action at a time,
	/// so nothing is in flight to drop (see the module docs of [`stage`](super)).
	pub(in super::super) fn shields_local(&self, rel_path: &str) -> bool {
		self.shields(WROTE_LOCAL, rel_path, None)
	}

	/// Whether trashing the remote item `remote_uuid` at `rel_path` would take down something this
	/// pass wrote: the item itself, which the pass created — a create the dedup answered with an
	/// existing directory names that directory — or moved, or a remote write at that path or under
	/// it, under any spelling.
	///
	/// A read that fails panics: only a trash asks, as the apply composes its chunk, before anything
	/// is sent, so nothing is in flight to drop (see the module docs of [`stage`](super)).
	pub(in super::super) fn shields_remote(&self, rel_path: &str, remote_uuid: Uuid) -> bool {
		self.shields(WROTE_REMOTE, rel_path, Some(remote_uuid))
	}

	/// The deletions `action` carries out along with its own, when it is a directory deletion that
	/// carries any (see the module docs).
	///
	/// A read that fails panics: only a deletion asks, before it runs — a local one alone, a trash
	/// as the apply composes its chunk — so nothing is in flight to drop (see the module docs of
	/// [`stage`](super)).
	pub(in super::super) fn carried(&self, action: &SyncAction) -> Option<Carried> {
		if !self.plan.carries || !action.is_delete() {
			return None;
		}
		let (op, dir) = (action_columns(action).0, action.rel_path());
		let stage = self.plan.stage.lock();
		let read = || -> rusqlite::Result<bool> {
			stage
				.conn
				.prepare_cached(CARRIES)?
				.query_row(params![op, dir], |row| row.get(0))
		};
		let carries = read().unwrap_or_else(|error| {
			panic!(
				"reading the pass's staged plan in {} for the deletions {dir:?} carries failed: \
				 {error} — the plan has no other copy to apply",
				stage.path().display()
			)
		});
		carries.then(|| Carried {
			stage: self.plan.stage.clone(),
			op,
			dir: dir.to_owned(),
		})
	}

	fn shields(&self, side: i64, rel_path: &str, uuid: Option<Uuid>) -> bool {
		let stage = self.plan.stage.lock();
		let read = || -> rusqlite::Result<bool> {
			stage
				.conn
				.prepare_cached(SHIELDS)?
				.query_row(params![side, uuid, collision_key(rel_path)], |row| {
					row.get(0)
				})
		};
		read().unwrap_or_else(|error| {
			panic!(
				"reading the pass's staged plan in {} for what it wrote at {rel_path:?} failed: \
				 {error} — a deletion cannot tell whether it takes down a write of its own pass",
				stage.path().display()
			)
		})
	}

	fn read_page(&mut self) -> rusqlite::Result<()> {
		let page = read_page(&self.plan.stage.lock(), self.next, self.plan.len)?;
		self.next += page.len();
		for (ord, tie_until, entry) in page {
			let releasable = self.reach.is_none_or(|reach| reach < ord);
			self.reach = self.reach.max(tie_until);
			let run = if ord < self.plan.before {
				Run::Before
			} else if ord < self.plan.before + self.plan.transfers {
				Run::Transfers
			} else {
				Run::After
			};
			self.page.push_back(Staged {
				run,
				releasable,
				entry,
			});
		}
		Ok(())
	}
}

/// The plan from position `from` up to (not including) `end`, at most a page of it: each action's
/// position, where its tie on the drive lock ends, and its entry.
///
/// Positions run from 0 without a gap, so a short page is a plan that lost actions, and an error
/// like any other read that fails: never the end of the plan.
fn read_page(
	stage: &Stage,
	from: usize,
	end: usize,
) -> rusqlite::Result<Vec<(usize, Option<usize>, PlannedEntry)>> {
	let mut bases = stage.conn.prepare_cached(&READ_BASES)?;
	let mut page = stage.conn.prepare_cached(READ_PAGE)?;
	let mut rows = page.query(params![from, end, PAGE_ROWS])?;
	let expected = PAGE_ROWS.min(end - from);
	let mut read = Vec::with_capacity(expected);
	while let Some(row) = rows.next()? {
		let seq: i64 = row.get(2)?;
		let (action, mut inputs) = entry_of(row)?;
		let mut base_rows = bases.query([seq])?;
		while let Some(base) = base_rows.next()? {
			let at = Some(Box::new(BaselineStore::row_to_entry(base)?));
			match base.get::<_, i64>(14)? {
				AT_PATH => inputs.base = at,
				AT_SOURCE => inputs.base_from = at,
				other => return Err(corrupt("plan_base endpoint", other)),
			}
		}
		let start = PassStart::of(&action, &mut inputs);
		read.push((row.get(0)?, row.get(1)?, PlannedEntry { action, start }));
	}
	if read.len() != expected {
		return Err(rusqlite::Error::SqliteFailure(
			ffi::Error::new(ffi::SQLITE_CORRUPT),
			Some(format!(
				"the pass's staged plan in {} read {} action(s) at {from} where it holds {expected}",
				stage.path().display(),
				read.len()
			)),
		));
	}
	Ok(read)
}

/// The action a [`READ_PAGE`] row stages and the inputs it holds, but for its baseline rows, which
/// `plan_base` holds.
fn entry_of(row: &Row<'_>) -> rusqlite::Result<(SyncAction, StagedInputs)> {
	let path: String = row.get(5)?;
	// Each node owns its path, as it did in the pass; the action owns its own copy.
	let node_path = |column| -> rusqlite::Result<String> {
		Ok(row
			.get::<_, Option<String>>(column)?
			.unwrap_or_else(|| path.clone()))
	};
	let local = match row.get::<_, Option<i64>>(9)? {
		None => None,
		Some(kind) => Some(Box::new(LocalNode {
			rel_path: node_path(8)?,
			kind: kind_of(kind)?,
			size: row.get(10)?,
			mtime_millis: row.get(11)?,
			// Not staged: only the seeding reads it, before the plan.
			mtime_nanos: 0,
			content_hash: hash_at(row, 12)?,
		})),
	};
	let remote = match row.get::<_, Option<i64>>(14)? {
		None => None,
		Some(kind) => Some(Box::new(RemoteNode {
			rel_path: node_path(13)?,
			kind: kind_of(kind)?,
			remote_uuid: row.get(15)?,
			stable_uuid: row.get::<_, Option<StableUuid>>(16)?,
			content_hash: hash_at(row, 17)?,
			size: row.get(18)?,
			modified_millis: row.get(19)?,
		})),
	};
	let inputs = StagedInputs {
		local,
		remote,
		base: None,
		base_from: None,
		parent_uuid: row.get(20)?,
	};
	Ok((action_of(row, 3, path)?, inputs))
}

/// The action a row stages at `path`, its columns from `first` on in [`READ_PAGE`]'s order: `op`,
/// `kind`, `path` (not read here), `from_path` and `remote_uuid`.
pub(super) fn action_of(row: &Row<'_>, first: usize, path: String) -> rusqlite::Result<SyncAction> {
	let op: i64 = row.get(first)?;
	let kind = || -> rusqlite::Result<NodeKind> {
		let raw: Option<i64> = row.get(first + 1)?;
		kind_of(raw.ok_or_else(|| corrupt("missing kind for op", op))?)
	};
	let from_path = || -> rusqlite::Result<String> {
		row.get::<_, Option<String>>(first + 3)?
			.ok_or_else(|| corrupt("missing from_path for op", op))
	};
	let remote_uuid = || -> rusqlite::Result<Uuid> {
		row.get::<_, Option<Uuid>>(first + 4)?
			.ok_or_else(|| corrupt("missing remote_uuid for op", op))
	};
	Ok(match op {
		OP_CREATE_LOCAL_DIR => SyncAction::CreateLocalDir { rel_path: path },
		OP_DOWNLOAD => SyncAction::DownloadFile {
			rel_path: path,
			remote_uuid: remote_uuid()?,
		},
		OP_DELETE_LOCAL => SyncAction::DeleteLocal {
			rel_path: path,
			kind: kind()?,
		},
		OP_CREATE_REMOTE_DIR => SyncAction::CreateRemoteDir { rel_path: path },
		OP_UPLOAD => SyncAction::UploadFile { rel_path: path },
		OP_TRASH_REMOTE => SyncAction::TrashRemote {
			rel_path: path,
			kind: kind()?,
			remote_uuid: remote_uuid()?,
		},
		OP_CONFLICT => SyncAction::Conflict { rel_path: path },
		OP_ADOPT => SyncAction::AdoptBaseline { rel_path: path },
		OP_MOVE_REMOTE => SyncAction::MoveRemote {
			from_path: from_path()?,
			to_path: path,
			kind: kind()?,
			remote_uuid: remote_uuid()?,
		},
		OP_MOVE_LOCAL => SyncAction::MoveLocal {
			from_path: from_path()?,
			to_path: path,
			kind: kind()?,
		},
		other => return Err(corrupt("op", other)),
	})
}

fn kind_of(raw: i64) -> rusqlite::Result<NodeKind> {
	NodeKind::from_i64(raw).ok_or_else(|| corrupt("kind", raw))
}

fn corrupt(what: &str, value: i64) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(
		0,
		Type::Integer,
		format!("invalid {what} in the staged plan: {value}").into(),
	)
}

/// What the tests do to a staged plan they cannot do through it.
#[cfg(test)]
mod test_support {
	use rusqlite::params;

	use super::{super::super::plan::SyncAction, Pager, PlanWriter, StagedPlan, action_columns};

	impl PlanWriter {
		/// Stage `paths`, the deletions the directory deletion `action` carries out along with its
		/// own. Nothing for an action that is no deletion.
		pub(in crate::sync_engine) fn carry(
			&mut self,
			action: &SyncAction,
			paths: &[String],
		) -> rusqlite::Result<()> {
			if !action.is_delete() {
				return Ok(());
			}
			let (op, dir) = (action_columns(action).0, action.rel_path());
			for path in paths {
				self.batch.row(|conn| {
					conn.prepare_cached("INSERT INTO carried (op, dir, path) VALUES (?1, ?2, ?3)")?
						.execute(params![op, dir, path])?;
					Ok(())
				})?;
			}
			self.carried += paths.len();
			Ok(())
		}
	}

	impl StagedPlan {
		/// Spoil every action from position `ord` on, so that the page holding it fails to read
		/// while the pages before it read as staged.
		pub(in crate::sync_engine) fn spoil_from(&self, ord: usize) {
			self.stage
				.lock()
				.conn
				.execute(
					"UPDATE plan SET op = -1 WHERE seq IN (SELECT seq FROM exec WHERE ord >= ?1)",
					[ord],
				)
				.expect("spoiling the staged plan");
		}
	}

	impl Pager {
		/// Take the plan's tables out from under the pager, so that every read of the stage that
		/// asks them and every write it records fails from here on, the way one on a full or
		/// failing disk would.
		pub(in crate::sync_engine) fn break_stage(&self) {
			self.plan
				.stage
				.lock()
				.conn
				.execute_batch("DROP TABLE plan; DROP TABLE written")
				.expect("dropping the staged plan's tables");
		}
	}
}
