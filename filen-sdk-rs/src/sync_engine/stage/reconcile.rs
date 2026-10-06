//! A whole pass's reconcile, window by window, into the pass's stage: the plan a whole pass makes
//! from its two staged sides and its rows, decided path by path as the resident reconcile decides
//! it (`plan::reconcile`), with no set of the tree's paths in memory and each action written to the
//! stage as it is planned.
//!
//! # Windows
//!
//! The local side, the remote side and the rows are each read in path order, at most [`WINDOW`]
//! entries of each held at once. A window is the key range `[lo, hi)`, `hi` being the least of the
//! three inputs' `WINDOW + 1`-th keys from `lo` on, or the end where none has that many left: each
//! input holds at most `WINDOW` entries inside it, and every entry it held past `hi` waits for the
//! next one. Its keys are what any of the three holds there — the sides' nodes and every row, held
//! by a side or by neither — each decided once, in path order, by the per-path decision the
//! resident reconcile makes (`plan::decide_path`), which reads the three inputs at that one path
//! and nothing else. The actions it plans go into `reconciled` as they are made, each with a `seq`
//! that grows with the windows, from [`WINDOW_SEQ`]. A path under one the cache is holding is
//! decided by nobody and counted as deferred, as the resident reconcile counts it.
//!
//! # The move join
//!
//! A file move is the one decision that reads more than its own path: the source row's file is
//! found at its new path by uuid or lineage on the remote, or by content on the local side. A window
//! only notes the candidates its keys hold, in `cand`: the rows each half of the join can move from
//! (`plan::pull_source`, `plan::push_source`), in path order, and the new local files a moved row's
//! content can reappear at (`plan::push_target`). A first sync has no row to move from and notes
//! nothing; a converged tree notes none either.
//!
//! Once every window is done, the join runs over the sources in path order, the pull half first,
//! through the decision bodies the resident join runs (`plan::pull_move`, `plan::push_move`). The
//! pull half finds a file by the remote side's own uuid and lineage indexes, built by the first
//! source that asks; the push half finds the new local files carrying a content in `cand`, and runs
//! only where a window noted one, as the resident join skips a half with no targets: a mass deletion
//! notes none. The paths each move takes go into `consumed`, which the later sources of the join
//! ask, as the resident join asks its own set; the actions of the join take `seq` values from 0,
//! below every window's, so they come before them in the plan as the resident reconcile emits them
//! first.
//!
//! A window plans at a path a move later takes all the same — the move is found after it — so the
//! windows' actions at any path in `consumed` are taken out of the plan before it is finished
//! ([`Windows::screen`]): what the resident reconcile, which joins first, never plans there.
//!
//! # What the pass holds
//!
//! The three windows and a page of each input's read behind them: about a megabyte at [`WINDOW`],
//! whatever the tree holds. The plan stays in the stage: it is finished and screened there (see
//! `stage::Screen`), and never read back into memory whole.
//!
//! # A failed read or write
//!
//! A write that fails — a full disk — is an error the pass returns before it applies anything. A
//! read of what the windows wrote, made while the join decides, panics instead, as a read of a staged
//! side does (see `stage::side`): "not taken" and "no candidate" are answers the join acts on, and a
//! failed read must not become one.

use std::{
	borrow::Cow,
	collections::{BTreeSet, VecDeque},
	num::NonZeroUsize,
};

use rusqlite::{Connection, params};
use uuid::Uuid;

#[cfg(any(test, feature = "bench-internals"))]
use super::super::plan::Plan;
use super::{
	super::{
		SyncMode,
		baseline::{BaselineEntry, NodeKind},
		plan::{self, MoveSides, PassHolds, RemoteNode, SyncAction, Took, is_under_held_path},
		rows::Baseline,
		scan::LocalNode,
	},
	BatchWriter, Stage,
	plan::action_columns,
	screen::{Rows, Screen},
	side::StagedSide,
};

/// How many entries of each of the three inputs a window holds at most (see the module docs): a
/// page of a staged side. Each window is read and dropped before the next, and what a window holds
/// is the most the planning holds at once, so it is no larger than the pages the sides are read in
/// anyway. Eight times as large took no less time and left the probe's fresh process holding
/// 10 MiB more at the end of a 100k pass.
pub(in super::super) const WINDOW: NonZeroUsize =
	NonZeroUsize::new(1024).expect("1024 is not zero");

const SCHEMA: &str = "
CREATE TABLE reconciled (
	seq INTEGER PRIMARY KEY,
	op INTEGER NOT NULL,
	kind INTEGER,
	path TEXT NOT NULL,
	from_path TEXT,
	remote_uuid BLOB
);
CREATE TABLE cand (role INTEGER NOT NULL, path TEXT NOT NULL, key BLOB);
CREATE TABLE consumed (path TEXT PRIMARY KEY) WITHOUT ROWID;
";

/// `cand.role`: a row the pull half can move from, keyed by the uuid it records; a row the push
/// half can move from; and a new local file a push can end at, keyed by its content hash.
const PULL_FROM: i64 = 0;
const PUSH_FROM: i64 = 1;
const PUSH_TO: i64 = 2;

/// The first `seq` of the windows' actions. The move join's start from 0, and a plan of 2^40 moves
/// is not one a pass can hold.
const WINDOW_SEQ: i64 = 1 << 40;

/// How many sources the join reads a statement.
const SOURCE_PAGE: i64 = 1024;

const INSERT_ACTION: &str = "INSERT INTO reconciled (seq, op, kind, path, from_path, remote_uuid)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6)";
const INSERT_CAND: &str = "INSERT INTO cand (role, path, key) VALUES (?1, ?2, ?3)";
const SOURCES: &str = "SELECT rowid, path, key FROM cand WHERE role = ?1 AND rowid > ?2
	 ORDER BY rowid LIMIT ?3";
/// Built for the push half, which looks its sources' content up in it.
const BY_CONTENT: &str = "CREATE INDEX cand_content ON cand (key) WHERE role = 2";
const PUSH_TARGETS: &str = "SELECT path FROM cand WHERE role = 2 AND key = ?1 ORDER BY rowid";
const CONSUME: &str = "INSERT OR IGNORE INTO consumed (path) VALUES (?1)";
const IS_CONSUMED: &str = "SELECT EXISTS (SELECT 1 FROM consumed WHERE path = ?1)";
/// A window's actions at a path a move took.
const DROP_CONSUMED: &str = "DELETE FROM reconciled
	 WHERE seq >= ?1 AND path IN (SELECT path FROM consumed)";
const DROP_TABLES: &str = "DROP TABLE reconciled; DROP TABLE cand; DROP TABLE consumed;";

// The statements above spell `PUSH_TO` out.
const _: () = assert!(PUSH_TO == 2);

/// A whole pass's plan, reconciled window by window into `stage` from its rows and its two staged
/// sides (see the module docs), `window` entries of each input a window: [`WINDOW`], which the
/// tests shrink to cut a handful of paths into several windows.
///
/// Plans what [`plan::reconcile`] plans over the same inputs, deciding every path, and defers what
/// it defers, once finished ([`Windows::screen`]).
pub(in super::super) fn reconcile_windows(
	mode: SyncMode,
	baseline: &Baseline,
	local: &StagedSide<LocalNode>,
	remote: &StagedSide<RemoteNode>,
	holds: &PassHolds,
	stage: Stage,
	window: NonZeroUsize,
) -> rusqlite::Result<Windows> {
	stage.conn.execute_batch(SCHEMA)?;
	// No row, no source: a first sync notes no candidate at all.
	let notes = !baseline.is_empty();
	let mut pass = Pass {
		mode,
		held: &holds.held_remote,
		pulls: notes && mode.pulls(),
		pushes: notes && mode.pushes(),
		out: Out {
			batch: BatchWriter::new(stage),
			next: WINDOW_SEQ,
		},
		actions: Vec::new(),
		keys: 0,
		deferred: 0,
		pull_sources: 0,
		push_sources: 0,
		push_targets: 0,
		consumed: false,
	};
	// The nodes alone: each carries its path, which the page hands out a copy of beside it.
	let (mut locals, mut remotes, mut rows) = (
		Lookahead::new(local.pages().map(|(_, node)| node)),
		Lookahead::new(remote.pages().map(|(_, node)| node)),
		Lookahead::new(baseline.iter()),
	);
	loop {
		let hi = [
			locals.bound(window),
			remotes.bound(window),
			rows.bound(window),
		]
		.into_iter()
		.flatten()
		.min()
		.map(str::to_owned);
		let hi = hi.as_deref();
		let (locals, remotes, rows) = (
			locals.take_below(hi),
			remotes.take_below(hi),
			rows.take_below(hi),
		);
		if locals.is_empty() && remotes.is_empty() && rows.is_empty() {
			break;
		}
		pass.window(locals, remotes, rows)?;
	}
	// A held path nothing holds is skipped by no window, but it IS being withheld and the report
	// has to say so.
	pass.deferred += holds
		.held_remote
		.iter()
		.filter(|path| !local.holds(path) && !remote.holds(path) && !baseline.contains_key(path))
		.count();
	tracing::debug!(
		"reconcile: mode {mode:?} — {} path(s) decided {window} a window, {} baseline row(s)",
		pass.keys,
		baseline.len()
	);
	let sides = MoveSides {
		mode,
		local,
		remote,
		baseline,
	};
	pass.join(&sides)?;
	Ok(Windows {
		stage: pass.out.batch.finish()?,
		deferred: pass.deferred,
		consumed: pass.consumed,
	})
}

/// What [`reconcile_windows`] staged, and the paths it deferred.
pub(in super::super) struct Windows {
	stage: Stage,
	deferred: usize,
	/// Whether a move of the join took a path, whose window actions are then out of the plan.
	consumed: bool,
}

impl Windows {
	/// The plan, finished in the stage as the resident reconcile finishes its own
	/// (`plan::finish_plan`) — the windows' actions at a path a move took left out, the deletions of
	/// what this engine already trashed and everything under a conflict taken out, the rest ordered —
	/// and led by the directory moves the pass folded (`dir_moves`): what [`plan::reconcile`] plans
	/// over the same inputs, behind those moves. With the paths the reconcile deferred, and the
	/// reconcile's own tables gone from the stage.
	pub(in super::super) fn screen(
		self,
		holds: &PassHolds,
		dir_moves: &[SyncAction],
	) -> rusqlite::Result<(Screen, usize)> {
		let Self {
			stage,
			mut deferred,
			consumed,
		} = self;
		if consumed {
			stage.conn.execute(DROP_CONSUMED, [WINDOW_SEQ])?;
		}
		let mut screen = Screen::order(stage)?;
		screen.conn().execute_batch(DROP_TABLES)?;
		deferred += screen.retire_trashed(&holds.trashed)?;
		screen.suppress_conflicted()?;
		if tracing::enabled!(tracing::Level::DEBUG) {
			tracing::debug!(
				"reconcile: planned {} action(s), {deferred} path(s) deferred",
				screen.count(Rows::All)?
			);
		}
		screen.lead_with(dir_moves)?;
		Ok((screen, deferred))
	}

	/// The plan, finished as [`screen`](Self::screen) finishes it and read back whole, with the
	/// stage: what the tests and the probe hold to the resident reconcile's plan.
	#[cfg(any(test, feature = "bench-internals"))]
	pub(in super::super) fn into_plan(self, holds: &PassHolds) -> rusqlite::Result<(Plan, Stage)> {
		let (screen, deferred_paths) = self.screen(holds, &[])?;
		let actions = screen.read(Rows::All)?;
		Ok((
			Plan {
				actions,
				deferred_paths,
			},
			screen.into_stage(),
		))
	}
}

/// One input of the windows, read in path order and held a window ahead.
struct Lookahead<I: Iterator> {
	read: I,
	held: VecDeque<I::Item>,
}

impl<I> Lookahead<I>
where
	I: Iterator,
	I::Item: Keyed,
{
	fn new(read: I) -> Self {
		Self {
			read,
			held: VecDeque::new(),
		}
	}

	/// The key of its `window + 1`-th entry ahead, which no window that leaves it at most `window`
	/// entries may reach; `None` where it has no more than `window` left.
	fn bound(&mut self, window: NonZeroUsize) -> Option<&str> {
		let window = window.get();
		while self.held.len() <= window {
			let Some(entry) = self.read.next() else {
				break;
			};
			self.held.push_back(entry);
		}
		self.held.get(window).map(Keyed::key)
	}

	/// Its entries before `hi`, or all it holds for `None`, in path order.
	fn take_below(&mut self, hi: Option<&str>) -> Vec<I::Item> {
		let below = hi.map_or(self.held.len(), |hi| {
			self.held.partition_point(|entry| entry.key() < hi)
		});
		self.held.drain(..below).collect()
	}
}

/// An entry of an input, by the path it is at.
trait Keyed {
	fn key(&self) -> &str;
}

impl Keyed for LocalNode {
	fn key(&self) -> &str {
		&self.rel_path
	}
}

impl Keyed for RemoteNode {
	fn key(&self) -> &str {
		&self.rel_path
	}
}

impl Keyed for BaselineEntry {
	fn key(&self) -> &str {
		&self.rel_path
	}
}

/// The windowed reconcile under way.
struct Pass<'h> {
	mode: SyncMode,
	/// The paths the cache is holding, which nothing under is decided.
	held: &'h BTreeSet<String>,
	/// Whether the windows note candidates for the pull half, and for the push half.
	pulls: bool,
	pushes: bool,
	out: Out,
	/// The actions of the path being decided, written as soon as it is.
	actions: Vec<SyncAction>,
	/// How many paths the windows met.
	keys: usize,
	deferred: usize,
	/// How many rows the windows noted as the pull half's sources, and as the push half's.
	pull_sources: usize,
	push_sources: usize,
	/// How many new local files the windows noted as the push half's targets: with none, no push
	/// source has anywhere to move to.
	push_targets: usize,
	/// Whether a source of the join took a path.
	consumed: bool,
}

impl Pass<'_> {
	/// Decide every key of one window, in path order.
	fn window(
		&mut self,
		locals: Vec<LocalNode>,
		remotes: Vec<RemoteNode>,
		rows: Vec<BaselineEntry>,
	) -> rusqlite::Result<()> {
		let (mut locals, mut remotes, mut rows) = (
			locals.into_iter().peekable(),
			remotes.into_iter().peekable(),
			rows.into_iter().peekable(),
		);
		loop {
			let (in_local, in_remote, in_rows) = {
				let keys = [
					locals.peek().map(Keyed::key),
					remotes.peek().map(Keyed::key),
					rows.peek().map(Keyed::key),
				];
				let Some(least) = keys.iter().flatten().min().copied() else {
					return Ok(());
				};
				(
					keys[0] == Some(least),
					keys[1] == Some(least),
					keys[2] == Some(least),
				)
			};
			let local = in_local.then(|| locals.next()).flatten();
			let remote = in_remote.then(|| remotes.next()).flatten();
			let base = in_rows.then(|| rows.next()).flatten();
			let key = local
				.as_ref()
				.map(Keyed::key)
				.or_else(|| remote.as_ref().map(Keyed::key))
				.or_else(|| base.as_ref().map(Keyed::key))
				.expect("the least key is held by an input (should be impossible)");
			self.decide(key, local.as_ref(), remote.as_ref(), base.as_ref())?;
		}
	}

	/// Decide `key`, noting it for the move join where it is one of its candidates.
	fn decide(
		&mut self,
		key: &str,
		local: Option<&LocalNode>,
		remote: Option<&RemoteNode>,
		base: Option<&BaselineEntry>,
	) -> rusqlite::Result<()> {
		self.keys += 1;
		if taken_by_held(self.held, key) {
			tracing::debug!(
				"reconcile: skipping {key:?} — the cache is listing that name twice, so the view cannot resolve it"
			);
			self.deferred += 1;
			return Ok(());
		}
		if let Some(base) = base {
			if self.pulls
				&& let Some(uuid) = plan::pull_source(base, || remote.map(Cow::Borrowed))
			{
				self.out
					.note(PULL_FROM, key, Some(uuid.as_bytes().as_slice()))?;
				self.pull_sources += 1;
			}
			if self.pushes
				&& plan::push_source(base, || local.is_some(), || remote.map(Cow::Borrowed))
					.is_some()
			{
				self.out.note(PUSH_FROM, key, None)?;
				self.push_sources += 1;
			}
		}
		if self.pushes
			&& let Some(node) = local
			&& let Some(hash) = plan::push_target(node, || remote.is_some(), || base.is_some())
		{
			self.out.note(PUSH_TO, key, Some(hash.as_slice()))?;
			self.push_targets += 1;
		}
		plan::decide_path(
			self.mode,
			key,
			base,
			|| (local.map(Cow::Borrowed), remote.map(Cow::Borrowed)),
			&mut self.actions,
		);
		for action in self.actions.drain(..) {
			self.out.action(&action)?;
		}
		Ok(())
	}

	/// The move join over the sources the windows noted (see the module docs): the pull half, then
	/// the push half, each source in path order.
	fn join(
		&mut self,
		sides: &MoveSides<'_, StagedSide<LocalNode>, StagedSide<RemoteNode>>,
	) -> rusqlite::Result<()> {
		let pulls = self.pull_sources > 0;
		// A source with no target moves nowhere, as the resident join skips a half with no targets.
		let pushes = self.push_sources > 0 && self.push_targets > 0;
		if !pulls && !pushes {
			return Ok(());
		}
		self.out.next = 0;
		let mut rows = sides.baseline.cursor();
		if pulls {
			self.each_source(PULL_FROM, |pass, path, key| {
				let base = rows
					.get(path)
					.expect("a row the windows read is a row of the pass (should be impossible)");
				let uuid = key
					.as_deref()
					.and_then(|key| Uuid::from_slice(key).ok())
					.expect("a pull source is noted with its uuid (should be impossible)");
				let target = || match sides.remote.file_of_uuid(uuid) {
					Some(to) => Some((Cow::Owned(to), false)),
					None => base
						.remote_stable_uuid
						.and_then(|lineage| sides.remote.file_of_lineage(lineage))
						.map(|to| (Cow::Owned(to), true)),
				};
				let took = plan::pull_move(
					sides,
					|path| taken(pass.held, pass.out.batch.conn(), path),
					&base,
					target,
					&mut pass.actions,
				);
				pass.record(path, took)
			})?;
		}
		if pushes {
			self.out.batch.conn().execute_batch(BY_CONTENT)?;
			self.each_source(PUSH_FROM, |pass, path, _| {
				let base = rows
					.get(path)
					.expect("a row the windows read is a row of the pass (should be impossible)");
				let conn = pass.out.batch.conn();
				let mut statement = conn.prepare_cached(PUSH_TARGETS)?;
				let targets = &mut statement;
				let candidates = move |hash: &[u8; 32]| {
					// Moved out of the closure, which a borrow of the statement may then outlive.
					let targets = targets;
					let paths = targets
						.query_map([hash.as_slice()], |row| row.get::<_, String>(0))
						.unwrap_or_else(|error| failed_read(conn, "a content's new files", &error));
					Some(paths.map(|path| {
						path.unwrap_or_else(|error| {
							failed_read(conn, "a content's new files", &error)
						})
					}))
				};
				let took = plan::push_move(
					sides,
					|path| taken(pass.held, conn, path),
					&base,
					candidates,
					&mut pass.actions,
				);
				drop(statement);
				pass.record(path, took)
			})?;
		}
		Ok(())
	}

	/// Hand `each` every source of `role` with its key, in path order, a page at a time.
	fn each_source(
		&mut self,
		role: i64,
		mut each: impl FnMut(&mut Self, &str, Option<Vec<u8>>) -> rusqlite::Result<()>,
	) -> rusqlite::Result<()> {
		let mut after = 0;
		loop {
			let page: Vec<(i64, String, Option<Vec<u8>>)> = self
				.out
				.batch
				.conn()
				.prepare_cached(SOURCES)?
				.query_map(params![role, after, SOURCE_PAGE], |row| {
					Ok((row.get(0)?, row.get(1)?, row.get(2)?))
				})?
				.collect::<rusqlite::Result<_>>()?;
			let Some(&(last, _, _)) = page.last() else {
				return Ok(());
			};
			after = last;
			for (_, path, key) in page {
				each(self, &path, key)?;
			}
		}
	}

	/// Write the actions a source of the join planned, and the paths it took (`from` and, for a
	/// move, where it went).
	fn record(&mut self, from: &str, took: Option<Took>) -> rusqlite::Result<()> {
		for action in self.actions.drain(..) {
			self.out.action(&action)?;
		}
		self.consumed |= took.is_some();
		match took {
			None => {}
			Some(Took::Source) => self.out.consume(from)?,
			Some(Took::Move { to }) => {
				self.out.consume(from)?;
				self.out.consume(&to)?;
			}
		}
		Ok(())
	}
}

/// What the windowed reconcile writes, a batch at a time.
struct Out {
	batch: BatchWriter,
	/// The `seq` the next action takes.
	next: i64,
}

impl Out {
	fn action(&mut self, action: &SyncAction) -> rusqlite::Result<()> {
		let seq = self.next;
		self.next += 1;
		let (op, kind, from_path, remote_uuid) = action_columns(action);
		self.batch.row(|conn| {
			conn.prepare_cached(INSERT_ACTION)?.execute(params![
				seq,
				op,
				kind.map(NodeKind::as_i64),
				action.rel_path(),
				from_path,
				remote_uuid,
			])?;
			Ok(())
		})
	}

	fn note(&mut self, role: i64, path: &str, key: Option<&[u8]>) -> rusqlite::Result<()> {
		self.batch.row(|conn| {
			conn.prepare_cached(INSERT_CAND)?
				.execute(params![role, path, key])?;
			Ok(())
		})
	}

	fn consume(&mut self, path: &str) -> rusqlite::Result<()> {
		self.batch.row(|conn| {
			conn.prepare_cached(CONSUME)?.execute([path])?;
			Ok(())
		})
	}
}

/// Whether a path the cache is holding (`held`) has taken `path`: it or one above it. No ancestor of
/// a path is looked up where nothing is held, the usual shape.
fn taken_by_held(held: &BTreeSet<String>, path: &str) -> bool {
	!held.is_empty() && is_under_held_path(path, held)
}

/// Whether a path the cache is holding (`held`), or a move of the join, has taken `path`.
fn taken(held: &BTreeSet<String>, conn: &Connection, path: &str) -> bool {
	if taken_by_held(held, path) {
		return true;
	}
	conn.prepare_cached(IS_CONSUMED)
		.and_then(|mut taken| taken.query_row([path], |row| row.get(0)))
		.unwrap_or_else(|error| failed_read(conn, "whether a path is taken", &error))
}

/// A read of what the windows wrote failed while the join decided (see the module docs).
fn failed_read(conn: &Connection, what: &str, error: &rusqlite::Error) -> ! {
	panic!(
		"reading the pass's windowed reconcile in {} ({what}) failed: {error} — refusing to read a \
		 failed read as no answer",
		conn.path().unwrap_or("its stage")
	)
}

/// A plan written as the windows write one, for the tests that screen it.
#[cfg(test)]
pub(super) mod test_support {
	use super::{
		super::{super::plan::SyncAction, BatchWriter, Stage},
		Out, SCHEMA, WINDOW_SEQ, Windows,
	};

	/// `actions` written into `stage` as a whole pass's windows write their plan — each a window's,
	/// in the order given — for a test to finish and screen ([`Windows::screen`]).
	pub(in crate::sync_engine) fn windows_of(stage: Stage, actions: &[SyncAction]) -> Windows {
		stage
			.conn
			.execute_batch(SCHEMA)
			.expect("the reconcile's tables");
		let mut out = Out {
			batch: BatchWriter::new(stage),
			next: WINDOW_SEQ,
		};
		for action in actions {
			out.action(action).expect("writing a window's action");
		}
		Windows {
			stage: out.batch.finish().expect("committing the windows"),
			deferred: 0,
			consumed: false,
		}
	}
}

#[cfg(test)]
mod tests {
	use filen_types::{crypto::Blake3Hash, fs::StableUuid};

	use super::{
		super::{
			super::{baseline::BaselineState, plan::PassPaths, side::Side},
			side::PAGE,
			staged_sides,
		},
		*,
	};
	use crate::alloc_meter;

	/// A row's file found at two paths by its lineage — the remote holding two versions of one file,
	/// each at its own path — moves to the last of them in path order, which is the one the resident
	/// reconcile finds walking the staged remote side in path order; the other downloads as a new
	/// file.
	#[test]
	fn a_lineage_held_at_two_paths_moves_to_the_last_of_them() {
		let lineage = StableUuid::new_for_test(Uuid::from_u128(1));
		let (ours, theirs) = (Blake3Hash::from([1; 32]), Blake3Hash::from([2; 32]));
		let row = BaselineEntry {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			remote_uuid: Some(Uuid::from_u128(10)),
			content_hash: Some(ours),
			size: Some(1),
			local_mtime: Some(1),
			remote_modified: Some(1),
			state: BaselineState::Synced,
			local_kind: None,
			remote_kind: None,
			remote_hash: None,
			remote_size: None,
			remote_stable_uuid: Some(lineage),
			agreed_hash: Some(ours),
		};
		let version = |rel_path: &str, uuid: u128| RemoteNode {
			rel_path: rel_path.to_string(),
			kind: NodeKind::File,
			remote_uuid: Uuid::from_u128(uuid),
			stable_uuid: Some(lineage),
			content_hash: Some(theirs),
			size: 2,
			modified_millis: 2,
		};
		let local = LocalNode {
			rel_path: "a.txt".to_string(),
			kind: NodeKind::File,
			size: 1,
			mtime_millis: 1,
			content_hash: Some(ours),
		};
		let (_stages, local, remote) =
			staged_sides([local], [version("c.txt", 12), version("b.txt", 11)], 1);
		let (Side::Staged(staged_local), Side::Staged(staged_remote)) = (&local, &remote) else {
			unreachable!("staged_sides stages both sides")
		};
		let baseline = Baseline::from_rows([row]);
		let holds = PassHolds::default();
		let (_dir, stage) = Stage::in_temp_dir();
		let (plan, _stage) = reconcile_windows(
			SyncMode::RemoteToLocal,
			&baseline,
			staged_local,
			staged_remote,
			&holds,
			stage,
			NonZeroUsize::MIN,
		)
		.and_then(|windows| windows.into_plan(&holds))
		.unwrap();
		assert_eq!(
			plan.actions,
			vec![
				SyncAction::MoveLocal {
					from_path: "a.txt".to_string(),
					to_path: "c.txt".to_string(),
					kind: NodeKind::File,
				},
				SyncAction::DownloadFile {
					rel_path: "b.txt".to_string(),
					remote_uuid: Uuid::from_u128(11),
				},
				SyncAction::DownloadFile {
					rel_path: "c.txt".to_string(),
					remote_uuid: Uuid::from_u128(12),
				},
			]
		);
		assert_eq!(
			plan.actions,
			plan::reconcile(
				SyncMode::RemoteToLocal,
				&baseline,
				&local.of(&baseline),
				&remote.of(&baseline),
				&holds,
				PassPaths::Whole,
			)
			.actions,
			"the plan the resident reconcile makes of the same staged sides"
		);
	}

	/// A first sync of a hundred thousand new files in a thousand new directories, planned the way a
	/// whole pass plans it: what the planning holds is a window of its inputs, not the tree's paths
	/// or its plan.
	#[test]
	fn a_first_sync_is_planned_holding_a_window_not_the_tree() {
		const FILES: usize = 100_000;
		const DIRS: usize = 1_000;
		let dirs = (0..DIRS).map(|n| LocalNode {
			rel_path: format!("d{n:03}"),
			kind: NodeKind::Dir,
			size: 0,
			mtime_millis: 1_700_000_000_000,
			content_hash: None,
		});
		let files = (0..FILES).map(|n| {
			let mut hash = [0; 32];
			hash[..8].copy_from_slice(&u64::try_from(n).unwrap().to_le_bytes());
			LocalNode {
				rel_path: format!("d{:03}/f{n:06}.txt", n % DIRS),
				kind: NodeKind::File,
				size: 1,
				mtime_millis: 1_700_000_000_000,
				content_hash: Some(Blake3Hash::from(hash)),
			}
		});
		let (_stages, local, remote) =
			staged_sides(dirs.chain(files), std::iter::empty(), PAGE.get());
		let (Side::Staged(staged_local), Side::Staged(staged_remote)) = (&local, &remote) else {
			unreachable!("staged_sides stages both sides")
		};
		let baseline = Baseline::default();
		let holds = PassHolds::default();
		let (_dir, stage) = Stage::in_temp_dir();
		let (windows, peak) = alloc_meter::peak_bytes(|| {
			reconcile_windows(
				SyncMode::TwoWay,
				&baseline,
				staged_local,
				staged_remote,
				&holds,
				stage,
				WINDOW,
			)
			.unwrap()
		});
		let (plan, _stage) = windows.into_plan(&holds).unwrap();
		assert_eq!(plan.actions.len(), FILES + DIRS);
		// The resident reconcile over the same staged sides held 23.8 MiB: a copy of every path and
		// the whole plan.
		assert!(
			peak < 16 << 20,
			"planning {FILES} new files window by window held {peak} bytes at once"
		);
		assert_eq!(
			plan.actions,
			plan::reconcile(
				SyncMode::TwoWay,
				&baseline,
				&local.of(&baseline),
				&remote.of(&baseline),
				&holds,
				PassPaths::Whole,
			)
			.actions,
			"the plan the resident reconcile makes of the same sides"
		);
	}
}
