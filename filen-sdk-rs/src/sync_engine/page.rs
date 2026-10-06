//! A page of rows read in path order, and the read-ahead a question about one path reads pages
//! with: what the baseline's reads (`rows::snapshot`), a staged side's (`stage::side`) and the
//! staged confirmations' (`stage::confirmed`) answer out of.
//!
//! # Read-ahead
//!
//! A question the page in hand cannot answer reads a new page from its path on, sized to how the
//! last one was used ([`ReadAhead::ask`]). A question past the last row of a page that answered at
//! least half as many questions as it holds rows is a walk in path order — which may pass over the
//! rows it does not ask about, as the move detection asks about files and passes the directories —
//! and reads twice as many rows as that page did, up to a ceiling; any other question reads one
//! row, which is what a lookup of the one path reads. So a walk pays a statement a page rather than
//! one a path, a walk backwards one a path, and lookups of scattered paths read a page of at most
//! four.
//!
//! Half and not all: a walk that passes over a row or two of each page would otherwise read one row
//! a statement again, page after page, and over a tree's directories that was a statement for every
//! few files. It counts questions, not distinct rows: a sparse walk asking each path several times
//! grows its pages past the one row a path needs, reading a few rows more per path, never a wrong
//! answer.

use std::num::NonZeroUsize;

/// A contiguous stretch of a table, in path order, as one read saw it: every row from `from` on
/// (`inclusive` or not) up to the last row read — or, when the read ran out of rows before its
/// limit, up to the end of the range it asked for, `to` (exclusive) or the table's end. Within that
/// stretch the page is AUTHORITATIVE: a path it holds no row at has no row in the table either, for
/// as long as nothing writes to the table, which every reader here sees to.
#[derive(Debug)]
pub(super) struct Page<T> {
	from: String,
	inclusive: bool,
	to: Option<String>,
	/// Whether the read reached the end of its range rather than its limit.
	complete: bool,
	pub(super) rows: Vec<T>,
	/// The path a row is filed at, which the read ordered them by.
	path: fn(&T) -> &str,
}

/// What a [`Page`] says about one path.
#[derive(Debug)]
pub(super) enum PageAnswer<'p, T> {
	/// The path lies outside the stretch the page read, so the page cannot say.
	Outside,
	/// The page spans the path and so speaks for the table there: the row at it, or `None` where
	/// the table holds none.
	Spans(Option<&'p T>),
}

impl<'p, T> PageAnswer<'p, T> {
	/// `read` of the row or its absence, where the page spans the path; `None` where it cannot say.
	pub(super) fn map<R>(self, read: impl FnOnce(Option<&'p T>) -> R) -> Option<R> {
		match self {
			Self::Spans(row) => Some(read(row)),
			Self::Outside => None,
		}
	}
}

impl<T> Page<T> {
	/// The `rows` a read of up to `limit` of them handed back, from `from` on (`inclusive` or not)
	/// and strictly below `to` where there is one, each filed at its `path`.
	pub(super) fn read(
		from: &str,
		inclusive: bool,
		to: Option<&str>,
		limit: usize,
		rows: Vec<T>,
		path: fn(&T) -> &str,
	) -> Self {
		Self {
			from: from.to_owned(),
			inclusive,
			to: to.map(str::to_owned),
			complete: rows.len() < limit,
			rows,
			path,
		}
	}

	/// What this page says about `path` (see [`PageAnswer`]).
	pub(super) fn answer(&self, path: &str) -> PageAnswer<'_, T> {
		let past_from = if self.inclusive {
			path >= self.from.as_str()
		} else {
			path > self.from.as_str()
		};
		let before_end = match self.rows.last() {
			Some(last) if path <= (self.path)(last) => true,
			_ => self.complete && self.to.as_deref().is_none_or(|to| path < to),
		};
		if !(past_from && before_end) {
			return PageAnswer::Outside;
		}
		PageAnswer::Spans(
			self.rows
				.binary_search_by(|row| (self.path)(row).cmp(path))
				.ok()
				.and_then(|at| self.rows.get(at)),
		)
	}

	/// Whether the read stopped at its limit, so the range goes on past the last row.
	pub(super) fn is_full(&self) -> bool {
		!self.complete
	}

	/// What the page holds in memory past its own handle, in bytes, given what each row holds past
	/// its own.
	#[cfg(feature = "bench-internals")]
	pub(super) fn bytes(&self, row: impl Fn(&T) -> usize) -> usize {
		self.from.capacity()
			+ self.to.as_ref().map_or(0, String::capacity)
			+ self.rows.capacity() * size_of::<T>()
			+ self.rows.iter().map(row).sum::<usize>()
	}
}

/// The page the questions of one kind read last, and how many of them it answered (see the module
/// docs).
pub(super) struct ReadAhead<P> {
	/// Boxed, so that what holds a read-ahead stays a few words wide — a staged side, which a pass
	/// holds in the same enum as the sides it holds in memory — for one allocation a statement.
	page: Option<Box<P>>,
	answered: usize,
}

impl<P> Default for ReadAhead<P> {
	fn default() -> Self {
		Self {
			page: None,
			answered: 0,
		}
	}
}

impl<T> ReadAhead<Page<T>> {
	/// What `answer` makes of the page in hand where that page spans `path`, else of the page `read`
	/// reads from `path` on for as many rows as it is handed, at most `most` (see the module docs).
	/// `answer` is `None` for a page that does not span `path`, and a page read from `path` does.
	pub(super) fn ask<R>(
		&mut self,
		path: &str,
		most: NonZeroUsize,
		answer: impl Fn(&Page<T>) -> Option<R>,
		read: impl FnOnce(usize) -> Page<T>,
	) -> R {
		if let Some(found) = self.page.as_deref().and_then(&answer) {
			self.answered += 1;
			return found;
		}
		let want = match self.page.as_deref() {
			Some(page)
				if self.answered.saturating_mul(2) >= page.rows.len()
					&& page
						.rows
						.last()
						.is_some_and(|last| (page.path)(last) < path) =>
			{
				page.rows.len().saturating_mul(2).min(most.get())
			}
			_ => 1,
		};
		let page = Box::new(read(want));
		let found = answer(&page).expect("a page read from a path answers for that path");
		*self = Self {
			page: Some(page),
			answered: 1,
		};
		found
	}

	/// The page the last question it could not answer read.
	#[cfg(feature = "bench-internals")]
	pub(super) fn page(&self) -> Option<&Page<T>> {
		self.page.as_deref()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const MOST: NonZeroUsize = NonZeroUsize::new(16).expect("16 is not zero");

	/// `(answer, rows read)` for each of `asked`, over a table of the paths `0000`..`0099`.
	fn asked(asked: impl IntoIterator<Item = usize>) -> Vec<(Option<String>, usize)> {
		let table: Vec<String> = (0..100).map(|n| format!("{n:04}")).collect();
		let mut ahead = ReadAhead::default();
		asked
			.into_iter()
			.map(|n| {
				let at = format!("{n:04}");
				let mut read = 0;
				let found = ahead.ask(
					&at,
					MOST,
					|page: &Page<String>| page.answer(&at).map(|row| row.cloned()),
					|want| {
						let rows: Vec<String> = table
							.iter()
							.filter(|row| **row >= at)
							.take(want)
							.cloned()
							.collect();
						read = rows.len();
						Page::read(&at, true, None, want, rows, String::as_str)
					},
				);
				(found, read)
			})
			.collect()
	}

	/// A walk in path order doubles its pages up to the ceiling, also when it asks about only half
	/// of each page's rows, and its answers are the table's.
	#[test]
	fn a_walk_in_order_doubles_its_pages_up_to_the_ceiling() {
		let every = asked(0..40);
		assert!(
			every
				.iter()
				.enumerate()
				.all(|(n, (found, _))| *found == Some(format!("{n:04}")))
		);
		let reads: Vec<usize> = every
			.iter()
			.map(|&(_, read)| read)
			.filter(|&read| read > 0)
			.collect();
		assert_eq!(reads, [1, 2, 4, 8, 16, 16]);

		let every_other: Vec<usize> = asked((0..40).step_by(2))
			.into_iter()
			.map(|(_, read)| read)
			.filter(|&read| read > 0)
			.collect();
		assert_eq!(every_other, [1, 2, 4, 8, 16, 16]);
	}

	/// Lookups asking about fewer than half of each page's rows read a page of at most four, a walk
	/// backwards reads the one row a lookup does, and a question past the end of the table finds
	/// no row there.
	#[test]
	fn questions_that_walk_no_page_read_small_pages() {
		let sparse: Vec<usize> = asked((0..100).step_by(10))
			.into_iter()
			.map(|(_, read)| read)
			.collect();
		assert_eq!(sparse, [1, 2, 4, 1, 2, 4, 1, 2, 4, 1]);

		let backwards = asked((0..10).rev());
		assert!(
			backwards
				.iter()
				.all(|(found, read)| found.is_some() && *read == 1),
			"{backwards:?}"
		);

		assert_eq!(asked([99, 120]), [(Some("0099".to_owned()), 1), (None, 0)]);
	}
}
