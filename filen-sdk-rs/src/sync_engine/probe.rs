//! What one sync pass costs, phase by phase, with no account and no network.
//!
//! The harness is permanent on purpose: every optimization is measured against the numbers this
//! produces, on the same shapes, so two runs diff. It builds a local tree in a temp directory, a
//! cache DB through [`cache::bench_support`](crate::cache::bench_support), and a baseline DB, then
//! times each step a real pass runs, in the order `prepare` runs them.
//!
//! Driven by `tests/sync_engine_probe.rs` through the one entry point, [`run`], and scaled by two
//! env vars:
//!
//! - `SYNC_PROBE_N` — how many nodes the tree should hold (the shape rounds up to the next whole
//!   branching factor).
//! - `SYNC_PROBE_SHAPE` — `<files per leaf>,<directory levels>,<bytes per file>`, default
//!   `20,3,73`.
//!
//! Gated on `bench-internals` exactly like [`cache::bench_support`](crate::cache::bench_support),
//! and NOT on `cfg(test)`: `cfg(test)` holds only while the library compiles its own unit tests,
//! and the `tests/` binary links the library compiled without it, so a `cfg(test)` seam would be
//! invisible to the very file that drives it.

use std::{
	borrow::Cow,
	collections::{BTreeSet, HashMap, HashSet},
	fmt::Write as _,
	fs,
	path::{Path, PathBuf},
	time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor, auth::FileEncryptionVersion, crypto::Blake3Hash, fs::StableUuid,
};
use uuid::Uuid;

use crate::{
	cache::bench_support::{self, BenchCache},
	crypto::file::FileKey,
	fs::{dir::cache::CacheableDir, file::cache::CacheableFile},
};

use super::{
	SyncMode,
	baseline::{BaselineChange, BaselineEntry, BaselineState, BaselineStore, NodeKind},
	ignore::{IgnoreRules, parse_user_ignore},
	plan::{self, PassHolds, RemoteNode},
	scan::{self, LocalNode, LocalScan, RuleFiles, ScanDepth},
};

/// Node count when `SYNC_PROBE_N` is unset — small enough to run on a laptop in seconds.
const DEFAULT_N: usize = 10_000;

/// `<files per leaf>,<directory levels>,<bytes per file>` when `SYNC_PROBE_SHAPE` is unset. These
/// are the shape every recorded measurement so far was taken on; changing them makes new runs
/// incomparable with old ones.
const DEFAULT_SHAPE: &str = "20,3,73";

/// How many rows the per-action write phase writes at most. One autocommit transaction per row is
/// the phase's whole point, and at a million rows it runs for ten minutes; the cost is per row, so
/// a sample measures the same constant. The detail column names the sample size.
const PER_ACTION_SAMPLE: usize = 20_000;

/// The user-level rules the filtered view build is measured with — five patterns over the built-in
/// defaults, the shape a real pair carries.
const PROBE_RULES: &str = "*.log\nbuild/\n*.tmp\nnode_modules/\n*.bak\n";

/// The tree's shape, as `SYNC_PROBE_SHAPE` gives it.
struct Shape {
	files_per_leaf: usize,
	depth: usize,
	file_bytes: usize,
}

impl Shape {
	fn from_env() -> Self {
		let raw = std::env::var("SYNC_PROBE_SHAPE").unwrap_or_else(|_| DEFAULT_SHAPE.to_owned());
		let mut parts = raw.split(',').map(|part| {
			part.trim().parse::<usize>().unwrap_or_else(|e| {
				panic!("SYNC_PROBE_SHAPE is <files per leaf>,<levels>,<bytes>: {raw:?} ({e})")
			})
		});
		let shape = Self {
			files_per_leaf: parts.next().unwrap_or(20),
			depth: parts.next().unwrap_or(3),
			file_bytes: parts.next().unwrap_or(73),
		};
		assert!(
			shape.depth > 0 && shape.files_per_leaf > 0,
			"SYNC_PROBE_SHAPE needs at least one level and one file per leaf: {raw:?}"
		);
		shape
	}

	/// How many nodes a tree with branching factor `b` holds: every directory level plus the files
	/// in the leaves.
	fn nodes_at(&self, b: usize) -> usize {
		let mut level = 1usize;
		let mut dirs = 0usize;
		for _ in 0..self.depth {
			level *= b;
			dirs += level;
		}
		dirs + level * self.files_per_leaf
	}

	/// The smallest branching factor whose tree holds at least `n` nodes.
	fn branching_for(&self, n: usize) -> usize {
		(1..)
			.find(|b| self.nodes_at(*b) >= n)
			.expect("a tree that large exists")
	}
}

/// The temp tree, its cache DB and its baseline DB.
struct Fixture {
	dir: PathBuf,
	root: PathBuf,
	cache_db: PathBuf,
	baseline_db: PathBuf,
	remote_root: Uuid,
	dirs: usize,
	files: usize,
}

impl Fixture {
	fn nodes(&self) -> usize {
		self.dirs + self.files
	}

	/// Write the tree to disk AND the matching remote mirror into a cache DB, so the two sides are
	/// converged by construction and a pass over them plans nothing.
	fn build(n: usize, shape: &Shape) -> Self {
		let dir = std::env::temp_dir().join(format!("filen_sync_probe_{}", Uuid::new_v4()));
		let root = dir.join("root");
		fs::create_dir_all(&root).expect("creating the probe root");
		let remote_root = Uuid::new_v4();

		let mut builder = TreeBuilder {
			branching: shape.branching_for(n),
			shape,
			// Parsed once: `from_str_with_version` pays a key derivation, and the fixture needs one
			// key per file only because the column is not nullable.
			key: FileKey::from_str_with_version(&"a".repeat(64), FileEncryptionVersion::V3)
				.expect("the probe's v3 file key parses"),
			timestamp: Utc::now(),
			dirs: Vec::new(),
			files: Vec::new(),
		};
		builder.level(&root, remote_root, 1);
		let (dirs, files) = (builder.dirs, builder.files);

		let cache_db = dir.join("cache.db");
		let mut cache = BenchCache::open(&cache_db, remote_root);
		cache.upsert(&dirs, &files);
		// Fold the WAL back in, so the snapshot phase measures the read and not the populate's
		// leftovers.
		cache.checkpoint();
		drop(cache);

		Self {
			root,
			cache_db,
			baseline_db: dir.join("baseline.db"),
			remote_root,
			dirs: dirs.len(),
			files: files.len(),
			dir,
		}
	}
}

/// Builds the on-disk tree and its cache rows in one walk, so the two cannot drift.
struct TreeBuilder<'a> {
	shape: &'a Shape,
	branching: usize,
	key: FileKey,
	timestamp: DateTime<Utc>,
	dirs: Vec<CacheableDir<'static>>,
	files: Vec<CacheableFile<'static>>,
}

impl TreeBuilder<'_> {
	fn level(&mut self, path: &Path, parent: Uuid, level: usize) {
		if level > self.shape.depth {
			self.leaf_files(path, parent);
			return;
		}
		for index in 0..self.branching {
			let name = format!("dir_{index:06}");
			let child = path.join(&name);
			fs::create_dir(&child).expect("creating a probe directory");
			let uuid = Uuid::new_v4();
			self.dirs.push(CacheableDir {
				uuid,
				parent,
				color: DirColor::Default,
				favorited: false,
				timestamp: self.timestamp,
				name: Cow::Owned(name),
				created: Some(self.timestamp),
			});
			self.level(&child, uuid, level + 1);
		}
	}

	fn leaf_files(&mut self, path: &Path, parent: Uuid) {
		for index in 0..self.shape.files_per_leaf {
			let name = format!("file_{index:06}.dat");
			let content = self.content();
			fs::write(path.join(&name), &content).expect("writing a probe file");
			let uuid = Uuid::new_v4();
			self.files.push(CacheableFile {
				uuid,
				stable_uuid: stable_uuid(uuid),
				parent,
				chunks_size: content.len() as u64,
				chunks: 1,
				favorited: false,
				region: Cow::Borrowed("probe-region"),
				bucket: Cow::Borrowed("probe-bucket"),
				timestamp: self.timestamp,
				name: Cow::Owned(name),
				size: content.len() as u64,
				mime: Cow::Borrowed("application/octet-stream"),
				key: self.key,
				last_modified: self.timestamp,
				created: Some(self.timestamp),
				// The real hash of the bytes on disk: the local scan computes the same one, which
				// is what makes the two sides converged and the reconcile a genuine no-op.
				hash: Some(Blake3Hash::from(*blake3::hash(&content).as_bytes())),
			});
		}
	}

	/// Distinct content per file, padded to the configured size. Below ~35 bytes the discriminating
	/// prefix is truncated away and files start repeating content, which only matters if a run
	/// asks for files that small.
	fn content(&self) -> Vec<u8> {
		let mut content =
			format!("filen sync engine probe file {:012}\n", self.files.len()).into_bytes();
		content.resize(self.shape.file_bytes, b'.');
		content
	}
}

/// Mint the fixture's stable ids through serde, the sanctioned entrance: `StableUuid` has no
/// constructor outside the `test-seams` feature, which only dev-dependencies enable — and this
/// module is part of the library.
fn stable_uuid(uuid: Uuid) -> StableUuid {
	serde_json::from_value(serde_json::Value::String(uuid.to_string()))
		.expect("a uuid string deserializes as a stable uuid")
}

/// One TSV line per phase, so two runs diff.
struct Probe {
	n: usize,
	out: String,
}

impl Probe {
	fn record(&mut self, phase: &str, items: usize, elapsed: Duration, detail: &str) {
		let micros_per_item = if items == 0 {
			0.0
		} else {
			elapsed.as_secs_f64() * 1e6 / items as f64
		};
		writeln!(
			self.out,
			"{phase}\t{}\t{items}\t{:.3}\t{micros_per_item:.3}\t{:.1}\t{detail}",
			self.n,
			elapsed.as_secs_f64() * 1e3,
			peak_rss_bytes() as f64 / (1024.0 * 1024.0),
		)
		.expect("writing to a String never fails");
	}
}

/// Peak resident set size of this process so far, in bytes.
///
/// `getrusage(RUSAGE_SELF).ru_maxrss` is the kernel's own high-water mark: an external sampler
/// misses every spike shorter than its interval, and the peak of a pass is exactly such a spike.
/// macOS reports bytes, Linux kibibytes. Returns 0 where the call is unavailable.
#[cfg(unix)]
fn peak_rss_bytes() -> u64 {
	// The real `struct rusage` is two `timeval`s (16 bytes each on every 64-bit target: macOS pads
	// its 32-bit `suseconds_t` up, Linux has a 64-bit one) followed by 14 `c_long`s, of which
	// `ru_maxrss` is the first. The trailing array is only here so the struct cannot be smaller
	// than the one the kernel writes.
	#[repr(C)]
	struct Rusage {
		user_time: [i64; 2],
		system_time: [i64; 2],
		max_rss: i64,
		rest: [i64; 15],
	}
	unsafe extern "C" {
		fn getrusage(who: i32, usage: *mut Rusage) -> i32;
	}
	let mut usage = Rusage {
		user_time: [0; 2],
		system_time: [0; 2],
		max_rss: 0,
		rest: [0; 15],
	};
	// SAFETY: `getrusage` writes exactly one `struct rusage` through the pointer and reads nothing
	// else; `Rusage` is `repr(C)` and at least as large as the platform's, and the local outlives
	// the call. `RUSAGE_SELF` is 0 on macOS, Linux and the BSDs.
	if unsafe { getrusage(0, &mut usage) } != 0 {
		return 0;
	}
	let max_rss = usage.max_rss.max(0) as u64;
	if cfg!(target_os = "macos") {
		max_rss
	} else {
		max_rss * 1024
	}
}

#[cfg(not(unix))]
fn peak_rss_bytes() -> u64 {
	0
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, Duration) {
	let start = Instant::now();
	let value = f();
	(value, start.elapsed())
}

fn env_usize(key: &str, default: usize) -> usize {
	std::env::var(key)
		.ok()
		.map(|raw| {
			raw.trim()
				.parse()
				.unwrap_or_else(|e| panic!("{key} must be a number: {raw:?} ({e})"))
		})
		.unwrap_or(default)
}

fn probe_rules() -> IgnoreRules {
	IgnoreRules::new(Some(
		parse_user_ignore(PROBE_RULES).expect("the probe's user patterns compile"),
	))
}

/// A fully synced baseline for the scanned tree: what a pair looks like the pass after it
/// converged, which is the state every steady-state measurement is taken in.
fn baseline_rows(scan: &LocalScan, remote: &HashMap<String, RemoteNode>) -> Vec<BaselineEntry> {
	scan.nodes
		.values()
		.map(|node| {
			let remote_node = remote.get(&node.rel_path);
			BaselineEntry {
				rel_path: node.rel_path.clone(),
				kind: node.kind,
				remote_uuid: remote_node.map(|r| r.remote_uuid),
				content_hash: node.content_hash,
				size: (node.kind == NodeKind::File).then_some(node.size),
				local_mtime: Some(node.mtime_millis),
				remote_modified: remote_node.map(|r| r.modified_millis),
				state: BaselineState::Synced,
				local_kind: None,
				remote_kind: None,
				remote_hash: None,
				remote_size: None,
				remote_stable_uuid: remote_node.and_then(|r| r.stable_uuid),
				agreed_hash: node.content_hash,
			}
		})
		.collect()
}

/// Diverge the local files in the half-open range `from..to` of the map's iteration order from
/// their baseline rows, the way an edit would. The order is stable while nothing is inserted, so
/// successive calls dirty successive files and the percentages accumulate.
fn dirty_local(local: &mut HashMap<String, LocalNode>, from: usize, to: usize) {
	for (index, node) in local
		.values_mut()
		.filter(|node| node.kind == NodeKind::File)
		.skip(from)
		.take(to.saturating_sub(from))
		.enumerate()
	{
		node.size += 1;
		node.content_hash = Some(Blake3Hash::from([(index % 251) as u8; 32]));
	}
}

/// Everything a pass does locally, end to end, on inputs it re-reads itself: the phases above in
/// the order `prepare` runs them, minus the network and the apply. Returns the action count.
fn pass_pure(fixture: &Fixture, store: &BaselineStore, pair: i64, rules: &IgnoreRules) -> usize {
	let baseline: HashMap<String, BaselineEntry> = store
		.entries(pair)
		.expect("reading the baseline")
		.into_iter()
		.map(|entry| (entry.rel_path.clone(), entry))
		.collect();
	let snapshot = bench_support::snapshot(&fixture.cache_db, fixture.remote_root)
		.expect("reading the cache snapshot");
	let raw_view = plan::build_remote_view(
		fixture.remote_root,
		&snapshot.dirs,
		&snapshot.files,
		&snapshot.undecodable,
		None,
	);
	let (scan, rules_used) = scan::scan_local(
		&fixture.root,
		&baseline,
		ScanDepth::Fast,
		probe_rules(),
		RuleFiles::Read,
	);
	drop(rules_used);
	drop(raw_view);
	let view = plan::build_remote_view(
		fixture.remote_root,
		&snapshot.dirs,
		&snapshot.files,
		&snapshot.undecodable,
		Some(rules),
	);
	let mut baseline = baseline;
	let mut local = scan.nodes;
	let mut remote = view.nodes;
	let held = HashSet::new();
	plan::fold_dir_moves(
		SyncMode::TwoWay,
		&mut baseline,
		&mut local,
		&mut remote,
		&held,
	);
	plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&local,
		&remote,
		&PassHolds::default(),
	)
	.actions
	.len()
}

/// Run every phase once and return the TSV. One header comment naming the shape, then one line per
/// phase: `phase, n, items, ms, us_per_item, peak_rss_mib, detail`.
#[must_use]
pub fn run() -> String {
	let n = env_usize("SYNC_PROBE_N", DEFAULT_N);
	let shape = Shape::from_env();

	let (fixture, build_time) = timed(|| Fixture::build(n, &shape));
	let nodes = fixture.nodes();
	let mut probe = Probe {
		n,
		out: String::new(),
	};
	writeln!(
		probe.out,
		"# sync-engine probe: n={n} shape={},{},{} nodes={nodes} dirs={} files={} os={} \
		 fixture_build_s={:.1}",
		shape.files_per_leaf,
		shape.depth,
		shape.file_bytes,
		fixture.dirs,
		fixture.files,
		std::env::consts::OS,
		build_time.as_secs_f64(),
	)
	.expect("writing to a String never fails");
	writeln!(
		probe.out,
		"#phase\tn\titems\tms\tus_per_item\tpeak_rss_mib\tdetail"
	)
	.expect("writing to a String never fails");

	// The scan with nothing in the baseline: every file misses the fast path and is hashed.
	let no_baseline = HashMap::new();
	let ((cold_scan, _), cold) = timed(|| {
		scan::scan_local(
			&fixture.root,
			&no_baseline,
			ScanDepth::Fast,
			probe_rules(),
			RuleFiles::Read,
		)
	});
	probe.record(
		"scan_cold",
		nodes,
		cold,
		"empty baseline, so every file is hashed",
	);
	assert!(
		cold_scan.complete && cold_scan.errors.is_empty(),
		"the probe tree must scan cleanly: {:?}",
		cold_scan.errors
	);

	// The snapshot SQL — the one serial step of every pass that no earlier measurement covered.
	let (snapshot, first) = timed(|| {
		bench_support::snapshot(&fixture.cache_db, fixture.remote_root).expect("snapshot")
	});
	probe.record(
		"snapshot_sql_first",
		nodes,
		first,
		"read_subtree_snapshot, first read after the populate",
	);
	let (again, warm) = timed(|| {
		bench_support::snapshot(&fixture.cache_db, fixture.remote_root).expect("snapshot")
	});
	probe.record(
		"snapshot_sql_warm",
		nodes,
		warm,
		"read_subtree_snapshot, page cache warm",
	);
	drop(again);

	let rules = probe_rules();
	let (raw_view, raw) = timed(|| {
		plan::build_remote_view(
			fixture.remote_root,
			&snapshot.dirs,
			&snapshot.files,
			&snapshot.undecodable,
			None,
		)
	});
	probe.record("view_raw", nodes, raw, "build_remote_view, no rules");
	let (view, filtered) = timed(|| {
		plan::build_remote_view(
			fixture.remote_root,
			&snapshot.dirs,
			&snapshot.files,
			&snapshot.undecodable,
			Some(&rules),
		)
	});
	probe.record(
		"view_filtered",
		nodes,
		filtered,
		"build_remote_view, 5 user rules + defaults",
	);
	probe.record(
		"view_both",
		nodes,
		raw + filtered,
		"derived: what one pass actually builds",
	);

	// The baseline writes, batched and per action.
	let entries = baseline_rows(&cold_scan, &raw_view.nodes);
	drop(cold_scan);
	drop(raw_view);
	let store = BaselineStore::open(&fixture.baseline_db).expect("opening the baseline DB");
	let local_root = fixture.root.to_string_lossy().into_owned();
	let (pair, _) = store
		.create_pair(&local_root, fixture.remote_root, SyncMode::TwoWay)
		.expect("registering the probe pair");
	let changes: Vec<BaselineChange<'_>> = entries.iter().map(BaselineChange::Upsert).collect();
	let (_, batched) = timed(|| {
		store
			.apply_changes(pair, &changes)
			.expect("writing the baseline in one transaction")
	});
	probe.record(
		"write_batched",
		entries.len(),
		batched,
		"apply_changes, one transaction",
	);
	drop(changes);

	let (scratch, _) = store
		.create_pair(
			&format!("{local_root}#scratch"),
			fixture.remote_root,
			SyncMode::TwoWay,
		)
		.expect("registering the per-action pair");
	let sample = entries.len().min(PER_ACTION_SAMPLE);
	let (_, per_action) = timed(|| {
		for entry in &entries[..sample] {
			store
				.upsert_entry(scratch, entry)
				.expect("writing one baseline row");
		}
	});
	probe.record(
		"write_per_action",
		sample,
		per_action,
		&format!(
			"upsert_entry, one autocommit transaction each, {sample} of {} rows",
			entries.len()
		),
	);
	store
		.delete_pair(scratch)
		.expect("dropping the scratch pair");
	drop(entries);

	let (baseline, read) = timed(|| {
		store
			.entries(pair)
			.expect("reading the baseline")
			.into_iter()
			.map(|entry| (entry.rel_path.clone(), entry))
			.collect::<HashMap<String, BaselineEntry>>()
	});
	probe.record(
		"baseline_read",
		baseline.len(),
		read,
		"entries() -> Vec -> HashMap",
	);

	let ((warm_scan, _), warm_time) = timed(|| {
		scan::scan_local(
			&fixture.root,
			&baseline,
			ScanDepth::Fast,
			probe_rules(),
			RuleFiles::Read,
		)
	});
	probe.record("scan_warm", nodes, warm_time, "fast path, no re-hashing");
	probe.record(
		"hashing_only",
		fixture.files,
		cold.saturating_sub(warm_time),
		"derived: cold scan - warm scan",
	);

	let mut baseline = baseline;
	let mut local = warm_scan.nodes;
	let mut remote = view.nodes;
	let held = HashSet::new();
	let (moves, fold) = timed(|| {
		plan::fold_dir_moves(
			SyncMode::TwoWay,
			&mut baseline,
			&mut local,
			&mut remote,
			&held,
		)
	});
	probe.record(
		"fold_dir_moves_zero",
		fixture.dirs,
		fold,
		&format!("{} moves found", moves.len()),
	);

	let holds = PassHolds::default();
	let (converged, reconcile_0) =
		timed(|| plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds));
	probe.record(
		"reconcile_0pct",
		nodes,
		reconcile_0,
		&format!("{} actions", converged.actions.len()),
	);
	assert!(
		converged.actions.is_empty(),
		"the probe fixture must be converged, so the 0 % reconcile is a real no-op: {:?}",
		&converged.actions[..converged.actions.len().min(3)]
	);

	let one_percent = fixture.files / 100;
	dirty_local(&mut local, 0, one_percent);
	let (plan_1, reconcile_1) =
		timed(|| plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds));
	probe.record(
		"reconcile_1pct",
		nodes,
		reconcile_1,
		&format!(
			"{} files changed, {} actions",
			one_percent,
			plan_1.actions.len()
		),
	);

	let ten_percent = fixture.files / 10;
	dirty_local(&mut local, one_percent, ten_percent);
	let (plan_10, reconcile_10) =
		timed(|| plan::reconcile(SyncMode::TwoWay, &baseline, &local, &remote, &holds));
	probe.record(
		"reconcile_10pct",
		nodes,
		reconcile_10,
		&format!(
			"{} files changed, {} actions",
			ten_percent,
			plan_10.actions.len()
		),
	);

	// The whole local half of a pass, on inputs it re-reads itself — so this line's peak RSS is a
	// pass's peak, not the sum of the phases above.
	drop((
		converged, plan_1, plan_10, baseline, local, remote, snapshot,
	));
	let (actions, whole) = timed(|| pass_pure(&fixture, &store, pair, &rules));
	probe.record(
		"pass_pure",
		nodes,
		whole,
		&format!(
			"{actions} actions; baseline read + snapshot + both views + warm scan + fold + \
			 reconcile, no network and no apply"
		),
	);

	// What `untrack_ignored` pays per pass at the shape the built-in rules produce on macOS: a
	// `.DS_Store` root per synced directory, so G = D and not one of them matches a row. Measured
	// after the pass phases because it is the pass's last step.
	let roots: BTreeSet<String> = store
		.entries(pair)
		.expect("reading the baseline")
		.into_iter()
		.filter(|entry| entry.kind == NodeKind::Dir)
		.map(|entry| format!("{}/.DS_Store", entry.rel_path))
		.collect();
	let root_count = roots.len();
	let (_, untrack) = timed(|| {
		store
			.delete_subtrees(pair, &roots)
			.expect("untracking the ignored roots")
	});
	probe.record(
		"untrack_ignored",
		root_count,
		untrack,
		"delete_subtrees, one untracked .DS_Store root per synced directory (G = D), no row hit",
	);

	drop(store);
	fs::remove_dir_all(&fixture.dir).ok();
	probe.out
}
