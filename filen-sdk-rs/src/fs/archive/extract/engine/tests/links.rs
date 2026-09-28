//! A tar's hard links: each extracted as a copy of the file it names, whenever that file is
//! registered; skipped when it names none; within the expansion limit; paused and cancelled as any
//! transfer.

use super::*;

/// Appends a tar hard link at `path` to the member at `target`.
/// A hard link at `path` to the file at `target`.
fn hard_link<'a>(path: &'a str, target: &'a str) -> TarMember<'a> {
	TarMember::HardLink { path, target }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_is_extracted_as_a_copy_of_the_file_it_names() {
	let big = incompressible(2 * CHUNK_SIZE + 77, 0x51);
	let tar = tar_with(&[
		TarMember::Data("docs/", b""),
		TarMember::Data("docs/a.bin", &big),
		TarMember::Data("empty", b""),
		hard_link("docs/hard", "docs/a.bin"),
		hard_link("top.bin", "docs/a.bin"),
		hard_link("empty-link", "empty"),
		// nothing to copy: a path no file came at, and a directory's
		hard_link("gone", "missing.txt"),
		hard_link("dir-link", "docs"),
	]);
	let setup = setup("bundle.tar", tar, |backend| backend.keep_uploads = true);
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	let files = finished(&setup);
	let copied = files["bundle/docs/a.bin"];
	assert_eq!(copied.0, big.len() as u64);
	assert_eq!(copied.2, hash(&big));
	assert_eq!(files["bundle/docs/hard"], copied);
	assert_eq!(files["bundle/top.bin"], copied);
	assert_eq!(files["bundle/empty-link"], files["bundle/empty"]);
	assert_eq!(files.len(), 5);
	assert_eq!(
		report
			.skipped
			.iter()
			.map(|skipped| (skipped.path.as_str(), &skipped.reason))
			.collect::<Vec<_>>(),
		[
			(
				"gone",
				&ExtractSkipReason::Hardlink {
					target: "missing.txt".into()
				}
			),
			(
				"dir-link",
				&ExtractSkipReason::Hardlink {
					target: "docs".into()
				}
			),
		]
	);
	assert_eq!(report.counts.files_done, 5);
	assert_released(&setup, &job.reporter, &job.recorder);
}

/// A hard link at `path` to the file sent at `target`, as the codec sends it.
fn link_entry(ordinal: u64, path: &str, target: &str) -> WorkerEvent {
	let (shown, truncated) = display_path(path);
	WorkerEvent::Link(Box::new(LinkHead {
		ordinal,
		path: entry_path(path).unwrap(),
		modified: None,
		target: entry_path(target).unwrap(),
		unresolved: SkippedMember {
			ordinal,
			path: shown.to_owned(),
			path_truncated: truncated,
			bytes: 0,
			reason: ExtractSkipReason::Hardlink {
				target: target.to_owned(),
			},
		},
	}))
}

/// Runs a scripted tar codec over `setup` that sends the file `a.txt` (with `a`), then, once
/// `before_link` holds, a hard link `b.txt` to it, letting every held request go on once the
/// link is taken; the report.
async fn link_after(
	setup: &Setup,
	before_link: impl Fn(&Setup) -> bool,
) -> Result<ExtractReport, ExtractFailed> {
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(setup, Options::default(), Box::new(move || Ok(link)));
	for event in [
		WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }),
		file_entry(0, "a.txt", 1),
		WorkerEvent::Data(b"a".to_vec()),
		WorkerEvent::FileEnd,
	] {
		events.send(event).await.unwrap();
	}
	wait_until("the link's turn", || before_link(setup)).await;
	events.send(link_entry(1, "b.txt", "a.txt")).await.unwrap();
	wait_until("the link is taken", || {
		events.capacity() == events.max_capacity()
	})
	.await;
	setup.backend.release_all();
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap();
	assert_released(setup, &job.reporter, &job.recorder);
	report
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_copies_its_target_whenever_that_is_registered() {
	// the file is registered before the link comes
	let setup = setup("bundle.tar", Vec::new(), |backend| {
		backend.keep_uploads = true
	});
	let report = link_after(&setup, |setup| !setup.backend.log().finished.is_empty())
		.await
		.unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt", "bundle/b.txt"]);
	assert_eq!(report.counts.files_done, 2);
	let files = finished(&setup);
	assert_eq!(files["bundle/a.txt"], files["bundle/b.txt"]);

	// the link comes while the file is being registered, and waits for it
	let setup = setup_registering();
	let report = link_after(&setup, |setup| !setup.backend.log().held_named.is_empty())
		.await
		.unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt", "bundle/b.txt"]);
	assert_eq!(report.counts.files_done, 2);
}

/// A drive where `a.txt`'s registration is held.
fn setup_registering() -> Setup {
	setup("bundle.tar", Vec::new(), |backend| {
		backend.keep_uploads = true;
		backend.hold_named(Request::Finish, ["a.txt"]);
	})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_to_a_file_that_failed_is_skipped() {
	let skipped_link = |report: &ExtractReport| {
		report
			.skipped
			.iter()
			.map(|skipped| (skipped.path.clone(), skipped.reason.clone()))
			.collect::<Vec<_>>()
			== [(
				"b.txt".to_owned(),
				ExtractSkipReason::Hardlink {
					target: "a.txt".into(),
				},
			)]
	};
	// failed before the link comes
	let failed = setup("bundle.tar", Vec::new(), |backend| {
		backend
			.fail_upload
			.insert("a.txt".to_owned(), ErrorKind::Server);
	});
	let report = link_after(&failed, |setup| {
		setup.backend.log().upload_starts.len() == 1
	})
	.await
	.unwrap();
	assert!(skipped_link(&report), "{:?}", report.skipped);
	assert!(finished_paths(&failed).is_empty());

	// failing to register while the link waits for it
	let unregistered = setup("bundle.tar", Vec::new(), |backend| {
		backend.hold_named(Request::Finish, ["a.txt"]);
		backend
			.fail_finish
			.insert("a.txt".to_owned(), ErrorKind::Server);
	});
	let report = link_after(&unregistered, |setup| {
		!setup.backend.log().held_named.is_empty()
	})
	.await
	.unwrap();
	assert!(skipped_link(&report), "{:?}", report.skipped);
	assert_eq!(report.counts.files_failed, 1);
}

/// A bare tar of the file `a.bin` holding `data`, then `links` hard links to it.
fn tar_with_links(data: &[u8], links: usize) -> Vec<u8> {
	let paths: Vec<String> = (0..links).map(|link| format!("l{link:03}")).collect();
	let members: Vec<TarMember> = [TarMember::Data("a.bin", data)]
		.into_iter()
		.chain(paths.iter().map(|path| hard_link(path, "a.bin")))
		.collect();
	tar_with(&members)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_links_copy_no_more_than_the_expansion_limit_allows() {
	// 1 MiB and 400 links to it: 400 MiB out of a 1.2 MB tar
	let data = pattern(1 << 20, 3);
	let tar = tar_with_links(&data, 400);
	let limit = ExpansionLimit {
		ratio: 10,
		floor: 4 << 20,
	};
	let bomb = setup("bomb.tar", tar.clone(), |_| {});
	let job = start(
		&bomb,
		Options {
			expansion: Some(limit),
			..Options::default()
		},
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
	// what the limit allows, of 12 MiB: the file and 11 copies
	let uploaded: u64 = finished(&bomb).values().map(|(size, ..)| size).sum();
	assert!(uploaded <= 12 << 20, "{uploaded} bytes uploaded");
	assert_released(&bomb, &job.reporter, &job.recorder);

	// with no limit, every link is copied
	let unlimited = setup("links.tar", tar_with_links(&data[..10], 400), |_| {});
	let job = start(
		&unlimited,
		Options {
			expansion: None,
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn many_links_to_one_file_open_as_fast_as_they_are_worked_off() {
	let setup = setup("links.tar", tar_with_links(b"linked", 200), |backend| {
		backend.keep_uploads = true;
		backend.hold_named(Request::Finish, ["a.bin"]);
	});
	let job = start(&setup, Options::default());
	// every link waits for the file first: the codec has read them all, and given back the
	// chunk it read them from
	wait_until("every link waits for a.bin", || {
		!setup.backend.log().held_named.is_empty()
			&& setup.backend.memory.available_permits() == setup.backend.budget
	})
	.await;
	setup.backend.release_all();
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 201);
	let log = setup.backend.log();
	assert_eq!(log.fetched_items.len(), 200);
	assert!(
		log.peak_item_fetches <= MAX_SMALL_PARALLEL_REQUESTS,
		"{} fetched at once",
		log.peak_item_fetches
	);
	drop(log);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_links_name_is_reported_as_any_entrys() {
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		hard_link("c\u{202E}txt.exe", "a.txt"),
		hard_link("d:e", "a.txt"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.keep_uploads = true;
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(
		report
			.misleading_names
			.iter()
			.map(|name| name.path.as_str())
			.collect::<Vec<_>>(),
		["c\u{202E}txt.exe"]
	);
	assert_eq!(
		renames(&report)
			.into_iter()
			.map(|(_, _, reason)| reason)
			.collect::<Vec<_>>(),
		[ExtractRenameReason::PathRewritten]
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn links_to_links_and_to_themselves() {
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		hard_link("b", "a.txt"),
		// a link to a link copies what that one copies
		hard_link("c", "b"),
		// a link to itself names nothing before it
		hard_link("self", "self"),
		// a link to a link to nothing
		hard_link("d", "self"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.keep_uploads = true;
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	let files = finished(&setup);
	assert_eq!(
		files.keys().collect::<Vec<_>>(),
		["bundle/a.txt", "bundle/b", "bundle/c"]
	);
	assert_eq!(files["bundle/c"], files["bundle/a.txt"]);
	assert_eq!(
		report
			.skipped
			.iter()
			.map(|skipped| skipped.path.as_str())
			.collect::<Vec<_>>(),
		["self", "d"]
	);
	// the skipped links are no items
	assert_eq!(report.counts.files_done, 3);
	assert_released(&setup, &job.reporter, &job.recorder);

	// a listing tells the same of each link
	let listing = list(&setup, JobControl::default(), test_config());
	let listed = listing.running.await.unwrap().unwrap();
	assert_eq!(
		listed
			.entries
			.iter()
			.map(|entry| (entry.stored_path.as_str(), entry.size, entry.skip.is_some()))
			.collect::<Vec<_>>(),
		[
			("a.txt", Some(5), false),
			("b", Some(5), false),
			("c", Some(5), false),
			("self", Some(0), true),
			("d", Some(0), true),
		]
	);
	assert_eq!(listed.totals.files, report.counts.files_done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chosen_hard_link_comes_out_with_the_target_its_listing_names() {
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("other.txt", b"other"),
		hard_link("b", "a.txt"),
		hard_link("c", "b"),
	]);
	let listing = list(
		&setup("bundle.tar", tar.clone(), |_| {}),
		JobControl::default(),
		test_config(),
	);
	let listed = listing.running.await.unwrap().unwrap();
	let target_of = |index: usize| match &listed.entries[index].kind {
		ArchiveEntryKind::Hardlink { target_id, .. } => target_id.map(|id| id.index),
		other => panic!("{other:?} is no hard link"),
	};
	// each names the entry it copies, a link naming a link
	assert_eq!((target_of(2), target_of(3)), (Some(0), Some(2)));

	// alone, a link has nothing to copy; with the entries its listing names, it is extracted
	for (chosen_ids, extracted, skipped) in [
		(&[3][..], &[][..], &["c"][..]),
		(&[3, 2, 0], &["a.txt", "b", "c"], &[]),
	] {
		let setup = setup("bundle.tar", tar.clone(), |_| {});
		let job = start(&setup, chosen(chosen_ids, &[]));
		let report = job.running.await.unwrap().unwrap();
		assert_eq!(finished_paths(&setup), extracted, "{chosen_ids:?}");
		assert_eq!(
			report
				.skipped
				.iter()
				.map(|skipped| skipped.path.as_str())
				.collect::<Vec<_>>(),
			skipped,
			"{chosen_ids:?}"
		);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_that_waited_for_a_file_that_failed_is_no_item() {
	let setup = setup("bundle.tar", Vec::new(), |backend| {
		backend.hold_named(Request::Finish, ["a.txt"]);
		backend
			.fail_finish
			.insert("a.txt".to_owned(), ErrorKind::Server);
	});
	let (events, result, link) = worker::scripted::<CodecResult>();
	// a.txt, and the two files after the link: the link counts only while it waits
	let job = start_with(
		&setup,
		Options {
			max_items: Some(3),
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	for event in [
		WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }),
		file_entry(0, "a.txt", 1),
		WorkerEvent::Data(b"a".to_vec()),
		WorkerEvent::FileEnd,
		link_entry(1, "b.txt", "a.txt"),
	] {
		events.send(event).await.unwrap();
	}
	// the link waits for a.txt, whose registration fails only then
	wait_until("the link is taken", || {
		events.capacity() == events.max_capacity()
	})
	.await;
	setup.backend.release_all();
	wait_until("a.txt failed", || job.reporter.counts().files_failed == 1).await;
	for (ordinal, name) in [(2, "c.txt"), (3, "d.txt")] {
		for event in [
			file_entry(ordinal, name, 1),
			WorkerEvent::Data(b"x".to_vec()),
			WorkerEvent::FileEnd,
		] {
			events.send(event).await.unwrap();
		}
	}
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/c.txt", "bundle/d.txt"]);
	assert_eq!(report.counts.entries_skipped, 1);
	assert_released(&setup, &job.reporter, &job.recorder);
}

/// A job whose scripted codec sends the file `a.txt` and, once it is registered and the fetches
/// of it held, a hard link `b.txt` to it; returned once the link's copy waits on its fetch.
async fn a_link_copying(control: JobControl) -> (Setup, Job) {
	let setup = setup("bundle.tar", Vec::new(), |backend| {
		backend.keep_uploads = true
	});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	for event in [
		WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }),
		file_entry(0, "a.txt", 1),
		WorkerEvent::Data(b"a".to_vec()),
		WorkerEvent::FileEnd,
	] {
		events.send(event).await.unwrap();
	}
	wait_until("a.txt is registered", || {
		!setup.backend.log().finished.is_empty()
	})
	.await;
	let target: Vec<Uuid> = setup.backend.log().finished.keys().copied().collect();
	setup.backend.hold_requests(Request::Fetch, target);
	events.send(link_entry(1, "b.txt", "a.txt")).await.unwrap();
	drop(events);
	let _ = result.send(read_in_full());
	wait_until("the copy is fetched", || {
		!setup.backend.log().held.is_empty()
	})
	.await;
	(setup, job)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_copy_is_paused_as_any_transfer() {
	let (pause, _cancel, control) = controls();
	let (setup, job) = a_link_copying(control).await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job, &test_config());
	pause.send_replace(false);
	job.running.await.unwrap().unwrap();
	let files = finished(&setup);
	assert_eq!(files["bundle/b.txt"], files["bundle/a.txt"]);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_copy_is_dropped_by_a_cancel() {
	let (_pause, cancel, control) = controls();
	let (setup, job) = a_link_copying(control).await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(
		(
			failed.report.counts.files_done,
			failed.report.counts.files_not_attempted
		),
		(1, 1)
	);
	setup.backend.release_all();
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_hard_link_has_no_retry() {
	let data = pattern(1000, 7);
	let setup = setup("bundle.tar", tar_with_links(&data, 1), |backend| {
		backend
			.fail_upload
			.insert("l000".to_owned(), ErrorKind::Server);
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.bin"]);
	let [failure] = report.failures.as_slice() else {
		panic!("{:?}", report.failures);
	};
	// a request for the link alone would have nothing to copy: the link's file is in the drive
	assert_eq!(
		(failure.path.as_str(), failure.retry.is_none()),
		("l000", true)
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_with_hundreds_of_links_is_copied_for_each_within_bounds() {
	const LINKS: usize = 300;
	let data = incompressible(4096, 0x0F);
	let setup = setup("fan.tar", tar_with_links(&data, LINKS), |backend| {
		backend.keep_uploads = true;
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	let files = finished(&setup);
	assert_eq!(files.len(), LINKS + 1);
	let expected = (data.len() as u64, 1, hash(&data));
	assert!(
		files.values().all(|file| *file == expected),
		"every copy holds the file's data"
	);
	assert_eq!(
		report.counts,
		ItemCounts {
			dirs_created: 1,
			files_done: LINKS as u64 + 1,
			bytes_done: (LINKS as u64 + 1) * data.len() as u64,
			..ItemCounts::default()
		}
	);
	let log = setup.backend.log();
	assert_eq!(log.fetched_items.len(), LINKS);
	assert!(
		log.peak_item_fetches <= MAX_SMALL_PARALLEL_REQUESTS,
		"{} targets fetched at once",
		log.peak_item_fetches
	);
	assert!(
		log.peak_finishes <= MAX_SMALL_PARALLEL_REQUESTS,
		"{} registered at once",
		log.peak_finishes
	);
	drop(log);
	// every memory reservation given back, and none reported paused holding one
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn links_past_a_tight_limit_end_the_job_with_what_they_created() {
	const LINKS: usize = 300;
	let data = incompressible(64 << 10, 0x1F);
	let setup = setup("fan.tar", tar_with_links(&data, LINKS), |backend| {
		backend.keep_uploads = true;
	});
	// the archive is far smaller: the floor bounds the copies, sixteen of the file's size
	let limit = ExpansionLimit {
		ratio: 1,
		floor: 16 * data.len() as u64,
	};
	let job = start(
		&setup,
		Options {
			expansion: Some(limit),
			..Options::default()
		},
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);

	let files = finished(&setup);
	let expected = (data.len() as u64, 1, hash(&data));
	assert!(files.values().all(|file| *file == expected));
	assert!(files.len() <= 17, "{} files", files.len());
	let counts = failed.report.counts;
	// what the report says was created is what was, and the file and the sixteen links taken on
	// are each done, failed or not attempted
	assert_eq!(counts.files_done, files.len() as u64);
	assert_eq!(counts.bytes_done, files.len() as u64 * data.len() as u64);
	assert_eq!(
		counts.files_done + counts.files_failed + counts.files_not_attempted,
		17
	);
	assert_eq!(
		counts.bytes_done + counts.bytes_failed + counts.bytes_not_attempted,
		17 * data.len() as u64
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Failed);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_to_mac_metadata_is_left_out_as_metadata() {
	let apple_double = [&[0x00, 0x05, 0x16, 0x07][..], b"\x00\x02\x00\x00"].concat();
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("__MACOSX/._a.txt", &apple_double),
		// a copy of metadata left out is left out; a copy of a file of the user's is extracted,
		// in a __MACOSX folder or not
		hard_link("__MACOSX/._b.txt", "__MACOSX/._a.txt"),
		hard_link("__MACOSX/copy.txt", "a.txt"),
	]);
	let setup = setup("bundle.tar", tar, |_| {});
	let report = start(&setup, Options::default())
		.running
		.await
		.unwrap()
		.unwrap();
	assert_eq!(
		finished_paths(&setup),
		["bundle/__MACOSX/copy.txt", "bundle/a.txt"]
	);
	assert_eq!(
		report
			.skipped
			.iter()
			.map(|skipped| (skipped.path.as_str(), &skipped.reason))
			.collect::<Vec<_>>(),
		[
			("__MACOSX/._a.txt", &ExtractSkipReason::MacMetadata),
			("__MACOSX/._b.txt", &ExtractSkipReason::MacMetadata),
		]
	);
}
