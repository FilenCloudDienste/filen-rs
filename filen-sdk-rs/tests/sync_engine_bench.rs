//! The sync engine's named-scenario cost harness: one command to run a scenario, one to compare two
//! result files.
//!
//! Unlike `sync_engine_probe`, which times a hand-assembled copy of a pass, this drives
//! `SyncEngine::prepare` itself — so what it reports is what the engine does, and a step the engine
//! stops running stops being timed.
//!
//! `#[ignore]`d: a scenario builds (and deletes) its whole tree on disk.
//!
//! macOS and Linux only. On Windows `PairChanges::new` marks every pair
//! `LocalEventsDegraded`, so every pass falls back to a whole read and every change-scoped
//! scenario fails its read-kind assertion — in this process and in the memory children alike.
//! Nothing here checks for it; it simply fails. Every figure in `BASELINE.md` was taken on macOS.
//!
//! Run one scenario, a list of them, or a named set. `--exact` is not optional: cargo's test filter
//! is a SUBSTRING match, so a bare `sync_engine_bench` also selects `sync_engine_bench_compare`.
//!
//! ```sh
//! SYNC_BENCH_SCENARIO=twoway_one_file_10k SYNC_BENCH_OUT=/tmp/bench \
//!   cargo test -p filen-sdk-rs -F sync-engine,bench-internals \
//!   --test sync_engine_bench -- --ignored --nocapture --exact sync_engine_bench
//! ```
//!
//! Compare two runs:
//!
//! ```sh
//! SYNC_BENCH_COMPARE=/tmp/bench/before.json,/tmp/bench/after.json \
//!   cargo test -p filen-sdk-rs -F sync-engine,bench-internals \
//!   --test sync_engine_bench -- --ignored --nocapture --exact sync_engine_bench_compare
//! ```
//!
//! `SYNC_BENCH_SCENARIO` takes one name, a comma-separated list, `default` (every scenario but the
//! three 1M rows) or `all`; it defaults to `default`. `SYNC_BENCH_SAMPLES` defaults to 3, and
//! `SYNC_BENCH_OUT` to the temp directory. Each run writes ONE fresh JSON file and never appends to
//! an existing one.
//!
//! `SYNC_BENCH_MEM_SAMPLES` (default 2, `0` to skip) is how many FRESH PROCESSES each scenario's
//! memory figures are taken in. Each one re-invokes this binary on the tree its parent already
//! built, holds what a pass holds and samples its own resident set; budget about one extra cold
//! pass apiece. They are the only memory figures worth quoting — an in-process one carries the
//! whole run's history — and they land in the same result file, under the same scenario name and
//! definition hash, as the timing figures beside them.

use filen_sdk_rs::sync_engine::bench;

#[test]
#[ignore = "cost harness: builds a tree on disk; run it explicitly"]
fn sync_engine_bench() {
	match bench::run() {
		Ok(report) => println!("{report}"),
		Err(error) => panic!("{error}"),
	}
}

#[test]
#[ignore = "reads two result files named by SYNC_BENCH_COMPARE"]
fn sync_engine_bench_compare() {
	// Skipped rather than failed when nothing named two files. This test gets SELECTED
	// incidentally — cargo's filter is a substring match, so `-- ... sync_engine_bench` picks it up
	// alongside the run above — and panicking here failed a run whose measurement had already been
	// written to disk, taking any CI job wired to the documented command red on every good run.
	let Ok(raw) = std::env::var("SYNC_BENCH_COMPARE") else {
		println!(
			"SYNC_BENCH_COMPARE is unset, so there are no two files to diff; set it to \
			 <before.json>,<after.json> to run this."
		);
		return;
	};
	let (before, after) = raw
		.split_once(',')
		.expect("SYNC_BENCH_COMPARE is <before.json>,<after.json>");
	match bench::compare(before.trim().as_ref(), after.trim().as_ref()) {
		Ok(report) => println!("{report}"),
		Err(error) => panic!("{error}"),
	}
}
