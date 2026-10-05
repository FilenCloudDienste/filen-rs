#![warn(unreachable_pub, unused_qualifications)]

// Apply-path surface for the criterion insertion benchmark (`benches/cache_insertion.rs`); gated so
// it never widens the real API.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
mod error;
// The sync engine's cache read side: a consistent whole-subtree snapshot of a sync root + its
// watermark. Gated on `sync-engine` (its only reader — otherwise the whole module is dead code)
// and native only: it opens its own read-only connection, like search, which the wasm
// single-connection VFS does not support, and the sync engine is native-only anyway.
#[cfg(all(
	feature = "sync-engine",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
mod enumerate;
mod handle;
// UniFFI exports on mobile, wasm-bindgen twins on web. The twins share method names, which is
// only sound because the two never compile together (`uniffi` is a native-only dependency).
#[cfg(any(feature = "uniffi", all(target_family = "wasm", target_os = "unknown")))]
pub mod js_impl;
pub mod search;
mod sql;
mod state;

// `CacheControlMessage` is the INTERNAL worker control protocol — the public API exposes it only
// through `Client::{configure_cache, add_sync_root, flush_cache}` and
// `SyncRootHandle::{evict, update_list_dir_recursive}` + its `Drop`, never as a constructible type.
#[cfg(all(
	feature = "sync-engine",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
pub(crate) use enumerate::{
	RemoteItem, SnapshotSink, hydrate_by_uuids, hydrate_cached_items_at, read_ancestors,
};
#[cfg(all(test, feature = "sync-engine"))]
pub(crate) use enumerate::{read_subtree_snapshot, read_subtree_snapshot_into};
pub(crate) use handle::CacheSlot;
#[cfg(all(
	feature = "sync-engine",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
pub(crate) use sql::UndecodableItem;
pub(crate) use state::{CacheControlMessage, CacheState};
pub use {
	error::CacheError,
	handle::{CacheMessage, ResyncProgress, SyncRootHandle},
	search::{
		Search, SearchConfig, SearchHit, SearchItemType, SearchResult, SearchSnapshot,
		SearchWindowCallback, SearchWindowHandle,
	},
	state::{
		CacheEvent, CacheEventType, DirEvent, FileEvent, GlobalEvent, RootKey, SyncRootCallback,
	},
};

#[cfg(test)]
pub(crate) mod test_support {
	use std::{
		fs::{self, OpenOptions},
		io::{Seek, SeekFrom, Write},
		path::Path,
	};

	/// Overwrite the one place `from` sits in the database file at `path` with `to`, behind every
	/// open connection's back.
	///
	/// Neither the file's change counter nor the WAL index moves, so SQLite has no reason to doubt a
	/// page a connection already holds: one that still caches the page keeps answering `from`, and
	/// only one that has given its page cache back reads `to` off the file. That is how a test tells
	/// a released page cache from a kept one, which rusqlite has no safe way to measure (it does not
	/// wrap `sqlite3_db_status`). The page has to be in the main file, not the WAL, so a test
	/// checkpoints before it reads the page in.
	///
	/// Only the read after the release under test is worth asserting. The bundled SQLite shares one
	/// page cache among every connection in the process (it is built with
	/// `SQLITE_ENABLE_MEMORY_MANAGEMENT`), so a release by any other test running alongside evicts
	/// the page too: a read asserting the page is STILL cached is a race with those tests.
	pub(crate) fn rewrite_behind_sqlite(path: &Path, from: &[u8], to: &[u8]) {
		assert_eq!(from.len(), to.len(), "a rewrite in place keeps the length");
		let bytes = fs::read(path).expect("reading the database file");
		let found: Vec<usize> = bytes
			.windows(from.len())
			.enumerate()
			.filter_map(|(at, window)| (window == from).then_some(at))
			.collect();
		let [at] = found[..] else {
			panic!(
				"{:?} sits {} times in the database file, not once",
				String::from_utf8_lossy(from),
				found.len()
			);
		};
		let mut file = OpenOptions::new()
			.write(true)
			.open(path)
			.expect("opening the database file");
		file.seek(SeekFrom::Start(
			u64::try_from(at).expect("a file offset fits in a u64"),
		))
		.expect("seeking to the page");
		file.write_all(to).expect("rewriting the page");
	}
}
