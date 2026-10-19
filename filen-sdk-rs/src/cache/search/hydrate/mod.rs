//! The search engine's DB side: its own READ-ONLY connection to the cache file (WAL lets it read
//! concurrently with the worker's writer), the `filen_name_matches` scalar function that gives
//! SQLite full-Unicode case-insensitive matching (its built-in `LIKE`/`lower()` are ASCII-only)
//! via per-query compiled regexes (see `NameMatchers`), and the window/count queries that do ALL
//! filtering, ordering, windowing, and hydration inside SQLite — the engine holds no result set
//! in memory.
//!
//! NAME NORMALIZATION ASSUMPTION: cached names are assumed to already be NFC-normalized
//! (matching compares codepoints; there is no canonical-equivalence pass). Pre-v4 drives can
//! contain NFD names written by old clients; those may fail to match visually-identical needles
//! until the v4 re-encode normalizes the whole drive. The NEEDLE is always NFC-normalized in
//! Rust (user input is untrusted either way).

use std::{borrow::Cow, ops::Range, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::Blake3Hash};
use regex::bytes::Regex;
use rusqlite::{Connection, Row, functions::FunctionFlags, params, types::ValueRef};
use uuid::Uuid;

// Native-only: the engine opens its own read connection here (wasm routes reads through the
// worker), so these are unused on the wasm cache build.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
use {
	crate::cache::{open_sqlite_with_flags, sql::bound_memory},
	rusqlite::OpenFlags,
	std::path::Path,
};

use crate::{
	crypto::file::FileKey,
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};

use super::{
	config::CompiledFilter,
	result::{SearchHit, SearchResult},
};
use crate::cache::sql::columns::{
	COUNT, DIR_CREATED, DIR_FAVORITE, DIR_NAME, DIR_TIMESTAMP, DIRS_COLOR, FILE_CREATED,
	FILE_FAVORITE, FILE_NAME, FILE_TIMESTAMP, FILES_BUCKET, FILES_CHUNKS, FILES_CHUNKS_SIZE,
	FILES_HASH, FILES_KEY, FILES_KEY_VERSION, FILES_MIME, FILES_MODIFIED, FILES_REGION, FILES_SIZE,
	FILES_STABLE_UUID, ITEMS_PARENT, ITEMS_TYPE, ITEMS_UUID, SEARCH_PARENT_PATH, SEARCH_TOTAL,
};

// Bounds the worker round-trip in `ReadConn::run`; wasm-only (native reads are synchronous, so
// the worker read path does not exist there).
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
use wasmtimer::tokio::timeout as reply_timeout;

/// How long a [`ReadConn`] query waits for its reply. The worker serves reads with
/// priority (including during a resync's network waits), so the residual queue time is one
/// in-flight unit of worker work — e.g. applying one drained event burst. A timeout surfaces as
/// a query error: the engine logs it and keeps the window's last snapshot; the next ping retries.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
const READ_REPLY_TIMEOUT: Duration = Duration::from_secs(30);

const SEARCH_WINDOW_ACCOUNT: &str = include_str!("../raw/search_window_account.sql");
const SEARCH_WINDOW_SUBTREE: &str = include_str!("../raw/search_window_subtree.sql");
const SEARCH_WINDOW_CHILDREN: &str = include_str!("../raw/search_window_children.sql");
const SEARCH_COUNT_ACCOUNT: &str = include_str!("../raw/search_count_account.sql");
const SEARCH_COUNT_SUBTREE: &str = include_str!("../raw/search_count_subtree.sql");
const SEARCH_COUNT_CHILDREN: &str = include_str!("../raw/search_count_children.sql");

/// What part of the cache a search queries: the account root needs no scoping at all (everything
/// cached lives under it), a subdir scope walks the recursive CTE, and non-recursive mode is a
/// parent lookup.
#[derive(Debug, Clone, Copy)]
pub(super) enum Scope {
	/// Carries the account-root uuid — bound as the climb stop-anchor so each result's parent
	/// path is computed relative to the account root.
	Account(Uuid),
	Subtree(Uuid),
	Children(Uuid),
}

/// A read query shipped to the cache worker, run against ITS connection between drains (the
/// wasm read path — see [`ReadConn`]).
pub(crate) type ReadTask = Box<dyn FnOnce(&Connection) + Send + 'static>;

// Where the engine's queries run — a separate newtype per target (the choice is fixed at compile
// time), not an enum. Native holds a dedicated READ-ONLY connection reading concurrently with the
// worker's writer via WAL. The wasm VFS supports neither WAL nor a second connection to the same
// DB, so wasm holds a sender that boxes each query up and runs it on the cache worker's single
// connection between drains — which serializes snapshots behind in-flight worker work (a bounded
// resync attempt at worst), instead of reading in parallel.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(super) struct ReadConn(pub(super) Connection);
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub(super) struct ReadConn(pub(super) tokio::sync::mpsc::UnboundedSender<ReadTask>);

impl ReadConn {
	/// Run `query` against the cache DB: synchronously on the native connection, or round-tripped
	/// through the worker on wasm. A worker that shut down surfaces as an error (the search is
	/// terminal by then anyway).
	pub(super) async fn run<T, F>(&self, query: F) -> rusqlite::Result<T>
	where
		T: Send + 'static,
		F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
	{
		#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
		{
			query(&self.0)
		}
		#[cfg(all(target_family = "wasm", target_os = "unknown"))]
		{
			let (reply_sender, reply_receiver) = tokio::sync::oneshot::channel();
			self.0
				.send(Box::new(move |conn| {
					let _ = reply_sender.send(query(conn));
				}))
				.map_err(|_| worker_gone())?;
			match reply_timeout(READ_REPLY_TIMEOUT, reply_receiver).await {
				Ok(reply) => reply.map_err(|_| worker_gone())?,
				// A worker alive-but-wedged is indistinguishable from dead from here; erroring
				// beats hanging the search forever (the caller keeps its last snapshot).
				Err(_) => Err(read_timed_out())?,
			}
		}
	}
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn read_timed_out() -> rusqlite::Error {
	rusqlite::Error::UserFunctionError(
		"cache worker did not answer a search query in time (wedged or overloaded)".into(),
	)
}

#[cfg(all(target_family = "wasm", target_os = "unknown"))]
fn worker_gone() -> rusqlite::Error {
	rusqlite::Error::UserFunctionError(
		"cache worker shut down before answering a search query".into(),
	)
}

/// The matchers behind the SQL `filen_name_matches(name, needle, case_insensitive)` function:
/// one escaped-literal regex per case mode, compiled once per query and cached on the needle
/// argument via SQLite's auxiliary-data slot (`get_or_create_aux` — kept across rows, dropped
/// when the bound needle changes). An escaped literal compiles to a SIMD-prefiltered substring
/// scan, so the per-row cost is allocation-free with NO haystack transform — unlike a
/// `to_lowercase().contains()` fold, which rewrites every non-ASCII name on every row.
///
/// BOTH modes are compiled together because the aux slot is keyed on the NEEDLE argument alone:
/// the case flag is a separate binding whose changes do not invalidate this slot, so the cached
/// object must serve either flag value.
///
/// Case-insensitive matching is Unicode SIMPLE case folding (`(?i)`). Versus folding both sides
/// through `to_lowercase`: Greek Σ/σ/ς now match position-independently (needle "ΟΔΟΣ" finds
/// "ΟΔΟΣ.txt", which final-sigma lowercasing got wrong), ſ/s and µ/μ fold together, while İ no
/// longer folds onto plain "i" (it needs full folding, which neither approach does — same as
/// ß≠ss). Matching runs on the raw TEXT bytes, so an out-of-contract non-UTF-8 name is matched
/// bytewise instead of erroring the whole query.
struct NameMatchers {
	sensitive: Regex,
	insensitive: Regex,
}

impl NameMatchers {
	/// Only fails on pathological needles (the compiled-size limit); an escaped literal can
	/// never produce a parse error.
	fn compile(needle: &str) -> Result<Self, regex::Error> {
		let literal = regex::escape(needle);
		Ok(Self {
			sensitive: Regex::new(&literal)?,
			insensitive: Regex::new(&format!("(?i){literal}"))?,
		})
	}

	fn matches(&self, name: &[u8], case_insensitive: bool) -> bool {
		if case_insensitive {
			self.insensitive.is_match(name)
		} else {
			self.sensitive.is_match(name)
		}
	}
}

/// Open the engine's NATIVE read-only connection and register `filen_name_matches` on it (WAL
/// lets it read concurrently with the worker's writer). `NO_MUTEX` is safe because the
/// connection never leaves the engine task; the busy timeout rides out WAL checkpoints by the
/// worker. On wasm the engine never opens this — the wasm VFS supports neither WAL nor a second
/// connection, so reads route to the worker instead (see [`ReadConn`]).
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
pub(in crate::cache) fn open_read_connection(path: &Path) -> rusqlite::Result<Connection> {
	let conn = open_sqlite_with_flags(
		path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)?;
	conn.busy_timeout(Duration::from_millis(5000))?;
	bound_memory(&conn)?;
	register_name_matches(&conn)?;
	Ok(conn)
}

/// Register the `filen_name_matches` scalar function on `conn` (re-registering replaces, so this
/// is idempotent). Needed on every connection search queries run against: the engine's own read
/// connection on native, and the WORKER's single connection (which serves [`ReadConn`]
/// queries) on wasm.
pub(in crate::cache) fn register_name_matches(conn: &Connection) -> rusqlite::Result<()> {
	conn.create_scalar_function(
		"filen_name_matches",
		3,
		FunctionFlags::SQLITE_UTF8
			| FunctionFlags::SQLITE_DETERMINISTIC
			| FunctionFlags::SQLITE_INNOCUOUS,
		|ctx| {
			let name = match ctx.get_raw(0) {
				// An items row whose files/dirs companion row is missing (mid-upsert or
				// corrupt): never a match; the explaining event converges it later.
				ValueRef::Null => return Ok(false),
				ValueRef::Text(bytes) => bytes,
				other => {
					return Err(rusqlite::Error::InvalidFunctionParameterType(
						0,
						other.data_type(),
					));
				}
			};
			if matches!(ctx.get_raw(1), ValueRef::Text(needle) if needle.is_empty()) {
				return Ok(true);
			}
			let case_insensitive: bool = ctx.get(2)?;
			let matchers: Arc<NameMatchers> = ctx.get_or_create_aux(
				1,
				|needle| -> Result<_, Box<dyn std::error::Error + Send + Sync + 'static>> {
					Ok(NameMatchers::compile(needle.as_str()?)?)
				},
			)?;
			Ok(matchers.matches(name, case_insensitive))
		},
	)
}

fn type_filter_param(filter: &CompiledFilter) -> i64 {
	use super::config::SearchItemType;
	match filter.item_type {
		SearchItemType::All => 0,
		SearchItemType::Dir => 1,
		SearchItemType::File => 2,
	}
}

/// One window: scope + filter + order + LIMIT/OFFSET + full hydration, all in one statement.
/// With a needle this scans the scope (substring matches cannot use an index); without one it
/// is bounded by the scope and the window.
/// One window of results PLUS the pre-`LIMIT` match total, from a single scan: the window
/// statements compute `count(*) OVER ()` on every row, so the count costs nothing on top of the
/// sort the window already pays for. Two cases still fall back to [`count_results`] (same
/// connection, so within a caller's transaction the totals agree): an empty requested range
/// (no window pass to piggyback on) and an empty result page (offset past the end, or zero
/// matches — no rows means no piggybacked total).
pub(super) fn window_and_count(
	conn: &Connection,
	scope: Scope,
	filter: &CompiledFilter,
	range: &Range<usize>,
) -> rusqlite::Result<(Vec<SearchHit>, usize)> {
	let limit = i64::try_from(range.end.saturating_sub(range.start)).unwrap_or(i64::MAX);
	let offset = i64::try_from(range.start).unwrap_or(i64::MAX);
	if limit == 0 {
		return Ok((Vec::new(), count_results(conn, scope, filter)?));
	}
	let needle = filter.needle.as_deref().unwrap_or("");
	let type_filter = type_filter_param(filter);
	let case_insensitive = !filter.case_sensitive;
	let map_row = |row: &Row<'_>| {
		let hit = SearchHit {
			result: row_to_result(row)?,
			parent_path: row.get::<_, String>(SEARCH_PARENT_PATH)?.into(),
		};
		Ok((hit, row.get::<_, i64>(SEARCH_TOTAL)?))
	};
	let rows: rusqlite::Result<Vec<(SearchHit, i64)>> = match scope {
		Scope::Account(root) => conn
			.prepare_cached(SEARCH_WINDOW_ACCOUNT)?
			.query_map(
				params![root, type_filter, needle, case_insensitive, limit, offset],
				map_row,
			)?
			.collect(),
		Scope::Subtree(anchor) => conn
			.prepare_cached(SEARCH_WINDOW_SUBTREE)?
			.query_map(
				params![anchor, type_filter, needle, case_insensitive, limit, offset],
				map_row,
			)?
			.collect(),
		Scope::Children(parent) => conn
			.prepare_cached(SEARCH_WINDOW_CHILDREN)?
			.query_map(
				params![parent, type_filter, needle, case_insensitive, limit, offset],
				map_row,
			)?
			.collect(),
	};
	let rows = rows?;
	let Some(&(_, total)) = rows.first() else {
		return Ok((Vec::new(), count_results(conn, scope, filter)?));
	};
	let results = rows.into_iter().map(|(hit, _)| hit).collect();
	Ok((results, usize::try_from(total.max(0)).unwrap_or(usize::MAX)))
}

/// Test-facing shim over [`window_and_count`] for assertions that only care about the page.
#[cfg(test)]
pub(super) fn window_results(
	conn: &Connection,
	scope: Scope,
	filter: &CompiledFilter,
	range: &Range<usize>,
) -> rusqlite::Result<Vec<SearchHit>> {
	window_and_count(conn, scope, filter, range).map(|(results, _)| results)
}

/// The total match count for the same scope + filter as [`window_results`].
pub(super) fn count_results(
	conn: &Connection,
	scope: Scope,
	filter: &CompiledFilter,
) -> rusqlite::Result<usize> {
	let needle = filter.needle.as_deref().unwrap_or("");
	let type_filter = type_filter_param(filter);
	let case_insensitive = !filter.case_sensitive;
	let count: i64 = match scope {
		// The count SQL is unchanged (no path computation, no anchor param), so the account-root
		// uuid is unused here.
		Scope::Account(_) => conn
			.prepare_cached(SEARCH_COUNT_ACCOUNT)?
			.query_row(params![type_filter, needle, case_insensitive], |row| {
				row.get(COUNT)
			})?,
		Scope::Subtree(anchor) => conn.prepare_cached(SEARCH_COUNT_SUBTREE)?.query_row(
			params![anchor, type_filter, needle, case_insensitive],
			|row| row.get(COUNT),
		)?,
		Scope::Children(parent) => conn.prepare_cached(SEARCH_COUNT_CHILDREN)?.query_row(
			params![parent, type_filter, needle, case_insensitive],
			|row| row.get(COUNT),
		)?,
	};
	Ok(usize::try_from(count.max(0)).unwrap_or(usize::MAX))
}

/// The position of `column` in `row`'s result set, for the `FromSqlConversionFailure` payload
/// (which is index-based). Only ever called on an error path, so the name lookup is free in
/// practice; an unresolvable name degrades to 0 rather than masking the real error.
fn column_index(row: &Row<'_>, column: &str) -> usize {
	let stmt: &rusqlite::Statement<'_> = row.as_ref();
	stmt.column_index(column).unwrap_or(0)
}

fn conversion_error(
	row: &Row<'_>,
	column: &str,
	error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(
		column_index(row, column),
		rusqlite::types::Type::Text,
		Box::new(error),
	)
}

fn timestamp_millis(row: &Row<'_>, column: &str, millis: i64) -> rusqlite::Result<DateTime<Utc>> {
	DateTime::from_timestamp_millis(millis).ok_or_else(|| {
		rusqlite::Error::FromSqlConversionFailure(
			column_index(row, column),
			rusqlite::types::Type::Integer,
			Box::new(rusqlite::types::FromSqlError::OutOfRange(millis)),
		)
	})
}

/// One window-query row → the full result payload, mirroring the column encodings written by
/// `sql/file.rs` / `sql/dir.rs`.
///
/// Every column is read by NAME, so this is independent of the `SELECT` order — which the three
/// window queries do NOT agree on (`search_window_children.sql` emits `parent_path` before
/// `total`, the account/subtree windows after). Shared with `cache::enumerate` (the sync engine's
/// whole-subtree snapshot projects the SAME columns under the same names).
pub(in crate::cache) fn row_to_result(row: &Row<'_>) -> rusqlite::Result<SearchResult> {
	let uuid: Uuid = row.get(ITEMS_UUID)?;
	let parent: Uuid = row.get(ITEMS_PARENT)?;
	let item_type: i64 = row.get(ITEMS_TYPE)?;
	match item_type {
		2 => {
			let key_str: String = row.get(FILES_KEY)?;
			let key_version: u8 = row.get(FILES_KEY_VERSION)?;
			let key_version = FileEncryptionVersion::try_from(key_version)
				.map_err(|e| conversion_error(row, FILES_KEY_VERSION, e))?;
			let key = FileKey::from_str_with_version(&key_str, key_version)
				.map_err(|e| conversion_error(row, FILES_KEY, e))?;
			let hash = row
				.get::<_, Option<String>>(FILES_HASH)?
				.map(|hex_str| {
					let mut bytes = [0u8; 32];
					hex::decode_to_slice(hex_str, &mut bytes)
						.map_err(|e| conversion_error(row, FILES_HASH, e))?;
					Ok::<_, rusqlite::Error>(Blake3Hash::from(bytes))
				})
				.transpose()?;
			Ok(SearchResult::File(CacheableFile {
				uuid,
				parent,
				stable_uuid: row.get(FILES_STABLE_UUID)?,
				chunks_size: row.get(FILES_CHUNKS_SIZE)?,
				chunks: row.get(FILES_CHUNKS)?,
				favorited: row.get(FILE_FAVORITE)?,
				region: Cow::Owned(row.get::<_, String>(FILES_REGION)?),
				bucket: Cow::Owned(row.get::<_, String>(FILES_BUCKET)?),
				timestamp: timestamp_millis(row, FILE_TIMESTAMP, row.get(FILE_TIMESTAMP)?)?,
				size: row.get(FILES_SIZE)?,
				name: Cow::Owned(row.get::<_, String>(FILE_NAME)?),
				mime: Cow::Owned(row.get::<_, String>(FILES_MIME)?),
				key,
				created: row
					.get::<_, Option<i64>>(FILE_CREATED)?
					.map(|millis| timestamp_millis(row, FILE_CREATED, millis))
					.transpose()?,
				last_modified: timestamp_millis(row, FILES_MODIFIED, row.get(FILES_MODIFIED)?)?,
				hash,
			}))
		}
		1 => Ok(SearchResult::Dir(CacheableDir {
			uuid,
			parent,
			favorited: row.get(DIR_FAVORITE)?,
			color: row
				.get::<_, Option<DirColor<'static>>>(DIRS_COLOR)?
				.unwrap_or_default(),
			timestamp: timestamp_millis(row, DIR_TIMESTAMP, row.get(DIR_TIMESTAMP)?)?,
			name: Cow::Owned(row.get::<_, String>(DIR_NAME)?),
			created: row
				.get::<_, Option<i64>>(DIR_CREATED)?
				.map(|millis| timestamp_millis(row, DIR_CREATED, millis))
				.transpose()?,
		})),
		other => Err(rusqlite::Error::FromSqlConversionFailure(
			column_index(row, ITEMS_TYPE),
			rusqlite::types::Type::Integer,
			Box::new(rusqlite::types::FromSqlError::OutOfRange(other)),
		)),
	}
}

#[cfg(test)]
mod tests;
