//! The driver against the fake drive, with the real codec on its thread; the archive the fake
//! received is put back together and read with the SDK's own decoders.

use std::{
	borrow::Cow,
	collections::BTreeMap,
	io::Read,
	sync::{Mutex, atomic::Ordering},
	time::Duration,
};

use chrono::Utc;
use tokio::{
	sync::{Semaphore, mpsc},
	task::JoinHandle,
};

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasName,
		archive::{
			compress::{
				CompressFormat, CompressUpdate, RunState,
				codec::{ArchiveEntry, CompressJob, compress},
				report::CompressCallback,
			},
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			decode::open_stream,
			dispose::{DisposalOutcome, ExpectedFile, KeptReason, SourceDisposal, Tree},
			encode::Compression,
			format::StreamCodec,
			tar_iter::TarReader,
			worker,
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			test_support::{FakeBackend, Quirk, wait_until},
		},
		file::{
			AnonymousRemoteFile, RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
	},
	job::test_support::controls,
};

#[derive(Default)]
struct Recorder {
	created: Mutex<Vec<RemoteFile>>,
	updates: Mutex<Vec<CompressUpdate>>,
	/// The client's memory budget, to see what an update reading paused was sent with.
	memory: Option<Arc<Semaphore>>,
	/// The budget's free permits at each update reading paused.
	free_when_paused: Mutex<Vec<usize>>,
}

impl CompressCallback for Recorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		self.created.lock().unwrap().push(archive);
	}

	fn on_update(&self, update: CompressUpdate) {
		if update.run_state == RunState::Paused
			&& let Some(memory) = &self.memory
		{
			self.free_when_paused
				.lock()
				.unwrap()
				.push(memory.available_permits());
		}
		self.updates.lock().unwrap().push(update);
	}
}

impl Recorder {
	fn events(&self) -> Vec<CompressEvent> {
		self.updates
			.lock()
			.unwrap()
			.iter()
			.flat_map(|update| update.events.clone())
			.collect()
	}

	fn last(&self) -> CompressUpdate {
		self.updates.lock().unwrap().last().unwrap().clone()
	}
}

fn source_file(name: &str, bytes: &[u8], hash: Option<Blake3Hash>) -> RemoteFileType<'static> {
	let size = bytes.len() as u64;
	let meta = FileMeta::Decoded(DecryptedFileMeta {
		name: Cow::Owned(name.to_owned()),
		size,
		mime: Cow::Borrowed("application/octet-stream"),
		key: FileKey::V3(EncryptionKey::generate()),
		last_modified: Utc::now(),
		created: None,
		hash,
	});
	let file: AnonymousRemoteFile = RemoteFile::from_meta(
		Uuid::new_v4(),
		(),
		Uuid::new_v4().into(),
		size,
		size.div_ceil(CHUNK_SIZE_U64),
		"de-1",
		"bucket",
		Utc::now(),
		false,
		meta,
	);
	RemoteFileType::File(Cow::Owned(file))
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len)
		.map(|i| (i % 241).to_le_bytes()[0] ^ seed)
		.collect()
}

fn hash_of(data: &[u8]) -> Option<Blake3Hash> {
	Some(Blake3Hash::from(blake3::hash(data)))
}

/// A directory with two files, one over a chunk, and a file at the top.
struct Setup {
	backend: Arc<FakeBackend>,
	destination: Uuid,
	entries: Vec<ArchiveEntry>,
	sources: Vec<(String, RemoteFileType<'static>)>,
	contents: Vec<Vec<u8>>,
}

fn setup(configure: impl FnOnce(&mut FakeBackend, &[RemoteFileType<'static>])) -> Setup {
	let destination = Uuid::new_v4();
	let contents = vec![
		b"alpha".to_vec(),
		pattern(CHUNK_SIZE + 77, 3),
		b"top".to_vec(),
	];
	let paths = ["docs/a.txt", "docs/big.bin", "top.txt"];
	let files: Vec<RemoteFileType<'static>> = contents
		.iter()
		.zip(paths)
		.map(|(data, path)| source_file(path.rsplit('/').next().unwrap(), data, hash_of(data)))
		.collect();
	let mut backend = FakeBackend::new(destination);
	backend.quirks.insert(Quirk::KeepUploads);
	for (file, data) in files.iter().zip(&contents) {
		backend.contents.insert(file.uuid(), data.clone());
	}
	configure(&mut backend, &files);
	let mut entries = vec![ArchiveEntry::Dir {
		path: "docs".into(),
		modified: None,
	}];
	entries.extend(
		paths
			.iter()
			.zip(&contents)
			.enumerate()
			.map(|(source, (path, data))| ArchiveEntry::File {
				source: u32::try_from(source).unwrap(),
				path: (*path).to_owned(),
				size: data.len() as u64,
				modified: None,
			}),
	);
	Setup {
		backend: Arc::new(backend),
		destination,
		entries,
		sources: paths.iter().map(|p| (*p).to_owned()).zip(files).collect(),
		contents,
	}
}

struct Job {
	running: JoinHandle<Result<CompressReport, CompressFailed>>,
	recorder: Arc<Recorder>,
	reporter: MaybeArc<Reporter>,
}

fn start_with(
	setup: &Setup,
	name: &str,
	format: CompressFormat,
	control: JobControl,
	max_bytes: Option<u64>,
	start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
) -> Job {
	start_disposing(
		setup,
		name,
		format,
		control,
		max_bytes,
		start,
		None,
		CompressReport::default(),
	)
}

#[expect(clippy::too_many_arguments)]
fn start_disposing(
	setup: &Setup,
	name: &str,
	format: CompressFormat,
	control: JobControl,
	max_bytes: Option<u64>,
	start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
	disposal: Option<CompressDisposal>,
	report: CompressReport,
) -> Job {
	let recorder = Arc::new(Recorder {
		memory: Some(Arc::clone(&setup.backend.memory)),
		..Recorder::default()
	});
	let reporter = Reporter::new(Arc::clone(&recorder));
	let extension_len = format.check_name(name).unwrap();
	let running = tokio::spawn(run_compress(CompressTask {
		backend: Arc::clone(&setup.backend),
		control,
		reporter: MaybeArc::clone(&reporter),
		destination: DirType::Root(Cow::Owned(RootDirectory::new(setup.destination))),
		name: ValidatedName::try_from(name).unwrap(),
		extension_len,
		sources: setup
			.sources
			.iter()
			.map(|(path, file)| Source {
				file: file.clone(),
				path: path.clone(),
				// the directory `docs` is the first top-level source, `top.txt` the second
				request: usize::from(!path.starts_with("docs/")),
			})
			.collect(),
		max_bytes,
		config: ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY),
		start,
		report,
		disposal,
	}));
	Job {
		running,
		recorder,
		reporter,
	}
}

fn start(
	setup: &Setup,
	name: &str,
	format: CompressFormat,
	control: JobControl,
	max_bytes: Option<u64>,
) -> Job {
	let job = CompressJob {
		format,
		entries: setup.entries.clone(),
		password: None,
	};
	start_with(
		setup,
		name,
		format,
		control,
		max_bytes,
		Box::new(move || worker::start(move |port| compress(&port, job))),
	)
}

fn gzip_tar() -> CompressFormat {
	CompressFormat::Tar {
		compression: Some(Compression {
			codec: StreamCodec::Gzip,
			level: None,
		}),
	}
}

/// The uploaded archive, chunk by chunk in order.
fn uploaded(setup: &Setup, uuid: Uuid) -> Vec<u8> {
	let log = setup.backend.log();
	let mut chunks: BTreeMap<u64, &Vec<u8>> = BTreeMap::new();
	for ((file, index), data) in &log.uploaded_data {
		if *file == uuid {
			chunks.insert(*index, data);
		}
	}
	chunks.into_values().flatten().copied().collect()
}

fn members(archive: &[u8], codec: Option<StreamCodec>) -> Vec<(String, Vec<u8>)> {
	let reader: Box<dyn Read> = match codec {
		None => Box::new(archive),
		Some(codec) => Box::new(open_stream(codec, archive, 64 << 20).unwrap()),
	};
	let mut tar = TarReader::new(reader, 100);
	let mut members = Vec::new();
	while let Some(member) = tar.next_member().unwrap() {
		let mut data = Vec::new();
		let mut buf = [0u8; 4096];
		loop {
			let n = tar.read_body(&mut buf).unwrap();
			if n == 0 {
				break;
			}
			data.extend_from_slice(&buf[..n]);
		}
		members.push((member.path, data));
	}
	members
}

fn assert_released(setup: &Setup, reporter: &Reporter) {
	assert_eq!(
		setup.backend.memory.available_permits(),
		setup.backend.budget,
		"every memory reservation is released"
	);
	assert_eq!(
		setup.backend.live_locks.load(Ordering::SeqCst),
		0,
		"no drive lock is held"
	);
	assert_eq!(reporter.ops_in_flight(), 0, "nothing is in flight");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compresses_the_sources_into_one_new_file() {
	let setup = setup(|backend, _| {
		backend.listed = ListedNames {
			names: vec!["bundle.tar.gz".into()],
			unverified: false,
		};
	});
	let job = start(
		&setup,
		"bundle.tar.gz",
		gzip_tar(),
		JobControl::default(),
		None,
	);
	let report = job.running.await.unwrap().unwrap();

	let archive = report.archive.as_ref().expect("the archive is registered");
	assert_eq!(
		archive.name(),
		Some("bundle (1).tar.gz"),
		"keep-both keeps .tar.gz whole"
	);
	let finished = setup.backend.log().finished.clone();
	assert_eq!(finished.len(), 1, "one file is registered: the archive");
	let (name, completion) = &finished[&archive.uuid()];
	assert_eq!(name, "bundle (1).tar.gz");
	let bytes = uploaded(&setup, archive.uuid());
	assert_eq!(completion.written, bytes.len() as u64);
	assert_eq!(completion.hash, Blake3Hash::from(blake3::hash(&bytes)));
	assert_eq!(
		members(&bytes, Some(StreamCodec::Gzip)),
		[
			("docs/".to_owned(), Vec::new()),
			("docs/a.txt".to_owned(), setup.contents[0].clone()),
			("docs/big.bin".to_owned(), setup.contents[1].clone()),
			("top.txt".to_owned(), setup.contents[2].clone()),
		]
	);
	let total: u64 = setup.contents.iter().map(|c| c.len() as u64).sum();
	assert_eq!(report.counts.files_done, 3);
	assert_eq!(report.counts.bytes_read, total);
	assert_eq!(report.counts.bytes_done, bytes.len() as u64);
	assert_eq!(job.recorder.created.lock().unwrap().len(), 1);
	assert_eq!(job.recorder.last().phase, CompressPhase::Done);
	assert!(
		job.recorder.events().is_empty(),
		"every source matched its hash"
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_does_not_match_its_hash_is_reported() {
	let setup = setup(|backend, files| {
		// the fake serves other bytes than the ones the metadata's hash is of
		backend.contents.insert(files[2].uuid(), b"TOP".to_vec());
	});
	let job = start(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
	);
	let report = job.running.await.unwrap().unwrap();
	assert!(report.archive.is_some());
	let mismatched: Vec<String> = job
		.recorder
		.events()
		.into_iter()
		.filter_map(|event| match event {
			CompressEvent::SourceHashMismatch { path, .. } => Some(path),
			_ => None,
		})
		.collect();
	assert_eq!(mismatched, ["top.txt"]);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_ends_early_leaves_nothing_behind() {
	// running out of storage while writing
	let setup_storage = setup(|_, _| {});
	let job = start(
		&setup_storage,
		"b.tar.gz",
		gzip_tar(),
		JobControl::default(),
		Some(100),
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached);
	assert!(failed.report.archive.is_none());
	assert!(setup_storage.backend.log().finished.is_empty());
	assert_released(&setup_storage, &job.reporter);

	// a source that cannot be read
	let setup_fetch = setup(|backend, _| {
		backend
			.fail_fetch
			.insert("big.bin".to_owned(), ErrorKind::Server);
	});
	let job = start(
		&setup_fetch,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Server);
	assert!(setup_fetch.backend.log().finished.is_empty());
	assert_released(&setup_fetch, &job.reporter);

	// a cancel while the archive is uploading
	let setup_cancel = setup(|backend, _| {
		backend.blocked_uploads.insert("b.tar".to_owned());
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup_cancel,
		"b.tar",
		CompressFormat::Tar { compression: None },
		control,
		None,
	);
	// every source is read, and the archive's first chunk is stuck uploading
	let total: u64 = setup_cancel.contents.iter().map(|c| c.len() as u64).sum();
	wait_until("every source is read", || {
		job.reporter.counts().bytes_read == total
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(setup_cancel.backend.log().finished.is_empty());
	assert_eq!(job.recorder.last().phase, CompressPhase::Cancelled);
	assert_released(&setup_cancel, &job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_silent_codec_is_given_up_on() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
		Box::new(move || Ok(link)),
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWorkerDied);
	assert!(setup.backend.log().finished.is_empty());
	assert_released(&setup, &job.reporter);
	drop((events, result));
}

/// The sources as placed in the fake drive: `docs` (holding the first two files) in `parent`,
/// and the third file next to it.
struct Placed {
	parent: Uuid,
	docs: Uuid,
}

fn place(setup: &Setup) -> Placed {
	let parent = Uuid::new_v4();
	let docs = Uuid::new_v4();
	setup.backend.place_dir(docs, parent);
	for (index, (_, file)) in setup.sources.iter().enumerate() {
		let at = if index < 2 { docs } else { parent };
		setup.backend.place_file(file.uuid(), at, file.size());
	}
	Placed { parent, docs }
}

fn targets(setup: &Setup, placed: &Placed) -> Vec<DisposalTarget> {
	let read = Tree {
		files: setup.sources[..2]
			.iter()
			.map(|(_, file)| (file.uuid(), file.size()))
			.collect(),
		dirs: Default::default(),
	};
	let top = &setup.sources[2].1;
	vec![
		DisposalTarget::Dir {
			uuid: placed.docs,
			read,
		},
		DisposalTarget::File(ExpectedFile::of(top, top.uuid(), placed.parent)),
	]
}

async fn compress_disposing(
	setup: &Setup,
	how: SourceDisposal,
	hashed: [bool; 2],
	report: CompressReport,
) -> CompressReport {
	let placed = place(setup);
	let disposal = CompressDisposal {
		how,
		targets: targets(setup, &placed),
		hashed: hashed.to_vec(),
	};
	let job = CompressJob {
		format: CompressFormat::Tar { compression: None },
		entries: setup.entries.clone(),
		password: None,
	};
	let job = start_disposing(
		setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
		Box::new(move || worker::start(move |port| compress(&port, job))),
		Some(disposal),
		report,
	);
	let report = job.running.await.unwrap().unwrap();
	assert_released(setup, &job.reporter);
	report
}

fn outcomes(report: &CompressReport) -> Vec<DisposalOutcome> {
	report
		.dispositions
		.iter()
		.map(|disposition| disposition.outcome.clone())
		.collect()
}

fn all_kept_for(report: &CompressReport, expected: fn(&KeptReason) -> bool) {
	assert_eq!(report.dispositions.len(), 2);
	for outcome in outcomes(report) {
		match outcome {
			DisposalOutcome::Kept { reason, .. } => assert!(expected(&reason), "{reason:?}"),
			other => panic!("expected the source kept, got {other:?}"),
		}
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn verified_sources_are_trashed() {
	let setup = setup(|_, _| {});
	let report = compress_disposing(
		&setup,
		SourceDisposal::Trash,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	for outcome in outcomes(&report) {
		assert!(matches!(
			outcome,
			DisposalOutcome::Disposed {
				how: SourceDisposal::Trash,
				bytes_freed: 0
			}
		));
	}
	let log = setup.backend.log();
	assert_eq!(log.trashed_files, [setup.sources[2].1.uuid()]);
	assert_eq!(log.trashed_dirs.len(), 1);
	assert!(log.deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_removal_deletes_what_was_read_and_trashes_the_emptied_directory() {
	let setup = setup(|_, _| {});
	let report = compress_disposing(
		&setup,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let freed: Vec<u64> = outcomes(&report)
		.into_iter()
		.map(|outcome| match outcome {
			DisposalOutcome::Disposed { bytes_freed, .. } => bytes_freed,
			other => panic!("{other:?}"),
		})
		.collect();
	let sizes: Vec<u64> = setup.contents.iter().map(|c| c.len() as u64).collect();
	assert_eq!(freed, [sizes[0] + sizes[1], sizes[2]]);
	let log = setup.backend.log();
	let mut deleted = log.deleted_files.clone();
	deleted.sort();
	let mut expected: Vec<Uuid> = setup.sources.iter().map(|(_, file)| file.uuid()).collect();
	expected.sort();
	assert_eq!(deleted, expected);
	assert_eq!(
		log.trashed_dirs.len(),
		1,
		"the emptied directory is trashed, never purged"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_changed_is_kept_on_its_own() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	// a file arrived in the directory after it was read
	setup.backend.place_file(Uuid::new_v4(), placed.docs, 10);
	let disposal = CompressDisposal {
		how: SourceDisposal::DeletePermanently,
		targets: targets(&setup, &placed),
		hashed: vec![true, true],
	};
	let job = CompressJob {
		format: CompressFormat::Tar { compression: None },
		entries: setup.entries.clone(),
		password: None,
	};
	let job = start_disposing(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
		Box::new(move || worker::start(move |port| compress(&port, job))),
		Some(disposal),
		CompressReport::default(),
	);
	let report = job.running.await.unwrap().unwrap();
	let outcomes = outcomes(&report);
	assert!(matches!(
		&outcomes[0],
		DisposalOutcome::Kept {
			reason: KeptReason::Changed,
			..
		}
	));
	assert!(matches!(&outcomes[1], DisposalOutcome::Disposed { .. }));
	let log = setup.backend.log();
	assert_eq!(
		log.deleted_files,
		[setup.sources[2].1.uuid()],
		"nothing in the directory"
	);
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sources_are_kept_when_the_archive_cannot_be_trusted() {
	// a source whose data did not match its hash
	let setup_mismatch = setup(|backend, files| {
		backend.contents.insert(files[2].uuid(), b"TOP".to_vec());
	});
	let report = compress_disposing(
		&setup_mismatch,
		SourceDisposal::Trash,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	// only the source the mismatching file belongs to is kept
	let [docs, top] = &outcomes(&report)[..] else {
		panic!("two sources");
	};
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(
		matches!(
			top,
			DisposalOutcome::Kept {
				reason: KeptReason::HashMismatch,
				..
			}
		),
		"{top:?}"
	);
	assert_eq!(setup_mismatch.backend.log().trashed_dirs.len(), 1);
	assert!(setup_mismatch.backend.log().trashed_files.is_empty());

	// a source without hashes is kept on its own under a permanent removal
	let setup_half = setup(|_, _| {});
	let report = compress_disposing(
		&setup_half,
		SourceDisposal::DeletePermanently,
		[true, false],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = &outcomes(&report)[..] else {
		panic!("two sources");
	};
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(
		matches!(
			top,
			DisposalOutcome::Kept {
				reason: KeptReason::HashUnavailable,
				..
			}
		),
		"{top:?}"
	);

	// no hash to check a permanent deletion against
	let setup_unhashed = setup(|_, _| {});
	let report = compress_disposing(
		&setup_unhashed,
		SourceDisposal::DeletePermanently,
		[false; 2],
		CompressReport::default(),
	)
	.await;
	all_kept_for(&report, |reason| {
		matches!(reason, KeptReason::HashUnavailable)
	});

	// an entry that was skipped while planning
	let setup_skipped = setup(|_, _| {});
	let skipped = CompressReport {
		skipped: vec![crate::fs::drive_job::plan::SkippedEntry {
			source_path: "docs/secret".into(),
			bytes: 1,
			reason: crate::fs::drive_job::plan::SkipReason::UndecryptableFile {
				uuid: Uuid::new_v4(),
			},
		}],
		..CompressReport::default()
	};
	let report =
		compress_disposing(&setup_skipped, SourceDisposal::Trash, [true; 2], skipped).await;
	all_kept_for(&report, |reason| matches!(reason, KeptReason::Incomplete));

	// the archive is not in the drive as it was registered
	let setup_gone = setup(|backend, _| {
		backend.quirks.insert(Quirk::ForgetRegistered);
	});
	let report = compress_disposing(
		&setup_gone,
		SourceDisposal::Trash,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	all_kept_for(&report, |reason| matches!(reason, KeptReason::Unconfirmed));

	for setup in [setup_unhashed, setup_skipped, setup_gone] {
		let log = setup.backend.log();
		assert!(log.trashed_files.is_empty() && log.deleted_files.is_empty());
		assert!(log.trashed_dirs.is_empty());
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_removal_keeps_what_has_older_versions() {
	// the top-level file has versions: it is kept, the folder goes
	let setup_file = setup(|backend, files| {
		backend.versioned_files.insert(files[2].uuid());
	});
	let report = compress_disposing(
		&setup_file,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = &outcomes(&report)[..] else {
		panic!("two sources");
	};
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(
		matches!(
			top,
			DisposalOutcome::Kept {
				reason: KeptReason::HasVersions,
				..
			}
		),
		"{top:?}"
	);

	// a file in the folder has versions: nothing of the folder is deleted
	let setup_dir = setup(|backend, files| {
		backend.versioned_files.insert(files[1].uuid());
	});
	let report = compress_disposing(
		&setup_dir,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, _] = &outcomes(&report)[..] else {
		panic!("two sources");
	};
	assert!(
		matches!(
			docs,
			DisposalOutcome::Kept {
				reason: KeptReason::HasVersions,
				..
			}
		),
		"{docs:?}"
	);
	let log = setup_dir.backend.log();
	let folder_files = [setup_dir.sources[0].1.uuid(), setup_dir.sources[1].1.uuid()];
	assert!(
		log.deleted_files
			.iter()
			.all(|uuid| !folder_files.contains(uuid))
	);
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_removal_cut_short_says_what_it_deleted() {
	// the folder's files are deleted in uuid order: the second one fails
	let setup = setup(|backend, files| {
		let last = files[..2].iter().map(HasUUID::uuid).max().unwrap();
		backend.fail_deletes_of.insert(last);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, _] = &outcomes(&report)[..] else {
		panic!("two sources");
	};
	let first = setup.sources[..2]
		.iter()
		.min_by_key(|(_, file)| file.uuid())
		.unwrap()
		.1
		.size();
	let DisposalOutcome::Kept {
		reason: KeptReason::Failed { .. },
		bytes_freed,
	} = docs
	else {
		panic!("{docs:?}");
	};
	assert_eq!(
		*bytes_freed, first,
		"the file deleted before the failure is reported gone"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_inside_another_goes_with_it() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	// docs, top.txt, docs/a.txt given on its own too, and top.txt given twice
	let a = &setup.sources[0].1;
	let top = &setup.sources[2].1;
	let mut targets = targets(&setup, &placed);
	targets.push(DisposalTarget::File(ExpectedFile::of(
		a,
		a.uuid(),
		placed.docs,
	)));
	targets.push(DisposalTarget::File(ExpectedFile::of(
		top,
		top.uuid(),
		placed.parent,
	)));
	let disposal = CompressDisposal {
		how: SourceDisposal::DeletePermanently,
		targets,
		hashed: vec![true; 4],
	};
	let job = CompressJob {
		format: CompressFormat::Tar { compression: None },
		entries: setup.entries.clone(),
		password: None,
	};
	let job = start_disposing(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
		Box::new(move || worker::start(move |port| compress(&port, job))),
		Some(disposal),
		CompressReport::default(),
	);
	let report = job.running.await.unwrap().unwrap();
	let outcomes = outcomes(&report);
	assert!(
		outcomes
			.iter()
			.all(|outcome| matches!(outcome, DisposalOutcome::Disposed { .. })),
		"{outcomes:?}"
	);
	// each file deleted once, its bytes counted once
	let freed: u64 = outcomes
		.iter()
		.map(|outcome| match outcome {
			DisposalOutcome::Disposed { bytes_freed, .. } => *bytes_freed,
			_ => 0,
		})
		.sum();
	let total: u64 = setup.contents.iter().map(|c| c.len() as u64).sum();
	assert_eq!(freed, total);
	let mut deleted = setup.backend.log().deleted_files.clone();
	deleted.sort();
	deleted.dedup();
	assert_eq!(deleted.len(), 3);
}

fn run_permanent_disposal(setup: &Setup, targets: Vec<DisposalTarget>, control: JobControl) -> Job {
	run_disposal(setup, SourceDisposal::DeletePermanently, targets, control)
}

fn run_disposal(
	setup: &Setup,
	how: SourceDisposal,
	targets: Vec<DisposalTarget>,
	control: JobControl,
) -> Job {
	let hashed = vec![true; targets.len()];
	let job = CompressJob {
		format: CompressFormat::Tar { compression: None },
		entries: setup.entries.clone(),
		password: None,
	};
	start_disposing(
		setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		control,
		None,
		Box::new(move || worker::start(move |port| compress(&port, job))),
		Some(CompressDisposal {
			how,
			targets,
			hashed,
		}),
		CompressReport::default(),
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_a_partial_removal_did_not_reach_is_kept() {
	// a.txt comes last in the folder's (uuid) order, and the removal stops at the second file
	let setup = loop {
		let setup = setup(|backend, files| {
			backend
				.fail_deletes_of
				.insert(files[1].uuid().max(files[2].uuid()));
		});
		let ids: Vec<Uuid> = setup.sources.iter().map(|(_, file)| file.uuid()).collect();
		if ids[0] > ids[1] && ids[0] > ids[2] {
			break setup;
		}
	};
	let [a, big, top] = [0, 1, 2].map(|source| setup.sources[source].1.clone());
	let docs = Uuid::new_v4();
	setup.backend.place_dir(docs, Uuid::new_v4());
	for file in [&a, &big, &top] {
		setup.backend.place_file(file.uuid(), docs, file.size());
	}
	let targets = vec![
		DisposalTarget::Dir {
			uuid: docs,
			read: Tree {
				files: [&a, &big, &top]
					.iter()
					.map(|file| (file.uuid(), file.size()))
					.collect(),
				dirs: Default::default(),
			},
		},
		DisposalTarget::File(ExpectedFile::of(&a, a.uuid(), docs)),
	];
	let job = run_permanent_disposal(&setup, targets, JobControl::default());
	let report = job.running.await.unwrap().unwrap();
	let log = setup.backend.log();
	assert_eq!(log.deleted_files, vec![big.uuid().min(top.uuid())]);
	assert!(log.file_parents.contains_key(&a.uuid()));
	assert!(
		matches!(
			report.dispositions[1].outcome,
			DisposalOutcome::Kept { bytes_freed: 0, .. }
		),
		"{:?}",
		report.dispositions
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_a_partial_removal_took_is_reported_removed() {
	// a.txt comes first in the folder's (uuid) order, and the removal stops at the last file
	let setup = loop {
		let setup = setup(|backend, files| {
			backend
				.fail_deletes_of
				.insert(files[1].uuid().max(files[2].uuid()));
		});
		let ids: Vec<Uuid> = setup.sources.iter().map(|(_, file)| file.uuid()).collect();
		if ids[0] < ids[1] && ids[0] < ids[2] {
			break setup;
		}
	};
	let [a, big, top] = [0, 1, 2].map(|source| setup.sources[source].1.clone());
	let docs = Uuid::new_v4();
	setup.backend.place_dir(docs, Uuid::new_v4());
	for file in [&a, &big, &top] {
		setup.backend.place_file(file.uuid(), docs, file.size());
	}
	let targets = vec![
		DisposalTarget::Dir {
			uuid: docs,
			read: Tree {
				files: [&a, &big, &top]
					.iter()
					.map(|file| (file.uuid(), file.size()))
					.collect(),
				dirs: Default::default(),
			},
		},
		DisposalTarget::File(ExpectedFile::of(&a, a.uuid(), docs)),
	];
	let job = run_permanent_disposal(&setup, targets, JobControl::default());
	let report = job.running.await.unwrap().unwrap();
	assert!(setup.backend.log().deleted_files.contains(&a.uuid()));
	assert!(
		matches!(
			report.dispositions[0].outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Failed { .. },
				bytes_freed
			} if bytes_freed > 0
		),
		"{:?}",
		report.dispositions
	);
	assert!(
		matches!(
			report.dispositions[1].outcome,
			DisposalOutcome::Disposed {
				how: SourceDisposal::DeletePermanently,
				bytes_freed: 0
			}
		),
		"{:?}",
		report.dispositions
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folders_whose_reads_hold_each_other_are_kept() {
	let setup = setup(|_, _| {});
	let [x, y] = [Uuid::new_v4(), Uuid::new_v4()];
	let holding = |other| DisposalTarget::Dir {
		uuid: if other == y { x } else { y },
		read: Tree {
			files: Default::default(),
			dirs: [other].into(),
		},
	};
	let targets = vec![holding(y), holding(x)];
	let job = run_permanent_disposal(&setup, targets, JobControl::default());
	let report = tokio::time::timeout(Duration::from_secs(30), job.running)
		.await
		.expect("the disposal ends")
		.unwrap()
		.unwrap();
	assert_eq!(report.dispositions.len(), 2);
	for disposition in &report.dispositions {
		assert!(
			matches!(
				disposition.outcome,
				DisposalOutcome::Kept {
					reason: KeptReason::Changed,
					bytes_freed: 0
				}
			),
			"{disposition:?}"
		);
	}
	assert!(setup.backend.log().deleted_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_early_end_tells_the_callback_of_every_kept_source() {
	let setup = setup(|backend, _| {
		backend
			.fail_fetch
			.insert("big.bin".to_owned(), ErrorKind::Server);
	});
	let placed = place(&setup);
	let targets = targets(&setup, &placed);
	let job = run_permanent_disposal(&setup, targets, JobControl::default());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.report.dispositions.len(), 2);
	let told = job
		.recorder
		.events()
		.into_iter()
		.filter(|event| matches!(event, CompressEvent::SourceDisposition(_)))
		.count();
	assert_eq!(told, 2);
}

/// The dispositions the callback was told of, in order.
fn told(recorder: &Recorder) -> Vec<SourceDisposition> {
	recorder
		.events()
		.into_iter()
		.filter_map(|event| match event {
			CompressEvent::SourceDisposition(disposition) => Some(disposition),
			_ => None,
		})
		.collect()
}

fn hold(setup: &Setup, uuid: Uuid) {
	setup.backend.held.send_modify(|held| {
		held.insert(uuid);
	});
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_dropped_past_its_cancel_grace_has_told_of_what_it_removed() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	let top = setup.sources[2].1.uuid();
	// the folder goes first; the top file's deletion is sent and never answered
	hold(&setup, top);
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	wait_until("the top file's deletion is sent", || {
		setup.backend.log().held.contains(&top)
	})
	.await;
	cancel.send_replace(true);
	// the bindings drop a job that has not ended within its cancel grace
	job.running.abort();
	assert!(job.running.await.unwrap_err().is_cancelled());
	let told = told(&job.recorder);
	assert_eq!(told.len(), 1, "{told:?}");
	assert_eq!(told[0].uuid, placed.docs);
	let folder_bytes = setup.contents[0].len() + setup.contents[1].len();
	assert!(
		matches!(
			told[0].outcome,
			DisposalOutcome::Disposed {
				how: SourceDisposal::DeletePermanently,
				bytes_freed,
			} if bytes_freed == folder_bytes as u64
		),
		"{told:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_drops_a_listing_in_flight_and_keeps_the_source() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	// the top file goes first; the folder's listing never answers
	let targets = targets(&setup, &placed).into_iter().rev().collect();
	hold(&setup, placed.docs);
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets, control);
	wait_until("the folder is being listed", || {
		setup.backend.log().held.contains(&placed.docs)
	})
	.await;
	cancel.send_replace(true);
	let report = tokio::time::timeout(Duration::from_secs(30), job.running)
		.await
		.expect("the cancel ends the job")
		.unwrap()
		.unwrap();
	let top = &setup.sources[2].1;
	let [removed, kept] = &report.dispositions[..] else {
		panic!("{:?}", report.dispositions);
	};
	assert_eq!(removed.uuid, top.uuid());
	assert!(
		matches!(
			removed.outcome,
			DisposalOutcome::Disposed { bytes_freed, .. } if bytes_freed == top.size()
		),
		"{removed:?}"
	);
	assert_eq!(kept.uuid, placed.docs);
	assert!(
		matches!(
			kept.outcome,
			DisposalOutcome::Kept {
				reason: KeptReason::Interrupted,
				bytes_freed: 0,
			}
		),
		"{kept:?}"
	);
	assert_eq!(told(&job.recorder).len(), 2);
	assert_eq!(setup.backend.log().deleted_files, [top.uuid()]);
	// the archive exists, so the job is done; it still ends as a cancelled one winds down
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Done, RunState::Cancelling)
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_during_a_folders_removal_says_what_it_deleted() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	// the folder's files are deleted in uuid order: the cancel comes while the first is
	let (_, first) = setup.sources[..2]
		.iter()
		.min_by_key(|(_, file)| file.uuid())
		.unwrap();
	hold(&setup, first.uuid());
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	wait_until("the first file's deletion is sent", || {
		setup.backend.log().held.contains(&first.uuid())
	})
	.await;
	cancel.send_replace(true);
	setup.backend.held.send_modify(|held| held.clear());
	let report = job.running.await.unwrap().unwrap();
	let outcomes = outcomes(&report);
	assert!(
		matches!(
			outcomes[..],
			[
				DisposalOutcome::Kept {
					reason: KeptReason::Interrupted,
					bytes_freed,
				},
				DisposalOutcome::Kept {
					reason: KeptReason::Interrupted,
					bytes_freed: 0,
				},
			] if bytes_freed == first.size()
		),
		"{outcomes:?}"
	);
	assert_eq!(setup.backend.log().deleted_files, [first.uuid()]);
	assert!(setup.backend.log().trashed_dirs.is_empty());
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_compressing_keeps_every_source() {
	let setup = setup(|backend, _| {
		backend.blocked_uploads.insert("b.tar".to_owned());
	});
	let placed = place(&setup);
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	let total: u64 = setup.contents.iter().map(|c| c.len() as u64).sum();
	// every source is read, and the archive's first chunk is stuck uploading
	wait_until("every source is read", || {
		job.reporter.counts().bytes_read == total
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	let expected = [placed.docs, setup.sources[2].1.uuid()];
	for dispositions in [failed.report.dispositions, told(&job.recorder)] {
		let uuids: Vec<Uuid> = dispositions.iter().map(|d| d.uuid).collect();
		assert_eq!(uuids, expected);
		for disposition in dispositions {
			assert!(
				matches!(
					disposition.outcome,
					DisposalOutcome::Kept {
						reason: KeptReason::Interrupted,
						bytes_freed: 0,
					}
				),
				"{disposition:?}"
			);
		}
	}
	assert!(setup.backend.log().deleted_files.is_empty());
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Cancelled, RunState::Cancelling)
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_before_the_archive_is_named_ends_cancelling() {
	// the destination's targets, fetched while the archive is named, never come
	let setup = setup(|backend, _| backend.targets_delay = Duration::from_secs(3600));
	let placed = place(&setup);
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	wait_until("the archive is being named", || {
		setup.backend.log().target_fetches == 1
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Cancelled, RunState::Cancelling)
	);
	assert_eq!(told(&job.recorder).len(), 2);
	assert!(setup.backend.log().uploaded.is_empty());
}

/// The run states the callback saw, each change once; a pause may be complete before an update
/// shows it pausing, so pausing is left out.
fn run_states(recorder: &Recorder) -> Vec<RunState> {
	let mut states: Vec<RunState> = recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.map(|update| update.run_state)
		.filter(|state| *state != RunState::Pausing)
		.collect();
	states.dedup();
	states
}

/// Plays a codec that reads the first source and the first chunk of the second, and then keeps
/// reading that chunk.
async fn read_into_the_second_source(events: &mpsc::Sender<WorkerEvent>) {
	for (source, index) in [(0, 0), (1, 0)] {
		let (reply, answer) = oneshot::channel();
		events
			.send(WorkerEvent::Ask {
				source,
				index,
				reply,
			})
			.await
			.unwrap();
		answer.await.unwrap().unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_compress_holds_nothing_of_the_clients_budget() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let (pause, cancel, control) = controls();
	let job = start_with(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		control,
		None,
		Box::new(move || Ok(link)),
	);
	// the first source's chunk takes the job's own slot; the chunk being read and the two
	// fetched ahead of it take the client's budget
	read_into_the_second_source(&events).await;
	let budget = setup.backend.budget;
	wait_until("two chunks are fetched ahead", || {
		setup.backend.memory.available_permits() == budget - 3 * CHUNK_BYTES
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_released(&setup, &job.reporter);
	assert_eq!(job.recorder.last().run_state, RunState::Paused);
	let free = job.recorder.free_when_paused.lock().unwrap().clone();
	assert!(
		!free.is_empty() && free.iter().all(|&free| free == budget),
		"no update reads paused while the budget is held: {free:?}"
	);

	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		run_states(&job.recorder),
		[RunState::Running, RunState::Paused, RunState::Cancelling]
	);
	assert!(setup.backend.log().finished.is_empty());
	assert_released(&setup, &job.reporter);
	drop((events, result));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_compress_writes_the_archive_it_would_have() {
	let setup_paused = setup(|backend, _| {
		// the job is still reading when the pause comes
		backend
			.slow
			.insert("big.bin".to_owned(), Duration::from_millis(300));
	});
	let (pause, _cancel, control) = controls();
	let format = CompressFormat::Tar { compression: None };
	let job = start(&setup_paused, "b.tar", format, control, None);
	wait_until("the first source is read", || {
		job.reporter.counts().bytes_read > 0
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_released(&setup_paused, &job.reporter);
	assert!(setup_paused.backend.log().finished.is_empty());
	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	let paused = uploaded(&setup_paused, report.archive.unwrap().uuid());
	assert_eq!(
		run_states(&job.recorder),
		[RunState::Running, RunState::Paused, RunState::Running]
	);

	let setup_straight = setup(|_, _| {});
	let job = start(
		&setup_straight,
		"b.tar",
		format,
		JobControl::default(),
		None,
	);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(
		paused,
		uploaded(&setup_straight, report.archive.unwrap().uuid()),
		"a pause changes nothing in the archive"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_registering_holds_the_disposal_back() {
	let setup = setup(|backend, _| {
		backend
			.slow_finish
			.insert("b.tar".to_owned(), Duration::from_millis(300));
	});
	let placed = place(&setup);
	let (pause, _cancel, control) = controls();
	let job = run_disposal(
		&setup,
		SourceDisposal::Trash,
		targets(&setup, &placed),
		control,
	);
	wait_until("the archive is being registered", || {
		!setup.backend.log().finishing.is_empty()
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_eq!(
		job.recorder.created.lock().unwrap().len(),
		1,
		"the registration under way finished"
	);
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::DisposingSources, RunState::Paused)
	);
	assert!(setup.backend.log().trashed_files.is_empty());
	assert_released(&setup, &job.reporter);

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert!(
		outcomes(&report)
			.iter()
			.all(|outcome| matches!(outcome, DisposalOutcome::Disposed { .. })),
		"{:?}",
		report.dispositions
	);
}
