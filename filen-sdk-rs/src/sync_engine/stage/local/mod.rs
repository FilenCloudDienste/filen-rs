//! A whole pass's local scan, staged: a row per entry the walk found, in the pass's stage, where a
//! map held a node per entry in memory from the walk to the end of the plan. The walk writes it
//! ([`StageSink`]) and the pass reads it as its local side (a [`StagedSide`] over [`LOCAL`]).
//!
//! # The table
//!
//! `local` holds a row per entry: its root-relative path, its kind — [`NodeKind`]'s code, as the
//! plan table writes it, or [`CLAIM`] for an entry that took its folded name and then could not
//! become a node (see `scan::Found::Claim`) — and its size, mtime, content hash and the
//! nanoseconds its mtime lies past the millisecond (`LocalNode::mtime_nanos`). A rowid table,
//! so a directory move rewrites its rows' paths in the indexes rather than moving the rows.
//!
//! Two unique indexes: the path's own, which every read seeks, and the folded path's,
//! `fold(path)`, where `fold` is [`collision_key`] registered on the stage's connection as a
//! deterministic SQL function. The folded one is the walk's collision check. An entry whose path
//! folds to a name a row already holds is not written, and that row is the twin the walk reports:
//! what an in-memory set of digests and a list of claims did before, with no copy of any path kept
//! in memory to do it. An identical path folds identically, so it is refused the same way. The
//! claims go once the walk is done, so the side reads nodes and nothing else.
//!
//! And a partial index of the directories' paths, which the directory-move fold walks for its
//! candidates without reading a file (see `plan::fold_dir_moves`).

use rusqlite::{Connection, Row, Statement, functions::FunctionFlags, params, types::Type};

use super::{
	super::{
		baseline::NodeKind,
		scan::{Collision, Found, LocalNode, ScanSink, collision_key},
		side::Side,
	},
	BatchWriter, Stage, hash_at, hash_bytes,
	side::{Paging, Sql, StagedSide, Table},
};

const SCHEMA: &str = "
CREATE TABLE local (
	path TEXT NOT NULL UNIQUE,
	kind INTEGER NOT NULL,
	size INTEGER,
	mtime INTEGER,
	hash BLOB,
	nanos INTEGER
);
CREATE UNIQUE INDEX local_fold ON local (fold(path));
CREATE INDEX local_dirs ON local (path) WHERE kind = 1;
";

/// `local.kind` of a claim, which no [`NodeKind`] has. [`DELETE_CLAIMS`] spells it out.
const CLAIM: i64 = 0;
const _: () =
	assert!(CLAIM == 0 && NodeKind::Dir.as_i64() != CLAIM && NodeKind::File.as_i64() != CLAIM);
/// The `kind = 1` both staged tables' index of directories and the statements that read it spell
/// out (see `Sql::dirs_from`).
const _: () = assert!(NodeKind::Dir.as_i64() == 1);

/// The path holding `?1`'s folded name, on the folded index.
const TWIN: &str = "SELECT path FROM local WHERE fold(path) = fold(?1)";
const DELETE_CLAIMS: &str = "DELETE FROM local WHERE kind = 0";

/// The statements of [`LOCAL`].
const LOCAL_SQL: Sql = Sql {
	page_from: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE path >= ?1 ORDER BY path LIMIT ?2",
	page_after: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE path > ?1 ORDER BY path LIMIT ?2",
	page_from_below: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE path >= ?1 AND path < ?2 ORDER BY path LIMIT ?3",
	page_after_below: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE path > ?1 AND path < ?2 ORDER BY path LIMIT ?3",
	dirs_from: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE kind = 1 AND path >= ?1 ORDER BY path LIMIT ?2",
	dirs_after: "SELECT path, kind, size, mtime, hash, nanos FROM local
		 WHERE kind = 1 AND path > ?1 ORDER BY path LIMIT ?2",
	insert:
		"INSERT INTO local (path, kind, size, mtime, hash, nanos) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
		 ON CONFLICT DO NOTHING",
	update: "UPDATE local SET kind = ?2, size = ?3, mtime = ?4, hash = ?5, nanos = ?6 WHERE path = ?1",
	remove: "DELETE FROM local WHERE path = ?1 RETURNING path, kind, size, mtime, hash, nanos",
	forget: "DELETE FROM local WHERE path = ?1",
	forget_under: "DELETE FROM local WHERE path >= ?1 || '/' AND path < ?1 || '0'",
	occupied: "SELECT EXISTS (SELECT 1 FROM local WHERE fold(path) = ?1)
		 OR EXISTS (SELECT 1 FROM local WHERE fold(path) >= ?1 || '/' AND fold(path) < ?1 || '0')",
	rekey_at: "UPDATE local SET path = ?2 WHERE path = ?1",
	rekey_under: "UPDATE local SET path = ?2 || substr(path, length(?1) + 1)
		 WHERE rowid IN (SELECT rowid FROM local WHERE path >= ?1 || '/' AND path < ?1 || '0'
		 LIMIT ?3)",
};

/// The local walk's table (see the module docs). Its insert refuses a row whose path or folded name
/// a row already holds.
pub(super) const LOCAL: Table<LocalNode> = Table {
	sql: &LOCAL_SQL,
	node: node_of,
	write: write_node,
	path: |node| &node.rel_path,
};

/// Register `fold` on `conn`: [`collision_key`], which a folded index is built on and every query
/// of one asks. Deterministic, as an index expression has to be.
pub(super) fn register_fold(conn: &Connection) -> rusqlite::Result<()> {
	conn.create_scalar_function(
		"fold",
		1,
		FunctionFlags::SQLITE_UTF8
			| FunctionFlags::SQLITE_DETERMINISTIC
			| FunctionFlags::SQLITE_INNOCUOUS,
		|ctx| {
			let path = ctx
				.get_raw(0)
				.as_str()
				.map_err(|error| rusqlite::Error::UserFunctionError(error.into()))?;
			Ok(collision_key(path))
		},
	)
}

/// The node a page row holds.
fn node_of(row: &Row<'_>) -> rusqlite::Result<LocalNode> {
	Ok(LocalNode {
		rel_path: row.get(0)?,
		kind: kind_at(row, 1)?,
		size: row.get(2)?,
		mtime_millis: row.get(3)?,
		mtime_nanos: row.get(5)?,
		content_hash: hash_at(row, 4)?,
	})
}

/// The [`NodeKind`] in `row`'s `column`, as a staged table writes it.
pub(super) fn kind_at(row: &Row<'_>, column: usize) -> rusqlite::Result<NodeKind> {
	let kind: i64 = row.get(column)?;
	NodeKind::from_i64(kind).ok_or_else(|| {
		rusqlite::Error::FromSqlConversionFailure(
			column,
			Type::Integer,
			format!("invalid kind in a staged side: {kind}").into(),
		)
	})
}

/// [`Table::write`] for the local table.
fn write_node(
	statement: &mut Statement<'_>,
	path: &str,
	node: &LocalNode,
) -> rusqlite::Result<usize> {
	statement.execute(params![
		path,
		node.kind.as_i64(),
		node.size,
		node.mtime_millis,
		hash_bytes(node.content_hash.as_ref()),
		node.mtime_nanos,
	])
}

/// The [`ScanSink`] a whole pass's walk writes into: a row per entry into the pass's stage, a batch
/// at a time (see the module docs).
pub(in super::super) struct StageSink {
	batch: BatchWriter,
	/// How many nodes were written.
	nodes: usize,
	/// Whether any claim was.
	claims: bool,
	/// What the side the walk leaves is read with.
	paging: Paging,
}

impl StageSink {
	/// A sink into `stage`, with its `local` table and the `fold` that table's index asks, whose
	/// side is read as `paging` says.
	pub(in super::super) fn new(stage: Stage, paging: Paging) -> rusqlite::Result<Self> {
		register_fold(&stage.conn)?;
		stage.conn.execute_batch(SCHEMA)?;
		Ok(Self {
			batch: BatchWriter::new(stage),
			nodes: 0,
			claims: false,
			paging,
		})
	}
}

impl ScanSink for StageSink {
	type Error = rusqlite::Error;

	fn put(&mut self, found: Found) -> rusqlite::Result<Option<Collision>> {
		let mut taken = None;
		self.batch.row(|conn| {
			let mut insert = conn.prepare_cached(LOCAL_SQL.insert)?;
			let written = match &found {
				Found::Node(node) => write_node(&mut insert, &node.rel_path, node)?,
				Found::Claim(rel_path) => insert.execute(params![
					rel_path,
					CLAIM,
					None::<u64>,
					None::<i64>,
					None::<&[u8]>,
					None::<i32>
				])?,
			};
			if written == 0 {
				taken = Some(
					conn.prepare_cached(TWIN)?
						.query_row([found.rel_path()], |row| row.get(0))?,
				);
			}
			Ok(())
		})?;
		if let Some(taken) = taken {
			return Ok(Some(Collision {
				taken,
				refused: found.into_rel_path(),
			}));
		}
		match found {
			Found::Node(_) => self.nodes += 1,
			Found::Claim(_) => self.claims = true,
		}
		Ok(None)
	}

	fn finish(self) -> rusqlite::Result<Side<LocalNode>> {
		let stage = self.batch.finish()?;
		// A claim has done its work once the walk is done: what the pass reads is nodes.
		if self.claims {
			stage.conn.execute(DELETE_CLAIMS, [])?;
		}
		Ok(Side::Staged(StagedSide::new(
			stage,
			self.nodes,
			LOCAL,
			self.paging,
		)))
	}
}

#[cfg(test)]
mod tests;
