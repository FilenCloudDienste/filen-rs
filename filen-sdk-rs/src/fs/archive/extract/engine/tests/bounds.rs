//! What an extraction keeps in flight, and how much: directories planned, files open, registrations,
//! top-level batches and memory held, each bounded however the archive is shaped; and a codec that
//! falls silent given up on.

use super::*;

#[tokio::test(start_paused = true)]
async fn directories_are_planned_only_as_fast_as_they_are_created() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.hold_named(Request::Create, ["blocked"]);
	});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	let below = MAX_UNCREATED_DIRS + 10;
	let entry = |ordinal: usize| match ordinal {
		0 => dir_entry(0, "blocked"),
		_ => dir_entry(ordinal as u64, &format!("blocked/d{ordinal:04}")),
	};
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	// longer than a silent codec is given, which a waiting one must not be taken for
	let wait = 2 * ARCHIVE_STALL_TIMEOUT;
	let mut taken = None;
	for ordinal in 0..=below {
		if tokio::time::timeout(wait, events.send(entry(ordinal)))
			.await
			.is_err()
		{
			taken.get_or_insert(ordinal);
			// once `blocked` is created, the rest are taken as they are created
			setup.backend.release_all();
			events.send(entry(ordinal)).await.unwrap();
		}
	}
	// `blocked` and the directories below it up to the backlog, and one more in the channel
	assert_eq!(
		taken,
		Some(MAX_UNCREATED_DIRS + 1),
		"the codec waits for the directories"
	);
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.dirs_created, below as u64 + 2);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registrations_run_bounded() {
	let names: Vec<String> = (0..3 * MAX_SMALL_PARALLEL_REQUESTS)
		.map(|i| format!("e{i:03}"))
		.collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let setup = setup("empty.tar", tar_of(&members), |backend| {
		backend.hold_named(Request::Finish, &names);
	});
	let job = start(&setup, Options::default());
	// each registration let go in turn: the next one starts in its place
	for (done, name) in names.iter().enumerate() {
		let started = (done + MAX_SMALL_PARALLEL_REQUESTS).min(names.len());
		wait_until("the registrations after it start", || {
			setup.backend.log().held_named.len() == started
		})
		.await;
		assert_eq!(
			setup.backend.log().peak_finishes,
			MAX_SMALL_PARALLEL_REQUESTS,
			"as many at once as other small requests, and no more"
		);
		setup.backend.release_named(Request::Finish, [name]);
	}
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, names.len() as u64);
	let started: Vec<String> = setup
		.backend
		.log()
		.held_named
		.iter()
		.map(|(_, name)| name.clone())
		.collect();
	assert_eq!(started, names, "registered in archive order");
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn files_are_read_only_as_fast_as_they_are_registered() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		// another client holds the drive lock from the first registration on (the folder's
		// create is the first acquisition)
		backend.block_locks_from.send_replace(Some(1));
	});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	let count = MAX_OPEN_ENTRIES + 10;
	let mut taken = None;
	for ordinal in 0..count {
		let entry = || file_entry(ordinal as u64, &format!("e{ordinal:03}"), 0);
		// longer than a silent codec is given, which a waiting one must not be taken for
		if tokio::time::timeout(2 * ARCHIVE_STALL_TIMEOUT, events.send(entry()))
			.await
			.is_err()
		{
			taken.get_or_insert(ordinal);
			setup.backend.block_locks_from.send_replace(None);
			events.send(entry()).await.unwrap();
		}
		events.send(WorkerEvent::FileEnd).await.unwrap();
	}
	// the last file taken ended in the channel, not yet taken either
	assert_eq!(
		taken,
		Some(MAX_OPEN_ENTRIES),
		"the codec waits for the files to be registered"
	);
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, count as u64);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_silent_codec_is_given_up_on() {
	let setup = setup("silent.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWorkerDied);
	assert_released(&setup, &job.reporter, &job.recorder);
	// the codec's ends were held open all along
	drop((events, result));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn top_level_items_arrive_in_bounded_batches() {
	let names: Vec<String> = (0..300).map(|i| format!("f{i:03}.txt")).collect();
	let members: Vec<(&str, &[u8])> = names
		.iter()
		.map(|name| (name.as_str(), &b"x"[..]))
		.collect();
	let setup = setup("bundle.tar", tar_of(&members), |_| {});
	let options = Options {
		root: ExtractRoot::Destination,
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 300);
	// every item reached the callback before the job returned, none in a batch over 256
	assert_eq!(job.recorder.top_level.lock().unwrap().len(), 300);
	let batches = job.recorder.batches.lock().unwrap().clone();
	assert!(
		batches.iter().all(|&n| (1..=256).contains(&n)),
		"{batches:?}"
	);
	assert!(
		batches.len() < 300,
		"items are batched, not sent one by one: {batches:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_archive_read_to_its_end_holds_no_memory() {
	let big = pattern(3 * CHUNK_SIZE, 8);
	let setup = setup("bundle.tar", tar_of(&[("big.bin", &big)]), |backend| {
		backend.hold_named(Request::Finish, ["big.bin"]);
	});
	let job = start(&setup, Options::default());
	wait_until("big.bin registers, the archive read", || {
		!setup.backend.log().held_named.is_empty()
	})
	.await;
	// while it still registers
	wait_until("the codec's last chunk is given back", || {
		setup.backend.memory.available_permits() == setup.backend.budget
	})
	.await;
	setup.backend.release_all();
	job.running.await.unwrap().unwrap();
	assert_released(&setup, &job.reporter, &job.recorder);
}
