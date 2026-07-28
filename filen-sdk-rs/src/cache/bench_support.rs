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
