//! The failures an extraction records and the errors that end it: a directory with its subtree, a
//! file, a damaged archive, a limit; and where a failed entry is extracted again.

use super::*;
use crate::fs::archive::format::TAR_CHECKSUM;

#[tokio::test(start_paused = true)]
async fn a_failed_codec_gives_back_its_own_error() {
	let setup = setup("damaged.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	drop(events);
	result
		.send(Err(Error::custom_with_source(
			ErrorKind::ArchiveCorrupt,
			std::io::Error::other("damaged"),
			None::<&str>,
		)))
		.unwrap();
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveCorrupt);
	assert!(
		failed.error.downcast_ref::<std::io::Error>().is_some(),
		"the codec's error reaches the caller whole, source and all"
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn the_sizes_of_skipped_entries_add_up_without_overflowing() {
	let setup = setup("bundle.zip", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	// sizes a zip64 entry may state, which together overflow a u64
	for ordinal in 0..2 {
		events
			.send(WorkerEvent::Skipped(SkippedMember {
				ordinal,
				path: format!("huge{ordinal}.bin"),
				path_truncated: false,
				bytes: u64::MAX / 2 + 1,
				reason: ExtractSkipReason::UnsupportedMethod,
			}))
			.await
			.unwrap();
	}
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(
		(report.counts.entries_skipped, report.counts.bytes_skipped),
		(2, u64::MAX)
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

/// The failure events the updates carried: whether of a directory, path, stage, error kind.
fn failure_events(recorder: &Recorder) -> Vec<(bool, String, ExtractStage, ErrorKind)> {
	let mut events: Vec<_> = recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.flat_map(|update| &update.events)
		.filter_map(|event| match event {
			ExtractEvent::DirFailed(f) => Some((true, f.path.clone(), f.stage, f.error.kind())),
			ExtractEvent::FileFailed(f) => Some((false, f.path.clone(), f.stage, f.error.kind())),
			_ => None,
		})
		.collect();
	events.sort_by(|a, b| a.1.cmp(&b.1));
	events
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_directory_that_fails_takes_its_subtree_and_nothing_else() {
	let tar = tar_of(&[
		("a/b/c/x.txt", b"x below"),
		("a/b/y.txt", b"y"),
		("a/z.txt", b"z beside"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.fail_create.insert("b".into(), ErrorKind::Server);
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(finished_paths(&setup), ["bundle/a/z.txt"]);
	assert_eq!(created_dirs(&setup), ["bundle", "a"]);
	let log = setup.backend.log();
	assert!(
		log.upload_starts
			.iter()
			.all(|(uuid, _)| log.finished.contains_key(uuid)),
		"nothing below the failed directory is uploaded"
	);
	drop(log);
	assert_eq!(
		report.counts,
		ItemCounts {
			dirs_created: 2,
			// b, and c below it, never attempted
			dirs_failed: 2,
			files_done: 1,
			files_failed: 2,
			bytes_done: 8,
			bytes_failed: 8,
			..ItemCounts::default()
		}
	);
	let a = log_dir(&setup, "a");
	assert_eq!(report.failures.len(), 3);
	let dir = report
		.failures
		.iter()
		.find(|f| f.path == "a/b")
		.expect("the directory's own failure");
	assert_eq!(
		(dir.dest_parent, dir.dest_name.as_str(), dir.stage),
		(a, "b", ExtractStage::CreateDirectory)
	);
	assert_eq!(
		failure_events(&job.recorder),
		[
			(
				true,
				"a/b".into(),
				ExtractStage::CreateDirectory,
				ErrorKind::Server
			),
			(
				false,
				"a/b/c/x.txt".into(),
				ExtractStage::CreateDirectory,
				ErrorKind::Server
			),
			(
				false,
				"a/b/y.txt".into(),
				ExtractStage::CreateDirectory,
				ErrorKind::Server
			),
		]
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_fails_is_recorded_once_and_the_rest_extract() {
	let tar = tar_of(&[
		("docs/up.txt", b"fails uploading"),
		("docs/reg.txt", b"fails registering"),
		("docs/ok.txt", b"fine"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend
			.fail_upload
			.insert("up.txt".into(), ErrorKind::Server);
		backend
			.fail_finish
			.insert("reg.txt".into(), ErrorKind::Server);
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(finished_paths(&setup), ["bundle/docs/ok.txt"]);
	let docs = log_dir(&setup, "docs");
	let recorded: Vec<_> = {
		let mut recorded: Vec<_> = report
			.failures
			.iter()
			.map(|f| {
				(
					f.path.as_str(),
					f.dest_parent,
					f.dest_name.as_str(),
					f.stage,
					f.error.kind(),
				)
			})
			.collect();
		recorded.sort_by_key(|(path, ..)| *path);
		recorded
	};
	assert_eq!(
		recorded,
		[
			(
				"docs/reg.txt",
				docs,
				"reg.txt",
				ExtractStage::Finalize,
				ErrorKind::Server
			),
			(
				"docs/up.txt",
				docs,
				"up.txt",
				ExtractStage::Upload,
				ErrorKind::Server
			),
		]
	);
	assert_eq!(
		failure_events(&job.recorder),
		[
			(
				false,
				"docs/reg.txt".into(),
				ExtractStage::Finalize,
				ErrorKind::Server
			),
			(
				false,
				"docs/up.txt".into(),
				ExtractStage::Upload,
				ErrorKind::Server
			),
		]
	);
	assert_eq!(
		report.counts,
		ItemCounts {
			dirs_created: 2,
			files_done: 1,
			files_failed: 2,
			bytes_done: 4,
			bytes_failed: 15 + 17,
			..ItemCounts::default()
		}
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_registered_as_a_version_is_a_failure() {
	let existing = Uuid::from_u128(0xE);
	let setup = setup(
		"bundle.tar",
		tar_of(&[("a.txt", b"a"), ("b.txt", b"b")]),
		|backend| {
			backend.version_of.insert("a.txt".into(), existing);
		},
	);
	let options = Options {
		root: ExtractRoot::Destination,
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(
		failures(&report),
		[(
			"a.txt",
			"a.txt",
			ExtractStage::RegisteredAsVersion {
				existing_file: existing
			},
			ErrorKind::InvalidState
		)]
	);
	assert_eq!(
		(report.counts.files_done, report.counts.files_failed),
		(1, 1)
	);
	let top: Vec<Uuid> = report.top_level.iter().map(|top| top.item.uuid()).collect();
	assert_eq!(top.len(), 1, "only the new file is reported created");
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_archive_fetch_or_lock_ends_the_job() {
	let setup_fetch = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend
			.fail_fetch
			.insert("bundle.tar".into(), ErrorKind::Server);
	});
	let job = start(&setup_fetch, Options::default());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Server);
	assert!(created_dirs(&setup_fetch).is_empty());
	assert_eq!(job.recorder.last().phase, ExtractPhase::Failed);
	assert_released(&setup_fetch, &job.reporter, &job.recorder);

	// the drive lock is lost after the folder was created: its entries fail, and the job with
	// the first error that ends it
	let setup_lock = setup(
		"bundle.tar",
		tar_of(&[("a.txt", b"a"), ("d/b.txt", b"b")]),
		|backend| backend.fail_locks_from = Some((1, ErrorKind::Unauthenticated)),
	);
	let job = start(&setup_lock, Options::default());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Unauthenticated);
	assert_eq!(created_dirs(&setup_lock), ["bundle"]);
	assert!(finished(&setup_lock).is_empty());
	assert_eq!(job.recorder.last().phase, ExtractPhase::Failed);
	assert_released(&setup_lock, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_damaged_archive_ends_the_job_keeping_what_it_extracted() {
	let tar = tar_of(&[
		("first.txt", b"first"),
		("second.bin", &pattern(3 * CHUNK_SIZE, 3)),
	]);
	let mut archive = gzip(&tar);
	archive.truncate(archive.len() / 2);
	let setup = setup("broken.tgz", archive, |_| {});
	let job = start(&setup, Options::default());
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveCorrupt);
	// every entry reached is done or not attempted
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 1,
			files_done: 1,
			bytes_done: 5,
			files_not_attempted: 1,
			bytes_not_attempted: 3 * CHUNK_SIZE as u64,
			..ItemCounts::default()
		}
	);
	assert_eq!(finished_paths(&setup), ["broken/first.txt"]);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Failed);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn damage_before_the_archives_end_leaves_no_time_to_wait_for() {
	/// Where `second.bin`'s header starts: past `first.txt`'s header and its data block.
	const SECOND_HEADER: usize = 2 * 512;
	let mut tar = tar_of(&[
		("first.txt", b"first"),
		("second.bin", &pattern(3 * CHUNK_SIZE, 3)),
	]);
	tar[SECOND_HEADER + TAR_CHECKSUM.start] ^= 1;
	let setup = setup("broken.tar", tar, |_| {});
	let job = start(&setup, Options::default());
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveCorrupt);
	let last = job.recorder.last();
	// most of the archive is unread, and the job never reads it: no time is left
	assert!(last.bytes_read < setup.archive.size(), "{last:?}");
	assert_eq!(
		(last.phase, last.eta),
		(ExtractPhase::Failed, Some(Duration::ZERO))
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_limits_end_the_job() {
	let tar = tar_of(&[("a.txt", &pattern(1000, 0)), ("b.txt", b"b")]);

	let setup_bytes = setup("a.tar", tar.clone(), |_| {});
	let options = Options {
		max_bytes: Some(500),
		..Options::default()
	};
	let job = start(&setup_bytes, options);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached);
	// ended by an error, not cancelled
	assert!(
		job.recorder
			.updates
			.lock()
			.unwrap()
			.iter()
			.all(|update| update.run_state == RunState::Running)
	);
	assert!(finished(&setup_bytes).is_empty());
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 1,
			files_not_attempted: 1,
			bytes_not_attempted: 1000,
			..ItemCounts::default()
		},
		"the file that would not fit is not attempted"
	);
	assert_released(&setup_bytes, &job.reporter, &job.recorder);

	let setup_items = setup("a.tar", tar, |_| {});
	let options = Options {
		max_items: Some(1),
		..Options::default()
	};
	let job = start(&setup_items, options);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
	assert_released(&setup_items, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn directories_implied_past_the_member_cap_end_the_job() {
	// few directory members, each deep below a path of its own: far more directories than
	// members
	let depth = 25;
	let paths: Vec<String> = (0..100)
		.map(|member| {
			(0..depth)
				.map(|level| format!("m{member}l{level}/"))
				.collect()
		})
		.collect();
	let members: Vec<(&str, &[u8])> = paths.iter().map(|path| (path.as_str(), &b""[..])).collect();
	let setup = setup("bomb.tar", tar_of(&members), |_| {});
	let job = start(&setup, Options::default());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
	assert!(
		created_dirs(&setup).len() <= usize::try_from(MAX_MEMBERS).unwrap() + 1,
		"no more directories than the cap, besides the new folder"
	);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extraction_that_needs_exactly_the_free_storage_fits() {
	let tar = tar_of(&[("a.txt", &pattern(1000, 0)), ("b.txt", b"b")]);

	let setup_exact = setup("a.tar", tar.clone(), |_| {});
	let options = Options {
		max_bytes: Some(1001),
		..Options::default()
	};
	let report = start(&setup_exact, options).running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 2);

	let setup_short = setup("a.tar", tar, |_| {});
	let options = Options {
		max_bytes: Some(1000),
		..Options::default()
	};
	let failed = start(&setup_short, options)
		.running
		.await
		.unwrap()
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_indexed_archive_stating_more_than_max_bytes_creates_nothing() {
	let entries: [(&str, Option<&[u8]>); 3] = [
		("docs", None),
		("docs/a.txt", Some(b"alpha")),
		("b.txt", Some(b"beta")),
	];
	for (name, archive) in [
		("s.zip", zip_of(&entries, None)),
		("s.7z", sevenz_of(&entries, LZMA2, true, None)),
	] {
		// what the index states, 9 bytes, is more than the limit
		let refused = setup(name, archive.clone(), |_| {});
		let options = Options {
			max_bytes: Some(8),
			..Options::default()
		};
		let failed = start(&refused, options).running.await.unwrap().unwrap_err();
		assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached, "{name}");
		assert!(created_dirs(&refused).is_empty(), "{name}");
		assert!(finished(&refused).is_empty(), "{name}");

		// exactly the limit fits
		let fits = setup(name, archive, |_| {});
		let options = Options {
			max_bytes: Some(9),
			..Options::default()
		};
		let report = start(&fits, options).running.await.unwrap().unwrap();
		assert_eq!(report.counts.bytes_done, 9, "{name}");
	}

	// an entry skipped for its path takes no storage
	let with_unsafe: [(&str, Option<&[u8]>); 2] = [
		("../outside.txt", Some(&[7; 100])),
		("a.txt", Some(b"alpha")),
	];
	for (name, archive) in [
		("u.zip", zip_of(&with_unsafe, None)),
		("u.7z", sevenz_of(&with_unsafe, LZMA2, true, None)),
	] {
		let setup = setup(name, archive, |_| {});
		let options = Options {
			max_bytes: Some(10),
			..Options::default()
		};
		let report = start(&setup, options).running.await.unwrap().unwrap();
		assert_eq!(report.counts.bytes_done, 5, "{name}");
		assert_eq!(report.counts.entries_skipped, 1, "{name}");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_entry_is_extracted_again_where_it_was_meant_to_go() {
	let tar = tar_of(&[
		("docs/a.txt", b"alpha"),
		("docs/deep/b.txt", b"beta"),
		("docs/c.txt", b"gamma"),
	]);
	let setup = setup("bundle.tar", tar.clone(), |backend| {
		backend
			.fail_upload
			.insert("a.txt".to_owned(), ErrorKind::Server);
		backend
			.fail_create
			.insert("deep".to_owned(), ErrorKind::Server);
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/docs/c.txt"]);
	let docs = log_dir(&setup, "docs");
	// the file whose upload failed, the directory that failed and the file waiting in it: all go
	// again into the directory that exists, as its contents
	let retries: BTreeMap<&str, (Uuid, Vec<&str>)> = report
		.failures
		.iter()
		.map(|failure| (failure.path.as_str(), retry_target(failure)))
		.collect();
	let in_docs = (docs, vec!["docs"]);
	assert_eq!(
		retries,
		BTreeMap::from([
			("docs/a.txt", in_docs.clone()),
			("docs/deep", in_docs.clone()),
			("docs/deep/b.txt", in_docs),
		])
	);

	// with the right drive now, into the directory the first job created
	let retry = setup_in(docs, "bundle.tar", tar, |_| {});
	let indices: Vec<u32> = report.failures.iter().map(|f| f.entry.index).collect();
	let job = start(&retry, chosen(&indices, &["docs"]));
	job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&retry), ["a.txt", "deep/b.txt"]);
}

/// Where `failure` goes again: the uuid of the directory, and its path in the archive.
fn retry_target(failure: &ExtractFailure) -> (Uuid, Vec<&str>) {
	let retry = failure.retry.as_ref().expect("a failed entry goes again");
	(
		retry.destination.uuid(),
		retry.base.iter().map(AsRef::as_ref).collect(),
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_at_the_top_is_retried_in_the_drives_root() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"alpha")]), |backend| {
		backend
			.fail_upload
			.insert("a.txt".to_owned(), ErrorKind::Server);
	});
	let job = start(&setup, chosen(&[0], &[]));
	let report = job.running.await.unwrap().unwrap();
	let [failure] = report.failures.as_slice() else {
		panic!("{:?}", report.failures);
	};
	let retry = failure.retry.as_ref().expect("a failed file goes again");
	// the extraction's destination itself, the drive's root here: as given, not fetched again
	assert!(
		matches!(&retry.destination, DirType::Root(root) if root.uuid() == setup.destination),
		"{:?}",
		retry.destination
	);
	assert!(retry.base.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failure_of_a_partial_extraction_is_retried_below_its_base() {
	let tar = tar_of(&[
		("docs/", b""),
		("docs/sub/a.txt", b"alpha"),
		("docs/sub/b.txt", b"beta"),
	]);
	let setup = setup("bundle.tar", tar.clone(), |backend| {
		backend
			.fail_upload
			.insert("b.txt".to_owned(), ErrorKind::Server);
	});
	let job = start(&setup, chosen(&[0], &["docs"]));
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["sub/a.txt"]);
	let failure = &report.failures[0];
	// the entry's path in the archive, and where it goes again
	assert_eq!(failure.path, "docs/sub/b.txt");
	let sub = log_dir(&setup, "sub");
	assert_eq!(retry_target(failure), (sub, vec!["docs", "sub"]));

	let retry = setup_in(sub, "bundle.tar", tar, |_| {});
	let job = start(&retry, chosen(&[failure.entry.index], &["docs", "sub"]));
	job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&retry), ["b.txt"]);
}
