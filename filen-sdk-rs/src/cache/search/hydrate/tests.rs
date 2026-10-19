use std::borrow::Cow;

use chrono::DateTime;
use filen_types::{crypto::Blake3Hash, fs::StableUuid};
use uuid::Uuid;

use crate::cache::{
	CacheState,
	sql::columns::{PRAGMA_CACHE_SIZE, PRAGMA_MMAP_SIZE, PRAGMA_TEMP_STORE},
};

use super::super::config::{SearchConfig, SearchItemType};
use super::*;

/// A read connection holds no more in memory than the worker's does: the PRAGMAs that bound it
/// are per connection, so the opener has to apply them itself.
#[test]
fn a_read_connection_opens_with_the_cache_memory_bounds() {
	let path = temp_db_path();
	let _state = CacheState::new_on_path(&path, Uuid::new_v4());
	let conn = open_read_connection(&path).unwrap();

	let pragma = |name: &str| -> i64 {
		conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(name))
			.unwrap()
	};
	// A 4 MiB page cache (a negative `cache_size` is KiB), no memory map, and TEMP storage in a
	// file (1 = FILE).
	assert_eq!(pragma(PRAGMA_CACHE_SIZE), -4096, "page-cache budget");
	assert_eq!(pragma(PRAGMA_MMAP_SIZE), 0, "no memory map");
	assert_eq!(pragma(PRAGMA_TEMP_STORE), 1, "TEMP storage in a file");
}

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
		stable_uuid: StableUuid::new_for_test(uuid),
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
		key: FileKey::from_str_with_version(&"b".repeat(64), FileEncryptionVersion::V3).unwrap(),
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

	let results = window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
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

	let children = window_results(&conn, Scope::Children(fixture.root), &filter, &(0..10)).unwrap();
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

/// Between an edit's `fileTrash`/`fileArchived` and the paired `fileNew`, a tracked row sits
/// under the SUCCESSOR uuid still carrying the PREDECESSOR's chunks/size/key. Handing that out
/// hands out a file that cannot be downloaded — the successor's bytes under the old key — so
/// every scope must refuse it, and take it back the moment the successor's own record lands.
#[test]
fn a_row_mid_supersede_is_refused_until_the_successor_lands() {
	let path = temp_db_path();
	let root = Uuid::new_v4();
	let mut state = CacheState::new_on_path(&path, root);
	let original = test_file(Uuid::new_v4(), root, "edited.txt");
	state.upsert_files(std::iter::once(&original)).unwrap();

	let successor_uuid = Uuid::new_v4();
	state
		.supersede_file_uuid(original.uuid, successor_uuid)
		.unwrap();

	let conn = open_read_connection(&path).unwrap();
	let filter = filter(&SearchConfig::new());
	let scopes = [
		Scope::Account(root),
		Scope::Subtree(root),
		Scope::Children(root),
	];
	for scope in scopes {
		assert!(
			window_results(&conn, scope, &filter, &(0..10))
				.unwrap()
				.is_empty(),
			"a mid-supersede row must not be readable ({scope:?})"
		);
		assert_eq!(
			count_results(&conn, scope, &filter).unwrap(),
			0,
			"and must not be counted either ({scope:?})"
		);
	}

	// The paired `fileNew`: the successor's own record, which is exactly what the marker was
	// waiting for.
	let mut successor = original.clone();
	successor.uuid = successor_uuid;
	state.upsert_files(std::iter::once(&successor)).unwrap();
	for scope in scopes {
		assert_eq!(
			result_names(&window_results(&conn, scope, &filter, &(0..10)).unwrap()),
			vec!["edited.txt"],
			"the successor's record clears the marker ({scope:?})"
		);
	}
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

	let beyond = window_results(&conn, Scope::Account(fixture.root), &filter, &(50..60)).unwrap();
	assert!(beyond.is_empty());

	let empty = window_results(&conn, Scope::Account(fixture.root), &filter, &(3..3)).unwrap();
	assert!(empty.is_empty());
}

#[test]
fn hydration_round_trips_payloads_faithfully() {
	let fixture = fixture();
	let conn = open_read_connection(&fixture.path).unwrap();
	let filter = filter(&SearchConfig::new());

	let results = window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
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

	let account = window_results(&conn, Scope::Account(fixture.root), &filter, &(0..10)).unwrap();
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
	let children = window_results(&conn, Scope::Children(fixture.root), &filter, &(0..10)).unwrap();
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
