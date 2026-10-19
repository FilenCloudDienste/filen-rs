use super::*;

const MIB: u64 = 1024 * 1024;

fn at(start: Instant, millis: u64) -> Instant {
	start + Duration::from_millis(millis)
}

fn started(start: Instant) -> TransferRates {
	let mut rates = TransferRates::default();
	rates.record(
		&SyncEvent::PassStarted {
			mode: SyncMode::TwoWay,
		},
		start,
	);
	rates
}

fn progress(rel_path: &str, direction: TransferDirection, bytes: u64) -> SyncEvent {
	SyncEvent::Progress {
		rel_path: rel_path.to_owned(),
		direction,
		bytes,
		total: 10 * MIB,
	}
}

fn rate(bytes: u64, file_tenths: u64, dir_tenths: u64) -> Rate {
	Rate {
		bytes,
		file_tenths,
		dir_tenths,
	}
}

#[test]
fn cumulative_progress_becomes_bytes_moved_per_direction() {
	let start = Instant::now();
	let mut rates = started(start);
	rates.record(
		&progress("a", TransferDirection::Upload, 1000),
		at(start, 100),
	);
	rates.record(
		&progress("a", TransferDirection::Upload, 3000),
		at(start, 300),
	);
	rates.record(
		&progress("b", TransferDirection::Upload, 2000),
		at(start, 300),
	);
	rates.record(
		&progress("c", TransferDirection::Download, 500),
		at(start, 400),
	);
	assert_eq!(
		rates.current(at(start, 5000)),
		Some(Rates {
			upload: rate(1000, 0, 0),
			download: rate(100, 0, 0),
		})
	);
}

#[test]
fn progress_older_than_the_window_drops_out() {
	let start = Instant::now();
	let mut rates = started(start);
	rates.record(
		&progress("a", TransferDirection::Upload, 10_000),
		at(start, 1000),
	);
	rates.record(
		&SyncEvent::Uploading {
			rel_path: "b".to_owned(),
		},
		at(start, 1500),
	);
	rates.record(
		&progress("a", TransferDirection::Upload, 15_000),
		at(start, 6000),
	);
	assert_eq!(
		rates.current(at(start, 7000)),
		Some(Rates {
			upload: rate(1000, 0, 0),
			download: rate(0, 0, 0),
		})
	);
}

#[test]
fn files_per_second_counts_only_finished_transfers_of_its_own_direction() {
	let start = Instant::now();
	let mut rates = started(start);
	let path = |p: &str| p.to_owned();
	rates.record(
		&progress("in-flight", TransferDirection::Upload, 10),
		at(start, 500),
	);
	rates.record(
		&SyncEvent::Uploading {
			rel_path: path("a"),
		},
		at(start, 1000),
	);
	rates.record(
		&SyncEvent::Uploading {
			rel_path: path("b"),
		},
		at(start, 2000),
	);
	rates.record(
		&SyncEvent::Downloading {
			rel_path: path("c"),
		},
		at(start, 3000),
	);
	rates.record(
		&SyncEvent::CreatingRemoteDir {
			rel_path: path("d"),
		},
		at(start, 3000),
	);
	rates.record(
		&SyncEvent::CreatingLocalDir {
			rel_path: path("e"),
		},
		at(start, 3000),
	);
	rates.record(
		&SyncEvent::ActionFailed {
			rel_path: path("f"),
			error: "failed".to_owned(),
		},
		at(start, 3000),
	);
	assert_eq!(
		rates.current(at(start, 5000)),
		Some(Rates {
			upload: rate(2, 4, 2),
			download: rate(0, 2, 2),
		})
	);
}

#[test]
fn a_failed_transfer_counts_from_zero_when_its_path_moves_again() {
	let start = Instant::now();
	let mut rates = started(start);
	rates.record(
		&progress("a", TransferDirection::Upload, 4000),
		at(start, 1000),
	);
	rates.record(
		&SyncEvent::ActionFailed {
			rel_path: "a".to_owned(),
			error: "failed".to_owned(),
		},
		at(start, 2000),
	);
	rates.record(
		&progress("a", TransferDirection::Upload, 1000),
		at(start, 3000),
	);
	assert_eq!(
		rates.current(at(start, 5000)),
		Some(Rates {
			upload: rate(1000, 0, 0),
			download: rate(0, 0, 0),
		})
	);
}

#[test]
fn the_span_is_the_time_since_the_pass_started_but_at_least_a_second() {
	let start = Instant::now();
	let mut rates = started(start);
	rates.record(
		&progress("a", TransferDirection::Download, 4000),
		at(start, 200),
	);
	assert_eq!(
		rates.current(at(start, 200)),
		Some(Rates {
			upload: rate(0, 0, 0),
			download: rate(4000, 0, 0),
		})
	);
	assert_eq!(
		rates.current(at(start, 2000)),
		Some(Rates {
			upload: rate(0, 0, 0),
			download: rate(2000, 0, 0),
		})
	);
}

#[test]
fn there_are_no_rates_outside_a_pass() {
	let start = Instant::now();
	let mut rates = TransferRates::default();
	assert_eq!(rates.current(start), None);
	rates.record(
		&SyncEvent::PassStarted {
			mode: SyncMode::TwoWay,
		},
		start,
	);
	rates.record(
		&progress("a", TransferDirection::Upload, 4000),
		at(start, 500),
	);
	rates.record(
		&SyncEvent::PassFailed {
			error: "failed".to_owned(),
		},
		at(start, 1000),
	);
	assert_eq!(rates.current(at(start, 1500)), None);
}

#[test]
fn directories_count_on_the_side_they_were_created_unless_creating_them_failed() {
	let start = Instant::now();
	let mut rates = started(start);
	let path = |p: &str| p.to_owned();
	let failed = |p: &str| SyncEvent::ActionFailed {
		rel_path: p.to_owned(),
		error: "failed".to_owned(),
	};
	rates.record(
		&SyncEvent::CreatingRemoteDir {
			rel_path: path("up-1"),
		},
		at(start, 1000),
	);
	rates.record(
		&SyncEvent::CreatingRemoteDir {
			rel_path: path("up-2"),
		},
		at(start, 1100),
	);
	rates.record(&failed("up-2"), at(start, 1200));
	rates.record(
		&SyncEvent::CreatingLocalDir {
			rel_path: path("down-1"),
		},
		at(start, 1300),
	);
	// Another path's failure right after a created directory takes nothing back.
	rates.record(&failed("elsewhere"), at(start, 1400));
	rates.record(
		&SyncEvent::CreatingLocalDir {
			rel_path: path("down-2"),
		},
		at(start, 1500),
	);
	// Nor does a failure of the same path that is not the very next event.
	rates.record(
		&SyncEvent::Uploading {
			rel_path: path("file"),
		},
		at(start, 1600),
	);
	rates.record(&failed("down-2"), at(start, 1700));
	assert_eq!(
		rates.current(at(start, 5000)),
		Some(Rates {
			upload: rate(0, 2, 2),
			download: rate(0, 0, 4),
		})
	);
}

#[tokio::test]
async fn transfers_count_as_they_arrive_while_a_question_holds_the_printing_back() {
	let mut ui = UI::new();
	let mut output = Output {
		status_line: StatusLine {
			term: Term::stdout(),
			state: LineState::Hidden,
			rates: TransferRates::default(),
		},
		printed_failures: HashSet::new(),
		last_status: WatchStatus::default(),
		held: held::test_support::asking("t1", tokio::spawn(async { Ok(false) })),
	};
	for event in [
		SyncEvent::PassStarted {
			mode: SyncMode::TwoWay,
		},
		progress("a", TransferDirection::Upload, 4000),
	] {
		let _ = output.deliver(&mut ui, Update::Event(event));
	}
	let rates = &output.status_line.rates;
	assert!(rates.pass_started.is_some(), "the pass runs from its start");
	let moved: Vec<u64> = rates.upload.moved.iter().map(|&(_, bytes)| bytes).collect();
	assert_eq!(moved, [4000]);
	let (_, deferred) = output.held.answered().await;
	assert!(
		matches!(
			deferred.as_slice(),
			[Update::Event(SyncEvent::PassStarted { .. })]
		),
		"a progress tick prints nothing, so nothing of it is held back"
	);
}

#[tokio::test]
async fn a_failure_a_question_dropped_is_printed_with_its_report_and_the_drop_is_counted() {
	let printed = ui::test_support::Printed::default();
	let mut ui = printed.ui();
	let mut output = Output {
		status_line: StatusLine {
			term: Term::stdout(),
			state: LineState::Disabled,
			rates: TransferRates::default(),
		},
		printed_failures: HashSet::new(),
		last_status: WatchStatus::default(),
		held: held::test_support::asking("t1", tokio::spawn(async { Ok(false) })),
	};
	let mut report = SyncReport::default();
	report.errors.push("b: denied".to_owned());
	let uploads = (0..1000).map(|i| SyncEvent::Uploading {
		rel_path: format!("file {i}"),
	});
	let events = [SyncEvent::PassStarted {
		mode: SyncMode::TwoWay,
	}]
	.into_iter()
	.chain(uploads)
	.chain([
		SyncEvent::ActionFailed {
			rel_path: "b".to_owned(),
			error: "denied".to_owned(),
		},
		SyncEvent::PassCompleted { report },
	]);
	for event in events {
		let _ = output.deliver(&mut ui, Update::Event(event));
	}
	let (_, deferred) = output.held.answered().await;
	let _ = output.replay(&mut ui, deferred);

	let lines = console::strip_ansi_codes(&printed.lines()).into_owned();
	let tail: Vec<&str> = lines.lines().skip(1000).collect();
	assert_eq!(
		tail,
		[
			"upload    file 999",
			"✘ b: denied",
			"⚠ Sync pass completed: 1 errors",
			"1 more update(s) from while the question was open are not printed",
		]
	);
}

#[test]
fn a_pass_lets_go_of_the_failures_it_printed_and_the_next_pass_prints_them_again() {
	let printed = ui::test_support::Printed::default();
	let mut ui = printed.ui();
	let mut printed_failures = HashSet::new();
	let started = SyncEvent::PassStarted {
		mode: SyncMode::TwoWay,
	};
	let mut report = SyncReport::default();
	report.errors.push("a: denied".to_owned());
	let completed = SyncEvent::PassCompleted { report };

	for event in [
		started.clone(),
		SyncEvent::ActionFailed {
			rel_path: "a".to_owned(),
			error: "denied".to_owned(),
		},
	] {
		print_event(&mut ui, &mut printed_failures, &event);
	}
	assert_eq!(
		printed_failures,
		HashSet::from(["a: denied".to_owned()]),
		"a failure printed as it happens is left out of the report"
	);
	print_event(&mut ui, &mut printed_failures, &completed);
	assert_eq!(
		printed_failures.capacity(),
		0,
		"nothing of the pass is kept while the watch waits for the next one"
	);

	// The same failure again, this time only in the report.
	print_event(&mut ui, &mut printed_failures, &started);
	assert_eq!(printed_failures.capacity(), 0);
	print_event(&mut ui, &mut printed_failures, &completed);
	assert_eq!(
		console::strip_ansi_codes(&printed.lines()),
		"Sync pass started\n\
			 ✘ a: denied\n\
			 ⚠ Sync pass completed: 1 errors\n\
			 Sync pass started\n\
			 ✘ a: denied\n\
			 ⚠ Sync pass completed: 1 errors"
	);
}

#[test]
fn rates_render_as_one_line_that_keeps_its_width() {
	let slow = Rates {
		upload: rate(3 * MIB / 2, 42, 5),
		download: rate(0, 0, 0),
	};
	assert_eq!(
		slow.to_string(),
		"↑    1.5 MiB/s,   4.2 files/s,   0.5 dirs/s   ↓      0.0 B/s,   0.0 files/s,   0.0 dirs/s"
	);
	let fast = Rates {
		upload: rate(1023 * 1024 + 900, 999, 120),
		download: rate(85 * MIB, 123, 999),
	};
	assert_eq!(
		fast.to_string(),
		"↑ 1023.9 KiB/s,  99.9 files/s,  12.0 dirs/s   ↓   85.0 MiB/s,  12.3 files/s,  99.9 dirs/s"
	);
}
