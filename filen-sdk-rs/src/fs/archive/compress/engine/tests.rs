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
				read_back::ReadBack,
				report::CompressCallback,
			},
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			decode::open_stream,
			dispose::{DisposalOutcome, ExpectedFile, KeptReason, SourceDisposal, Tree},
			encode::Compression,
			format::StreamCodec,
			password::ArchivePassword,
			sevenz::write::{SevenZEncryption, SevenZMethod},
			tar_iter::TarReader,
			worker,
			zip::{crypto::AesStrength, write::ZipMethod},
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			test_support::{FakeBackend, wait_until},
		},
		file::{
			AnonymousRemoteFile, RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
	},
	job::{report::JobState, test_support::controls},
};

#[derive(Default)]
struct Recorder {
	created: Mutex<Vec<RemoteFile>>,
	updates: Mutex<Vec<CompressUpdate>>,
	/// The client's memory budget, to see what an update reading paused was sent with.
	memory: Option<Arc<Semaphore>>,
	/// The budget's free permits at each update reading paused.
	free_when_paused: Mutex<Vec<usize>>,
	/// What to hold in this drive once the archive is registered, the sources having been
	/// read: these items, and the archive's own chunks if asked.
	hold_when_created: Option<(Arc<FakeBackend>, Vec<Uuid>, bool)>,
}

impl CompressCallback for Recorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		if let Some((backend, items, and_archive)) = &self.hold_when_created {
			backend.held.send_modify(|held| {
				held.extend(items);
				if *and_archive {
					held.insert(archive.uuid());
				}
			});
		}
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
	(0..len).map(|i| (i % 241) as u8 ^ seed).collect()
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
	/// The archive's password, which reading it back takes too.
	password: Option<ArchivePassword>,
	/// Reading the archive back waits from its first chunk until the test releases it.
	hold_archive: bool,
	/// Items held once the archive is registered (a source's fetches are never held).
	hold_after_registering: Vec<Uuid>,
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
	backend.keep_uploads = true;
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
				source: source as u32,
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
		password: None,
		hold_archive: false,
		hold_after_registering: Vec::new(),
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
		hold_when_created: Some((
			Arc::clone(&setup.backend),
			setup.hold_after_registering.clone(),
			setup.hold_archive,
		)),
		..Recorder::default()
	});
	let reporter = Reporter::new(Arc::clone(&recorder));
	let extension_len = format.check_name(name).unwrap();
	let config = ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY);
	let read_back = disposal
		.as_ref()
		.map(|_| ReadBack::as_extracting(&setup.entries, &config, setup.password.clone()));
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
		config,
		head_last: matches!(format, CompressFormat::SevenZ { .. }),
		start,
		report,
		disposal,
		read_back,
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
	// each file with data was the active one while it was read, named as in the archive
	let mut active: Vec<(String, String)> = job
		.recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.flat_map(|update| update.active.clone())
		.map(|file| (file.name, file.path))
		.collect();
	active.dedup();
	assert!(
		active
			.iter()
			.all(|(name, path)| path.ends_with(name.as_str())),
		"{active:?}"
	);
	assert!(job.recorder.last().active.is_empty());
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
	let setup_gone = setup(|backend, _| backend.forget_registered = true);
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

/// The entries of a 7z read back through the SDK's reader, every CRC-32 checked.
fn sevenz_entries(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
	use crate::fs::archive::sevenz::read::{FolderCursor, Keys, SevenZLimits, read_index};
	let limits = SevenZLimits {
		max_index_bytes: 1 << 20,
		max_entries: 100,
		decoder_memory: 64 << 20,
	};
	let mut keys = Keys::new(None);
	let mut source = std::io::Cursor::new(archive);
	let index = read_index(&mut source, archive.len() as u64, limits, &mut keys).unwrap();
	assert_eq!(index.unaccounted_bytes, 0);
	let mut cursor = FolderCursor::new(source, limits.decoder_memory);
	index
		.entries
		.iter()
		.map(|entry| {
			let mut data = Vec::new();
			if entry.stream.is_some() {
				cursor
					.open(&index, entry, &mut keys)
					.unwrap()
					.read_to_end(&mut data)
					.unwrap();
			}
			(entry.name.clone(), data)
		})
		.collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_uploads_its_first_chunk_last() {
	for (method, solid) in [
		(SevenZMethod::Copy, false),
		(SevenZMethod::Lzma2 { level: 1 }, true),
	] {
		let setup = setup(|_, _| {});
		let format = CompressFormat::SevenZ {
			method,
			solid,
			encryption: None,
		};
		let job = start(&setup, "bundle.7z", format, JobControl::default(), None);
		let report = job.running.await.unwrap().unwrap();
		let archive = report.archive.as_ref().expect("the archive is registered");
		let (_, completion) = setup.backend.log().finished[&archive.uuid()].clone();
		let bytes = uploaded(&setup, archive.uuid());
		assert_eq!(completion.written, bytes.len() as u64, "{method:?}");
		assert_eq!(
			completion.num_chunks,
			(bytes.len() as u64).div_ceil(CHUNK_SIZE as u64),
			"{method:?}"
		);
		// the hash merged around the late first chunk is the whole archive's
		assert_eq!(
			completion.hash,
			Blake3Hash::from(blake3::hash(&bytes)),
			"{method:?}"
		);
		let order: Vec<u64> = setup
			.backend
			.log()
			.uploaded
			.iter()
			.filter(|(file, _)| *file == archive.uuid())
			.map(|(_, index)| *index)
			.collect();
		assert_eq!(order.last(), Some(&0), "{method:?}: chunk 0 goes up last");
		assert_eq!(
			sevenz_entries(&bytes),
			[
				("docs/a.txt".to_owned(), setup.contents[0].clone()),
				("docs/big.bin".to_owned(), setup.contents[1].clone()),
				("top.txt".to_owned(), setup.contents[2].clone()),
				("docs".to_owned(), Vec::new()),
			],
			"{method:?}"
		);
		assert_eq!(report.counts.files_done, 3);
		assert_released(&setup, &job.reporter);
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
	let mut setup = setup(|_, _| {});
	let placed = place(&setup);
	let top = setup.sources[2].1.uuid();
	// the folder goes first; the top file's deletion is sent and never answered
	setup.hold_after_registering.push(top);
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
	let mut setup = setup(|_, _| {});
	let placed = place(&setup);
	// the folder's files are deleted in uuid order: the cancel comes while the first is
	let (_, first) = setup.sources[..2]
		.iter()
		.min_by_key(|(_, file)| file.uuid())
		.unwrap();
	let first = first.clone();
	setup.hold_after_registering.push(first.uuid());
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
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	let (_pause, cancel, control) = controls();
	cancel.send_replace(true);
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
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

/// The phases the callback saw, each change once.
fn phases(recorder: &Recorder) -> Vec<CompressPhase> {
	let mut phases: Vec<CompressPhase> = recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.map(|update| update.phase)
		.collect();
	phases.dedup();
	phases
}

/// Compresses the setup into `name` in `format`, removing the sources for good.
fn dispose_permanently_as(setup: &Setup, name: &str, format: CompressFormat) -> Job {
	let placed = place(setup);
	let job = CompressJob {
		format,
		entries: setup.entries.clone(),
		password: setup.password.clone(),
	};
	start_disposing(
		setup,
		name,
		format,
		JobControl::default(),
		None,
		Box::new(move || worker::start(move |port| compress(&port, job))),
		Some(CompressDisposal {
			how: SourceDisposal::DeletePermanently,
			targets: targets(setup, &placed),
			hashed: vec![true; 2],
		}),
		CompressReport::default(),
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_removal_reads_the_archive_back_first() {
	let password = || Some(ArchivePassword::new("hunter2".to_owned()).unwrap());
	for (name, format, password) in [
		("b.tar.gz", gzip_tar(), None),
		(
			"b.zip",
			CompressFormat::Zip {
				method: ZipMethod::Deflate { level: 6 },
				encryption: Some(AesStrength::Aes256),
			},
			password(),
		),
		(
			"b.7z",
			CompressFormat::SevenZ {
				method: SevenZMethod::Lzma2 { level: 1 },
				solid: true,
				encryption: Some(SevenZEncryption::EntriesAndHeaders),
			},
			password(),
		),
	] {
		let mut setup = setup(|_, _| {});
		setup.password = password;
		let job = dispose_permanently_as(&setup, name, format);
		let report = job.running.await.unwrap().unwrap();
		let archive = report.archive.as_ref().unwrap().uuid();
		assert!(
			outcomes(&report)
				.iter()
				.all(|outcome| matches!(outcome, DisposalOutcome::Disposed { .. })),
			"{name}: {:?}",
			report.dispositions
		);
		assert!(
			setup
				.backend
				.log()
				.fetched
				.iter()
				.any(|(file, _)| *file == archive),
			"{name}: the archive is read back"
		);
		let phases = phases(&job.recorder);
		assert!(
			phases.ends_with(&[
				CompressPhase::Finishing,
				CompressPhase::Verifying,
				CompressPhase::DisposingSources,
				CompressPhase::Done,
			]),
			"{name}: {phases:?}"
		);
		assert_released(&setup, &job.reporter);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sources_are_kept_when_the_archive_does_not_read_back_as_them() {
	// the archive a codec writes for the same sources, with one byte of a.txt's data off
	let straight = setup(|_, _| {});
	let job = start(
		&straight,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
	);
	let report = job.running.await.unwrap().unwrap();
	let mut damaged = uploaded(&straight, report.archive.unwrap().uuid());
	// the directory's header, then a.txt's, then its data
	let data = 2 * 512;
	assert_eq!(&damaged[data..data + 5], b"alpha");
	damaged[data] ^= 1;

	let setup = setup(|_, _| {});
	let placed = place(&setup);
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_disposing(
		&setup,
		"b.tar",
		CompressFormat::Tar { compression: None },
		JobControl::default(),
		None,
		Box::new(move || Ok(link)),
		Some(CompressDisposal {
			how: SourceDisposal::DeletePermanently,
			targets: targets(&setup, &placed),
			hashed: vec![true; 2],
		}),
		CompressReport::default(),
	);
	// a codec that reads every source whole and writes the damaged archive
	for (source, index) in [(0, 0), (1, 0), (1, 1), (2, 0)] {
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
		events.send(WorkerEvent::FileEnd).await.ok();
	}
	for chunk in damaged.chunks(CHUNK_SIZE) {
		events
			.send(WorkerEvent::Data(chunk.to_vec()))
			.await
			.unwrap();
	}
	drop(events);
	result.send(Ok(damaged.len() as u64)).unwrap();

	let report = job.running.await.unwrap().unwrap();
	assert!(report.archive.is_some(), "the archive stays");
	all_kept_for(&report, |reason| matches!(reason, KeptReason::Unconfirmed));
	assert!(setup.backend.log().deleted_files.is_empty());
	assert!(setup.backend.log().trashed_dirs.is_empty());
	assert_released(&setup, &job.reporter);
}

/// Starts a permanent disposal of the setup whose archive's read back waits at its first chunk,
/// and waits until it does; the archive's uuid.
async fn held_at_the_read_back(setup: &mut Setup, control: JobControl) -> (Job, Uuid) {
	setup.hold_archive = true;
	let placed = place(setup);
	let job = run_permanent_disposal(setup, targets(setup, &placed), control);
	wait_until("the archive's first chunk is asked for", || {
		!setup.backend.log().held.is_empty()
	})
	.await;
	let archive = setup.backend.log().held[0];
	assert_eq!(job.recorder.last().phase, CompressPhase::Verifying);
	(job, archive)
}

fn archive_fetches(setup: &Setup, archive: Uuid) -> usize {
	setup
		.backend
		.log()
		.fetched
		.iter()
		.filter(|(file, _)| *file == archive)
		.count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_reading_the_archive_back_keeps_the_sources() {
	let mut setup = setup(|_, _| {});
	let (_pause, cancel, control) = controls();
	let (job, archive) = held_at_the_read_back(&mut setup, control).await;
	cancel.send_replace(true);
	// the fetch never answers: only dropping it ends the job
	let report = tokio::time::timeout(Duration::from_secs(30), job.running)
		.await
		.expect("the cancel ends the read back")
		.unwrap()
		.unwrap();
	all_kept_for(&report, |reason| matches!(reason, KeptReason::Interrupted));
	assert_eq!(told(&job.recorder).len(), 2);
	setup.backend.held.send_modify(|held| held.clear());
	assert_eq!(
		archive_fetches(&setup, archive),
		0,
		"nothing reads the archive once the job ended"
	);
	assert!(setup.backend.log().deleted_files.is_empty());
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Done, RunState::Cancelling)
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_read_back_holds_nothing_and_reports_its_progress() {
	let mut setup = setup(|_, _| {});
	let (pause, _cancel, control) = controls();
	let (job, archive) = held_at_the_read_back(&mut setup, control).await;
	pause.send_replace(true);
	// the fetch in flight finishes; the pause is seen when the reader asks again
	setup.backend.held.send_modify(|held| held.clear());
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Verifying, RunState::Paused)
	);
	assert_released(&setup, &job.reporter);
	let fetched = archive_fetches(&setup, archive);
	assert_eq!(fetched, 1, "the first chunk is read, the second waits");
	assert_eq!(
		last.counts.bytes_verified, CHUNK_SIZE as u64,
		"the read back reports its progress"
	);
	// the plan's totals, which the engine is handed with its report
	let sources: u64 = setup.contents.iter().map(|c| c.len() as u64).sum();
	let totals = crate::fs::drive_job::plan::PlanTotals {
		dirs: 1,
		files: 3,
		bytes: sources,
	};
	job.reporter.set_plan(totals, &[], &[]);
	let units = job.reporter.read(|state| state.progress().units);
	assert_eq!(
		units.total - units.settled,
		setup.backend.log().finished[&archive].1.written - CHUNK_SIZE as u64,
		"what is left of the archive to read is the work left"
	);

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert!(
		outcomes(&report)
			.iter()
			.all(|outcome| matches!(outcome, DisposalOutcome::Disposed { .. })),
		"{:?}",
		report.dispositions
	);
	assert_eq!(report.counts.bytes_verified, report.counts.bytes_done);
	assert_released(&setup, &job.reporter);
}
