//! What becomes of the archive once its extraction ends: removed when what it held is verified to
//! be in the drive, kept when it cannot be, or when a cancel came first.

use super::*;

/// An archive with a hash in its metadata, placed in the fake drive in `parent`.
pub(super) fn disposable(
	bytes: Vec<u8>,
	hash: Option<Blake3Hash>,
	configure: impl FnOnce(&mut FakeBackend),
) -> (Setup, Uuid) {
	let parent = ARCHIVE_PARENT;
	let mut s = setup("bundle.tar", bytes.clone(), configure);
	let archive = archive_file_with("bundle.tar", &bytes, hash);
	let backend = Arc::get_mut(&mut s.backend).unwrap();
	let data = backend.contents.remove(&s.archive.uuid()).unwrap();
	backend.contents.insert(archive.uuid(), data);
	s.archive = archive;
	s.backend
		.place_file(s.archive.uuid(), parent, bytes.len() as u64);
	(s, parent)
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
	let (setup, parent) = disposable(bytes, hash, configure);
	let options = Options {
		root,
		dispose: Some(ArchiveDisposal::Remove { how, parent }),
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
	let apple_double = [&[0x00, 0x05, 0x16, 0x07][..], b"\x00\x02\x00\x00"].concat();
	let tar = tar_of(&[
		("__MACOSX/", b""),
		("__MACOSX/._a.txt", &apple_double),
		("._a.txt", &apple_double),
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
	let apple_double = [&[0x00, 0x05, 0x16, 0x07][..], b"\x00\x02\x00\x00"].concat();
	let tar = tar_of(&[
		("__MACOSX/", b""),
		("__MACOSX/meta/", b""),
		("__MACOSX/meta/._a.txt", &apple_double),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_that_cannot_be_verified_is_kept() {
	let tar = good_tar();
	let new_folder = || ExtractRoot::NewFolder { name: None };

	// no hash to check the read against, which only a permanent deletion needs
	let (_, report) = extract_disposing(
		tar.clone(),
		None,
		SourceDisposal::DeletePermanently,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::HashUnavailable
	));
	let (setup, report) = extract_disposing(
		tar.clone(),
		None,
		SourceDisposal::Trash,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().trashed_files.len(), 1);

	// a hash that does not match what was read
	let (setup, report) = extract_disposing(
		tar.clone(),
		Some(hash(b"something else")),
		SourceDisposal::Trash,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::HashMismatch
	));
	assert!(setup.backend.log().trashed_files.is_empty());

	// data behind the tar
	let mut junk = tar.clone();
	junk.extend_from_slice(b"junk");
	let (_, report) = extract_disposing(
		junk.clone(),
		Some(hash(&junk)),
		SourceDisposal::Trash,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::UnaccountedData { bytes: 4 }
	));

	// an entry that was skipped
	let mut builder = tar::Builder::new(Vec::new());
	let mut link = tar::Header::new_gnu();
	link.set_entry_type(tar::EntryType::Symlink);
	link.set_size(0);
	builder.append_link(&mut link, "link", "x").unwrap();
	let linked = builder.into_inner().unwrap();
	let (_, report) = extract_disposing(
		linked.clone(),
		Some(hash(&linked)),
		SourceDisposal::Trash,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));

	// a compressed tar whose codec carries no checksum (an lz4 frame without one): the archive's
	// hash matches the bytes read, but nothing checked what they decoded to
	let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
	encoder.write_all(&tar).unwrap();
	let unchecked = encoder.finish().unwrap();
	let (setup, report) = extract_disposing(
		unchecked.clone(),
		Some(hash(&unchecked)),
		SourceDisposal::Trash,
		new_folder(),
		|_| {},
	)
	.await;
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::Unconfirmed
	));
	assert!(setup.backend.log().trashed_files.is_empty());

	// a file that failed to upload
	let (setup, report) = {
		let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |backend| {
			backend
				.fail_upload
				.insert("a.txt".to_owned(), ErrorKind::Server);
		});
		let options = Options {
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent,
			}),
			..Options::default()
		};
		let job = start(&setup, options);
		let report = job.running.await.unwrap().unwrap();
		(setup, report)
	};
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}

/// The archive's dispositions the updates carried.
fn disposition_events(recorder: &Recorder) -> Vec<DisposalOutcome> {
	recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.flat_map(|update| &update.events)
		.filter_map(|event| match event {
			ExtractEvent::SourceDisposition(disposition) => Some(disposition.outcome.clone()),
			_ => None,
		})
		.collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_extraction_keeps_its_archive_as_interrupted() {
	let interrupted = |outcome: &DisposalOutcome| {
		matches!(
			outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Interrupted,
				bytes_freed: 0
			}
		)
	};
	let tar = tar_of(&[("done.txt", b"done"), ("stuck.bin", b"stuck")]);

	// cancelled while it extracts
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |backend| {
		backend.blocked_uploads.insert("stuck.bin".to_owned());
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::DeletePermanently,
				parent,
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
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(interrupted(&disposition(&failed.report)));
	let events = disposition_events(&job.recorder);
	assert!(events.len() == 1 && interrupted(&events[0]), "{events:?}");
	assert!(setup.backend.log().deleted_files.is_empty());
	assert_released(&setup, &job.reporter, &job.recorder);

	// cancelled while it waits for a slot
	let config = one_slot();
	// another job holds the slot
	let other = Reporter::new(Recorder::default(), 0);
	let _running = config.admit(&JobControl::default(), &other.ops()).await;
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent,
			}),
			..Options::default()
		},
	);
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(interrupted(&disposition(&failed.report)));
	let events = disposition_events(&job.recorder);
	assert!(events.len() == 1 && interrupted(&events[0]), "{events:?}");
	assert!(setup.backend.log().trashed_files.is_empty());
}

/// Starts an extraction of `members` into the destination that removes the archive once it is
/// verified, with a cancel for it.
fn start_disposing(members: &[(&str, &[u8])]) -> (Setup, Job, watch::Sender<bool>) {
	let tar = tar_of(members);
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			root: ExtractRoot::Destination,
			control,
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent,
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
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
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
				parent,
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
async fn an_archive_that_changed_or_whose_output_is_gone_is_kept() {
	let tar = good_tar();
	// moved elsewhere while it was extracted
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	setup
		.backend
		.place_file(setup.archive.uuid(), Uuid::from_u128(0xF), tar.len() as u64);
	let job = start(
		&setup,
		Options {
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::Trash,
				parent,
			}),
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert!(matches!(kept(disposition(&report)), KeptReason::Changed));
	assert!(setup.backend.log().trashed_files.is_empty());

	// the extracted folder lost a file before the check: the fake drive forgets every file
	// registered from now on, so the re-listing finds fewer than were created
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |backend| {
		backend.quirks.insert(Quirk::ForgetRegistered);
	});
	let job = start(
		&setup,
		Options {
			dispose: Some(ArchiveDisposal::Remove {
				how: SourceDisposal::DeletePermanently,
				parent,
			}),
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert!(matches!(
		kept(disposition(&report)),
		KeptReason::Unconfirmed
	));
	assert!(setup.backend.log().deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_in_the_trash_is_kept_as_changed() {
	let tar = good_tar();
	let (setup, _) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
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
async fn a_zip_with_duplicate_names_is_kept() {
	let zip = zip_of(&[("same", Some(b"1")), ("same", Some(b"2"))], None);
	let (setup, parent) = disposable(zip, None, |_| {});
	let options = Options {
		dispose: Some(ArchiveDisposal::Remove {
			how: SourceDisposal::Trash,
			parent,
		}),
		..Options::default()
	};
	let report = start(&setup, options).running.await.unwrap().unwrap();
	assert_eq!(report.duplicates.as_ref().map(|d| d.count), Some(1));
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}
