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
	collections::{BTreeMap, BTreeSet, HashMap},
	fmt::Write as _,
	fs, mem,
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
	engine::{PendingKind, PendingWrites, assembly_accounted},
	ignore::{IgnoreRules, Origin, load_remote_rules, parse_user_ignore, rule_file_dir},
	observe,
	plan::{self, PassHolds, RemoteNode, RemoteView},
	remote::{RemoteObserved, cache_ancestry, observe_remote},
	scan::{self, LocalNode, LocalScan, RuleFiles},
	side::NodesAt,
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

/// What `scoped_pull_one_file_changed` must plan: the probe edits one file locally, and on a
/// `RemoteToLocal` pair the remote copy is authoritative, so the pass plans the one download that
/// puts the file back. Written as a constant so the phase's assertion is a claim about the pass and
/// not about whatever the pass happened to return.
const PULL_ONE_FILE_ACTIONS: usize = 1;

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

/// Where a pass reads its two sides from: the local tree, the cache DB the remote view is built
/// out of, and the root that view is for. A [`Fixture`] hands one out, and so does the spec a
/// child process is measured through — a pass needs these three and nothing else about the tree.
struct PassPlace {
	root: PathBuf,
	cache_db: PathBuf,
	remote_root: Uuid,
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

	fn place(&self) -> PassPlace {
		PassPlace {
			root: self.root.clone(),
			cache_db: self.cache_db.clone(),
			remote_root: self.remote_root,
		}
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

/// What each whole-tree scan of one change-scoped pass cost, timed INSIDE the phase that ran them.
///
/// In-run deltas are the only figures two builds can be compared on: step 1b found a binary-wide
/// systematic worth ~3.5 % between two builds of code that had not changed, which swamps every scan
/// here but the largest. A row recorded from these fields is one process's own accounting of the
/// phase printed beside it, so `sum(scans) < phase` always holds within one run and the remainder
/// is the per-change work the scans are not.
#[derive(Default)]
struct ScanCosts {
	/// `derive::from_baseline`: two maps sized to the whole baseline, one visit per row.
	from_baseline: Duration,
	/// `RemoteObservation::new`'s uuid index over the whole derived map, plus applying the delta
	/// (empty here, so what is left is the index).
	observe_remote: Duration,
	/// `load_remote_rules`' candidate scan over the UNFILTERED view.
	rule_files_scan: Duration,
	/// `RuleFiles::Only`'s scan of every baseline row. Zero on a mode that pushes, which never
	/// builds it.
	rule_files_only: Duration,
	/// `RemoteView::filter` — `hide`'s whole-map `retain` and `resolve_collisions`' whole-map key
	/// scan, which no seam separates from outside.
	view_filter: Duration,
	/// `PendingWrites::fold_into`'s uuid index over the whole view.
	pending_fold: Duration,
}

impl ScanCosts {
	/// One row per scan, named `<phase>__<scan>` so a TSV groups them under the phase they were
	/// measured in.
	fn record(&self, probe: &mut Probe, phase: &str, items: usize) {
		for (scan, elapsed, detail) in [
			(
				"from_baseline",
				self.from_baseline,
				"two maps sized to the baseline, one visit per row",
			),
			(
				"observe_remote",
				self.observe_remote,
				"`RemoteObservation::new`'s uuid index over the whole derived map",
			),
			(
				"rule_files_scan",
				self.rule_files_scan,
				"`load_remote_rules`' candidate scan over the unfiltered view",
			),
			(
				"rule_files_only",
				self.rule_files_only,
				"`RuleFiles::Only`'s scan of every baseline row; 0 on a mode that pushes",
			),
			(
				"view_filter",
				self.view_filter,
				"`hide`'s whole-map retain plus `resolve_collisions`' whole-map key scan",
			),
			(
				"pending_fold",
				self.pending_fold,
				"`PendingWrites::fold_into`'s uuid index over the whole view",
			),
		] {
			probe.record(&format!("{phase}__{scan}"), items, elapsed, detail);
		}
	}
}

/// Bytes as mebibytes: the unit every memory figure in this harness is written in.
fn mib(bytes: u64) -> f64 {
	bytes as f64 / (1024.0 * 1024.0)
}

/// One heap allocation of `bytes`, rounded the way an allocator hands out size classes — the same
/// convention [`Baseline::resident_bytes`] counts in, so a pass's structures can be added up.
fn heap(bytes: usize) -> usize {
	bytes.div_ceil(16) * 16
}

/// What one path-keyed side of a pass costs: the table's slots, every key's own bytes, and
/// whatever each node holds on the heap.
///
/// Counted from `capacity()` — the entries the map takes before it grows — because hashbrown's
/// table is the next power of two above `capacity * 8 / 7`: this is a LOWER bound on the
/// allocation rather than a guess at it, and at a million rows the real table is nearly twice
/// this figure's slot term. Same convention as [`Baseline::resident_bytes`], which is what lets
/// the three numbers be summed and compared against a resident set.
fn side_bytes<V>(map: &HashMap<String, V>, node_heap: impl Fn(&V) -> usize) -> usize {
	let slots = map.capacity() * (size_of::<String>() + size_of::<V>() + 1);
	let owned: usize = map
		.iter()
		.map(|(path, node)| heap(path.len()) + node_heap(node))
		.sum();
	slots + owned
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

/// The process's resident set right NOW, in bytes. Unlike [`peak_rss_bytes`] it can go down,
/// which is the whole point: what a structure costs is the difference it makes to a figure that
/// falls again when it is dropped, and a high-water mark every earlier phase has already raised
/// cannot say that.
///
/// Linux reads `/proc/self/statm`; everything else asks `ps`, which is a fork and a pipe and so
/// belongs nowhere inside a timed phase. Returns 0 where neither answers.
#[cfg(target_os = "linux")]
fn current_rss_bytes() -> u64 {
	// Field 2 is the resident page count. 4 KiB is the page size on every target this runs on;
	// a wrong guess here would scale the figure, not invent one.
	fs::read_to_string("/proc/self/statm")
		.ok()
		.and_then(|statm| {
			statm
				.split_whitespace()
				.nth(1)
				.and_then(|pages| pages.parse::<u64>().ok())
		})
		.map_or(0, |pages| pages * 4096)
}

#[cfg(not(target_os = "linux"))]
fn current_rss_bytes() -> u64 {
	std::process::Command::new("ps")
		.args(["-o", "rss=", "-p", &std::process::id().to_string()])
		.output()
		.ok()
		.and_then(|out| String::from_utf8(out.stdout).ok())
		// `ps` reports kibibytes.
		.and_then(|out| out.trim().parse::<u64>().ok())
		.map_or(0, |kib| kib * 1024)
}

/// What a child process is asked to hold while it measures itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildAsk {
	/// A LOADED PAIR and nothing else: the steady state an engine sits in between passes.
	Pair,
	/// ONE FULL PASS over the fixture and nothing else: the widest point an engine ever reaches,
	/// in a process whose resident set IS that pass rather than a run's history.
	FullPass,
}

impl ChildAsk {
	fn tag(self) -> &'static str {
		match self {
			Self::Pair => "pair",
			Self::FullPass => "pass",
		}
	}
}

/// One child's answer: the samples it took around the thing it was asked to hold, and what that
/// thing computes its own size as.
#[derive(Default)]
struct ChildSample {
	/// Before it opened anything: the binary, the runtime, the allocator's first pages. What is
	/// left when every structure is subtracted, and the floor no engine change can move.
	floor: u64,
	/// With everything it was asked to hold alive.
	widest: u64,
	/// Its own high-water mark, which the transients of BUILDING that thing sit in — the gap
	/// between this and `widest` is what was allocated and freed on the way.
	peak: u64,
	/// After dropping all of it, including the store: what the allocator kept and what the mapped
	/// DB pages cost.
	after: u64,
	rows: usize,
	elapsed_ms: f64,
	baseline_bytes: usize,
	view_bytes: usize,
	scan_bytes: usize,
	actions: usize,
}

impl ChildSample {
	const FIELDS: usize = 10;

	fn encode(&self) -> String {
		format!(
			"{}\t{}\t{}\t{}\t{}\t{:.3}\t{}\t{}\t{}\t{}",
			self.floor,
			self.widest,
			self.peak,
			self.after,
			self.rows,
			self.elapsed_ms,
			self.baseline_bytes,
			self.view_bytes,
			self.scan_bytes,
			self.actions,
		)
	}

	fn decode(raw: &str) -> Option<Self> {
		let fields: Vec<&str> = raw.trim().split('\t').collect();
		if fields.len() != Self::FIELDS {
			return None;
		}
		Some(Self {
			floor: fields[0].parse().ok()?,
			widest: fields[1].parse().ok()?,
			peak: fields[2].parse().ok()?,
			after: fields[3].parse().ok()?,
			rows: fields[4].parse().ok()?,
			elapsed_ms: fields[5].parse().ok()?,
			baseline_bytes: fields[6].parse().ok()?,
			view_bytes: fields[7].parse().ok()?,
			scan_bytes: fields[8].parse().ok()?,
			actions: fields[9].parse().ok()?,
		})
	}

	/// The structures the child accounted for, and what is left over once they are subtracted from
	/// what it actually held.
	fn accounting(&self) -> String {
		let structures = self.baseline_bytes + self.view_bytes + self.scan_bytes;
		format!(
			"baseline {:.1} + view {:.1} + scan {:.1} = {:.1} MiB computed, {:.1} MiB floor, \
			 {:.1} MiB neither (allocator retention, mapped DB pages, table slack)",
			mib(self.baseline_bytes as u64),
			mib(self.view_bytes as u64),
			mib(self.scan_bytes as u64),
			mib(structures as u64),
			mib(self.floor),
			mib(self.widest.saturating_sub(self.floor + structures as u64)),
		)
	}
}

/// Ask a FRESH PROCESS what one thing costs: this binary re-invokes itself with `SYNC_PROBE_CHILD`
/// set, the child holds what it was asked to hold, samples itself around it and writes the samples
/// down.
///
/// It exists because an in-process figure cannot answer an ABSOLUTE question. The resident set of
/// this process is its whole history — every phase has allocated and freed, and what the allocator
/// has not returned to the kernel is still counted — so the most it can give is an upper bound and
/// the DIFFERENCE one structure makes. The `pass_pure` phase makes that concrete: at a million rows
/// it reads ~1.5 GiB resident, of which barely a third is the pass, and a pass that reuses pages
/// the phases before it freed raises the figure by less than it holds. A child that has done
/// nothing else is the only honest absolute.
fn child_sample(
	ask: ChildAsk,
	baseline_db: &Path,
	pair: i64,
	place: &PassPlace,
) -> Option<ChildSample> {
	let answer = std::env::temp_dir().join(format!("filen_probe_child_{}", Uuid::new_v4()));
	let spec = format!(
		"{}\t{}\t{pair}\t{}\t{}\t{}\t{}",
		ask.tag(),
		baseline_db.to_string_lossy(),
		answer.to_string_lossy(),
		place.root.to_string_lossy(),
		place.cache_db.to_string_lossy(),
		place.remote_root,
	);
	let ok = std::process::Command::new(std::env::current_exe().ok()?)
		.args(["--ignored", "--exact", "sync_engine_phase_costs"])
		.env("SYNC_PROBE_CHILD", spec)
		.status()
		.ok()?
		.success();
	let read = fs::read_to_string(&answer).ok();
	fs::remove_file(&answer).ok();
	if !ok {
		return None;
	}
	ChildSample::decode(&read?)
}

/// The child half of [`child_sample`]: sample the floor, hold what was asked for, sample again,
/// drop it and sample once more. Runs INSTEAD of everything in [`run`], so its figures describe a
/// process that has built no tree it was not asked to build.
fn answer_child(spec: &str) {
	let mut parts = spec.split('\t');
	let ask = parts.next().expect("the spec names what to measure");
	let baseline_db = PathBuf::from(parts.next().expect("the spec names a baseline DB"));
	let pair: i64 = parts
		.next()
		.expect("the spec names a pair")
		.parse()
		.expect("the pair id is a number");
	let answer = PathBuf::from(parts.next().expect("the spec names an answer file"));
	let place = PassPlace {
		root: PathBuf::from(parts.next().expect("the spec names a local root")),
		cache_db: PathBuf::from(parts.next().expect("the spec names a cache DB")),
		remote_root: parts
			.next()
			.expect("the spec names a remote root")
			.parse()
			.expect("the remote root is a uuid"),
	};
	// Taken before anything is opened: this is the process, not what it is about to hold.
	let mut sample = ChildSample {
		floor: current_rss_bytes(),
		..ChildSample::default()
	};
	let store = BaselineStore::open(&baseline_db).expect("opening the baseline DB");
	match ask {
		"pair" => {
			let (resident, elapsed) = timed(|| store.baseline(pair).expect("reading the baseline"));
			sample.rows = resident.len();
			sample.baseline_bytes = resident.resident_bytes();
			sample.elapsed_ms = elapsed.as_secs_f64() * 1e3;
			// Sampled with the pair still held, which is the state an idle engine sits in.
			sample.widest = current_rss_bytes();
			sample.peak = peak_rss_bytes();
			// The store keeps its own copy of the pair, so both go — what is left is neither the
			// pair nor the read that built it.
			drop(resident);
			drop(store);
			sample.after = current_rss_bytes();
		}
		"pass" => {
			let rules = probe_rules();
			let (footprint, elapsed) = timed(|| pass_pure(&place, &store, pair, &rules));
			sample.rows = footprint.rows;
			sample.actions = footprint.actions;
			sample.widest = footprint.widest;
			sample.peak = footprint.peak;
			sample.baseline_bytes = footprint.baseline_bytes;
			sample.view_bytes = footprint.view_bytes;
			sample.scan_bytes = footprint.scan_bytes;
			sample.elapsed_ms = elapsed.as_secs_f64() * 1e3;
			drop(store);
			sample.after = current_rss_bytes();
		}
		other => panic!("SYNC_PROBE_CHILD asks for {other:?}, which is not a thing to measure"),
	}
	fs::write(answer, sample.encode()).expect("writing the answer");
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

/// What one full pass holds at its widest point, and what the pieces of it cost.
///
/// The two RSS samples and the three computed figures answer different halves of the same
/// question. `widest` is what the process actually holds with every structure of the pass alive;
/// the computed figures say which structure holds it. What the two do not account for between
/// them is the answer to "where is the rest" — table slack above `capacity`, the allocator's
/// retention, and the cache DB's mapped pages.
struct PassFootprint {
	actions: usize,
	rows: usize,
	/// The process's resident set with every one of the pass's structures alive.
	widest: u64,
	/// The high-water mark the pass reached, which its build-and-drop transients sit in: the local
	/// walk's collision digest, the rows of the baseline read, the plan's own churn.
	peak: u64,
	baseline_bytes: usize,
	view_bytes: usize,
	scan_bytes: usize,
}

/// Everything a pass does locally, end to end, on inputs it re-reads itself: the phases above in
/// the order `prepare` runs them, minus the network and the apply.
fn pass_pure(
	place: &PassPlace,
	store: &BaselineStore,
	pair: i64,
	rules: &IgnoreRules,
) -> PassFootprint {
	let baseline = store.baseline(pair).expect("reading the baseline");
	// Streamed into the view, as `prepare_whole` streams it: no `Vec` of the subtree in between.
	let mut builder = plan::ViewBuilder::with_capacity(place.remote_root, baseline.len());
	bench_support::snapshot_into(&place.cache_db, place.remote_root, &mut builder)
		.expect("streaming the cache snapshot");
	let mut view = builder.finish();
	let (scan, rules_used) =
		scan::scan_local(&place.root, &baseline, probe_rules(), RuleFiles::Read);
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
	let planned = plan::reconcile(
		SyncMode::TwoWay,
		&baseline,
		&local,
		&remote,
		&PassHolds::default(),
		plan::PassPaths::Whole,
	);
	// Everything a full pass holds at once is alive right here: the resident baseline, the view,
	// the scan and the plan. The peak column cannot say what that costs — `ru_maxrss` is a
	// high-water mark every phase before this one has already raised — and the CURRENT resident
	// set can, at this one moment, before any of it is dropped.
	let widest = current_rss_bytes();
	PassFootprint {
		actions: planned.actions.len(),
		rows: baseline.len(),
		widest,
		peak: peak_rss_bytes(),
		baseline_bytes: baseline.resident_bytes(),
		// Each node carries its own path a SECOND time, beside the key it is filed under: two
		// allocations per node, which is why both sides are counted the same way.
		view_bytes: side_bytes(&remote, |node| heap(node.rel_path.len())),
		scan_bytes: side_bytes(&local, |node| heap(node.rel_path.len())),
	}
}

/// What a CHANGE-SCOPED pass does locally, end to end: every phase of
/// [`SyncEngine::prepare_scoped`](super::SyncEngine) that runs without an account, in the order it
/// runs them — derive both sides from the resident baseline, apply the remote delta, find the
/// remote rule files, build the rule-file list the local half reads under, re-observe the dirty
/// paths, check the assembly, filter the view, fold the pair's unacknowledged writes back into it —
/// followed by the directory-move fold and the reconcile, which the pass runs on what
/// `prepare_scoped` hands back.
///
/// `mode` is the pair's own, and it changes what this costs: a mode that does not push builds
/// `RuleFiles::Only` out of every baseline row, which a pushing pair never pays. Both are measured,
/// so neither mode is priced off the other's number.
///
/// Driving the real method would be better and is not possible here: it is an `async` method on a
/// `SyncEngine`, which owns an authenticated `Client`, a registered pair, a cache sync-root
/// subscription and the carried state of a previous whole pass. This module has no account, no
/// network and no runtime by design, so the phases below are assembled by hand — which is exactly
/// how this measurement drifted from the pass once before. What it leaves out is listed below, ALL
/// of it, each with the cost class that says whether leaving it out can grow with the tree. EVERY
/// per-node step a scoped pass runs is now inside the phase; nothing omitted here is one.
///
/// - `pass_inputs`' registry, failure and user-ignore reads, the carried-state lookup, the
///   `Observations` snapshot and the cache-slot check — PER PAIR,
/// - APPLYING a remote delta: the probe's is empty, so `observe_remote` here builds its whole-map
///   `path_of` index and does nothing else — PER ANNOUNCED CHANGE, one map operation each,
/// - READING a remote `.filenignore`. The scan that finds the candidates is timed (see below); the
///   fetch and the parse are PER RULE FILE, and only where a directory has one,
/// - `confirm_pushes` — PER UNCONFIRMED ROW (`Baseline::unconfirmed`, and only while the baseline
///   holds one), plus one round trip per foreign version found at such a row's path,
/// - `PendingWrites::settle`, `strangers` and `retire_superseded_creates` — PER PENDING WRITE,
///   plus one round trip per stranger; the per-directory sibling collision check — PER OBSERVED
///   DIRECTORY; `remote_emptied` (two O(1) checks) and `existing_pair_changes` — PER CHANGE,
/// - the facts merge — `merge_remote_view` prunes PER TOUCHED PATH and `unknown_remote_paths`
///   returns before it looks at a row when nothing was skipped, which a derived view guarantees;
///   `facts::observe_local` is PER OBSERVATION,
/// - `unaccounted_key`'s `debug_assert` — PER MAP KEY, so per node, and the one omission here that
///   does grow with the tree. Deliberate: it is one `Baseline` path resolve per key and a shipped
///   release pass does not run it (see `prepare_scoped`).
///
/// The three per-node steps this phase used to leave out are in it as of the previous round —
/// `load_remote_rules`' candidate scan over the UNFILTERED view, `RuleFiles::Only`'s scan of every
/// baseline row on a mode that does not push, and `PendingWrites::fold_into`'s whole-view `path_of`
/// rebuild — which is why no figure here is comparable with one recorded before the rename.
///
/// Each whole-tree scan inside the phase is also timed on its own ([`ScanCosts`]) and reported as a
/// `<phase>__<scan>` row. Those are in-run deltas and are what a narrowing is judged on: an
/// absolute figure at a million rows is not trustworthy between two builds to better than ~10 %.
///
/// The fold is here because `prepare_scoped` ends with it exactly as `prepare_whole` does, and
/// because [`pass_pure`] pays it: a phase that skipped it would credit change-scoping with the cost
/// of a step the real pass still runs every time.
///
/// What a whole pass does and this does not: the cache snapshot, both view builds, and the walk of
/// the tree.
///
/// Returns the action count, so a phase that was supposed to plan something can say whether it did,
/// and what each whole-tree scan inside it cost ([`ScanCosts`]).
fn prepare_scoped(
	fixture: &Fixture,
	store: &BaselineStore,
	pair: i64,
	mode: SyncMode,
	dirty: BTreeSet<String>,
) -> (usize, ScanCosts) {
	let mut costs = ScanCosts::default();
	let mut baseline = store.baseline(pair).expect("reading the baseline");
	let (mut derived, elapsed) = timed(|| derive::from_baseline(&baseline, dirty));
	costs.from_baseline = elapsed;

	// The remote half FIRST, as the pass runs it: every path the delta touched is a path the local
	// half has to re-observe too. `RemoteObservation::new` indexes the whole derived map by uuid
	// before a single change is applied, which is per-node work every scoped pass pays.
	let nodes = mem::take(&mut derived.remote);
	let mut ancestry = |uuid| cache_ancestry(&fixture.cache_db, uuid);
	let (observed, elapsed) =
		timed(|| observe_remote(fixture.remote_root, &baseline, nodes, &[], &mut ancestry));
	costs.observe_remote = elapsed;
	// A GUARD, and not a check that can fail on this fixture: with an EMPTY delta `observe_remote`
	// never enters the loop that refuses a change, and its only other route to `Full` needs an
	// emptied view over a baseline that still has remote rows, which a converged fixture cannot
	// produce. It earns its place by catching a future `observe_remote` that learns to refuse on
	// the empty path; it is not evidence that this phase measured a derivation.
	let mut observation = match observed {
		RemoteObserved::Applied(observation) => *observation,
		RemoteObserved::Full(reason) => panic!(
			"the probe's converged fixture must derive a remote view, not fall back to a whole \
			 read: {reason:?}"
		),
	};
	derived.decided.append(&mut observation.changed);
	// TAKEN before the view's held set is built out of it, for the reason the pass takes it: the
	// assembly check below needs the ROWS this pass holds, not the hidden paths `merge_local` puts
	// back into `derived.held`.
	let held_rows = mem::take(&mut derived.held);
	let mut view = RemoteView {
		nodes: observation.nodes,
		has_collisions: false,
		skipped: Vec::new(),
		ignored: BTreeMap::new(),
		ignored_default_untracked: 0,
		held_paths: held_rows
			.iter()
			.cloned()
			.chain(observation.held_paths)
			.collect(),
	};
	derived.dirty.extend(observation.touched.iter().cloned());

	// The remote rules, where the pass reads them: BEFORE `view.filter`, so the candidate scan runs
	// over the UNFILTERED map. What is TIMED here is that scan — `load_remote_rules` opens by
	// walking every node in the view to find the `.filenignore` files — and not the reads it would
	// then make. The probe's fixture holds no rule file, so no candidate survives and the fetch
	// closure is never called; it panics rather than returning a body, so a fixture that grows a
	// rule file has to be told to serve it instead of quietly timing a network read in here.
	let local_root = &fixture.root;
	let cached: HashMap<Uuid, Arc<str>> = HashMap::new();
	let (remote_rules, elapsed) = timed(|| {
		futures::executor::block_on(load_remote_rules(
			mode,
			&view,
			Some(parse_user_ignore(PROBE_RULES).expect("the probe's user patterns compile")),
			|dir| {
				scan::rule_file_metadata(&local_root.join(dir))
					.map_or(true, |found| found.is_some())
			},
			|dir| baseline.contains_key(&Origin::File { dir }.to_string()),
			&cached,
			|_uuid| async move {
				panic!(
					"the probe's fixture holds no remote `.filenignore`, so this phase times the \
					 scan that looks for one and never a fetch"
				)
			},
		))
	});
	costs.rule_files_scan = elapsed;
	assert!(
		remote_rules.blocked.is_empty() && remote_rules.errors.is_empty(),
		"the scan must find no remote rule file to read on this fixture: blocked {:?}, errors {:?}",
		remote_rules.blocked,
		remote_rules.errors
	);
	// `RuleFiles::Only`, which a mode that does not push builds by iterating EVERY baseline row
	// with a view lookup per row. A pushing pair never pays it, which is why this function takes a
	// mode: both answers are measured rather than one being read off the other.
	let (rule_files, elapsed) = timed(|| {
		if mode.pushes() {
			RuleFiles::Read
		} else {
			RuleFiles::Only(
				baseline
					.iter()
					.filter(|entry| {
						entry.kind == NodeKind::File && !view.nodes.holds(&entry.rel_path)
					})
					.filter_map(|entry| rule_file_dir(&entry.rel_path).map(str::to_owned))
					.collect(),
			)
		}
	});
	costs.rule_files_only = elapsed;

	// The local half: one stat per dirty path and its ancestors, one subtree walk per dirty
	// directory. `derived.dirty` and not the caller's set, because `from_baseline` adds every row
	// it could not carry to it.
	let dirty = mem::take(&mut derived.dirty);
	let (observations, rules) = observe::observe_local(
		&fixture.root,
		&baseline,
		remote_rules.rules,
		&rule_files,
		&dirty,
	);
	derive::merge_local(&mut derived, &baseline, &observations);
	// Plan 3.6's self-check, which the pass runs before anything plans against these maps.
	assert!(
		assembly_accounted(&baseline, &derived, &observations, &held_rows),
		"the derived local map must account for the rows and observations it was built from; a \
		 pass that fails this reads both sides whole instead, so the phase would be timing a pass \
		 no engine would run"
	);
	// The rules, then the case-fold collision check — both over the WHOLE view, and both paid by
	// every scoped pass.
	let ((), elapsed) = timed(|| {
		view.filter(Some(plan::ViewFilter {
			rules: &rules,
			baseline: &baseline,
		}));
	});
	costs.view_filter = elapsed;
	// The paths an observation found hidden with a row still behind them, added AFTER the filter
	// exactly as the pass adds them.
	view.held_paths.append(&mut derived.held);

	// `PendingWrites::fold_into`, which a pass pays whenever the pair holds an unacknowledged
	// write — the pass after every apply that pushed something. ONE record, because what is per
	// node here is the `path_of` index it rebuilds from the whole view before it reads any record;
	// the loop after that index is per record.
	let written = baseline
		.iter()
		.find(|entry| entry.kind == NodeKind::File && entry.remote_uuid.is_some())
		.expect("the probe's baseline holds a synced file");
	let pending = PendingWrites::default();
	pending.restore(
		pair,
		written.remote_uuid.expect("filtered for above"),
		PendingKind::Created {
			path: written.rel_path.clone(),
			replaced: None,
		},
		Duration::ZERO,
	);
	// FIRST, because `fold_into` returns 0 both for a record it could not apply and for a pair
	// holding no record at all — and the second of those returns before it builds the index this
	// phase exists to time.
	assert_eq!(
		pending.uuids().len(),
		1,
		"the fold must have a record to walk, or the 0 below is an early return and this phase is \
		 timing nothing"
	);
	let (folded, elapsed) =
		timed(|| pending.fold_into(pair, &baseline, &mut view.nodes, &mut derived.decided));
	costs.pending_fold = elapsed;
	assert_eq!(
		folded, 0,
		"the fixture's view already shows the row's own uuid at that path, so the fold has nothing \
		 to correct; a fold that applied here would be editing the maps this phase goes on to time"
	);

	let Derived {
		mut local,
		mut decided,
		..
	} = derived;
	let mut remote = view.nodes;
	// What the pass folds and plans with is `holds.held_remote`, which is the view's held set.
	let held = view.held_paths;
	let moves = plan::fold_dir_moves(
		mode,
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
	let actions = plan::reconcile(
		mode,
		&baseline,
		&local,
		&remote,
		&PassHolds::default(),
		plan::PassPaths::Changed(&decided),
	)
	.actions
	.len();
	(actions, costs)
}

/// Run every phase once and return the TSV. One header comment naming the shape, then one line per
/// phase: `phase, n, items, ms, us_per_item, peak_rss_mib, detail`.
#[must_use]
pub fn run() -> String {
	// A child asked what one thing costs (see `child_sample`). It answers before anything here
	// builds a tree, which is the whole point of asking a second process.
	if let Ok(spec) = std::env::var("SYNC_PROBE_CHILD") {
		answer_child(&spec);
		return String::new();
	}
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
	let before_vecs = current_rss_bytes();
	let (again, warm) = timed(|| {
		bench_support::snapshot(&fixture.cache_db, fixture.remote_root).expect("snapshot")
	});
	let holding_vecs = current_rss_bytes();
	probe.record(
		"snapshot_sql_warm",
		nodes,
		warm,
		&format!(
			"read_subtree_snapshot, page cache warm; holding the two Vecs costs {:.1} MiB \
			 resident ({:.0} B/row) — the copy of the tree a streamed read never builds",
			holding_vecs.saturating_sub(before_vecs) as f64 / (1024.0 * 1024.0),
			holding_vecs.saturating_sub(before_vecs) as f64 / nodes as f64,
		),
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

	// The same view, built the way a pass builds it: every row placed as it arrives, with no copy
	// of the subtree in between. Comparable to `snapshot_sql_warm` + `view_place`, the two steps
	// it replaces — and the assertion below is the equivalence, checked at this run's scale
	// rather than only on the handful of items a unit test can spell out.
	let before_stream = current_rss_bytes();
	let ((streamed, streamed_watermark), streamed_time) = timed(|| {
		let mut builder = plan::ViewBuilder::with_capacity(fixture.remote_root, nodes);
		let watermark =
			bench_support::snapshot_into(&fixture.cache_db, fixture.remote_root, &mut builder)
				.expect("streaming the snapshot");
		(builder.finish(), watermark)
	});
	let holding_stream = current_rss_bytes();
	assert_eq!(
		streamed.nodes, view.nodes,
		"the streamed view must be the view the slices build, node for node"
	);
	assert_eq!(
		streamed_watermark, snapshot.watermark,
		"and it must carry the same watermark, which is what aligns it with the event stream"
	);
	probe.record(
		"view_stream",
		nodes,
		streamed_time,
		&format!(
			"read + place in one walk of the rows: {:.1} MiB resident for the view, against \
			 {:.1} MiB for the read's Vecs alone above",
			holding_stream.saturating_sub(before_stream) as f64 / (1024.0 * 1024.0),
			holding_vecs.saturating_sub(before_vecs) as f64 / (1024.0 * 1024.0),
		),
	);
	drop(streamed);

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
	let before_pass = current_rss_bytes();
	let (footprint, whole) = timed(|| pass_pure(&fixture.place(), &store, pair, &rules));
	probe.record(
		"pass_pure",
		nodes,
		whole,
		&format!(
			"{} actions; baseline read + streamed snapshot + view + warm scan + fold + reconcile, \
			 no network and no apply; {:.1} MiB resident at its widest point, {:.1} MiB of it this \
			 pass; the structures alive there: baseline {:.1} + view {:.1} + scan {:.1} = {:.1} \
			 MiB computed",
			footprint.actions,
			mib(footprint.widest),
			mib(footprint.widest.saturating_sub(before_pass)),
			mib(footprint.baseline_bytes as u64),
			mib(footprint.view_bytes as u64),
			mib(footprint.scan_bytes as u64),
			mib((footprint.baseline_bytes + footprint.view_bytes + footprint.scan_bytes) as u64),
		),
	);

	// The same pass, in a process that has done nothing else — the figure a memory target can
	// honestly be read off. The phase above cannot be one: `before_pass` says most of this run's
	// resident set is what the phases before it allocated and freed, and a pass that reuses those
	// pages raises RSS by less than it holds, so the same run's number is at once too high to be
	// the pass and too low to be its structures. The child's baseline read is COLD, which is what
	// the first pass after a start pays and what the phase above — holding the pair already —
	// leaves out.
	//
	// Measured HERE, before the phases below change a file on disk: a pass over a diverged tree
	// plans actions, and a plan is another structure alive at the widest point.
	let db_bytes = |path: &Path| fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
	match child_sample(
		ChildAsk::FullPass,
		&fixture.baseline_db,
		pair,
		&fixture.place(),
	) {
		Some(child) => probe.record(
			"pass_pure_fresh",
			child.rows,
			Duration::from_secs_f64(child.elapsed_ms / 1e3),
			&format!(
				"{} action(s); {:.1} MiB resident at the widest point of a FRESH process, peak \
				 {:.1} MiB, {:.1} MiB still resident once every structure and the store are \
				 dropped; {}; cache.db {:.1} MiB (mapped up to 256 MiB, page cache 32 MiB), \
				 baseline.db {:.1} MiB (no mmap, 2 MiB page cache)",
				child.actions,
				mib(child.widest),
				mib(child.peak),
				mib(child.after),
				child.accounting(),
				mib(db_bytes(&fixture.cache_db)),
				mib(db_bytes(&fixture.baseline_db)),
			),
		),
		None => probe.record(
			"pass_pure_fresh",
			0,
			Duration::ZERO,
			"the child process could not be asked; only the in-process figure above stands",
		),
	}

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
	let ((scoped_actions, scoped_costs), scoped) =
		timed(|| prepare_scoped(&fixture, &store, pair, SyncMode::TwoWay, dirty.clone()));
	probe.record(
		"scoped_twoway_one_file_changed",
		nodes,
		scoped,
		&format!(
			"{scoped_actions} action(s) for 1 changed file ({changed}); baseline + derived maps + \
			 the remote observation + the rule-file scan + one re-observed path + the assembly \
			 check + the view filter + the pending-write fold + dir-move fold + reconcile, no walk \
			 and no snapshot"
		),
	);
	assert_eq!(
		scoped_actions, 1,
		"one changed file must plan exactly one action, or this phase is timing the wrong thing"
	);
	scoped_costs.record(&mut probe, "scoped_twoway_one_file_changed", nodes);

	// The same wake on a pair that only PULLS, which is the mode that pays `RuleFiles::Only` — a
	// scan of every baseline row with a view lookup per row, built before the local half reads
	// anything. No pushing pair pays it, so pricing those modes off the row above would understate
	// them by a whole-tree scan.
	let ((pull_actions, pull_costs), pull) = timed(|| {
		prepare_scoped(
			&fixture,
			&store,
			pair,
			SyncMode::RemoteToLocal,
			dirty.clone(),
		)
	});
	probe.record(
		"scoped_pull_one_file_changed",
		nodes,
		pull,
		&format!(
			"{pull_actions} action(s) for the same 1 changed file on a `RemoteToLocal` pair; the \
			 row above plus `RuleFiles::Only`, which iterates every baseline row"
		),
	);
	assert_eq!(
		pull_actions, PULL_ONE_FILE_ACTIONS,
		"a local edit on a pull-only pair is the remote's to overwrite, so the count is fixed; a \
		 different one means this phase is no longer timing that pass"
	);
	pull_costs.record(&mut probe, "scoped_pull_one_file_changed", nodes);

	// The phase the one-per-cent target is read off: a real change-scoped pass, with one per cent
	// of the files edited on disk and named in its dirty set.
	// `scoped_twoway_one_file_changed` above is the same machinery at its floor and `pass_pure`
	// the whole-read yardstick — same fixture, same run, so the three numbers are comparable.
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
	let ((percent_actions, _), percent) = timed(|| {
		prepare_scoped(
			&fixture,
			&store,
			pair,
			SyncMode::TwoWay,
			percent_dirty.clone(),
		)
	});
	probe.record(
		"scoped_twoway_one_percent_changed",
		nodes,
		percent,
		&format!(
			"{percent_actions} action(s) for {} changed file(s); the same steps as \
			 `scoped_twoway_one_file_changed`, with one per cent of the files in the dirty set",
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
	let ((idle_actions, _), idle) =
		timed(|| prepare_scoped(&fixture, &store, pair, SyncMode::TwoWay, BTreeSet::new()));
	probe.record(
		"scoped_twoway_idle_floor",
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

	// The memory figure the targets are written in and nothing here has answered: what a process
	// holding a LOADED PAIR costs between passes. Every other number in this run is either the
	// process's peak — which includes the scan, the views and the snapshot — or
	// `Baseline::resident_bytes`, which the structure sums from its own capacities and cannot see
	// the allocator.
	//
	// A store that has never been asked for the pair holds no resident copy of it, so the sample
	// before the ask and the sample after are the same process with and without one. Both figures
	// are UPPER bounds on an engine's own: this process has run every phase above, and whatever
	// it freed that the allocator has not returned to the kernel is still counted here.
	drop(store);
	let store = BaselineStore::open(&fixture.baseline_db).expect("reopening the baseline DB");
	let without_pair = current_rss_bytes();
	let resident = store.baseline(pair).expect("reading the baseline");
	let resident_rows = resident.len();
	let with_pair = current_rss_bytes();
	probe.record(
		"steady_state_rss",
		resident_rows,
		Duration::ZERO,
		&format!(
			"{:.1} MiB resident with every per-pass structure dropped, {:.1} MiB of which is the \
			 loaded pair itself ({:.0} B/row)",
			with_pair as f64 / (1024.0 * 1024.0),
			with_pair.saturating_sub(without_pair) as f64 / (1024.0 * 1024.0),
			with_pair.saturating_sub(without_pair) as f64 / resident_rows.max(1) as f64,
		),
	);
	// The store keeps its own copy, which is exactly what an idle engine holds between passes.
	drop(resident);

	// And the same question asked of a process that has done nothing else, which is the only
	// answer here that is not an upper bound. The pair's rows are on disk; the child opens them
	// and reports what it then holds.
	match child_sample(ChildAsk::Pair, &fixture.baseline_db, pair, &fixture.place()) {
		Some(child) => probe.record(
			"steady_state_rss_fresh",
			child.rows,
			Duration::from_secs_f64(child.elapsed_ms / 1e3),
			&format!(
				"{:.1} MiB resident in a FRESH process holding the loaded pair and nothing else \
				 ({:.0} B/row over the whole process, binary and SQLite included); {:.1} MiB floor \
				 before it opened anything, {:.1} MiB peak while reading the rows, {:.1} MiB left \
				 after the pair and its store are dropped; the tree itself computes as {:.1} MiB, \
				 leaving {:.1} MiB that is neither floor nor tree",
				mib(child.widest),
				child.widest as f64 / child.rows.max(1) as f64,
				mib(child.floor),
				mib(child.peak),
				mib(child.after),
				mib(child.baseline_bytes as u64),
				mib(child
					.widest
					.saturating_sub(child.floor + child.baseline_bytes as u64)),
			),
		),
		None => probe.record(
			"steady_state_rss_fresh",
			0,
			Duration::ZERO,
			"the child process could not be asked; only the in-process upper bound above stands",
		),
	}

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
