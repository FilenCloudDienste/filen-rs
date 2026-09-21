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
	collections::{BTreeSet, HashMap},
	fmt::Write as _,
	fs,
	path::{Path, PathBuf},
	sync::Arc,
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
	derive::{self, Derived},
	ignore::{IgnoreRules, parse_user_ignore},
	observe,
	plan::{self, PassHolds, RemoteNode},
	scan::{self, LocalNode, LocalScan, RuleFiles},
	tree::Baseline,
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

/// How many times a phase that is measured AGAINST another one is run, alternating between the
/// two. One ordered sample of each cannot separate an effect of a few per cent from this probe's
/// own run-to-run spread — `write_batched`, with the same index in place both times, moved 20 %
/// between two runs at 100k — and whichever phase runs second reads a warmer cache.
const COMPARE_REPS: usize = 3;

/// The user-level rules the filtered view build is measured with — five patterns over the built-in
/// defaults, the shape a real pair carries.
const PROBE_RULES: &str = "*.log\nbuild/\n*.tmp\nnode_modules/\n*.bak\n";

/// How many registry point reads the control-verb phase makes. One is too few to time on a
/// microsecond clock; the cost is per read, and the phase reports the per-read figure.
const CONTROL_VERB_SAMPLE: usize = 100;

/// How long the contending whole-tree read runs for in the control-verb phase. Long enough that
/// the reads it is measured against certainly overlap it.
const CONTENDING_READ: Duration = Duration::from_millis(500);

/// How long the contended control-WRITE phase keeps issuing whole-tree writes for. Long enough to
/// cover several transactions at 100k rows, so a control write certainly lands inside one; at a
/// million rows a single transaction already outlasts it.
///
/// It has to outlast that transaction's START, not just its length: with the `(pair_id, state)`
/// index in place a 100k re-write takes about 1.6 s, and a 1.5 s window could end while the
/// contending writer was still opening its connection — leaving the phase reporting the
/// UNCONTENDED cost (0.7 ms, seen once) as though it were the wait it exists to find.
const CONTENDED_WRITE: Duration = Duration::from_millis(5_000);

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
		debug_assert!(
			!detail.contains(['\t', '\n']),
			"a detail column with a tab or a newline in it splits the row into more columns than \
			 the header names: {detail:?}"
		);
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

/// The median and the best of a phase's samples: the median is the figure a run reports, the best
/// is the floor the noise sits above. Sorts in place; `samples` must not be empty.
fn median_and_best(samples: &mut [Duration]) -> (Duration, Duration) {
	samples.sort_unstable();
	(samples[samples.len() / 2], samples[0])
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
/// their baseline rows, the way an edit would, and return the paths it diverged. The order is
/// stable while nothing is inserted, so successive calls dirty successive files and the
/// percentages accumulate.
///
/// The paths are what a pass would hold in its changelist for those edits, which is the scope the
/// change-scoped phases reconcile at.
fn dirty_local(local: &mut HashMap<String, LocalNode>, from: usize, to: usize) -> BTreeSet<String> {
	let mut changed = BTreeSet::new();
	for (index, node) in local
		.values_mut()
		.filter(|node| node.kind == NodeKind::File)
		.skip(from)
		.take(to.saturating_sub(from))
		.enumerate()
	{
		node.size += 1;
		node.content_hash = Some(Blake3Hash::from([(index % 251) as u8; 32]));
		changed.insert(node.rel_path.clone());
	}
	changed
}

/// Move every remote node at `from` or under it to the same place under `to`, which is what the
/// view holds after somebody moved that directory on the remote — the input the directory-move
/// fold is measured on.
fn rekey_remote_subtree(remote: &mut HashMap<String, RemoteNode>, from: &str, to: &str) {
	let moving: Vec<(String, String)> = remote
		.keys()
		.filter_map(|path| Some((path.clone(), plan::moved_path(path, from, to)?)))
		.collect();
	for (old, new) in moving {
		let Some(mut node) = remote.remove(&old) else {
			continue;
		};
		node.rel_path = new.clone();
		remote.insert(new, node);
	}
}

/// A synced file row, for a phase that needs rows to exist and nothing more of them.
fn plain_row(rel_path: &str) -> BaselineEntry {
	BaselineEntry {
		rel_path: rel_path.to_string(),
		kind: NodeKind::File,
		remote_uuid: None,
		content_hash: None,
		size: Some(0),
		local_mtime: Some(0),
		remote_modified: Some(0),
		state: BaselineState::Synced,
		local_kind: None,
		remote_kind: None,
		remote_hash: None,
		remote_size: None,
		remote_stable_uuid: None,
		agreed_hash: None,
	}
}

/// Everything a pass does locally, end to end, on inputs it re-reads itself: the phases above in
/// the order `prepare` runs them, minus the network and the apply. Returns the action count.
fn pass_pure(fixture: &Fixture, store: &BaselineStore, pair: i64, rules: &IgnoreRules) -> usize {
	let baseline = store.baseline(pair).expect("reading the baseline");
	let snapshot = bench_support::snapshot(&fixture.cache_db, fixture.remote_root)
		.expect("reading the cache snapshot");
	let mut view = plan::place_remote_items(
		fixture.remote_root,
		&snapshot.dirs,
		&snapshot.files,
		&snapshot.undecodable,
	);
	let (scan, rules_used) =
		scan::scan_local(&fixture.root, &baseline, probe_rules(), RuleFiles::Read);
	drop(rules_used);
	view.filter(Some(plan::ViewFilter {
		rules,
		baseline: &baseline,
	}));
	let mut baseline = baseline;
	let mut local = scan.nodes;
	let mut remote = view.nodes;
	let held = BTreeSet::new();
	plan::fold_dir_moves(
		SyncMode::TwoWay,
		&mut baseline,
		&mut local,
		&mut remote,
		&held,
		plan::PassPaths::Whole,
	);
	plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&local,
		&remote,
		&PassHolds::default(),
		plan::PassPaths::Whole,
	)
	.actions
	.len()
}

/// What a CHANGE-SCOPED pass does locally, end to end: the resident baseline, the two maps derived
/// from its rows, the re-observation of the dirty paths, the directory-move fold and the reconcile
/// over the complete maps.
///
/// The fold is here because `prepare_scoped` ends with it exactly as `prepare_whole` does, and
/// because [`pass_pure`] pays it: a phase that skipped it would credit change-scoping with the cost
/// of a step the real pass still runs every time.
///
/// What a whole pass does and this does not: the cache snapshot, both view builds, and the walk of
/// the tree. What it leaves out that a real pass does is applying the announced remote changes,
/// which costs one map operation per announced change and nothing per tree node.
///
/// Returns the action count, so a phase that was supposed to plan something can say whether it did.
fn pass_scoped(
	fixture: &Fixture,
	store: &BaselineStore,
	pair: i64,
	dirty: BTreeSet<String>,
) -> usize {
	let mut baseline = store.baseline(pair).expect("reading the baseline");
	let mut derived = derive::from_baseline(&baseline, dirty.clone());
	let (observations, _rules) = observe::observe_local(
		&fixture.root,
		&baseline,
		probe_rules(),
		&RuleFiles::Read,
		&dirty,
	);
	derive::merge_local(&mut derived, &baseline, &observations);
	let Derived {
		mut local,
		mut remote,
		held,
		mut decided,
		..
	} = derived;
	// The held paths are the rows that record one side only, which is what the pass folds with too
	// (they reach it as `PassHolds::held_remote`).
	let moves = plan::fold_dir_moves(
		SyncMode::TwoWay,
		&mut baseline,
		&mut local,
		&mut remote,
		&held,
		plan::PassPaths::Changed(&decided),
	);
	// The decided set follows the fold, as `Prepared::fold_dir_moves` makes it follow for a pass.
	for action in &moves {
		let (from, to) = action.endpoints();
		decided = decided
			.into_iter()
			.map(|path| plan::moved_path(&path, from, to).unwrap_or(path))
			.collect();
	}
	plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&local,
		&remote,
		&PassHolds::default(),
		plan::PassPaths::Changed(&decided),
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
	let no_baseline = Baseline::default();
	let ((cold_scan, _), cold) =
		timed(|| scan::scan_local(&fixture.root, &no_baseline, probe_rules(), RuleFiles::Read));
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
	let (mut view, placed) = timed(|| {
		plan::place_remote_items(
			fixture.remote_root,
			&snapshot.dirs,
			&snapshot.files,
			&snapshot.undecodable,
		)
	});
	probe.record(
		"view_place",
		nodes,
		placed,
		"place_remote_items, the view before the rules",
	);

	// The baseline writes, batched and per action. The rows come off the placed view, which is
	// what a pass records them from.
	let entries = baseline_rows(&cold_scan, &view.nodes);
	drop(cold_scan);

	let (_, hidden) = timed(|| {
		// The fixture's baseline is written further down, and nothing in this tree is hidden by
		// the built-in defaults, so an empty one measures the same filter a pass runs.
		view.filter(Some(plan::ViewFilter {
			rules: &rules,
			baseline: &no_baseline,
		}));
	});
	probe.record(
		"view_filter",
		nodes,
		hidden,
		"RemoteView::filter, 5 user rules + defaults",
	);
	probe.record(
		"view_once",
		nodes,
		placed + hidden,
		"derived: what one pass actually builds",
	);

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

	// The per-action baseline write, with the `(pair_id, state)` index and without it: what the
	// index costs on every row a pass records. Measured ALTERNATELY and several times each, because
	// one ordered sample of each cannot separate an effect of a few per cent from this probe's own
	// run-to-run spread — `write_batched` alone moved 20 % between two runs of it at this size —
	// and whichever of two consecutive phases runs second gets the warmer cache.
	let sample = entries.len().min(PER_ACTION_SAMPLE);
	let mut per_action_indexed: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	let mut per_action_plain: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	for rep in 0..COMPARE_REPS {
		for indexed in [true, false] {
			store
				.set_state_index(indexed)
				.expect("toggling the conflict-state index");
			// A pair of its own per sample, so every sample writes its rows into an empty pair
			// rather than replacing the previous sample's.
			let (scratch, _) = store
				.create_pair(
					&format!("{local_root}#per-action-{rep}-{indexed}"),
					fixture.remote_root,
					SyncMode::TwoWay,
				)
				.expect("registering the per-action pair");
			let (_, elapsed) = timed(|| {
				for entry in &entries[..sample] {
					store
						.upsert_entry(scratch, entry)
						.expect("writing one baseline row");
				}
			});
			let samples = if indexed {
				&mut per_action_indexed
			} else {
				&mut per_action_plain
			};
			samples.push(elapsed);
			store
				.delete_pair(scratch)
				.expect("dropping the scratch pair");
		}
	}
	let (per_action, per_action_best) = median_and_best(&mut per_action_indexed);
	probe.record(
		"write_per_action",
		sample,
		per_action,
		&format!(
			"upsert_entry, one autocommit transaction each, {sample} of {} rows — median of \
			 {COMPARE_REPS} alternating with the no-index phase, best {:.3} ms",
			entries.len(),
			per_action_best.as_secs_f64() * 1e3
		),
	);
	let (per_action_no_index, per_action_no_index_best) = median_and_best(&mut per_action_plain);
	probe.record(
		"write_per_action_no_index",
		sample,
		per_action_no_index,
		&format!(
			"upsert_entry with no (pair_id, state) index, {sample} of {} rows — median of \
			 {COMPARE_REPS}, best {:.3} ms",
			entries.len(),
			per_action_no_index_best.as_secs_f64() * 1e3
		),
	);

	// The same comparison on the BATCHED write — the whole tree in one transaction, which is what
	// a first sync actually pays and what the per-action figure cannot stand in for: that one's
	// cost is dominated by a commit per row. Both of these REWRITE rows already present, so they
	// are comparable to each other rather than to `write_batched` above, which inserted them.
	let changes: Vec<BaselineChange<'_>> = entries.iter().map(BaselineChange::Upsert).collect();
	let mut batched_indexed: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	let mut batched_plain: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	for _ in 0..COMPARE_REPS {
		for indexed in [true, false] {
			store
				.set_state_index(indexed)
				.expect("toggling the conflict-state index");
			let (_, elapsed) = timed(|| {
				store
					.apply_changes(pair, &changes)
					.expect("re-writing the baseline in one transaction")
			});
			let samples = if indexed {
				&mut batched_indexed
			} else {
				&mut batched_plain
			};
			samples.push(elapsed);
		}
	}
	drop(changes);
	let (batched_with_index, batched_with_index_best) = median_and_best(&mut batched_indexed);
	probe.record(
		"write_batched_indexed",
		entries.len(),
		batched_with_index,
		&format!(
			"apply_changes re-writing every row in one transaction — median of {COMPARE_REPS} \
			 alternating with the no-index phase, best {:.1} ms",
			batched_with_index_best.as_secs_f64() * 1e3
		),
	);
	let (batched_no_index, batched_no_index_best) = median_and_best(&mut batched_plain);
	probe.record(
		"write_batched_no_index",
		entries.len(),
		batched_no_index,
		&format!(
			"the same transaction with no (pair_id, state) index — median of {COMPARE_REPS}, best \
			 {:.1} ms",
			batched_no_index_best.as_secs_f64() * 1e3
		),
	);

	// What the index costs to BUILD: every row of the file read and sorted, inside the write lock.
	// The first open of a DB written before the index existed pays this once, and another pair's
	// connection opening at the same moment waits it out on its busy timeout.
	store
		.set_state_index(false)
		.expect("dropping the conflict-state index");
	let (_, index_build) = timed(|| {
		store
			.set_state_index(true)
			.expect("creating the conflict-state index")
	});
	probe.record(
		"state_index_build",
		nodes,
		index_build,
		"CREATE INDEX baseline_state over the whole file — what the first open after it was added \
		 pays, once",
	);

	// The conflict read, both ways round and alternating for the same reason the writes are. It
	// hands back the rows a pair holds in conflict — none here, which is the steady state — so what
	// the two phases differ by is the lookup: a seek of the `(pair_id, state)` index against a walk
	// of every row of the pair. That walk is also how long the read holds the pair's store mutex,
	// which is what a control verb on the same pair waits out.
	let mut conflict_indexed: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	let mut conflict_walked: Vec<Duration> = Vec::with_capacity(COMPARE_REPS);
	for _ in 0..COMPARE_REPS {
		for indexed in [true, false] {
			store
				.set_state_index(indexed)
				.expect("toggling the conflict-state index");
			let (_, elapsed) = timed(|| store.conflicts(pair).expect("reading the conflicts"));
			let samples = if indexed {
				&mut conflict_indexed
			} else {
				&mut conflict_walked
			};
			samples.push(elapsed);
		}
	}
	store
		.set_state_index(true)
		.expect("re-creating the conflict-state index");
	let (conflict_read, _) = median_and_best(&mut conflict_indexed);
	probe.record(
		"conflict_read_indexed",
		nodes,
		conflict_read,
		&format!(
			"conflicts() on the (pair_id, state) index — also the hold it takes on the pair's \
			 mutex; median of {COMPARE_REPS}"
		),
	);
	let (conflict_read_walked, _) = median_and_best(&mut conflict_walked);
	probe.record(
		"conflict_read_no_index",
		nodes,
		conflict_read_walked,
		&format!(
			"conflicts() with the index dropped: every row of the pair; median of {COMPARE_REPS}"
		),
	);

	// A control WRITE on its own connection while another connection is inside a whole-tree
	// transaction. This is the window the control-verb target is about: WAL lets a READER through
	// during a write, so `control_verb_under_read` below can never queue, but a second WRITER waits
	// for the first to commit — and waits inside `sqlite3_step`'s busy handler, not on any lock the
	// engine holds. The contending writer replaces rows that are already there, so the DB does not
	// grow and its transaction is the one `write_batched` timed.
	let (control_write, control_outcome) = {
		let control = BaselineStore::open(&fixture.baseline_db).expect("the control connection");
		let rows = &entries;
		let bulk_db = fixture.baseline_db.clone();
		std::thread::scope(|scope| {
			// Sampled over a sustained window and reported as the WORST case, not timed once: a
			// single sample races the contending transaction's first statement, and a control
			// write issued in the moment before the write lock is taken measures the uncontended
			// cost (70 us here) rather than the wait this phase exists to find.
			let deadline = Instant::now() + CONTENDED_WRITE;
			let writing = scope.spawn(move || {
				let bulk =
					BaselineStore::open(&bulk_db).expect("the contending writer's connection");
				let changes: Vec<BaselineChange<'_>> =
					rows.iter().map(BaselineChange::Upsert).collect();
				let mut transactions = 0usize;
				// One transaction at a million rows outlasts the window by itself, which is the
				// point: the window bounds when sampling STARTS, not how long one write takes.
				while Instant::now() < deadline {
					bulk.apply_changes(pair, &changes)
						.expect("the contending whole-tree write");
					transactions += 1;
				}
				transactions
			});
			let mut worst = Duration::ZERO;
			let mut samples = 0usize;
			let mut failed = None;
			while Instant::now() < deadline {
				// The flag alternates so no sample can be a write the planner shortcuts.
				let (outcome, verb) = timed(|| control.set_paused(pair, samples.is_multiple_of(2)));
				worst = worst.max(verb);
				samples += 1;
				// Reported rather than unwrapped: a transaction longer than the busy timeout is
				// exactly what this phase exists to find, and a panicking probe would hide it.
				if let Err(error) = outcome {
					failed = Some(error.to_string());
					break;
				}
			}
			let transactions = writing.join().expect("the contending writer thread");
			control.set_paused(pair, false).expect("unpausing the pair");
			let outcome = match failed {
				None => format!(
					"worst of {samples} control write(s) against {transactions} whole-tree \
					 transaction(s)"
				),
				Some(error) => format!("FAILED after {samples} sample(s): {error}"),
			};
			(worst, outcome)
		})
	};
	probe.record(
		"control_write_under_write",
		1,
		control_write,
		&format!(
			"set_paused on its own connection while another connection held the write lock — \
			 {control_outcome}"
		),
	);

	// What the directory-rename phase at the end needs, taken from the rows that are already here.
	// Reading the baseline back for it down there instead would allocate a second whole-tree `Vec`
	// at a point where this run's peak RSS is the number a memory target gets read from — so the
	// harness would be reporting its own measurement.
	let rename_root = entries
		.iter()
		.find(|entry| entry.kind == NodeKind::Dir && !entry.rel_path.contains('/'))
		.map(|entry| entry.rel_path.clone())
		.expect("the probe tree must have a top-level directory");
	let rename_rows = {
		let under = format!("{rename_root}/");
		entries
			.iter()
			.filter(|entry| entry.rel_path == rename_root || entry.rel_path.starts_with(&under))
			.count()
	};
	// Every top-level directory, for the whole-tree rename phase: renaming all of them in one
	// transaction re-keys every row of the pair, which is what renaming the pair root does. A
	// handful of names, taken here for the same reason `rename_root` is.
	let top_level: Vec<String> = entries
		.iter()
		.filter(|entry| entry.kind == NodeKind::Dir && !entry.rel_path.contains('/'))
		.map(|entry| entry.rel_path.clone())
		.collect();
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
		"entries() -> Vec -> HashMap, the read a pass used to pay per pass",
	);
	drop(baseline);

	// What a pass reads now: the same work on the pair's FIRST read, and an `Arc` clone on every
	// one after it — which is what an idle pass pays instead of the line above.
	let (first_read, resident_cold) = timed(|| store.baseline(pair).expect("reading the baseline"));
	probe.record(
		"baseline_resident_cold",
		first_read.len(),
		resident_cold,
		"baseline(), first read of the pair: the SELECT and the tree build",
	);
	drop(first_read);
	let (baseline, resident_warm) = timed(|| store.baseline(pair).expect("reading the baseline"));
	probe.record(
		"baseline_resident_warm",
		baseline.len(),
		resident_warm,
		"baseline(), resident: an Arc clone",
	);
	// What the resident copy COSTS, which no timing shows: the bytes the tree holds for the life of
	// the pair, summed from the structures themselves (see `Baseline::resident_bytes`).
	let resident_bytes = baseline.resident_bytes();
	probe.record(
		"baseline_resident_bytes",
		baseline.len(),
		Duration::ZERO,
		&format!(
			"{:.1} MiB resident, {:.0} B/row computed from nodes + names + children + indexes",
			resident_bytes as f64 / (1024.0 * 1024.0),
			resident_bytes as f64 / baseline.len().max(1) as f64,
		),
	);

	// What ONE path-keyed lookup costs in each shape, over the same rows in the same run — the only
	// comparison this machine's run-to-run spread cannot blur. The map hands back a reference and
	// the tree builds the row it hands back, which is what the callers need either way, so this is
	// what a caller pays for one path in each shape.
	let paths: Vec<String> = baseline.paths().collect();
	let map: HashMap<String, BaselineEntry> = store
		.entries(pair)
		.expect("reading the baseline")
		.into_iter()
		.map(|entry| (entry.rel_path.clone(), entry))
		.collect();
	let (_, map_lookups) = timed(|| {
		for path in &paths {
			std::hint::black_box(map.get(path));
		}
	});
	probe.record(
		"lookup_map",
		paths.len(),
		map_lookups,
		"HashMap::get per path — the shape a pass read before the tree",
	);
	drop(map);
	let (_, tree_lookups) = timed(|| {
		for path in &paths {
			std::hint::black_box(baseline.get(path));
		}
	});
	probe.record(
		"lookup_tree",
		paths.len(),
		tree_lookups,
		"Baseline::get per path, resolved from the root every time",
	);
	let (_, cursor_lookups) = timed(|| {
		let mut rows = baseline.cursor();
		for path in &paths {
			std::hint::black_box(rows.get(path));
		}
	});
	probe.record(
		"lookup_cursor",
		paths.len(),
		cursor_lookups,
		"Baseline::cursor in path order — the shape the scan and the reconcile ask in",
	);
	drop(paths);

	// And what it costs the pass that WRITES: the first row written while the pass holds the copy
	// clones the whole map once (`Arc::make_mut`), every row after it lands in place. The row is
	// one the pair already holds, written back unchanged, so the tree stays converged.
	let first_row = baseline
		.get(&rename_root)
		.expect("the rename root has a baseline row");
	let (written, first_write) = timed(|| store.upsert_entry(pair, &first_row));
	written.expect("re-writing a row the pair already holds");
	probe.record(
		"baseline_first_write",
		baseline.len(),
		first_write,
		"upsert while a pass holds the copy: one Arc::make_mut clone of the whole tree",
	);

	let ((warm_scan, _), warm_time) =
		timed(|| scan::scan_local(&fixture.root, &baseline, probe_rules(), RuleFiles::Read));
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
	let held = BTreeSet::new();
	let (moves, fold) = timed(|| {
		plan::fold_dir_moves(
			SyncMode::TwoWay,
			&mut baseline,
			&mut local,
			&mut remote,
			&held,
			plan::PassPaths::Whole,
		)
	});
	probe.record(
		"fold_dir_moves_zero",
		fixture.dirs,
		fold,
		&format!("{} moves found", moves.len()),
	);

	// The same fold at the scope a change-scoped pass gives it. The pair is converged, so both find
	// nothing — what the pair of lines says is what LOOKING costs, which is what every pass pays
	// whether or not anything moved.
	let idle_scope: BTreeSet<String> = local.keys().next().cloned().into_iter().collect();
	let (scoped_moves, scoped_fold) = timed(|| {
		plan::fold_dir_moves(
			SyncMode::TwoWay,
			&mut baseline,
			&mut local,
			&mut remote,
			&held,
			plan::PassPaths::Changed(&idle_scope),
		)
	});
	probe.record(
		"fold_dir_moves_scoped_zero",
		idle_scope.len(),
		scoped_fold,
		&format!("{} moves found, one changed path", scoped_moves.len()),
	);

	// The widest single move a pass can fold: a whole top-level directory moved on the remote, so
	// one action carries every row under it. Measured at both scopes over the same subtree in the
	// same run — the remote's copy is moved to the other name before each, so the second fold
	// carries the directory back and the three structures end where they started.
	//
	// The store held the other handle on the resident tree until `baseline_first_write` above took
	// its own copy, so this detaching write is free; it is here so that a run which ever stops
	// being true cannot charge a whole-tree clone to the fold.
	let _ = Arc::make_mut(&mut baseline);
	let moved_root = format!("{rename_root}-moved");
	let move_scope = BTreeSet::from([rename_root.clone(), moved_root.clone()]);
	let mut remote_at = rename_root.clone();
	for (phase, scope) in [
		("fold_dir_moves_dir_whole", plan::PassPaths::Whole),
		(
			"fold_dir_moves_dir_scoped",
			plan::PassPaths::Changed(&move_scope),
		),
	] {
		let to = if remote_at == rename_root {
			moved_root.clone()
		} else {
			rename_root.clone()
		};
		rekey_remote_subtree(&mut remote, &remote_at, &to);
		remote_at = to;
		let (dir_moves, dir_fold) = timed(|| {
			plan::fold_dir_moves(
				SyncMode::TwoWay,
				&mut baseline,
				&mut local,
				&mut remote,
				&held,
				scope,
			)
		});
		assert_eq!(
			dir_moves.len(),
			1,
			"{phase}: the moved directory must fold as exactly one move"
		);
		probe.record(
			phase,
			rename_rows,
			dir_fold,
			"one directory move carrying a whole top-level subtree",
		);
	}
	assert_eq!(
		remote_at, rename_root,
		"the two folds must leave the tree where they found it"
	);

	// Where the reconcile's time actually goes. The phase below times the whole of it, which cannot
	// say whether the cost is the KEY SET it builds before deciding anything or the per-path
	// decisions themselves — and those two are removed by different changes, so a run that cannot
	// separate them cannot say whether either worked.
	let (visited, walked) = timed(|| {
		let mut rows = 0usize;
		baseline.visit_row_paths(|_| rows += 1);
		rows
	});
	probe.record(
		"reconcile_keys_walk",
		visited,
		walked,
		"visit_row_paths alone: every row's path against one reused buffer",
	);
	let (key_count, keys_built) = timed(|| {
		let mut keys: BTreeSet<Cow<'_, str>> = local
			.keys()
			.chain(remote.keys())
			.map(|path| Cow::Borrowed(path.as_str()))
			.collect();
		baseline.visit_row_paths(|path| {
			if !local.contains_key(path) && !remote.contains_key(path) {
				keys.insert(Cow::Owned(path.to_string()));
			}
		});
		keys.len()
	});
	probe.record(
		"reconcile_keys_build",
		key_count,
		keys_built,
		"the sorted union of the three sides, as reconcile builds it",
	);

	let holds = PassHolds::default();
	// What a change-scoped pass would name as changed, growing with each `dirty_local` below. The
	// twin phases reconcile the SAME maps at that scope, so the pair of lines is the whole claim:
	// same inputs, same run, one deciding every path and one deciding what moved.
	let mut changed_paths: BTreeSet<String> = BTreeSet::new();
	let (converged, reconcile_0) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Whole,
		)
	});
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

	let (dirty_0, reconcile_dirty_0) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Changed(&changed_paths),
		)
	});
	probe.record(
		"reconcile_dirty_0pct",
		nodes,
		reconcile_dirty_0,
		&format!(
			"{} changed path(s), {} actions",
			changed_paths.len(),
			dirty_0.actions.len()
		),
	);
	// No equality assertion here, deliberately: with an empty key set on a converged fixture both
	// plans are empty whatever the reconcile does, so the assertion would be `[] == []` and could
	// not fail. This phase is a timing floor — what an empty scope costs — and the 1 % and 10 %
	// twins below are what carry the claim that a narrowed reconcile plans what a whole one plans.

	let one_percent = fixture.files / 100;
	changed_paths.extend(dirty_local(&mut local, 0, one_percent));
	let (plan_1, reconcile_1) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Whole,
		)
	});
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

	let (dirty_1, reconcile_dirty_1) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Changed(&changed_paths),
		)
	});
	probe.record(
		"reconcile_dirty_1pct",
		nodes,
		reconcile_dirty_1,
		&format!(
			"{} changed path(s), {} actions",
			changed_paths.len(),
			dirty_1.actions.len()
		),
	);
	assert_eq!(
		dirty_1.actions, plan_1.actions,
		"the narrowed reconcile must plan what the whole one plans, at this scale too"
	);

	let ten_percent = fixture.files / 10;
	changed_paths.extend(dirty_local(&mut local, one_percent, ten_percent));
	let (plan_10, reconcile_10) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Whole,
		)
	});
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

	let (dirty_10, reconcile_dirty_10) = timed(|| {
		plan::reconcile(
			SyncMode::TwoWay,
			&baseline,
			&local,
			&remote,
			&holds,
			plan::PassPaths::Changed(&changed_paths),
		)
	});
	probe.record(
		"reconcile_dirty_10pct",
		nodes,
		reconcile_dirty_10,
		&format!(
			"{} changed path(s), {} actions",
			changed_paths.len(),
			dirty_10.actions.len()
		),
	);
	assert_eq!(
		dirty_10.actions, plan_10.actions,
		"the narrowed reconcile must plan what the whole one plans, at this scale too"
	);

	// The whole local half of a pass, on inputs it re-reads itself — so this line's peak RSS is a
	// pass's peak, not the sum of the phases above.
	drop((
		converged, plan_1, plan_10, baseline, local, remote, snapshot,
	));
	drop((dirty_0, dirty_1, dirty_10, changed_paths));
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

	// The same tree read the other way: ONE file changed, and only that file's path in the dirty
	// set. This is the phase the whole change-scoped design exists for, so it is measured against
	// `pass_pure` directly above it, on the same fixture, in the same run.
	let changed = store
		.entries(pair)
		.expect("reading the baseline")
		.into_iter()
		.find(|entry| entry.kind == NodeKind::File)
		.expect("the probe tree holds files")
		.rel_path;
	fs::write(fixture.root.join(&changed), b"changed by the probe")
		.expect("changing one probe file");
	let dirty = BTreeSet::from([changed.clone()]);
	let (scoped_actions, scoped) = timed(|| pass_scoped(&fixture, &store, pair, dirty.clone()));
	probe.record(
		"pass_one_file_changed",
		nodes,
		scoped,
		&format!(
			"{scoped_actions} action(s) for 1 changed file ({changed}); baseline + derived maps + \
			 one re-observed path + fold + reconcile, no walk and no snapshot"
		),
	);
	assert_eq!(
		scoped_actions, 1,
		"one changed file must plan exactly one action, or this phase is timing the wrong thing"
	);

	// The phase the one-per-cent target is read off: a real change-scoped pass, with one per cent
	// of the files edited on disk and named in its dirty set. `pass_one_file_changed` above is the
	// same machinery at its floor and `pass_pure` the whole-read yardstick — same fixture, same
	// run, so the three numbers are comparable.
	let percent_changed: Vec<String> = store
		.entries(pair)
		.expect("reading the baseline")
		.into_iter()
		.filter(|entry| entry.kind == NodeKind::File)
		.take((fixture.files / 100).max(1))
		.map(|entry| entry.rel_path)
		.collect();
	for rel_path in &percent_changed {
		fs::write(
			fixture.root.join(rel_path),
			b"changed by the probe, one per cent of the tree",
		)
		.expect("changing a probe file");
	}
	let percent_dirty: BTreeSet<String> = percent_changed.iter().cloned().collect();
	let (percent_actions, percent) =
		timed(|| pass_scoped(&fixture, &store, pair, percent_dirty.clone()));
	probe.record(
		"pass_one_percent_changed",
		nodes,
		percent,
		&format!(
			"{percent_actions} action(s) for {} changed file(s); baseline + derived maps + \
			 re-observation + fold + reconcile, no walk and no snapshot",
			percent_changed.len()
		),
	);
	assert_eq!(
		percent_actions,
		percent_changed.len(),
		"one action per changed file, or this phase is timing the wrong thing"
	);

	// The floor the same machinery costs with NOTHING dirty. The engine does not pay it — a wake
	// with an empty change list returns before it reads anything — so this is the cost that
	// skipping an idle wake avoids, not a cost any pass incurs.
	let (idle_actions, idle) = timed(|| pass_scoped(&fixture, &store, pair, BTreeSet::new()));
	probe.record(
		"pass_scoped_idle_floor",
		nodes,
		idle,
		&format!(
			"{idle_actions} action(s); what a change-scoped pass over an EMPTY dirty set would \
			 cost — `run_pass` skips it outright, so no pass pays this"
		),
	);
	assert_eq!(
		idle_actions, 0,
		"a converged pair with nothing dirty must plan nothing, or this phase is timing a pass that \
		 stopped deciding paths"
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

	// A directory rename, which re-keys every row of that directory's subtree in one transaction —
	// the widest single write a pass makes, and what a pair-root rename is made of.
	let renamed = format!("{rename_root}-renamed");
	let (_, rename) = timed(|| {
		store
			.apply_changes(
				pair,
				&[BaselineChange::MoveSubtree {
					from: &rename_root,
					to: &renamed,
				}],
			)
			.expect("renaming a top-level directory")
	});
	probe.record(
		"dir_rename_subtree",
		rename_rows,
		rename,
		"apply_changes(MoveSubtree), one transaction, every row under one top-level directory",
	);

	// The whole tree re-keyed in ONE transaction: every row of the pair, which is what renaming the
	// pair root costs — measured rather than extrapolated from the single subtree above, because
	// the per-row cost of an `UPDATE OR REPLACE` over the primary key grows with the rows already
	// re-keyed in the same transaction. The `-wal` file after it is the other half of that cost:
	// those pages are written twice, once here and once at the checkpoint. `dir_rename_subtree`
	// has already renamed one of these directories, so that one is named by where it now is.
	let renames: Vec<(String, String)> = top_level
		.iter()
		.map(|name| {
			let from = if *name == rename_root {
				renamed.clone()
			} else {
				name.clone()
			};
			let to = format!("{from}-w");
			(from, to)
		})
		.collect();
	let whole_tree: Vec<BaselineChange<'_>> = renames
		.iter()
		.map(|(from, to)| BaselineChange::MoveSubtree {
			from: from.as_str(),
			to: to.as_str(),
		})
		.collect();
	let (_, whole_rename) = timed(|| {
		store
			.apply_changes(pair, &whole_tree)
			.expect("renaming every top-level directory in one transaction")
	});
	let wal_mib = fs::metadata(PathBuf::from(format!(
		"{}-wal",
		fixture.baseline_db.display()
	)))
	.map(|meta| meta.len() as f64 / (1024.0 * 1024.0))
	.unwrap_or(0.0);
	probe.record(
		"dir_rename_whole_tree",
		nodes,
		whole_rename,
		&format!(
			"apply_changes(MoveSubtree) for all {} top-level directories in one transaction, \
			 -wal {wal_mib:.1} MiB after it",
			renames.len()
		),
	);

	// A control verb on ANOTHER pair while this pair's whole-tree read is in flight. Each pair has
	// its own connection, so the two never queue on one mutex; what is left to measure is SQLite's
	// own concurrency, which is what WAL buys. The contending reader opens its own handle to the
	// same file — exactly what another pair's store is.
	let reader_db = fixture.baseline_db.clone();
	let reading = std::thread::spawn(move || {
		let reader = BaselineStore::open(&reader_db).expect("the contending reader's connection");
		let mut reads = 0usize;
		let until = Instant::now() + CONTENDING_READ;
		while Instant::now() < until {
			// `conflicts` rather than `entries`: same connection, same store lock, but only the
			// few rows a pair holds come back, so a whole-tree `Vec` per read — which at a
			// million rows would dominate the peak this harness reports — is not part of what
			// this phase measures. Now that the read seeks the `(pair_id, state)` index, what
			// sustains the contention is the LOOP rather than one long scan; a reader never
			// blocks a reader under WAL, so the point-read figure below is unchanged by that.
			reader.conflicts(pair).expect("scanning for held conflicts");
			reads += 1;
		}
		reads
	});
	let control = BaselineStore::open(&fixture.baseline_db).expect("the control connection");
	let (_, verb) = timed(|| {
		for _ in 0..CONTROL_VERB_SAMPLE {
			control.pair(pair).expect("reading the pair registry");
		}
	});
	let reads = reading.join().expect("the contending reader thread");
	probe.record(
		"control_verb_under_read",
		CONTROL_VERB_SAMPLE,
		verb,
		&format!(
			"pair() on its own connection, while another connection ran {reads} whole-tree \
			 scan(s) of the same file"
		),
	);

	// What dropping a pair costs: one DELETE plus the `ON DELETE CASCADE` over every row it owns.
	// It is the other long write a single connection makes — the one the 30 s busy timeout has to
	// outlast for another pair's record write to land rather than fail — and it was unmeasured.
	let (_, cascade) = timed(|| store.delete_pair(pair).expect("deleting the probe pair"));
	probe.record(
		"pair_delete_cascade",
		nodes,
		cascade,
		"delete_pair: one DELETE and the ON DELETE CASCADE over the pair's rows",
	);

	// What a row costs in a directory that holds very many of them. A node's children are ONE `Vec`
	// of ids kept sorted by the folding comparator, so writing a row memmoves every id that sorts
	// after it. Nothing a converged pass does hits that, but a subtree move writes one row per row
	// it carries into the destination directory, so a move into a flat directory pays this per row
	// — which is the ceiling that answers "what if somebody keeps 100k files in one folder".
	//
	// Last in the run, and on a tree of its own, because every phase above reports the process's
	// peak RSS: a structure built earlier would raise that high-water mark for all of them and make
	// this run's memory column incomparable with the ones already recorded.
	const FLAT_CHILDREN: usize = 100_000;
	const FLAT_INSERTS: usize = 1_000;
	let mut flat =
		Baseline::from_rows((0..FLAT_CHILDREN).map(|index| plain_row(&format!("flat/{index:07}"))));
	let (_, flat_inserts) = timed(|| {
		// `!` sorts before every digit, so each of these lands at the FRONT of the child vector:
		// the worst position, and the one a name-ordered walk into a full directory keeps hitting.
		for index in 0..FLAT_INSERTS {
			flat.upsert(&plain_row(&format!("flat/!{index:06}")));
		}
	});
	probe.record(
		"baseline_insert_flat_dir",
		FLAT_INSERTS,
		flat_inserts,
		&format!("one row at the front of a directory holding {FLAT_CHILDREN} children"),
	);
	drop(flat);

	drop(store);
	fs::remove_dir_all(&fixture.dir).ok();
	probe.out
}
