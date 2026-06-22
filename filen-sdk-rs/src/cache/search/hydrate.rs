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
use {rusqlite::OpenFlags, std::path::Path};

use crate::{
	crypto::file::FileKey,
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};

use super::{
	config::CompiledFilter,
	result::{SearchHit, SearchResult},
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

const SEARCH_WINDOW_ACCOUNT: &str = include_str!("raw/search_window_account.sql");
const SEARCH_WINDOW_SUBTREE: &str = include_str!("raw/search_window_subtree.sql");
const SEARCH_WINDOW_CHILDREN: &str = include_str!("raw/search_window_children.sql");
const SEARCH_COUNT_ACCOUNT: &str = include_str!("raw/search_count_account.sql");
const SEARCH_COUNT_SUBTREE: &str = include_str!("raw/search_count_subtree.sql");
const SEARCH_COUNT_CHILDREN: &str = include_str!("raw/search_count_children.sql");

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
pub(super) fn open_read_connection(path: &Path) -> rusqlite::Result<Connection> {
	let conn = Connection::open_with_flags(
		path,
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
	)?;
	conn.busy_timeout(Duration::from_millis(5000))?;
	// Scan-heavy reader: give it the same page-cache/mmap budget as the worker connection (a
	// subtree query touches most of a ~100MB DB at scale; the 2MB default thrashes).
	conn.execute_batch("PRAGMA cache_size = -32768; PRAGMA mmap_size = 268435456;")?;
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
	let limit = range.end.saturating_sub(range.start).min(i64::MAX as usize) as i64;
	let offset = range.start.min(i64::MAX as usize) as i64;
	if limit == 0 {
		return Ok((Vec::new(), count_results(conn, scope, filter)?));
	}
	let needle = filter.needle.as_deref().unwrap_or("");
	let type_filter = type_filter_param(filter);
	let case_insensitive = !filter.case_sensitive;
	// `parent_path` and `total` are read by NAME (robust to their column position); the payload
	// columns are positional (see `row_to_result`).
	let map_row = |row: &Row<'_>| {
		let hit = SearchHit {
			result: row_to_result(row)?,
			parent_path: row.get::<_, String>("parent_path")?.into(),
		};
		Ok((hit, row.get::<_, i64>("total")?))
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
	Ok((results, total.max(0) as usize))
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
				row.get(0)
			})?,
		Scope::Subtree(anchor) => conn.prepare_cached(SEARCH_COUNT_SUBTREE)?.query_row(
			params![anchor, type_filter, needle, case_insensitive],
			|row| row.get(0),
		)?,
		Scope::Children(parent) => conn.prepare_cached(SEARCH_COUNT_CHILDREN)?.query_row(
			params![parent, type_filter, needle, case_insensitive],
			|row| row.get(0),
		)?,
	};
	Ok(count.max(0) as usize)
}

fn conversion_error(
	index: usize,
	error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
	rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(error))
}

fn timestamp_millis(index: usize, millis: i64) -> rusqlite::Result<DateTime<Utc>> {
	DateTime::from_timestamp_millis(millis).ok_or_else(|| {
		rusqlite::Error::FromSqlConversionFailure(
			index,
			rusqlite::types::Type::Integer,
			Box::new(rusqlite::types::FromSqlError::OutOfRange(millis)),
		)
	})
}

/// One window-query row → the full result payload, mirroring the column encodings written by
/// `sql/file.rs` / `sql/dir.rs`.
fn row_to_result(row: &Row<'_>) -> rusqlite::Result<SearchResult> {
	let uuid: Uuid = row.get(0)?;
	let parent: Uuid = row.get(1)?;
	let item_type: i64 = row.get(2)?;
	match item_type {
		2 => {
			let key_str: String = row.get(12)?;
			let key_version: u8 = row.get(13)?;
			let key_version = FileEncryptionVersion::try_from(key_version)
				.map_err(|e| conversion_error(13, e))?;
			let key = FileKey::from_str_with_version(&key_str, key_version)
				.map_err(|e| conversion_error(12, e))?;
			let hash = row
				.get::<_, Option<String>>(16)?
				.map(|hex_str| {
					let mut bytes = [0u8; 32];
					hex::decode_to_slice(hex_str, &mut bytes)
						.map_err(|e| conversion_error(16, e))?;
					Ok::<_, rusqlite::Error>(Blake3Hash::from(bytes))
				})
				.transpose()?;
			Ok(SearchResult::File(CacheableFile {
				uuid,
				parent,
				chunks_size: row.get(3)?,
				chunks: row.get(4)?,
				favorited: row.get(5)?,
				region: Cow::Owned(row.get::<_, String>(6)?),
				bucket: Cow::Owned(row.get::<_, String>(7)?),
				timestamp: timestamp_millis(8, row.get(8)?)?,
				size: row.get(9)?,
				name: Cow::Owned(row.get::<_, String>(10)?),
				mime: Cow::Owned(row.get::<_, String>(11)?),
				key,
				created: row
					.get::<_, Option<i64>>(14)?
					.map(|millis| timestamp_millis(14, millis))
					.transpose()?,
				last_modified: timestamp_millis(15, row.get(15)?)?,
				hash,
			}))
		}
		1 => Ok(SearchResult::Dir(CacheableDir {
			uuid,
			parent,
			favorited: row.get(17)?,
			color: row
				.get::<_, Option<DirColor<'static>>>(18)?
				.unwrap_or_default(),
			timestamp: timestamp_millis(19, row.get(19)?)?,
			name: Cow::Owned(row.get::<_, String>(20)?),
			created: row
				.get::<_, Option<i64>>(21)?
				.map(|millis| timestamp_millis(21, millis))
				.transpose()?,
		})),
		other => Err(rusqlite::Error::FromSqlConversionFailure(
			2,
			rusqlite::types::Type::Integer,
			Box::new(rusqlite::types::FromSqlError::OutOfRange(other)),
		)),
	}
}

#[cfg(test)]
mod tests {
	use std::borrow::Cow;

	use chrono::DateTime;
	use filen_types::crypto::Blake3Hash;
	use uuid::Uuid;

	use crate::cache::CacheState;

	use super::super::config::{SearchConfig, SearchItemType};
	use super::*;

	#[test]
	fn matcher_handles_ascii_unicode_and_the_case_toggle() {
		let report = NameMatchers::compile("report").unwrap();
		assert!(report.matches(b"q3-REPORT.txt", true));
		assert!(!report.matches(b"other.txt", true));
		assert!(!report.matches(b"q3-REPORT.txt", false));
		assert!(!NameMatchers::compile("abc").unwrap().matches(b"ab", true));
		assert!(
			NameMatchers::compile("REPORT")
				.unwrap()
				.matches(b"q3-REPORT.txt", false)
		);

		// Unicode simple case folding, in both directions — the needle arrives RAW (NFC only),
		// never pre-folded.
		let umlaut = NameMatchers::compile("ärger").unwrap();
		assert!(umlaut.matches("Ärger.txt".as_bytes(), true));
		assert!(!umlaut.matches("Ärger.txt".as_bytes(), false));
		assert!(
			NameMatchers::compile("ÄRGER")
				.unwrap()
				.matches("ärger.txt".as_bytes(), true)
		);
	}

	#[test]
	fn matcher_folds_sigma_position_independently_and_escapes_metacharacters() {
		// Lowercasing both sides got this WRONG: "ΟΔΟΣ".to_lowercase() ends in final ς while
		// "ΟΔΟΣ.txt".to_lowercase() holds non-final σ (the '.' is Case_Ignorable, so the sigma
		// is not word-final there) — exact-name search failed. Simple case folding puts Σ/σ/ς
		// in one orbit.
		let sigma = NameMatchers::compile("ΟΔΟΣ").unwrap();
		assert!(sigma.matches("ΟΔΟΣ.txt".as_bytes(), true));
		assert!(sigma.matches("οδος-notes.txt".as_bytes(), true));

		// Pre-lowercasing "İstanbul" would inject a combining dot ("i̇stanbul") and break
		// self-matching — this asserts the raw-needle contract end to end.
		let dotted = NameMatchers::compile("İstanbul").unwrap();
		assert!(dotted.matches("İstanbul.txt".as_bytes(), true));

		// Needles are regex-escaped: metacharacters only match themselves.
		let punctuated = NameMatchers::compile("a.b(c").unwrap();
		assert!(punctuated.matches(b"xa.b(cy", false));
		assert!(!punctuated.matches(b"xaxb(cy", false));
	}

	fn temp_db_path() -> std::path::PathBuf {
		std::env::temp_dir().join(format!("filen_search_query_test_{}.db", Uuid::new_v4()))
	}

	fn ms(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	fn test_dir(uuid: Uuid, parent: Uuid, name: &str) -> CacheableDir<'static> {
		CacheableDir {
			uuid,
			parent,
			color: DirColor::Custom(Cow::Borrowed("#123456")),
			favorited: true,
			timestamp: ms(1_700_000_000_000),
			name: Cow::Owned(name.to_string()),
			created: Some(ms(1_700_000_000_001)),
		}
	}

	fn test_file(uuid: Uuid, parent: Uuid, name: &str) -> CacheableFile<'static> {
		CacheableFile {
			uuid,
			parent,
			chunks_size: 7,
			chunks: 2,
			favorited: true,
			region: Cow::Borrowed("eu-central-1"),
			bucket: Cow::Borrowed("bucket-x"),
			timestamp: ms(1_700_000_000_002),
			name: Cow::Owned(name.to_string()),
			size: 1234,
			mime: Cow::Borrowed("image/png"),
			key: FileKey::from_str_with_version(&"b".repeat(64), FileEncryptionVersion::V3)
				.unwrap(),
			last_modified: ms(1_700_000_000_003),
			created: Some(ms(1_700_000_000_004)),
			hash: Some(Blake3Hash::from([7u8; 32])),
		}
	}

	/// account_root → { Beta.txt, sub(dir) → { alpha.txt, Übung.txt } }.
	struct Fixture {
		path: std::path::PathBuf,
		root: Uuid,
		sub: CacheableDir<'static>,
		beta: CacheableFile<'static>,
		alpha: CacheableFile<'static>,
		uebung: CacheableFile<'static>,
		// Held open: the read connection works alongside the writer (WAL).
		_state: CacheState,
	}

	fn fixture() -> Fixture {
		let path = temp_db_path();
		let root = Uuid::new_v4();
		let mut state = CacheState::new_on_path(&path, root);
		let sub = test_dir(Uuid::new_v4(), root, "sub");
		let beta = test_file(Uuid::new_v4(), root, "Beta.txt");
		let alpha = test_file(Uuid::new_v4(), sub.uuid, "alpha.txt");
		let uebung = test_file(Uuid::new_v4(), sub.uuid, "Übung.txt");
		state.upsert_dirs(std::iter::once(&sub)).unwrap();
		state
			.upsert_files([&beta, &alpha, &uebung].into_iter())
			.unwrap();
		Fixture {
			path,
			root,
			sub,
			beta,
			alpha,
			uebung,
			_state: state,
		}
	}

	fn filter(config: &SearchConfig) -> CompiledFilter {
		CompiledFilter::compile(config)
	}

	fn result_names(results: &[SearchHit]) -> Vec<String> {
		results
			.iter()
			.map(|hit| hit.result.name().to_string())
			.collect()
	}

	fn parent_paths(results: &[SearchHit]) -> Vec<&str> {
		results.iter().map(|hit| hit.parent_path()).collect()
	}

	#[test]
	fn account_scope_orders_dirs_first_then_case_folded_names() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let filter = filter(&SearchConfig::new());

		let results =
			window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
		assert_eq!(
			result_names(&results),
			vec!["sub", "alpha.txt", "Beta.txt", "Übung.txt"],
			"dir first; then alpha < Beta under ASCII case folding; non-ASCII after"
		);
		assert_eq!(
			count_results(&conn, Scope::Account(fixture.root), &filter).unwrap(),
			4
		);
	}

	#[test]
	fn subtree_and_children_scopes_bound_the_results() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let filter = filter(&SearchConfig::new());

		let subtree =
			window_results(&conn, Scope::Subtree(fixture.sub.uuid), &filter, &(0..10)).unwrap();
		assert_eq!(result_names(&subtree), vec!["alpha.txt", "Übung.txt"]);

		let children =
			window_results(&conn, Scope::Children(fixture.root), &filter, &(0..10)).unwrap();
		assert_eq!(
			result_names(&children),
			vec!["sub", "Beta.txt"],
			"direct children only"
		);
		assert_eq!(
			count_results(&conn, Scope::Children(fixture.root), &filter).unwrap(),
			2
		);
	}

	#[test]
	fn needle_and_type_filters_apply_in_sql() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();

		let insensitive = filter(&SearchConfig::new().with_name("ÜBUNG"));
		let results =
			window_results(&conn, Scope::Account(fixture.root), &insensitive, &(0..10)).unwrap();
		assert_eq!(result_names(&results), vec!["Übung.txt"]);

		let sensitive = filter(
			&SearchConfig::new()
				.with_name("beta")
				.with_case_sensitive(true),
		);
		assert_eq!(
			count_results(&conn, Scope::Account(fixture.root), &sensitive).unwrap(),
			0,
			"case-sensitive mode matches raw bytes only"
		);

		let dirs_only = filter(&SearchConfig::new().with_item_type(SearchItemType::Dir));
		let results =
			window_results(&conn, Scope::Account(fixture.root), &dirs_only, &(0..10)).unwrap();
		assert_eq!(result_names(&results), vec!["sub"]);
	}

	/// `window_and_count` serves the window AND the pre-LIMIT total from one scan — and the
	/// total must survive the pages that return no rows (offset beyond the end; an empty
	/// requested range), where it falls back to the dedicated count.
	#[test]
	fn window_and_count_reports_the_total_even_for_empty_pages() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let all = filter(&SearchConfig::new());
		let expected_total = count_results(&conn, Scope::Account(fixture.root), &all).unwrap();
		assert!(expected_total >= 4, "fixture sanity");

		let (page, total) =
			window_and_count(&conn, Scope::Account(fixture.root), &all, &(0..2)).unwrap();
		assert_eq!(page.len(), 2, "a clamped page");
		assert_eq!(total, expected_total, "piggybacked total");

		let (beyond, total) =
			window_and_count(&conn, Scope::Account(fixture.root), &all, &(50..60)).unwrap();
		assert!(beyond.is_empty());
		assert_eq!(
			total, expected_total,
			"fallback total on an out-of-range page"
		);

		let (empty, total) =
			window_and_count(&conn, Scope::Account(fixture.root), &all, &(3..3)).unwrap();
		assert!(empty.is_empty());
		assert_eq!(
			total, expected_total,
			"fallback total on a zero-length range"
		);
	}

	#[test]
	fn case_flag_flip_with_unchanged_needle_is_not_confused_by_the_aux_cache() {
		// The compiled matchers are cached on the NEEDLE binding (SQLite aux data); flipping
		// only the case flag re-runs the same prepared statement with the needle binding
		// unchanged, so the cache survives — and must serve both modes correctly.
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let insensitive = filter(&SearchConfig::new().with_name("beta"));
		let sensitive = filter(
			&SearchConfig::new()
				.with_name("beta")
				.with_case_sensitive(true),
		);

		assert_eq!(
			count_results(&conn, Scope::Account(fixture.root), &insensitive).unwrap(),
			1
		);
		assert_eq!(
			count_results(&conn, Scope::Account(fixture.root), &sensitive).unwrap(),
			0
		);
		assert_eq!(
			count_results(&conn, Scope::Account(fixture.root), &insensitive).unwrap(),
			1
		);
	}

	#[test]
	fn windows_paginate_with_limit_offset_and_clamp() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let filter = filter(&SearchConfig::new());

		let second_page =
			window_results(&conn, Scope::Account(fixture.root), &filter, &(2..4)).unwrap();
		assert_eq!(result_names(&second_page), vec!["Beta.txt", "Übung.txt"]);

		let beyond =
			window_results(&conn, Scope::Account(fixture.root), &filter, &(50..60)).unwrap();
		assert!(beyond.is_empty());

		let empty = window_results(&conn, Scope::Account(fixture.root), &filter, &(3..3)).unwrap();
		assert!(empty.is_empty());
	}

	#[test]
	fn hydration_round_trips_payloads_faithfully() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let filter = filter(&SearchConfig::new());

		let results =
			window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
		let by_uuid = |uuid: Uuid| {
			results
				.iter()
				.find(|hit| hit.result.uuid() == uuid)
				.unwrap()
				.clone()
		};
		assert_eq!(
			by_uuid(fixture.beta.uuid).result,
			SearchResult::File(fixture.beta.clone())
		);
		assert_eq!(
			by_uuid(fixture.alpha.uuid).result,
			SearchResult::File(fixture.alpha.clone())
		);
		assert_eq!(
			by_uuid(fixture.uebung.uuid).result,
			SearchResult::File(fixture.uebung.clone())
		);
		assert_eq!(
			by_uuid(fixture.sub.uuid).result,
			SearchResult::Dir(fixture.sub.clone())
		);
	}

	/// The parent path is the `/`-joined ancestor-dir chain from the search root (exclusive) down
	/// to the parent (inclusive): empty for direct children of the root, the parent's name one
	/// level down, and anchor-relative under a subtree scope.
	#[test]
	fn parent_path_is_relative_to_the_search_root() {
		let fixture = fixture();
		let conn = open_read_connection(&fixture.path).unwrap();
		let filter = filter(&SearchConfig::new());

		let account =
			window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
		let path_of = |uuid: Uuid| {
			account
				.iter()
				.find(|hit| hit.result.uuid() == uuid)
				.unwrap()
				.parent_path()
				.to_string()
		};
		assert_eq!(path_of(fixture.sub.uuid), "", "dir directly under the root");
		assert_eq!(
			path_of(fixture.beta.uuid),
			"",
			"file directly under the root"
		);
		assert_eq!(path_of(fixture.alpha.uuid), "sub", "one level down");
		assert_eq!(path_of(fixture.uebung.uuid), "sub");

		// Anchored at `sub`: its own children sit at the root of the search → empty parent path.
		let subtree =
			window_results(&conn, Scope::Subtree(fixture.sub.uuid), &filter, &(0..10)).unwrap();
		assert_eq!(parent_paths(&subtree), vec!["", ""]);

		// Non-recursive: every result is a direct child of the root.
		let children =
			window_results(&conn, Scope::Children(fixture.root), &filter, &(0..10)).unwrap();
		assert_eq!(parent_paths(&children), vec!["", ""]);
	}

	/// Exercises the recursive climb STEP (more than one level) and the anchor-relative trimming:
	/// a file under root/A/B/C reports "A/B/C" account-scoped, but "C" when anchored at B.
	#[test]
	fn parent_path_joins_multi_level_ancestry() {
		let path = temp_db_path();
		let root = Uuid::new_v4();
		let mut state = CacheState::new_on_path(&path, root);
		let a = test_dir(Uuid::new_v4(), root, "A");
		let b = test_dir(Uuid::new_v4(), a.uuid, "B");
		let c = test_dir(Uuid::new_v4(), b.uuid, "C");
		let deep = test_file(Uuid::new_v4(), c.uuid, "deep.txt");
		state.upsert_dirs([&a, &b, &c].into_iter()).unwrap();
		state.upsert_files(std::iter::once(&deep)).unwrap();

		let conn = open_read_connection(&path).unwrap();
		let filter = filter(&SearchConfig::new().with_name("deep.txt"));

		let account = window_results(&conn, Scope::Account(root), &filter, &(0..10)).unwrap();
		assert_eq!(result_names(&account), vec!["deep.txt"]);
		assert_eq!(
			account[0].parent_path(),
			"A/B/C",
			"full chain from the account root"
		);

		let from_b = window_results(&conn, Scope::Subtree(b.uuid), &filter, &(0..10)).unwrap();
		assert_eq!(from_b[0].parent_path(), "C", "relative to the anchor B");
	}
}
