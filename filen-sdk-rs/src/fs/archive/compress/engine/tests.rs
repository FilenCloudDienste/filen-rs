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
use tokio::task::JoinHandle;

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasName,
		archive::{
			compress::{
				CompressFormat, CompressUpdate,
				codec::{ArchiveEntry, CompressJob, compress},
				report::CompressCallback,
			},
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			decode::open_stream,
			dispose::{DisposalOutcome, ExpectedFile, KeptReason, SourceDisposal, Tree},
			encode::Compression,
			format::StreamCodec,
			sevenz::write::SevenZMethod,
			tar_iter::TarReader,
			worker,
		},
		dir::RootDirectory,
		drive_job::{backend::ListedNames, test_support::FakeBackend},
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
}

impl CompressCallback for Recorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		self.created.lock().unwrap().push(archive);
	}

	fn on_update(&self, update: CompressUpdate) {
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
	let recorder = Arc::new(Recorder::default());
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
		head_last: matches!(format, CompressFormat::SevenZ { .. }),
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
	tokio::time::sleep(Duration::from_millis(200)).await;
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
