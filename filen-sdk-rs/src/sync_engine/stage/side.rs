//! A side of a whole pass, staged: a row per node of one table of the pass's stage, read a page at
//! a time, where a map held a node per item of the tree in memory. Both of a whole pass's sides are
//! staged: the local walk ([`local`](super::local)) and the remote snapshot
//! ([`remote`](super::remote)), each in a table of its own described by a [`Table`].
//!
//! # Reads
//!
//! Every read takes the connection for one statement and lets it go before it hands anything out,
//! as the baseline's own reads do (see `rows::snapshot`), so a caller asking the side something
//! while it is handed a page of it never waits on itself. A walk of the side reads [`PAGE`] rows a
//! statement. A question about one path is answered out of the page the last such question read
//! when that page spans the path, and that page doubles, up to [`PAGE`], while the questions walk
//! it in path order (see `page`): the reconcile asks about every path it decides, one after the
//! next, and pays a statement a page rather than one a path.
//!
//! # A failed read or edit
//!
//! A read that fails panics, as a baseline read does: "nothing here" is an answer the pass acts on,
//! and a failed read must not become it. An edit that fails panics too: the table is the pass's only
//! record of its side, and one that no longer matches the pass leaves it nothing to plan from. Both
//! can happen only while the pass plans, before it has applied anything. Writing a side in the first
//! place can fail instead ([`StageSink`](super::StageSink), [`RemoteSink`](super::RemoteSink)),
//! which fails the pass before it plans.

use std::{
	borrow::Cow,
	fmt,
	num::NonZeroUsize,
	ops::{Bound, Range},
	sync::{Mutex, PoisonError},
	vec,
};

use rusqlite::{Connection, OptionalExtension, Row, Statement, params};

use super::{
	super::{
		page::{Page, ReadAhead},
		plan::subtree_bounds,
		scan::collision_key,
		side::NodesAt,
	},
	Stage,
	remote::CHILDREN_PAGE,
};

/// How many rows a walk of the side reads a statement, and the most a question's read-ahead does.
pub(super) const PAGE: NonZeroUsize = NonZeroUsize::new(1024).expect("1024 is not zero");

/// How many rows a directory move re-keys a statement (see [`StagedSide::rekey`]).
const REKEY_SLICE: NonZeroUsize = NonZeroUsize::new(65_536).expect("65,536 is not zero");

/// How many rows each statement that stages a side or reads one takes at most: a pass's
/// ([`PASS`](Self::PASS)), or a handful, for a test that walks, moves and places a few nodes across
/// several statements. None is zero, which would leave a walk with nothing to read and a move with
/// nothing to re-key.
#[derive(Debug, Clone, Copy)]
pub(in super::super) struct Paging {
	/// How many rows a walk of a side reads a statement, and the most a question's read-ahead does.
	pub(super) page: NonZeroUsize,
	/// How many rows a directory move re-keys a statement (see [`StagedSide::rekey`]).
	pub(super) rekey: NonZeroUsize,
	/// How many children of a directory placing a remote snapshot reads a statement (see
	/// [`RemoteSink::finish`](super::RemoteSink::finish)).
	pub(super) children: NonZeroUsize,
}

impl Paging {
	/// What a pass stages and reads its sides with.
	pub(in super::super) const PASS: Self = Self {
		page: PAGE,
		rekey: REKEY_SLICE,
		children: CHILDREN_PAGE,
	};
}

/// The statements every read and edit of a staged side's table runs. Every statement names the
/// table's columns in the same order, the path first: the page statements and `remove` select them
/// so, and `insert` and `update` bind them so (`?1` the path).
pub(super) struct Sql {
	/// Up to `?2` rows from the path `?1` on (`>=`), in path order.
	pub(super) page_from: &'static str,
	/// Up to `?2` rows after the path `?1` (`>`), in path order.
	pub(super) page_after: &'static str,
	/// [`page_from`](Self::page_from), below the path `?2`; the limit is `?3`.
	pub(super) page_from_below: &'static str,
	/// [`page_after`](Self::page_after), below the path `?2`; the limit is `?3`.
	pub(super) page_after_below: &'static str,
	/// [`page_from`](Self::page_from), of directories only, off the table's partial index of them.
	pub(super) dirs_from: &'static str,
	/// [`page_after`](Self::page_after), of directories only, off the same index.
	pub(super) dirs_after: &'static str,
	/// A row, unless a row already holds its path (or, where the table keeps one spelling of each
	/// folded name, its folded name).
	pub(super) insert: &'static str,
	/// Overwrite the row at the path `?1`.
	pub(super) update: &'static str,
	/// Take the row at `?1` out, returning it as the page statements select it.
	pub(super) remove: &'static str,
	/// Take the row at `?1` out.
	pub(super) forget: &'static str,
	/// Take every row strictly under `?1` out.
	pub(super) forget_under: &'static str,
	/// Whether a row's folded path is `?1` or lies under it, one `EXISTS` per seek of the folded
	/// index: an `OR` of a point and a range would not seek.
	pub(super) occupied: &'static str,
	/// A directory move: the directory's own row (`?1` onto `?2`)...
	pub(super) rekey_at: &'static str,
	/// ...then up to `?3` of the rows under it, on one seek: a slice of the subtree, which moves
	/// its rows out of the source's range, so the next slice starts on what is left. `length` and
	/// `substr` count characters on text, so the rest of a path is cut where its prefix ends.
	pub(super) rekey_under: &'static str,
}

/// How a staged side's table is read and written: its statements, and how a row becomes a node and
/// a node a row. One per table, as a `const`, which is what lets a
/// [`Side`](super::super::side::Side) of any `T` hold a staged backing with no bound on `T`: only
/// the two tables' constructors make one.
pub(super) struct Table<T> {
	pub(super) sql: &'static Sql,
	/// The node a row holds, read as the page statements select it.
	pub(super) node: fn(&Row<'_>) -> rusqlite::Result<T>,
	/// Bind `node`, filed at `path`, to `insert` or `update` and run it.
	pub(super) write: fn(&mut Statement<'_>, &str, &T) -> rusqlite::Result<usize>,
	/// The path a node carries, which is the one its row is filed under.
	pub(super) path: fn(&T) -> &str,
}

// By hand: a derive would ask `T: Copy`, and a table holds no `T`.
impl<T> Clone for Table<T> {
	fn clone(&self) -> Self {
		*self
	}
}

impl<T> Copy for Table<T> {}

/// Which of a table's rows a walk of it reads.
enum Over {
	/// Every row.
	All,
	/// The rows below this path.
	Below(String),
	/// The directories' rows, off the table's index of them.
	Dirs,
}

/// Up to `limit` of the nodes `over` reads, from `from` on, in path order.
fn read_page<T>(
	conn: &Connection,
	table: &Table<T>,
	from: Bound<&str>,
	over: &Over,
	limit: usize,
) -> rusqlite::Result<Vec<T>> {
	let (inclusive, from) = match from {
		Bound::Included(path) => (true, path),
		Bound::Excluded(path) => (false, path),
		// Every path is at or after the empty one.
		Bound::Unbounded => (true, ""),
	};
	let sql = match (over, inclusive) {
		(Over::All, true) => table.sql.page_from,
		(Over::All, false) => table.sql.page_after,
		(Over::Below(_), true) => table.sql.page_from_below,
		(Over::Below(_), false) => table.sql.page_after_below,
		(Over::Dirs, true) => table.sql.dirs_from,
		(Over::Dirs, false) => table.sql.dirs_after,
	};
	let mut statement = conn.prepare_cached(sql)?;
	let rows = match over {
		Over::Below(below) => statement.query(params![from, below, limit])?,
		Over::All | Over::Dirs => statement.query(params![from, limit])?,
	};
	rows.mapped(table.node).collect()
}

/// The nodes a whole pass staged for one of its sides, read out of the stage (see the module docs):
/// what a [`Side::Staged`](super::super::side::Side::Staged) holds.
///
/// A node's path is the row's: what it was filed under, and what a directory move re-keys it to.
pub(crate) struct StagedSide<T> {
	stage: Mutex<Stage>,
	/// How many nodes the table holds.
	len: usize,
	/// The page the questions about one path read last (see the module docs).
	ahead: Mutex<ReadAhead<Page<T>>>,
	/// [`Paging::page`]: how many rows a walk reads a statement.
	page: NonZeroUsize,
	/// [`Paging::rekey`]: how many rows a directory move re-keys a statement.
	rekey: NonZeroUsize,
	table: Table<T>,
}

impl<T> fmt::Debug for StagedSide<T> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("StagedSide")
			.field("len", &self.len)
			.finish_non_exhaustive()
	}
}

impl<T> StagedSide<T> {
	/// The `len` nodes `stage` holds in `table`, read and re-keyed as `paging` says.
	pub(super) fn new(stage: Stage, len: usize, table: Table<T>, paging: Paging) -> Self {
		Self {
			stage: Mutex::new(stage),
			len,
			ahead: Mutex::new(ReadAhead::default()),
			page: paging.page,
			rekey: paging.rekey,
			table,
		}
	}

	/// How many nodes it holds.
	pub(in super::super) fn len(&self) -> usize {
		self.len
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
			conn.prepare_cached(self.table.sql.occupied)?
				.query_row([folded], |row| row.get(0))
		})
	}

	/// Every node with its path, in path order.
	pub(in super::super) fn pages(&self) -> Pages<'_, T> {
		Pages {
			side: self,
			cursor: Cursor::from(Bound::Unbounded, Over::All),
		}
	}

	/// Every node strictly under `dir`, with its path, in path order.
	pub(in super::super) fn pages_under(&self, dir: &str) -> Pages<'_, T> {
		let bounds = subtree_bounds(dir);
		Pages {
			side: self,
			cursor: Cursor::from(Bound::Included(bounds.start), Over::Below(bounds.end)),
		}
	}

	/// Every directory node with its path, in path order, off the table's index of directories:
	/// none of the files is read.
	pub(in super::super) fn dir_pages(&self) -> Pages<'_, T> {
		Pages {
			side: self,
			cursor: Cursor::from(Bound::Unbounded, Over::Dirs),
		}
	}

	/// [`pages`](Self::pages), handing the nodes over.
	pub(in super::super) fn into_pages(self) -> IntoPages<T> {
		IntoPages {
			side: self,
			cursor: Cursor::from(Bound::Unbounded, Over::All),
		}
	}

	/// Every path strictly under `dir`.
	pub(in super::super) fn subtree_paths(&self, dir: &str) -> Vec<String> {
		self.pages_under(dir).map(|(path, _)| path).collect()
	}

	/// File `node` at `path`, over whatever was there.
	///
	/// A table that holds one spelling of each folded name (the local walk's) refuses a node at
	/// another spelling of a name it holds, and that panics as a failed edit does (see the module
	/// docs): no pass puts one there, as no pass adds a node to a whole local side.
	pub(in super::super) fn insert(&mut self, path: &str, node: &T) {
		let table = self.table;
		let added = self.edit("a node put", |conn| {
			if (table.write)(&mut *conn.prepare_cached(table.sql.insert)?, path, node)? == 1 {
				return Ok(Some(true));
			}
			let updated = (table.write)(&mut *conn.prepare_cached(table.sql.update)?, path, node)?;
			Ok((updated == 1).then_some(false))
		});
		match added {
			Some(added) => self.len += usize::from(added),
			None => panic!(
				"the staged side holds another spelling of {path:?}, and its table holds one \
				 spelling of each folded name"
			),
		}
	}

	/// Take the node at `path` out, returning it.
	pub(in super::super) fn remove(&mut self, path: &str) -> Option<T> {
		let table = self.table;
		let removed = self.edit("a node taken out", |conn| {
			conn.prepare_cached(table.sql.remove)?
				.query_row([path], table.node)
				.optional()
		});
		self.len -= usize::from(removed.is_some());
		removed
	}

	/// Take the node at `path` out, saying whether there was one.
	pub(in super::super) fn forget(&mut self, path: &str) -> bool {
		let table = self.table;
		let forgot = self.edit("a node dropped", |conn| {
			conn.prepare_cached(table.sql.forget)?.execute([path])
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

	/// Walk every node in path order, dropping the ones `hidden` hides with everything under them.
	///
	/// `hidden` is asked about a node and answers with the root of what hides it — the node itself,
	/// or the ancestor a rule hides — and whether every node it hides counts on its own. The root
	/// goes with its whole subtree in two statements, so nothing under it is asked about. Returns how
	/// many nodes the roots that count each took besides the one asked about.
	///
	/// Where [`retain`](Self::retain) asks about every node and drops each with a statement of its
	/// own, holding every dropped path until the walk is done.
	pub(in super::super) fn drop_hidden(
		&mut self,
		mut hidden: impl FnMut(&str, &T) -> Option<(String, bool)>,
	) -> usize {
		let mut also = 0;
		// The subtrees dropped whose range the walk has not passed yet, for the nodes of the page in
		// hand that went with them: the table no longer holds them, the page still does. Each is
		// dropped at a node past the one before it and outside its range, which is then before that
		// range in path order — `a.txt` after `a` and before `a/` — so the ranges end ever earlier
		// up the stack, and a node is under a dropped subtree only where it is under the top one
		// still ahead of it.
		let mut gone: Vec<Range<String>> = Vec::new();
		let mut cursor = Cursor::from(Bound::Unbounded, Over::All);
		while let Some((path, node)) = cursor.next(self) {
			while gone
				.last()
				.is_some_and(|range| range.end.as_str() <= path.as_str())
			{
				gone.pop();
			}
			if gone
				.last()
				.is_some_and(|range| range.start.as_str() <= path.as_str())
			{
				continue;
			}
			let Some((root, each)) = hidden(&path, &node) else {
				continue;
			};
			let table = self.table;
			let dropped = self.edit("a hidden subtree dropped", |conn| {
				Ok(conn.prepare_cached(table.sql.forget)?.execute([&root])?
					+ conn
						.prepare_cached(table.sql.forget_under)?
						.execute([&root])?)
			});
			self.len -= dropped;
			// The asked node is the root or under it, and was read from the table just now: it is
			// among them.
			if each {
				also += dropped.saturating_sub(1);
			}
			gone.push(subtree_bounds(&root));
		}
		also
	}

	/// Move the node at `from` and everything under it onto `to`. The destination must be free
	/// under every spelling, as a directory move's is, or the edit fails (see the module docs).
	///
	/// The rows under `from` go [`Paging::rekey`] a statement ([`REKEY_SLICE`] in a pass). An
	/// update that rewrites the very index it finds its rows by has SQLite collect them before it
	/// writes any, so what one statement over the whole subtree holds in memory grows with the
	/// subtree; a slice bounds it.
	pub(in super::super) fn rekey(&mut self, from: &str, to: &str) {
		let (table, slice) = (self.table, self.rekey.get());
		self.edit("a directory move", |conn| {
			conn.prepare_cached(table.sql.rekey_at)?
				.execute([from, to])?;
			let mut under = conn.prepare_cached(table.sql.rekey_under)?;
			// A slice that came back short was the last: no row is left in the source's range.
			while under.execute(params![from, to, slice])? == slice {}
			Ok(())
		});
	}

	/// What `read` answers on the connection, which a failed read panics instead of answering (see
	/// the module docs).
	pub(super) fn read<R>(
		&self,
		what: &str,
		read: impl FnOnce(&Connection) -> rusqlite::Result<R>,
	) -> R {
		let stage = self.stage.lock().unwrap_or_else(PoisonError::into_inner);
		read(&stage.conn).unwrap_or_else(|error| {
			panic!(
				"reading the pass's staged side in {} ({what}) failed: {error} — refusing to read a \
				 failed read as an absent node",
				stage.path().display()
			)
		})
	}

	/// What `edit` answers on the connection, which a failed edit panics instead of answering (see
	/// the module docs). The read-ahead goes with it: it no longer says what the table holds.
	pub(super) fn edit<R>(
		&mut self,
		what: &str,
		edit: impl FnOnce(&Connection) -> rusqlite::Result<R>,
	) -> R {
		*self.ahead.get_mut().unwrap_or_else(PoisonError::into_inner) = ReadAhead::default();
		let stage = self.stage.get_mut().unwrap_or_else(PoisonError::into_inner);
		edit(&stage.conn).unwrap_or_else(|error| {
			panic!(
				"editing the pass's staged side in {} ({what}) failed: {error} — it is the pass's \
				 only record of that side",
				stage.path().display()
			)
		})
	}

	/// Take `dropped` nodes off the count, for an edit that dropped them outside the methods here.
	pub(super) fn dropped(&mut self, dropped: usize) {
		self.len -= dropped;
	}

	/// The node at exactly `path`, handed to `answer`: out of the page the last question read when
	/// it spans the path, else out of a new one read from the path (see the module docs).
	fn point<R>(&self, path: &str, answer: impl Fn(Option<&T>) -> R) -> R {
		self.ahead
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.ask(
				path,
				self.page,
				|page| page.answer(path).map(&answer),
				|want| {
					let nodes = self.read("a path", |conn| {
						read_page(conn, &self.table, Bound::Included(path), &Over::All, want)
					});
					Page::read(path, true, None, want, nodes, self.table.path)
				},
			)
	}
}

impl<T: Clone> StagedSide<T> {
	/// The node at `path`.
	pub(in super::super) fn at(&self, path: &str) -> Option<T> {
		self.point(path, |node| node.cloned())
	}
}

/// A staged side answers about a path on its own: it derives nothing from the rows, so the move
/// join of a whole pass's staged reconcile asks it directly (see `stage::reconcile`).
impl<T: Clone> NodesAt for StagedSide<T> {
	type Node = T;

	fn at(&self, path: &str) -> Option<Cow<'_, T>> {
		Self::at(self, path).map(Cow::Owned)
	}

	fn holds(&self, path: &str) -> bool {
		Self::holds(self, path)
	}

	fn occupied(&self, path: &str) -> bool {
		Self::occupied(self, path)
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

/// Where a walk of a staged side is: the page in hand, and where the next one starts.
struct Cursor<T> {
	next: Bound<String>,
	over: Over,
	page: vec::IntoIter<T>,
	/// Whether the last page read ran to the end of what the walk covers.
	done: bool,
}

impl<T> Cursor<T> {
	fn from(next: Bound<String>, over: Over) -> Self {
		Self {
			next,
			over,
			page: Vec::new().into_iter(),
			done: false,
		}
	}

	fn next(&mut self, side: &StagedSide<T>) -> Option<(String, T)> {
		if self.page.len() == 0 && !self.done {
			let nodes = side.read("a page", |conn| {
				read_page(
					conn,
					&side.table,
					self.next.as_ref().map(String::as_str),
					&self.over,
					side.page.get(),
				)
			});
			self.done = nodes.len() < side.page.get();
			if let Some(last) = nodes.last() {
				self.next = Bound::Excluded((side.table.path)(last).to_owned());
			}
			self.page = nodes.into_iter();
		}
		let node = self.page.next()?;
		Some(((side.table.path)(&node).to_owned(), node))
	}
}

/// A staged side's nodes with their paths, a page at a time.
pub(crate) struct Pages<'s, T> {
	side: &'s StagedSide<T>,
	cursor: Cursor<T>,
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
	cursor: Cursor<T>,
}

impl<T> Iterator for IntoPages<T> {
	type Item = (String, T);

	fn next(&mut self) -> Option<(String, T)> {
		self.cursor.next(&self.side)
	}
}
