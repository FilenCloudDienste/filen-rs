//! Apply-path surface for the criterion insertion benchmark (`benches/cache_insertion.rs`).
//!
//! Gated behind the `bench-internals` feature so the otherwise-`pub(crate)` [`CacheState`] and its
//! bulk upsert never leak into the supported API. A thin [`BenchCache`] newtype wraps `CacheState`
//! (rather than re-exporting it `pub`, which would widen the real surface).

use std::borrow::Cow;
use std::path::Path;

use chrono::Utc;
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion};
use uuid::Uuid;

use crate::crypto::file::FileKey;
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

/// A representative cached file (15 columns, a real V3 `FileKey` so the blake3 fingerprint is paid).
pub fn cacheable_file(parent: Uuid) -> CacheableFile<'static> {
	let now = Utc::now();
	CacheableFile {
		uuid: Uuid::new_v4(),
		parent,
		chunks_size: 1024,
		chunks: 1,
		favorited: false,
		region: Cow::Borrowed("us-east-1"),
		bucket: Cow::Borrowed("bench-bucket"),
		timestamp: now,
		name: Cow::Borrowed("bench_file.txt"),
		size: 1024,
		mime: Cow::Borrowed("text/plain"),
		key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3).unwrap(),
		last_modified: now,
		created: Some(now),
		hash: None,
	}
}

/// A representative cached dir.
pub fn cacheable_dir(parent: Uuid) -> CacheableDir<'static> {
	let now = Utc::now();
	CacheableDir {
		uuid: Uuid::new_v4(),
		parent,
		color: DirColor::Default,
		favorited: false,
		timestamp: now,
		name: Cow::Borrowed("bench_dir"),
		created: Some(now),
	}
}
