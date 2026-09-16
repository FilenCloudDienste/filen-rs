//! The sync engine's permanent cost probe: one TSV line per phase of a pass, measured on a
//! synthetic tree in a temp directory. No account, no network — every phase here is local work.
//!
//! `#[ignore]`d: it builds (and deletes) up to a million files, which is minutes of I/O.
//!
//! ```sh
//! SYNC_PROBE_N=100000 SYNC_PROBE_OUT=/tmp/probe.tsv \
//!   cargo test -p filen-sdk-rs -F sync-engine,bench-internals \
//!   --test sync_engine_probe -- --ignored --nocapture
//! ```
//!
//! `SYNC_PROBE_N` is the node count (default 10 000) and `SYNC_PROBE_SHAPE` is
//! `<files per leaf>,<levels>,<bytes per file>` (default `20,3,73` — the shape every recorded
//! number so far was taken on). `SYNC_PROBE_OUT`, if set, is a file the TSV is APPENDED to, so a
//! sweep over several sizes lands in one table.

use std::io::Write;

#[test]
#[ignore = "scale probe: minutes of local I/O; run it explicitly"]
fn sync_engine_phase_costs() {
	let tsv = filen_sdk_rs::sync_engine::probe::run();
	print!("{tsv}");

	if let Ok(path) = std::env::var("SYNC_PROBE_OUT") {
		let mut file = std::fs::OpenOptions::new()
			.create(true)
			.append(true)
			.open(&path)
			.unwrap_or_else(|e| panic!("opening {path}: {e}"));
		file.write_all(tsv.as_bytes())
			.unwrap_or_else(|e| panic!("writing {path}: {e}"));
	}
}
