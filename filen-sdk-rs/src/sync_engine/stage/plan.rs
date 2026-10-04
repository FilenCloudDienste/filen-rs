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
//!
//! # The order
//!
//! The apply runs the plan in three [`Run`]s. First, one at a time and in plan order, everything
//! but the transfers and the deletions — the moves and creates the transfers land in — and the
//! deletion half of each type flip (a deletion whose path a create of the same pass fills again),
//! which has to clear the path before that create runs. Then the transfers, concurrently. Then the
//! other deletions, one at a time and in plan order, which is children first.
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

use std::{collections::VecDeque, sync::LazyLock};

use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use rusqlite::{Connection, OptionalExtension, Row, ffi, params, types::Type};
use uuid::Uuid;

use super::{
	super::{
		apply::{PassStart, PlannedEntry, StartInputs},
		baseline::{BaselineEntry, BaselineStore, ENTRY_COLUMNS, NodeKind},
		plan::{RemoteNode, SyncAction},
		scan::LocalNode,
	},
	BatchWriter, Stage,
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
";

/// `plan.op`: which [`SyncAction`] the row is.
const OP_CREATE_LOCAL_DIR: i64 = 0;
const OP_DOWNLOAD: i64 = 1;
const OP_DELETE_LOCAL: i64 = 2;
const OP_CREATE_REMOTE_DIR: i64 = 3;
const OP_UPLOAD: i64 = 4;
const OP_TRASH_REMOTE: i64 = 5;
const OP_CONFLICT: i64 = 6;
const OP_ADOPT: i64 = 7;
const OP_MOVE_REMOTE: i64 = 8;
const OP_MOVE_LOCAL: i64 = 9;

// `plan_created` in SCHEMA spells this op out: with any other value every created-directory
// lookup would scan the plan instead of seeking the index.
const _: () = assert!(OP_CREATE_REMOTE_DIR == 3);

/// The ops of [`SyncAction::is_delete`] and of [`SyncAction::is_create`], for the statements below.
const DELETES: &str = "(2, 5)";
const CREATES: &str = "(0, 1, 3, 4)";
const MOVES: &str = "(8, 9)";

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

/// A page of the plan in apply order, with what [`entry_of`] reads by position.
const READ_PAGE: &str = "SELECT x.ord, x.tie_until, p.seq, p.op, p.kind, p.path, p.from_path,
	 p.remote_uuid, p.l_path, p.l_kind, p.l_size, p.l_mtime, p.l_hash,
	 p.r_path, p.r_kind, p.r_uuid, p.r_stable, p.r_hash, p.r_size, p.r_modified, p.parent_uuid
	 FROM exec AS x JOIN plan AS p ON p.seq = x.seq
	 WHERE x.ord >= ?1 AND x.ord < ?2 ORDER BY x.ord LIMIT ?3";

/// Which part of the apply an action runs in (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in super::super) enum Run {
	/// One at a time, before any transfer.
	Before,
	/// The file transfers, concurrently.
	Transfers,
	/// One at a time, after every transfer: the deletions that are not half of a type flip.
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
		})
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
		} = self;
		let stage = batch.finish()?;
		let conn = &stage.conn;
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
			stage,
			before: before + flips,
			transfers,
			len: before + transfers + after,
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
	conn.prepare_cached(&INSERT_BASE)?.execute(params![
		seq,
		endpoint,
		row.rel_path,
		row.kind.as_i64(),
		row.remote_uuid,
		hash_bytes(row.content_hash.as_ref()),
		row.size,
		row.local_mtime,
		row.remote_modified,
		row.state.as_i64(),
		row.local_kind.map(NodeKind::as_i64),
		row.remote_kind.map(NodeKind::as_i64),
		hash_bytes(row.remote_hash.as_ref()),
		row.remote_size,
		row.remote_stable_uuid,
		hash_bytes(row.agreed_hash.as_ref()),
	])?;
	Ok(())
}

fn hash_bytes(hash: Option<&Blake3Hash>) -> Option<&[u8]> {
	hash.map(|hash| hash.as_ref().as_slice())
}

/// `(op, kind, from_path, remote_uuid)` of `action`, whose path is [`SyncAction::rel_path`].
fn action_columns(action: &SyncAction) -> (i64, Option<NodeKind>, Option<&str>, Option<Uuid>) {
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
	stage: Stage,
	/// How many actions run in [`Run::Before`], and in [`Run::Transfers`] after them.
	before: usize,
	transfers: usize,
	len: usize,
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
			let page = read_page(&self.plan.stage, at, end)?;
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
			.conn
			.prepare_cached(&READ_CREATED)?
			.query_row([path], |row| row.get::<_, Option<Uuid>>(0))
			.optional()?
			.flatten())
	}

	fn read_page(&mut self) -> rusqlite::Result<()> {
		let page = read_page(&self.plan.stage, self.next, self.plan.len)?;
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
	Ok((action_of(row, path)?, inputs))
}

/// The action a [`READ_PAGE`] row stages, at `path`.
fn action_of(row: &Row<'_>, path: String) -> rusqlite::Result<SyncAction> {
	let op: i64 = row.get(3)?;
	let kind = || -> rusqlite::Result<NodeKind> {
		let raw: Option<i64> = row.get(4)?;
		kind_of(raw.ok_or_else(|| corrupt("missing kind for op", op))?)
	};
	let from_path = || -> rusqlite::Result<String> {
		row.get::<_, Option<String>>(6)?
			.ok_or_else(|| corrupt("missing from_path for op", op))
	};
	let remote_uuid = || -> rusqlite::Result<Uuid> {
		row.get::<_, Option<Uuid>>(7)?
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

fn hash_at(row: &Row<'_>, column: usize) -> rusqlite::Result<Option<Blake3Hash>> {
	Ok(row
		.get::<_, Option<[u8; 32]>>(column)?
		.map(Blake3Hash::from))
}

fn corrupt(what: &str, value: i64) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(
		0,
		Type::Integer,
		format!("invalid {what} in the staged plan: {value}").into(),
	)
}

/// What the apply's tests do to a staged plan they cannot do through it.
#[cfg(test)]
mod test_support {
	use super::{Pager, StagedPlan};

	impl StagedPlan {
		/// Spoil every action from position `ord` on, so that the page holding it fails to read
		/// while the pages before it read as staged.
		pub(in crate::sync_engine) fn spoil_from(&self, ord: usize) {
			self.stage
				.conn
				.execute(
					"UPDATE plan SET op = -1 WHERE seq IN (SELECT seq FROM exec WHERE ord >= ?1)",
					[ord],
				)
				.expect("spoiling the staged plan");
		}
	}

	impl Pager {
		/// Take the plan's table out from under the pager, so that every read of the stage that
		/// asks it fails from here on, the way one on a full or failing disk would.
		pub(in crate::sync_engine) fn break_stage(&self) {
			self.plan
				.stage
				.conn
				.execute_batch("DROP TABLE plan")
				.expect("dropping the staged plan's table");
		}
	}
}
