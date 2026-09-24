//! The sync engine's named-scenario cost harness: one command to run a scenario, one to compare two
//! result files.
//!
//! Unlike `sync_engine_probe`, which times a hand-assembled copy of a pass, this drives
//! `SyncEngine::prepare` itself — so what it reports is what the engine does, and a step the engine
//! stops running stops being timed.
//!
//! `#[ignore]`d: a scenario builds (and deletes) its whole tree on disk.
//!
//! Run one scenario, or all of them:
//!
//! ```sh
//! SYNC_BENCH_SCENARIO=twoway_one_file_10k SYNC_BENCH_OUT=/tmp/bench \
//!   cargo test -p filen-sdk-rs -F sync-engine,bench-internals \
//!   --test sync_engine_bench -- --ignored --nocapture sync_engine_bench
//! ```
//!
//! `SYNC_BENCH_SCENARIO` defaults to `all`, `SYNC_BENCH_SAMPLES` to 3, and `SYNC_BENCH_OUT` to the
//! temp directory. Each run writes ONE fresh JSON file and never appends to an existing one.

use filen_sdk_rs::sync_engine::bench;

#[test]
#[ignore = "cost harness: builds a tree on disk; run it explicitly"]
fn sync_engine_bench() {
	match bench::run() {
		Ok(report) => println!("{report}"),
		Err(error) => panic!("{error}"),
	}
}
