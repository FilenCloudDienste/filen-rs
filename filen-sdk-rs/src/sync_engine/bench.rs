//! What one REAL sync pass costs, named scenario by named scenario, with no account and no network.
//!
//! The difference between this and [`probe`](super::probe) is the whole reason it exists: the probe
//! re-implements a pass by hand, and has twice drifted from the engine it claims to measure. This
//! drives [`SyncEngine::prepare`](super::SyncEngine) itself. There is no second copy of the pass to
//! keep in step, so there is nothing to drift: a step the engine stops running stops being timed,
//! and a step it starts running is timed the day it lands.
//!
//! What made that possible is that nothing in a pass's READ path needs an account. `SyncEngine::open`
//! on a baseline DB with no pairs in it makes no network call; `Client::configure_cache` only writes
//! a path into a slot, which is the one thing `prepare_scoped` asks the client for; and on a
//! converged fixture holding no `.filenignore` and no unconfirmed row, the two paths that WOULD dial
//! the server — the remote rule-file fetch and `confirm_pushes`' version chain — are unreachable.
//! The engine's own unit tests already open engines this way (`offline_client` in `engine.rs`).
//!
//! # How the pieces answer the ways a microbenchmark lies
//!
//! - **Fixture work leaking into the timed region.** The tree, the cache DB, the baseline rows and
//!   the on-disk edits a scenario's change class calls for are ALL applied before the first sample.
//!   A sample announces paths and runs a pass; it creates nothing.
//! - **The optimiser deleting work whose result is unused.** Every sample's outcome is asserted
//!   against the scenario's expected action count and passed through [`std::hint::black_box`], so
//!   the plan cannot be dead code.
//! - **Timer resolution against figures as small as 14 us.** A scenario declares `reps`: the sample
//!   runs the pass that many times inside one clock and divides. The raw sample is recorded too, so
//!   a reader can see the figure the division came from.
//! - **First-iteration and page-cache effects.** [`WARMUPS`] passes run before the first recorded
//!   sample and are discarded. They are not free measurements — they are the page-cache and
//!   branch-predictor state every recorded sample is then taken in.
//! - **Instrumentation inside the region it measures.** The step marks are one `Instant::now()` and
//!   one push each. Their cost is inside the phase they bound, so it lands in that step rather than
//!   in the remainder — and the run records `mark_overhead_ns` and each sample's mark COUNT, so a
//!   reader can bound the total rather than take it on trust.
//!
//! # Why a record cannot be compared with one it is not comparable to
//!
//! Every record carries its scenario's `definition_hash` — an FNV-1a over the scenario's own fields.
//! Redefining a scenario changes the hash whether or not anyone remembers to bump `version`, and
//! the comparison command refuses to diff two records whose hashes differ. Each run writes ONE fresh file;
//! nothing is ever appended to, so a stale row cannot read as a fresh one.

use std::{
	collections::BTreeMap,
	fmt::Write as _,
	fs,
	path::PathBuf,
	sync::{Arc, Mutex, PoisonError},
	time::{Duration, Instant},
};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use notify::{
	Event, EventKind,
	event::{DataChange, ModifyKind},
};
use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};
use serde::{Deserialize, Serialize};

use super::{
	SyncEngine, SyncMode,
	baseline::BaselineEntry,
	plan,
	probe::{Fixture, Shape, baseline_rows, probe_rules},
	scan::{self, RuleFiles},
	tree::Baseline,
};
use crate::{
	auth::{Client, StringifiedClient, http::ClientConfig, unauth::UnauthClient},
	cache::bench_support,
};

/// The record format's own version. Bumped when a field's MEANING changes, so a reader can refuse a
/// file it would misread rather than silently misreading it.
const HARNESS_VERSION: u32 = 1;

/// Passes run and discarded before the first recorded sample of a scenario. The first pass over a
/// fresh fixture reads a cold page cache and a cold baseline (the store's resident copy is built on
/// the pair's first read), neither of which any later pass pays.
const WARMUPS: usize = 2;

/// Samples per scenario when `SYNC_BENCH_SAMPLES` is unset. Three is the floor a median means
/// anything over, and every one of them is recorded — a median alone hides a bimodal distribution.
const DEFAULT_SAMPLES: usize = 3;

// ---------------------------------------------------------------------------------------------
// Step marks: the engine's own phase boundaries, timed from inside the real pass.
// ---------------------------------------------------------------------------------------------

tokio::task_local! {
	/// Where [`mark`] writes, for the duration of one measured pass. A task-local rather than a
	/// thread-local because a pass hops threads: `prepare_scoped` runs its two halves on
	/// `spawn_blocking` threads, and a thread-local would lose every mark either side of those
	/// awaits the moment the runtime resumed the task somewhere else.
	static STEPS: Arc<Mutex<Vec<(&'static str, Instant)>>>;
}

/// Close the step ending here, if a measured pass is running.
///
/// Called from the engine's own code through [`super::step`], which compiles to nothing without
/// `bench-internals` — so a shipping build carries neither the call nor this function.
///
/// The clock is read FIRST and the lock taken after, so the contention (there is none: one task
/// owns the vector) cannot land inside the figure.
pub(super) fn mark(name: &'static str) {
	let now = Instant::now();
	let _ = STEPS.try_with(|log| {
		log.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.push((name, now));
	});
}

/// What one measured pass cost, split into the steps the engine itself marked.
struct Timing {
	/// The caller's clock around the whole pass, including the drop of everything it built.
	total: Duration,
	/// `(step, elapsed)` in the order the pass ran them.
	steps: Vec<(&'static str, Duration)>,
	/// The total minus every step: the moves between them, and anything no mark bounds.
	unattributed: Duration,
}

impl Timing {
	/// Split `marks` — each the instant one step ENDED — into per-step deltas against `start`.
	///
	/// The steps are disjoint and ordered by construction (each closes where the last ended), so
	/// they can only sum to less than `total`. That is asserted rather than assumed: a saturating
	/// remainder would otherwise report a perfectly balanced table for a pass that double-counted.
	fn split(start: Instant, total: Duration, marks: &[(&'static str, Instant)]) -> Self {
		let mut steps = Vec::with_capacity(marks.len());
		let mut previous = start;
		for (name, at) in marks {
			steps.push((*name, at.saturating_duration_since(previous)));
			previous = *at;
		}
		let accounted = previous.saturating_duration_since(start);
		assert!(
			accounted <= total,
			"the steps sum to {accounted:?}, past the {total:?} the caller timed: two marks are \
			 out of order, or a step was timed twice"
		);
		Self {
			total,
			steps,
			unattributed: total - accounted,
		}
	}
}

// ---------------------------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------------------------

/// What a scenario changes between converging the fixture and running the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
	/// Nothing at all: the floor the machinery costs with an empty changelist.
	Idle,
	/// One file's content, and only that file's path announced.
	OneFile,
	/// This many files per thousand, rounded up to at least one.
	PerMille(u32),
}

impl Change {
	fn label(self) -> String {
		match self {
			Self::Idle => "idle".to_owned(),
			Self::OneFile => "one_file".to_owned(),
			Self::PerMille(per_mille) => format!("per_mille_{per_mille}"),
		}
	}

	/// How many of `files` this class edits.
	fn count(self, files: usize) -> usize {
		match self {
			Self::Idle => 0,
			Self::OneFile => 1,
			Self::PerMille(per_mille) => (files * per_mille as usize / 1000).max(1),
		}
	}
}

/// A named, versioned benchmark definition: one deterministic fixture and one thing measured on it.
///
/// Running one means naming it. Nothing here is read from the environment, so a row in a result
/// file says exactly what produced it without anyone having to remember which three variables were
/// exported that afternoon.
#[derive(Debug, Clone, Copy)]
pub struct Scenario {
	/// How a run names it, and how a result file keys it.
	pub name: &'static str,
	/// Bumped by hand when the definition changes. Belt to the braces of `definition_hash`, which
	/// changes whether or not this does.
	pub version: u32,
	/// Files per leaf directory.
	pub files_per_leaf: usize,
	/// Directory levels.
	pub depth: usize,
	/// Bytes per file.
	pub file_bytes: usize,
	/// Nodes the tree should hold; the shape rounds up to the next whole branching factor.
	pub nodes: usize,
	/// What changes before the pass runs.
	pub change: Change,
	/// The pair's mode, which decides real work: a mode that does not push builds `RuleFiles::Only`,
	/// and a pushing pair never pays it.
	pub mode: SyncMode,
	/// Passes per sample. Above one for a scenario whose figure approaches the clock's resolution:
	/// the sample divides by it, and records the undivided figure beside it.
	pub reps: usize,
	/// Whether the pass must narrow its read. A scenario that silently fell back to a whole read
	/// would report a believable number for the wrong pass, so this is checked every sample.
	pub expect_scoped: bool,
}

impl Scenario {
	/// An FNV-1a over every field that decides what this measures.
	///
	/// This is what makes a wrong comparison impossible rather than merely discouraged: edit any
	/// field and the hash moves, so the comparison command refuses to set the new rows beside the old
	/// even if nobody remembered to bump [`version`](Self::version).
	fn definition_hash(&self) -> u64 {
		let mut spec = String::new();
		write!(
			spec,
			"v{} {} {}f/{}d/{}b n{} {} {:?} reps{} scoped{}",
			self.version,
			self.name,
			self.files_per_leaf,
			self.depth,
			self.file_bytes,
			self.nodes,
			self.change.label(),
			self.mode,
			self.reps,
			self.expect_scoped,
		)
		.expect("writing to a String never fails");
		let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
		for byte in spec.as_bytes() {
			hash ^= u64::from(*byte);
			hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
		}
		hash
	}

	fn shape(&self) -> Shape {
		Shape::new(self.files_per_leaf, self.depth, self.file_bytes)
	}
}

/// Every scenario this harness knows, keyed by name.
///
/// These three mirror the change classes the probe measures today, on the one shape every recorded
/// figure so far was taken on, so the first runs of this harness can be read against that history.
/// The matrix — wide flat directories, deep narrow trees, long and non-ASCII names, first sync, mass
/// delete, rename storms — is the next step's, and adding one is a row here.
pub const SCENARIOS: &[Scenario] = &[
	Scenario {
		name: "twoway_idle_10k",
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		nodes: 10_000,
		change: Change::Idle,
		mode: SyncMode::TwoWay,
		// The floor is tens of microseconds: one pass per clock would be measuring the clock.
		reps: 32,
		expect_scoped: true,
	},
	Scenario {
		name: "twoway_one_file_10k",
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		nodes: 10_000,
		change: Change::OneFile,
		mode: SyncMode::TwoWay,
		reps: 8,
		expect_scoped: true,
	},
	Scenario {
		name: "twoway_one_percent_10k",
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		nodes: 10_000,
		change: Change::PerMille(10),
		mode: SyncMode::TwoWay,
		reps: 1,
		expect_scoped: true,
	},
];

/// The scenario called `name`, or `None`.
#[must_use]
pub fn scenario(name: &str) -> Option<&'static Scenario> {
	SCENARIOS.iter().find(|scenario| scenario.name == name)
}

// ---------------------------------------------------------------------------------------------
// Result records
// ---------------------------------------------------------------------------------------------

/// One measurement. Everything needed to know whether it may be set beside another one is ON the
/// record, not in the file name or in somebody's notes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
	pub scenario: String,
	pub scenario_version: u32,
	/// See [`Scenario::definition_hash`].
	pub definition_hash: u64,
	/// Which sample this came from; the yardstick and the per-run constants carry `None`.
	pub sample: Option<usize>,
	/// `total`, `unattributed`, `step:<name>`, `yardstick_whole_pass`, ...
	pub metric: String,
	pub ms: f64,
	/// For a metric that counts rather than times. A count encoded as a duration reads as
	/// `0.000013 ms` in a column headed milliseconds, which is exactly the kind of figure this
	/// harness exists to stop anyone quoting.
	pub count: Option<u64>,
	/// Nodes the fixture actually held, so a figure is never read per-node against the wrong tree.
	pub nodes: usize,
}

/// One run of this harness: what produced every record in it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunFile {
	pub harness_version: u32,
	pub started: String,
	pub commit: String,
	pub toolchain: String,
	pub machine: String,
	/// `debug` or `release`. A figure from one says nothing about the other.
	pub profile: String,
	/// What one step mark costs, measured on this machine in this run.
	pub mark_overhead_ns: f64,
	pub records: Vec<Record>,
}

/// The constants a run is stamped with. Collected once, before anything is measured.
fn run_meta() -> (String, String, String, String) {
	let output = |program: &str, args: &[&str]| -> String {
		std::process::Command::new(program)
			.args(args)
			.current_dir(env!("CARGO_MANIFEST_DIR"))
			.output()
			.ok()
			.filter(|out| out.status.success())
			.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
			.unwrap_or_else(|| "unknown".to_owned())
	};
	let commit = output("git", &["rev-parse", "HEAD"]);
	let toolchain = output("rustc", &["--version"]);
	let machine = format!(
		"{} {}/{}",
		output("uname", &["-n"]),
		std::env::consts::OS,
		std::env::consts::ARCH,
	);
	// What the binary can actually observe about its own build. Deliberately not the words
	// "debug" and "release": this crate's `test` profile compiles OPTIMIZED with
	// `debug_assertions` still on, so either name would be a guess, and overflow checks — which
	// `[profile.release]` turns off — follow this flag rather than the profile's name.
	let profile = if cfg!(debug_assertions) {
		"debug_assertions".to_owned()
	} else {
		"no_debug_assertions".to_owned()
	};
	(commit, toolchain, machine, profile)
}

/// What one [`mark`] costs here, so a reader can bound what the instrumentation contributed to the
/// steps it bounds rather than take it on trust.
fn mark_overhead_ns() -> f64 {
	const ROUNDS: usize = 10_000;
	let log = Arc::new(Mutex::new(Vec::with_capacity(ROUNDS)));
	let start = Instant::now();
	for _ in 0..ROUNDS {
		let now = Instant::now();
		log.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.push(("overhead", now));
	}
	let elapsed = start.elapsed();
	// Kept alive across the clock read so the loop cannot be optimised away.
	assert_eq!(
		std::hint::black_box(&log)
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.len(),
		ROUNDS
	);
	elapsed.as_secs_f64() * 1e9 / ROUNDS as f64
}

// ---------------------------------------------------------------------------------------------
// Running a scenario
// ---------------------------------------------------------------------------------------------

/// A client with no account and no network behind it, for an engine that will never dial one.
///
/// A pass's READ path asks the client for exactly one thing — the configured cache DB path — and the
/// two paths that would reach the server (the remote `.filenignore` fetch and `confirm_pushes`'
/// version chain) are unreachable on a converged fixture holding no rule file and no unconfirmed
/// row. So the credentials here only have to PARSE; nothing ever authenticates with them.
///
/// 512 bits because this key is never used to encrypt anything: `from_stringified` derives a public
/// key and an HMAC key from it and that is all. A real 2048-bit key would cost every run half a
/// second to generate and measure nothing.
fn offline_client() -> Arc<Client> {
	let private_key = RsaPrivateKey::new(&mut old_rng::thread_rng(), 512)
		.expect("generating the harness's throwaway RSA key");
	let unauthed = UnauthClient::from_config(ClientConfig::default())
		.expect("building an unauthenticated client makes no network call");
	Arc::new(
		unauthed
			.from_stringified(StringifiedClient {
				email: "sync-engine-bench@example.invalid".to_owned(),
				user_id: 1,
				root_uuid: uuid::Uuid::nil().to_string(),
				auth_info: "0".repeat(64),
				private_key: BASE64_STANDARD.encode(
					private_key
						.to_pkcs8_der()
						.expect("a freshly generated key encodes")
						.as_bytes(),
				),
				api_key: String::new(),
				auth_version: 2,
				max_parallel_requests: None,
				max_io_memory_usage: None,
			})
			.expect("the harness's own credentials parse"),
	)
}

/// A fixture with a real engine open on it, converged, with one pair registered and its carried
/// state seeded — the state a pair is in after one whole pass, which is the only state from which a
/// change-scoped pass runs at all.
struct Bed {
	fixture: Fixture,
	engine: SyncEngine,
	pair: super::baseline::PairId,
	rows: usize,
	/// Every file path in the tree, in baseline order, for a change class to pick from.
	files: Vec<String>,
}

/// Build the fixture, open a REAL engine on it, register the pair and converge the baseline.
///
/// None of this is timed: it is what a scenario measures FROM, not what it measures.
async fn prepare_bed(scenario: &Scenario) -> Bed {
	let fixture = Fixture::build(scenario.nodes, &scenario.shape());

	let client = offline_client();
	client
		.configure_cache(fixture.cache_db().to_path_buf(), |_| {})
		.await
		.expect("configuring the cache slot writes a path and nothing else");
	let engine = SyncEngine::open(client, fixture.baseline_db().to_path_buf())
		.await
		.expect("opening an engine on a baseline DB with no pairs in it makes no network call");

	let pair = engine
		.bench_create_pair(
			fixture
				.root()
				.to_str()
				.expect("the probe builds its fixture under a UTF-8 temp path"),
			fixture.remote_root(),
			scenario.mode,
		)
		.expect("registering the pair writes one registry row");

	// The converged baseline: what the local walk and the cache snapshot BOTH say, which is the
	// state a pair is in the pass after it converged.
	let empty = Baseline::default();
	let (scan, _) = scan::scan_local(fixture.root(), &empty, probe_rules(), RuleFiles::Read);
	assert!(
		scan.complete && scan.errors.is_empty(),
		"the fixture tree must scan cleanly: {:?}",
		scan.errors
	);
	let mut builder = plan::ViewBuilder::with_capacity(fixture.remote_root(), fixture.nodes());
	bench_support::snapshot_into(fixture.cache_db(), fixture.remote_root(), &mut builder)
		.expect("streaming the cache snapshot the fixture just wrote");
	let view = builder.finish();
	let rows: Vec<BaselineEntry> = baseline_rows(&scan, &view.nodes);
	let files: Vec<String> = rows
		.iter()
		.filter(|row| row.kind == super::baseline::NodeKind::File)
		.map(|row| row.rel_path.clone())
		.collect();
	let count = rows.len();
	engine
		.bench_seed_rows(pair, &rows)
		.await
		.expect("seeding the converged baseline");

	// What the last whole pass left: without it every pass returns `FullPassReason::FirstPass` and
	// this harness would be timing a whole read while reporting a scoped one.
	engine.bench_seed_carry(pair).await;
	// The pair's changelists, as a watched pair's are: a local watcher is covering the tree, and the
	// caps scale with a tree this size rather than with the floor.
	let changes = engine.pair_changes(pair).await;
	changes.cover_local();
	changes.note_tree_size(count);

	Bed {
		fixture,
		engine,
		pair,
		rows: count,
		files,
	}
}

/// The paths a scenario's change class edits ON DISK, applied once before any sample runs.
fn apply_change(bed: &Bed, scenario: &Scenario) -> Vec<String> {
	let wanted = scenario.change.count(bed.files.len());
	let changed: Vec<String> = bed.files.iter().take(wanted).cloned().collect();
	for (index, rel_path) in changed.iter().enumerate() {
		// Content the fixture's converged rows do not describe, so the pass has something real to
		// plan. Varied per file so no two edits produce the same bytes.
		fs::write(
			bed.fixture.root().join(rel_path),
			format!("changed by the bench harness: {index}").as_bytes(),
		)
		.expect("editing a fixture file");
	}
	changed
}

/// Announce `changed` on the pair's changelist the way its filesystem watcher would, then take the
/// scope that announcement produced.
///
/// Through the real [`PairChanges::note_local_event`](super::changes::PairChanges) rather than by
/// forging a scope: the caps, the rule-file collapse and the unmapped-path collapse are all real
/// behaviour, and a scenario big enough to trip one of them must trip it here too rather than
/// measure a pass no watcher could ever have produced.
async fn announce(bed: &Bed, changed: &[String]) -> super::changes::PassScope {
	let changes = bed.engine.pair_changes(bed.pair).await;
	let root = bed.fixture.root();
	for rel_path in changed {
		let event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
			.add_path(root.join(rel_path));
		changes.note_local_event(root, &event, |_| false);
	}
	changes.take()
}

/// One measured pass: announce, run the REAL `prepare` under a step log, and split what it cost.
async fn one_pass(bed: &Bed, scenario: &Scenario, changed: &[String]) -> (Timing, usize, bool) {
	let mut scope = announce(bed, changed).await;
	let log: Arc<Mutex<Vec<(&'static str, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
	let start = Instant::now();
	let pass = STEPS
		.scope(
			Arc::clone(&log),
			bed.engine.bench_prepare(bed.pair, &mut scope),
		)
		.await
		.expect("a pass over a converged fixture prepares");
	let total = start.elapsed();
	// Consumed rather than discarded: a plan nothing reads is a plan the optimiser may delete.
	let pass = std::hint::black_box(pass);
	let marks = log.lock().unwrap_or_else(PoisonError::into_inner).clone();
	assert!(
		!marks.is_empty(),
		"the pass ran no instrumented step: either the engine's step marks were removed, or this \
		 harness is not driving the engine at all"
	);
	assert_eq!(
		pass.rows, bed.rows,
		"the pass read {} baseline row(s) where this fixture seeded {}: it is not reading the tree \
		 this scenario built",
		pass.rows, bed.rows,
	);
	assert_eq!(
		pass.scoped,
		scenario.expect_scoped,
		"{}: the pass read {} when the scenario expects {}{}",
		scenario.name,
		if pass.scoped { "scoped" } else { "whole" },
		if scenario.expect_scoped {
			"scoped"
		} else {
			"whole"
		},
		pass.full_reason
			.map(|reason| format!(" ({reason:?})"))
			.unwrap_or_default(),
	);
	(
		Timing::split(start, total, &marks),
		pass.actions,
		pass.scoped,
	)
}

/// Run `scenario` `samples` times and return every record it produced.
///
/// The yardstick is a WHOLE pass over the same fixture in the same run, through the same
/// `prepare`: it is the figure that says whether two runs are comparable at all, and taking it
/// through the real function means it prices the same work the engine does.
pub async fn run_scenario(scenario: &Scenario, samples: usize) -> Vec<Record> {
	let bed = prepare_bed(scenario).await;
	let changed = apply_change(&bed, scenario);
	let nodes = bed.rows;
	let hash = scenario.definition_hash();
	let mut records = Vec::new();
	let make =
		|sample: Option<usize>, metric: String, elapsed: Duration, count: Option<u64>| Record {
			scenario: scenario.name.to_owned(),
			scenario_version: scenario.version,
			definition_hash: hash,
			sample,
			metric,
			ms: elapsed.as_secs_f64() * 1e3,
			count,
			nodes,
		};

	// The yardstick FIRST, while nothing this scenario does has touched the fixture's warmth: a
	// whole read of both sides, which is the work change-scoping exists to avoid.
	let start = Instant::now();
	let whole = bed
		.engine
		.bench_prepare_whole(bed.pair)
		.await
		.expect("a whole pass over a converged fixture prepares");
	let yardstick = start.elapsed();
	assert!(
		!std::hint::black_box(&whole).scoped,
		"the yardstick must be a whole read"
	);
	records.push(make(
		None,
		"yardstick_whole_pass".to_owned(),
		yardstick,
		None,
	));

	// Discarded: the page cache, the store's resident baseline and the branch predictors this
	// scenario's recorded samples are then measured in.
	for _ in 0..WARMUPS {
		one_pass(&bed, scenario, &changed).await;
	}

	let mut expected: Option<usize> = None;
	for sample in 0..samples {
		let reps = scenario.reps.max(1);
		// Accumulated ACROSS every rep, because the steps and the total a sample reports have to be
		// two views of ONE quantity. Summing the steps of the last rep against a total averaged
		// over all of them is how a table that looks balanced comes to hide a term — the first cut
		// of this harness did exactly that, and its remainder read 0.0000 ms while 7 % of the pass
		// sat outside every step.
		let mut per_step: Vec<(&'static str, Duration)> = Vec::new();
		let mut passes = Duration::ZERO;
		// Each pass's OWN remainder, summed. Recording this rather than recomputing the remainder
		// from the aggregate keeps one accounting: the figure published is the one each pass
		// measured, and the assert below is what says the two agree.
		let mut remainders = Duration::ZERO;
		let mut actions = 0;
		let mut marks = 0usize;
		let wall = Instant::now();
		for _ in 0..reps {
			let (timing, planned, _) = one_pass(&bed, scenario, &changed).await;
			for (step, elapsed) in &timing.steps {
				match per_step.iter_mut().find(|(name, _)| name == step) {
					Some((_, total)) => *total += *elapsed,
					None => per_step.push((step, *elapsed)),
				}
			}
			passes += timing.total;
			remainders += timing.unattributed;
			marks = timing.steps.len();
			actions = planned;
		}
		let wall = wall.elapsed();
		// Every sample of one scenario must plan the same thing. A sample that suddenly planned
		// something else is measuring a different pass, which is exactly what a run of medians hides.
		match expected {
			None => expected = Some(actions),
			Some(first) => assert_eq!(
				actions, first,
				"{}: sample {sample} planned {actions} action(s) where the first planned {first}; \
				 the samples are not measuring one thing",
				scenario.name
			),
		}
		let stepped: Duration = per_step.iter().map(|(_, elapsed)| *elapsed).sum();
		assert!(
			stepped <= passes,
			"{}: the steps sum to {stepped:?} across {reps} rep(s), past the {passes:?} those \
			 passes took — two step timers overlap",
			scenario.name
		);
		assert_eq!(
			remainders,
			passes - stepped,
			"{}: the passes' own remainders sum to {remainders:?}, but the total minus the steps \
			 is {:?} — the per-pass and aggregate accountings disagree",
			scenario.name,
			passes - stepped,
		);
		let reps32 = u32::try_from(reps).expect("a scenario does not run four billion reps");
		records.push(make(
			Some(sample),
			"total".to_owned(),
			passes / reps32,
			None,
		));
		for (step, elapsed) in &per_step {
			records.push(make(
				Some(sample),
				format!("step:{step}"),
				*elapsed / reps32,
				None,
			));
		}
		// The total minus every step, on the SAME basis, so the table closes: what no mark bounds,
		// including whatever is paid on the way out.
		records.push(make(
			Some(sample),
			"unattributed".to_owned(),
			remainders / reps32,
			None,
		));
		// THIS HARNESS's own per-pass work — announcing the changed paths onto the changelist the
		// way a watcher would — named rather than buried in the remainder above. It is not part of
		// `total`, and no pass pays it.
		records.push(make(
			Some(sample),
			"harness_announce".to_owned(),
			wall.saturating_sub(passes) / reps32,
			None,
		));
		records.push(make(
			Some(sample),
			"marks".to_owned(),
			Duration::ZERO,
			Some(marks as u64),
		));
	}
	assert_eq!(
		expected,
		Some(scenario.change.count(bed.files.len())),
		"{}: the pass must plan one action per changed file, or it is not doing the work this \
		 scenario exists to price",
		scenario.name
	);
	// The fixture goes with the `Bed`: `Fixture`'s `Drop` removes the tree on the way out and on a
	// panic alike, which is the only way an assertion doing its job does not cost the machine its
	// disk.
	records
}

// ---------------------------------------------------------------------------------------------
// The two commands
// ---------------------------------------------------------------------------------------------

/// Run one named scenario, or every one of them, and write ONE fresh result file.
///
/// `SYNC_BENCH_SCENARIO` names it (`all` for every scenario); `SYNC_BENCH_SAMPLES` overrides the
/// sample count; `SYNC_BENCH_OUT` is the DIRECTORY the file lands in.
///
/// # Errors
///
/// When `SYNC_BENCH_SCENARIO` names no scenario, or the result file cannot be written.
pub fn run() -> Result<String, String> {
	let wanted = std::env::var("SYNC_BENCH_SCENARIO").unwrap_or_else(|_| "all".to_owned());
	let samples = std::env::var("SYNC_BENCH_SAMPLES")
		.ok()
		.and_then(|raw| raw.trim().parse::<usize>().ok())
		.unwrap_or(DEFAULT_SAMPLES)
		.max(1);
	let chosen: Vec<&Scenario> = if wanted == "all" {
		SCENARIOS.iter().collect()
	} else {
		vec![scenario(&wanted).ok_or_else(|| {
			format!(
				"no scenario named {wanted:?}; this harness knows: {}",
				SCENARIOS
					.iter()
					.map(|s| s.name)
					.collect::<Vec<_>>()
					.join(", ")
			)
		})?]
	};

	let (commit, toolchain, machine, profile) = run_meta();
	let overhead = mark_overhead_ns();
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.map_err(|e| format!("building the bench runtime: {e}"))?;
	let mut records = Vec::new();
	for scenario in &chosen {
		records.extend(runtime.block_on(run_scenario(scenario, samples)));
	}

	let run = RunFile {
		harness_version: HARNESS_VERSION,
		started: chrono::Utc::now().to_rfc3339(),
		commit,
		toolchain,
		machine,
		profile,
		mark_overhead_ns: overhead,
		records,
	};
	let dir = std::env::var("SYNC_BENCH_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|_| std::env::temp_dir());
	fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
	// One fresh file per run, named so two runs cannot land on one path. Nothing is ever appended
	// to: an appended table is how a stale row comes to read as a fresh one.
	let path = dir.join(format!(
		"sync_bench_{}_{}.json",
		wanted.replace(['/', ' '], "_"),
		uuid::Uuid::new_v4(),
	));
	let json =
		serde_json::to_string_pretty(&run).map_err(|e| format!("encoding the result file: {e}"))?;
	fs::write(&path, json).map_err(|e| format!("writing {}: {e}", path.display()))?;
	Ok(format!("{}\n{}", summarize(&run), path.display()))
}

/// The median of `values`, which must not be empty.
fn median(values: &mut [f64]) -> f64 {
	values.sort_by(f64::total_cmp);
	values[values.len() / 2]
}

/// Every metric of a run as `(scenario, definition_hash, metric) -> samples`.
fn grouped(run: &RunFile) -> BTreeMap<(String, u64, String), Vec<f64>> {
	let mut out: BTreeMap<(String, u64, String), Vec<f64>> = BTreeMap::new();
	for record in &run.records {
		out.entry((
			record.scenario.clone(),
			record.definition_hash,
			record.metric.clone(),
		))
		.or_default()
		.push(record.ms);
	}
	out
}

/// A human table of one run: median, min and max per metric, so a bimodal sample set is visible
/// rather than averaged away.
fn summarize(run: &RunFile) -> String {
	let mut out = String::new();
	writeln!(
		out,
		"# {} {} {} ({}), mark {:.0} ns",
		run.commit.get(..12).unwrap_or(&run.commit),
		run.toolchain,
		run.machine,
		run.profile,
		run.mark_overhead_ns,
	)
	.expect("writing to a String never fails");
	writeln!(out, "scenario\tmetric\tn\tmedian_ms\tmin_ms\tmax_ms\tcount")
		.expect("writing to a String never fails");
	for ((scenario, _, metric), mut samples) in grouped(run) {
		let count = run
			.records
			.iter()
			.find(|record| {
				record.scenario == scenario && record.metric == metric && record.count.is_some()
			})
			.and_then(|record| record.count);
		let (min, max) = samples
			.iter()
			.fold((f64::MAX, f64::MIN), |(lo, hi), value| {
				(lo.min(*value), hi.max(*value))
			});
		writeln!(
			out,
			"{scenario}\t{metric}\t{}\t{:.4}\t{:.4}\t{:.4}\t{}",
			samples.len(),
			median(&mut samples),
			min,
			max,
			count.map_or_else(|| "-".to_owned(), |n| n.to_string()),
		)
		.expect("writing to a String never fails");
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Every scenario's name is unique and its hash distinct — the two things a result file is keyed
	/// by. Two scenarios sharing either would silently merge in every table this harness prints.
	#[test]
	fn scenarios_are_distinctly_named_and_hashed() {
		let mut names: Vec<&str> = SCENARIOS.iter().map(|s| s.name).collect();
		let count = names.len();
		names.sort_unstable();
		names.dedup();
		assert_eq!(names.len(), count, "two scenarios share a name");

		let mut hashes: Vec<u64> = SCENARIOS.iter().map(Scenario::definition_hash).collect();
		hashes.sort_unstable();
		hashes.dedup();
		assert_eq!(hashes.len(), count, "two scenarios hash alike");
	}

	/// Editing a scenario moves its hash even when its version is left alone. This is the whole
	/// mechanism that makes a comparison across a redefinition impossible rather than discouraged.
	#[test]
	fn redefining_a_scenario_changes_its_hash_without_a_version_bump() {
		let original = SCENARIOS[0];
		let mut edited = original;
		edited.file_bytes += 1;
		assert_eq!(edited.version, original.version);
		assert_ne!(
			edited.definition_hash(),
			original.definition_hash(),
			"a scenario's fixture changed and its hash did not: every comparison across that \
			 change would read as a like-for-like one"
		);
	}

	/// The step table closes: the steps and the remainder sum to the total the caller timed.
	#[test]
	fn a_step_table_accounts_for_the_whole_pass() {
		let start = Instant::now();
		let marks = [
			("first", start + Duration::from_millis(3)),
			("second", start + Duration::from_millis(7)),
		];
		let timing = Timing::split(start, Duration::from_millis(10), &marks);
		let summed: Duration = timing.steps.iter().map(|(_, elapsed)| *elapsed).sum();
		assert_eq!(summed + timing.unattributed, timing.total);
		assert_eq!(timing.unattributed, Duration::from_millis(3));
	}

	/// A change class names the files it is meant to.
	#[test]
	fn a_change_class_edits_what_it_says() {
		assert_eq!(Change::Idle.count(1_000), 0);
		assert_eq!(Change::OneFile.count(1_000), 1);
		assert_eq!(Change::PerMille(10).count(1_000), 10);
		// Never zero where the class means "some": a scenario that silently changed nothing would
		// report the idle floor under a name that promises otherwise.
		assert_eq!(Change::PerMille(10).count(3), 1);
	}
}
