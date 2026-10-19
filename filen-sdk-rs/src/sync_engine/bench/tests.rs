use num_traits::ToPrimitive;

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
	let mib = |value: f64| {
		(value * 1024.0 * 1024.0)
			.to_u64()
			.expect("a test's MiB figure fits a byte count")
	};
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
		let path =
			std::env::temp_dir().join(format!("filen_bench_compare_{}.json", uuid::Uuid::new_v4()));
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
