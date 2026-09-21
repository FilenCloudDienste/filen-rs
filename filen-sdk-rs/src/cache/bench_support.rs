//! Apply-path surface for the criterion insertion benchmark (`benches/cache_insertion.rs`).
//!
//! Gated behind the `bench-internals` feature so the otherwise-`pub(crate)` [`CacheState`] and its
//! bulk upsert never leak into the supported API. A thin [`BenchCache`] newtype wraps `CacheState`
//! (rather than re-exporting it `pub`, which would widen the real surface).

use std::path::Path;

use uuid::Uuid;

use crate::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};

use super::state::CacheState;

/// Owns a file-backed [`CacheState`] for the insertion benchmark.
pub struct BenchCache(CacheState);

impl BenchCache {
	/// Open a fresh cache DB at `path` with `root` as the account root (runs schema init).
	pub fn open(path: &Path, root: Uuid) -> Self {
		Self(CacheState::new_on_path(path, root))
	}

	/// The bulk upsert under test: dirs then files, exactly as the resync apply drives it.
	pub fn upsert(&mut self, dirs: &[CacheableDir<'_>], files: &[CacheableFile<'_>]) {
		self.0.upsert_dirs(dirs.iter()).expect("bench upsert_dirs");
		self.0
			.upsert_files(files.iter())
			.expect("bench upsert_files");
	}

	/// Fold the WAL back into the main DB (the post-apply checkpoint a real resync performs). The
	/// larger transaction size shifts work into this fold, so benchmarks track it separately.
	pub fn checkpoint(&mut self) {
		self.0
			.db
			.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
			.expect("bench checkpoint");
	}
}

/// One sync root's cached subtree, read exactly as a sync pass reads it (the `ENUMERATE_SUBTREE`
/// recursive CTE plus the hydration of every row).
///
/// The engine's own entrance, `Client::enumerate_sync_root_snapshot`, needs an authenticated
/// client; this one needs only a populated DB file, which is what lets the sync-engine probe
/// ([`sync_engine::probe`](crate::sync_engine::probe)) measure the one serial SQL step of every
/// pass with no account and no network.
#[cfg(all(
	feature = "sync-engine",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
pub(crate) fn snapshot(
	path: &Path,
	root: Uuid,
) -> rusqlite::Result<super::enumerate::SubtreeSnapshot> {
	super::enumerate::read_subtree_snapshot(path, root)
}

/// The same read STREAMED into `sink` — what a pass does — so the probe can measure the two
/// against each other on one tree. Returns the watermark, as the engine's own entrance does.
#[cfg(all(
	feature = "sync-engine",
	not(all(target_family = "wasm", target_os = "unknown"))
))]
pub(crate) fn snapshot_into(
	path: &Path,
	root: Uuid,
	sink: &mut dyn super::enumerate::SnapshotSink,
) -> rusqlite::Result<Option<u64>> {
	super::enumerate::read_subtree_snapshot_into(path, root, sink)
}
