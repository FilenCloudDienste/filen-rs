//! A whole pass's remote snapshot, staged: a row per item the cache lists under the pair's remote
//! root, in a stage of the pass's own, where a map held a node per item in memory from the read to
//! the end of the plan. The read copies the rows in ([`RemoteSink`]), placing them gives each its
//! path ([`RemoteSink::finish`]), and the pass reads them as its remote view's nodes (a
//! [`StagedSide`] over [`REMOTE`]).
//!
//! # Two tables
//!
//! The read hands its rows over in no order (see `SnapshotSink`), so no path can be known while it
//! runs. Each row is copied as it comes into `remote_raw` — uuid, parent, whether it is a directory,
//! the NFC name, and a file's lineage, hash, size and stamp — inside the cache's one read
//! transaction, which ends when the copy does.
//!
//! Placing walks `remote_raw` down from the root, a directory's children a page at a time off an
//! index on the parent built once the copy is done, and writes each item it places into `remote`
//! under its path. Then `remote` gets its folded index — `fold(path)`, as the local walk's table has,
//! but not unique: two remote items whose names fold together are what `RemoteView::filter` refuses
//! the pass over — and `remote_raw` goes. An index on the uuid is built by the first question about
//! a uuid, which a pass with writes of its own to settle asks, and so does the move join of a whole
//! pass's windowed reconcile for a row whose file left its path (`stage::reconcile`). That join asks
//! by a file's lineage too where its uuid finds nothing, and the first such question builds a partial
//! index on the lineage. A partial index of the directories' paths is kept as the walk writes them,
//! as the local table's is, for the directory-move fold to walk without reading a file.
//!
//! # What the walk places, and what it leaves
//!
//! The walk places exactly what `ViewBuilder` places, at exactly its paths: an item with a safe name
//! whose parent's chain to the root is a chain of placeable directories at most
//! `MAX_REMOTE_DEPTH` deep, none of them undecodable. It descends into every such directory, the
//! engine's own quarantine directory and a path the cache lists twice included, since the items
//! under those resolve all the same, and places each item as the builder does: nothing in the
//! quarantine directory, nothing at a path it holds back, and a second item at a path it already
//! placed holds that path back for the pass. It never descends into the root's own row, which a
//! cache holding the root under its own subtree lists there: every chain ends at the root, so what
//! is under that row is the root's, placed once at the top.
//!
//! What it does not place is everything `ViewBuilder` records as skipped or leaves out: an unsafe
//! name and its subtree, an item under an undecodable directory, under a file or under a chain that
//! never reaches the root, and anything deeper than the chain may run. Each row it places is marked
//! in a bit set, a bit per row, and every row left unmarked — none, on a tree with nothing wrong in
//! it — is handed to `ViewBuilder` itself with the undecodable records, which records them exactly
//! as it records them from a whole read: its index holds the directories every one of their chains
//! climbs, read off `remote_raw` by uuid, and it is asked nothing else. So the records are the
//! builder's own, kept in memory as it keeps them, and they are as many as the items with something
//! wrong in or above them.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap, HashSet},
	num::NonZeroUsize,
};

use filen_types::fs::StableUuid;
use rusqlite::{Connection, OptionalExtension, Row, Statement, params};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use super::{
	super::{
		baseline::NodeKind,
		plan::{
			MAX_REMOTE_DEPTH, RemoteNode, RemoteView, SkippedRemote, ViewBuilder, in_quarantine,
			is_safe_name, join_path,
		},
		side::Side,
	},
	BatchWriter, Stage, hash_at, hash_bytes,
	local::{kind_at, register_fold},
	side::{Paging, Sql, StagedSide, Table},
};
use crate::cache::{RemoteItem, SnapshotSink, UndecodableItem};

const SCHEMA: &str = "
CREATE TABLE remote_raw (
	uuid BLOB NOT NULL,
	parent BLOB NOT NULL,
	dir INTEGER NOT NULL,
	name TEXT NOT NULL,
	lineage BLOB,
	hash BLOB,
	size INTEGER NOT NULL,
	modified INTEGER NOT NULL
);
CREATE TABLE remote (
	path TEXT NOT NULL UNIQUE,
	kind INTEGER NOT NULL,
	uuid BLOB NOT NULL,
	lineage BLOB,
	hash BLOB,
	size INTEGER NOT NULL,
	modified INTEGER NOT NULL
);
CREATE INDEX remote_dirs ON remote (path) WHERE kind = 1;
";

const RAW_INSERT: &str =
	"INSERT INTO remote_raw (uuid, parent, dir, name, lineage, hash, size, modified)
	 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";
/// Built once the copy is done: a directory's children are read off it, by rowid within a parent.
const RAW_BY_PARENT: &str = "CREATE INDEX remote_raw_parent ON remote_raw (parent)";
/// Up to `?3` children of the directory `?1` after the rowid `?2`, in rowid order.
const CHILDREN: &str = "SELECT rowid, uuid, parent, dir, name, lineage, hash, size, modified
	 FROM remote_raw WHERE parent = ?1 AND rowid > ?2 ORDER BY rowid LIMIT ?3";
/// Every row, for the rows the walk did not place.
const EVERY_RAW: &str = "SELECT rowid, uuid, parent, dir, name, lineage, hash, size, modified
	 FROM remote_raw ORDER BY rowid";
/// Built only where something is left to replay: its chains are climbed by uuid.
const RAW_BY_UUID: &str = "CREATE INDEX remote_raw_uuid ON remote_raw (uuid)";
const RAW_DIR: &str = "SELECT name, parent FROM remote_raw WHERE uuid = ?1 AND dir = 1";
/// What the view's reads seek, then the copy goes.
const PLACED: &str = "CREATE INDEX remote_fold ON remote (fold(path));
	 DROP TABLE remote_raw;";

/// Built by the first question about a uuid: a pass whose pair has writes of its own in the journal
/// asks one (see `PendingWrites::settle`), and so does a whole pass's move join for a row whose file
/// left its path (`StagedSide::file_of_uuid`).
const BY_UUID: &str = "CREATE INDEX IF NOT EXISTS remote_uuid ON remote (uuid)";
/// Where the view holds the item `?1`. One row unless a pass has put a second node of one uuid in
/// (see `PendingWrites::fold_into`), and then the first in path order, so the answer is one answer.
const PATH_OF: &str = "SELECT path FROM remote WHERE uuid = ?1 ORDER BY path LIMIT 1";
/// Where the view holds the FILE `?1` (`kind = 2`): the last of them in path order.
const FILE_OF_UUID: &str =
	"SELECT path FROM remote WHERE uuid = ?1 AND kind = 2 ORDER BY path DESC LIMIT 1";
/// Built by the first question about a lineage, which only a whole pass's move join asks, for a row
/// whose file left its path and whose uuid finds nothing (`StagedSide::file_of_lineage`). Directories
/// carry none.
const BY_LINEAGE: &str =
	"CREATE INDEX IF NOT EXISTS remote_lineage ON remote (lineage) WHERE lineage IS NOT NULL";
/// [`FILE_OF_UUID`], by lineage.
const FILE_OF_LINEAGE: &str =
	"SELECT path FROM remote WHERE lineage = ?1 AND kind = 2 ORDER BY path DESC LIMIT 1";
// The two statements above spell a file's kind out.
const _: () = assert!(NodeKind::File.as_i64() == 2);
/// Every folded path two or more nodes have, off the folded index.
const TWIN_KEYS: &str = "SELECT fold(path) FROM remote GROUP BY fold(path) HAVING count(*) > 1";
/// Every node with the folded path `?1` but the first in path order.
const DROP_TWINS: &str = "DELETE FROM remote WHERE fold(path) = ?1
	 AND path > (SELECT min(path) FROM remote WHERE fold(path) = ?1)";
/// Every node whose path folds as the held path `?1` does.
const DROP_HELD_TWINS: &str = "DELETE FROM remote WHERE fold(path) = fold(?1)";

/// How many children of a directory the walk reads a statement.
pub(super) const CHILDREN_PAGE: NonZeroUsize = NonZeroUsize::new(256).expect("256 is not zero");

/// The statements of [`REMOTE`].
const REMOTE_SQL: Sql = Sql {
	page_from: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE path >= ?1 ORDER BY path LIMIT ?2",
	page_after: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE path > ?1 ORDER BY path LIMIT ?2",
	page_from_below: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE path >= ?1 AND path < ?2 ORDER BY path LIMIT ?3",
	page_after_below: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE path > ?1 AND path < ?2 ORDER BY path LIMIT ?3",
	dirs_from: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE kind = 1 AND path >= ?1 ORDER BY path LIMIT ?2",
	dirs_after: "SELECT path, kind, uuid, lineage, hash, size, modified FROM remote
		 WHERE kind = 1 AND path > ?1 ORDER BY path LIMIT ?2",
	insert: "INSERT INTO remote (path, kind, uuid, lineage, hash, size, modified)
		 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT DO NOTHING",
	update: "UPDATE remote SET kind = ?2, uuid = ?3, lineage = ?4, hash = ?5, size = ?6,
		 modified = ?7 WHERE path = ?1",
	remove: "DELETE FROM remote WHERE path = ?1
		 RETURNING path, kind, uuid, lineage, hash, size, modified",
	forget: "DELETE FROM remote WHERE path = ?1",
	forget_under: "DELETE FROM remote WHERE path >= ?1 || '/' AND path < ?1 || '0'",
	occupied: "SELECT EXISTS (SELECT 1 FROM remote WHERE fold(path) = ?1)
		 OR EXISTS (SELECT 1 FROM remote WHERE fold(path) >= ?1 || '/' AND fold(path) < ?1 || '0')",
	rekey_at: "UPDATE remote SET path = ?2 WHERE path = ?1",
	rekey_under: "UPDATE remote SET path = ?2 || substr(path, length(?1) + 1)
		 WHERE rowid IN (SELECT rowid FROM remote WHERE path >= ?1 || '/' AND path < ?1 || '0'
		 LIMIT ?3)",
};

/// The remote view's table (see the module docs).
pub(super) const REMOTE: Table<RemoteNode> = Table {
	sql: &REMOTE_SQL,
	node: node_of,
	write: write_node,
	path: |node| &node.rel_path,
};

/// The node a page row holds.
fn node_of(row: &Row<'_>) -> rusqlite::Result<RemoteNode> {
	Ok(RemoteNode {
		rel_path: row.get(0)?,
		kind: kind_at(row, 1)?,
		remote_uuid: row.get(2)?,
		stable_uuid: row.get(3)?,
		content_hash: hash_at(row, 4)?,
		size: row.get(5)?,
		modified_millis: row.get(6)?,
	})
}

/// [`Table::write`] for the remote table.
fn write_node(
	statement: &mut Statement<'_>,
	path: &str,
	node: &RemoteNode,
) -> rusqlite::Result<usize> {
	statement.execute(params![
		path,
		node.kind.as_i64(),
		node.remote_uuid,
		node.stable_uuid,
		hash_bytes(node.content_hash.as_ref()),
		node.size,
		node.modified_millis,
	])
}

/// One row of `remote_raw`, as [`CHILDREN`] and [`EVERY_RAW`] select it.
struct Raw {
	rowid: i64,
	dir: bool,
	item: RemoteItem,
}

fn raw_of(row: &Row<'_>) -> rusqlite::Result<Raw> {
	Ok(Raw {
		rowid: row.get(0)?,
		dir: row.get(3)?,
		item: RemoteItem {
			uuid: row.get(1)?,
			parent: row.get(2)?,
			name: row.get(4)?,
			stable_uuid: row.get(5)?,
			hash: hash_at(row, 6)?,
			size: row.get(7)?,
			modified_millis: row.get(8)?,
		},
	})
}

/// The [`SnapshotSink`] a whole pass's snapshot read copies into: a row per item into the pass's
/// stage, a batch at a time (see the module docs).
pub(in super::super) struct RemoteSink {
	batch: BatchWriter,
	/// How many rows the read copied.
	rows: usize,
	/// The records the cache could not decode, every sync root's, handed over before the first row.
	undecodable: Vec<UndecodableItem>,
	/// What placing the rows reads them with, and what the placed side is read with.
	paging: Paging,
	/// Why the stage could not take a row (a full disk), after which the sink copies none: a copy
	/// with a row missing is not the snapshot, so [`finish`](Self::finish) returns this instead of
	/// placing it. Kept here rather than handed to the read, which would report a write to the
	/// stage as a failed read of the cache.
	failed: Option<rusqlite::Error>,
}

impl RemoteSink {
	/// A sink into `stage`, with its two tables and the `fold` the placed one's index asks, placed
	/// and read as `paging` says.
	pub(in super::super) fn new(stage: Stage, paging: Paging) -> rusqlite::Result<Self> {
		register_fold(&stage.conn)?;
		stage.conn.execute_batch(SCHEMA)?;
		Ok(Self {
			batch: BatchWriter::new(stage),
			rows: 0,
			undecodable: Vec::new(),
			paging,
			failed: None,
		})
	}

	/// Place every row the read copied under `root` (see the module docs): the view a whole pass
	/// reads the remote from, its nodes staged and its skipped items, held paths and nothing else
	/// filled in, as `ViewBuilder::finish` hands its view over. A row the copy could not write fails
	/// it with that row's error, before anything is placed.
	pub(in super::super) fn finish(self, root: Uuid) -> rusqlite::Result<RemoteView> {
		let Self {
			batch,
			rows,
			undecodable,
			paging,
			failed,
		} = self;
		if let Some(error) = failed {
			return Err(error);
		}
		let stage = batch.finish()?;
		stage.conn.execute_batch(RAW_BY_PARENT)?;
		let undecodable_dirs: HashSet<Uuid> = undecodable
			.iter()
			.filter(|item| item.stable_uuid.is_none())
			.map(|item| item.uuid)
			.collect();
		let mut walk = Walk {
			batch: BatchWriter::new(stage),
			children: paging.children,
			undecodable_dirs: &undecodable_dirs,
			visited: Visited::with_rows(rows),
			reached: 0,
			placed: 0,
			held: BTreeSet::new(),
		};
		walk.run(root)?;
		let Walk {
			batch,
			visited,
			reached,
			placed,
			held,
			..
		} = walk;
		let stage = batch.finish()?;
		let left = if reached < rows {
			unplaced(&stage.conn, &visited)?
		} else {
			Vec::new()
		};
		let skipped = replay(&stage.conn, root, undecodable, left)?;
		stage.conn.execute_batch(PLACED)?;
		Ok(RemoteView {
			nodes: Side::Staged(StagedSide::new(stage, placed, REMOTE, paging)),
			has_collisions: false,
			held_paths: held,
			skipped,
			ignored: BTreeMap::new(),
			ignored_default_untracked: 0,
		})
	}
}

impl SnapshotSink for RemoteSink {
	fn undecodable(&mut self, items: Vec<UndecodableItem>) {
		self.undecodable = items;
	}

	fn item(&mut self, is_dir: bool, item: RemoteItem) {
		if self.failed.is_some() {
			return;
		}
		// What the view keeps of a directory decided here, once, as `ViewBuilder` decides it: no
		// lineage, hash or size, whatever a directory's row is merely expected to leave empty.
		let file = !is_dir;
		let copied = self.batch.row(|conn| {
			conn.prepare_cached(RAW_INSERT)?
				.execute(params![
					item.uuid,
					item.parent,
					is_dir,
					item.name.nfc().collect::<String>(),
					item.stable_uuid.filter(|_| file),
					hash_bytes(item.hash.as_ref().filter(|_| file)),
					if file { item.size } else { 0 },
					item.modified_millis,
				])
				.map(drop)
		});
		match copied {
			Ok(()) => self.rows += 1,
			Err(error) => self.failed = Some(error),
		}
	}
}

/// The walk down `remote_raw` that places what it can (see the module docs).
struct Walk<'u> {
	/// The placed rows, written a batch at a time, and the copied ones read on the same connection.
	batch: BatchWriter,
	children: NonZeroUsize,
	/// No chain through one of these resolves, so the walk never goes into one.
	undecodable_dirs: &'u HashSet<Uuid>,
	visited: Visited,
	/// How many rows it marked.
	reached: usize,
	/// How many nodes `remote` holds.
	placed: usize,
	/// The paths two items resolved to, which neither is placed at.
	held: BTreeSet<String>,
}

impl Walk<'_> {
	/// Walk from `root` down: one open directory per level, each with the page of its children in
	/// hand, so what it holds is the depth of the tree and not its width.
	fn run(&mut self, root: Uuid) -> rusqlite::Result<()> {
		let mut open = vec![Dir::new(root, String::new(), 0)];
		while let Some(dir) = open.last_mut() {
			let Some(child) = dir.next(self.batch.conn(), self.children)? else {
				open.pop();
				continue;
			};
			// Left unmarked, for the replay to record: an unsafe name is never placed, and nothing
			// under it is.
			if !is_safe_name(&child.item.name) {
				continue;
			}
			let path = join_path(&dir.path, &child.item.name);
			let depth = dir.depth + 1;
			// Counted once whatever reaches it twice, so a row reached twice can never stand in for
			// one left unmarked, whose record the replay would then never write.
			if self.visited.set(child.rowid) {
				self.reached += 1;
			}
			let path = self.place(path, child.dir, &child.item)?;
			// A chain through this directory resolves for what is under it while it is at most the
			// deepest a chain may run; past that, its children are left to the replay. Never into the
			// root's own row (see the module docs): with that, and the cache's uuids unique, no
			// directory is gone into twice.
			if child.dir
				&& depth <= MAX_REMOTE_DEPTH
				&& child.item.uuid != root
				&& !self.undecodable_dirs.contains(&child.item.uuid)
			{
				open.push(Dir::new(child.item.uuid, path, depth));
			}
		}
		Ok(())
	}

	/// Place `item` at `path` as `ViewBuilder::insert` does, handing the path back.
	fn place(&mut self, path: String, dir: bool, item: &RemoteItem) -> rusqlite::Result<String> {
		// The engine's own directory comes before every rule, and a path held back stays held.
		if in_quarantine(&path) || self.held.contains(&path) {
			return Ok(path);
		}
		let node = RemoteNode {
			rel_path: path,
			kind: if dir { NodeKind::Dir } else { NodeKind::File },
			remote_uuid: item.uuid,
			stable_uuid: item.stable_uuid,
			content_hash: item.hash,
			size: item.size,
			modified_millis: item.modified_millis,
		};
		let mut placed = false;
		self.batch.row(|conn| {
			placed = write_node(
				&mut *conn.prepare_cached(REMOTE_SQL.insert)?,
				&node.rel_path,
				&node,
			)? == 1;
			if !placed {
				conn.prepare_cached(REMOTE_SQL.forget)?
					.execute([&node.rel_path])?;
			}
			Ok(())
		})?;
		if placed {
			self.placed += 1;
		} else {
			// Byte-identical names under one parent: the cache mid-transition, as the builder says.
			tracing::debug!(
				"remote view: holding {:?} — the cache is mid-transition, listing two items under that exact name",
				node.rel_path
			);
			self.placed -= 1;
			self.held.insert(node.rel_path.clone());
		}
		Ok(node.rel_path)
	}
}

/// A directory the walk is in: where it is, and the page of its children in hand.
struct Dir {
	uuid: Uuid,
	path: String,
	/// How many names its path has: none for the root.
	depth: usize,
	/// The rowid of the last child read.
	after: i64,
	page: std::vec::IntoIter<Raw>,
	/// Whether the last page read was its last.
	done: bool,
}

impl Dir {
	fn new(uuid: Uuid, path: String, depth: usize) -> Self {
		Self {
			uuid,
			path,
			depth,
			after: 0,
			page: Vec::new().into_iter(),
			done: false,
		}
	}

	fn next(&mut self, conn: &Connection, page: NonZeroUsize) -> rusqlite::Result<Option<Raw>> {
		if self.page.len() == 0 && !self.done {
			let rows = conn
				.prepare_cached(CHILDREN)?
				.query_map(params![self.uuid, self.after, page.get()], raw_of)?
				.collect::<rusqlite::Result<Vec<Raw>>>()?;
			self.done = rows.len() < page.get();
			if let Some(last) = rows.last() {
				self.after = last.rowid;
			}
			self.page = rows.into_iter();
		}
		Ok(self.page.next())
	}
}

/// A bit per row of `remote_raw`, set for each row the walk placed: the one thing the walk holds in
/// memory for each item, an eighth of a byte.
struct Visited(Vec<u64>);

impl Visited {
	fn with_rows(rows: usize) -> Self {
		Self(vec![0; rows / 64 + 1])
	}

	/// Mark `rowid`, and whether it was unmarked.
	fn set(&mut self, rowid: i64) -> bool {
		let Ok(at) = usize::try_from(rowid) else {
			return false;
		};
		if self.0.len() <= at / 64 {
			self.0.resize(at / 64 + 1, 0);
		}
		let Some(word) = self.0.get_mut(at / 64) else {
			return false;
		};
		let bit = 1 << (at % 64);
		let unmarked = *word & bit == 0;
		*word |= bit;
		unmarked
	}

	fn has(&self, rowid: i64) -> bool {
		usize::try_from(rowid).is_ok_and(|at| {
			self.0
				.get(at / 64)
				.is_some_and(|word| word & (1 << (at % 64)) != 0)
		})
	}
}

/// Every row the walk left unmarked, with whether it is a directory, in rowid order.
fn unplaced(conn: &Connection, visited: &Visited) -> rusqlite::Result<Vec<(bool, RemoteItem)>> {
	let mut statement = conn.prepare(EVERY_RAW)?;
	let mut rows = statement.query([])?;
	let mut left = Vec::new();
	while let Some(row) = rows.next()? {
		if !visited.has(row.get(0)?) {
			let raw = raw_of(row)?;
			left.push((raw.dir, raw.item));
		}
	}
	Ok(left)
}

/// What `ViewBuilder` records for the rows the walk left and for the undecodable records (see the
/// module docs): its index holds every directory their chains climb, read off `remote_raw`.
fn replay(
	conn: &Connection,
	root: Uuid,
	undecodable: Vec<UndecodableItem>,
	left: Vec<(bool, RemoteItem)>,
) -> rusqlite::Result<Vec<SkippedRemote>> {
	if left.is_empty() && undecodable.is_empty() {
		return Ok(Vec::new());
	}
	conn.execute_batch(RAW_BY_UUID)?;
	let mut lookup = conn.prepare(RAW_DIR)?;
	let mut dir_index: HashMap<Uuid, (String, Uuid)> = HashMap::new();
	let starts = left
		.iter()
		.map(|(_, item)| item.parent)
		.chain(undecodable.iter().map(|item| item.parent));
	for start in starts {
		// Up to the root, a directory no row is, or one an earlier climb indexed — and with it
		// everything above it, which is why a climb never reads a directory twice.
		let mut at = start;
		while at != root && !dir_index.contains_key(&at) {
			let Some((name, parent)) = lookup
				.query_row([at], |row| Ok((row.get(0)?, row.get(1)?)))
				.optional()?
			else {
				break;
			};
			dir_index.insert(at, (name, parent));
			at = parent;
		}
	}
	Ok(ViewBuilder::replay(root, dir_index, undecodable, left).finish_unplaced())
}

impl StagedSide<RemoteNode> {
	/// Where it holds the FILE `uuid`, off the uuid index, which the first question about a uuid
	/// builds: where the move join of a whole pass's staged reconcile finds a row's file now (see
	/// `stage::reconcile`). Of two files of one uuid, which no view holds but a test can, the last in
	/// path order, as a walk of the side keeping the last one it met answers.
	pub(in super::super) fn file_of_uuid(&self, uuid: Uuid) -> Option<String> {
		self.read("where a file is", |conn| {
			conn.execute_batch(BY_UUID)?;
			conn.prepare_cached(FILE_OF_UUID)?
				.query_row([uuid], |row| row.get(0))
				.optional()
		})
	}

	/// [`file_of_uuid`](Self::file_of_uuid), by the file's lineage: what a move that carried an edit,
	/// which re-mints the uuid, keeps. The lineage index is built by the first question about one.
	pub(in super::super) fn file_of_lineage(&self, lineage: StableUuid) -> Option<String> {
		self.read("where a file of a lineage is", |conn| {
			conn.execute_batch(BY_LINEAGE)?;
			conn.prepare_cached(FILE_OF_LINEAGE)?
				.query_row([lineage], |row| row.get(0))
				.optional()
		})
	}

	/// Where it holds each of `uuids` it holds, a seek of the uuid index each; the first call builds
	/// the index.
	pub(in super::super) fn paths_of(&self, uuids: &HashSet<Uuid>) -> HashMap<Uuid, String> {
		self.read("where items are", |conn| {
			conn.execute_batch(BY_UUID)?;
			let mut statement = conn.prepare_cached(PATH_OF)?;
			let mut out = HashMap::new();
			for uuid in uuids {
				if let Some(path) = statement.query_row([uuid], |row| row.get(0)).optional()? {
					out.insert(*uuid, path);
				}
			}
			Ok(out)
		})
	}

	/// Drop every node whose path folds together with another's but the first of them in path
	/// order, and every node whose path folds as one of `held` does — a held path took its folded
	/// name when the view held it. Whether it dropped any: the view's collisions (see
	/// `RemoteView::resolve_collisions`).
	///
	/// The folded paths are compared whole, so two that merely share a digest never collide.
	pub(in super::super) fn drop_fold_twins(&mut self, held: &BTreeSet<String>) -> bool {
		let dropped = self.edit("the folded twins dropped", |conn| {
			let keys = conn
				.prepare_cached(TWIN_KEYS)?
				.query_map([], |row| row.get::<_, String>(0))?
				.collect::<rusqlite::Result<Vec<String>>>()?;
			let mut dropped = 0;
			for key in &keys {
				tracing::debug!(
					"remote view: two or more remote items fold onto {key:?}, so no 1:1 local mapping exists"
				);
				dropped += conn.prepare_cached(DROP_TWINS)?.execute([key])?;
			}
			for path in held {
				dropped += conn.prepare_cached(DROP_HELD_TWINS)?.execute([path])?;
			}
			Ok(dropped)
		});
		self.dropped(dropped);
		dropped > 0
	}
}

#[cfg(test)]
mod tests {
	use filen_types::{crypto::Blake3Hash, fs::StableUuid};
	use rand::{Rng, SeedableRng, rngs::StdRng, seq::SliceRandom};

	use super::{
		super::{
			super::{
				changes::tests::{cacheable_dir, cacheable_file},
				ignore::{IgnoreRules, IgnoreSource, Origin},
				plan::{ViewFilter, place_remote_items},
				rows::Baseline,
				scan::QUARANTINE_DIR,
				side::Nodes,
			},
			assert_views_alike, paging_of,
			tests::row,
		},
		*,
	};
	use crate::{
		alloc_meter,
		cache::{CacheState, read_subtree_snapshot, read_subtree_snapshot_into},
	};

	const CASES: u64 = 200;
	const ROOT: Uuid = Uuid::from_u128(1);
	/// A parent no row is.
	const MISSING: Uuid = Uuid::from_u128(2);
	/// The account root of a cache the tests write, with [`ROOT`] a directory under it, so that the
	/// sync root has a row of its own to move.
	const ACCOUNT: Uuid = Uuid::from_u128(3);

	/// The names a generated snapshot draws from: case twins, plain and non-ASCII; two spellings NFC
	/// folds into one, so two siblings drawing them are held back; every unsafe name; the engine's
	/// own quarantine directory; and names the filter's rules hide.
	const NAMES: &[&str] = &[
		"a",
		"A",
		"b.txt",
		"B.TXT",
		"\u{c4}",
		"\u{e4}",
		"e\u{301}",
		"\u{e9}",
		QUARANTINE_DIR,
		"build",
		"x.tmp",
		".DS_Store",
		"..",
		".",
		"",
		"s/l",
		"b\\s",
		"n\0l",
	];

	/// A generated snapshot: its items in the order a read hands them over, and its undecodable
	/// records.
	struct Snapshot {
		items: Vec<(bool, RemoteItem)>,
		undecodable: Vec<UndecodableItem>,
	}

	/// What `seed` generates: a tree drawn from [`NAMES`], with items under a file, under a parent
	/// no row is, under an undecodable directory, now and then a directory with an undecodable
	/// record of its own, a cycle of two directories, the root's own row under a directory of the
	/// tree, and a chain deeper than a chain may run, with an unsafe name somewhere along it or
	/// none. Everything the cache can list and everything a `ViewBuilder` answers for.
	fn snapshot(seed: u64) -> Snapshot {
		let mut rng = StdRng::seed_from_u64(seed);
		let mut next = 100u128;
		let mut fresh = || {
			next += 1;
			Uuid::from_u128(next)
		};
		let item = |uuid, parent, name: &str, is_dir: bool, rng: &mut StdRng| {
			let hash = rng.random_range(0..3u8);
			RemoteItem {
				uuid,
				parent,
				name: name.to_owned(),
				stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
				hash: (!is_dir && hash > 0).then(|| Blake3Hash::from([hash; 32])),
				size: if is_dir { 0 } else { rng.random_range(0..10) },
				modified_millis: rng.random_range(0..10),
			}
		};
		let (mut dirs, mut files, mut undecodable_dirs) = (vec![ROOT], Vec::new(), Vec::new());
		let (mut items, mut undecodable) = (Vec::new(), Vec::new());
		for _ in 0..rng.random_range(1..=40) {
			let is_dir = rng.random_bool(0.4);
			let pick = |from: &Vec<Uuid>, rng: &mut StdRng| from[rng.random_range(0..from.len())];
			let parent = match rng.random_range(0..20) {
				0 => MISSING,
				1 if !files.is_empty() => pick(&files, &mut rng),
				2 if !undecodable_dirs.is_empty() => pick(&undecodable_dirs, &mut rng),
				_ => pick(&dirs, &mut rng),
			};
			let uuid = fresh();
			let name = NAMES[rng.random_range(0..NAMES.len())];
			items.push((is_dir, item(uuid, parent, name, is_dir, &mut rng)));
			if is_dir {
				dirs.push(uuid);
			} else {
				files.push(uuid);
			}
			if rng.random_range(0..6) == 0 {
				let is_dir = rng.random_bool(0.5);
				let uuid = fresh();
				let parent = match rng.random_range(0..6) {
					0 => MISSING,
					_ => dirs[rng.random_range(0..dirs.len())],
				};
				undecodable.push(UndecodableItem {
					uuid,
					parent,
					stable_uuid: (!is_dir).then(|| StableUuid::new_for_test(uuid)),
				});
				if is_dir {
					undecodable_dirs.push(uuid);
				}
			}
		}
		// A directory the cache also holds an undecodable record of. The cache's own listing never
		// hands over both (a decodable row hides the record), but the builder answers for it, and so
		// must the walk: nothing under it is placed.
		if rng.random_range(0..4) == 0
			&& let Some((_, dir)) = items.iter().find(|(is_dir, _)| *is_dir)
		{
			undecodable.push(UndecodableItem {
				uuid: dir.uuid,
				parent: dir.parent,
				stable_uuid: None,
			});
		}
		if rng.random_range(0..6) == 0 {
			let (one, other) = (fresh(), fresh());
			items.push((true, item(one, other, "cycle", true, &mut rng)));
			items.push((true, item(other, one, "cycle", true, &mut rng)));
			items.push((false, item(fresh(), one, "in.txt", false, &mut rng)));
		}
		// The root's own row, under a directory of its subtree or under itself: a cache that holds
		// the root moved into its own subtree lists it with the rest. Every chain ends at the root,
		// so whatever the row is placed as holds nothing: the root's children are the root's.
		if rng.random_range(0..6) == 0 {
			let parent = dirs[rng.random_range(0..dirs.len())];
			items.push((true, item(ROOT, parent, "loop", true, &mut rng)));
		}
		if rng.random_range(0..8) == 0 {
			let depth = rng.random_range(MAX_REMOTE_DEPTH - 4..=MAX_REMOTE_DEPTH + 4);
			let unsafe_at = rng.random_range(0..depth * 2);
			let mut parent = dirs[rng.random_range(0..dirs.len())];
			for at in 0..depth {
				let uuid = fresh();
				let name = if at == unsafe_at { ".." } else { "deep" };
				items.push((true, item(uuid, parent, name, true, &mut rng)));
				parent = uuid;
			}
			items.push((false, item(fresh(), parent, "foot.txt", false, &mut rng)));
		}
		items.shuffle(&mut rng);
		Snapshot { items, undecodable }
	}

	/// The view `ViewBuilder` builds from `snapshot`, its rows streamed in in order.
	fn resident(snapshot: &Snapshot) -> RemoteView {
		let mut builder = ViewBuilder::with_capacity(ROOT, 0);
		builder.undecodable(snapshot.undecodable.clone());
		for (is_dir, item) in &snapshot.items {
			builder.item(*is_dir, item.clone());
		}
		builder.finish()
	}

	/// The view staged from `snapshot`, placed and read as `paging` says.
	fn staged(snapshot: &Snapshot, paging: Paging) -> (tempfile::TempDir, RemoteView) {
		let (dir, stage) = Stage::in_temp_dir();
		let mut sink = RemoteSink::new(stage, paging).unwrap();
		sink.undecodable(snapshot.undecodable.clone());
		for (is_dir, item) in &snapshot.items {
			sink.item(*is_dir, item.clone());
		}
		(dir, sink.finish(ROOT).unwrap())
	}

	/// Root rules that hide `build/` and `*.tmp`, over the built-in defaults, which hide every
	/// `.DS_Store`; and rows under two of those, so a default hit there is a root and elsewhere is
	/// counted on its own.
	fn rules_and_rows() -> (IgnoreRules, Baseline) {
		let (source, errors) =
			IgnoreSource::parse("build/\n*.tmp\n", Origin::File { dir: "" }).unwrap();
		assert!(errors.is_empty(), "{errors:?}");
		let mut rules = IgnoreRules::new(None);
		rules.insert_file(String::new(), source);
		(
			rules,
			Baseline::from_rows([row(".DS_Store"), row("a/.DS_Store")]),
		)
	}

	/// The staged view places, holds and records exactly what `ViewBuilder` does, before the rules
	/// and after them, over 200 generated snapshots read a few children and a few rows a statement
	/// as well as a full page: the expectation is not written down here, it is whatever the
	/// builder answers.
	#[test]
	fn the_staged_view_is_the_one_the_builder_builds() {
		let (rules, rows) = rules_and_rows();
		let filter = ViewFilter {
			rules: &rules,
			baseline: &rows,
		};
		let (mut held, mut skipped, mut collided, mut hidden, mut untracked) = (0, 0, 0, 0, 0);
		let mut looped = 0;
		for paging in [paging_of(1), paging_of(2), paging_of(7), Paging::PASS] {
			for seed in 0..CASES {
				let snapshot = snapshot(seed);
				looped += usize::from(snapshot.items.iter().any(|(_, item)| item.uuid == ROOT));
				let at = |step: &str| format!("{paging:?} seed {seed}: {step}");
				let resident = resident(&snapshot);
				let (_dir, staged) = staged(&snapshot, paging);
				assert_views_alike(&at("as placed"), &resident, &staged);
				held += resident.held_paths.len();
				skipped += resident.skipped.len();

				for filter in [None, Some(filter)] {
					let mut resident = self::resident(&snapshot);
					resident.filter(filter);
					let (_dir, mut staged) = self::staged(&snapshot, paging);
					staged.filter(filter);
					assert_views_alike(&at("filtered"), &resident, &staged);
					collided += usize::from(resident.has_collisions);
					hidden += resident.ignored.len();
					untracked += resident.ignored_default_untracked;
				}
			}
		}
		// A corpus that stopped producing these would let the equality above pass by proving
		// nothing about them.
		assert!(held > 100, "only {held} held path(s)");
		assert!(skipped > 1000, "only {skipped} record(s)");
		assert!(collided > 100, "only {collided} collision(s)");
		assert!(hidden > 100, "only {hidden} ignored root(s)");
		assert!(untracked > 100, "only {untracked} untracked default hit(s)");
		assert!(looped > 50, "only {looped} root row(s) under the tree");
	}

	/// The same, through the cache's own read: generated snapshots written into a cache DB and read
	/// back as a pass reads them, copied into the stage inside the read's transaction on one side
	/// and collected into two `Vec`s for `place_remote_items` on the other. What the cache lists is
	/// a subtree of the root, so an orphan and a cycle never reach either side, but the root's own
	/// row does once a snapshot moves it under a directory of its subtree.
	#[test]
	fn the_staged_view_of_a_cache_read_is_the_one_the_builder_builds() {
		let mut items_read = 0;
		for seed in 0..32 {
			let snapshot = snapshot(seed);
			let dir = tempfile::tempdir().unwrap();
			let path = dir.path().join("cache.db");
			let mut cache = CacheState::new_on_path(&path, ACCOUNT);
			cache
				.upsert_dirs([cacheable_dir(ROOT, ACCOUNT, "root")].into_iter())
				.unwrap();
			let (dirs, files): (Vec<_>, Vec<_>) =
				snapshot.items.iter().partition(|(is_dir, _)| *is_dir);
			cache
				.upsert_dirs(
					dirs.iter()
						.map(|(_, item)| cacheable_dir(item.uuid, item.parent, &item.name)),
				)
				.unwrap();
			cache
				.upsert_files(
					files
						.iter()
						.map(|(_, item)| cacheable_file(item.uuid, item.parent, &item.name)),
				)
				.unwrap();
			cache
				.replace_undecodable(ROOT, &snapshot.undecodable)
				.unwrap();

			let read = read_subtree_snapshot(&path, ROOT).unwrap();
			items_read += read.dirs.len() + read.files.len();
			let mut resident = place_remote_items(ROOT, &read.dirs, &read.files, &read.undecodable);
			let (_stage_dir, stage) = Stage::in_temp_dir();
			let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
			let watermark = read_subtree_snapshot_into(&path, ROOT, &mut sink).unwrap();
			assert_eq!(watermark, read.watermark, "seed {seed}");
			let mut staged = sink.finish(ROOT).unwrap();
			assert_views_alike(&format!("seed {seed}: as placed"), &resident, &staged);
			resident.filter(None);
			staged.filter(None);
			assert_views_alike(&format!("seed {seed}: filtered"), &resident, &staged);
		}
		assert!(items_read > 300, "only {items_read} item(s) read");
	}

	/// A cache holding the sync root under its own child `d` lists the root's row with the rest,
	/// under `d`. The builder ends every chain at the root, so it places that row at `d/loop` and the
	/// root's children once, at the top; a walk that went into the row would read them again under
	/// it, at every second level down to the deepest a chain may run.
	#[test]
	fn a_sync_root_listed_under_its_own_child_is_walked_once() {
		let d = Uuid::from_u128(10);
		let dir = tempfile::tempdir().unwrap();
		let path = dir.path().join("cache.db");
		let mut cache = CacheState::new_on_path(&path, ACCOUNT);
		cache
			.upsert_dirs(
				[
					cacheable_dir(ROOT, ACCOUNT, "root"),
					cacheable_dir(d, ROOT, "d"),
				]
				.into_iter(),
			)
			.unwrap();
		cache
			.upsert_files([cacheable_file(Uuid::from_u128(11), ROOT, "f.txt")].into_iter())
			.unwrap();
		cache
			.upsert_dirs([cacheable_dir(ROOT, d, "loop")].into_iter())
			.unwrap();

		let read = read_subtree_snapshot(&path, ROOT).unwrap();
		assert_eq!(
			read.dirs.len() + read.files.len(),
			3,
			"the read lists the root's row"
		);
		let resident = place_remote_items(ROOT, &read.dirs, &read.files, &read.undecodable);
		let (_stage_dir, stage) = Stage::in_temp_dir();
		let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
		read_subtree_snapshot_into(&path, ROOT, &mut sink).unwrap();
		let staged = sink.finish(ROOT).unwrap();
		assert_views_alike("the root under its own child", &resident, &staged);
		let paths: Vec<String> = staged
			.nodes
			.whole()
			.iter()
			.map(|(path, _)| path.into_owned())
			.collect();
		assert_eq!(paths, ["d", "d/loop", "f.txt"]);
		assert_eq!(staged.skipped, []);
	}

	/// A row the stage cannot take stops the copy, and placing it returns that row's error: a copy
	/// with a row missing is not the snapshot. Forced with triggers that refuse two of four rows,
	/// as a full disk would.
	#[test]
	fn a_row_the_stage_cannot_take_fails_the_copy_with_its_error() {
		let (_dir, stage) = Stage::in_temp_dir();
		let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
		sink.batch
			.conn()
			.execute_batch(
				"CREATE TRIGGER refuse_b BEFORE INSERT ON remote_raw WHEN NEW.name = 'b'
				 BEGIN SELECT RAISE(FAIL, 'no room for b'); END;
				 CREATE TRIGGER refuse_c BEFORE INSERT ON remote_raw WHEN NEW.name = 'c'
				 BEGIN SELECT RAISE(FAIL, 'no room for c'); END;",
			)
			.unwrap();
		sink.undecodable(Vec::new());
		for (n, name) in (10..).zip(["a", "b", "c", "d"]) {
			sink.item(
				false,
				RemoteItem {
					uuid: Uuid::from_u128(n),
					parent: ROOT,
					name: name.to_owned(),
					stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(n))),
					hash: None,
					size: 1,
					modified_millis: 0,
				},
			);
		}
		assert_eq!(
			sink.rows, 1,
			"only the row before the refused one is copied"
		);
		let Err(error) = sink.finish(ROOT) else {
			panic!("a copy missing a row was placed");
		};
		assert!(
			error.to_string().contains("no room for b"),
			"the first refusal is the error: {error}"
		);
	}

	/// Staging a 20,000-item snapshot, placing it and reading it all back holds a page of it, not
	/// the tree: the builder holds a few MiB for the same items.
	#[test]
	fn staging_and_placing_a_snapshot_holds_a_page_of_it() {
		const DIRS: u128 = 100;
		const FILES: u128 = 20_000;
		let items: Vec<(bool, RemoteItem)> = (0..DIRS)
			.map(|n| {
				(
					true,
					RemoteItem {
						uuid: Uuid::from_u128(1_000 + n),
						parent: ROOT,
						name: format!("d{n:03}"),
						stable_uuid: None,
						hash: None,
						size: 0,
						modified_millis: 0,
					},
				)
			})
			.chain((0..FILES).map(|n| {
				(
					false,
					RemoteItem {
						uuid: Uuid::from_u128(100_000 + n),
						parent: Uuid::from_u128(1_000 + n % DIRS),
						name: format!("f{n:05}.txt"),
						stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(100_000 + n))),
						hash: Some(Blake3Hash::from([7; 32])),
						size: 1,
						modified_millis: 1,
					},
				)
			}))
			.collect();
		let builder_peak = alloc_meter::peak_bytes(|| {
			let mut builder = ViewBuilder::with_capacity(ROOT, 0);
			builder.undecodable(Vec::new());
			for (is_dir, item) in &items {
				builder.item(*is_dir, item.clone());
			}
			builder.finish().nodes.whole().len()
		});
		let (_dir, stage) = Stage::in_temp_dir();
		let (read, peak) = alloc_meter::peak_bytes(|| {
			let mut sink = RemoteSink::new(stage, Paging::PASS).unwrap();
			sink.undecodable(Vec::new());
			// Moved in one at a time, as the read hands them over: the `Vec` they come from is the
			// test's, and counted before the meter starts.
			for (is_dir, item) in &items {
				sink.item(*is_dir, item.clone());
			}
			let view = sink.finish(ROOT).unwrap();
			view.nodes.whole().iter().count()
		});
		assert_eq!(read, items.len());
		assert_eq!(builder_peak.0, items.len());
		assert!(
			peak < 1024 * 1024,
			"staging, placing and reading back the snapshot held {peak} bytes at once, where the \
			 builder held {}",
			builder_peak.1
		);
	}

	/// What a pass's journal asks of the view: where it holds each uuid. A staged view answers off
	/// its uuid index as a walk of the map answers, for items it holds, moved or not, and for ones
	/// it does not.
	#[test]
	fn a_staged_view_finds_items_by_uuid_as_a_walk_does() {
		let snapshot = Snapshot {
			items: vec![
				(
					true,
					RemoteItem {
						uuid: Uuid::from_u128(10),
						parent: ROOT,
						name: "docs".to_owned(),
						stable_uuid: None,
						hash: None,
						size: 0,
						modified_millis: 0,
					},
				),
				(
					false,
					RemoteItem {
						uuid: Uuid::from_u128(11),
						parent: Uuid::from_u128(10),
						name: "a.txt".to_owned(),
						stable_uuid: Some(StableUuid::new_for_test(Uuid::from_u128(11))),
						hash: None,
						size: 3,
						modified_millis: 0,
					},
				),
			],
			undecodable: Vec::new(),
		};
		let resident = resident(&snapshot);
		let (_dir, mut staged) = staged(&snapshot, Paging::PASS);
		let asked = HashSet::from([10, 11, 12].map(Uuid::from_u128));
		let found = |view: &RemoteView| -> BTreeMap<Uuid, String> {
			view.nodes
				.whole()
				.paths_of(&asked)
				.into_iter()
				.map(|(uuid, path)| (uuid, path.into_owned()))
				.collect()
		};
		assert_eq!(
			found(&staged),
			BTreeMap::from([
				(Uuid::from_u128(10), "docs".to_owned()),
				(Uuid::from_u128(11), "docs/a.txt".to_owned()),
			])
		);
		assert_eq!(found(&staged), found(&resident));
		staged
			.nodes
			.rekey_subtree(&Baseline::default(), "docs", "notes", |_, _| {});
		assert_eq!(
			found(&staged),
			BTreeMap::from([
				(Uuid::from_u128(10), "notes".to_owned()),
				(Uuid::from_u128(11), "notes/a.txt".to_owned()),
			])
		);
	}
}
