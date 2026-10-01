//! What becomes of the archive once its extraction ends: removed when what it held is verified to
//! be in the drive, kept when it cannot be, or when a cancel came first.

use super::*;

/// An archive with `hash` in its metadata, placed in the fake drive in [`ARCHIVE_PARENT`].
pub(super) fn disposable(
	bytes: Vec<u8>,
	hash: Option<Blake3Hash>,
	configure: impl FnOnce(&mut FakeBackend),
) -> Setup {
	let size = bytes.len() as u64;
	let setup = setup_in(DESTINATION, "bundle.tar", bytes, hash, configure);
	setup.backend.place_file(ARCHIVE, ARCHIVE_PARENT, size);
	setup
}

pub(super) fn disposition(report: &ExtractReport) -> DisposalOutcome {
	assert_eq!(report.dispositions.len(), 1);
	report.dispositions[0].outcome.clone()
}

fn kept(outcome: DisposalOutcome) -> KeptReason {
	match outcome {
		DisposalOutcome::Kept { reason, .. } => reason,
		other => panic!("expected the archive kept, got {other:?}"),
	}
}

async fn extract_disposing(
	bytes: Vec<u8>,
	hash: Option<Blake3Hash>,
	how: SourceDisposal,
	root: ExtractRoot,
	configure: impl FnOnce(&mut FakeBackend),
) -> (Setup, ExtractReport) {
	let setup = disposable(bytes, hash, configure);
	let options = Options {
		root,
		dispose: Some(ArchiveDisposal::Remove {
			how,
			parent: ARCHIVE_PARENT,
		}),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_released(&setup, &job.reporter, &job.recorder);
	(setup, report)
}

fn good_tar() -> Vec<u8> {
	tar_of(&[
		("docs/", b""),
		("docs/a.txt", b"alpha"),
		("top.txt", b"top"),
	])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verified_archive_is_trashed_or_deleted() {
	let tar = good_tar();
	for how in [SourceDisposal::Trash, SourceDisposal::DeletePermanently] {
		for root in [
			ExtractRoot::NewFolder { name: None },
			ExtractRoot::Destination,
		] {
			let (setup, report) =
				extract_disposing(tar.clone(), Some(hash(&tar)), how, root.clone(), |_| {}).await;
			let freed = match how {
				SourceDisposal::Trash => 0,
				SourceDisposal::DeletePermanently => tar.len() as u64,
			};
			match disposition(&report) {
				DisposalOutcome::Disposed {
					how: done,
					bytes_freed,
				} => assert_eq!((done, bytes_freed), (how, freed), "{how:?} {root:?}"),
				other => panic!("{how:?} {root:?}: {other:?}"),
			}
			let log = setup.backend.log();
			let removed = match how {
				SourceDisposal::Trash => &log.trashed_files,
				SourceDisposal::DeletePermanently => &log.deleted_files,
			};
			assert_eq!(removed, &[setup.archive.uuid()], "{how:?} {root:?}");
		}
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mac_metadata_left_out_keeps_nothing_from_removing_the_archive() {
	let tar = tar_of(&[
		("__MACOSX/", b""),
		("__MACOSX/._a.txt", &APPLE_DOUBLE),
		("._a.txt", &APPLE_DOUBLE),
		("a.txt", b"alpha"),
	]);
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::Trash,
		ExtractRoot::NewFolder { name: None },
		|_| {},
	)
	.await;
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(
		report
			.skipped
			.iter()
			.map(|skipped| (skipped.path.as_str(), &skipped.reason))
			.collect::<Vec<_>>(),
		// the folder once everything in it was judged
		[
			("__MACOSX/._a.txt", &ExtractSkipReason::MacMetadata),
			("._a.txt", &ExtractSkipReason::MacMetadata),
			("__MACOSX/", &ExtractSkipReason::MacMetadata),
		]
	);
	assert_eq!(report.counts.entries_skipped, 3);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mac_folder_holding_anything_of_the_users_is_created_and_reported_so() {
	let tar = tar_of(&[
		("__MACOSX/", b""),
		("__MACOSX/meta/", b""),
		("__MACOSX/meta/._a.txt", &APPLE_DOUBLE),
		// no metadata: an empty folder, and a file that is not AppleDouble, of the user's
		("__MACOSX/empty/", b""),
		("__MACOSX/user/", b""),
		("__MACOSX/user/notes.txt", b"notes"),
	]);
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::Trash,
		ExtractRoot::NewFolder { name: None },
		|_| {},
	)
	.await;
	assert_eq!(finished_paths(&setup), ["bundle/__MACOSX/user/notes.txt"]);
	assert_eq!(
		created_dirs(&setup),
		["bundle", "__MACOSX", "user", "empty"]
	);
	// what was created is not reported skipped: only the folder holding metadata alone
	assert_eq!(
		report
			.skipped
			.iter()
			.map(|skipped| (skipped.path.as_str(), &skipped.reason))
			.collect::<Vec<_>>(),
		[
			("__MACOSX/meta/._a.txt", &ExtractSkipReason::MacMetadata),
			("__MACOSX/meta/", &ExtractSkipReason::MacMetadata),
		]
	);
	// nothing dropped: the archive goes
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ordinary_file_in_a_mac_folder_is_extracted_before_the_archive_goes() {
	// a file is only macOS metadata when its data says so, wherever it is stored
	let tar = tar_of(&[
		("__MACOSX/thesis.docx", b"irreplaceable"),
		("a.txt", b"alpha"),
	]);
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::DeletePermanently,
		ExtractRoot::NewFolder { name: None },
		|_| {},
	)
	.await;
	assert_eq!(
		finished_paths(&setup),
		["bundle/__MACOSX/thesis.docx", "bundle/a.txt"]
	);
	assert!(report.skipped.is_empty(), "{:?}", report.skipped);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
}

/// Extracts `archive`, with `hash` in its metadata, into a new folder, removing it as `how` says.
async fn extract_into_a_new_folder(
	archive: Vec<u8>,
	hash: Option<Blake3Hash>,
	how: SourceDisposal,
) -> (Setup, ExtractReport) {
	let root = ExtractRoot::NewFolder { name: None };
	extract_disposing(archive, hash, how, root, |_| {}).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_without_a_hash_is_kept_from_a_permanent_deletion() {
	let (setup, report) =
		extract_into_a_new_folder(good_tar(), None, SourceDisposal::DeletePermanently).await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::HashUnavailable
	));
	assert!(setup.backend.log().deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_without_a_hash_is_still_trashed() {
	// only a permanent deletion needs a hash to check the read against
	let (setup, report) = extract_into_a_new_folder(good_tar(), None, SourceDisposal::Trash).await;
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().trashed_files.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_whose_hash_is_not_of_what_was_read_is_kept() {
	let (setup, report) = extract_into_a_new_folder(
		good_tar(),
		Some(hash(b"something else")),
		SourceDisposal::Trash,
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::HashMismatch
	));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_behind_the_tar_keeps_the_archive() {
	let mut junk = good_tar();
	junk.extend_from_slice(b"junk");
	let (setup, report) =
		extract_into_a_new_folder(junk.clone(), Some(hash(&junk)), SourceDisposal::Trash).await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::UnaccountedData { bytes: 4 }
	));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_skipped_entry_keeps_the_archive_as_incomplete() {
	let linked = tar_with(&[TarMember::Symlink {
		path: "link",
		target: "x",
	}]);
	let (setup, report) =
		extract_into_a_new_folder(linked.clone(), Some(hash(&linked)), SourceDisposal::Trash).await;
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_whose_codec_checks_nothing_is_kept_as_unconfirmed() {
	// an lz4 frame without a checksum: the archive's hash matches the bytes read, but nothing
	// checked what they decoded to
	let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
	encoder.write_all(&good_tar()).unwrap();
	let unchecked = encoder.finish().unwrap();
	let (setup, report) = extract_into_a_new_folder(
		unchecked.clone(),
		Some(hash(&unchecked)),
		SourceDisposal::Trash,
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::Unconfirmed
	));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_that_failed_to_upload_keeps_the_archive_as_incomplete() {
	let tar = good_tar();
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::Trash,
		ExtractRoot::NewFolder { name: None },
		|backend| {
			backend
				.fail_upload
				.insert("a.txt".to_owned(), ErrorKind::Server);
		},
	)
	.await;
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}

/// The archive's dispositions the updates carried.
fn disposition_events(recorder: &Recorder) -> Vec<DisposalOutcome> {
	recorder
		.events()
		.into_iter()
		.filter_map(|event| match event {
			ExtractEvent::SourceDisposition(disposition) => Some(disposition.outcome),
			_ => None,
		})
		.collect()
}

/// A tar of a file that uploads and one that may be held: what the cancel tests extract.
fn cancelled_tar() -> Vec<u8> {
	tar_of(&[("done.txt", b"done"), ("stuck.bin", b"stuck")])
}

/// Checks that `failed`, the job `recorder` saw, ended cancelled with its archive kept as
/// interrupted, in its report and in the one disposition its updates carried.
fn assert_kept_as_interrupted(failed: &ExtractFailed, recorder: &Recorder) {
	let interrupted = |outcome: &DisposalOutcome| {
		matches!(
			outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Interrupted,
				bytes_freed: 0
			}
		)
	};
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(interrupted(&disposition(&failed.report)));
	let events = disposition_events(recorder);
	assert!(events.len() == 1 && interrupted(&events[0]), "{events:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extraction_cancelled_while_it_extracts_keeps_its_archive_as_interrupted() {
	let tar = cancelled_tar();
	let setup = disposable(tar.clone(), Some(hash(&tar)), |backend| {
		backend.hold_named(Request::Upload, ["stuck.bin"]);
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::DeletePermanently,
				parent: ARCHIVE_PARENT,
			}),
			..Options::default()
		},
	);
	wait_until("stuck.bin uploads", || {
		job.reporter
			.read(|state| state.active_names() == ["stuck.bin"])
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_kept_as_interrupted(&failed, &job.recorder);
	assert!(setup.backend.log().deleted_files.is_empty());
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extraction_cancelled_while_it_waits_for_a_slot_keeps_its_archive_as_interrupted() {
	let config = one_slot();
	// another job holds the slot
	let other = Reporter::new(Recorder::default(), 0);
	let _running = config.admit(&JobControl::default(), &other.ops()).await;
	let tar = cancelled_tar();
	let setup = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent: ARCHIVE_PARENT,
			}),
			..Options::default()
		},
	);
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_kept_as_interrupted(&failed, &job.recorder);
	assert!(setup.backend.log().trashed_files.is_empty());
}

/// Starts an extraction of `members` into the destination that removes the archive once it is
/// verified, with a cancel for it.
fn start_disposing(members: &[(&str, &[u8])]) -> (Setup, Job, watch::Sender<bool>) {
	let tar = tar_of(members);
	let setup = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			root: ExtractRoot::Destination,
			control,
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent: ARCHIVE_PARENT,
			}),
			..Options::default()
		},
	);
	(setup, job, cancel)
}

/// Cancels `job` once `setup`'s fake drive holds one of its requests, and lets them all go on
/// after it ended: the archive's contents all exist by then, so the job is done, it ends as a
/// cancelled one does, and the archive is kept, interrupted, told of once.
async fn cancel_once_held(setup: &Setup, job: Job, cancel: watch::Sender<bool>) {
	wait_until("a request is held", || !setup.backend.log().held.is_empty()).await;
	cancel.send_replace(true);
	let report = job.running.await.unwrap().unwrap();
	setup.backend.release_all();
	let interrupted = |outcome: &DisposalOutcome| {
		matches!(
			outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Interrupted,
				bytes_freed: 0
			}
		)
	};
	assert!(interrupted(&disposition(&report)));
	let events = disposition_events(&job.recorder);
	assert!(events.len() == 1 && interrupted(&events[0]), "{events:?}");
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(ExtractPhase::Done, RunState::Cancelling)
	);
	assert!(setup.backend.log().trashed_files.is_empty());
	assert_released(setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_the_output_is_checked_keeps_the_archive_as_interrupted() {
	// more top-level items than are checked at once: the check holds on the first batch
	let names: Vec<String> = (0..2 * MAX_SMALL_PARALLEL_REQUESTS + 2)
		.map(|i| format!("f{i:03}.txt"))
		.collect();
	let members: Vec<(&str, &[u8])> = names
		.iter()
		.map(|name| (name.as_str(), &b"x"[..]))
		.collect();
	let (setup, job, cancel) = start_disposing(&members);
	wait_until("a file is registered", || {
		!setup.backend.log().finished.is_empty()
	})
	.await;
	let first: Vec<Uuid> = setup.backend.log().finished.keys().copied().collect();
	setup.backend.hold_requests(Request::State, first);
	cancel_once_held(&setup, job, cancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_the_archive_is_removed_keeps_it_as_interrupted() {
	let (setup, job, cancel) = start_disposing(&[("a.txt", b"alpha")]);
	// the archive's own state is asked for only right before it goes to the trash
	setup
		.backend
		.hold_requests(Request::State, [setup.archive.uuid()]);
	cancel_once_held(&setup, job, cancel).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_the_output_is_checked_is_waited_out() {
	// more top-level items than are checked at once
	let names: Vec<String> = (0..2 * MAX_SMALL_PARALLEL_REQUESTS + 2)
		.map(|i| format!("f{i:03}.txt"))
		.collect();
	let members: Vec<(&str, &[u8])> = names
		.iter()
		.map(|name| (name.as_str(), &b"x"[..]))
		.collect();
	let tar = tar_of(&members);
	let setup = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			root: ExtractRoot::Destination,
			control,
			config: config.clone(),
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent: ARCHIVE_PARENT,
			}),
			..Options::default()
		},
	);
	// the files registered first are in the first batch checked: their checks wait
	wait_until("a file is registered", || {
		!setup.backend.log().finished.is_empty()
	})
	.await;
	let first: Vec<Uuid> = setup.backend.log().finished.keys().copied().collect();
	setup.backend.hold_requests(Request::State, first);
	wait_until("the output is checked", || {
		!setup.backend.log().held.is_empty()
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job);
	assert!(
		setup.backend.log().trashed_files.is_empty(),
		"the archive is not removed while paused"
	);

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().trashed_files, [setup.archive.uuid()]);
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_moved_while_it_was_extracted_is_kept_as_changed() {
	let tar = good_tar();
	let setup = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	setup
		.backend
		.place_file(ARCHIVE, Uuid::from_u128(0xF), tar.len() as u64);
	let job = start(
		&setup,
		Options {
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent: ARCHIVE_PARENT,
			}),
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert!(matches!(kept(disposition(&report)), KeptReason::Changed));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_whose_output_lost_a_file_is_kept_as_unconfirmed() {
	// the fake drive forgets every file registered, so the re-listing finds fewer than were
	// created, as when the extracted folder lost a file before the check
	let tar = good_tar();
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::DeletePermanently,
		ExtractRoot::NewFolder { name: None },
		|backend| {
			backend.quirks.insert(Quirk::ForgetRegistered);
		},
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::Unconfirmed
	));
	assert!(setup.backend.log().deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_in_the_trash_is_kept_as_changed() {
	let tar = good_tar();
	let setup = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let job = start(
		&setup,
		Options {
			dispose: Some(ArchiveDisposal::Unavailable),
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(created_dirs(&setup), ["bundle", "docs"]);
	assert!(matches!(kept(disposition(&report)), KeptReason::Changed));
	assert_released(&setup, &job.reporter, &job.recorder);
	let log = setup.backend.log();
	assert!(log.trashed_files.is_empty() && log.deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_moved_to_the_trash_meanwhile_is_kept_as_changed() {
	let tar = good_tar();
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::DeletePermanently,
		ExtractRoot::NewFolder { name: None },
		|backend| {
			backend.in_trash.insert(ARCHIVE);
		},
	)
	.await;
	assert!(matches!(kept(disposition(&report)), KeptReason::Changed));
	let log = setup.backend.log();
	assert!(log.trashed_files.is_empty() && log.deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_extraction_whose_output_cannot_be_listed_keeps_its_archive() {
	let tar = good_tar();
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(&tar)),
		SourceDisposal::Trash,
		ExtractRoot::NewFolder { name: None },
		|backend| {
			backend.quirks.insert(Quirk::FailTrees);
		},
	)
	.await;
	assert_eq!(finished_paths(&setup).len(), 2, "everything was extracted");
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::Unconfirmed
	));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_with_duplicate_names_is_kept() {
	let zip = zip_of(&[("same", Some(b"1")), ("same", Some(b"2"))], None);
	let setup = disposable(zip, None, |_| {});
	let options = Options {
		dispose: Some(ArchiveDisposal::Remove {
			how: SourceDisposal::Trash,
			parent: ARCHIVE_PARENT,
		}),
		..Options::default()
	};
	let report = start(&setup, options).running.await.unwrap().unwrap();
	assert_eq!(report.duplicates.as_ref().map(|d| d.count), Some(1));
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}
