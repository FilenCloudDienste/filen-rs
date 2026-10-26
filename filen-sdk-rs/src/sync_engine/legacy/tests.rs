use std::{
	io::Write,
	path::PathBuf,
	sync::{Arc, Mutex, PoisonError},
};

use rusqlite::params;
use tempfile::TempDir;
use tracing::{Dispatch, callsite::rebuild_interest_cache, subscriber::NoSubscriber};

use super::*;
use crate::sync_engine::SyncMode;

const STAMP: i64 = 1_700_000_000_123;
const SENTINEL: &str = "sentinel-key-6b1f0c";

fn uuid(n: u128) -> Uuid {
	Uuid::from_u128(n)
}

fn store_with_pair() -> (BaselineStore, PairId) {
	let store = BaselineStore::open_in_memory().unwrap();
	let (pair, _) = store
		.create_pair("/root", uuid(1), SyncMode::TwoWay)
		.unwrap();
	(store, pair)
}

/// `text` as a JSON string, as `JSON.stringify` quotes a path.
fn json(text: &str) -> String {
	serde_json::to_string(text).unwrap()
}

/// A line of `previousLocalTree` for a file, every field the old engine wrote.
fn local_line(prop: &str, size: u64, mtime: i64) -> String {
	let p = json(prop);
	format!(
		r#"{{"prop":{p},"data":{{"lastModified":{mtime},"type":"file","path":{p},"size":{size},"creation":1600000000000,"inode":42}}}}"#
	)
}

/// A line of `previousRemoteTree` for a file, `key` and `hash` included.
fn remote_line(prop: &str, id: Uuid, size: u64) -> String {
	let p = json(prop);
	format!(
		r#"{{"prop":{p},"data":{{"type":"file","uuid":"{id}","name":"n","size":{size},"mime":"application/octet-stream","lastModified":{STAMP},"version":2,"chunks":1,"key":"{SENTINEL}","bucket":"b","region":"r","creation":1,"hash":"abcdef","path":{p}}}}}"#
	)
}

fn local_dir_line(prop: &str) -> String {
	let p = json(prop);
	format!(
		r#"{{"prop":{p},"data":{{"lastModified":{STAMP},"type":"directory","path":{p},"size":0,"creation":1,"inode":7}}}}"#
	)
}

fn remote_dir_line(prop: &str, id: Uuid) -> String {
	let p = json(prop);
	format!(r#"{{"prop":{p},"data":{{"type":"directory","uuid":"{id}","name":"d","path":{p}}}}}"#)
}

/// A state directory holding the two trees, each the given lines joined by `\n`.
fn state_dir(local: &[String], remote: &[String]) -> TempDir {
	let dir = tempfile::tempdir().unwrap();
	write_tree(&dir, LOCAL_TREE, local.join("\n").as_bytes());
	write_tree(&dir, REMOTE_TREE, remote.join("\n").as_bytes());
	dir
}

fn write_tree(dir: &TempDir, name: &str, bytes: &[u8]) {
	let mut file = File::create(dir.path().join(name)).unwrap();
	file.write_all(bytes).unwrap();
	file.write_all(b"\n").unwrap();
}

/// What `pair` holds in `legacy_synced`, in path order.
fn imported_rows(store: &BaselineStore, pair: PairId) -> Vec<(String, Uuid, i64, i64)> {
	store
		.writer()
		.prepare(
			"SELECT rel_path, remote_uuid, size, local_mtime FROM legacy_synced WHERE pair_id = ?1
			 ORDER BY rel_path",
		)
		.unwrap()
		.query_map(params![pair], |row| {
			Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
		})
		.unwrap()
		.collect::<rusqlite::Result<_>>()
		.unwrap()
}

#[test]
fn two_trees_that_list_the_same_files_import_each_of_them() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[
			local_line("/a.txt", 5, STAMP),
			local_dir_line("/docs"),
			local_line("/docs/b.bin", 9, STAMP + 7),
		],
		&[
			remote_line("/a.txt", uuid(10), 5),
			remote_dir_line("/docs", uuid(11)),
			remote_line("/docs/b.bin", uuid(12), 9),
		],
	);
	let import = import(&store, pair, dir.path()).unwrap();
	assert_eq!(
		import,
		LegacyImport {
			imported: 2,
			skipped: 0
		}
	);
	assert_eq!(
		imported_rows(&store, pair),
		vec![
			("a.txt".to_owned(), uuid(10), 5, STAMP),
			("docs/b.bin".to_owned(), uuid(12), 9, STAMP + 7),
		]
	);
	assert_eq!(store.legacy_synced_count(pair).unwrap(), 2);
}

#[test]
fn unreadable_lines_are_skipped_and_counted_and_blank_ones_are_not_counted() {
	let (store, pair) = store_with_pair();
	let dir = tempfile::tempdir().unwrap();
	let mut local = Vec::new();
	for line in [
		local_line("/a.txt", 5, STAMP).as_str(),
		"",
		"   ",
		"not json at all",
		r#"{"prop":"/no-data.txt"}"#,
		r#"{"data":{"type":"file","size":1,"lastModified":1}}"#,
		r#"{"prop":"/null-data.txt","data":null}"#,
		r#"{"prop":7,"data":{"type":"file","size":1,"lastModified":1}}"#,
	] {
		local.extend_from_slice(line.as_bytes());
		local.push(b'\n');
	}
	// Not UTF-8: a lone continuation byte inside the path.
	local.extend_from_slice(b"{\"prop\":\"/\x80.txt\",\"data\":{\"type\":\"file\"}}\n");
	write_tree(&dir, LOCAL_TREE, &local);
	write_tree(
		&dir,
		REMOTE_TREE,
		remote_line("/a.txt", uuid(10), 5).as_bytes(),
	);

	let import = import(&store, pair, dir.path()).unwrap();
	assert_eq!(
		import,
		LegacyImport {
			imported: 1,
			skipped: 6
		},
		"garbage, no data, no prop, null data, a numeric prop and the non-UTF-8 line"
	);
	assert_eq!(
		imported_rows(&store, pair),
		vec![("a.txt".to_owned(), uuid(10), 5, STAMP)],
		"the good line around them still imports"
	);
}

#[test]
fn the_last_line_for_a_path_wins_and_crlf_line_breaks_are_read() {
	let (store, pair) = store_with_pair();
	let dir = tempfile::tempdir().unwrap();
	let local = [
		local_line("/a.txt", 5, STAMP),
		local_line("/a.txt", 6, STAMP + 1),
	]
	.join("\r\n");
	write_tree(&dir, LOCAL_TREE, local.as_bytes());
	let remote = [
		remote_line("/a.txt", uuid(10), 5),
		remote_line("/a.txt", uuid(20), 6),
	]
	.join("\r\n");
	write_tree(&dir, REMOTE_TREE, format!("{remote}\r").as_bytes());

	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 1,
			skipped: 0
		}
	);
	assert_eq!(
		imported_rows(&store, pair),
		vec![("a.txt".to_owned(), uuid(20), 6, STAMP + 1)]
	);
}

#[test]
fn a_directory_or_a_path_only_one_tree_lists_imports_nothing() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[
			local_dir_line("/docs"),
			local_line("/local-only.txt", 3, STAMP),
		],
		&[
			remote_dir_line("/docs", uuid(11)),
			remote_line("/remote-only.txt", uuid(12), 3),
		],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 0
		}
	);
	assert_eq!(imported_rows(&store, pair), vec![]);
}

#[test]
fn a_path_listed_as_a_file_in_one_tree_and_a_directory_in_the_other_imports_nothing() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(&[local_dir_line("/x")], &[remote_line("/x", uuid(12), 0)]);
	assert_eq!(import(&store, pair, dir.path()).unwrap().imported, 0);
}

#[test]
fn a_size_that_differs_between_the_trees_imports_nothing() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[local_line("/a.txt", 5, STAMP)],
		&[remote_line("/a.txt", uuid(10), 6)],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 0
		}
	);
}

#[test]
fn a_float_mtime_skips_the_line_and_imports_nothing() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[
			r#"{"prop":"/a.txt","data":{"lastModified":1700000000123.5,"type":"file","size":5}}"#
				.to_owned(),
		],
		&[remote_line("/a.txt", uuid(10), 5)],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 1
		}
	);
}

#[test]
fn a_remote_line_whose_mtime_this_engine_cannot_read_still_imports() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[
			local_line("/a.txt", 5, STAMP),
			local_line("/b.txt", 5, STAMP),
		],
		&[
			format!(
				r#"{{"prop":"/a.txt","data":{{"type":"file","uuid":"{}","size":5,"lastModified":1700000000123.5,"key":"{SENTINEL}"}}}}"#,
				uuid(10)
			),
			format!(
				r#"{{"prop":"/b.txt","data":{{"type":"file","uuid":"{}","size":5,"lastModified":"yesterday"}}}}"#,
				uuid(11)
			),
		],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 2,
			skipped: 0
		}
	);
	assert_eq!(
		imported_rows(&store, pair),
		vec![
			("a.txt".to_owned(), uuid(10), 5, STAMP),
			("b.txt".to_owned(), uuid(11), 5, STAMP),
		]
	);
}

#[test]
fn a_local_entry_with_no_mtime_or_a_remote_one_with_no_uuid_imports_nothing() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[
			r#"{"prop":"/no-mtime.txt","data":{"type":"file","size":5}}"#.to_owned(),
			local_line("/no-uuid.txt", 5, STAMP),
		],
		&[
			remote_line("/no-mtime.txt", uuid(10), 5),
			r#"{"prop":"/no-uuid.txt","data":{"type":"file","size":5}}"#.to_owned(),
		],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 0
		}
	);
}

#[test]
fn an_nfd_path_is_stored_as_nfc() {
	let (store, pair) = store_with_pair();
	// "é" decomposed: e + U+0301.
	let nfd = "/caf\u{65}\u{301}.txt";
	let dir = state_dir(
		&[local_line(nfd, 5, STAMP)],
		&[remote_line(nfd, uuid(10), 5)],
	);
	assert_eq!(import(&store, pair, dir.path()).unwrap().imported, 1);
	assert_eq!(
		imported_rows(&store, pair),
		vec![("caf\u{e9}.txt".to_owned(), uuid(10), 5, STAMP)]
	);
}

#[test]
fn two_spellings_of_one_nfc_path_import_neither() {
	let (store, pair) = store_with_pair();
	let nfc = "/caf\u{e9}.txt";
	let nfd = "/caf\u{65}\u{301}.txt";
	let dir = state_dir(
		&[local_line(nfc, 5, STAMP), local_line(nfd, 5, STAMP)],
		&[remote_line(nfc, uuid(10), 5)],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 0
		},
		"which of the two local files the remote one was synced with is unknown"
	);

	// Ambiguous for good: the first spelling again does not take it back.
	let dir = state_dir(
		&[
			local_line(nfc, 5, STAMP),
			local_line(nfd, 5, STAMP),
			local_line(nfc, 5, STAMP),
		],
		&[remote_line(nfc, uuid(10), 5)],
	);
	assert_eq!(import(&store, pair, dir.path()).unwrap().imported, 0);
}

#[test]
fn a_path_the_two_trees_spell_differently_imports_nothing() {
	let (store, pair) = store_with_pair();
	let nfc = "/caf\u{e9}.txt";
	let nfd = "/caf\u{65}\u{301}.txt";
	let dir = state_dir(
		&[local_line(nfd, 5, STAMP), local_line("/a.txt", 5, STAMP)],
		&[
			remote_line(nfc, uuid(10), 5),
			remote_line("a.txt", uuid(11), 5),
		],
	);
	assert_eq!(
		import(&store, pair, dir.path()).unwrap(),
		LegacyImport {
			imported: 0,
			skipped: 0
		},
		"the old engine compared the raw paths, so it never synced these two with each other"
	);
}

#[test]
fn a_path_that_names_no_plain_relative_path_is_skipped() {
	for prop in ["/../x", "/a//b", "/./a", "/a/..", "/", "", "//x", "/a/"] {
		let (store, pair) = store_with_pair();
		let dir = state_dir(
			&[local_line(prop, 5, STAMP)],
			&[remote_line(prop, uuid(10), 5)],
		);
		assert_eq!(
			import(&store, pair, dir.path()).unwrap(),
			LegacyImport {
				imported: 0,
				skipped: 2
			},
			"{prop:?}"
		);
	}
}

#[test]
fn a_path_without_a_leading_slash_is_read_as_relative() {
	assert_eq!(rel_key("a/b.txt").as_deref(), Some("a/b.txt"));
	assert_eq!(rel_key("/a/b.txt").as_deref(), Some("a/b.txt"));
}

/// A writer the log lines of a test go into.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
	fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.extend_from_slice(bytes);
		Ok(bytes.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

impl Captured {
	fn text(&self) -> String {
		String::from_utf8(
			self.0
				.lock()
				.unwrap_or_else(PoisonError::into_inner)
				.clone(),
		)
		.unwrap()
	}
}

#[test]
fn the_remote_key_appears_in_no_log_line_report_or_error() {
	let (store, pair) = store_with_pair();
	let logs = Captured::default();
	let subscriber = tracing_subscriber::fmt()
		.with_writer({
			let logs = logs.clone();
			move || logs.clone()
		})
		.with_max_level(tracing::Level::TRACE)
		.finish();

	// A good line, a bad one carrying the key, and a key in a line the path check refuses.
	let dir = state_dir(
		&[
			local_line("/a.txt", 5, STAMP),
			local_line("/../b", 5, STAMP),
		],
		&[
			remote_line("/a.txt", uuid(10), 5),
			format!(r#"{{"prop":"/c","data":{{"type":"file","size":1.5,"key":"{SENTINEL}"}}}}"#),
			remote_line("/../b", uuid(11), 5),
		],
	);
	// While a single dispatcher is registered, tracing caches a callsite's interest from the
	// default of whichever thread reaches it first: another test reaching the warning while this
	// one runs would cache "never" for it, and the lines this test reads would not be logged. With
	// a second one alive, the cache is built from every registered dispatcher instead. A thread
	// that began registering the callsite before the second one was registered still computes its
	// interest the single-dispatcher way and can store it after registering this test's subscriber
	// rebuilt the cache, so the cache is rebuilt once more with both in place.
	let _second = Dispatch::new(NoSubscriber::default());
	let report = tracing::subscriber::with_default(subscriber, || {
		rebuild_interest_cache();
		import(&store, pair, dir.path()).unwrap()
	});
	assert_eq!(
		report,
		LegacyImport {
			imported: 1,
			skipped: 3
		}
	);
	let logged = logs.text();
	assert!(
		logged.contains("skipped line 2 of previousRemoteTree"),
		"the skipped lines must be logged for this to test their log: {logged}"
	);
	assert!(!logged.contains(SENTINEL), "{logged}");
	assert!(!format!("{report:?}").contains(SENTINEL));

	// A tree that fails part-way: the remote one is a directory, which opens but cannot be read.
	let dir = state_dir(&[local_line("/a.txt", 5, STAMP)], &[]);
	std::fs::remove_file(dir.path().join(REMOTE_TREE)).unwrap();
	std::fs::create_dir(dir.path().join(REMOTE_TREE)).unwrap();
	let error = import(&store, pair, dir.path()).unwrap_err();
	assert_eq!(error.kind(), ErrorKind::IO);
	assert!(!format!("{error:?} {error}").contains(SENTINEL));
	assert_eq!(
		imported_rows(&store, pair),
		vec![("a.txt".to_owned(), uuid(10), 5, STAMP)],
		"a failed import leaves the earlier one as it was"
	);
}

#[test]
fn a_missing_tree_file_imports_nothing_and_replaces_an_earlier_import() {
	let (store, pair) = store_with_pair();
	let dir = state_dir(
		&[local_line("/a.txt", 5, STAMP)],
		&[remote_line("/a.txt", uuid(10), 5)],
	);
	assert_eq!(import(&store, pair, dir.path()).unwrap().imported, 1);

	for missing in [LOCAL_TREE, REMOTE_TREE] {
		let dir = state_dir(
			&[local_line("/a.txt", 5, STAMP)],
			&[remote_line("/a.txt", uuid(10), 5)],
		);
		std::fs::remove_file(dir.path().join(missing)).unwrap();
		assert_eq!(
			import(&store, pair, dir.path()).unwrap(),
			LegacyImport {
				imported: 0,
				skipped: 0
			},
			"without {missing}"
		);
		assert_eq!(store.legacy_synced_count(pair).unwrap(), 0);
	}
}

#[test]
fn a_state_directory_that_is_missing_or_a_file_is_an_io_error() {
	let (store, pair) = store_with_pair();
	let dir = tempfile::tempdir().unwrap();
	let missing: PathBuf = dir.path().join("nowhere");
	assert_eq!(
		import(&store, pair, &missing).unwrap_err().kind(),
		ErrorKind::IO
	);
	let file = dir.path().join("a-file");
	File::create(&file).unwrap();
	assert_eq!(
		import(&store, pair, &file).unwrap_err().kind(),
		ErrorKind::IO
	);
}
