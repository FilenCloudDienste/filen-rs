//! Criterion benchmark for the cache bulk-insert (resync apply) path.
//!
//! Requires the `bench-internals` feature, which exposes the otherwise-`pub(crate)` apply surface
//! via [`filen_sdk_rs::cache::bench_support`]:
//!
//! ```sh
//! cargo bench -p filen-sdk-rs --features bench-internals --bench cache_insertion
//! ```
//!
//! Each iteration runs on a FRESH file-backed DB (insertion mutates — iterating one DB would
//! measure `ON CONFLICT DO UPDATE`, not a cold populate; and `:memory:` would hide the WAL /
//! checkpoint behaviour). The synthetic dataset is generated ONCE and reused by reference.

use std::time::Duration;

use std::borrow::Cow;

use chrono::Utc;
use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use filen_sdk_rs::cache::bench_support::BenchCache;
use filen_sdk_rs::crypto::file::FileKey;
use filen_sdk_rs::fs::{dir::cache::CacheableDir, file::cache::CacheableFile};
use filen_types::{api::v3::dir::color::DirColor, auth::FileEncryptionVersion, fs::StableUuid};
use uuid::Uuid;

/// A representative cached file (16 columns, a real V3 `FileKey` so the blake3 fingerprint is paid).
pub fn cacheable_file(parent: Uuid) -> CacheableFile<'static> {
	let now = Utc::now();
	CacheableFile {
		uuid: Uuid::new_v4(),
		stable_uuid: StableUuid::new_for_test(Uuid::new_v4()),
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

// Scaled below the ~166k-item production populate so each sample is a few hundred ms. Raise these
// (and lower `sample_size`) for a realistic-scale run.
const N_DIRS: usize = 6_000;
const N_FILES: usize = 50_000;

fn fresh(root: Uuid) -> (BenchCache, tempfile::TempDir) {
	let dir = tempfile::tempdir().expect("tempdir");
	let cache = BenchCache::open(&dir.path().join("bench.db"), root);
	(cache, dir)
}

fn cache_insertion(c: &mut Criterion) {
	let root = Uuid::new_v4();
	// Generated ONCE (FileKey parsing × N is costly and must not be timed); reused by reference.
	let dirs: Vec<_> = (0..N_DIRS).map(|_| cacheable_dir(root)).collect();
	let files: Vec<_> = (0..N_FILES).map(|_| cacheable_file(root)).collect();

	let mut group = c.benchmark_group("cache_insertion");
	group.throughput(Throughput::Elements((N_DIRS + N_FILES) as u64));
	group.sample_size(20); // each iter is a few hundred ms; the default 100 would be slow
	// `re_upsert` pre-populates in setup, so its iterations are ~2× — give the window room for 20.
	group.measurement_time(Duration::from_secs(8));

	// Cold populate into a fresh file-backed DB — the resync apply we tuned.
	group.bench_function("populate", |b| {
		b.iter_batched(
			|| fresh(root),
			|(mut cache, dir)| {
				cache.upsert(&dirs, &files);
				// Returned so criterion drops it (DB close + temp cleanup) OUTSIDE the timed region.
				(cache, dir)
			},
			BatchSize::PerIteration,
		);
	});

	// Same, plus folding the WAL back — the full insertion cost (the larger transaction size shifts
	// work into this checkpoint, so track it separately from the apply).
	group.bench_function("populate_with_checkpoint", |b| {
		b.iter_batched(
			|| fresh(root),
			|(mut cache, dir)| {
				cache.upsert(&dirs, &files);
				cache.checkpoint();
				(cache, dir)
			},
			BatchSize::PerIteration,
		);
	});

	// Steady state: the DB is pre-populated in (untimed) setup, so every routine row hits
	// `ON CONFLICT DO UPDATE`.
	group.bench_function("re_upsert", |b| {
		b.iter_batched(
			|| {
				let (mut cache, dir) = fresh(root);
				cache.upsert(&dirs, &files);
				(cache, dir)
			},
			|(mut cache, dir)| {
				cache.upsert(&dirs, &files);
				(cache, dir)
			},
			BatchSize::PerIteration,
		);
	});

	group.finish();
}

criterion_group!(benches, cache_insertion);
criterion_main!(benches);
