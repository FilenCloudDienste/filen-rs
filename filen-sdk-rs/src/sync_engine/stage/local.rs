//! A whole pass's local scan, staged: a row per entry the walk found, in the pass's stage, where a
//! map held a node per entry in memory from the walk to the end of the plan. The walk writes it
//! ([`StageSink`]) and the pass reads it as its local side ([`StagedSide`]).
//!
//! # The table
//!
//! `local` holds a row per entry: its root-relative path, its kind — [`NodeKind`]'s code, as the
//! plan table writes it, or [`CLAIM`] for an entry that took its folded name and then could not
//! become a node (see `scan::Found::Claim`) — and its size, mtime and content hash. A rowid table,
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
//! # Reads
//!
//! Every read takes the connection for one statement and lets it go before it hands anything out,
//! as the baseline's own reads do (see `rows::snapshot`), so a caller asking the side something
//! while it is handed a page of it never waits on itself. A walk of the side reads [`PAGE`] rows a
//! statement. A question about one path is answered out of the page the last such question read
//! when that page spans the path, and that page doubles, up to [`PAGE`], while the questions walk
//! it in path order: the reconcile asks about every path it decides, one after the next, and pays a
//! statement a page rather than one a path.
//!
//! # A failed read or edit
//!
//! A read that fails panics, as a baseline read does: "nothing here" is an answer the pass acts on,
//! and a failed read must not become it. An edit that fails panics too: the table is the pass's only
//! record of the local tree, and one that no longer matches the pass leaves it nothing to plan from.
//! Both can happen only while the pass plans, before it has applied anything. A walk that cannot
//! write its rows is an error instead ([`StageSink`]), which fails the pass before it plans.

use std::{
	fmt,
	num::NonZeroUsize,
	ops::Bound,
	sync::{Mutex, PoisonError},
	vec,
};

use rusqlite::{Connection, OptionalExtension, Row, functions::FunctionFlags, params, types::Type};

use super::{
	super::{
		baseline::NodeKind,
		plan::subtree_bounds,
		scan::{Collision, Found, LocalNode, ScanSink, collision_key},
		side::Side,
	},
	BatchWriter, Stage, hash_at, hash_bytes,
};

const SCHEMA: &str = "
CREATE TABLE local (
	path TEXT NOT NULL UNIQUE,
	kind INTEGER NOT NULL,
	size INTEGER,
	mtime INTEGER,
	hash BLOB
);
CREATE UNIQUE INDEX local_fold ON local (fold(path));
";

/// `local.kind` of a claim, which no [`NodeKind`] has. [`DELETE_CLAIMS`] spells it out.
const CLAIM: i64 = 0;
const _: () =
	assert!(CLAIM == 0 && NodeKind::Dir.as_i64() != CLAIM && NodeKind::File.as_i64() != CLAIM);

/// How many rows a walk of the side reads a statement, and the most a question's read-ahead does.
const PAGE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 is not zero");

/// How many rows each statement that reads a staged side takes at most: a pass's
/// ([`PASS`](Self::PASS)), or a handful, for a test that walks a few nodes across several
/// statements. None is zero, which would leave a walk with nothing to read.
#[derive(Debug, Clone, Copy)]
pub(in super::super) struct Paging {
	/// How many rows a walk of a side reads a statement, and the most a question's read-ahead does.
	pub(super) page: NonZeroUsize,
}

impl Paging {
	/// What a pass stages and reads its sides with.
	pub(in super::super) const PASS: Self = Self { page: PAGE };
}

/// A row, unless a row already holds its path or its folded name.
const INSERT: &str = "INSERT INTO local (path, kind, size, mtime, hash) VALUES (?1, ?2, ?3, ?4, ?5)
	 ON CONFLICT DO NOTHING";
const UPDATE: &str = "UPDATE local SET kind = ?2, size = ?3, mtime = ?4, hash = ?5 WHERE path = ?1";
/// The path holding `?1`'s folded name, on the folded index.
const TWIN: &str = "SELECT path FROM local WHERE fold(path) = fold(?1)";
const DELETE_CLAIMS: &str = "DELETE FROM local WHERE kind = 0";
const REMOVE: &str = "DELETE FROM local WHERE path = ?1 RETURNING path, kind, size, mtime, hash";
const FORGET: &str = "DELETE FROM local WHERE path = ?1";
/// Whether a row's folded path is `?1` or lies under it, one `EXISTS` per seek of the folded index:
/// an `OR` of a point and a range would not seek.
const OCCUPIED: &str = "SELECT EXISTS (SELECT 1 FROM local WHERE fold(path) = ?1)
	 OR EXISTS (SELECT 1 FROM local WHERE fold(path) >= ?1 || '/' AND fold(path) < ?1 || '0')";
/// A directory move: the directory's own row, then every row under it, each statement on one seek.
/// `length` and `substr` count characters on text, so the rest of a path is cut where its prefix ends.
const REKEY_AT: &str = "UPDATE local SET path = ?2 WHERE path = ?1";
const REKEY_UNDER: &str = "UPDATE local SET path = ?2 || substr(path, length(?1) + 1)
	 WHERE path >= ?1 || '/' AND path < ?1 || '0'";

/// A page in path order: from a path on (`>=`) or after one (`>`), below a bound or not.
const PAGE_FROM: &str = "SELECT path, kind, size, mtime, hash FROM local
	 WHERE path >= ?1 ORDER BY path LIMIT ?2";
const PAGE_AFTER: &str = "SELECT path, kind, size, mtime, hash FROM local
	 WHERE path > ?1 ORDER BY path LIMIT ?2";
const PAGE_FROM_BELOW: &str = "SELECT path, kind, size, mtime, hash FROM local
	 WHERE path >= ?1 AND path < ?2 ORDER BY path LIMIT ?3";
const PAGE_AFTER_BELOW: &str = "SELECT path, kind, size, mtime, hash FROM local
	 WHERE path > ?1 AND path < ?2 ORDER BY path LIMIT ?3";

/// Register `fold` on `conn`: [`collision_key`], which the folded index is built on and every query
/// of it asks. Deterministic, as an index expression has to be.
fn register_fold(conn: &Connection) -> rusqlite::Result<()> {
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

/// Up to `limit` nodes from `from` on, below `below` when there is one, in path order.
fn read_page(
	conn: &Connection,
	from: Bound<&str>,
	below: Option<&str>,
	limit: usize,
) -> rusqlite::Result<Vec<LocalNode>> {
	let (inclusive, from) = match from {
		Bound::Included(path) => (true, path),
		Bound::Excluded(path) => (false, path),
		// Every path is at or after the empty one.
		Bound::Unbounded => (true, ""),
	};
	let sql = match (inclusive, below) {
		(true, None) => PAGE_FROM,
		(false, None) => PAGE_AFTER,
		(true, Some(_)) => PAGE_FROM_BELOW,
		(false, Some(_)) => PAGE_AFTER_BELOW,
	};
	let mut statement = conn.prepare_cached(sql)?;
	let rows = match below {
		Some(below) => statement.query(params![from, below, limit])?,
		None => statement.query(params![from, limit])?,
	};
	rows.mapped(node_of).collect()
}

/// The node a page row holds.
fn node_of(row: &Row<'_>) -> rusqlite::Result<LocalNode> {
	let kind: i64 = row.get(1)?;
	Ok(LocalNode {
		rel_path: row.get(0)?,
		kind: NodeKind::from_i64(kind).ok_or_else(|| {
			rusqlite::Error::FromSqlConversionFailure(
				1,
				Type::Integer,
				format!("invalid kind in the staged scan: {kind}").into(),
			)
		})?,
		size: row.get(2)?,
		mtime_millis: row.get(3)?,
		content_hash: hash_at(row, 4)?,
	})
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
			let mut insert = conn.prepare_cached(INSERT)?;
			let written = match &found {
				Found::Node(node) => insert.execute(params![
					node.rel_path,
					node.kind.as_i64(),
					node.size,
					node.mtime_millis,
					hash_bytes(node.content_hash.as_ref()),
				])?,
				Found::Claim(rel_path) => insert.execute(params![
					rel_path,
					CLAIM,
					None::<u64>,
					None::<i64>,
					None::<&[u8]>
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
		Ok(Side::Staged(StagedSide::local(
			stage,
			self.nodes,
			self.paging,
		)))
	}
}

/// That a staged side's `T` is [`LocalNode`], the one node a side stages: how the generic [`Side`]
/// hands out and takes in what the table holds. Made only by [`StagedSide::local`], where both
/// directions are the identity.
struct LocalIs<T> {
	from: fn(LocalNode) -> T,
	to: fn(&T) -> &LocalNode,
}

/// [`LocalIs::to`] for a `LocalNode` itself.
fn same(node: &LocalNode) -> &LocalNode {
	node
}

/// The nodes a whole pass's walk staged, read out of the stage (see the module docs): what a
/// [`Side::Staged`] holds.
///
/// A node's path is the row's: what it was filed under, and what a directory move re-keys it to.
pub(crate) struct StagedSide<T> {
	stage: Mutex<Stage>,
	/// How many nodes the table holds.
	len: usize,
	/// The page the questions about one path read last (see the module docs).
	ahead: Mutex<Ahead>,
	/// [`Paging::page`]: how many rows a walk reads a statement.
	page: NonZeroUsize,
	is: LocalIs<T>,
}

impl<T> fmt::Debug for StagedSide<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("StagedSide")
			.field("len", &self.len)
			.finish_non_exhaustive()
	}
}

impl StagedSide<LocalNode> {
	/// The `len` nodes `stage` holds, read as `paging` says.
	fn local(stage: Stage, len: usize, paging: Paging) -> Self {
		Self {
			stage: Mutex::new(stage),
			len,
			ahead: Mutex::new(Ahead::default()),
			page: paging.page,
			is: LocalIs {
				from: std::convert::identity,
				to: same,
			},
		}
	}
}

impl<T> StagedSide<T> {
	/// How many nodes it holds.
	pub(in super::super) fn len(&self) -> usize {
		self.len
	}

	/// The node at `path`.
	pub(in super::super) fn at(&self, path: &str) -> Option<T> {
		self.point(path, |node| node.cloned()).map(self.is.from)
	}

	/// Whether it holds a node at `path`.
	pub(in super::super) fn holds(&self, path: &str) -> bool {
		self.point(path, |node| node.is_some())
	}

	/// Whether it holds `path` under any spelling, or anything under it (see
	/// [`NodesAt::occupied`](super::super::side::NodesAt::occupied)).
	pub(in super::super) fn occupied(&self, path: &str) -> bool {
		let folded = collision_key(path);
		self.read("whether a folded path is occupied", |conn| {
			conn.prepare_cached(OCCUPIED)?
				.query_row([folded], |row| row.get(0))
		})
	}

	/// Every node with its path, in path order.
	pub(in super::super) fn pages(&self) -> Pages<'_, T> {
		Pages {
			side: self,
			cursor: Cursor::from(Bound::Unbounded, None),
		}
	}

	/// Every node strictly under `dir`, with its path, in path order.
	pub(in super::super) fn pages_under(&self, dir: &str) -> Pages<'_, T> {
		let bounds = subtree_bounds(dir);
		Pages {
			side: self,
			cursor: Cursor::from(Bound::Included(bounds.start), Some(bounds.end)),
		}
	}

	/// [`pages`](Self::pages), handing the nodes over.
	pub(in super::super) fn into_pages(self) -> IntoPages<T> {
		IntoPages {
			side: self,
			cursor: Cursor::from(Bound::Unbounded, None),
		}
	}

	/// Every path strictly under `dir`.
	pub(in super::super) fn subtree_paths(&self, dir: &str) -> Vec<String> {
		self.pages_under(dir).map(|(path, _)| path).collect()
	}

	/// File `node` at `path`, over whatever was there.
	///
	/// A staged side holds one spelling of each folded name, as the walk that made it does, so a
	/// node at another spelling of a name it holds is refused, and that panics as a failed edit
	/// does (see the module docs): no pass puts one there, as no pass adds a node to a whole side.
	pub(in super::super) fn insert(&mut self, path: &str, node: &T) {
		let node = (self.is.to)(node);
		let added = self.edit("a node put", |conn| {
			let row = params![
				path,
				node.kind.as_i64(),
				node.size,
				node.mtime_millis,
				hash_bytes(node.content_hash.as_ref()),
			];
			if conn.prepare_cached(INSERT)?.execute(row)? == 1 {
				return Ok(Some(true));
			}
			Ok((conn.prepare_cached(UPDATE)?.execute(row)? == 1).then_some(false))
		});
		match added {
			Some(added) => self.len += usize::from(added),
			None => panic!(
				"the staged scan holds another spelling of {path:?}, and a staged side holds one \
				 spelling of each folded name"
			),
		}
	}

	/// Take the node at `path` out, returning it.
	pub(in super::super) fn remove(&mut self, path: &str) -> Option<T> {
		let removed = self.edit("a node taken out", |conn| {
			conn.prepare_cached(REMOVE)?
				.query_row([path], node_of)
				.optional()
		});
		self.len -= usize::from(removed.is_some());
		removed.map(self.is.from)
	}

	/// Take the node at `path` out, saying whether there was one.
	pub(in super::super) fn forget(&mut self, path: &str) -> bool {
		let forgot = self.edit("a node dropped", |conn| {
			conn.prepare_cached(FORGET)?.execute([path])
		}) == 1;
		self.len -= usize::from(forgot);
		forgot
	}

	/// Keep the nodes `keep` answers `true` for: a walk of every node, and an edit per node dropped.
	pub(in super::super) fn retain(&mut self, mut keep: impl FnMut(&str, &T) -> bool) {
		let dropping: Vec<String> = self
			.pages()
			.filter(|(path, node)| !keep(path, node))
			.map(|(path, _)| path)
			.collect();
		for path in dropping {
			self.forget(&path);
		}
	}

	/// Move the node at `from` and everything under it onto `to`. The destination must be free
	/// under every spelling, as a directory move's is, or the edit fails (see the module docs).
	pub(in super::super) fn rekey(&mut self, from: &str, to: &str) {
		self.edit("a directory move", |conn| {
			conn.prepare_cached(REKEY_AT)?.execute([from, to])?;
			conn.prepare_cached(REKEY_UNDER)?.execute([from, to])
		});
	}

	/// The node at exactly `path`, handed to `answer`: out of the page the last question read when
	/// it spans the path, else out of a new one read from the path (see the module docs).
	fn point<R>(&self, path: &str, answer: impl Fn(Option<&LocalNode>) -> R) -> R {
		let mut ahead = self.ahead.lock().unwrap_or_else(PoisonError::into_inner);
		if let Some(Span::Inside(found)) = ahead.page.as_ref().map(|page| page.answer(path)) {
			let found = answer(found);
			ahead.answered += 1;
			return found;
		}
		let want = ahead.next_want(path, self.page.get());
		let nodes = self.read("a path", |conn| {
			read_page(conn, Bound::Included(path), None, want)
		});
		let page = Page {
			from: path.to_owned(),
			complete: nodes.len() < want,
			nodes,
		};
		let Span::Inside(found) = page.answer(path) else {
			unreachable!("a page read from a path spans that path")
		};
		let found = answer(found);
		*ahead = Ahead {
			page: Some(page),
			answered: 1,
			want,
		};
		found
	}

	/// What `read` answers on the connection, which a failed read panics instead of answering (see
	/// the module docs).
	fn read<R>(&self, what: &str, read: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> R {
		let stage = self.stage.lock().unwrap_or_else(PoisonError::into_inner);
		read(&stage.conn).unwrap_or_else(|error| {
			panic!(
				"reading the pass's staged scan in {} ({what}) failed: {error} — refusing to read a \
				 failed read as an absent node",
				stage.path().display()
			)
		})
	}

	/// What `edit` answers on the connection, which a failed edit panics instead of answering (see
	/// the module docs). The read-ahead goes with it: it no longer says what the table holds.
	fn edit<R>(&mut self, what: &str, edit: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> R {
		*self.ahead.get_mut().unwrap_or_else(PoisonError::into_inner) = Ahead::default();
		let stage = self.stage.get_mut().unwrap_or_else(PoisonError::into_inner);
		edit(&stage.conn).unwrap_or_else(|error| {
			panic!(
				"editing the pass's staged scan in {} ({what}) failed: {error} — it is the pass's \
				 only record of the local tree",
				stage.path().display()
			)
		})
	}
}

/// Two staged sides are equal when they hold the same nodes at the same paths, as two maps are: a
/// walk of each, in path order.
#[cfg(any(test, feature = "bench-internals"))]
impl<T: PartialEq> PartialEq for StagedSide<T> {
	fn eq(&self, other: &Self) -> bool {
		self.len == other.len && self.pages().eq(other.pages())
	}
}

#[cfg(any(test, feature = "bench-internals"))]
impl<T: Eq> Eq for StagedSide<T> {}

/// The page the questions about one path read last, and how they used it (see
/// [`StagedSide::point`]).
#[derive(Default)]
struct Ahead {
	page: Option<Page>,
	/// How many questions `page` answered.
	answered: usize,
	/// How many nodes `page` was read for.
	want: usize,
}

impl Ahead {
	/// How many nodes the page read for a question at `path` asks for: twice the last one's, up to
	/// `most`, when that one answered as many questions as it holds nodes and `path` lies past it — a
	/// walk in path order — and one otherwise, which is what a lookup of the one path reads.
	fn next_want(&self, path: &str, most: usize) -> usize {
		match &self.page {
			Some(page)
				if self.answered >= page.nodes.len()
					&& page
						.nodes
						.last()
						.is_some_and(|last| last.rel_path.as_str() < path) =>
			{
				(self.want * 2).min(most)
			}
			_ => 1,
		}
	}
}

/// Nodes read from a path on, in path order.
struct Page {
	from: String,
	nodes: Vec<LocalNode>,
	/// Whether it ran to the end of the table: fewer nodes came back than were asked for.
	complete: bool,
}

impl Page {
	/// What the table holds at `path`, where this page can say: `path` is at or after where it was
	/// read from, and not past its last node unless it ran to the end of the table.
	fn answer(&self, path: &str) -> Span<'_> {
		let past = self
			.nodes
			.last()
			.is_none_or(|last| path > last.rel_path.as_str());
		if path < self.from.as_str() || (past && !self.complete) {
			return Span::Outside;
		}
		Span::Inside(
			self.nodes
				.binary_search_by(|node| node.rel_path.as_str().cmp(path))
				.ok()
				.and_then(|at| self.nodes.get(at)),
		)
	}
}

/// What a [`Page`] says about one path.
enum Span<'p> {
	/// The page spans the path, and holds this node there, or none.
	Inside(Option<&'p LocalNode>),
	/// The page does not reach the path.
	Outside,
}

/// Where a walk of a staged side is: the page in hand, and where the next one starts.
struct Cursor {
	next: Bound<String>,
	below: Option<String>,
	page: vec::IntoIter<LocalNode>,
	/// Whether the last page read ran to the end of what the walk covers.
	done: bool,
}

impl Cursor {
	fn from(next: Bound<String>, below: Option<String>) -> Self {
		Self {
			next,
			below,
			page: Vec::new().into_iter(),
			done: false,
		}
	}

	fn next<T>(&mut self, side: &StagedSide<T>) -> Option<(String, T)> {
		if self.page.len() == 0 && !self.done {
			let nodes = side.read("a page", |conn| {
				read_page(
					conn,
					self.next.as_ref().map(String::as_str),
					self.below.as_deref(),
					side.page.get(),
				)
			});
			self.done = nodes.len() < side.page.get();
			if let Some(last) = nodes.last() {
				self.next = Bound::Excluded(last.rel_path.clone());
			}
			self.page = nodes.into_iter();
		}
		let node = self.page.next()?;
		Some((node.rel_path.clone(), (side.is.from)(node)))
	}
}

/// A staged side's nodes with their paths, a page at a time.
pub(crate) struct Pages<'s, T> {
	side: &'s StagedSide<T>,
	cursor: Cursor,
}

impl<T> Iterator for Pages<'_, T> {
	type Item = (String, T);

	fn next(&mut self) -> Option<(String, T)> {
		self.cursor.next(self.side)
	}
}

/// [`Pages`], over a side it owns.
pub(crate) struct IntoPages<T> {
	side: StagedSide<T>,
	cursor: Cursor,
}

impl<T> Iterator for IntoPages<T> {
	type Item = (String, T);

	fn next(&mut self) -> Option<(String, T)> {
		self.cursor.next(&self.side)
	}
}

#[cfg(test)]
mod tests {
	use std::{borrow::Cow, collections::HashMap};

	use filen_types::crypto::Blake3Hash;
	use rand::{Rng, SeedableRng, rngs::StdRng};

	use super::{
		super::{
			super::{
				rows::Baseline,
				scan::MapSink,
				side::{Nodes, NodesAt, test_support::sorted_entries},
			},
			paging_of,
			tests::row,
		},
		*,
	};
	use crate::alloc_meter;

	const CASES: u64 = 200;

	fn node(rel_path: &str, kind: NodeKind, version: u8) -> LocalNode {
		LocalNode {
			rel_path: rel_path.to_owned(),
			kind,
			size: match kind {
				NodeKind::File => u64::from(version) + 1,
				NodeKind::Dir => 0,
			},
			mtime_millis: 1_700_000_000_000 + i64::from(version),
			content_hash: (kind == NodeKind::File).then(|| Blake3Hash::from([version; 32])),
		}
	}

	/// A side staged in a fresh stage of its own, through the walk's sink, read as `paging` says;
	/// with the directory its stage is in.
	fn staged(
		nodes: impl IntoIterator<Item = LocalNode>,
		paging: Paging,
	) -> (tempfile::TempDir, Side<LocalNode>) {
		let (dir, stage) = Stage::in_temp_dir();
		let mut sink = StageSink::new(stage, paging).unwrap();
		for node in nodes {
			assert_eq!(sink.put(Found::Node(node)).unwrap(), None);
		}
		(dir, sink.finish().unwrap())
	}

	/// The walk's collision check, asked of both sinks: every entry put is answered alike — taken,
	/// or refused with the same twin named — and what is left is the same nodes. The paths fold
	/// onto one another every way the walk can meet: by case, by a non-ASCII case, a directory and
	/// a file, and the identical path twice (two names NFC folds together).
	#[test]
	fn the_stage_sink_takes_and_refuses_what_the_map_sink_does() {
		const PATHS: &[&str] = &[
			"a",
			"A",
			"a/b.txt",
			"A/B.TXT",
			"a/B.txt",
			"ab",
			"\u{c4}.txt",
			"\u{e4}.txt",
			"\u{c4}.TXT",
			"z",
			"z.txt",
			"Z.txt",
			"docs",
			"docs/x",
			"DOCS/x",
		];
		let (mut refused, mut claimed, mut taken_by_claim) = (0, 0, 0);
		for seed in 0..CASES {
			let mut rng = StdRng::seed_from_u64(seed);
			let (_dir, stage) = Stage::in_temp_dir();
			let (mut map, mut stage) = (
				MapSink::with_capacity(0),
				StageSink::new(stage, Paging::PASS).unwrap(),
			);
			let mut claims: Vec<String> = Vec::new();
			for step in 0..rng.random_range(1..=24u8) {
				let path = PATHS[rng.random_range(0..PATHS.len())];
				let found = match rng.random_range(0..4) {
					0 => Found::Claim(path.to_owned()),
					1 => Found::Node(node(path, NodeKind::Dir, step)),
					_ => Found::Node(node(path, NodeKind::File, step)),
				};
				let Ok(by_map) = map.put(found.clone());
				let by_stage = stage.put(found.clone()).unwrap();
				assert_eq!(by_stage, by_map, "seed {seed} step {step}: {found:?}");
				match (&found, by_map) {
					(_, Some(collision)) => {
						refused += 1;
						taken_by_claim += usize::from(claims.contains(&collision.taken));
					}
					(Found::Claim(path), None) => {
						claimed += 1;
						claims.push(path.clone());
					}
					(Found::Node(_), None) => {}
				}
			}
			let Ok(map) = map.finish();
			let stage = stage.finish().unwrap();
			assert_eq!(
				sorted_entries(&stage.whole()),
				sorted_entries(&map.whole()),
				"seed {seed}: the nodes left"
			);
			assert_eq!(Nodes::len(&stage.whole()), Nodes::len(&map.whole()));
		}
		// A corpus that stopped producing these would let the equality above pass by proving
		// nothing about the refusals.
		assert!(refused > 1000, "only {refused} refusal(s)");
		assert!(claimed > 200, "only {claimed} claim(s)");
		assert!(
			taken_by_claim > 50,
			"only {taken_by_claim} refusal(s) by a claim"
		);
	}

	fn sorted(paths: impl Iterator<Item = String>) -> Vec<String> {
		let mut out: Vec<String> = paths.collect();
		out.sort();
		out
	}

	/// The paths a staged side is held to the map over: no two fold together, as a walk leaves
	/// them. Nesting, a name that is a string prefix of a sibling's (`a/b` and `a/b.txt`) and of
	/// another directory's (`a`, `ab`), a file and a directory sharing a stem (`z`, `z.txt`), a
	/// non-ASCII name with a subtree, and a childless directory for a move to carry.
	const CORPUS: &[(&str, NodeKind)] = &[
		("a", NodeKind::Dir),
		("a/b", NodeKind::Dir),
		("a/b/c.txt", NodeKind::File),
		("a/b.txt", NodeKind::File),
		("ab", NodeKind::Dir),
		("ab/x.txt", NodeKind::File),
		("d", NodeKind::Dir),
		("d/e", NodeKind::Dir),
		("d/e/f.txt", NodeKind::File),
		("top.txt", NodeKind::File),
		("\u{c4}", NodeKind::Dir),
		("\u{c4}/y.txt", NodeKind::File),
		("z", NodeKind::Dir),
		("z/1.txt", NodeKind::File),
		("z.txt", NodeKind::File),
		("empty", NodeKind::Dir),
	];

	/// Every path the two are questioned about: the corpus in its own order and in reverse —
	/// a read-ahead walked forward and one asked out of order — spellings that fold onto it,
	/// paths nothing holds, the pair root, and where the moves below land.
	fn probe_paths() -> Vec<String> {
		let corpus = CORPUS.iter().map(|(path, _)| *path);
		corpus
			.clone()
			.chain(corpus.rev())
			.chain([
				"",
				"A",
				"A/B.TXT",
				"TOP.TXT",
				"\u{e4}/Y.TXT",
				"a/nope",
				"z.tx",
				"zz",
				"moved0",
				"moved3/1.txt",
				"MOVED5",
			])
			.map(str::to_owned)
			.collect()
	}

	/// Every read the two traits expose, asked of a map and of a staged side.
	///
	/// Iteration order is not compared: a map has its table's, and only the enumerations are
	/// sorted first.
	fn assert_answers_alike(at: &str, whole: &Side<LocalNode>, staged: &Side<LocalNode>) {
		let (w, s) = (whole.whole(), staged.whole());
		assert_eq!(
			sorted(w.paths().map(Cow::into_owned)),
			sorted(s.paths().map(Cow::into_owned)),
			"{at}: paths"
		);
		assert_eq!(Nodes::len(&w), Nodes::len(&s), "{at}: len");
		assert_eq!(Nodes::is_empty(&w), Nodes::is_empty(&s), "{at}: is_empty");
		assert_eq!(sorted_entries(&w), sorted_entries(&s), "{at}: iter");
		assert_eq!(
			sorted(whole.own_keys().map(Cow::into_owned)),
			sorted(staged.own_keys().map(Cow::into_owned)),
			"{at}: own_keys"
		);
		let rows = Baseline::default();
		for path in probe_paths() {
			assert_eq!(
				w.at(&path).map(Cow::into_owned),
				s.at(&path).map(Cow::into_owned),
				"{at}: at({path:?})"
			);
			assert_eq!(w.holds(&path), s.holds(&path), "{at}: holds({path:?})");
			assert_eq!(
				w.occupied(&path),
				s.occupied(&path),
				"{at}: occupied({path:?})"
			);
			assert_eq!(
				w.at_row(&row(&path)).map(Cow::into_owned),
				s.at_row(&row(&path)).map(Cow::into_owned),
				"{at}: at_row({path:?})"
			);
			assert_eq!(
				sorted(w.under(&path).map(|(path, _)| path.into_owned())),
				sorted(s.under(&path).map(|(path, _)| path.into_owned())),
				"{at}: under({path:?})"
			);
			assert_eq!(
				sorted(whole.subtree_paths(&rows, &path).into_iter()),
				sorted(staged.subtree_paths(&rows, &path).into_iter()),
				"{at}: subtree_paths({path:?})"
			);
		}
	}

	/// A staged side answers every read exactly as the map a walk into memory would have made,
	/// through every edit a side has, read one, two and seven rows at a time as well as a pass's
	/// pages: the expectation is not written down here, it is whatever the map answers.
	#[test]
	fn a_staged_side_answers_exactly_as_the_materialised_one() {
		let mut moves = 0usize;
		for paging in [paging_of(1), paging_of(2), paging_of(7), Paging::PASS] {
			for seed in 0..CASES {
				let mut rng = StdRng::seed_from_u64(seed);
				let nodes: Vec<LocalNode> = CORPUS
					.iter()
					.filter(|_| rng.random_range(0..3) != 0)
					.map(|(path, kind)| node(path, *kind, 0))
					.collect();
				let mut whole: Side<LocalNode> = nodes
					.iter()
					.map(|node| (node.rel_path.clone(), node.clone()))
					.collect();
				let (_dir, mut staged) = staged(nodes, paging);
				let at = |step: &str| format!("{paging:?} seed {seed} after {step}");
				assert_answers_alike(&at("the walk"), &whole, &staged);

				let rows = Baseline::default();
				for step in 0..8u8 {
					let (path, kind) = CORPUS[rng.random_range(0..CORPUS.len())];
					let what = match rng.random_range(0..7) {
						0 => {
							let put = node(path, kind, step + 1);
							whole.insert(path.to_owned(), put.clone());
							staged.insert(path.to_owned(), put);
							format!("insert({path:?})")
						}
						1 => {
							// A path the walk never found, which only a later edit can add.
							let fresh = format!("{path}/fresh{step}");
							let put = node(&fresh, NodeKind::File, step + 1);
							whole.insert(fresh.clone(), put.clone());
							staged.insert(fresh.clone(), put);
							format!("insert({fresh:?})")
						}
						2 => {
							assert_eq!(
								whole.remove(&rows, path),
								staged.remove(&rows, path),
								"{}: remove({path:?}) handed back different nodes",
								at("a remove")
							);
							format!("remove({path:?})")
						}
						3 => {
							assert_eq!(
								whole.forget(&row(path)),
								staged.forget(&row(path)),
								"{}: forget({path:?})",
								at("a forget")
							);
							format!("forget({path:?})")
						}
						4 => {
							let keep = if rng.random_range(0..2) == 0 {
								NodeKind::File
							} else {
								NodeKind::Dir
							};
							whole.retain(&rows, |_, node| node.kind == keep);
							staged.retain(&rows, |_, node| node.kind == keep);
							format!("retain({keep:?})")
						}
						5 => {
							let root = node("", NodeKind::Dir, step + 1);
							whole.insert(String::new(), root.clone());
							staged.insert(String::new(), root);
							"insert(\"\")".to_owned()
						}
						_ => {
							// A destination free under every spelling, which is the only kind a
							// directory move lands on (`next_dir_move` refuses an occupied one).
							let to = format!("moved{step}");
							assert!(!whole.whole().occupied(&to));
							let set_path =
								|node: &mut LocalNode, at: &str| node.rel_path = at.to_owned();
							whole.rekey_subtree(&rows, path, &to, set_path);
							staged.rekey_subtree(&rows, path, &to, set_path);
							moves += 1;
							format!("rekey({path:?} -> {to:?})")
						}
					};
					assert_answers_alike(&at(&format!("step {step} {what}")), &whole, &staged);
				}
				assert_eq!(
					sorted_entries(&whole.whole()),
					{
						let mut handed: Vec<(String, LocalNode)> =
							staged.into_entries(&rows).collect();
						handed.sort_by(|left, right| left.0.cmp(&right.0));
						handed
					},
					"{paging:?} seed {seed}: into_entries"
				);
			}
		}
		assert!(moves > 500, "only {moves} directory move(s)");
	}

	/// Two staged sides compare as two maps do, by the nodes they hold at their paths and not by
	/// which table holds them: two stages of the same walk read alike however they are paged, and
	/// one node put over its twin's tells them apart.
	#[test]
	fn staged_sides_compare_node_for_node() {
		let nodes = || CORPUS.iter().map(|(path, kind)| node(path, *kind, 0));
		let (_one_dir, mut one) = staged(nodes(), paging_of(2));
		let (_other_dir, other) = staged(nodes(), Paging::PASS);
		assert_eq!(one, other);
		one.insert("top.txt".to_owned(), node("top.txt", NodeKind::File, 1));
		assert_ne!(one, other);
	}

	/// What a walk into the stage and a read of all of it back hold in memory is a page, not the
	/// tree: 20,000 nodes into a map hold a few MiB.
	#[test]
	fn a_staged_walk_and_a_read_of_it_hold_a_page_of_the_tree() {
		const NODES: u16 = 20_000;
		let (_dir, stage) = Stage::in_temp_dir();
		let (read, peak) = alloc_meter::peak_bytes(|| {
			let mut sink = StageSink::new(stage, Paging::PASS).unwrap();
			for n in 0..NODES {
				let path = format!("d{:02}/f{n:05}.txt", n % 100);
				let put = sink.put(Found::Node(node(&path, NodeKind::File, 1)));
				assert_eq!(put.unwrap(), None);
			}
			sink.finish().unwrap().whole().iter().count()
		});
		assert_eq!(read, usize::from(NODES));
		assert!(
			peak < 1024 * 1024,
			"staging and reading back the walk held {peak} bytes at once"
		);
	}

	/// A claim holds its folded name while the walk runs and is gone once it is done: the side reads
	/// nodes and nothing else.
	#[test]
	fn a_claim_refuses_its_twin_and_leaves_no_node() {
		let (_dir, stage) = Stage::in_temp_dir();
		let mut sink = StageSink::new(stage, Paging::PASS).unwrap();
		assert_eq!(sink.put(Found::Claim("Notes".to_owned())).unwrap(), None);
		assert_eq!(
			sink.put(Found::Node(node("notes", NodeKind::Dir, 0)))
				.unwrap(),
			Some(Collision {
				taken: "Notes".to_owned(),
				refused: "notes".to_owned(),
			})
		);
		assert_eq!(
			sink.put(Found::Node(node("kept.txt", NodeKind::File, 0)))
				.unwrap(),
			None
		);
		let side = sink.finish().unwrap();
		assert_eq!(
			sorted_entries(&side.whole()),
			vec![("kept.txt".to_owned(), node("kept.txt", NodeKind::File, 0))]
		);
		assert!(!side.whole().occupied("NOTES"));
		assert_eq!(Nodes::len(&side.whole()), 1);
	}

	/// The oracle's corpus is what a walk leaves: no two of its paths fold together, so a staged
	/// side can hold all of it.
	#[test]
	fn the_oracle_corpus_holds_no_two_paths_that_fold_together() {
		let mut folded: HashMap<String, &str> = HashMap::new();
		for (path, _) in CORPUS {
			assert_eq!(folded.insert(collision_key(path), path), None, "{path:?}");
		}
	}
}
