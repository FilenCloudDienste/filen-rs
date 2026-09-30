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
//!   runs the pass that many times, each timed on its own clock, sums those and divides by `reps`.
//!   Every record carries the `reps` it was divided by, so the divisor is on the artifact rather
//!   than in whichever revision of this file produced it.
//! - **First-iteration and page-cache effects.** [`WARMUPS`] passes run before the first recorded
//!   sample and are discarded. They are not free measurements — they are the page-cache and
//!   branch-predictor state every recorded sample is then taken in. The yardstick is taken INSIDE
//!   each sample, after those warmups, for the same reason: a ratio between a cold unreplicated
//!   pass and a warm median measures the difference in state as much as the difference in work.
//! - **Instrumentation inside the region it measures.** The step marks are one `Instant::now()`, one
//!   task-local resolution and one push each. The clock is read FIRST, so a mark's own cost lands in
//!   the step AFTER the one it closes rather than in the remainder — and the run records
//!   `mark_overhead_ns`, measured through [`mark`] itself, so a reader can bound the total rather
//!   than take it on trust.
//! - **A memory figure that is really the process's history.** Every memory number is taken in a
//!   FRESH CHILD (see [`measure_memory`]) that opens the tree this process built and runs one pass
//!   over it — not literally nothing else, and `SCENARIOS.md` says where the difference lands: a
//!   child opening an engine on a registry that already holds the pair starts a cache worker the
//!   parent never started. An in-process resident set is the whole run's history — the warmups, every sample and
//!   three whole yardstick passes, minus whatever the allocator has not handed back — and it has
//!   been quoted as the cost of a phase. Each figure is named for whose process it measured
//!   (`mem:fresh_process_*_rss`) or for having been summed from a structure's own capacities
//!   (`*_computed_bytes`), so the two kinds cannot be read as one.
//! - **A phase that quietly moved.** Every pass asserts the exact ORDERED list of steps the engine
//!   marked against [`SCOPED_STEPS`] / [`WHOLE_STEPS`]. Timing what the engine marks stops a step
//!   being timed for work the engine no longer does; it does not, on its own, tell anyone that a
//!   step was renamed or split — and the comparison is keyed by metric NAME.
//!
//! # Why a record cannot be compared with one it is not comparable to
//!
//! Every record carries TWO hashes. `definition_hash` is an FNV-1a over the scenario's own fields:
//! redefining a scenario changes it whether or not anyone remembers to bump `version`.
//! `fixture_hash` is over what the fixture generator actually PRODUCED — the node counts, the first
//! and last path, the size of a file — because everything that turns a declaration into a tree lives
//! in [`probe`](super::probe) and none of it is in the declaration. The comparison command refuses
//! to diff two records whose scenario, fixture or node count disagree.
//!
//! A run is also stamped with the commit it was built from, marked `-dirty` when the working tree
//! did not match it — and a run so marked REFUSES to write into `benches/sync_engine/baseline/`,
//! where the published figures live (see [`refuse_unreproducible`]). Each run writes ONE fresh file;
//! nothing is ever appended to, so a stale row cannot read as a fresh one.

use std::{
	collections::{BTreeMap, BTreeSet, HashMap},
	fmt::Write as _,
	fs,
	path::{Path, PathBuf},
	sync::{Arc, Mutex, PoisonError},
	time::{Duration, Instant},
};

use base64::{Engine as _, prelude::BASE64_STANDARD};
use filen_types::crypto::Blake3Hash;
use notify::{
	Event, EventKind,
	event::{DataChange, ModifyKind, RemoveKind, RenameMode},
};
use rsa::{RsaPrivateKey, pkcs8::EncodePrivateKey};
use serde::{Deserialize, Serialize};

use super::{
	FullPassReason, SyncEngine, SyncMode,
	baseline::{BaselineEntry, NodeKind, PairId},
	changes::{PassScope, RemoteChange, RemoteDeltaEntry},
	engine::PassStructures,
	plan,
	probe::{self, Fixture, NameStyle, Shape, baseline_rows, probe_rules},
	rows::Baseline,
	scan::{self, RuleFiles},
};
use crate::{
	auth::{Client, StringifiedClient, http::ClientConfig, unauth::UnauthClient},
	cache::{RemoteItem, bench_support},
};

/// The record format's own version. Bumped when a field's MEANING changes, so a reader can refuse a
/// file it would misread rather than silently misreading it.
const HARNESS_VERSION: u32 = 4;

/// Passes run and discarded before the first recorded sample of a scenario. The first pass over a
/// fresh fixture reads a cold OS page cache and a cold reader connection (its statement cache and
/// SQLite's page cache), neither of which any later pass pays.
const WARMUPS: usize = 2;

/// What a pass's baseline edits may compute themselves as, whatever they hold: the edits' own
/// struct and the vector of their layers.
const EDITS_BASE_BYTES: u64 = 4 * 1024;

/// What ONE edit — a folded directory move, a confirmed push — may add to that: a layer holding two
/// paths, or a marker entry keyed by one, with room for the map's slack. A move that wrote the rows
/// it carries is hundreds of bytes per ROW, and fails this at any subtree past a handful of rows.
///
/// What a move's pass READS is bounded beside this, per file the move carries and per scenario
/// (see [`Scenario::read_rows_per_carried_file`]).
const EDIT_BYTES: u64 = 1024;

/// Samples per scenario when `SYNC_BENCH_SAMPLES` is unset. Three is the floor a median means
/// anything over, and every one of them is recorded — a median alone hides a bimodal distribution.
const DEFAULT_SAMPLES: usize = 3;

/// Memory children per scenario when `SYNC_BENCH_MEM_SAMPLES` is unset.
///
/// Two, not three: every one is a whole extra process that opens the pair and runs a cold pass,
/// which at a million rows is a minute apiece. Two is the fewest that can say how far a figure
/// MOVES between runs, and a figure whose movement is unknown is not a figure. `0` skips the
/// memory measurement outright, for an edit-test loop.
const DEFAULT_MEM_SAMPLES: usize = 2;

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

tokio::task_local! {
	/// Where [`mark`] ALSO writes, for the duration of a memory child's one pass: the process's
	/// resident set at each boundary the engine marks.
	///
	/// Scoped by nothing else. `current_rss_bytes` forks `ps` on macOS, which belongs nowhere
	/// inside a timed region — so this is set in a memory child and in no recorded timing sample,
	/// and [`measuring_memory`] is how the engine's own tail knows which it is running in.
	static STEP_RSS: Arc<Mutex<Vec<(&'static str, u64)>>>;
}

/// Whether this task is a MEMORY child's measured pass rather than a timed one.
///
/// Read by the engine's benchmark tail to decide whether to sum what its structures cost — an
/// O(nodes) walk that no timed pass may pay. One seam and one tail, so the pass a memory child
/// measures is the pass a timing sample measures.
pub(super) fn measuring_memory() -> bool {
	STEP_RSS.try_with(|_| ()).is_ok()
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
	// A memory child samples the resident set at the same boundaries, so its table and the timing
	// table describe one pass rather than two. Unset in every timed sample, where this costs the
	// task-local probe [`mark_overhead_ns`] already measures and nothing else.
	let _ = STEP_RSS.try_with(|log| {
		let rss = probe::current_rss_bytes();
		log.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.push((name, rss));
	});
}

/// The steps a change-SCOPED pass marks, in the order it marks them.
///
/// Asserted against what the engine actually marked, on every pass of every scenario. Without it a
/// step could be renamed, split or dropped and satisfy every other guard here: `definition_hash`
/// covers the scenario's declaration and not the engine's phases, and [`compare`] keys a metric by
/// NAME — so a renamed phase left a complete-looking table of zeroes on both sides of a diff with
/// nothing saying a phase had gone.
///
/// Changing this list is the signal that result files written before the change cannot be diffed
/// against ones written after it.
pub const SCOPED_STEPS: &[&str] = &[
	"pass_inputs",
	"from_baseline",
	"observe_remote",
	"view_assembly",
	"remote_rules",
	"rule_files_only",
	"observe_local",
	"merge_local",
	"assembly_check",
	"view_filter",
	"facts_merge",
	"confirm_pushes",
	"pending_settle_and_fold",
	"decided_check",
	"local_scan_assembly",
	"fold_dir_moves",
	"prepare_tail",
	"reconcile_and_screen",
	"drop_pass",
];

/// The steps a WHOLE pass marks. `prepare_whole` carries none of its own, so a whole read's
/// breakdown is coarse by construction: nearly all of it lands in `prepare_tail`. That is a property
/// of where the marks are, not of where the time goes.
pub const WHOLE_STEPS: &[&str] = &[
	"pass_inputs",
	"prepare_tail",
	"reconcile_and_screen",
	"drop_pass",
];

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
///
/// Every class but [`Idle`](Self::Idle) and [`FirstSync`](Self::FirstSync) leaves the two sides
/// genuinely diverged, and the scenario's [`Expect`] says what the engine must then plan. A class
/// that silently stopped diverging anything would report the idle floor under a name promising
/// otherwise, which is the single easiest way for this suite to mislead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
	/// Nothing at all: the floor the machinery costs with an empty changelist.
	Idle,
	/// One file's content, and only that file's path announced.
	OneFile,
	/// This many files per thousand, CONSECUTIVE in baseline order — so the edits cluster into a
	/// few leaf directories, which is the shape a working session leaves behind.
	///
	/// "Baseline order" is PATH order: `prepare_bed` sorts the rows before a change class sees
	/// them, and asserts it. It used to be raw `HashMap` iteration order, which made "consecutive"
	/// a fresh random permutation on every run — so this class and
	/// [`ScatteredPerMille`](Self::ScatteredPerMille) drew the same thing, and no scenario's
	/// edited file set was reproducible between two runs of one binary.
	PerMille(u32),
	/// EXACTLY this many files, consecutive in baseline order.
	///
	/// For a group whose rows must hold the change VOLUME fixed while the tree varies: a per-mille
	/// of a shape's own file population is a different number of edits on every shape, and the
	/// shape group compared 40 edits against 102 while its documentation said the rows differed in
	/// exactly one way.
	Files(u32),
	/// The same count STRIDED across the whole tree instead, so the edits touch as many distinct
	/// directories as there are files. The pair to [`PerMille`](Self::PerMille): same number of
	/// changed files, maximally different locality.
	ScatteredPerMille(u32),
	/// This many files per thousand REMOVED from disk. Above the guard's limit the pass plans
	/// nothing and holds everything, which is the case a `held` expectation exists to see.
	DeletePerMille(u32),
	/// This many files per thousand renamed in place, each announced as the two-ended rename a
	/// watcher reports.
	RenamePerMille(u32),
	/// One whole directory renamed, which the plan must fold into ONE move carrying its subtree
	/// rather than a re-upload of every child.
	MoveDir,
	/// No baseline at all: the first pass a pair ever runs, which reads both sides whole and plans
	/// the entire tree.
	FirstSync,
	/// This many files per thousand edited locally AND announced as changed remotely, so BOTH
	/// changelists are non-empty and `observe_remote` has a real delta to apply — the only scenario
	/// here that exercises the remote half with more than an empty list.
	///
	/// It is NOT a conflict scenario, despite diverging both sides: measured at 10k it plans one
	/// action per edited file and ZERO conflicts. Announcing a remote upsert through
	/// `note_owed_remote` does not, on its own, make the reconcile call the path both-sides-changed.
	/// A conflict-heavy scenario is not covered by this matrix — see the scenario document.
	BothSidesPerMille(u32),
	/// The pass after an UPLOAD: this many files per thousand carry the row an unconfirmed push of
	/// ours leaves — the content this side holds, the version it minted, and an agreed-content
	/// marker still on the previous content — and the cache has announced each of them long enough
	/// ago to confirm it. On top of that ONE other file is edited, so the pass has something to
	/// plan and [`Count::PerChanged`] reads it.
	///
	/// What it prices is the confirmation, not the edit: advancing an agreed-content marker is the
	/// one write a change-scoped pass makes to its own copy of the baseline, and that write is
	/// what used to copy the whole resident tree. `one_pass` asserts the pass confirmed every one
	/// of the pushes, so a pass that stopped confirming cannot report itself cheap.
	AfterUpload(u32),
}

impl Change {
	fn label(self) -> String {
		match self {
			Self::Idle => "idle".to_owned(),
			Self::OneFile => "one_file".to_owned(),
			Self::PerMille(per_mille) => format!("per_mille_{per_mille}"),
			Self::Files(files) => format!("files_{files}"),
			Self::ScatteredPerMille(per_mille) => format!("scattered_per_mille_{per_mille}"),
			Self::DeletePerMille(per_mille) => format!("delete_per_mille_{per_mille}"),
			Self::RenamePerMille(per_mille) => format!("rename_per_mille_{per_mille}"),
			Self::MoveDir => "move_dir".to_owned(),
			Self::FirstSync => "first_sync".to_owned(),
			Self::BothSidesPerMille(per_mille) => format!("both_sides_per_mille_{per_mille}"),
			Self::AfterUpload(per_mille) => format!("after_upload_per_mille_{per_mille}"),
		}
	}

	/// How many of `files` this class touches.
	fn count(self, files: usize) -> usize {
		match self {
			Self::Idle | Self::FirstSync => 0,
			// One directory, not one file — but one thing either way, which is what the
			// expectations are written against.
			Self::OneFile | Self::MoveDir | Self::AfterUpload(_) => 1,
			Self::Files(files) => files as usize,
			Self::PerMille(per_mille)
			| Self::ScatteredPerMille(per_mille)
			| Self::DeletePerMille(per_mille)
			| Self::RenamePerMille(per_mille)
			| Self::BothSidesPerMille(per_mille) => (files * per_mille as usize / 1000).max(1),
		}
	}

	/// How many of `files` carry an unconfirmed push of ours (see [`AfterUpload`](Self::AfterUpload)).
	fn pushes(self, files: usize) -> usize {
		match self {
			Self::AfterUpload(per_mille) => (files * per_mille as usize / 1000).max(1),
			_ => 0,
		}
	}

	/// Whether the fixture's converged rows are written to the baseline before the pass runs. Only
	/// [`FirstSync`](Self::FirstSync) says no — that is what makes it a first sync.
	fn seeds_baseline(self) -> bool {
		!matches!(self, Self::FirstSync)
	}
}

/// How many of something a scenario's pass must plan.
///
/// Written against the scenario's OWN definition — the files its change class touched, the nodes
/// its fixture holds — rather than against a figure read off a previous run, so an expectation says
/// what the engine is supposed to do rather than what it happened to do that afternoon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Count {
	/// This many, exactly.
	Exactly(u64),
	/// One per file (or directory) the change class touched.
	PerChanged,
	/// One per node the fixture holds — a first sync plans the whole tree.
	PerNode,
	/// At least this many, for a plan whose exact size the scenario's definition does not pin.
	/// Deliberately rare: an `AtLeast(0)` would be no expectation at all.
	AtLeast(u64),
}

impl Count {
	fn holds(self, actual: usize, changed: usize, nodes: usize) -> bool {
		match self {
			Self::Exactly(expected) => actual as u64 == expected,
			Self::PerChanged => actual == changed,
			Self::PerNode => actual == nodes,
			Self::AtLeast(least) => actual as u64 >= least,
		}
	}

	fn label(self) -> String {
		match self {
			Self::Exactly(expected) => format!("exactly {expected}"),
			Self::PerChanged => "one per changed file".to_owned(),
			Self::PerNode => "one per node".to_owned(),
			Self::AtLeast(least) => format!("at least {least}"),
		}
	}

	/// Whether a pass that planned NOTHING would satisfy this. The one thing every scenario but the
	/// idle floor has to rule out (see [`Expect::demands_work`]).
	fn satisfied_by_nothing(self, changed: usize, nodes: usize) -> bool {
		self.holds(0, changed, nodes)
	}
}

/// The whole shape of the plan a scenario's pass must produce, checked EVERY sample.
///
/// Four numbers rather than one because a pass can go wrong in ways an action count cannot see: a
/// mass delete whose deletions were all held and a pass that planned nothing both report
/// `actions: 0`, and a directory move that was folded and one that re-uploaded the subtree differ
/// only in `dir_moves`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expect {
	/// Actions an approved pass would execute, after the guard.
	pub actions: Count,
	/// Actions the guard held back.
	pub held: Count,
	/// Paths surfaced as two-way conflicts.
	pub conflicts: Count,
	/// Directory moves folded out of the plan, each carrying its subtree.
	pub dir_moves: Count,
}

impl Expect {
	/// Whether this expectation rules out a pass that did nothing whatsoever.
	///
	/// The structural answer to "a benchmark that quietly stops doing work reports itself fast":
	/// every scenario but the idle floor is checked against this by a unit test, so a scenario
	/// cannot be added with an expectation that a degenerate pass would satisfy.
	fn demands_work(&self, changed: usize, nodes: usize) -> bool {
		!(self.actions.satisfied_by_nothing(changed, nodes)
			&& self.held.satisfied_by_nothing(changed, nodes)
			&& self.conflicts.satisfied_by_nothing(changed, nodes)
			&& self.dir_moves.satisfied_by_nothing(changed, nodes))
	}
}

/// Check a scenario's own definition against the fixture just built for it, before anything is
/// measured on it.
///
/// Called by every RUN, not only by this module's unit tests. These invariants are what stop a
/// scenario reporting a believable figure for a pass that did nothing, and a harness that checked
/// them only under `cargo test` would not be checking them for the person actually running it —
/// which is the moment it matters.
fn validate(scenario: &Scenario, changed: usize, nodes: usize) {
	assert!(
		scenario.reps >= 1,
		"{}: a scenario runs at least one pass per sample",
		scenario.name
	);
	if !scenario.read.is_scoped() {
		assert_eq!(
			scenario.reps, 1,
			"{}: a whole-read pass is nowhere near the clock's resolution, so repeating it inside one \
			 sample only multiplies a very expensive read",
			scenario.name
		);
	}
	// A move without a read bound would read its subtree any number of times unnoticed, and a bound
	// on any other class has no carried files to be multiplied by.
	assert_eq!(
		scenario.read_rows_per_carried_file.is_some(),
		scenario.change == Change::MoveDir,
		"{}: a directory move, and only a directory move, declares the rows its pass may read per \
		 file it carries",
		scenario.name
	);
	match scenario.change {
		// The floor: the one scenario that plans nothing, and it has to say so outright.
		Change::Idle => {
			assert_eq!(
				scenario.expect, NOTHING,
				"{}: the idle floor is the only scenario that may expect an empty plan",
				scenario.name
			);
			return;
		}
		// Touches nothing on disk on purpose: what it measures is the ABSENCE of a baseline, so its
		// plan is read per NODE rather than per changed file.
		Change::FirstSync => {}
		_ => assert!(
			changed > 0,
			"{}: the change class touched no file at all, so this scenario measures the idle floor \
			 under a name that promises otherwise",
			scenario.name
		),
	}
	assert!(
		scenario.expect.demands_work(changed, nodes),
		"{}: a pass that planned, held and conflicted over nothing whatsoever would satisfy this \
		 scenario's expectation, so it cannot tell a working engine from a degenerate one",
		scenario.name
	);
}

/// Check the fixture ON DISK against the scenario's own declaration, after the change class has
/// been applied and before anything is measured on it.
///
/// [`validate`] checks the PLAN a scenario demands; this checks the TREE it demands. They are
/// different holes, and only the first was covered: `twoway_large_files_*` declared a megabyte a
/// file and shipped measuring 32-byte files for a whole round, because `fs::write` truncates and
/// every guard the harness had looked at the plan. The plan was identical either way.
///
/// Every class is checked at ITS OWN ends rather than only the one that rewrites content. A
/// rename that lost its file's bytes, a delete that left the file on disk, and a generator that
/// built the UNTOUCHED population at a size its scenario never declared are the same defect in
/// three costumes, and the first cut of this function recognised one of them: it returned early
/// for every class but `EDIT` and looked only at the handful of paths that class had just written.
/// A scenario's untouched files are most of what its pass walks and hashes.
///
/// The untouched population is checked by ONE file and not all of them: the first file row the
/// change class did not take. A stat per node would be a walk of the whole tree per scenario,
/// which at a million rows costs more than the pass being measured — and every file the generator
/// writes comes off one `content()` call, so one wrong size is all of them.
fn validate_fixture(
	scenario: &Scenario,
	root: &Path,
	nodes: usize,
	rows: &[BaselineEntry],
	applied: &Applied,
) {
	assert!(
		nodes >= scenario.nodes,
		"{}: the fixture holds {nodes} node(s) where the scenario asks for at least {}",
		scenario.name,
		scenario.nodes,
	);
	let declared = scenario.file_bytes as u64;
	let sized = |rel_path: &str, what: &str| {
		let len = fs::metadata(root.join(rel_path))
			.unwrap_or_else(|e| panic!("{}: {what} {rel_path} is not on disk: {e}", scenario.name))
			.len();
		assert_eq!(
			len, declared,
			"{}: {what} {rel_path} is {len} byte(s) on disk where the scenario declares \
			 {declared}. This scenario is measuring a tree of a different shape from the one it \
			 names — and the plan it produces is identical either way, which is why nothing else \
			 here would notice",
			scenario.name,
		);
	};
	// A file the change class did NOT touch, and is not under a directory it moved: the population
	// the pass spends most of itself on, which nothing here used to look at.
	let touched: BTreeSet<&str> = applied.touched.iter().map(String::as_str).collect();
	let under_a_rename = |rel_path: &str| {
		applied.renamed.iter().any(|(from, _)| {
			rel_path == from
				|| (rel_path.starts_with(from.as_str())
					&& rel_path.as_bytes().get(from.len()) == Some(&b'/'))
		})
	};
	let untouched = rows.iter().find(|row| {
		row.kind == NodeKind::File
			&& !touched.contains(row.rel_path.as_str())
			&& !under_a_rename(&row.rel_path)
	});
	// A class that took every file leaves nothing to check here, and the check would then pass by
	// being SKIPPED rather than by holding — the one signal being its absence. No class in the
	// matrix does that today (the largest takes six files in ten), and a first sync is exempt
	// because it seeds no baseline to find a file row in.
	assert!(
		untouched.is_some() || rows.iter().all(|row| row.kind != NodeKind::File),
		"{}: the change class took or moved every file the baseline holds, so the untouched \
		 population went unchecked and this fixture was verified at its ends alone",
		scenario.name
	);
	if let Some(untouched) = untouched {
		sized(&untouched.rel_path, "the untouched file");
	}
	// BOTH ends of every rename. A source still on disk is a file the watcher's two-ended event
	// describes and the tree contradicts; a destination that lost its bytes prices a different read
	// under the same row.
	for (from, to) in &applied.renamed {
		assert!(
			!root.join(from).exists(),
			"{}: {from} is still on disk after the change class renamed it away, so the tree holds \
			 a file this scenario announced as moved",
			scenario.name
		);
		let moved = fs::metadata(root.join(to))
			.unwrap_or_else(|e| panic!("{}: the renamed {to} is not on disk: {e}", scenario.name));
		// A moved DIRECTORY has no size to check; a moved file does, and it must be the one its
		// scenario declares.
		if moved.is_file() {
			sized(to, "the renamed file");
		}
	}
	if applied.touched_kind == EDIT {
		for rel_path in &applied.touched {
			sized(rel_path, "the edited file");
		}
	} else {
		// A delete leaves nothing to size, so what there is to check is that it is gone: a class
		// that announced removals over files still on disk measures a pass that plans against a
		// tree nobody removed anything from.
		for rel_path in &applied.touched {
			assert!(
				!root.join(rel_path).exists(),
				"{}: {rel_path} is still on disk after the change class deleted it",
				scenario.name
			);
		}
	}
}

/// Which read a scenario's pass must perform.
///
/// One value rather than a `bool` beside an `Option<FullPassReason>`, because the two can disagree:
/// a pass that fell back to a whole read for a reason nobody expected is a scenario measuring
/// something else, and a plausible figure for the wrong pass has been published here before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectRead {
	/// The pass must narrow its read to what changed.
	Scoped,
	/// The pass must read both sides whole, for EXACTLY this reason.
	Whole(FullPassReason),
}

impl ExpectRead {
	fn is_scoped(self) -> bool {
		matches!(self, Self::Scoped)
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
	/// How the tree's directories and files are NAMED, which decides what the NFC pass, the case
	/// fold and every materialised path cost (see [`NameStyle`]).
	pub names: NameStyle,
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
	/// Which read the pass must perform, checked every sample.
	pub read: ExpectRead,
	/// The plan the pass must produce, checked every sample.
	pub expect: Expect,
	/// For a directory move, the baseline rows its pass may read per file the move carries: the
	/// memory child fails the run when `walk:baseline_read_rows` goes past this times the carried
	/// files, as it fails one whose edits go past `EDIT_BYTES` per move. Each read of the moved
	/// subtree costs a row per file, so a bound set at the multiple the scenario measured, rounded
	/// up to the next whole one, fails a pass that reads the subtree once more. Per scenario rather
	/// than one constant, because the multiple is not one: a directory holding half the pair reads
	/// eleven rows a file, while a leaf of twenty files reads twenty to thirty-seven, most of them
	/// the page the fold's cursor reads ahead past the leaf (208 of the 10k tree's 393). Lower it
	/// when a read is removed. `None` for every other change class; `validate` holds the two
	/// together.
	///
	/// Not in [`definition_hash`](Self::definition_hash): it decides what a run REFUSES, not what
	/// it measures, and a hash it moved would orphan a scenario's published rows each time a read
	/// was removed and the bound lowered after it.
	pub read_rows_per_carried_file: Option<u64>,
}

/// FNV-1a over a spec string. Small, dependency-free and stable across builds, which is all a
/// comparison key needs.
fn fnv1a(spec: &str) -> u64 {
	let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
	for byte in spec.as_bytes() {
		hash ^= u64::from(*byte);
		hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
	}
	hash
}

impl Scenario {
	/// An FNV-1a over every field that decides what this measures.
	///
	/// This is what makes a wrong comparison impossible rather than merely discouraged: edit any
	/// field and the hash moves, so the comparison command refuses to set the new rows beside the old
	/// even if nobody remembered to bump [`version`](Self::version).
	///
	/// It covers the DECLARATION only. Everything that turns a declaration into a tree lives in
	/// [`probe`](super::probe) — the branching formula, the name styles, the padding — and a change
	/// there moves every fixture while leaving every one of these hashes alone. That is what
	/// `fixture_hash` is for.
	fn definition_hash(&self) -> u64 {
		let mut spec = String::new();
		write!(
			spec,
			"v{} {} {}f/{}d/{}b/{:?} n{} {} {:?} reps{} {:?} {:?}",
			self.version,
			self.name,
			self.files_per_leaf,
			self.depth,
			self.file_bytes,
			self.names,
			self.nodes,
			self.change.label(),
			self.mode,
			self.reps,
			self.read,
			self.expect,
		)
		.expect("writing to a String never fails");
		fnv1a(&spec)
	}

	fn shape(&self) -> Shape {
		Shape::new(self.files_per_leaf, self.depth, self.file_bytes, self.names)
	}
}

/// One action per changed file and nothing else: no conflict, no hold, no folded move.
const PER_CHANGED: Expect = Expect {
	actions: Count::PerChanged,
	held: Count::Exactly(0),
	conflicts: Count::Exactly(0),
	dir_moves: Count::Exactly(0),
};

/// No plan at all — the idle floor, and the ONLY expectation a degenerate pass satisfies. A unit
/// test holds it to scenarios whose change class is [`Change::Idle`].
const NOTHING: Expect = Expect {
	actions: Count::Exactly(0),
	held: Count::Exactly(0),
	conflicts: Count::Exactly(0),
	dir_moves: Count::Exactly(0),
};

/// A scenario on the balanced 20-files-per-leaf, three-level, 73-byte ASCII tree — the shape every
/// figure recorded before this harness existed was taken on, so a row using it can be read against
/// that history.
const fn balanced(
	name: &'static str,
	nodes: usize,
	change: Change,
	mode: SyncMode,
	reps: usize,
	read: ExpectRead,
	expect: Expect,
) -> Scenario {
	Scenario {
		name,
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		names: NameStyle::Ascii,
		nodes,
		change,
		mode,
		reps,
		read,
		expect,
		read_rows_per_carried_file: None,
	}
}

/// Every scenario this harness knows, keyed by name.
///
/// Five groups, and the group a row belongs to is what its figure is FOR:
///
/// 1. **Change classes** on the balanced shape at 10k — what each kind of change costs.
/// 2. **Shapes** at 10k with a FIXED number of edits — what the tree's geometry and its names
///    cost. Every row of the group changes the same count, `twoway_balanced_100_edits_10k`
///    included, because the shapes hold different file populations and a per-mille of each is a
///    different amount of work. The node counts still differ (the shape rounds up to a whole
///    branching factor), which is why every record carries the nodes its fixture held.
/// 3. **Modes** at 10k — what a pair's direction costs. NOT the `RuleFiles::Only` scan: that
///    iterates the rule-file INDEX, one entry per `.filenignore`, and this fixture holds none, so
///    the phase measures 0.0000-0.0006 ms on every mode row. What these rows price is the rest of
///    the mode difference.
/// 4. **Sizes** — the same three headline classes at 1k, 100k and 1M, so a figure can be read
///    against the tree it was taken on.
/// 5. The 1k rows, which exist to be fast: the whole group runs in seconds, for an edit-test loop.
pub const SCENARIOS: &[Scenario] = &[
	// ----- 1k: the edit-test loop -----
	balanced(
		"twoway_idle_1k",
		1_000,
		Change::Idle,
		SyncMode::TwoWay,
		32,
		ExpectRead::Scoped,
		NOTHING,
	),
	balanced(
		"twoway_one_file_1k",
		1_000,
		Change::OneFile,
		SyncMode::TwoWay,
		8,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_one_percent_1k",
		1_000,
		Change::PerMille(10),
		SyncMode::TwoWay,
		4,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	// ----- change classes, balanced shape, 10k -----
	balanced(
		"twoway_idle_10k",
		10_000,
		Change::Idle,
		SyncMode::TwoWay,
		// The floor is tens of microseconds: one pass per clock would be measuring the clock.
		32,
		ExpectRead::Scoped,
		NOTHING,
	),
	balanced(
		"twoway_one_file_10k",
		10_000,
		Change::OneFile,
		SyncMode::TwoWay,
		8,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_one_percent_10k",
		10_000,
		Change::PerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_ten_percent_10k",
		10_000,
		Change::PerMille(100),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_one_percent_scattered_10k",
		10_000,
		Change::ScatteredPerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_small_delete_10k",
		10_000,
		// Ten files: under the guard's floor, so they are planned rather than held, and far under
		// the changelist cap, so the pass still narrows its read.
		Change::DeletePerMille(1),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_mass_delete_10k",
		10_000,
		// Sixty per cent of the files. Past the guard's limit (half the tracked set), and past the
		// changelist's cap (a quarter of it) on the way — which is why a guard-tripping delete is
		// necessarily a WHOLE pass on a watched pair. Both are real engine behaviour, so the
		// scenario states them rather than fighting them.
		Change::DeletePerMille(600),
		SyncMode::TwoWay,
		1,
		ExpectRead::Whole(FullPassReason::LocalOverflow),
		Expect {
			actions: Count::Exactly(0),
			held: Count::PerChanged,
			conflicts: Count::Exactly(0),
			dir_moves: Count::Exactly(0),
		},
	),
	balanced(
		"twoway_rename_storm_10k",
		10_000,
		Change::RenamePerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	Scenario {
		// 393 rows for the twenty files the leaf carries.
		read_rows_per_carried_file: Some(20),
		..balanced(
			"twoway_dir_move_10k",
			10_000,
			Change::MoveDir,
			SyncMode::TwoWay,
			4,
			ExpectRead::Scoped,
			Expect {
				actions: Count::PerChanged,
				held: Count::Exactly(0),
				conflicts: Count::Exactly(0),
				dir_moves: Count::PerChanged,
			},
		)
	},
	balanced(
		"twoway_both_changelists_10k",
		10_000,
		Change::BothSidesPerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		// One action per edited file and NO conflicts — measured, not assumed. The remote delta is
		// real and 102 entries long, so this prices `observe_remote` against a non-empty list. It does
		// NOT price a conflict, and the zero is pinned so that stays visible.
		PER_CHANGED,
	),
	balanced(
		"twoway_first_sync_10k",
		10_000,
		Change::FirstSync,
		SyncMode::TwoWay,
		1,
		ExpectRead::Whole(FullPassReason::EmptyBaseline),
		Expect {
			actions: Count::PerNode,
			held: Count::Exactly(0),
			conflicts: Count::Exactly(0),
			dir_moves: Count::Exactly(0),
		},
	),
	// ----- shapes, 100 edits, 10k -----
	//
	// An ABSOLUTE edit count rather than a per-mille, so the only thing varying across the group is
	// the tree. `twoway_balanced_100_edits_10k` is the control: the same 100 edits on the shape
	// every figure recorded before this harness existed was taken on.
	balanced(
		"twoway_balanced_100_edits_10k",
		10_000,
		Change::Files(100),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	Scenario {
		name: "twoway_wide_flat_100_edits_10k",
		version: 1,
		// One directory holding every file: where the per-directory children vector's O(children)
		// insert and the folded-order comparison are paid at their worst.
		files_per_leaf: 10_000,
		depth: 1,
		file_bytes: 73,
		names: NameStyle::Ascii,
		nodes: 10_000,
		change: Change::Files(100),
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: PER_CHANGED,
		read_rows_per_carried_file: None,
	},
	Scenario {
		name: "twoway_deep_narrow_100_edits_10k",
		version: 1,
		// Twelve levels of binary branching: what ancestor walks and path materialisation pay for.
		files_per_leaf: 1,
		depth: 12,
		file_bytes: 73,
		names: NameStyle::Ascii,
		nodes: 10_000,
		change: Change::Files(100),
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: PER_CHANGED,
		read_rows_per_carried_file: None,
	},
	Scenario {
		name: "twoway_long_paths_100_edits_10k",
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		names: NameStyle::Long,
		nodes: 10_000,
		change: Change::Files(100),
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: PER_CHANGED,
		read_rows_per_carried_file: None,
	},
	Scenario {
		name: "twoway_unicode_100_edits_10k",
		version: 1,
		files_per_leaf: 20,
		depth: 3,
		file_bytes: 73,
		names: NameStyle::Unicode,
		nodes: 10_000,
		change: Change::Files(100),
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: PER_CHANGED,
		read_rows_per_carried_file: None,
	},
	// ----- bytes rather than nodes: its own group of one -----
	//
	// NOT part of the shape group above, which holds 10k nodes and 100 edits: this holds 530 nodes
	// and 50, and its point is the BYTES. `edit_files` rewrites a file at its declared size, and
	// `validate_fixture` asserts that on disk, because this scenario shipped once measuring
	// 32-byte files — `fs::write` truncates, and nothing here looked at the tree.
	Scenario {
		name: "twoway_large_files_50_edits_512",
		version: 1,
		// A megabyte a file, so re-hashing a changed file dominates instead of vanishing. Small in
		// NODES on purpose: this shape costs half a gigabyte of disk at five hundred files.
		files_per_leaf: 20,
		depth: 2,
		file_bytes: 1024 * 1024,
		names: NameStyle::Ascii,
		nodes: 512,
		change: Change::Files(50),
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: PER_CHANGED,
		read_rows_per_carried_file: None,
	},
	// ----- modes, 10k -----
	balanced(
		"pull_idle_10k",
		10_000,
		Change::Idle,
		SyncMode::RemoteToLocal,
		8,
		ExpectRead::Scoped,
		NOTHING,
	),
	balanced(
		"pull_one_file_10k",
		10_000,
		Change::OneFile,
		SyncMode::RemoteToLocal,
		8,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"pull_one_percent_10k",
		10_000,
		Change::PerMille(10),
		SyncMode::RemoteToLocal,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"backup_local_one_percent_10k",
		10_000,
		Change::PerMille(10),
		SyncMode::LocalBackup,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"backup_remote_one_percent_10k",
		10_000,
		Change::PerMille(10),
		SyncMode::RemoteBackup,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	// ----- sizes -----
	balanced(
		"twoway_idle_100k",
		100_000,
		Change::Idle,
		SyncMode::TwoWay,
		32,
		ExpectRead::Scoped,
		NOTHING,
	),
	balanced(
		"twoway_one_file_100k",
		100_000,
		Change::OneFile,
		SyncMode::TwoWay,
		8,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_one_percent_100k",
		100_000,
		Change::PerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_ten_percent_100k",
		100_000,
		Change::PerMille(100),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	Scenario {
		// 582 rows for the twenty files the leaf carries.
		read_rows_per_carried_file: Some(30),
		..balanced(
			"twoway_dir_move_100k",
			100_000,
			Change::MoveDir,
			SyncMode::TwoWay,
			4,
			ExpectRead::Scoped,
			Expect {
				actions: Count::PerChanged,
				held: Count::Exactly(0),
				conflicts: Count::Exactly(0),
				dir_moves: Count::PerChanged,
			},
		)
	},
	// A directory holding HALF the pair renamed: two top-level directories of 50k files each, one
	// of them moved. The move above carries the deepest directory, a leaf of twenty files, so its
	// cost says nothing about how a fold scales with what the moved directory holds; this is the
	// other end of that, with the other half left untouched for the fixture checks to verify. A pass
	// that folded the move by writing the rows it carries holds every one of them in its edits, and
	// the memory child's bound on those fails it; one that reads the moved subtree once more than it
	// needs to reads 50,000 more rows, and the bound on its reads fails that.
	Scenario {
		name: "twoway_top_dir_move_100k",
		version: 1,
		files_per_leaf: 50_000,
		depth: 1,
		file_bytes: 73,
		names: NameStyle::Ascii,
		nodes: 100_000,
		change: Change::MoveDir,
		mode: SyncMode::TwoWay,
		reps: 1,
		read: ExpectRead::Scoped,
		expect: Expect {
			actions: Count::PerChanged,
			held: Count::Exactly(0),
			conflicts: Count::Exactly(0),
			dir_moves: Count::PerChanged,
		},
		// 550,076 rows for 50,000 files: the moved subtree, read eleven times.
		read_rows_per_carried_file: Some(12),
	},
	balanced(
		"twoway_after_upload_100k",
		100_000,
		Change::AfterUpload(10),
		SyncMode::TwoWay,
		4,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_first_sync_100k",
		100_000,
		Change::FirstSync,
		SyncMode::TwoWay,
		1,
		ExpectRead::Whole(FullPassReason::EmptyBaseline),
		Expect {
			actions: Count::PerNode,
			held: Count::Exactly(0),
			conflicts: Count::Exactly(0),
			dir_moves: Count::Exactly(0),
		},
	),
	balanced(
		"twoway_idle_1m",
		1_000_000,
		Change::Idle,
		SyncMode::TwoWay,
		32,
		ExpectRead::Scoped,
		NOTHING,
	),
	balanced(
		"twoway_one_file_1m",
		1_000_000,
		Change::OneFile,
		SyncMode::TwoWay,
		8,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	balanced(
		"twoway_one_percent_1m",
		1_000_000,
		Change::PerMille(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
	// The two passes that edit their own view of the baseline — a folded directory move and a
	// confirmed push — at the size where a copy of the tree was the pass's widest point, before
	// those edits moved beside the tree instead of into a copy of it.
	Scenario {
		// 738 rows for the twenty files the leaf carries.
		read_rows_per_carried_file: Some(37),
		..balanced(
			"twoway_dir_move_1m",
			1_000_000,
			Change::MoveDir,
			SyncMode::TwoWay,
			1,
			ExpectRead::Scoped,
			Expect {
				actions: Count::PerChanged,
				held: Count::Exactly(0),
				conflicts: Count::Exactly(0),
				dir_moves: Count::PerChanged,
			},
		)
	},
	balanced(
		"twoway_after_upload_1m",
		1_000_000,
		Change::AfterUpload(10),
		SyncMode::TwoWay,
		1,
		ExpectRead::Scoped,
		PER_CHANGED,
	),
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
	/// For a metric that counts rather than times: a plan's actions, a pass's marks, and BYTES for
	/// every `mem:` metric. A count encoded as a duration reads as `0.000013 ms` in a column headed
	/// milliseconds, which is exactly the kind of figure this harness exists to stop anyone
	/// quoting. [`unit`] says which unit a metric is in, and every table prints it.
	pub count: Option<u64>,
	/// Nodes the fixture actually held, so a figure is never read per-node against the wrong tree.
	pub nodes: usize,
	/// See [`fixture_signature`]: what the generator actually built, as opposed to what the
	/// scenario declared.
	pub fixture_hash: u64,
	/// Passes per sample this figure was divided by. On the record rather than only in the source,
	/// so the divisor can be read off the artifact.
	pub reps: usize,
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
	/// What allocator every RSS figure in this file was taken through (see [`allocator`]).
	pub allocator: String,
	/// What one step mark costs, measured on this machine in this run.
	pub mark_overhead_ns: f64,
	pub records: Vec<Record>,
}

/// The commit a run is stamped with, marked `-dirty` when the tree it was built from does not
/// match it.
///
/// `git rev-parse HEAD` says nothing about the working tree, and a published baseline once stamped
/// a commit that held three of its thirty-three scenarios — the binary had been built from an
/// uncommitted tree. A stamp that cannot be wrong about that is the difference between a result
/// file and a note.
fn commit_stamp(head: &str, porcelain: &str) -> String {
	if porcelain.trim().is_empty() {
		head.to_owned()
	} else {
		format!("{head}-dirty")
	}
}

/// Where the PUBLISHED result files live: the directory `BASELINE.md` quotes its figures from.
fn published_dir() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("benches/sync_engine/baseline")
}

/// Refuse a run that would write into `published` a file its own stamp cannot reproduce.
///
/// The stamp says `-dirty` and nothing reads it: a whole round of published figures carried one,
/// every "after" in it measured on code that was never committed. A file in `published` is a
/// figure somebody will quote, so it has to name a commit that builds the binary it came from — a
/// dirty tree cannot, and neither can a stamp git could not produce at all (`unknown`). Anywhere
/// else a dirty run is an edit-test loop and is left alone; its file still says `-dirty`.
///
/// Compared as canonical paths, so `baseline/../baseline` and a symlink to it are the same place.
/// A `published` that does not exist holds nothing, and nothing that exists can be under it.
fn refuse_unreproducible(out: &Path, published: &Path, commit: &str) -> Result<(), String> {
	let (Ok(out), Ok(published)) = (out.canonicalize(), published.canonicalize()) else {
		return Ok(());
	};
	if !out.starts_with(&published) {
		return Ok(());
	}
	if commit.ends_with("-dirty") || commit.starts_with("unknown") {
		return Err(format!(
			"refusing to write a result file into {} from a build stamped {commit:?}: a published \
			 figure must name a commit that reproduces it. Commit the tree first, or point \
			 SYNC_BENCH_OUT somewhere else",
			published.display()
		));
	}
	Ok(())
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
	let commit = commit_stamp(
		&output("git", &["rev-parse", "HEAD"]),
		&output("git", &["status", "--porcelain"]),
	);
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

/// What allocator this run's memory figures were taken through.
///
/// Every RSS figure is an allocator's answer as much as the engine's: what it returns to the
/// kernel and what it holds as free pages is its policy, and that is most of the difference
/// between a pass's widest point and what stays resident after it. This crate declares no
/// `#[global_allocator]`, so it is the platform's — stamped on the run rather than assumed,
/// because a build that added one would move every memory figure and nothing else here would say
/// so.
fn allocator() -> String {
	format!(
		"system {} (filen-sdk-rs declares no #[global_allocator])",
		std::env::consts::OS
	)
}

/// A `usize` from the environment, or `default`.
///
/// Panics on a value it cannot parse rather than falling back: a mistyped `SYNC_BENCH_SAMPLES`
/// that silently ran the default is a run nobody knows the sample count of.
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

/// What one [`mark`] costs here, so a reader can bound what the instrumentation contributed to the
/// steps it bounds rather than take it on trust.
///
/// Through [`mark`] ITSELF, inside a [`STEPS`] scope over a log that starts empty — not an inlined
/// imitation of it over a pre-sized vector. The imitation left out the task-local resolution and
/// the vector growth every real mark pays, so it priced strictly less work than the operation it
/// was offered as a bound for. The assertion below is what holds it to that path: a calibration
/// that stopped going through the task-local would leave the log empty and fail here.
///
/// It over-states slightly instead — ten thousand marks grow a vector further than nineteen do —
/// which is the right direction for a bound.
async fn mark_overhead_ns() -> f64 {
	const ROUNDS: usize = 10_000;
	let log: Arc<Mutex<Vec<(&'static str, Instant)>>> = Arc::new(Mutex::new(Vec::new()));
	let counted = Arc::clone(&log);
	let elapsed = STEPS
		.scope(log, async {
			let start = Instant::now();
			for _ in 0..ROUNDS {
				mark("overhead");
			}
			start.elapsed()
		})
		.await;
	assert_eq!(
		std::hint::black_box(&counted)
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.len(),
		ROUNDS,
		"the calibration did not reach the task-local the engine's marks are written through, so \
		 it is not measuring the operation it is offered as a bound for"
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

/// What a watcher reports for a file whose contents were rewritten.
const EDIT: EventKind = EventKind::Modify(ModifyKind::Data(DataChange::Any));

/// What a change class did to the fixture, and what a filesystem watcher plus the cache would have
/// announced for it.
///
/// Built ONCE, before the first sample: a sample announces and runs a pass, it creates and edits
/// nothing. That is the difference between measuring a pass and measuring `fs::write`.
struct Applied {
	/// Paths whose content or existence changed.
	touched: Vec<String>,
	/// What a watcher would have CALLED those changes. A removal and an edit are the same set of
	/// paths under two different event kinds, and the engine reads the kind.
	touched_kind: EventKind,
	/// `(from, to)` per rename, announced as the two-ended event a watcher reports for one it saw
	/// whole.
	renamed: Vec<(String, String)>,
	/// The remote half, for a class that diverges BOTH sides. Announced through the pair's own
	/// `note_owed_remote`, which is a real entrance a pass's remote delta arrives by — nothing here
	/// forges a scope. Held as [`RemoteLine`]s so a memory child can be told what its parent
	/// announced.
	remote: Vec<RemoteLine>,
	/// How many files (or directories) the class touched — what the scenario's [`Expect`] is read
	/// against.
	changed: usize,
	/// How many unconfirmed pushes of ours the fixture's baseline holds, every one of which the
	/// pass must confirm (see [`Change::AfterUpload`]).
	pushes: usize,
	/// The file rows under the directory a [`MoveDir`](Change::MoveDir) moved: what its folded move
	/// carries, and what [`Scenario::read_rows_per_carried_file`] is a multiple of.
	carried: usize,
}

impl Applied {
	/// What a class that changes nothing leaves behind.
	fn nothing() -> Self {
		Self {
			touched: Vec::new(),
			touched_kind: EDIT,
			renamed: Vec::new(),
			remote: Vec::new(),
			changed: 0,
			pushes: 0,
			carried: 0,
		}
	}
}

/// Where a bed's pass reads from, with no [`Fixture`] behind it.
///
/// Split out because a MEMORY child opens the fixture its parent built rather than building one of
/// its own: a child that built a tree would carry the whole construction in the resident set it
/// was spawned to report, which is the one figure it exists to get away from.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BedPlace {
	root: PathBuf,
	cache_db: PathBuf,
	baseline_db: PathBuf,
	/// The remote root, as a string: written to a handover file, and this crate's `uuid` carries no
	/// serde.
	remote_root: String,
}

/// A fixture with a real engine open on it, converged, with one pair registered and its carried
/// state seeded — the state a pair is in after one whole pass, which is the only state from which a
/// change-scoped pass runs at all.
struct Bed {
	place: BedPlace,
	/// The tree this bed OWNS, whose `Drop` removes it. `None` in a memory child, which was handed
	/// a tree its parent still owns and must not delete out from under it.
	_fixture: Option<Fixture>,
	engine: SyncEngine,
	pair: PairId,
	/// Baseline rows seeded. ZERO for a first sync, which is what makes it one.
	rows: usize,
	/// Nodes the fixture's tree holds, whatever was seeded from it — what a per-node figure is read
	/// against, and what a first sync's plan is measured by.
	nodes: usize,
	/// What the change class did, and what to announce for it.
	applied: Applied,
	/// See [`fixture_signature`].
	fixture_hash: u64,
}

/// An FNV-1a over what the fixture generator actually PRODUCED, rather than over what the scenario
/// declared.
///
/// Three counts and two paths, so it costs nothing at a million rows. Between them they move when
/// the branching formula moves and when a name style or `LONG_SEGMENT` moves — the knobs under
/// [`probe`](super::probe) that decide what SHAPE a declaration becomes, none of which
/// [`Scenario::definition_hash`] can see. `compare` refuses to diff two runs whose fixtures
/// disagree.
///
/// What it does NOT see is a file's BYTES: the size term is the first file row's declared size, so
/// a change to the content prefix or the padding byte leaves every term here put. That is a
/// deliberate limit rather than an oversight — the length, and so the hashing cost, is unchanged by
/// such an edit — but it is stated because a hash whose coverage is overstated is worse than none.
fn fixture_signature(fixture: &Fixture, rows: &[BaselineEntry]) -> u64 {
	let first = rows.first().map_or("", |row| row.rel_path.as_str());
	let last = rows.last().map_or("", |row| row.rel_path.as_str());
	let file_bytes = rows
		.iter()
		.find(|row| row.kind == NodeKind::File)
		.and_then(|row| row.size)
		.unwrap_or_default();
	fnv1a(&format!(
		"n{} rows{} {first}|{last} {file_bytes}b",
		fixture.nodes(),
		rows.len(),
	))
}

/// Build the fixture, open a REAL engine on it, register the pair, converge the baseline and apply
/// the scenario's change.
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
	let mut rows: Vec<BaselineEntry> = baseline_rows(&scan, &view.nodes);
	// SORTED before anything reads them, which is what makes "baseline order" a real order.
	// `baseline_rows` walks the scan's `HashMap`, and `RandomState` is seeded per process, so the
	// rows arrived in a fresh random permutation on every run: `Change::PerMille`'s "consecutive"
	// and `Change::ScatteredPerMille`'s "strided" drew the same set, and no scenario's edited files
	// were reproducible between two runs of one binary. `apply_change` asserts this held.
	rows.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
	let fixture_hash = fixture_signature(&fixture, &rows);
	let pushes = leave_unconfirmed_pushes(&mut rows, scenario);

	let seeded = if scenario.change.seeds_baseline() {
		engine
			.bench_seed_rows(pair, &rows)
			.await
			.expect("seeding the converged baseline");
		// What the last whole pass left: without it every pass returns `FullPassReason::FirstPass`
		// and this harness would be timing a whole read while reporting a scoped one.
		engine.bench_seed_carry(pair).await;
		rows.len()
	} else {
		// A first sync has neither rows nor carry. The pass discovers that for itself and reads
		// both sides whole, which is exactly what the scenario exists to price.
		0
	};

	// The pair's changelists, as a watched pair's are: a local watcher is covering the tree, and the
	// caps scale with a tree this size rather than with the floor.
	let changes = engine.pair_changes(pair).await;
	changes.cover_local();
	changes.note_tree_size(seeded);

	// The change is applied here, with the baseline rows still in scope to build it from — and the
	// rows are dropped with them. A million-row fixture would otherwise leave a million paths
	// resident in the HARNESS for the rest of the run, against which no engine figure could be read.
	let mut applied = apply_change(&fixture, &rows, scenario);
	applied.pushes = pushes;
	let stood = engine
		.bench_seed_stood_pushes(pair)
		.await
		.expect("reading the pushes the seeded baseline holds");
	assert_eq!(
		stood, pushes,
		"{}: the seeded baseline holds {stood} unconfirmed push(es) where the fixture left {pushes}",
		scenario.name
	);
	let nodes = fixture.nodes();
	// The tree ON DISK, against the scenario's own declaration. Here rather than beside `validate`
	// in `run_scenario`, because the file a change class did NOT touch is found through the
	// baseline rows — and they are dropped on the next line.
	validate_fixture(scenario, fixture.root(), nodes, &rows, &applied);
	drop(rows);

	let place = BedPlace {
		root: fixture.root().to_path_buf(),
		cache_db: fixture.cache_db().to_path_buf(),
		baseline_db: fixture.baseline_db().to_path_buf(),
		remote_root: fixture.remote_root().to_string(),
	};
	Bed {
		place,
		_fixture: Some(fixture),
		engine,
		pair,
		rows: seeded,
		nodes,
		applied,
		fixture_hash,
	}
}

/// Rewrite each picked file so its baseline row no longer describes it, AT THE SIZE the scenario
/// declares.
///
/// The size is load-bearing twice over. `scan::fast_path_hash` reuses the baseline hash while
/// `(size, mtime)` still match, so a file has to differ to be re-hashed at all — and it has to
/// still be the size its scenario declared, or a row written to price hashing a megabyte prices
/// hashing thirty bytes instead. `fs::write` TRUNCATES, which is exactly how that shipped: the
/// megabyte scenario measured 32-byte files and planned precisely what it was supposed to, so
/// every assertion in this harness passed.
fn edit_files(root: &Path, picked: &[&BaselineEntry], file_bytes: usize) -> Vec<String> {
	picked
		.iter()
		.enumerate()
		.map(|(index, row)| {
			// Varied per file so no two edits produce the same bytes, then RESIZED — which pads a
			// short prefix out and cuts a long one down, so the file keeps its declared size
			// whatever the prefix costs.
			let mut content = format!("changed by the bench harness {index:012}\n").into_bytes();
			content.resize(file_bytes, b'#');
			fs::write(root.join(&row.rel_path), &content).expect("editing a fixture file");
			row.rel_path.clone()
		})
		.collect()
}

/// Which of a fixture's `files` a change class touches: the first `wanted` in baseline (path)
/// order, or `wanted` STRIDED across the whole population.
///
/// A free function over counts, so the locality contrast is testable without building a tree. The
/// whole point of the pair is that one picks as few directories as it can while the other picks as
/// many as it picks files — which was false for a round, because the rows arrived in `HashMap`
/// order and both ends drew the same random permutation.
fn pick_indices(files: usize, wanted: usize, scattered: bool) -> Vec<usize> {
	if scattered {
		let stride = (files / wanted.max(1)).max(1);
		(0..files).step_by(stride).take(wanted).collect()
	} else {
		(0..files.min(wanted)).collect()
	}
}

/// `rel_path` with its LEAF renamed and its parent left alone — a rename within one directory.
fn prefixed(rel_path: &str, prefix: &str) -> String {
	match rel_path.rsplit_once('/') {
		Some((dir, name)) => format!("{dir}/{prefix}{name}"),
		None => format!("{prefix}{rel_path}"),
	}
}

/// One announced remote upsert, in scalars a handover file can carry.
///
/// [`RemoteDeltaEntry`] cannot be written to one: it carries a `Blake3Hash` and a `StableUuid`,
/// neither of which anything here may construct except through the entrances they have. So the
/// parent records what it announced as these, a memory child reads the same lines back, and
/// [`remote_delta`] is the ONE place either of them turns a line into an entry — two processes
/// announcing different things is not a failure a benchmark would notice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteLine {
	uuid: String,
	parent: String,
	name: String,
	stable_uuid: Option<String>,
	/// The byte every lane of the announced content hash is filled with — a hash that matches
	/// neither the baseline row nor what the harness wrote locally.
	hash_fill: u8,
	size: u64,
	modified_millis: i64,
}

/// The announced remote delta `lines` describe. The only construction site, for parent and child
/// alike.
fn remote_delta(lines: &[RemoteLine]) -> Vec<RemoteDeltaEntry> {
	let uuid = |raw: &str| {
		raw.parse()
			.unwrap_or_else(|e| panic!("this harness wrote {raw:?} as a uuid: {e}"))
	};
	lines
		.iter()
		.map(|line| RemoteDeltaEntry {
			id: None,
			change: RemoteChange::Upsert(RemoteItem {
				uuid: uuid(&line.uuid),
				parent: uuid(&line.parent),
				name: line.name.clone(),
				stable_uuid: line
					.stable_uuid
					.as_deref()
					.map(|raw| probe::stable_uuid(uuid(raw))),
				hash: Some(Blake3Hash::from([line.hash_fill; 32])),
				size: line.size,
				modified_millis: line.modified_millis,
			}),
		})
		.collect()
}

/// One announced remote change per picked file, carrying content matching NEITHER the baseline row
/// nor what the harness just wrote locally — so each path is a genuine both-sides-changed conflict
/// rather than one side agreeing with the row.
fn remote_edits(
	fixture: &Fixture,
	rows: &[BaselineEntry],
	picked: &[&BaselineEntry],
) -> Vec<RemoteLine> {
	// A baseline row carries its own remote uuid but not its parent's, and an announced change
	// names both.
	let by_path: HashMap<&str, &BaselineEntry> = rows
		.iter()
		.map(|row| (row.rel_path.as_str(), row))
		.collect();
	picked
		.iter()
		.enumerate()
		.filter_map(|(index, row)| {
			let (dir, name) = row
				.rel_path
				.rsplit_once('/')
				.unwrap_or(("", row.rel_path.as_str()));
			let parent = if dir.is_empty() {
				fixture.remote_root()
			} else {
				by_path.get(dir)?.remote_uuid?
			};
			Some(RemoteLine {
				uuid: row.remote_uuid?.to_string(),
				parent: parent.to_string(),
				name: name.to_owned(),
				stable_uuid: row.remote_stable_uuid.map(|id| id.to_string()),
				hash_fill: (index % 251) as u8,
				size: row.size.unwrap_or_default().saturating_add(1),
				modified_millis: row.remote_modified.unwrap_or_default().saturating_add(1),
			})
		})
		.collect()
}

/// What `scenario`'s change class does to the fixture ON DISK, applied once before any sample runs.
/// Rewrite the LAST files of `rows` the way an unconfirmed push of ours leaves them, and answer how
/// many that was (see [`Change::AfterUpload`]).
///
/// The row keeps the content this side holds and the version it minted — the fixture's cache lists
/// that very uuid at the path — and only its agreed-content marker stays on a previous content. The
/// last files rather than the first, so they are never the file the class edits (which
/// [`pick_indices`] takes from the front).
fn leave_unconfirmed_pushes(rows: &mut [BaselineEntry], scenario: &Scenario) -> usize {
	let files = rows.iter().filter(|row| row.kind == NodeKind::File).count();
	let wanted = scenario.change.pushes(files);
	assert!(
		wanted < files,
		"{}: {wanted} push(es) would take every one of {files} file(s), leaving none to edit",
		scenario.name
	);
	let previous = Blake3Hash::from([0x5a; 32]);
	for row in rows
		.iter_mut()
		.rev()
		.filter(|row| row.kind == NodeKind::File)
		.take(wanted)
	{
		assert_ne!(
			row.content_hash,
			Some(previous),
			"a fixture file hashes to the marker's stand-in"
		);
		row.agreed_hash = Some(previous);
	}
	wanted
}

fn apply_change(fixture: &Fixture, rows: &[BaselineEntry], scenario: &Scenario) -> Applied {
	let root = fixture.root();
	assert!(
		rows.is_sorted_by(|a, b| a.rel_path <= b.rel_path),
		"the baseline rows reached a change class UNSORTED, so \"consecutive in baseline order\" \
		 is a fresh random permutation on every run: the clustered and scattered classes draw the \
		 same set, and no scenario's edited files are reproducible between two runs of one binary"
	);
	let files: Vec<&BaselineEntry> = rows
		.iter()
		.filter(|row| row.kind == NodeKind::File)
		.collect();
	let wanted = scenario.change.count(files.len());
	assert!(
		files.len() >= wanted,
		"{}: the change class asks for {wanted} file(s) where the fixture holds {}, so it would \
		 measure a smaller change than its name promises",
		scenario.name,
		files.len(),
	);
	// Consecutive in baseline order clusters the edits into a few leaf directories; strided spreads
	// them over as many directories as there are edits. Same count, opposite locality.
	let pick = |scattered: bool| -> Vec<&BaselineEntry> {
		pick_indices(files.len(), wanted, scattered)
			.into_iter()
			.map(|index| files[index])
			.collect()
	};
	match scenario.change {
		Change::Idle | Change::FirstSync => Applied::nothing(),
		Change::OneFile
		| Change::PerMille(_)
		| Change::ScatteredPerMille(_)
		| Change::Files(_)
		| Change::AfterUpload(_) => {
			let picked = pick(matches!(scenario.change, Change::ScatteredPerMille(_)));
			let touched = edit_files(root, &picked, scenario.file_bytes);
			Applied {
				changed: touched.len(),
				touched,
				..Applied::nothing()
			}
		}
		Change::DeletePerMille(_) => {
			let touched: Vec<String> = pick(false)
				.iter()
				.map(|row| {
					fs::remove_file(root.join(&row.rel_path)).expect("removing a fixture file");
					row.rel_path.clone()
				})
				.collect();
			Applied {
				changed: touched.len(),
				touched,
				touched_kind: EventKind::Remove(RemoveKind::File),
				..Applied::nothing()
			}
		}
		Change::RenamePerMille(_) => {
			let renamed: Vec<(String, String)> = pick(false)
				.iter()
				.map(|row| {
					let to = prefixed(&row.rel_path, "renamed_");
					fs::rename(root.join(&row.rel_path), root.join(&to))
						.expect("renaming a fixture file");
					(row.rel_path.clone(), to)
				})
				.collect();
			Applied {
				changed: renamed.len(),
				renamed,
				..Applied::nothing()
			}
		}
		Change::MoveDir => {
			// The DEEPEST directory, so the move carries a subtree rather than an empty leaf.
			let moved = rows
				.iter()
				.filter(|row| row.kind == NodeKind::Dir)
				.max_by_key(|row| row.rel_path.matches('/').count())
				.expect("the fixture holds directories");
			let to = prefixed(&moved.rel_path, "moved_");
			fs::rename(root.join(&moved.rel_path), root.join(&to))
				.expect("moving a fixture directory");
			let carried = files
				.iter()
				.filter(|row| {
					row.rel_path
						.strip_prefix(moved.rel_path.as_str())
						.is_some_and(|rest| rest.starts_with('/'))
				})
				.count();
			Applied {
				changed: 1,
				renamed: vec![(moved.rel_path.clone(), to)],
				carried,
				..Applied::nothing()
			}
		}
		Change::BothSidesPerMille(_) => {
			let picked = pick(false);
			let touched = edit_files(root, &picked, scenario.file_bytes);
			let remote = remote_edits(fixture, rows, &picked);
			assert_eq!(
				remote.len(),
				touched.len(),
				"every locally edited file must also be announced as changed remotely, or this \
				 scenario is measuring a one-sided edit with an empty remote changelist"
			);
			Applied {
				changed: touched.len(),
				touched,
				remote,
				..Applied::nothing()
			}
		}
	}
}

/// Announce what the change class did, the way the pair's watcher and cache subscription would,
/// then take the scope that announcement produced.
///
/// Through the real [`PairChanges`](super::changes::PairChanges) entrances rather than by forging a
/// scope: the caps, the rule-file collapse and the unmapped-path collapse are all live, and a
/// scenario big enough to trip one of them must trip it here too rather than measure a pass no
/// watcher could ever have produced.
async fn announce(bed: &Bed) -> PassScope {
	let changes = bed.engine.pair_changes(bed.pair).await;
	let root = bed.place.root.as_path();
	for rel_path in &bed.applied.touched {
		let event = Event::new(bed.applied.touched_kind).add_path(root.join(rel_path));
		changes.note_local_event(root, &event, |_| false);
	}
	for (from, to) in &bed.applied.renamed {
		let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
			.add_path(root.join(from))
			.add_path(root.join(to));
		changes.note_local_event(root, &event, |_| false);
	}
	if !bed.applied.remote.is_empty() {
		changes.note_owed_remote(remote_delta(&bed.applied.remote));
	}
	changes.take()
}

/// Check one number of the plan against what the scenario says it must be.
fn expect_count(scenario: &Scenario, what: &str, expected: Count, actual: usize, bed: &Bed) {
	assert!(
		expected.holds(actual, bed.applied.changed, bed.nodes),
		"{}: the pass planned {actual} {what} where the scenario expects {} ({} changed, {} \
		 node(s)); a scenario whose plan moved is no longer measuring what its name says",
		scenario.name,
		expected.label(),
		bed.applied.changed,
		bed.nodes,
	);
}

/// The SHAPE of what a pass planned, beside what it cost.
///
/// Recorded on every sample, not merely asserted: a reader of a result file can then see that a
/// scenario actually did work, instead of taking the expectation's word for it. The four numbers
/// are the ones an action count alone cannot separate — a mass delete whose deletions were all held
/// and a pass that planned nothing both report `actions: 0`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Plan {
	actions: usize,
	held: usize,
	conflicts: usize,
	dir_moves: usize,
	confirmed: usize,
}

/// One measured pass: announce, run the REAL `prepare` under a step log, and split what it cost.
///
/// The third value is what the pass's structures computed their own size as, which the engine's
/// tail fills in for a MEMORY child and leaves `None` for every timed sample.
async fn one_pass(bed: &Bed, scenario: &Scenario) -> (Timing, Plan, Option<PassStructures>) {
	let mut scope = announce(bed).await;
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
	// WHICH read ran, and for a whole one, exactly why. A pass that fell back for a reason nobody
	// expected is a scenario measuring something else under a believable name.
	match scenario.read {
		ExpectRead::Scoped => assert!(
			pass.scoped,
			"{}: the pass read both sides whole where the scenario expects a narrowed read{}",
			scenario.name,
			pass.full_reason
				.map(|reason| format!(" ({reason:?})"))
				.unwrap_or_default(),
		),
		ExpectRead::Whole(expected) => {
			assert!(
				!pass.scoped,
				"{}: the pass narrowed its read where the scenario expects a whole one",
				scenario.name
			);
			assert_eq!(
				pass.full_reason,
				Some(expected),
				"{}: the pass read both sides whole for a reason this scenario did not expect",
				scenario.name
			);
		}
	}
	// The exact ORDERED list of steps the engine marked. This is the half of the anti-drift
	// property the marks alone did not give: timing what the engine marks stops a step being timed
	// for work the engine no longer does, but says nothing when a step is renamed or split — and
	// `compare` keys a metric by NAME, so a renamed phase was dropped from BOTH sides of a diff
	// and left a complete-looking table of zeroes behind it.
	let observed: Vec<&str> = marks.iter().map(|(name, _)| *name).collect();
	let expected = match scenario.read {
		ExpectRead::Scoped => SCOPED_STEPS,
		ExpectRead::Whole(_) => WHOLE_STEPS,
	};
	assert_eq!(
		observed, expected,
		"{}: the pass marked a different sequence of steps than this harness knows about. A phase \
		 was added, removed, renamed, reordered or made conditional in the engine — update \
		 SCOPED_STEPS/WHOLE_STEPS, and treat every result file written before that change as \
		 incomparable",
		scenario.name
	);
	expect_count(
		scenario,
		"action(s)",
		scenario.expect.actions,
		pass.actions,
		bed,
	);
	expect_count(
		scenario,
		"held action(s)",
		scenario.expect.held,
		pass.held,
		bed,
	);
	expect_count(
		scenario,
		"conflict(s)",
		scenario.expect.conflicts,
		pass.conflicts,
		bed,
	);
	expect_count(
		scenario,
		"folded directory move(s)",
		scenario.expect.dir_moves,
		pass.dir_moves,
		bed,
	);
	// Every push the fixture left, and none it did not: a pass after an upload that confirmed
	// nothing never wrote to its baseline, and that write is what the scenario prices.
	assert_eq!(
		pass.confirmed, bed.applied.pushes,
		"{}: the pass confirmed {} push(es) where the fixture left {} awaiting confirmation",
		scenario.name, pass.confirmed, bed.applied.pushes,
	);
	(
		Timing::split(start, total, &marks),
		Plan {
			actions: pass.actions,
			held: pass.held,
			conflicts: pass.conflicts,
			dir_moves: pass.dir_moves,
			confirmed: pass.confirmed,
		},
		pass.structures,
	)
}

// ---------------------------------------------------------------------------------------------
// Memory: what a pass costs in a process that has done nothing else
// ---------------------------------------------------------------------------------------------

/// What a memory child is told to measure. Written by the parent, read by the child, deleted after.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Handover {
	scenario: String,
	place: BedPlace,
	pair: PairId,
	rows: usize,
	nodes: usize,
	fixture_hash: u64,
	touched: Vec<String>,
	/// `edit` or `remove`: which of the two kinds a change class announces its touched paths under.
	touched_kind: String,
	renamed: Vec<(String, String)>,
	remote: Vec<RemoteLine>,
	changed: usize,
	pushes: usize,
	carried: usize,
	/// Where the child writes its answer.
	answer: PathBuf,
}

fn kind_tag(kind: EventKind) -> &'static str {
	match kind {
		EDIT => "edit",
		EventKind::Remove(RemoveKind::File) => "remove",
		other => panic!("no change class announces {other:?}"),
	}
}

fn kind_of(tag: &str) -> EventKind {
	match tag {
		"edit" => EDIT,
		"remove" => EventKind::Remove(RemoveKind::File),
		other => panic!("a handover named the event kind {other:?}"),
	}
}

/// One child's answer: every figure taken in a process whose whole history is what it was asked to
/// measure.
///
/// Every RSS field here is that process's resident set — never this run's, which is what made the
/// earlier figures quotable as something they were not. The names they are RECORDED under say so
/// outright (see [`MemAnswer::metrics`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemAnswer {
	/// Before it opened anything: the binary and the allocator's first pages. The runtime is built
	/// AFTER this is taken, so its resident cost is part of what loading the pair is measured to
	/// add rather than part of the floor it is measured from.
	floor_rss: u64,
	/// With the pair's baseline loaded into the engine's store and NOTHING else — the state an idle
	/// engine sits in between passes.
	pair_loaded_rss: u64,
	/// The widest of the engine's own step boundaries during the pass. Not a continuous sampler: a
	/// spike inside one step is not here, and `peak_rss` is the bound that catches it.
	pass_widest_rss: u64,
	/// After the pass and everything it built are dropped, with the engine still open.
	after_pass_rss: u64,
	/// After the engine and its store go too.
	after_everything_dropped_rss: u64,
	/// `getrusage`'s high-water mark for the whole child: the transients of BUILDING what the pass
	/// held sit between this and `pass_widest_rss`.
	peak_rss: u64,
	rows: usize,
	actions: usize,
	/// What the resident baseline computes its own size as, with nothing else alive.
	pair_baseline_computed_bytes: u64,
	/// `(baseline, view, scan)` as the structures compute themselves at the pass's widest point.
	///
	/// Deliberately not an `Option`: a child that reached the engine's tail without a sum is a child
	/// whose two attribution columns would print `NaN` while every other assertion passed, so it
	/// fails where the sum is taken instead.
	structures: (u64, u64, u64),
	/// The runtime is built before anything else this child holds, so this minus `floor_rss` is
	/// what a tokio multi-thread runtime costs and nothing else.
	rss_after_runtime: u64,
	/// After the client and its cache slot, before any engine exists.
	rss_after_client: u64,
	/// After `SyncEngine::open` and the changelist reset — the store's connection, the cache
	/// worker, and no pair loaded. The gap between this and `pair_loaded_rss` is the tree, the row
	/// decode that built it, and SQLite's own pages: the three the published table folded into one
	/// unattributed "fixed cost".
	rss_after_engine_open: u64,
	/// The loaded pair's resident tree, TERM BY TERM (`Baseline::resident_terms`), in the order a
	/// table should print them. Sums to `pair_baseline_computed_bytes`.
	pair_baseline_terms: Vec<(String, u64)>,
	/// What a CARRIED side materialized during the measured pass:
	/// `(whole_calls, whole_rows, subtree_calls, subtree_rows)`. A change-scoped pass exists so as
	/// not to hold a second copy of the tree, and `whole_rows` is the figure that says whether it
	/// held one anyway.
	carried_walks: (u64, u64, u64, u64),
	/// What the measured pass asked the store for its baseline rows: `(statements, rows)`. The
	/// rows are no longer held between passes, so this is what a pass pays for them instead.
	baseline_reads: (u64, u64),
	/// The resident set at each boundary the engine marked, in order.
	at_step: Vec<(String, u64)>,
}

impl MemAnswer {
	/// Every figure under a name that says WHOSE process it is and HOW it was arrived at.
	///
	/// `rss` is always a measured resident set of the fresh child; `computed` is always a structure
	/// summing its own capacities, which is a different kind of number and cannot be quoted as the
	/// other. Nothing here is called `peak_rss` on its own: that name meant "this process since it
	/// started" and was quoted as the cost of a phase.
	fn metrics(&self) -> Vec<(String, u64)> {
		let mut out = vec![
			("mem:fresh_process_floor_rss".to_owned(), self.floor_rss),
			(
				"mem:fresh_process_pair_loaded_rss".to_owned(),
				self.pair_loaded_rss,
			),
			(
				"mem:fresh_process_pass_widest_rss".to_owned(),
				self.pass_widest_rss,
			),
			(
				"mem:fresh_process_pass_widest_over_floor_rss".to_owned(),
				self.pass_widest_rss.saturating_sub(self.floor_rss),
			),
			(
				// What the PASS added, over a process already holding the pair. The figure a
				// per-pass memory target is read off: `over_floor` above includes opening the
				// engine and reading the tree, which no pass pays again.
				"mem:fresh_process_pass_widest_over_pair_loaded_rss".to_owned(),
				self.pass_widest_rss.saturating_sub(self.pair_loaded_rss),
			),
			(
				"mem:fresh_process_after_pass_rss".to_owned(),
				self.after_pass_rss,
			),
			(
				"mem:fresh_process_after_everything_dropped_rss".to_owned(),
				self.after_everything_dropped_rss,
			),
			("mem:fresh_process_peak_rss".to_owned(), self.peak_rss),
			(
				"mem:pair_baseline_computed_bytes".to_owned(),
				self.pair_baseline_computed_bytes,
			),
			(
				"mem:fresh_process_rss_after_runtime".to_owned(),
				self.rss_after_runtime,
			),
			(
				"mem:fresh_process_rss_after_client".to_owned(),
				self.rss_after_client,
			),
			(
				"mem:fresh_process_rss_after_engine_open".to_owned(),
				self.rss_after_engine_open,
			),
		];
		// Each term under its own name, and `_computed_bytes` like the total they sum to: these are
		// structures summing their own capacities, not a resident set anyone observed.
		for (term, bytes) in &self.pair_baseline_terms {
			out.push((
				format!("mem:pair_baseline_term_{term}_computed_bytes"),
				*bytes,
			));
		}
		// COUNTS, under a prefix of their own, because they are not bytes and a `mem:` name would
		// be printed as though they were.
		let (whole_calls, whole_rows, subtree_calls, subtree_rows) = self.carried_walks;
		out.push(("walk:carried_entries_whole_calls".to_owned(), whole_calls));
		out.push(("walk:carried_entries_whole_rows".to_owned(), whole_rows));
		out.push((
			"walk:carried_entries_subtree_calls".to_owned(),
			subtree_calls,
		));
		out.push(("walk:carried_entries_subtree_rows".to_owned(), subtree_rows));
		let (statements, rows) = self.baseline_reads;
		out.push(("walk:baseline_read_statements".to_owned(), statements));
		out.push(("walk:baseline_read_rows".to_owned(), rows));
		let (baseline, view, scan) = self.structures;
		out.push(("mem:pass_baseline_computed_bytes".to_owned(), baseline));
		out.push(("mem:pass_view_computed_bytes".to_owned(), view));
		out.push(("mem:pass_scan_computed_bytes".to_owned(), scan));
		// The two SIDES, and not a sum with the baseline in it. That sum shipped once: at ten thousand
		// rows it was 98 % the pair's own baseline — resident before the pass began, and deliberately
		// excluded from the attribution ratio the same run prints — under a name that reads as what
		// the pass itself built.
		out.push(("mem:pass_sides_computed_bytes".to_owned(), view + scan));
		for (step, rss) in &self.at_step {
			out.push((format!("mem:fresh_process_rss_at_step:{step}"), *rss));
		}
		out
	}
}

/// Ask a FRESH PROCESS what this bed's pass costs in memory, `samples` times.
///
/// It exists because an in-process figure cannot answer an absolute question: this process has run
/// warmups, three samples and three whole yardstick passes, and what the allocator has not returned
/// to the kernel is still in its resident set. The child opens the tree THIS process built — it
/// builds nothing, because building a million-node fixture would put the construction in the very
/// figure it was spawned to report.
///
/// Blocking on purpose, inside an async fn: nothing else is running on this runtime, and a child
/// that overlapped the parent's own passes would measure a machine under a load the parent put
/// there.
fn measure_memory(bed: &Bed, scenario: &Scenario, samples: usize) -> Vec<MemAnswer> {
	(0..samples)
		.map(|sample| {
			let dir = std::env::temp_dir();
			let tag = uuid::Uuid::new_v4();
			let spec = dir.join(format!("filen_bench_mem_{tag}.json"));
			let answer = dir.join(format!("filen_bench_mem_answer_{tag}.json"));
			let handover = Handover {
				scenario: scenario.name.to_owned(),
				place: bed.place.clone(),
				pair: bed.pair,
				rows: bed.rows,
				nodes: bed.nodes,
				fixture_hash: bed.fixture_hash,
				touched: bed.applied.touched.clone(),
				touched_kind: kind_tag(bed.applied.touched_kind).to_owned(),
				renamed: bed.applied.renamed.clone(),
				remote: bed.applied.remote.clone(),
				changed: bed.applied.changed,
				pushes: bed.applied.pushes,
				carried: bed.applied.carried,
				answer: answer.clone(),
			};
			fs::write(
				&spec,
				serde_json::to_string(&handover).expect("a handover encodes"),
			)
			.expect("writing the handover");
			let exe = std::env::current_exe().expect("a test binary knows its own path");
			let status = std::process::Command::new(exe)
				.args(["--ignored", "--exact", "sync_engine_bench"])
				.env("SYNC_BENCH_MEM_CHILD", &spec)
				.status()
				.expect("spawning the memory child");
			let raw = fs::read_to_string(&answer);
			fs::remove_file(&spec).ok();
			fs::remove_file(&answer).ok();
			// LOUD. A child runs the same assertions this process does, so a failed one is a failed
			// scenario — and a memory number that quietly went missing is how a table comes to be
			// read as covering a row it never measured.
			assert!(
				status.success(),
				"{}: memory sample {sample} exited {status}; its assertions are this harness's own",
				scenario.name
			);
			let raw = raw.expect("the memory child wrote no answer");
			serde_json::from_str(&raw).expect("decoding the memory child's answer")
		})
		.collect()
}

/// The child half of [`measure_memory`]: open the tree it was handed, hold what a pass holds, and
/// sample itself around it. Runs INSTEAD of everything in [`run`].
fn answer_memory_child(spec: &Path) {
	// The process before it holds anything: the binary and the allocator's first pages. The runtime
	// is built three lines below and is NOT in this figure — it is part of what loading the pair is
	// then measured to add.
	let floor = probe::current_rss_bytes();
	// Zero is what `current_rss_bytes` answers where it cannot ask (`ps` missing, `/proc` absent),
	// and nothing downstream would notice: the table would print a complete-looking row of zeroes
	// and divide by one of them.
	assert!(
		floor > 0,
		"this platform answered no resident set at all, so every memory figure here would be zero"
	);
	let raw = fs::read_to_string(spec).expect("reading the handover");
	let handover: Handover = serde_json::from_str(&raw).expect("decoding the handover");
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.expect("building the memory child's runtime");
	let answer = runtime.block_on(measure_in_child(floor, &handover));
	fs::write(
		&handover.answer,
		serde_json::to_string(&answer).expect("an answer encodes"),
	)
	.expect("writing the answer");
}

/// Hold what one pass holds, in a process whose history is that pass and the opening of an engine
/// over the fixture — NOT a process that has done nothing else. `SyncEngine::open` here runs
/// against a registry that already holds the pair, which starts a cache worker and a second DB
/// connection the parent never started, and all of it is resident before `pair_loaded_rss` is
/// sampled. It therefore sits inside `pair_loaded - floor`, the denominator of `pair_attributed`.
async fn measure_in_child(floor: u64, handover: &Handover) -> MemAnswer {
	// The runtime is already built — this function runs on it — and nothing else is. Every stage
	// sample below is taken the same way, so the four of them telescope to `pair_loaded_rss`
	// ARITHMETICALLY. That is all it means: a resident set is not additive across stages — a stage
	// that frees what it borrowed hands pages back that an earlier sample was charged for — so a
	// delta between two of them is what the process grew by there, never what that stage costs.
	// `accounting_table` attributes against the floor for exactly this reason.
	let rss_after_runtime = probe::current_rss_bytes();
	let scenario = scenario(&handover.scenario)
		.unwrap_or_else(|| panic!("no scenario named {:?}", handover.scenario));
	let client = offline_client();
	client
		.configure_cache(handover.place.cache_db.clone(), |_| {})
		.await
		.expect("configuring the cache slot writes a path and nothing else");
	let rss_after_client = probe::current_rss_bytes();
	let engine = SyncEngine::open(client, handover.place.baseline_db.clone())
		.await
		.expect("opening an engine on the baseline DB its parent wrote");
	// Opening on a registry that ALREADY holds the pair tries to subscribe it to the cache, and a
	// fixture's synthetic remote root is refused — which marks the pair degraded for good and makes
	// every pass a whole read. The parent has no subscription either; it simply never asked for
	// one. See `bench_reset_changes`.
	engine.bench_reset_changes(handover.pair).await;
	// The engine open and no pair loaded: the store's connection, the cache worker and the second
	// DB connection this child starts. What separates it from `pair_loaded_rss` below is the tree
	// and the read that built it, which is the term the published table could not name.
	let rss_after_engine_open = probe::current_rss_bytes();
	// The pair, loaded and nothing else. Measured BEFORE the pass, because a figure taken after one
	// is a figure that has held a pass's structures.
	let (rows, pair_baseline_computed_bytes, pair_terms) = engine
		.bench_load_pair(handover.pair)
		.await
		.expect("loading the pair's baseline");
	let pair_baseline_terms: Vec<(String, u64)> = pair_terms
		.named()
		.iter()
		.map(|&(term, bytes)| (term.to_owned(), bytes as u64))
		.collect();
	assert_eq!(
		pair_baseline_terms
			.iter()
			.map(|&(_, bytes)| bytes)
			.sum::<u64>(),
		pair_baseline_computed_bytes as u64,
		"the resident tree's terms do not sum to the total published beside them"
	);
	assert_eq!(
		rows, handover.rows,
		"the child loaded {rows} row(s) where its parent seeded {}: it is not reading the tree the 		 scenario built",
		handover.rows
	);
	let pair_loaded_rss = probe::current_rss_bytes();

	// The in-memory half of `prepare_bed`, which no DB carries: the carried state of a previous
	// whole pass, and a watched pair's changelist caps.
	engine.bench_seed_carry(handover.pair).await;
	let changes = engine.pair_changes(handover.pair).await;
	changes.cover_local();
	changes.note_tree_size(handover.rows);
	let stood = engine
		.bench_seed_stood_pushes(handover.pair)
		.await
		.expect("reading the pushes the parent's baseline holds");
	assert_eq!(
		stood, handover.pushes,
		"the child found {stood} unconfirmed push(es) where its parent left {}",
		handover.pushes
	);
	// No assertion that the pair is UNdegraded here, deliberately: `bench_reset_changes` dropped the
	// changelist entry two lines above and `pair_changes` built a fresh one, so the flag is `None` by
	// construction and an assertion on it could not fail. What holds this honest is `one_pass`, which
	// asserts WHICH read the pass performed — a degraded pair reads both sides whole, and that fails
	// the scenario rather than publishing a whole pass under a scoped row's name.
	drop(changes);

	let bed = Bed {
		place: handover.place.clone(),
		_fixture: None,
		engine,
		pair: handover.pair,
		rows: handover.rows,
		nodes: handover.nodes,
		applied: Applied {
			touched: handover.touched.clone(),
			touched_kind: kind_of(&handover.touched_kind),
			renamed: handover.renamed.clone(),
			remote: handover.remote.clone(),
			changed: handover.changed,
			pushes: handover.pushes,
			carried: handover.carried,
		},
		fixture_hash: handover.fixture_hash,
	};

	// The SAME `one_pass` a timing sample runs, with every one of its assertions: the read kind, the
	// four plan numbers and the ordered step list. A memory figure for a pass that was not the
	// scenario's pass is the same lie a timing figure for one would be.
	// Zeroed HERE, so what comes back describes the measured pass and not the engine's opening.
	super::side::reset_carried_walks();
	super::rows::reset_reads();
	let log: Arc<Mutex<Vec<(&'static str, u64)>>> = Arc::new(Mutex::new(Vec::new()));
	let (_, plan, structures) = STEP_RSS
		.scope(Arc::clone(&log), one_pass(&bed, scenario))
		.await;
	let after_pass_rss = probe::current_rss_bytes();
	let walks = super::side::carried_walks();
	let baseline_reads = super::rows::reads();
	let carried_walks = (
		walks.whole_calls,
		walks.whole_rows,
		walks.subtree_calls,
		walks.subtree_rows,
	);
	let at_step: Vec<(String, u64)> = log
		.lock()
		.unwrap_or_else(PoisonError::into_inner)
		.iter()
		.map(|(step, rss)| ((*step).to_owned(), *rss))
		.collect();
	assert!(
		!at_step.is_empty(),
		"the pass sampled no step: the memory child is not reaching the engine's own marks"
	);
	// The widest point DURING the pass, which is not the sample after it: `drop_pass` is marked once
	// the plan and both sides are freed, and a resident set that does not fall at these sizes made
	// that last sample the maximum — so `widest`, `peak` and `after_pass` printed one number three
	// times, and a reader could not tell that from "the pass returned nothing to the kernel".
	let pass_widest_rss = at_step
		.iter()
		.filter(|(step, _)| step.as_str() != "drop_pass")
		.map(|(_, rss)| *rss)
		.max()
		.expect("the pass marked a boundary other than the one after the drop");
	let structures = structures.expect(
		"the pass reported no structure sizes: the memory task-local did not reach the engine's own \
		 tail, and this child's two attribution columns would be NaN",
	);
	let structures = (
		structures.baseline_bytes as u64,
		structures.view_bytes as u64,
		structures.scan_bytes as u64,
	);
	// A pass holds no rows it did not edit: it reads the table through the same kind of handle the
	// loaded pair holds, and what it holds beyond that is its EDITS — the directory moves it folded
	// and the pushes it confirmed — each costing what it edited, never what the rows under it
	// hold. A pass that copied the table, or folded a move by writing every row it carries (which
	// is what a move of a large directory used to cost), sizes its edits by rows and fails the
	// bound below.
	let pass_edits = structures
		.0
		.checked_sub(pair_baseline_computed_bytes as u64)
		.unwrap_or_else(|| {
			panic!(
				"the pass's baseline computes itself as {} byte(s), LESS than the loaded pair's {}: \
				 it is not reading the table through the handle the store hands out",
				structures.0, pair_baseline_computed_bytes
			)
		});
	let edit_bound = EDITS_BASE_BYTES + EDIT_BYTES * (plan.dir_moves + plan.confirmed) as u64;
	assert!(
		pass_edits <= edit_bound,
		"the pass's edits compute themselves as {pass_edits} byte(s) for {} directory move(s) \
		 folded and {} push(es) confirmed, over the {edit_bound} those edits are entitled to — \
		 the pass is holding rows it did not edit",
		plan.dir_moves,
		plan.confirmed,
	);
	// What a directory move's pass READS, bounded per file the move carries the way its edits are
	// bounded per move (see `Scenario::read_rows_per_carried_file`). A read of the moved subtree
	// that an earlier one already answered holds nothing afterwards, so neither the edits nor a
	// resident set shows it; the row count does, and the bound fails the run on it.
	if let Some(per_file) = scenario.read_rows_per_carried_file {
		let read_bound = per_file * handover.carried as u64;
		assert!(
			baseline_reads.1 <= read_bound,
			"{}: the pass read {} baseline row(s) for a directory move carrying {} file(s), over \
			 the {read_bound} ({per_file} a file) the scenario allows — it reads the moved subtree \
			 more times than it did when the bound was set",
			scenario.name,
			baseline_reads.1,
			handover.carried,
		);
	}
	drop(bed);
	// A high-water mark cannot sit BELOW a sample of the same process's resident set. Zero is what
	// `peak_rss_bytes` answers where it cannot ask — `getrusage` failing, or a target that has none
	// — and nothing downstream would notice: `peak_MiB` would print 0.0 beside a widest of 24.0 and
	// read as a formatting glitch rather than as the figure never having been taken.
	let peak_rss = probe::peak_rss_bytes();
	let after_everything_dropped_rss = probe::current_rss_bytes();
	// EVERY figure this child publishes, and not the floor alone. `current_rss_bytes` answers zero
	// wherever it cannot ask, and it is asked once per sample and once per step — so a reader that
	// works at the floor and stops working afterwards (a `ps` that fails after its first call is
	// enough) leaves the floor assertion satisfied and every later figure zero. A run with this
	// hole open exits 0 and prints `widest 0.0`, `after_pass 0.0` and `pass_added 0.00` beside a
	// real peak and a plausible `pair_attributed`, which reads as a pass that cost no memory
	// rather than as figures nobody took.
	for (what, rss) in [
		("the pair loaded", pair_loaded_rss),
		("the widest point of the pass", pass_widest_rss),
		("the sample after the pass", after_pass_rss),
		(
			"the sample after everything was dropped",
			after_everything_dropped_rss,
		),
	] {
		assert!(
			rss > 0,
			"{what} reads 0 byte(s): this process stopped answering its own resident set part way \
			 through, and every memory row here would carry that zero as a measurement"
		);
	}
	let unsampled: Vec<&str> = at_step
		.iter()
		.filter(|(_, rss)| *rss == 0)
		.map(|(step, _)| step.as_str())
		.collect();
	assert!(
		unsampled.is_empty(),
		"the resident set was never taken at {}: a step row of zeroes is published under the \
		 engine's own step name, where it reads as a phase that held nothing",
		unsampled.join(", ")
	);
	assert!(
		peak_rss >= pass_widest_rss,
		"this process's high-water mark reads {peak_rss} byte(s) against a resident set sampled at \
		 {pass_widest_rss}: the peak was never measured, and every memory row here would carry it"
	);
	MemAnswer {
		floor_rss: floor,
		pair_loaded_rss,
		pass_widest_rss,
		after_pass_rss,
		after_everything_dropped_rss,
		peak_rss,
		rows,
		actions: plan.actions,
		pair_baseline_computed_bytes: pair_baseline_computed_bytes as u64,
		structures,
		rss_after_runtime,
		rss_after_client,
		rss_after_engine_open,
		pair_baseline_terms,
		carried_walks,
		baseline_reads,
		at_step,
	}
}

/// Run `scenario` `samples` times and return every record it produced.
///
/// The yardstick is a WHOLE pass over the same fixture in the same run, through the same
/// `prepare`: it is the figure that says whether two runs are comparable at all, and taking it
/// through the real function means it prices the same work the engine does. It is taken once per
/// SAMPLE and after the warmups, on the same footing as the figure that divides by it.
pub async fn run_scenario(scenario: &Scenario, samples: usize) -> Vec<Record> {
	let bed = prepare_bed(scenario).await;
	let nodes = bed.nodes;
	let fixture_hash = bed.fixture_hash;
	// Against what the fixture ACTUALLY built and what the change class actually touched — a
	// stronger question than the unit tests' synthetic one, because a per-mille class on a small
	// tree can round its way down to something degenerate.
	validate(scenario, bed.applied.changed, nodes);
	// The tree on disk was checked by `prepare_bed`, which still had the baseline rows to check the
	// untouched population through.
	let hash = scenario.definition_hash();
	let reps = scenario.reps.max(1);
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
			fixture_hash,
			reps,
		};
	let make_bytes = |sample: usize, metric: String, bytes: u64| Record {
		scenario: scenario.name.to_owned(),
		scenario_version: scenario.version,
		definition_hash: hash,
		sample: Some(sample),
		metric,
		// Zero, and meant: the value is in `count` and `unit` calls it bytes. A memory figure
		// printed in a column headed milliseconds is the shape of mistake this harness exists to
		// stop.
		ms: 0.0,
		count: Some(bytes),
		nodes,
		fixture_hash,
		// One pass, undivided: a memory child runs the scenario's pass once and reports what it
		// held, so the scenario's `reps` is not a divisor of anything here.
		reps: 1,
	};

	// Discarded: the OS page cache, the reader connection's statement and page caches (the store
	// opens it on the pair's FIRST read) and the branch predictors every recorded figure — the
	// yardstick included — is then measured in.
	for _ in 0..WARMUPS {
		one_pass(&bed, scenario).await;
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
		let mut plan = Plan::default();
		let mut marks = 0usize;
		let wall = Instant::now();
		for _ in 0..reps {
			let (timing, planned, _) = one_pass(&bed, scenario).await;
			for (step, elapsed) in &timing.steps {
				match per_step.iter_mut().find(|(name, _)| name == step) {
					Some((_, total)) => *total += *elapsed,
					None => per_step.push((step, *elapsed)),
				}
			}
			passes += timing.total;
			remainders += timing.unattributed;
			marks = timing.steps.len();
			plan = planned;
		}
		let wall = wall.elapsed();
		let actions = plan.actions;
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
		// THIS HARNESS's own per-pass work, named rather than buried in the remainder above. It is
		// not part of `total`, and no pass pays it.
		//
		// `harness_overhead`, not `harness_announce`: it is the wall clock minus the timed passes,
		// so besides announcing the changed paths it also carries the marks clone, the read-kind
		// and row-count assertions, the four `expect_count` calls, the step-sequence check and the
		// linear step merge. A reader budgeting a real watcher's announcement off a figure called
		// `harness_announce` would have been budgeting this harness's assertions.
		//
		// ASSERTED rather than saturated: every other identity in this file asserts, and a
		// bookkeeping error that made the wall shorter than the passes inside it would otherwise
		// report a tidy 0.0000 instead of failing.
		assert!(
			wall >= passes,
			"{}: the sample's wall clock is {wall:?}, inside the {passes:?} of timed passes it \
			 contains — the harness's own accounting is wrong",
			scenario.name
		);
		records.push(make(
			Some(sample),
			"harness_overhead".to_owned(),
			(wall - passes) / reps32,
			None,
		));
		records.push(make(
			Some(sample),
			"marks".to_owned(),
			Duration::ZERO,
			Some(marks as u64),
		));
		// What the pass actually PLANNED, on the record rather than only inside an assertion, so a
		// reader can see this scenario did work without taking its expectation on trust.
		for (metric, value) in [
			("plan:actions", plan.actions),
			("plan:held", plan.held),
			("plan:conflicts", plan.conflicts),
			("plan:dir_moves", plan.dir_moves),
		] {
			records.push(make(
				Some(sample),
				metric.to_owned(),
				Duration::ZERO,
				Some(value as u64),
			));
		}
		// The yardstick, taken INSIDE the sample and AFTER the warmups: a whole pass over the same
		// fixture through the same `prepare`, in the state this sample's own figures were taken in.
		//
		// It used to run once, first, before the warmups — so every published `speedup` divided a
		// warm three-sample median by one cold unreplicated pass, including the ones quoted to six
		// significant figures. A ratio between two quantities measured in different states is a
		// measurement of the difference in state.
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
			Some(sample),
			"yardstick_whole_pass".to_owned(),
			yardstick,
			None,
		));
	}
	// What licenses dividing one by the other at all.
	let yardsticks = records
		.iter()
		.filter(|record| record.metric == "yardstick_whole_pass")
		.count();
	let totals = records
		.iter()
		.filter(|record| record.metric == "total")
		.count();
	assert_eq!(
		yardsticks, totals,
		"{}: {yardsticks} yardstick(s) against {totals} measured sample(s). Every published ratio \
		 divides one by the other, so the two have to be the same measurement taken the same \
		 number of times in the same state",
		scenario.name
	);
	// And what the SAME pass costs in memory, in fresh processes that run one pass and little else
	// (`measure_in_child` says what else). Taken after every timed sample, so no child's work sits
	// inside a figure this process timed.
	let mem_samples = env_usize("SYNC_BENCH_MEM_SAMPLES", DEFAULT_MEM_SAMPLES);
	for (sample, answer) in measure_memory(&bed, scenario, mem_samples)
		.into_iter()
		.enumerate()
	{
		// The child's pass is the same pass or the two tables are of different things. Its own
		// assertions cover the read kind and the step sequence; this is the one comparison only
		// the parent can make.
		if let Some(planned) = expected {
			assert_eq!(
				answer.actions, planned,
				"{}: memory sample {sample} planned {} action(s) where this process planned \
				 {planned}; the memory and timing figures are not two views of one pass",
				scenario.name, answer.actions,
			);
		}
		for (metric, bytes) in answer.metrics() {
			records.push(make_bytes(sample, metric, bytes));
		}
	}

	// The fixture goes with the `Bed`: `Fixture`'s `Drop` removes the tree on the way out and on a
	// panic alike, which is the only way an assertion doing its job does not cost the machine its
	// disk.
	records
}

/// Which scenarios a `SYNC_BENCH_SCENARIO` value names: `all`, `default` (every scenario but the
/// 1M rows), or a comma-separated list.
///
/// The set the documentation recommended was not expressible before — the selector took `all` or
/// exactly one name, and `all` pulled in three scenarios costing about five minutes and 4.7 GB of
/// disk each.
///
/// # Errors
///
/// When any name is not a scenario.
fn select(wanted: &str) -> Result<Vec<&'static Scenario>, String> {
	let known = || {
		SCENARIOS
			.iter()
			.map(|scenario| scenario.name)
			.collect::<Vec<_>>()
			.join(", ")
	};
	match wanted.trim() {
		"all" => Ok(SCENARIOS.iter().collect()),
		"default" => Ok(SCENARIOS
			.iter()
			.filter(|scenario| scenario.nodes < 1_000_000)
			.collect()),
		list => list
			.split(',')
			.map(|name| {
				let name = name.trim();
				scenario(name).ok_or_else(|| {
					format!(
						"no scenario named {name:?}; this harness knows: {}",
						known()
					)
				})
			})
			.collect(),
	}
}

// ---------------------------------------------------------------------------------------------
// The two commands
// ---------------------------------------------------------------------------------------------

/// Run the scenarios `SYNC_BENCH_SCENARIO` names and write ONE fresh result file.
///
/// `SYNC_BENCH_SCENARIO` takes one name, a comma-separated list, `default` (every scenario but the
/// 1M rows) or `all`, and defaults to `default`; `SYNC_BENCH_SAMPLES` overrides the sample
/// count; `SYNC_BENCH_OUT` is the DIRECTORY the file lands in.
///
/// # Errors
///
/// When `SYNC_BENCH_SCENARIO` names something that is not a scenario, or the result file cannot be
/// written.
pub fn run() -> Result<String, String> {
	// A MEMORY child, spawned by `measure_memory`. It answers before anything here builds a tree:
	// a child that built one would carry the whole construction in the resident set it was spawned
	// to report.
	if let Ok(spec) = std::env::var("SYNC_BENCH_MEM_CHILD") {
		answer_memory_child(Path::new(&spec));
		return Ok(String::new());
	}
	let wanted = std::env::var("SYNC_BENCH_SCENARIO").unwrap_or_else(|_| "default".to_owned());
	let samples = env_usize("SYNC_BENCH_SAMPLES", DEFAULT_SAMPLES).max(1);
	let chosen = select(&wanted)?;

	let (commit, toolchain, machine, profile) = run_meta();
	let dir = std::env::var("SYNC_BENCH_OUT")
		.map(PathBuf::from)
		.unwrap_or_else(|_| std::env::temp_dir());
	fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
	// Before anything is measured: a refusal after an hour of samples would be a refusal nobody
	// waits for.
	refuse_unreproducible(&dir, &published_dir(), &commit)?;
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.build()
		.map_err(|e| format!("building the bench runtime: {e}"))?;
	// On the runtime, because the calibration goes through `mark`'s own task-local.
	let overhead = runtime.block_on(mark_overhead_ns());
	let mut run = RunFile {
		harness_version: HARNESS_VERSION,
		started: chrono::Utc::now().to_rfc3339(),
		commit,
		toolchain,
		machine,
		profile,
		allocator: allocator(),
		mark_overhead_ns: overhead,
		records: Vec::new(),
	};
	// One fresh file per run, named so two runs cannot land on one path. Nothing is ever appended
	// to: an appended table is how a stale row comes to read as a fresh one.
	// The selector is in the name to make it readable, and the uuid is what makes it unique — so
	// the selector is BOUNDED and the uuid is not. A comma list naming the eleven scenarios that
	// cover every change class composes 314 bytes against a 255-byte limit, and the run then died
	// on `File name too long` after measuring its first scenario and wrote nothing at all.
	let label: String = wanted
		.replace(['/', ' ', ','], "_")
		.chars()
		.take(40)
		.collect();
	let path = dir.join(format!("sync_bench_{label}_{}.json", uuid::Uuid::new_v4()));
	let write = |run: &RunFile| -> Result<(), String> {
		let json = serde_json::to_string_pretty(run)
			.map_err(|e| format!("encoding the result file: {e}"))?;
		fs::write(&path, json).map_err(|e| format!("writing {}: {e}", path.display()))
	};
	// Rewritten in full after EVERY scenario rather than once at the end. The assertions in here
	// are the point of the harness and they are meant to fire; a default sweep is thirty scenarios
	// and hours of machine time, and holding every record until the last one meant one assertion
	// doing its job threw away every measurement already taken.
	for scenario in &chosen {
		run.records
			.extend(runtime.block_on(run_scenario(scenario, samples)));
		write(&run)?;
	}
	Ok(format!("{}\n{}", summarize(&run), path.display()))
}

/// The median of `values`, which must not be empty.
///
/// The MEAN of the two middles for an even count, not the upper one. `SYNC_BENCH_SAMPLES` is a
/// user-facing knob, and an even setting published a biased-high figure under a name that says
/// median.
fn median(values: &mut [f64]) -> f64 {
	values.sort_by(f64::total_cmp);
	let middle = values.len() / 2;
	if values.len().is_multiple_of(2) {
		f64::midpoint(values[middle - 1], values[middle])
	} else {
		values[middle]
	}
}

/// The headline table: one row per scenario, produced by COMMITTED code.
///
/// The published baseline's `speedup` and top-step columns came from a script that lived outside
/// the repository, so the table beside the result files could not be regenerated from them, and a
/// later run could not be rendered on the same conventions — the script and this module did not
/// even agree on what a median is.
///
/// `speedup` is the scenario's own yardstick over its own pass, both medians over the same number
/// of samples taken in the same state.
///
/// `spread` is the pass's max over its min WITHIN one run: a range, and deliberately no longer
/// described as this row's resolution. The samples run back to back in one process and drift
/// upward across it, so the figure is a trend as much as a noise floor, and it is systematically
/// tighter than the same row's movement between two runs — which is the movement a reader is
/// actually asking about. `BASELINE.md` carries that one, measured from two sweeps of one binary
/// at one commit, and it is the figure a change is read against.
fn headline(run: &RunFile) -> String {
	let mut passes: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
	let mut yardsticks: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
	let mut nodes: BTreeMap<&str, usize> = BTreeMap::new();
	let mut actions: BTreeMap<&str, u64> = BTreeMap::new();
	let mut reps: BTreeMap<&str, usize> = BTreeMap::new();
	for record in &run.records {
		nodes.insert(&record.scenario, record.nodes);
		match record.metric.as_str() {
			"total" => {
				// From a TIMING record only. A memory record divides by nothing and carries
				// `reps: 1`, and taking this from whichever record came last would print that
				// beside a figure divided by 32.
				reps.insert(&record.scenario, record.reps);
				passes.entry(&record.scenario).or_default().push(record.ms);
			}
			"yardstick_whole_pass" => yardsticks
				.entry(&record.scenario)
				.or_default()
				.push(record.ms),
			"plan:actions" => {
				actions.insert(&record.scenario, record.count.unwrap_or_default());
			}
			_ => {}
		}
	}
	let mut out = String::new();
	writeln!(
		out,
		"scenario\tnodes\treps\tn\tpass_ms\tspread\tyardstick_ms\tspeedup\tactions"
	)
	.expect("writing to a String never fails");
	for (name, mut pass) in passes {
		let (low, high) = pass
			.iter()
			.fold((f64::MAX, f64::MIN), |(low, high), value| {
				(low.min(*value), high.max(*value))
			});
		let n = pass.len();
		let pass_ms = median(&mut pass);
		let mut yardstick = yardsticks.remove(name).unwrap_or_default();
		let yardstick_ms = if yardstick.is_empty() {
			f64::NAN
		} else {
			median(&mut yardstick)
		};
		writeln!(
			out,
			"{name}\t{}\t{}\t{n}\t{pass_ms:.4}\t{:.2}x\t{yardstick_ms:.1}\t{:.1}x\t{}",
			nodes.get(name).copied().unwrap_or_default(),
			reps.get(name).copied().unwrap_or_default(),
			if low > 0.0 { high / low } else { f64::NAN },
			yardstick_ms / pass_ms,
			actions.get(name).copied().unwrap_or_default(),
		)
		.expect("writing to a String never fails");
	}
	out
}

/// What a metric's numbers are IN.
///
/// Three units share one record shape. Printing them all in a column headed `ms` is how a plan of
/// 102 actions came to read as `0.0001 ms`, and it is why the memory figures below are not simply
/// more `ms` rows: every table here prints this beside the number, and [`compare`] diffs a metric
/// in its own unit instead of diffing a zero against a zero.
fn unit(metric: &str) -> &'static str {
	if metric.starts_with("mem:") {
		"bytes"
	} else if metric.starts_with("plan:") || metric.starts_with("walk:") || metric == "marks" {
		"count"
	} else {
		"ms"
	}
}

/// One record's number, in its own unit.
fn value(record: &Record) -> f64 {
	if unit(&record.metric) == "ms" {
		record.ms
	} else {
		record.count.unwrap_or_default() as f64
	}
}

/// Every metric of a run as `(scenario, definition_hash, metric) -> samples`, each sample in the
/// metric's own [`unit`].
fn grouped(run: &RunFile) -> BTreeMap<(String, u64, String), Vec<f64>> {
	let mut out: BTreeMap<(String, u64, String), Vec<f64>> = BTreeMap::new();
	for record in &run.records {
		out.entry((
			record.scenario.clone(),
			record.definition_hash,
			record.metric.clone(),
		))
		.or_default()
		.push(value(record));
	}
	out
}

/// What a pass costs in MEMORY, one row per scenario, from the fresh-process children.
///
/// Every figure here was taken in a child process that ran one pass and little else (see
/// [`measure_in_child`] for what "little else" covers), and every column is a median over that
/// scenario's memory samples. `widest_spread` is that row's widest max over its min WITHIN one
/// run: a range and not this row's resolution, the same caveat [`headline`]'s `spread` carries.
/// The samples run back to back in one parent and the children are not independent of the machine
/// they land on, so the figure is systematically tighter than the same row's movement between two
/// runs. `BASELINE.md` carries that one, measured from two sweeps of one binary at one commit.
///
/// There are TWO attribution ratios here and not one, because a single one answered neither
/// question. A process that has loaded a pair has also opened SQLite, built a runtime and a client
/// and read a tree; a process running a pass over an already-loaded pair has done none of that
/// again. Dividing the pass's structures by everything since the floor charged the engine's
/// one-time cost to the pass and reported 2 % where the pass's own structures were simply small.
///
/// - `pair_attributed` is what the resident baseline COMPUTES itself as, over what loading it
///   actually added (`pair_loaded` minus `floor`). The remainder is the engine's fixed cost:
///   SQLite's page cache and its mapped pages, the tokio runtime, the client, the store's own
///   row decoding. At a thousand rows that remainder IS the figure — the tree is a quarter of a
///   mebibyte and the process grew by ten.
/// - `pass_attributed` is what the pass's two SIDES compute themselves as, over what the pass
///   added on top of the loaded pair (`widest` minus `pair_loaded`). The baseline is deliberately
///   not in this ratio: it was resident before the pass began. For a CHANGE-SCOPED pass the sides
///   are carried and hold only what the pass observed, so a small numerator here is the design
///   working rather than an accounting failure — what is left is the plan, the facts, the
///   overlays' transients and whatever the allocator kept.
///
/// Neither is expected to reach 100 %, in either direction: a map's `capacity` is allocated
/// without necessarily being faulted in, so a computed figure can exceed a resident one. What the
/// columns are for is the SIZE of what nobody has accounted for, which is the only honest thing to
/// publish until something accounts for it.
///
/// The computed columns carry three decimals and the ratios one, because they span five orders of
/// magnitude: a change-scoped pass's two sides at a thousand rows are about 2.6 kB, and at two
/// decimals that printed `0.00 MiB` and `0%` — which reads as "nothing could be attributed" rather
/// than "the sides are two kilobytes", and is the exact misreading this whole table exists to
/// stop. The per-metric table below carries every one of these figures in bytes regardless.
fn memory_table(run: &RunFile) -> String {
	let mut by: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
	for record in &run.records {
		if record.metric.starts_with("mem:") {
			by.entry((record.scenario.clone(), record.metric.clone()))
				.or_default()
				.push(value(record));
		}
	}
	if by.is_empty() {
		return "# no memory samples in this run (SYNC_BENCH_MEM_SAMPLES=0)\n".to_owned();
	}
	let scenarios: BTreeSet<String> = by.keys().map(|(scenario, _)| scenario.clone()).collect();
	let mut out = String::new();
	writeln!(
		out,
		"scenario\tn\tfloor_MiB\tpair_loaded_MiB\twidest_MiB\tpeak_MiB\tafter_pass_MiB\t\
		 after_drop_MiB\tpair_computed_MiB\tpair_attributed\tpass_added_MiB\t\
		 sides_computed_KiB\tpass_attributed\twidest_spread"
	)
	.expect("writing to a String never fails");
	for name in scenarios {
		let samples = |metric: &str| by.get(&(name.clone(), format!("mem:{metric}"))).cloned();
		let med = |metric: &str| -> f64 {
			samples(metric).map_or(f64::NAN, |mut values| {
				median(&mut values) / (1024.0 * 1024.0)
			})
		};
		let widest = samples("fresh_process_pass_widest_rss").unwrap_or_default();
		let (low, high) = widest
			.iter()
			.fold((f64::MAX, f64::MIN), |(low, high), value| {
				(low.min(*value), high.max(*value))
			});
		let pair_added = med("fresh_process_pair_loaded_rss") - med("fresh_process_floor_rss");
		let pair_computed = med("pair_baseline_computed_bytes");
		let pass_added = med("fresh_process_pass_widest_over_pair_loaded_rss");
		// In KIBIBYTES, alone among the columns here. A change-scoped pass's two sides are a few
		// hundred bytes at a thousand rows, and in mebibytes to three decimals 411 B and 0 B both
		// print `0.000` — and zero is a real answer here, since a scoped pass whose remote changelist
		// was empty owns nothing on that side. The two have to be tellable apart.
		let sides_computed = med("pass_sides_computed_bytes") * 1024.0;
		writeln!(
			out,
			"{name}\t{}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.3}\t{:.1}%\t{:.2}\t\
			 {:.3}\t{:.1}%\t{:.2}x",
			widest.len(),
			med("fresh_process_floor_rss"),
			med("fresh_process_pair_loaded_rss"),
			med("fresh_process_pass_widest_rss"),
			med("fresh_process_peak_rss"),
			med("fresh_process_after_pass_rss"),
			med("fresh_process_after_everything_dropped_rss"),
			pair_computed,
			pair_computed / pair_added * 100.0,
			pass_added,
			sides_computed,
			sides_computed / pass_added * 100.0,
			if low > 0.0 { high / low } else { f64::NAN },
		)
		.expect("writing to a String never fails");
	}
	out
}

/// What a process holding a LOADED PAIR is made of, as a table that SUMS to the resident set it
/// was measured at.
///
/// The memory table above prints `floor` and `pair_loaded` and leaves everything between them as
/// one number nobody had split. At a million rows that number is ~406 MiB, of which the resident
/// tree computes as ~212 — so about 194 MiB was attributed to nothing at all, and an optimisation
/// aimed at the tree would have been aimed at less than half the problem.
///
/// Every stage column is a DELTA between two stage samples of the same child, so the five of them
/// add up to the measured `steady_state` — arithmetically, and that is ALL that says. A resident
/// set is not additive across stages: the row decode hands whole blocks back as it tears down, and
/// the allocator returns them to the kernel, so `pair_stage` is what the process grew by while
/// loading the pair and NOT what the pair costs. At a million rows it reads about 7 MiB BELOW the
/// tree the pair is holding, which is how this was found — after a round had published the
/// resulting negative remainder as a finding about the engine.
///
/// So the attribution is taken against `floor`, the one sample nothing can have been freed after:
/// `unattributed` is `steady_state - floor - tree_computed`, over the same denominator
/// [`memory_table`]'s `pair_attributed` divides by, so the two tables cannot print opposite-signed
/// readings of one fact. It is what a loaded process holds that the tree does not account for:
/// SQLite's pages, the row decode's retained allocations, the runtime and the client, and whatever
/// the allocator has not handed back.
///
/// The remainder is printed, never folded in. A term nobody can attribute is a term with a size
/// and a name, which is the only honest thing to publish until something accounts for it. A
/// remainder below ZERO is a different thing — the tree is live inside the process that was
/// measured, so there is no reading of it that is about the engine — and it fails the run here
/// rather than publishing. The run file is already on disk when this renders (`run` rewrites it
/// after every scenario), so nothing measured is lost.
fn accounting_table(run: &RunFile) -> String {
	let mut by: BTreeMap<(String, String), Vec<f64>> = BTreeMap::new();
	for record in &run.records {
		if record.metric.starts_with("mem:") || record.metric.starts_with("walk:") {
			by.entry((record.scenario.clone(), record.metric.clone()))
				.or_default()
				.push(value(record));
		}
	}
	if by.is_empty() {
		return String::new();
	}
	let scenarios: BTreeSet<String> = by.keys().map(|(scenario, _)| scenario.clone()).collect();
	// Whatever terms this run published, rather than a list here that could fall behind
	// `ResidentTerms::named`.
	let terms: Vec<String> = by
		.keys()
		.filter_map(|(_, metric)| {
			metric
				.strip_prefix("mem:pair_baseline_term_")?
				.strip_suffix("_computed_bytes")
				.map(str::to_owned)
		})
		.collect::<BTreeSet<String>>()
		.into_iter()
		.collect();
	let mut out = String::new();
	writeln!(
		out,
		"# what a process holding the loaded pair is made of (MiB, columns sum to steady_state)"
	)
	.expect("writing to a String never fails");
	writeln!(
		out,
		"scenario\tn\tfloor\truntime\tclient\tengine_open\tpair_stage\tsteady_state\t\
		 tree_computed\tunattributed\tunattributed_pct"
	)
	.expect("writing to a String never fails");
	for name in &scenarios {
		let med = |metric: &str| -> f64 {
			by.get(&(name.clone(), metric.to_owned()))
				.cloned()
				.map_or(f64::NAN, |mut values| {
					median(&mut values) / (1024.0 * 1024.0)
				})
		};
		let n = by
			.get(&(name.clone(), "mem:fresh_process_floor_rss".to_owned()))
			.map_or(0, Vec::len);
		let floor = med("mem:fresh_process_floor_rss");
		let runtime = med("mem:fresh_process_rss_after_runtime");
		let client = med("mem:fresh_process_rss_after_client");
		let engine = med("mem:fresh_process_rss_after_engine_open");
		let loaded = med("mem:fresh_process_pair_loaded_rss");
		let tree = med("mem:pair_baseline_computed_bytes");
		// What the process grew by since its floor — the only span here that nothing has been
		// freed after, and the denominator `memory_table`'s `pair_attributed` already uses.
		let since_floor = loaded - floor;
		let unattributed = since_floor - tree;
		// In every build. A process cannot hold less than the live tree inside it, so a negative
		// remainder says the measurement has stopped meaning what the column says — which is
		// exactly what happened, silently, for a whole round.
		assert!(
			!since_floor.is_finite() || !tree.is_finite() || unattributed >= 0.0,
			"{name}: the loaded process grew {since_floor:.3} MiB over its floor while holding a \
			 tree that computes {tree:.3} MiB"
		);
		writeln!(
			out,
			"{name}\t{n}\t{floor:.1}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{loaded:.1}\t{tree:.3}\t\
			 {unattributed:.1}\t{:.1}%",
			runtime - floor,
			client - runtime,
			engine - client,
			loaded - engine,
			unattributed / since_floor * 100.0,
		)
		.expect("writing to a String never fails");
	}
	if !terms.is_empty() {
		writeln!(out, "# the resident tree, term by term (MiB)")
			.expect("writing to a String never fails");
		writeln!(out, "scenario\t{}\ttotal", terms.join("\t"))
			.expect("writing to a String never fails");
		for name in &scenarios {
			write!(out, "{name}").expect("writing to a String never fails");
			let mut total = 0.0;
			for term in &terms {
				let metric = format!("mem:pair_baseline_term_{term}_computed_bytes");
				let value = by
					.get(&(name.clone(), metric))
					.cloned()
					.map_or(f64::NAN, |mut values| {
						median(&mut values) / (1024.0 * 1024.0)
					});
				total += value;
				write!(out, "\t{value:.3}").expect("writing to a String never fails");
			}
			writeln!(out, "\t{total:.3}").expect("writing to a String never fails");
		}
	}
	writeln!(
		out,
		"# what a CARRIED side materialized during the pass (counts; whole_rows > 0 on a scoped \
		 row means the pass built the whole tree)"
	)
	.expect("writing to a String never fails");
	writeln!(
		out,
		"scenario\twhole_calls\twhole_rows\tsubtree_calls\tsubtree_rows"
	)
	.expect("writing to a String never fails");
	for name in &scenarios {
		let count = |metric: &str| -> f64 {
			by.get(&(name.clone(), metric.to_owned()))
				.cloned()
				.map_or(f64::NAN, |mut values| median(&mut values))
		};
		writeln!(
			out,
			"{name}\t{:.0}\t{:.0}\t{:.0}\t{:.0}",
			count("walk:carried_entries_whole_calls"),
			count("walk:carried_entries_whole_rows"),
			count("walk:carried_entries_subtree_calls"),
			count("walk:carried_entries_subtree_rows"),
		)
		.expect("writing to a String never fails");
	}
	out
}

/// A human table of one run: median, min and max per metric, so a bimodal sample set is visible
/// rather than averaged away.
fn summarize(run: &RunFile) -> String {
	let mut out = String::new();
	writeln!(
		out,
		"# {} {} {} ({}, {}), mark {:.0} ns",
		run.commit.get(..12).unwrap_or(&run.commit),
		run.toolchain,
		run.machine,
		run.profile,
		run.allocator,
		run.mark_overhead_ns,
	)
	.expect("writing to a String never fails");
	out.push_str(&headline(run));
	out.push_str(&memory_table(run));
	out.push_str(&accounting_table(run));
	writeln!(out, "scenario\tmetric\tunit\tn\tmedian\tmin\tmax")
		.expect("writing to a String never fails");
	for ((scenario, _, metric), mut samples) in grouped(run) {
		let (min, max) = samples
			.iter()
			.fold((f64::MAX, f64::MIN), |(lo, hi), value| {
				(lo.min(*value), hi.max(*value))
			});
		writeln!(
			out,
			"{scenario}\t{metric}\t{}\t{}\t{:.4}\t{:.4}\t{:.4}",
			unit(&metric),
			samples.len(),
			median(&mut samples),
			min,
			max,
		)
		.expect("writing to a String never fails");
	}
	out
}

/// Diff two result files.
///
/// Refusals, all of them loud, because the failure this exists to catch is a diff that LOOKS
/// complete:
///
/// - a scenario whose `definition_hash`, `fixture_hash` or node count differs is INCOMPARABLE and
///   is not diffed — it was redefined, or the generator built it a different tree;
/// - a metric present on one side only is printed as ONLY IN BEFORE / ONLY IN AFTER rather than
///   skipped. A renamed or dropped engine phase used to vanish from BOTH sides with no marker,
///   leaving a table of `+0.0%` rows and nothing saying a phase had gone;
/// - a scenario whose YARDSTICK moved more than [`YARDSTICK_TOLERANCE`] between the two runs is
///   marked `MACHINE MOVED` and every percentage in it is tagged, because a whole pass over the
///   same fixture through the same function is the one quantity in the file that cannot have got
///   faster by itself;
/// - both runs' `mark_overhead_ns` is printed in the header. It is one step mark timed through
///   [`mark`] itself, so it is a free per-run reading of how fast this machine was answering at
///   all: two files whose marks disagree by more than a few per cent were taken on a machine in
///   two different states, whatever their commits say;
/// - a percentage whose two medians sit inside each other's measured range is marked
///   `(within spread)`, and the sample counts are printed, so a noisy figure does not read as a
///   finding. Read that marker as a noise floor and not as a gate: a run's INTERNAL spread at three
///   samples is far tighter than the variation between two runs on one machine, so at the default
///   sample count most rows carry it, including ones whose medians are tens of per cent apart.
///
/// # Errors
///
/// When either file cannot be read or decoded.
pub fn compare(before: &Path, after: &Path) -> Result<String, String> {
	/// How far a scenario's whole-pass yardstick may move between two runs before every percentage
	/// in that scenario is a statement about the machine as much as about the code.
	///
	/// Ten per cent is this harness's own observed run-to-run movement on an otherwise quiet
	/// machine; a yardstick that moved more than that did not move because a pass got faster.
	const YARDSTICK_TOLERANCE: f64 = 0.10;
	let read = |path: &Path| -> Result<RunFile, String> {
		let raw =
			fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
		serde_json::from_str(&raw).map_err(|e| format!("decoding {}: {e}", path.display()))
	};
	let (before, after) = (read(before)?, read(after)?);
	let mut out = String::new();
	if before.harness_version != after.harness_version {
		return Err(format!(
			"these files were written by different harnesses (v{} and v{}); their fields do not \
			 mean the same thing",
			before.harness_version, after.harness_version
		));
	}
	writeln!(
		out,
		"# before {} ({}, mark {:.0} ns)\n# after  {} ({}, mark {:.0} ns)",
		before.commit.get(..12).unwrap_or(&before.commit),
		before.profile,
		before.mark_overhead_ns,
		after.commit.get(..12).unwrap_or(&after.commit),
		after.profile,
		after.mark_overhead_ns,
	)
	.expect("writing to a String never fails");
	if before.profile != after.profile
		|| before.toolchain != after.toolchain
		|| before.machine != after.machine
		|| before.allocator != after.allocator
	{
		writeln!(
			out,
			"# WARNING: different build profile, toolchain, MACHINE or allocator — these figures \
			 are not comparable"
		)
		.expect("writing to a String never fails");
	}

	let (mut old, mut new) = (grouped(&before), grouped(&after));
	// What each side actually measured: the declaration, the tree the generator built from it, and
	// the node count a per-node figure would be read against. `definition_hash` alone covers only
	// the first, so a change under `probe` moved every fixture while leaving every hash put.
	let facts = |run: &RunFile| -> BTreeMap<String, (u64, u64, usize)> {
		run.records
			.iter()
			.map(|record| {
				(
					record.scenario.clone(),
					(record.definition_hash, record.fixture_hash, record.nodes),
				)
			})
			.collect()
	};
	let (old_facts, new_facts) = (facts(&before), facts(&after));
	let mut incomparable: BTreeSet<String> = BTreeSet::new();
	for (scenario, (old_hash, old_fixture, old_nodes)) in &old_facts {
		let Some((new_hash, new_fixture, new_nodes)) = new_facts.get(scenario) else {
			continue;
		};
		let why = if old_hash != new_hash {
			Some(format!(
				"the scenario was redefined ({old_hash:016x} -> {new_hash:016x})"
			))
		} else if old_fixture != new_fixture {
			Some(format!(
				"the fixture GENERATOR built a different tree ({old_fixture:016x} -> \
				 {new_fixture:016x}); the declaration is unchanged, so something under `probe` moved"
			))
		} else if old_nodes != new_nodes {
			Some(format!(
				"the fixture held {old_nodes} node(s) and now holds {new_nodes}"
			))
		} else {
			None
		};
		if let Some(why) = why {
			incomparable.insert(scenario.clone());
			writeln!(
				out,
				"{scenario}\tINCOMPARABLE\t{why}; rows from the two are not two measurements of \
				 one thing"
			)
			.expect("writing to a String never fails");
		}
	}

	// The one quantity in the file that cannot have got faster by itself. `compare` used to refuse
	// on the profile, the toolchain, the machine NAME and the allocator — none of which move when
	// the machine is simply busier than it was an hour ago, which is the way two runs of one binary
	// at one commit come to differ by tens of per cent.
	let yardsticks = |run: &RunFile| -> BTreeMap<String, f64> {
		let mut by: BTreeMap<String, Vec<f64>> = BTreeMap::new();
		for record in &run.records {
			if record.metric == "yardstick_whole_pass" {
				by.entry(record.scenario.clone())
					.or_default()
					.push(record.ms);
			}
		}
		by.into_iter()
			.map(|(scenario, mut samples)| (scenario, median(&mut samples)))
			.collect()
	};
	let (old_yardstick, new_yardstick) = (yardsticks(&before), yardsticks(&after));
	let mut moved: BTreeSet<String> = BTreeSet::new();
	for (scenario, was) in &old_yardstick {
		let Some(now) = new_yardstick.get(scenario) else {
			continue;
		};
		if *was > 0.0 && (now / was - 1.0).abs() > YARDSTICK_TOLERANCE {
			moved.insert(scenario.clone());
			writeln!(
				out,
				"{scenario}\tMACHINE MOVED\tthe same whole pass over the same fixture measured \
				 {was:.1} ms and now measures {now:.1} ms ({:+.1} %); every percentage below for it \
				 carries that as well as whatever the code did",
				(now - was) / was * 100.0
			)
			.expect("writing to a String never fails");
		}
	}
	writeln!(
		out,
		"scenario\tmetric\tunit\tn_before\tn_after\tbefore\tafter\tchange"
	)
	.expect("writing to a String never fails");
	let range = |samples: &[f64]| -> (f64, f64) {
		samples
			.iter()
			.fold((f64::MAX, f64::MIN), |(low, high), value| {
				(low.min(*value), high.max(*value))
			})
	};
	let keys: Vec<(String, u64, String)> = new.keys().cloned().collect();
	for key in keys {
		let Some(mut after_samples) = new.remove(&key) else {
			continue;
		};
		let (scenario, hash, metric) = key;
		if incomparable.contains(&scenario) {
			continue;
		}
		let Some(mut before_samples) = old.remove(&(scenario.clone(), hash, metric.clone())) else {
			writeln!(
				out,
				"{scenario}\t{metric}\t{}\tONLY IN AFTER\t-\t-\t-\t-",
				unit(&metric)
			)
			.expect("writing to a String never fails");
			continue;
		};
		let (was, now) = (median(&mut before_samples), median(&mut after_samples));
		let (was_low, was_high) = range(&before_samples);
		let (now_low, now_high) = range(&after_samples);
		let change = if was > 0.0 {
			let percent = format!("{:+.1}%", (now - was) / was * 100.0);
			let percent = if moved.contains(&scenario) {
				format!("{percent} (machine moved)")
			} else {
				percent
			};
			// Two medians inside each other's measured range say nothing: at three samples one
			// outlier can BE the median, and a percentage taken off two of them reads as a finding.
			if was_low <= now_high && now_low <= was_high {
				format!("{percent} (within spread)")
			} else {
				percent
			}
		} else {
			"n/a".to_owned()
		};
		writeln!(
			out,
			"{scenario}\t{metric}\t{}\t{}\t{}\t{was:.4}\t{now:.4}\t{change}",
			unit(&metric),
			before_samples.len(),
			after_samples.len(),
		)
		.expect("writing to a String never fails");
	}
	// Whatever the AFTER side never claimed — a step the engine stopped running is exactly the
	// regression this harness exists to catch, and it used to leave no trace here at all.
	for (scenario, _, metric) in old.into_keys() {
		if incomparable.contains(&scenario) {
			continue;
		}
		writeln!(
			out,
			"{scenario}\t{metric}\t{}\tONLY IN BEFORE\t-\t-\t-\t-",
			unit(&metric)
		)
		.expect("writing to a String never fails");
	}
	Ok(out)
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

	/// One memory child's stage samples, as a run file the accounting table can be asked to render.
	fn accounting_run(loaded_mib: f64, tree_mib: f64) -> RunFile {
		let mib = |value: f64| (value * 1024.0 * 1024.0) as u64;
		let record = |metric: &str, bytes: u64| Record {
			scenario: "twoway_idle_1m".to_owned(),
			scenario_version: 1,
			definition_hash: 7,
			sample: Some(0),
			metric: metric.to_owned(),
			ms: 0.0,
			// BYTES, as every `mem:` metric is: a memory figure read off `ms` is a table of zeroes.
			count: Some(bytes),
			nodes: 1_065_236,
			fixture_hash: 9,
			reps: 1,
		};
		RunFile {
			harness_version: HARNESS_VERSION,
			started: "2026-01-01T00:00:00Z".to_owned(),
			commit: "abcdef123456".to_owned(),
			toolchain: "nightly".to_owned(),
			machine: "here macos/aarch64".to_owned(),
			profile: "debug_assertions".to_owned(),
			allocator: "system".to_owned(),
			mark_overhead_ns: 25.0,
			records: vec![
				record("mem:fresh_process_floor_rss", mib(6.8)),
				record("mem:fresh_process_rss_after_runtime", mib(7.9)),
				record("mem:fresh_process_rss_after_client", mib(9.5)),
				record("mem:fresh_process_rss_after_engine_open", mib(16.0)),
				record("mem:fresh_process_pair_loaded_rss", mib(loaded_mib)),
				record("mem:pair_baseline_computed_bytes", mib(tree_mib)),
			],
		}
	}

	/// A process that holds less than the live tree inside it fails the run.
	///
	/// The table published `-7.0 MiB` unattributed at a million rows for a whole round, under a
	/// column headed "what the pair holds that the tree does not account for", and a later round
	/// built a mechanism on the drop it appeared in. Nothing objected, because the remainder was
	/// taken from a stage delta rather than from a span a resident set can be read across.
	#[test]
	#[should_panic(expected = "while holding a tree that computes")]
	fn an_impossible_accounting_row_fails_the_run() {
		// The shape of the 1M row as it was published: a steady state below floor + tree.
		let _ = accounting_table(&accounting_run(130.0, 128.743));
	}

	/// And the row that IS possible still renders — the real 1M figures clear the check by ~2 MiB,
	/// so an assertion one step tighter would fail every million-row run instead of publishing one.
	#[test]
	fn a_possible_accounting_row_publishes() {
		let table = accounting_table(&accounting_run(137.64, 128.743));
		assert!(
			table.contains("twoway_idle_1m"),
			"the accounting table skipped the only scenario in the run: {table}"
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

	/// A dirty working tree is on the stamp. Every file of the first published baseline named a
	/// commit that held three of its thirty-three scenarios, because `git rev-parse HEAD` says
	/// nothing about the tree the binary was built from.
	#[test]
	fn a_dirty_tree_is_stamped_as_one() {
		assert_eq!(commit_stamp("abcdef123456", ""), "abcdef123456");
		assert_eq!(commit_stamp("abcdef123456", "   \n  "), "abcdef123456");
		assert_eq!(
			commit_stamp("abcdef123456", " M filen-sdk-rs/src/sync_engine/bench.rs"),
			"abcdef123456-dirty"
		);
	}

	/// A dirty or unstamped build may not write into the published directory, however the path to
	/// it is spelled — and anywhere else it may.
	#[test]
	fn a_dirty_build_cannot_publish() {
		let scratch = std::env::temp_dir().join(format!("bench_publish_{}", uuid::Uuid::new_v4()));
		let published = scratch.join("baseline");
		let elsewhere = scratch.join("elsewhere");
		fs::create_dir_all(published.join("nested")).unwrap();
		fs::create_dir_all(&elsewhere).unwrap();
		let respelled = published.join("..").join("baseline");
		for out in [&published, &published.join("nested"), &respelled] {
			for stamp in ["abcdef123456-dirty", "unknown"] {
				assert!(
					refuse_unreproducible(out, &published, stamp).is_err(),
					"{} accepted a {stamp:?} build",
					out.display()
				);
			}
			assert!(refuse_unreproducible(out, &published, "abcdef123456").is_ok());
		}
		assert!(refuse_unreproducible(&elsewhere, &published, "abcdef123456-dirty").is_ok());
		// A sibling whose NAME starts with the published directory's is not inside it.
		let sibling = scratch.join("baseline_old");
		fs::create_dir_all(&sibling).unwrap();
		assert!(refuse_unreproducible(&sibling, &published, "abcdef123456-dirty").is_ok());
		fs::remove_dir_all(&scratch).unwrap();
	}

	/// An even sample count takes the MEAN of the two middles. `SYNC_BENCH_SAMPLES` is a
	/// user-facing knob, and the upper middle is not a median.
	#[test]
	fn the_median_of_an_even_sample_set_is_the_middle_pair() {
		assert!((median(&mut [3.0, 1.0, 2.0]) - 2.0).abs() < f64::EPSILON);
		assert!((median(&mut [4.0, 1.0, 3.0, 2.0]) - 2.5).abs() < f64::EPSILON);
	}

	/// The locality pair really does pick opposite localities — given the PATH order
	/// `prepare_bed` sorts its rows into, which `apply_change` asserts it got.
	#[test]
	fn the_locality_pair_picks_opposite_localities() {
		// Ten leaf directories of ten files each, in path order.
		let paths: Vec<String> = (0..100)
			.map(|index| format!("dir_{:03}/file_{:03}", index / 10, index % 10))
			.collect();
		let directories = |picked: Vec<usize>| -> usize {
			let mut seen: Vec<&str> = picked
				.iter()
				.map(|index| {
					paths[*index]
						.rsplit_once('/')
						.expect("every fixture path has a parent")
						.0
				})
				.collect();
			seen.sort_unstable();
			seen.dedup();
			seen.len()
		};
		assert_eq!(
			directories(pick_indices(100, 10, false)),
			1,
			"clustered edits must land in as few directories as they can"
		);
		assert_eq!(
			directories(pick_indices(100, 10, true)),
			10,
			"scattered edits must land in as many directories as there are edits"
		);
	}

	/// The set the documentation recommends is the set the selector can produce.
	#[test]
	fn the_documented_scenario_sets_are_selectable() {
		let all = select("all").expect("all names every scenario");
		assert_eq!(all.len(), SCENARIOS.len());
		let default = select("default").expect("default names the routine set");
		assert!(
			default.len() < all.len(),
			"the default set must leave the expensive rows out"
		);
		assert!(
			default.iter().all(|scenario| scenario.nodes < 1_000_000),
			"the default set must not build a 1M fixture"
		);
		let pair = select("twoway_idle_1k, twoway_one_file_1k").expect("a list of two");
		assert_eq!(pair.len(), 2);
		assert!(select("twoway_idle_1k,not_a_scenario").is_err());
	}

	/// Every row of the shape group changes the same NUMBER of files, so the only thing left
	/// varying across it is the tree. A per-mille of each shape's own file population is a
	/// different amount of work — the group once compared 40 edits against 102 under a heading
	/// saying the rows differed in exactly one way.
	#[test]
	fn the_shape_group_holds_its_change_volume_fixed() {
		// Membership is derived from the NAME a row is published under, rather than from a list
		// kept beside this test. A hand-kept list makes membership a fact about the test instead of
		// about the table: a sixth `*_100_edits_10k` row added with a per-mille class copied from
		// an older scenario passed it, and the group was back to comparing 40 edits against 102
		// under a heading promising one variable.
		let rows: Vec<&Scenario> = SCENARIOS
			.iter()
			.filter(|scenario| scenario.name.ends_with("_100_edits_10k"))
			.collect();
		assert!(
			rows.len() >= 5,
			"the shape group is down to {} row(s); it is the comparison group this matrix is built \
			 around",
			rows.len()
		);
		let control = scenario("twoway_balanced_100_edits_10k")
			.expect("the shape group's control is in SCENARIOS");
		for row in &rows {
			assert_eq!(
				row.change, control.change,
				"{}: the shape group compares geometry, so every row changes the same count",
				row.name
			);
			assert_eq!(
				row.nodes, control.nodes,
				"{}: the shape group compares geometry at one DECLARED size; a row asking for a \
				 different node target belongs to the sizes group. What each shape then realises \
				 differs — the published trees span 10001 to 12286 nodes for the same 10000 asked \
				 for, because a shape reaches the target by its own geometry — so a row of this \
				 group is read per node or not at all",
				row.name
			);
		}
		// And the control really is the balanced shape the others are read against.
		assert_eq!(
			(control.files_per_leaf, control.depth, control.names),
			(20, 3, NameStyle::Ascii)
		);
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
		// A directory move touches one thing, like a one-file edit does.
		assert_eq!(Change::MoveDir.count(1_000), 1);
		// A first sync changes nothing: what it measures is the ABSENCE of a baseline.
		assert_eq!(Change::FirstSync.count(1_000), 0);
		assert_eq!(Change::DeletePerMille(600).count(1_000), 600);
		// An absolute count is absolute whatever the shape's file population is.
		assert_eq!(Change::Files(100).count(1_000), 100);
		assert_eq!(Change::Files(100).count(4_096), 100);
	}

	/// Every memory metric's NAME says whose process it measured and how the figure was arrived at.
	///
	/// The structural answer to the way every memory number in this effort was misquoted: a figure
	/// called `peak_rss` silently meant "this process since it started" and was read as the cost of
	/// a phase. A name here is either a `fresh_process_*` — a figure from a process whose whole
	/// history IS this measurement — or a `*_computed_bytes`, which is a structure summing its own
	/// capacities and a different kind of number entirely.
	#[test]
	fn every_memory_metric_names_whose_process_it_measured() {
		let answer = MemAnswer {
			floor_rss: 1,
			pair_loaded_rss: 2,
			pass_widest_rss: 5,
			after_pass_rss: 3,
			after_everything_dropped_rss: 2,
			peak_rss: 9,
			rows: 10,
			actions: 1,
			pair_baseline_computed_bytes: 100,
			// Distinct on purpose: a metric carrying `baseline + view + scan` is then a value no
			// other metric has, and the assertion below can refuse it by arithmetic rather than by
			// name.
			structures: (100, 20, 3),
			rss_after_runtime: 1,
			rss_after_client: 1,
			rss_after_engine_open: 1,
			pair_baseline_terms: vec![("nodes".to_owned(), 60), ("names".to_owned(), 40)],
			carried_walks: (7, 8, 9, 11),
			baseline_reads: (13, 17),
			at_step: vec![("from_baseline".to_owned(), 4)],
		};
		let names: Vec<String> = answer.metrics().into_iter().map(|(name, _)| name).collect();
		for name in &names {
			// A child publishes two kinds of figure. A `mem:` one is BYTES and must say whose
			// process it measured or that it was computed; a `walk:` one is a COUNT of what a
			// carried side materialized, and printing it in mebibytes is the same misreading under
			// a different name.
			if let Some(tail) = name.strip_prefix("mem:") {
				assert!(
					tail.starts_with("fresh_process_") || tail.ends_with("_computed_bytes"),
					"{name} says neither whose process it measured nor that it was computed rather \
					 than observed"
				);
				assert_eq!(
					unit(name),
					"bytes",
					"{name} would be printed in the wrong unit"
				);
			} else if name.starts_with("walk:") {
				assert_eq!(
					unit(name),
					"count",
					"{name} counts walks and would be printed as bytes"
				);
			} else {
				panic!("{name} is marked as neither a memory figure nor a walk count");
			}
		}
		// The SIDES, and no total with the pair's baseline folded into it. That sum shipped once,
		// as `mem:pass_structures_computed_bytes`: at ten thousand rows it was 98 % the baseline —
		// resident before the pass began, and excluded from the attribution ratio the same run
		// prints — under a name that reads as what the pass itself built. A name alone cannot
		// refuse its return, so the arithmetic does.
		let by_name = |wanted: &str| {
			answer
				.metrics()
				.into_iter()
				.find(|(name, _)| name == wanted)
				.map(|(_, value)| value)
		};
		assert_eq!(
			by_name("mem:pass_sides_computed_bytes"),
			Some(23),
			"the sides metric is the view and the scan, and nothing else"
		);
		for (name, value) in answer.metrics() {
			assert_ne!(
				value, 123,
				"{name} publishes the pair's baseline summed with the pass's two sides, under a \
				 name that reads as the pass's own cost"
			);
		}
		// The four kinds the definition calls for, each under its own name rather than one figure
		// serving as all of them.
		for wanted in [
			"mem:fresh_process_pair_loaded_rss",
			"mem:fresh_process_pass_widest_rss",
			"mem:fresh_process_peak_rss",
			"mem:pass_sides_computed_bytes",
		] {
			assert!(
				names.iter().any(|name| name == wanted),
				"{wanted} is not among the memory metrics"
			);
		}
		// And the profile is keyed by the engine's own marks, so it lines up with the timing table.
		assert!(
			names
				.iter()
				.any(|name| name == "mem:fresh_process_rss_at_step:from_baseline"),
			"the per-step resident set is not recorded under the engine's own step name"
		);
	}

	/// The resident-set readers answer something on the platform a run would be taken on.
	///
	/// Both of them map a failure to ZERO — `current_rss_bytes` where `ps` is missing or `/proc`
	/// absent, `peak_rss_bytes` where `getrusage` fails or the target has none — and a memory table
	/// of plausible zeroes is worse than no table at all: `pair_attributed` divides by one of them,
	/// every column prints `0.0`, and nothing in a run says the figure was never taken. A child
	/// asserts this for itself at its floor, at every sample it publishes and at its peak; this
	/// says it at `cargo test` time, which is where someone porting the harness to a target with
	/// neither reader will meet it.
	///
	/// This test covers the target that never answers. The reader that answers and then STOPS is a
	/// different failure and is not reachable from here — it is caught in the child, where the
	/// figures are published.
	#[test]
	fn the_resident_set_readers_answer_something() {
		let current = probe::current_rss_bytes();
		assert!(
			current > 0,
			"this platform answered no resident set at all, so every memory figure a run published \
			 here would be zero"
		);
		let peak = probe::peak_rss_bytes();
		assert!(
			peak >= current,
			"the high-water mark reads {peak} byte(s) against a resident set of {current}: a peak \
			 below a sample of the same process was never measured"
		);
	}

	/// A memory child announces exactly what its parent announced.
	///
	/// The announcement is the one thing the child cannot re-derive: `RemoteDeltaEntry` carries a
	/// hash and a stable id neither process may construct freely, so the parent writes lines and
	/// both build the delta through [`remote_delta`]. Two processes announcing different things is
	/// not a failure any assertion here would catch.
	#[test]
	fn a_remote_line_survives_the_trip_to_a_child() {
		let uuid = uuid::Uuid::new_v4();
		let parent = uuid::Uuid::new_v4();
		let line = RemoteLine {
			uuid: uuid.to_string(),
			parent: parent.to_string(),
			name: "file_000001.dat".to_owned(),
			stable_uuid: Some(uuid::Uuid::new_v4().to_string()),
			hash_fill: 7,
			size: 74,
			modified_millis: 1_700_000_000_000,
		};
		let encoded = serde_json::to_string(std::slice::from_ref(&line)).expect("a line encodes");
		let decoded: Vec<RemoteLine> = serde_json::from_str(&encoded).expect("a line decodes");
		assert_eq!(decoded, vec![line.clone()]);
		assert_eq!(remote_delta(&[line]), remote_delta(&decoded));
		match &remote_delta(&decoded)[0].change {
			RemoteChange::Upsert(item) => {
				assert_eq!(item.uuid, uuid);
				assert_eq!(item.parent, parent);
				assert_eq!(item.hash, Some(Blake3Hash::from([7u8; 32])));
			}
			other => panic!("a remote line is an upsert, not {other:?}"),
		}
	}

	/// Counts and bytes are not milliseconds, and nothing here prints them as though they were.
	#[test]
	fn a_metric_knows_what_unit_it_is_in() {
		assert_eq!(unit("total"), "ms");
		assert_eq!(unit("step:from_baseline"), "ms");
		assert_eq!(unit("yardstick_whole_pass"), "ms");
		assert_eq!(unit("plan:actions"), "count");
		assert_eq!(unit("marks"), "count");
		assert_eq!(unit("mem:fresh_process_peak_rss"), "bytes");
		assert_eq!(unit("walk:carried_entries_whole_rows"), "count");
		// A count reads back as its count rather than as the zero sitting in its `ms` column, which
		// is what left `plan:*` — and would have left every memory figure — invisible to a diff.
		let record = Record {
			scenario: "s".to_owned(),
			scenario_version: 1,
			definition_hash: 0,
			sample: Some(0),
			metric: "plan:actions".to_owned(),
			ms: 0.0,
			count: Some(102),
			nodes: 1,
			fixture_hash: 0,
			reps: 1,
		};
		assert!((value(&record) - 102.0).abs() < f64::EPSILON);
	}

	/// A machine that moved is not a speedup, and the marks say the machine moved.
	///
	/// `compare`'s only guard used to be each run's INTERNAL spread, which cannot see between-run
	/// drift by construction: two runs of one binary at one commit produced a plain `-49.5%` on a
	/// pass total with no marker on it. The gate is the yardstick — a whole pass over the same
	/// fixture through the same function, which cannot get faster by itself — and `mark_overhead_ns`
	/// is the second reading of the same thing, one step mark timed through `mark` itself.
	#[test]
	fn a_machine_that_moved_is_not_a_speedup() {
		let record = |metric: &str, ms: f64| Record {
			scenario: "s".to_owned(),
			scenario_version: 1,
			definition_hash: 7,
			sample: Some(0),
			metric: metric.to_owned(),
			ms,
			count: None,
			nodes: 10,
			fixture_hash: 9,
			reps: 1,
		};
		let run = |yardstick: f64, total: f64, mark: f64| RunFile {
			harness_version: HARNESS_VERSION,
			started: "2026-01-01T00:00:00Z".to_owned(),
			commit: "abcdef123456".to_owned(),
			toolchain: "nightly".to_owned(),
			machine: "here macos/aarch64".to_owned(),
			profile: "debug_assertions".to_owned(),
			allocator: "system".to_owned(),
			mark_overhead_ns: mark,
			records: vec![
				record("total", total),
				record("yardstick_whole_pass", yardstick),
			],
		};
		let write = |run: &RunFile| -> PathBuf {
			let path = std::env::temp_dir()
				.join(format!("filen_bench_compare_{}.json", uuid::Uuid::new_v4()));
			fs::write(&path, serde_json::to_string(run).expect("a run encodes"))
				.expect("writing a result file");
			path
		};
		// Same code, a machine 30 % slower at the one pass that cannot have changed — and a pass
		// total that halved.
		let before = write(&run(100.0, 1.0, 25.0));
		let drifted = write(&run(130.0, 0.5, 41.0));
		// And the same halving on a machine that held still.
		let steady = write(&run(101.0, 0.5, 26.0));
		let moved = compare(&before, &drifted).expect("two readable files diff");
		let quiet = compare(&before, &steady).expect("two readable files diff");
		for path in [&before, &drifted, &steady] {
			fs::remove_file(path).ok();
		}
		assert!(
			moved.contains("MACHINE MOVED"),
			"a yardstick that moved 30 % is the machine, not the code:\n{moved}"
		);
		assert!(
			moved.contains("-50.0% (machine moved)"),
			"every percentage under a moved yardstick has to carry it:\n{moved}"
		);
		assert!(
			moved.contains("mark 25 ns") && moved.contains("mark 41 ns"),
			"both runs' mark overhead belongs in the header: it is a free second reading of how \
			 fast the machine was answering at all:\n{moved}"
		);
		assert!(
			!quiet.contains("MACHINE MOVED"),
			"a yardstick that held to 1 % is not the machine moving:\n{quiet}"
		);
	}

	/// Every scenario in the table satisfies the invariants a RUN checks, through the very function
	/// a run checks them with — so a scenario that could not tell a working engine from a degenerate
	/// one is rejected when it is ADDED, rather than when somebody finally runs it.
	#[test]
	fn every_scenario_rules_out_a_pass_that_did_nothing() {
		for scenario in SCENARIOS {
			// One thing changed in a one-node tree, the weakest case an expectation has to survive.
			// The two classes that touch nothing on disk are asked as themselves.
			let changed = usize::from(!matches!(scenario.change, Change::Idle | Change::FirstSync));
			validate(scenario, changed, 1);
		}
	}
}
