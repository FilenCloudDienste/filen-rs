//! Pausing and cancelling an extraction: a paused job holds no memory, slot or drive lock,
//! and changes nothing until resumed; a cancelled one drops its transfers and tells what exists.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_drops_the_transfers_and_reports_what_exists() {
	let tar = tar_of(&[
		("done.txt", b"done"),
		("stuck.bin", &pattern(CHUNK_SIZE, 9)),
	]);
	let setup = setup("c.tar", tar, |backend| {
		backend.blocked_uploads.insert("stuck.bin".to_owned());
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("done.txt is registered and stuck.bin uploading", || {
		finished(&setup).contains_key("c/done.txt")
			&& job
				.reporter
				.read(|state| state.active_names().contains(&"stuck.bin".to_owned()))
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 1,
			files_done: 1,
			bytes_done: 4,
			// the dropped file, at the size the archive states
			files_not_attempted: 1,
			bytes_not_attempted: CHUNK_SIZE as u64,
			..ItemCounts::default()
		}
	);
	let last = job.recorder.last();
	assert_eq!(last.phase, ExtractPhase::Cancelled);
	assert_eq!(last.eta, Some(Duration::ZERO));
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_before_it_starts_takes_no_slot() {
	let config = one_slot();
	let setup_paused = setup("paused.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	let paused = start(
		&setup_paused,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("the job reports itself paused", || {
		paused.reporter.is_paused()
	})
	.await;

	// the only slot is free for a job started later
	let setup_other = setup("other.tar", tar_of(&[("b.txt", b"b")]), |_| {});
	let other = start(
		&setup_other,
		Options {
			config: config.clone(),
			..Options::default()
		},
	);
	tokio::time::timeout(Duration::from_secs(20), other.running)
		.await
		.expect("the unpaused job runs")
		.unwrap()
		.unwrap();
	assert!(setup_paused.backend.log().fetched.is_empty());
	assert!(created_dirs(&setup_paused).is_empty());
	assert!(paused.reporter.is_paused());
	assert_eq!(config.free_slots(), 1);

	pause.send_replace(false);
	let report = paused.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 1);
	assert_eq!(
		paused.recorder.run_states(),
		[RunState::Paused, RunState::Running]
	);
	assert_released(&setup_paused, &paused.reporter, &paused.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_while_queued_leaves_the_slot_to_the_next() {
	let config = one_slot();
	let setup_first = setup("first.tar", tar_of(&[("stuck.bin", b"stuck")]), |backend| {
		backend.blocked_uploads.insert("stuck.bin".to_owned());
	});
	let (_pause, cancel_first, control) = controls();
	let first = start(
		&setup_first,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("the first job holds the slot", || {
		first
			.reporter
			.read(|state| state.active_names() == ["stuck.bin"])
	})
	.await;

	let setup_queued = setup("queued.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (pause, _cancel, control) = controls();
	let queued = start(
		&setup_queued,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	pause.send_replace(true);
	wait_until("the queued job reports itself paused", || {
		queued.reporter.is_paused()
	})
	.await;
	cancel_first.send_replace(true);
	first.running.await.unwrap().unwrap_err();

	let setup_next = setup("next.tar", tar_of(&[("b.txt", b"b")]), |_| {});
	let next = start(
		&setup_next,
		Options {
			config: config.clone(),
			..Options::default()
		},
	);
	tokio::time::timeout(Duration::from_secs(20), next.running)
		.await
		.expect("the slot the paused job waited for is free")
		.unwrap()
		.unwrap();
	assert!(setup_queued.backend.log().fetched.is_empty());

	pause.send_replace(false);
	let report = queued.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 1);
	assert_released(&setup_queued, &queued.reporter, &queued.recorder);
	assert_eq!(config.free_slots(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_a_registration_waits_for_the_lock_holds_nothing() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		// another client holds the drive lock from the registration on (the folder's create
		// is the first acquisition)
		backend.block_locks_from.send_replace(Some(1));
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("a.txt's registration waits for the lock", || {
		setup.backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job);
	assert!(finished(&setup).is_empty());

	setup.backend.block_locks_from.send_replace(None);
	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(report.counts.files_done, 1);
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Running
		]
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_pause_while_the_archive_opens_holds_nothing() {
	// larger than the floor, so chunks are prefetched on the client's budget
	let setup = setup("bundle.tar", pattern(3 * CHUNK_SIZE, 2), |backend| {
		// the destination's shares take long to fetch, so the pause comes while opening
		backend.targets_delay = Duration::from_secs(30);
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	// chunks are prefetched (their memory taken) before the event is, and wait meanwhile
	wait_until("the job opens the archive", || {
		setup.backend.log().target_fetches == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job);
	assert!(created_dirs(&setup).is_empty(), "the folder waits too");

	pause.send_replace(false);
	for event in [
		file_entry(0, "a.txt", 1),
		WorkerEvent::Data(b"a".to_vec()),
		WorkerEvent::FileEnd,
	] {
		events.send(event).await.unwrap();
	}
	drop(events);
	let _ = result.send(read_in_full());
	job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(
		job.recorder.run_states(),
		[RunState::Running, RunState::Paused, RunState::Running],
		"the pause is taken up once the archive is opened, with nothing left in flight"
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

/// Operations in flight while directories are created as many at once as they can be: those
/// creates, and the archive's one chunk, which a scripted codec never asks for.
const CREATING_WITH_THE_ARCHIVE: u64 = MAX_SMALL_PARALLEL_REQUESTS as u64 + 1;

#[tokio::test(start_paused = true)]
async fn a_pause_leaves_no_directory_uncreated() {
	// more directories than are created at once, the rest waiting their turn
	let count = MAX_SMALL_PARALLEL_REQUESTS + 10;
	let names: Vec<String> = (0..count).map(|ordinal| format!("d{ordinal:03}")).collect();
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.hold_named(Request::Create, &names);
	});
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	for (ordinal, name) in names.iter().enumerate() {
		events.send(dir_entry(ordinal as u64, name)).await.unwrap();
	}
	wait_until(
		"every directory is planned, the first being created",
		|| {
			events.capacity() == events.max_capacity()
				&& setup.backend.log().held_named.len() == MAX_SMALL_PARALLEL_REQUESTS
		},
	)
	.await;
	drop(events);
	let _ = result.send(read_in_full());
	// time only moves once every task waits: a moment of it lets the driver take the archive's
	// end, with nothing else it could do while the creates are held
	sleep(Duration::from_millis(1)).await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert!(!job.running.is_finished(), "the rest are created on resume");

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.dirs_created, count as u64 + 1);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_leaves_the_directories_not_created_yet_not_attempted() {
	let count = MAX_SMALL_PARALLEL_REQUESTS + 10;
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.delay = Duration::from_secs(10);
	});
	let (_pause, cancel, control) = controls();
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	for ordinal in 0..count {
		events
			.send(dir_entry(ordinal as u64, &format!("d{ordinal:03}")))
			.await
			.unwrap();
	}
	wait_until(
		"every directory is planned, the first being created",
		|| {
			events.capacity() == events.max_capacity()
				&& job.reporter.ops_in_flight() == CREATING_WITH_THE_ARCHIVE
		},
	)
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	drop((events, result));
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			// the new folder, and the creates in flight, which finish
			dirs_created: MAX_SMALL_PARALLEL_REQUESTS as u64 + 1,
			dirs_not_attempted: 10,
			..ItemCounts::default()
		}
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_mid_extraction_holds_nothing_and_changes_nothing() {
	let files: Vec<(String, Vec<u8>)> = (0..3)
		.map(|i| {
			(
				format!("f{i}.bin"),
				pattern(2 * CHUNK_SIZE + i, u8::try_from(i).unwrap()),
			)
		})
		.collect();
	let members: Vec<(&str, &[u8])> = files
		.iter()
		.map(|(name, data)| (name.as_str(), &data[..]))
		.collect();
	let setup = setup("bundle.tar", tar_of(&members), |backend| {
		backend.hold_named(Request::Upload, ["f1.bin"]);
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("f1.bin's first chunk uploads", || {
		!setup.backend.log().held_named.is_empty()
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job);

	pause.send_replace(false);
	job.running.await.unwrap().unwrap();
	let expected: BTreeMap<String, (u64, u64, Blake3Hash)> = files
		.iter()
		.map(|(name, data)| {
			(
				format!("bundle/{name}"),
				(
					data.len() as u64,
					data.len().div_ceil(CHUNK_SIZE) as u64,
					hash(data),
				),
			)
		})
		.collect();
	assert_eq!(finished(&setup), expected);
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Running
		]
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_paused_winds_down() {
	let setup = setup(
		"bundle.tar",
		tar_of(&[("a.bin", &pattern(3 * CHUNK_SIZE, 4))]),
		|backend| backend.hold_named(Request::Upload, ["a.bin"]),
	);
	let (pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("a chunk uploads", || {
		!setup.backend.log().held_named.is_empty()
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(finished(&setup).is_empty());
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Cancelling
		]
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Cancelled);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_pause_while_finishing_gives_back_the_input_and_the_lock() {
	// larger than the floor, and never asked for in full by the codec: chunks prefetched past
	// its last read, as a zip's index chunks fetched again for its entries
	let setup = setup("bundle.tar", pattern(3 * CHUNK_SIZE, 3), |backend| {
		// the destination is shared while the job runs: every item is propagated again
		backend.later_targets = Some(crate::connect::ConnectedTargets::with_test_users(1));
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			root: ExtractRoot::Destination,
			control,
			config: config.clone(),
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	// more top-level items than are propagated at once
	let count = MAX_SMALL_PARALLEL_REQUESTS + 6;
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	for ordinal in 0..count {
		for event in [
			file_entry(ordinal as u64, &format!("f{ordinal:03}"), 1),
			WorkerEvent::Data(vec![u8::try_from(ordinal).unwrap()]),
			WorkerEvent::FileEnd,
		] {
			events.send(event).await.unwrap();
		}
	}
	wait_until("every file is registered", || {
		setup.backend.log().finished.len() == count
	})
	.await;
	// the first batch propagated waits
	let registered: Vec<Uuid> = setup.backend.log().finished.keys().copied().collect();
	setup.backend.hold_requests(Request::Propagate, registered);
	drop(events);
	let _ = result.send(read_in_full());
	wait_until("the first batch propagates", || {
		setup.backend.log().held.len() == MAX_SMALL_PARALLEL_REQUESTS
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Finishing);
	assert_eq!(
		setup.backend.log().propagated_trees.len(),
		MAX_SMALL_PARALLEL_REQUESTS,
		"the batch in flight finished, then the lock was given back"
	);

	pause.send_replace(false);
	job.running.await.unwrap().unwrap();
	assert_eq!(setup.backend.log().propagated_trees.len(), count);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_lifted_before_the_job_went_idle_loses_no_file() {
	// b.txt's registration runs long; a.bin's upload ends while the pause is requested, and the
	// pause is lifted while b.txt still registers, so the job never goes idle
	let big = pattern(CHUNK_SIZE + 3, 5);
	let tar = tar_of(&[("b.txt", b"b"), ("a.bin", &big[..])]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.hold_named(Request::Finish, ["b.txt"]);
		backend.hold_named(Request::Upload, ["a.bin"]);
	});
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("b.txt registers and a.bin's chunks upload", || {
		setup.backend.log().held_named.len() == 3
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_named(Request::Upload, ["a.bin"]);
	wait_until("a.bin's chunks are uploaded", || {
		setup.backend.log().uploaded.len() == 3
	})
	.await;
	assert!(
		!finished(&setup).contains_key("bundle/b.txt"),
		"b.txt still registers: the job has not gone idle"
	);
	pause.send_replace(false);
	setup.backend.release_all();
	let report = tokio::time::timeout(Duration::from_secs(20), job.running)
		.await
		.expect("the job finishes")
		.unwrap()
		.unwrap();
	assert_eq!(report.counts.files_done, 2);
	assert_eq!(finished(&setup).len(), 2);
	assert_eq!(
		job.recorder.run_states(),
		[RunState::Running, RunState::Pausing, RunState::Running],
		"never paused: a registration was in flight all along"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_the_archive_is_read_ends_with_no_time_left() {
	// read in part, and no further until the cancel: the rate is known, and time is left
	let tar = tar_of(&[("big.bin", &incompressible(6 * CHUNK_SIZE, 0x40))]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.hold_named(Request::Upload, ["big.bin"]);
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("the rate is known", || {
		job.recorder
			.updates
			.lock()
			.unwrap()
			.last()
			.is_some_and(|update| update.eta.is_some_and(|eta| !eta.is_zero()))
	})
	.await;
	cancel.send_replace(true);
	job.running.await.unwrap().unwrap_err();
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.eta),
		(ExtractPhase::Cancelled, Some(Duration::ZERO))
	);
}
