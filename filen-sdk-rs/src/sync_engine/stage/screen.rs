//! A pass's plan, screened in its stage: every step from the plan the reconcile made to the one the
//! apply runs — the plan finished and ordered, the actions screened out of it, the guard's count and
//! hold, the held batch's token, and the deletions folded into the directory deletions that carry
//! them — made on the plan where it is staged, a page of it at a time, rather than on a list of it.
//!
//! # The table
//!
//! `screen` holds an action per row, keyed by `pos`, its place in the plan: every read that walks
//! the plan walks it in `pos` order, and the one step that reorders the plan moves rows to new
//! positions ([`Screen::lead_creates`]). `held` marks what the guard holds ([`Screen::hold`]), and
//! `carried_by` the directory deletion a deletion is folded into ([`Screen::fold_subsumed`]).
//!
//! # The order
//!
//! A whole pass's windows write their plan unordered, and it is ordered here as `plan::finish_plan`
//! orders a plan in memory, by `plan::action_phase` and the path within each phase
//! ([`Screen::order`]), which the plan's tail of trashed and conflicted actions is then screened out
//! of. A change-scoped pass's plan is finished in memory, and written here in its order
//! ([`Screen::new`]).
//!
//! The directory moves the pass folded lead the plan, in the order the fold found them, at
//! negative positions [`LEAD_ROOM`] apart; the creates of the directories above a move's
//! destination are moved into the room just before it ([`Screen::lead_creates`]).
//!
//! # What it holds
//!
//! A page of the plan at a time, and what each step is decided by: the paths the pass blocks,
//! withholds or holds in conflict, which are few — a pass's problem paths, not its tree. What a list
//! of the plan cost — the plan itself, the copy an approval released, the partitions of the guard and
//! of the conflicts, a line per held deletion for the token, and the sets of the folded deletions —
//! is not held at all.
//!
//! # A failed read or write
//!
//! Is an error the pass returns, before it has applied anything: the plan is screened before the
//! apply starts, and the apply reads its own copy, staged from this one ([`Screen::stage_plan`]).

use std::{
	collections::{HashMap, HashSet},
	path::Path,
};

use rusqlite::{Connection, Row, params};
use uuid::Uuid;

use super::{
	super::{
		apply::{PassStart, PlannedEntry},
		baseline::NodeKind,
		plan::{
			self, PHASE_CONFLICT, PHASE_CREATE, PHASE_DELETE, PHASE_DIR_MOVE,
			PHASE_DIR_REPLACE_DELETE, PHASE_FILE_REPLACE_DELETE, PHASE_MOVE, PHASE_TRANSFER,
			SyncAction,
		},
	},
	BatchWriter, Stage,
	plan::{
		CREATES, DELETES, MOVES, OP_ADOPT, OP_CONFLICT, OP_CREATE_LOCAL_DIR, OP_CREATE_REMOTE_DIR,
		OP_DELETE_LOCAL, OP_DOWNLOAD, OP_TRASH_REMOTE, OP_UPLOAD, PlanWriter, StagedPlan,
		action_columns, action_of,
	},
};

const SCHEMA: &str = "
CREATE TABLE screen (
	pos INTEGER PRIMARY KEY,
	op INTEGER NOT NULL,
	kind INTEGER,
	path TEXT NOT NULL,
	from_path TEXT,
	remote_uuid BLOB,
	held INTEGER NOT NULL DEFAULT 0,
	carried_by TEXT
);";

const INSERT: &str = "INSERT INTO screen (pos, op, kind, path, from_path, remote_uuid)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6)";

/// How far apart the directory moves leading the plan are: the room before each one holds the
/// creates it runs right after, which are the directories above its destination — as many as it is
/// deep, and a path is far shallower than this.
const LEAD_ROOM: i64 = 1 << 20;

/// How many actions a sweep reads a statement.
const SWEEP_PAGE: i64 = 1024;

/// Which actions of the plan a read of it visits.
#[derive(Debug, Clone, Copy)]
pub(in super::super) enum Rows {
	/// Every action.
	All,
	/// Every move, of a file or a directory.
	Moves,
	/// Every deletion, on either side.
	Deletes,
	/// What the guard holds ([`Screen::hold`]).
	Held,
	/// What the guard lets through, but for what a directory deletion carries
	/// ([`Screen::fold_subsumed`]).
	Safe,
	/// The remote deletions.
	Trashes,
	/// The conflicts.
	Conflicts,
	/// The directory moves.
	DirMoves,
	/// The directory creates, on either side.
	DirCreates,
}

impl Rows {
	fn condition(self) -> String {
		match self {
			Self::All => "1".to_owned(),
			Self::Moves => format!("op IN {MOVES}"),
			Self::Deletes => format!("op IN {DELETES}"),
			Self::Held => "held = 1".to_owned(),
			Self::Safe => "held = 0 AND carried_by IS NULL".to_owned(),
			Self::Trashes => format!("op = {OP_TRASH_REMOTE}"),
			Self::Conflicts => format!("op = {OP_CONFLICT}"),
			Self::DirMoves => format!("op IN {MOVES} AND kind = {}", NodeKind::Dir.as_i64()),
			Self::DirCreates => format!("op IN ({OP_CREATE_LOCAL_DIR}, {OP_CREATE_REMOTE_DIR})"),
		}
	}
}

/// Why a plan could not be screened: its stage failed, or [`Screen::lead_creates`] was handed a
/// plan this module did not make.
#[derive(Debug, thiserror::Error)]
pub(in super::super) enum ScreenError {
	/// A read or a write of the stage failed.
	#[error(transparent)]
	Sql(#[from] rusqlite::Error),
	/// A directory move the plan holds at `pos`, past the lead the fold's moves are put in.
	#[error("the directory move to {to:?} at {pos} does not lead the pass's screened plan")]
	MoveNotLeading { to: String, pos: i64 },
	/// The room before the directory move at `by` is full.
	#[error(
		"{} or more creates run before the directory move at {by} in the pass's screened plan",
		LEAD_ROOM
	)]
	LeadRoomFull { by: i64 },
}

/// A pass's plan, screened in its stage (see the module docs).
pub(in super::super) struct Screen {
	stage: Stage,
}

impl Screen {
	/// `actions`, a plan finished in memory (`plan::finish_plan`), in `stage` in that order, led by
	/// the directory moves the pass folded (`dir_moves`).
	pub(in super::super) fn new<'a>(
		stage: Stage,
		dir_moves: &[SyncAction],
		actions: impl IntoIterator<Item = &'a SyncAction>,
	) -> rusqlite::Result<Self> {
		stage.conn.execute_batch(SCHEMA)?;
		let mut batch = BatchWriter::new(stage);
		for (pos, action) in (0..).zip(actions) {
			batch.row(|conn| insert(conn, pos, action))?;
		}
		let mut screen = Self {
			stage: batch.finish()?,
		};
		screen.lead_with(dir_moves)?;
		Ok(screen)
	}

	/// The plan a whole pass's windows reconciled into `stage`'s `reconciled` table (`seq`, `op`,
	/// `kind`, `path`, `from_path`, `remote_uuid`), in the order `plan::finish_plan` sorts a plan
	/// into: by `plan::action_phase`, then by path — descending in the two phases of deletions that
	/// cascade — and then by `seq`, the order the reconcile planned them in, as the sort is stable.
	///
	/// The phase of a deletion asks whether the plan creates at its path. Only a plan with both
	/// deletions and creates can, and only then is `reconciled` indexed by path for it to ask.
	///
	/// What [`retire_trashed`](Self::retire_trashed) and
	/// [`suppress_conflicted`](Self::suppress_conflicted) then take out of the plan, `finish_plan`
	/// takes out before it sorts; no phase of what is left changes either way: the one takes remote
	/// deletions alone, and the other a path's every action, a deletion and the create at its path
	/// alike.
	pub(super) fn order(stage: Stage) -> rusqlite::Result<Self> {
		stage.conn.execute_batch(SCHEMA)?;
		let flips: bool = stage.conn.query_row(
			&format!(
				"SELECT EXISTS (SELECT 1 FROM reconciled WHERE op IN {DELETES})
				 AND EXISTS (SELECT 1 FROM reconciled WHERE op IN {CREATES})"
			),
			[],
			|row| row.get(0),
		)?;
		if flips {
			stage
				.conn
				.execute_batch("CREATE INDEX reconciled_path ON reconciled (path)")?;
		}
		stage.conn.execute(&order_statement(flips), [])?;
		Ok(Self { stage })
	}

	/// The stage's connection, for the reconcile to drop its own tables from.
	pub(super) fn conn(&self) -> &Connection {
		&self.stage.conn
	}

	/// Where the stage's file is.
	pub(in super::super) fn path(&self) -> &Path {
		self.stage.path()
	}

	/// Take out every deletion of a remote item this engine already trashed (`trashed`), which the
	/// cache has not caught up with (see `plan::already_trashed`), and say how many: each is a path
	/// the pass defers.
	pub(super) fn retire_trashed(&mut self, trashed: &HashSet<Uuid>) -> rusqlite::Result<usize> {
		if trashed.is_empty() {
			return Ok(0);
		}
		self.retain(Rows::Trashes, |action| {
			!plan::already_trashed(action, trashed)
		})
	}

	/// Take out every action under a path the plan surfaces as a conflict (see
	/// `plan::under_conflict`).
	pub(super) fn suppress_conflicted(&mut self) -> rusqlite::Result<()> {
		let mut conflicted = Vec::new();
		self.each(Rows::Conflicts, |action| {
			conflicted.push(action.rel_path().to_owned());
		})?;
		if !conflicted.is_empty() {
			self.retain(Rows::All, |action| {
				!plan::under_conflict(action, &conflicted)
			})?;
		}
		Ok(())
	}

	/// Put `dir_moves` ahead of the plan, in their order, each [`LEAD_ROOM`] apart.
	pub(super) fn lead_with(&mut self, dir_moves: &[SyncAction]) -> rusqlite::Result<()> {
		if dir_moves.is_empty() {
			return Ok(());
		}
		let count = dir_moves.len();
		let lead = self.stage.conn.transaction()?;
		for (index, action) in dir_moves.iter().enumerate() {
			let pos = i64::try_from(count - index)
				.ok()
				.and_then(|ahead| ahead.checked_mul(LEAD_ROOM))
				.expect("a pass folds fewer than 2^43 directory moves (should be impossible)");
			insert(&lead, -pos, action)?;
		}
		lead.commit()
	}

	/// How many actions of `rows` the plan holds.
	pub(in super::super) fn count(&self, rows: Rows) -> rusqlite::Result<usize> {
		let count: i64 = self.stage.conn.query_row(
			&format!("SELECT COUNT(*) FROM screen WHERE {}", rows.condition()),
			[],
			|row| row.get(0),
		)?;
		Ok(usize::try_from(count).unwrap_or(usize::MAX))
	}

	/// Hand `visit` every action of `rows`, in plan order.
	pub(in super::super) fn each(
		&self,
		rows: Rows,
		mut visit: impl FnMut(SyncAction),
	) -> rusqlite::Result<()> {
		self.sweep(rows, |_, action| {
			visit(action);
			true
		})
	}

	/// Hand `visit` every action of `rows`, in plan order, until it answers `false`.
	pub(in super::super) fn each_while(
		&self,
		rows: Rows,
		mut visit: impl FnMut(SyncAction) -> bool,
	) -> rusqlite::Result<()> {
		self.sweep(rows, |_, action| visit(action))
	}

	/// Take out every action of `rows` that `keep` refuses, and say how many it took. What `rows`
	/// leaves out is kept.
	pub(in super::super) fn retain(
		&mut self,
		rows: Rows,
		mut keep: impl FnMut(&SyncAction) -> bool,
	) -> rusqlite::Result<usize> {
		let mut taken = 0;
		let mut after = i64::MIN;
		loop {
			let page = page(&self.stage.conn, rows, after)?;
			let Some(&(last, _)) = page.last() else {
				return Ok(taken);
			};
			after = last;
			let dropped: Vec<i64> = page
				.iter()
				.filter(|(_, action)| !keep(action))
				.map(|(pos, _)| *pos)
				.collect();
			if dropped.is_empty() {
				continue;
			}
			let taking = self.stage.conn.transaction()?;
			for pos in &dropped {
				taking
					.prepare_cached("DELETE FROM screen WHERE pos = ?1")?
					.execute([pos])?;
			}
			taking.commit()?;
			taken += dropped.len();
		}
	}

	/// Run each directory move right after the creates of the new directories above its destination
	/// on the side it moves, and everything else in the order it has: each such create moves into the
	/// room before the first move it is above, in the order the creates have (see the module docs).
	///
	/// The moves are the pass's fold's, which lead the plan; one the plan holds anywhere else is a
	/// plan this did not make, and refused rather than reordered, as is a room too full to take
	/// another create.
	pub(in super::super) fn lead_creates(&mut self) -> Result<(), ScreenError> {
		let mut moves: Vec<(i64, bool, String)> = Vec::new();
		self.sweep(Rows::DirMoves, |pos, action| {
			let local = matches!(action, SyncAction::MoveLocal { .. });
			moves.push((pos, local, action.rel_path().to_owned()));
			true
		})?;
		if moves.is_empty() {
			return Ok(());
		}
		if let Some((pos, _, to)) = moves.iter().find(|(pos, _, _)| *pos >= 0) {
			return Err(ScreenError::MoveNotLeading {
				to: to.clone(),
				pos: *pos,
			});
		}
		// Each directory above a destination, on the side that moves, by the first move under it.
		let mut first_under: HashMap<(bool, &str), i64> = HashMap::new();
		for (pos, local, to) in &moves {
			for dir in plan::ancestors(to) {
				first_under.entry((*local, dir)).or_insert(*pos);
			}
		}
		let mut led: Vec<(i64, i64)> = Vec::new();
		self.sweep(Rows::DirCreates, |pos, action| {
			let local = matches!(action, SyncAction::CreateLocalDir { .. });
			if let Some(&by) = first_under.get(&(local, action.rel_path())) {
				led.push((by, pos));
			}
			true
		})?;
		if led.is_empty() {
			return Ok(());
		}
		let mut room: HashMap<i64, i64> = HashMap::new();
		let lead = self.stage.conn.transaction()?;
		for (by, pos) in led {
			let taken = room.entry(by).or_insert(0);
			*taken += 1;
			if *taken >= LEAD_ROOM {
				return Err(ScreenError::LeadRoomFull { by });
			}
			lead.prepare_cached("UPDATE screen SET pos = ?2 WHERE pos = ?1")?
				.execute(params![pos, by - LEAD_ROOM + *taken])?;
		}
		Ok(lead.commit()?)
	}

	/// Hand `visit` the path of every conflict, in plan order, and take them all out of the plan.
	pub(in super::super) fn take_conflicts(
		&mut self,
		mut visit: impl FnMut(&str),
	) -> rusqlite::Result<()> {
		self.each(Rows::Conflicts, |action| visit(action.rel_path()))?;
		self.stage
			.conn
			.execute(&format!("DELETE FROM screen WHERE op = {OP_CONFLICT}"), [])?;
		Ok(())
	}

	/// Hold every deletion and every create at a path a deletion empties — what a guard's reason
	/// holds (see `guard::reason_for`) — and say how many actions that holds.
	///
	/// A create's question is a seek of the plan's paths, indexed for it here, and only a plan that
	/// has a create asks it.
	pub(in super::super) fn hold(&mut self) -> rusqlite::Result<usize> {
		let creates: bool = self.stage.conn.query_row(
			&format!("SELECT EXISTS (SELECT 1 FROM screen WHERE op IN {CREATES})"),
			[],
			|row| row.get(0),
		)?;
		let flips = if creates {
			self.stage
				.conn
				.execute_batch("CREATE INDEX IF NOT EXISTS screen_path ON screen (path)")?;
			format!(
				" OR (op IN {CREATES} AND EXISTS (SELECT 1 FROM screen AS deleted
				  WHERE deleted.path = screen.path AND deleted.op IN {DELETES}))"
			)
		} else {
			String::new()
		};
		self.stage.conn.execute(
			&format!("UPDATE screen SET held = 1 WHERE op IN {DELETES}{flips}"),
			[],
		)
	}

	/// Release everything the guard held: an approval of the held batch. Says how many actions that
	/// releases.
	pub(in super::super) fn release_held(&mut self) -> rusqlite::Result<usize> {
		self.stage
			.conn
			.execute("UPDATE screen SET held = 0 WHERE held = 1", [])
	}

	/// Hand `visit` every held action in the order of the held batch's token lines: the local
	/// deletions, then the rest, then the remote deletions, each by path — `local` before `other`
	/// before `remote`, which is how the lines sort.
	pub(in super::super) fn each_held_by_line(
		&self,
		mut visit: impl FnMut(&SyncAction),
	) -> rusqlite::Result<()> {
		let mut statement = self.stage.conn.prepare(&format!(
			"SELECT pos, op, kind, path, from_path, remote_uuid FROM screen WHERE held = 1
			 ORDER BY CASE op WHEN {OP_DELETE_LOCAL} THEN 0 WHEN {OP_TRASH_REMOTE} THEN 2 ELSE 1 END,
			 path"
		))?;
		let mut rows = statement.query([])?;
		while let Some(row) = rows.next()? {
			visit(&action_at(row)?);
		}
		Ok(())
	}

	/// Fold every deletion a directory deletion of the same side carries out anyway into the
	/// top-most such directory, as `plan::test_support::fold_subsumed_deletes` folds a list: it
	/// stays in the plan, marked `carried_by` the directory, and leaves [`Rows::Safe`]. Say how many
	/// it folded.
	///
	/// Each side's deletions are read once, in path order, with the directory deletions read so far
	/// on a stack. A subtree is not one run of that order — `a-b` sorts between `a` and `a/b` — so a
	/// directory leaves the stack only once a path sorts past everything under it (`a0` and on), and
	/// the top-most directory above a path is the first one on the stack it is under.
	pub(in super::super) fn fold_subsumed(&mut self) -> rusqlite::Result<usize> {
		let mut folded = 0;
		for op in [OP_DELETE_LOCAL, OP_TRASH_REMOTE] {
			let dirs: bool = self.stage.conn.query_row(
				"SELECT EXISTS (SELECT 1 FROM screen WHERE op = ?1 AND kind = ?2 AND held = 0)",
				params![op, NodeKind::Dir.as_i64()],
				|row| row.get(0),
			)?;
			if !dirs {
				continue;
			}
			let fold = self.stage.conn.transaction()?;
			fold.execute_batch(
				"CREATE TABLE IF NOT EXISTS subsumed (pos INTEGER PRIMARY KEY, dir TEXT NOT NULL)",
			)?;
			folded += fold_side(&fold, op)?;
			fold.commit()?;
		}
		if folded > 0 {
			self.stage.conn.execute_batch(
				"UPDATE screen SET carried_by = subsumed.dir FROM subsumed
				  WHERE subsumed.pos = screen.pos;
				 DROP TABLE subsumed;",
			)?;
		}
		Ok(folded)
	}

	/// Hand `visit` every deletion a directory deletion carries ([`fold_subsumed`]): the path of the
	/// directory, and the path of the deletion.
	///
	/// [`fold_subsumed`]: Self::fold_subsumed
	pub(in super::super) fn each_carried(
		&self,
		mut visit: impl FnMut(&str, &str),
	) -> rusqlite::Result<()> {
		let mut statement = self.stage.conn.prepare(
			"SELECT carried_by, path FROM screen WHERE carried_by IS NOT NULL ORDER BY pos",
		)?;
		let mut rows = statement.query([])?;
		while let Some(row) = rows.next()? {
			visit(row.get_ref(0)?.as_str()?, row.get_ref(1)?.as_str()?);
		}
		Ok(())
	}

	/// The plan the apply runs, staged in the same stage: every [`Rows::Safe`] action in plan order,
	/// with what `start` reads for it, and the deletions each directory deletion carries (see
	/// [`StagedPlan`]).
	pub(in super::super) fn stage_plan(
		self,
		mut start: impl FnMut(&SyncAction) -> PassStart,
	) -> rusqlite::Result<StagedPlan> {
		let mut plan = PlanWriter::new(self.stage)?;
		let mut after = i64::MIN;
		loop {
			let page = page(plan.conn(), Rows::Safe, after)?;
			let Some(&(last, _)) = page.last() else {
				break;
			};
			after = last;
			for (_, action) in page {
				let start = start(&action);
				plan.push(&PlannedEntry { action, start })?;
			}
		}
		plan.carry_screened()?;
		plan.finish()
	}

	/// The stage, done with.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(in super::super) fn into_stage(self) -> Stage {
		self.stage
	}

	/// Every action of `rows`, in plan order.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(in super::super) fn read(&self, rows: Rows) -> rusqlite::Result<Vec<SyncAction>> {
		let mut read = Vec::new();
		self.each(rows, |action| read.push(action))?;
		Ok(read)
	}

	/// Hand `visit` every action of `rows` with its position, in plan order, a page at a time, until
	/// it answers `false`.
	fn sweep(
		&self,
		rows: Rows,
		mut visit: impl FnMut(i64, SyncAction) -> bool,
	) -> rusqlite::Result<()> {
		let mut after = i64::MIN;
		loop {
			let page = page(&self.stage.conn, rows, after)?;
			let Some(&(last, _)) = page.last() else {
				return Ok(());
			};
			after = last;
			for (pos, action) in page {
				if !visit(pos, action) {
					return Ok(());
				}
			}
		}
	}
}

/// Fold the deletions of the side whose deletions are `op` (see [`Screen::fold_subsumed`]) into
/// `subsumed`, and say how many.
fn fold_side(conn: &Connection, op: i64) -> rusqlite::Result<usize> {
	let mut deletes = conn.prepare(
		"SELECT pos, op, kind, path, from_path, remote_uuid FROM screen
		 WHERE op = ?1 AND held = 0 ORDER BY path, pos",
	)?;
	let mut fold = conn.prepare("INSERT INTO subsumed (pos, dir) VALUES (?1, ?2)")?;
	let mut rows = deletes.query([op])?;
	// The directory deletions whose subtrees the paths still to come may be in.
	let mut open: Vec<String> = Vec::new();
	let mut folded = 0;
	while let Some(row) = rows.next()? {
		let pos: i64 = row.get(0)?;
		let action = action_at(row)?;
		let path = action.rel_path();
		while open.last().is_some_and(|dir| past_subtree(path, dir)) {
			open.pop();
		}
		if let Some(dir) = open.iter().find(|dir| plan::is_under(path, dir)) {
			tracing::trace!(
				"plan: {} — the deletion of {dir:?} takes it along",
				action.describe()
			);
			fold.execute(params![pos, dir])?;
			folded += 1;
		}
		if matches!(
			action,
			SyncAction::DeleteLocal {
				kind: NodeKind::Dir,
				..
			} | SyncAction::TrashRemote {
				kind: NodeKind::Dir,
				..
			}
		) {
			open.push(path.to_owned());
		}
	}
	Ok(folded)
}

/// The statement [`Screen::order`] runs (see there): `flips` says whether a deletion's phase has to
/// ask for a create at its path.
fn order_statement(flips: bool) -> String {
	let replaced = if flips {
		format!(
			"EXISTS (SELECT 1 FROM reconciled AS created
			 WHERE created.path = r.path AND created.op IN {CREATES})"
		)
	} else {
		"0".to_owned()
	};
	let (dir, file) = (NodeKind::Dir.as_i64(), NodeKind::File.as_i64());
	// `plan::action_phase`, arm for arm.
	let phase = format!(
		"CASE
		 WHEN op IN {MOVES} AND kind = {dir} THEN {PHASE_DIR_MOVE}
		 WHEN op IN {DELETES} AND kind = {file} AND {replaced} THEN {PHASE_FILE_REPLACE_DELETE}
		 WHEN op IN ({OP_ADOPT}, {OP_CREATE_LOCAL_DIR}, {OP_CREATE_REMOTE_DIR}) THEN {PHASE_CREATE}
		 WHEN op IN {MOVES} THEN {PHASE_MOVE}
		 WHEN op IN {DELETES} AND {replaced} THEN {PHASE_DIR_REPLACE_DELETE}
		 WHEN op IN ({OP_DOWNLOAD}, {OP_UPLOAD}) THEN {PHASE_TRANSFER}
		 WHEN op IN {DELETES} THEN {PHASE_DELETE}
		 ELSE {PHASE_CONFLICT} END"
	);
	// The deletions that cascade run child before parent: by path, descending.
	let cascading = format!("({PHASE_DIR_REPLACE_DELETE}, {PHASE_DELETE})");
	format!(
		"INSERT INTO screen (pos, op, kind, path, from_path, remote_uuid)
		 SELECT ROW_NUMBER() OVER (ORDER BY phase,
		   CASE WHEN phase IN {cascading} THEN NULL ELSE path END,
		   CASE WHEN phase IN {cascading} THEN path END DESC,
		   seq) - 1,
		  op, kind, path, from_path, remote_uuid
		 FROM (SELECT seq, op, kind, path, from_path, remote_uuid, {phase} AS phase
		  FROM reconciled AS r)"
	)
}

/// Up to a page of `rows` after the position `after`, in plan order, each with its position.
fn page(conn: &Connection, rows: Rows, after: i64) -> rusqlite::Result<Vec<(i64, SyncAction)>> {
	conn.prepare_cached(&format!(
		"SELECT pos, op, kind, path, from_path, remote_uuid FROM screen
		 WHERE pos > ?1 AND {} ORDER BY pos LIMIT ?2",
		rows.condition()
	))?
	.query_map(params![after, SWEEP_PAGE], |row| {
		Ok((row.get(0)?, action_at(row)?))
	})?
	.collect()
}

/// The action a row of `screen` holds, its columns read as [`page`] selects them.
fn action_at(row: &Row<'_>) -> rusqlite::Result<SyncAction> {
	action_of(row, 1, row.get(3)?)
}

fn insert(conn: &Connection, pos: i64, action: &SyncAction) -> rusqlite::Result<()> {
	let (op, kind, from_path, remote_uuid) = action_columns(action);
	conn.prepare_cached(INSERT)?.execute(params![
		pos,
		op,
		kind.map(NodeKind::as_i64),
		action.rel_path(),
		from_path,
		remote_uuid,
	])?;
	Ok(())
}

/// Whether `path` sorts past everything under `dir`: at `dir0` or after it (see
/// `plan::subtree_bounds`). What sorts between `dir` and `dir/` — `dir-b`, `dir.b` — is not.
fn past_subtree(path: &str, dir: &str) -> bool {
	match path.strip_prefix(dir) {
		Some(rest) => rest.as_bytes().first().is_some_and(|byte| *byte >= b'0'),
		None => path > dir,
	}
}

#[cfg(test)]
mod tests {
	use rand::{Rng, SeedableRng, rngs::StdRng};

	use super::{super::super::plan::test_support, *};

	fn delete(rel_path: &str, kind: NodeKind) -> SyncAction {
		SyncAction::DeleteLocal {
			rel_path: rel_path.to_string(),
			kind,
		}
	}

	fn trash(rel_path: &str, kind: NodeKind) -> SyncAction {
		SyncAction::TrashRemote {
			rel_path: rel_path.to_string(),
			kind,
			remote_uuid: Uuid::from_u128(1),
		}
	}

	/// `actions`, screened as they stand in a stage of their own.
	fn screened(actions: &[SyncAction]) -> (tempfile::TempDir, Screen) {
		let (dir, stage) = Stage::in_temp_dir();
		(dir, Screen::new(stage, &[], actions).unwrap())
	}

	/// What the screen folded, read back: the actions left safe, in plan order, and each carried
	/// deletion with its directory.
	fn folded(screen: &mut Screen) -> (Vec<SyncAction>, Vec<(String, String)>) {
		screen.fold_subsumed().unwrap();
		let mut carried = Vec::new();
		screen
			.each_carried(|dir, path| carried.push((dir.to_owned(), path.to_owned())))
			.unwrap();
		carried.sort();
		(screen.read(Rows::Safe).unwrap(), carried)
	}

	/// A directory's deletion carries everything under it on its side, the top-most directory's
	/// taking what a nested one would — and nothing that only shares its name's prefix, though
	/// `docs-old` and `docs.bak` sort between `docs` and what is under it.
	#[test]
	fn a_directory_deletion_carries_its_subtree_past_the_names_that_sort_inside_it() {
		let actions = vec![
			delete("docs", NodeKind::Dir),
			delete("docs-old", NodeKind::Dir),
			delete("docs-old/x", NodeKind::File),
			delete("docs.bak", NodeKind::File),
			delete("docs/a", NodeKind::File),
			delete("docs/sub", NodeKind::Dir),
			delete("docs/sub/b", NodeKind::File),
			delete("docs0", NodeKind::File),
			trash("docs/c", NodeKind::File),
		];
		let (_dir, mut screen) = screened(&actions);
		let (safe, carried) = folded(&mut screen);
		assert_eq!(
			safe,
			vec![
				delete("docs", NodeKind::Dir),
				delete("docs-old", NodeKind::Dir),
				delete("docs.bak", NodeKind::File),
				delete("docs0", NodeKind::File),
				trash("docs/c", NodeKind::File),
			]
		);
		assert_eq!(
			carried,
			[
				("docs", "docs/a"),
				("docs", "docs/sub"),
				("docs", "docs/sub/b"),
				("docs-old", "docs-old/x"),
			]
			.map(|(dir, path)| (dir.to_owned(), path.to_owned()))
		);
		assert_eq!((safe, carried), test_support::folded(actions));
	}

	/// A random deletion over names that sort inside one another's subtrees.
	fn random_deletion(rng: &mut StdRng) -> SyncAction {
		let path = (0..rng.random_range(1..=4))
			.map(|_| ["a", "a-b", "a.b", "a0", "b"][rng.random_range(0..5)])
			.collect::<Vec<_>>()
			.join("/");
		let kind = if rng.random_bool(0.5) {
			NodeKind::Dir
		} else {
			NodeKind::File
		};
		if rng.random_bool(0.5) {
			delete(&path, kind)
		} else {
			trash(&path, kind)
		}
	}

	/// The fold over the stage carries what the fold over a list carries, over plans of every shape.
	#[test]
	fn the_staged_fold_is_the_list_fold_over_generated_plans() {
		for seed in 0..300u64 {
			let mut rng = StdRng::seed_from_u64(seed);
			let len = rng.random_range(0..=40);
			let actions: Vec<SyncAction> = (0..len).map(|_| random_deletion(&mut rng)).collect();
			let (_dir, mut screen) = screened(&actions);
			assert_eq!(
				folded(&mut screen),
				test_support::folded(actions.clone()),
				"seed {seed}: {actions:?}"
			);
		}
	}

	/// A directory move the plan holds anywhere but ahead of it is a plan the fold did not make, and
	/// is refused as such, by where it is, rather than reordered.
	#[test]
	fn a_directory_move_that_does_not_lead_the_plan_is_refused() {
		let (_dir, mut screen) = screened(&[
			SyncAction::CreateLocalDir {
				rel_path: "x".to_owned(),
			},
			SyncAction::MoveLocal {
				from_path: "a".to_owned(),
				to_path: "x/a".to_owned(),
				kind: NodeKind::Dir,
			},
		]);
		let error = screen.lead_creates().unwrap_err();
		assert!(
			matches!(&error, ScreenError::MoveNotLeading { to, pos: 1 } if to == "x/a"),
			"{error:?}"
		);
		assert_eq!(
			error.to_string(),
			"the directory move to \"x/a\" at 1 does not lead the pass's screened plan"
		);
	}

	#[test]
	fn a_path_sorts_past_a_subtree_only_once_it_is_past_everything_under_it() {
		for (path, past) in [
			("a", false),
			("a-b", false),
			("a.b", false),
			("a/b", false),
			("a/z/z", false),
			("a0", true),
			("ab", true),
			("b", true),
		] {
			assert_eq!(past_subtree(path, "a"), past, "{path:?}");
		}
	}
}
