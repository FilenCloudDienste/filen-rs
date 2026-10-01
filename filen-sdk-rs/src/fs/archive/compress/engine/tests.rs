//! The driver against the fake drive, with the real codec on its thread; the archive the fake
//! received is put back together and read with the SDK's own decoders.

use std::{
	borrow::Cow,
	collections::BTreeMap,
	io::Read,
	sync::{Mutex, atomic::Ordering},
	time::Duration,
};

use rand::{Rng, SeedableRng, rngs::StdRng};
use tokio::{
	sync::{Semaphore, mpsc},
	task::JoinHandle,
};

use super::*;
use crate::{
	auth::http::ClientConfig,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FULL_CHUNK_BYTES},
	fs::{
		HasName,
		archive::{
			compress::{
				CheckedFormat, CompressFormat, CompressUpdate,
				codec::{ArchiveEntry, CompressJob, compress},
				read_back::{ReadBack, ReadBackResult, StartReadBack},
				report::CompressCallback,
			},
			decode::open_stream,
			dispose::{
				DisposalOutcome, ExpectedDir, ExpectedFile, KeptReason, SourceDisposal, Tree,
			},
			encode::Compression,
			entry_path::entry_path,
			extract::{PasswordCheck, codec::ArchiveEnd},
			format::{ArchiveFormat, StreamCodec},
			password::ArchivePassword,
			sevenz::{
				read::{FolderCursor, Keys, read_index},
				write::{SevenZEncryption, SevenZMethod},
			},
			tar_iter::{TAR_BLOCK, TarReader},
			test_support::{
				READ_BACK_MEMORY, READ_BACK_SEVEN_Z, archive_password, hash, pattern, tar_members,
			},
			worker::{self, ARCHIVE_STALL_TIMEOUT, EntryHead, EntryKind},
			zip::{crypto::AesStrength, write::ZipMethod},
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			plan::{PlanTotals, SkipReason, SkippedEntry},
			test_support::{FakeBackend, Quirk, Request, remote_file, wait_until},
		},
		file::RemoteFile,
	},
	job::{
		report::{JobState, RunState},
		test_support::{controls, settled_run_states},
	},
};

#[derive(Default)]
struct Recorder {
	created: Mutex<Vec<RemoteFile>>,
	updates: Mutex<Vec<CompressUpdate>>,
	/// The client's memory budget, to see what an update reading paused was sent with.
	memory: Option<Arc<Semaphore>>,
	/// The budget's free permits at each update reading paused.
	free_when_paused: Mutex<Vec<usize>>,
	/// What to hold once the archive is registered, the sources having been read.
	hold_when_created: Option<HoldWhenCreated>,
}

/// Requests a [`Recorder`] holds in a drive once the archive is registered.
struct HoldWhenCreated {
	backend: Arc<FakeBackend>,
	requests: Vec<(Request, Uuid)>,
	/// Fetching the archive's own chunks too.
	archive_fetches: bool,
}

impl CompressCallback for Recorder {
	fn on_archive_created(&self, archive: RemoteFile) {
		if let Some(hold) = &self.hold_when_created {
			for &(request, uuid) in &hold.requests {
				hold.backend.hold_requests(request, [uuid]);
			}
			if hold.archive_fetches {
				hold.backend.hold_requests(Request::Fetch, [archive.uuid()]);
			}
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

	/// The run states the callback saw, each change once; a pause may be complete before an
	/// update shows it pausing, so pausing is left out.
	fn run_states(&self) -> Vec<RunState> {
		let updates = self.updates.lock().unwrap();
		settled_run_states(updates.iter().map(|update| update.run_state))
	}
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
	/// Requests held once the archive is registered (a source's fetches are never held).
	hold_after_registering: Vec<(Request, Uuid)>,
	/// The archive settings the job runs under, whose slots jobs sharing them share.
	config: ArchiveConfig,
	/// Reads the archive back in place of the extracting codec, for the first job that does.
	reader: Mutex<Option<StartReadBack>>,
}

/// The job's destination, and the folder the sources were read from.
const DESTINATION: Uuid = Uuid::from_u128(0xD0);
const SOURCES_PARENT: Uuid = Uuid::from_u128(0x50);

/// The sources' uuids, `a.txt`'s, `big.bin`'s and `top.txt`'s, in the order they sort in.
const SOURCE_UUIDS: [Uuid; 3] = [
	Uuid::from_u128(0x5A),
	Uuid::from_u128(0x5B),
	Uuid::from_u128(0x5C),
];

fn setup(configure: impl FnOnce(&mut FakeBackend, &[RemoteFileType<'static>])) -> Setup {
	setup_with(SOURCE_UUIDS, configure)
}

/// A [`Setup`] whose sources have `uuids`.
fn setup_with(
	uuids: [Uuid; 3],
	configure: impl FnOnce(&mut FakeBackend, &[RemoteFileType<'static>]),
) -> Setup {
	let destination = DESTINATION;
	let contents = vec![
		b"alpha".to_vec(),
		pattern(CHUNK_SIZE + 77, 3),
		b"top".to_vec(),
	];
	let paths = ["docs/a.txt", "docs/big.bin", "top.txt"];
	let files: Vec<RemoteFileType<'static>> = contents
		.iter()
		.zip(paths)
		.zip(uuids)
		.map(|((data, path), uuid)| {
			let name = path.rsplit('/').next().unwrap();
			remote_file(uuid, SOURCES_PARENT, name, data, Some(hash(data)))
		})
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
		password: None,
		hold_archive: false,
		hold_after_registering: Vec::new(),
		config: ArchiveConfig::new(&ClientConfig::default()),
		reader: Mutex::new(None),
	}
}

struct Job {
	running: JoinHandle<Result<CompressReport, CompressFailed>>,
	recorder: Arc<Recorder>,
	reporter: MaybeArc<Reporter>,
}

/// A job a test starts: the archive's name and format, and whatever else the test sets.
struct Run {
	name: &'static str,
	format: CompressFormat,
	control: JobControl,
	max_bytes: Option<u64>,
	disposal: Option<(SourceDisposal, Vec<DisposalSource>)>,
	report: CompressReport,
	/// Starts in place of the real codec compressing the setup's entries in `format`.
	codec: Option<CodecStart<CodecResult>>,
}

impl Run {
	fn new(name: &'static str, format: CompressFormat) -> Self {
		Self {
			name,
			format,
			control: JobControl::default(),
			max_bytes: None,
			disposal: None,
			report: CompressReport::default(),
			codec: None,
		}
	}

	/// An uncompressed `b.tar`.
	fn tar() -> Self {
		Self::new("b.tar", CompressFormat::Tar { compression: None })
	}
}

fn start(setup: &Setup, run: Run) -> Job {
	let Run {
		name,
		format,
		control,
		max_bytes,
		disposal,
		report,
		codec,
	} = run;
	let recorder = Arc::new(Recorder {
		memory: Some(Arc::clone(&setup.backend.memory)),
		hold_when_created: Some(HoldWhenCreated {
			backend: Arc::clone(&setup.backend),
			requests: setup.hold_after_registering.clone(),
			archive_fetches: setup.hold_archive,
		}),
		..Recorder::default()
	});
	let reporter = Reporter::new(Arc::clone(&recorder));
	let extension_len = format.check_name(name).unwrap();
	let config = setup.config.clone();
	let written = job(format, setup.entries.clone(), setup.password.clone());
	let disposal = disposal.map(|(how, sources)| CompressDisposal {
		removal: match how {
			SourceDisposal::Trash => Removal::Trash,
			SourceDisposal::DeletePermanently => {
				let mut read_back = ReadBack::as_extracting(&written, &config);
				if let Some(start) = setup.reader.lock().unwrap().take() {
					read_back.start = start;
				}
				Removal::DeletePermanently { read_back }
			}
		},
		sources,
	});
	let start = codec
		.unwrap_or_else(|| Box::new(move || worker::start(move |port| compress(&port, written))));
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
	}));
	Job {
		running,
		recorder,
		reporter,
	}
}

/// The codec's job for an archive of `entries` in `format`, checked with `password`.
fn job(
	format: CompressFormat,
	entries: Vec<ArchiveEntry>,
	password: Option<ArchivePassword>,
) -> CompressJob {
	let CheckedFormat::Archive(checked) = format.check(password).unwrap() else {
		panic!("{format:?} holds entries");
	};
	CompressJob::Archive {
		format: checked,
		entries,
	}
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
		Some(codec) => Box::new(open_stream(codec, archive, READ_BACK_MEMORY).unwrap()),
	};
	tar_members(&mut TarReader::new(reader, 100))
		.into_iter()
		.map(|(member, data)| (member.path, data))
		.collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compresses_the_sources_into_one_new_file() {
	let setup = setup(|backend, _| {
		backend.listed = ListedNames {
			names: vec!["bundle.tar.gz".into()],
			unverified: false,
		};
	});
	let job = start(&setup, Run::new("bundle.tar.gz", gzip_tar()));
	let report = job.running.await.unwrap().unwrap();
	let archive = report.archive.as_ref().unwrap();

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
	assert_eq!(report.counts.archive_bytes, bytes.len() as u64);
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
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_does_not_match_its_hash_is_reported() {
	let setup = setup(|backend, files| {
		// the fake serves other bytes than the ones the metadata's hash is of
		backend.contents.insert(files[2].uuid(), b"TOP".to_vec());
	});
	let job = start(&setup, Run::tar());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(
		report
			.hash_mismatches
			.iter()
			.map(|mismatch| mismatch.path.as_str())
			.collect::<Vec<_>>(),
		["top.txt"]
	);
	assert_eq!(report.omitted_hash_mismatches, 0);
	let mismatched: Vec<String> = job
		.recorder
		.events()
		.into_iter()
		.filter_map(|event| match event {
			CompressEvent::SourceHashMismatch(mismatch) => Some(mismatch.path),
			_ => None,
		})
		.collect();
	assert_eq!(mismatched, ["top.txt"]);
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_exactly_as_large_as_the_free_storage_fits() {
	let tar = CompressFormat::Tar { compression: None };
	let setup = setup(|_, _| {});
	let size = start(&setup, Run::new("a.tar", tar))
		.running
		.await
		.unwrap()
		.unwrap()
		.counts
		.archive_bytes;

	let exact = start(
		&setup,
		Run {
			max_bytes: Some(size),
			..Run::new("b.tar", tar)
		},
	);
	let report = exact.running.await.unwrap().unwrap();
	assert_eq!(report.counts.archive_bytes, size);

	let short = start(
		&setup,
		Run {
			max_bytes: Some(size - 1),
			..Run::new("c.tar", tar)
		},
	);
	let failed = short.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_out_of_storage_while_writing_leaves_nothing_behind() {
	let setup_storage = setup(|_, _| {});
	let job = start(
		&setup_storage,
		Run {
			max_bytes: Some(100),
			..Run::new("b.tar.gz", gzip_tar())
		},
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached);
	assert!(setup_storage.backend.log().finished.is_empty());
	setup_storage.backend.assert_released(&job.reporter);
	// ended by an error, not cancelled
	assert_eq!(job.recorder.run_states(), [RunState::Running]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_with_a_source_that_cannot_be_read_leaves_nothing_behind() {
	let setup_fetch = setup(|backend, _| {
		backend
			.fail_fetch
			.insert("big.bin".to_owned(), ErrorKind::Server);
	});
	let job = start(&setup_fetch, Run::tar());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Server);
	assert!(setup_fetch.backend.log().finished.is_empty());
	setup_fetch.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_cancelled_while_the_archive_uploads_leaves_nothing_behind() {
	let setup_cancel = setup(|backend, _| {
		backend.hold_named(Request::Upload, ["b.tar"]);
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup_cancel,
		Run {
			control,
			..Run::tar()
		},
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
	setup_cancel.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_chunk_that_comes_back_short_ends_the_job() {
	let setup = setup(|backend, files| {
		// made-up data, whose last chunk comes back a byte short
		backend.contents.remove(&files[1].uuid());
		backend.short_reads.insert("big.bin".to_owned());
	});
	let job = start(&setup, Run::tar());
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Response);
	assert!(
		failed
			.error
			.to_string()
			.contains(&SOURCE_UUIDS[1].to_string()),
		"the error names the source: {}",
		failed.error
	);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_silent_codec_is_given_up_on() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start(
		&setup,
		Run {
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWorkerDied);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
}

#[tokio::test(start_paused = true)]
async fn a_failed_codec_gives_back_its_own_error() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start(
		&setup,
		Run {
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
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
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_chunk_after_the_head_is_an_internal_error() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let format = CompressFormat::SevenZ {
		method: SevenZMethod::Copy,
		solid: false,
		encryption: None,
	};
	let job = start(
		&setup,
		Run {
			codec: Some(Box::new(move || Ok(link))),
			..Run::new("b.7z", format)
		},
	);
	// the head is the archive's last chunk: nothing may follow it
	events.send(WorkerEvent::Head(vec![1; 32])).await.unwrap();
	events.send(WorkerEvent::Data(vec![2; 8])).await.unwrap();
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Internal);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
}

#[tokio::test(start_paused = true)]
async fn an_event_only_an_extracting_codec_sends_is_an_internal_error() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start(
		&setup,
		Run {
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Zip))
		.await
		.unwrap();
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Internal);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
}

#[tokio::test(start_paused = true)]
async fn a_codec_waiting_for_its_chunk_is_not_given_up_on() {
	let setup = setup(|backend, files| {
		backend.hold_requests(Request::Fetch, [files[0].uuid()]);
	});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Run {
			control,
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	let (reply, answer) = oneshot::channel();
	events
		.send(WorkerEvent::Ask {
			source: 0,
			index: 0,
			reply,
		})
		.await
		.unwrap();
	// longer than a silent codec is given, which one owed its chunk must not be taken for
	tokio::time::sleep(2 * ARCHIVE_STALL_TIMEOUT).await;
	assert!(!job.running.is_finished());
	setup.backend.release_all();
	assert_eq!(answer.await.unwrap(), setup.contents[0]);

	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
}

/// The sources as placed in the fake drive: `docs` (holding the first two files) in `parent`,
/// and the third file next to it.
struct Placed {
	parent: Uuid,
	docs: Uuid,
}

/// Where [`place`] puts the sources.
const PLACED_PARENT: Uuid = Uuid::from_u128(0x70);
const PLACED_DOCS: Uuid = Uuid::from_u128(0x71);

fn place(setup: &Setup) -> Placed {
	let (parent, docs) = (PLACED_PARENT, PLACED_DOCS);
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
		DisposalTarget::Dir(ExpectedDir {
			uuid: placed.docs,
			parent: placed.parent,
			read,
		}),
		DisposalTarget::File(ExpectedFile::of(top, top.uuid(), placed.parent)),
	]
}

/// `targets` to remove, each with whether the files read below it were `hashed`.
fn sources(targets: Vec<DisposalTarget>, hashed: &[bool]) -> Vec<DisposalSource> {
	assert_eq!(targets.len(), hashed.len());
	targets
		.into_iter()
		.zip(hashed)
		.map(|(target, &hashed)| DisposalSource { target, hashed })
		.collect()
}

async fn compress_disposing(
	setup: &Setup,
	how: SourceDisposal,
	hashed: [bool; 2],
	report: CompressReport,
) -> CompressReport {
	let placed = place(setup);
	let disposal = (how, sources(targets(setup, &placed), &hashed));
	let job = start(
		setup,
		Run {
			disposal: Some(disposal),
			report,
			..Run::tar()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	setup.backend.assert_released(&job.reporter);
	report
}

fn outcomes(report: &CompressReport) -> Vec<DisposalOutcome> {
	report
		.dispositions
		.iter()
		.map(|disposition| disposition.outcome.clone())
		.collect()
}

/// Asserts that every source was removed, saying `what` case failed if one was not.
fn assert_all_disposed(report: &CompressReport, what: &str) {
	assert!(
		outcomes(report)
			.iter()
			.all(|outcome| matches!(outcome, DisposalOutcome::Disposed { .. })),
		"{what}: {:?}",
		report.dispositions
	);
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

/// The outcomes of the two sources, the folder `docs` and the file `top.txt`.
fn docs_and_top(report: &CompressReport) -> [DisposalOutcome; 2] {
	outcomes(report)
		.try_into()
		.unwrap_or_else(|outcomes| panic!("two sources: {outcomes:?}"))
}

fn is_changed(outcome: &DisposalOutcome) -> bool {
	matches!(
		outcome,
		DisposalOutcome::Kept {
			reason: KeptReason::Changed,
			bytes_freed: 0
		}
	)
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
async fn a_source_is_kept_once_the_archive_is_gone_by_its_turn() {
	let mut setup = setup(|_, _| {});
	let placed = place(&setup);
	// the folder goes first: its recheck waits under the lock, after the archive's
	setup
		.hold_after_registering
		.push((Request::State, placed.docs));
	let job = run_disposal(
		&setup,
		SourceDisposal::Trash,
		targets(&setup, &placed),
		JobControl::default(),
	);
	wait_until("the folder's removal is under way", || {
		setup
			.backend
			.log()
			.held
			.contains(&(Request::State, placed.docs))
	})
	.await;
	// the archive, the one file in the destination, goes before the top file's turn
	let destination = setup.destination;
	setup
		.backend
		.log()
		.file_parents
		.retain(|_, (parent, ..)| *parent != destination);
	setup.backend.release_all();
	let report = job.running.await.unwrap().unwrap();
	let [docs, top] = docs_and_top(&report);
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(
		matches!(
			top,
			DisposalOutcome::Kept {
				reason: KeptReason::Unconfirmed,
				bytes_freed: 0
			}
		),
		"{top:?}"
	);
	assert!(setup.backend.log().trashed_files.is_empty());
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_file_in_the_trash_is_kept_as_changed() {
	let setup = setup(|backend, _| {
		backend.in_trash.insert(SOURCE_UUIDS[2]);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::Trash,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = docs_and_top(&report);
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(is_changed(&top), "{top:?}");
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_file_with_a_newer_version_is_kept_as_changed() {
	let setup = setup(|backend, _| {
		backend.superseded.insert(SOURCE_UUIDS[2]);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = docs_and_top(&report);
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	assert!(is_changed(&top), "{top:?}");
	assert!(!setup.backend.log().deleted_files.contains(&SOURCE_UUIDS[2]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_folder_in_the_trash_is_kept_as_changed() {
	let setup = setup(|backend, _| {
		backend.in_trash.insert(PLACED_DOCS);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = docs_and_top(&report);
	assert!(is_changed(&docs), "{docs:?}");
	assert!(matches!(top, DisposalOutcome::Disposed { .. }), "{top:?}");
	let log = setup.backend.log();
	assert_eq!(log.deleted_files, [SOURCE_UUIDS[2]]);
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_whose_state_cannot_be_fetched_is_kept_with_the_error() {
	let setup = setup(|backend, _| {
		backend.fail_state_of.insert(SOURCE_UUIDS[2]);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::Trash,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = docs_and_top(&report);
	assert!(matches!(docs, DisposalOutcome::Disposed { .. }), "{docs:?}");
	let DisposalOutcome::Kept {
		reason: KeptReason::Failed { error },
		bytes_freed: 0,
	} = top
	else {
		panic!("{top:?}");
	};
	assert_eq!(error.kind(), ErrorKind::Server);
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_folder_that_cannot_be_listed_is_kept_with_the_error() {
	let setup = setup(|backend, _| {
		backend.quirks.insert(Quirk::FailTrees);
	});
	let report = compress_disposing(
		&setup,
		SourceDisposal::DeletePermanently,
		[true; 2],
		CompressReport::default(),
	)
	.await;
	let [docs, top] = docs_and_top(&report);
	assert!(
		matches!(
			docs,
			DisposalOutcome::Kept {
				reason: KeptReason::Failed { .. },
				bytes_freed: 0
			}
		),
		"{docs:?}"
	);
	assert!(matches!(top, DisposalOutcome::Disposed { .. }), "{top:?}");
	let log = setup.backend.log();
	assert_eq!(log.deleted_files, [SOURCE_UUIDS[2]]);
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_that_changed_is_kept_on_its_own() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	// a file arrived in the directory after it was read
	setup
		.backend
		.place_file(Uuid::from_u128(0x7A), placed.docs, 10);
	let disposal = (
		SourceDisposal::DeletePermanently,
		sources(targets(&setup, &placed), &[true; 2]),
	);
	let job = start(
		&setup,
		Run {
			disposal: Some(disposal),
			..Run::tar()
		},
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
async fn only_the_source_whose_data_differs_from_its_hash_is_kept() {
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
}

/// Asserts that nothing was removed from the fake drive.
fn assert_nothing_removed(setup: &Setup) {
	let log = setup.backend.log();
	assert!(log.trashed_files.is_empty() && log.deleted_files.is_empty());
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_without_hashes_is_kept_on_its_own_from_a_permanent_removal() {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sources_without_hashes_are_kept_from_a_permanent_removal() {
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
	assert_nothing_removed(&setup_unhashed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_skipped_while_planning_keeps_every_source() {
	let setup_skipped = setup(|_, _| {});
	let skipped = CompressReport {
		skipped: vec![SkippedEntry {
			source_path: "docs/secret".into(),
			bytes: 1,
			reason: SkipReason::UndecryptableFile {
				uuid: Uuid::from_u128(0x5E),
			},
		}],
		..CompressReport::default()
	};
	let report =
		compress_disposing(&setup_skipped, SourceDisposal::Trash, [true; 2], skipped).await;
	all_kept_for(&report, |reason| matches!(reason, KeptReason::Incomplete));
	assert_nothing_removed(&setup_skipped);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_not_in_the_drive_as_registered_keeps_every_source() {
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
	assert_nothing_removed(&setup_gone);
}

/// The entries of a 7z read back through the SDK's reader, every CRC-32 checked.
fn sevenz_entries(archive: &[u8]) -> Vec<(String, Vec<u8>)> {
	let limits = READ_BACK_SEVEN_Z;
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
		let job = start(&setup, Run::new("bundle.7z", format));
		let report = job.running.await.unwrap().unwrap();
		let archive = report.archive.as_ref().unwrap();
		let (_, completion) = setup.backend.log().finished[&archive.uuid()].clone();
		let bytes = uploaded(&setup, archive.uuid());
		assert_eq!(completion.written, bytes.len() as u64, "{method:?}");
		assert_eq!(
			completion.num_chunks,
			(bytes.len() as u64).div_ceil(CHUNK_SIZE_U64),
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
		setup.backend.assert_released(&job.reporter);
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
	let disposal = (
		SourceDisposal::DeletePermanently,
		sources(targets, &[true; 4]),
	);
	let job = start(
		&setup,
		Run {
			disposal: Some(disposal),
			..Run::tar()
		},
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
	start(
		setup,
		Run {
			control,
			disposal: Some((how, sources(targets, &hashed))),
			..Run::tar()
		},
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_a_partial_removal_did_not_reach_is_kept() {
	// a.txt comes last in the folder's (uuid) order, and the removal stops at the second file
	let [a, big, top] = SOURCE_UUIDS;
	let setup = setup_with([top, a, big], |backend, files| {
		backend.fail_deletes_of.insert(files[2].uuid());
	});
	let [a, big, top] = [0, 1, 2].map(|source| setup.sources[source].1.clone());
	let docs = PLACED_DOCS;
	setup.backend.place_dir(docs, PLACED_PARENT);
	for file in [&a, &big, &top] {
		setup.backend.place_file(file.uuid(), docs, file.size());
	}
	let targets = vec![
		DisposalTarget::Dir(ExpectedDir {
			uuid: docs,
			parent: PLACED_PARENT,
			read: Tree {
				files: [&a, &big, &top]
					.iter()
					.map(|file| (file.uuid(), file.size()))
					.collect(),
				dirs: Default::default(),
			},
		}),
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
	let setup = setup(|backend, files| {
		backend.fail_deletes_of.insert(files[2].uuid());
	});
	let [a, big, top] = [0, 1, 2].map(|source| setup.sources[source].1.clone());
	let docs = PLACED_DOCS;
	setup.backend.place_dir(docs, PLACED_PARENT);
	for file in [&a, &big, &top] {
		setup.backend.place_file(file.uuid(), docs, file.size());
	}
	let targets = vec![
		DisposalTarget::Dir(ExpectedDir {
			uuid: docs,
			parent: PLACED_PARENT,
			read: Tree {
				files: [&a, &big, &top]
					.iter()
					.map(|file| (file.uuid(), file.size()))
					.collect(),
				dirs: Default::default(),
			},
		}),
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
	let [x, y] = [Uuid::from_u128(0x7B), Uuid::from_u128(0x7C)];
	let holding = |other| {
		DisposalTarget::Dir(ExpectedDir {
			uuid: if other == y { x } else { y },
			parent: PLACED_PARENT,
			read: Tree {
				files: Default::default(),
				dirs: [other].into(),
			},
		})
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_dropped_past_its_cancel_grace_has_told_of_what_it_removed() {
	let mut setup = setup(|_, _| {});
	let placed = place(&setup);
	let top = setup.sources[2].1.uuid();
	// the folder goes first; the top file's deletion is sent and never answered
	setup.hold_after_registering.push((Request::Delete, top));
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	wait_until("the top file's deletion is sent", || {
		setup.backend.log().held.contains(&(Request::Delete, top))
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
	setup.backend.hold_requests(Request::List, [placed.docs]);
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets, control);
	wait_until("the folder is being listed", || {
		setup
			.backend
			.log()
			.held
			.contains(&(Request::List, placed.docs))
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
	setup.backend.assert_released(&job.reporter);
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
	setup
		.hold_after_registering
		.push((Request::Delete, first.uuid()));
	let (_pause, cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets(&setup, &placed), control);
	wait_until("the first file's deletion is sent", || {
		setup
			.backend
			.log()
			.held
			.contains(&(Request::Delete, first.uuid()))
	})
	.await;
	cancel.send_replace(true);
	setup.backend.release_all();
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
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_compressing_keeps_every_source() {
	let setup = setup(|backend, _| {
		backend.hold_named(Request::Upload, ["b.tar"]);
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
	setup.backend.assert_released(&job.reporter);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_codec_reading_its_sources_out_of_order_fails_the_job() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start(
		&setup,
		Run {
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	// the second source before the first
	let (reply, _answer) = oneshot::channel();
	events
		.send(WorkerEvent::Ask {
			source: 1,
			index: 0,
			reply,
		})
		.await
		.unwrap();
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Internal);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
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
		answer.await.unwrap();
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_compress_holds_nothing_of_the_clients_budget() {
	let setup = setup(|_, _| {});
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let (pause, cancel, control) = controls();
	let job = start(
		&setup,
		Run {
			control,
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	// the first source's chunk takes the job's own slot; the chunk being read and the two
	// fetched ahead of it take the client's budget
	read_into_the_second_source(&events).await;
	let budget = setup.backend.budget;
	wait_until("two chunks are fetched ahead", || {
		setup.backend.memory.available_permits() == budget - 3 * FULL_CHUNK_BYTES
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	setup.backend.assert_released(&job.reporter);
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
		job.recorder.run_states(),
		[RunState::Running, RunState::Paused, RunState::Cancelling]
	);
	assert!(setup.backend.log().finished.is_empty());
	setup.backend.assert_released(&job.reporter);
	drop((events, result));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compress_paused_before_it_starts_takes_no_slot() {
	let mut setup_paused = setup(|_, _| {});
	setup_paused.config =
		ArchiveConfig::new(&ClientConfig::default().with_archive_job_concurrency(1));
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	let paused = start(
		&setup_paused,
		Run {
			control,
			..Run::new("paused.tgz", gzip_tar())
		},
	);
	wait_until("the job reports itself paused", || {
		paused.reporter.is_paused()
	})
	.await;

	// the only slot is free for a job started later
	let mut setup_other = setup(|_, _| {});
	setup_other.config = setup_paused.config.clone();
	let other = start(&setup_other, Run::new("other.tgz", gzip_tar()));
	tokio::time::timeout(Duration::from_secs(20), other.running)
		.await
		.expect("the unpaused job runs")
		.unwrap()
		.unwrap();
	assert!(setup_paused.backend.log().fetched.is_empty());
	assert!(paused.reporter.is_paused());
	assert_eq!(
		paused.recorder.last().phase,
		CompressPhase::WaitingForWorker
	);
	assert_eq!(setup_paused.config.free_slots(), 1);

	pause.send_replace(false);
	let report = paused.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 3);
	assert_eq!(
		paused.recorder.run_states(),
		[RunState::Paused, RunState::Running]
	);
	setup_paused.backend.assert_released(&paused.reporter);
	assert_eq!(setup_paused.config.free_slots(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resumed_compress_writes_the_archive_it_would_have() {
	// the job is still reading when the pause comes
	let mut big = None;
	let setup_paused = setup(|backend, files| {
		backend.hold_requests(Request::Fetch, [files[1].uuid()]);
		big = Some(files[1].uuid());
	});
	let big = (Request::Fetch, big.unwrap());
	let (pause, _cancel, control) = controls();
	let format = CompressFormat::Tar { compression: None };
	let job = start(
		&setup_paused,
		Run {
			control,
			..Run::new("b.tar", format)
		},
	);
	wait_until("the second source is being read", || {
		setup_paused.backend.log().held.contains(&big)
	})
	.await;
	pause.send_replace(true);
	setup_paused.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	setup_paused.backend.assert_released(&job.reporter);
	assert!(setup_paused.backend.log().finished.is_empty());
	pause.send_replace(false);
	let archive = job.running.await.unwrap().unwrap().archive.unwrap();
	let paused = uploaded(&setup_paused, archive.uuid());
	assert_eq!(
		job.recorder.run_states(),
		[RunState::Running, RunState::Paused, RunState::Running]
	);

	let setup_straight = setup(|_, _| {});
	let job = start(&setup_straight, Run::new("b.tar", format));
	let archive = job.running.await.unwrap().unwrap().archive.unwrap();
	assert_eq!(
		paused,
		uploaded(&setup_straight, archive.uuid()),
		"a pause changes nothing in the archive"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_registering_holds_the_disposal_back() {
	let registering = (Request::Finish, "b.tar".to_owned());
	let setup = setup(|backend, _| {
		backend.hold_named(Request::Finish, ["b.tar"]);
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
		setup.backend.log().held_named.contains(&registering)
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_named(Request::Finish, ["b.tar"]);
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
	setup.backend.assert_released(&job.reporter);

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_all_disposed(&report, "");
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
fn dispose_permanently_as(setup: &Setup, name: &'static str, format: CompressFormat) -> Job {
	let placed = place(setup);
	start(
		setup,
		Run {
			disposal: Some((
				SourceDisposal::DeletePermanently,
				sources(targets(setup, &placed), &[true; 2]),
			)),
			..Run::new(name, format)
		},
	)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permanent_removal_reads_the_archive_back_first() {
	let password = || Some(archive_password("hunter2"));
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
		let archive = report.archive.as_ref().unwrap();
		let archive = archive.uuid();
		assert_all_disposed(&report, name);
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
		setup.backend.assert_released(&job.reporter);
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sources_are_kept_when_the_archive_does_not_read_back_as_them() {
	// the archive a codec writes for the same sources, with one byte of a.txt's data off
	let straight = setup(|_, _| {});
	let job = start(&straight, Run::tar());
	let archive = job.running.await.unwrap().unwrap().archive.unwrap();
	let mut damaged = uploaded(&straight, archive.uuid());
	// the directory's header, then a.txt's, then its data
	let data = 2 * TAR_BLOCK;
	assert_eq!(&damaged[data..data + 5], b"alpha");
	damaged[data] ^= 1;

	let setup = setup(|_, _| {});
	let job = dispose_scripted(&setup, &damaged).await;
	let report = job.running.await.unwrap().unwrap();
	assert_kept_unconfirmed(&setup, &job.reporter, &report);
}

/// Starts a permanent disposal of the setup under a codec that reads every source whole and
/// writes `archive`, and plays that codec to its end.
async fn dispose_scripted(setup: &Setup, archive: &[u8]) -> Job {
	let placed = place(setup);
	let (events, result, link) = worker::test_support::scripted::<CodecResult>();
	let job = start(
		setup,
		Run {
			disposal: Some((
				SourceDisposal::DeletePermanently,
				sources(targets(setup, &placed), &[true; 2]),
			)),
			codec: Some(Box::new(move || Ok(link))),
			..Run::tar()
		},
	);
	// each source's chunks, and whether it is the source's last
	for (source, index, last) in [(0, 0, true), (1, 0, false), (1, 1, true), (2, 0, true)] {
		let (reply, answer) = oneshot::channel();
		events
			.send(WorkerEvent::Ask {
				source,
				index,
				reply,
			})
			.await
			.unwrap();
		answer.await.unwrap();
		if last {
			events.send(WorkerEvent::FileEnd).await.unwrap();
		}
	}
	for chunk in archive.chunks(CHUNK_SIZE) {
		events
			.send(WorkerEvent::Data(chunk.to_vec()))
			.await
			.unwrap();
	}
	drop(events);
	result.send(Ok(archive.len() as u64)).unwrap();
	job
}

/// The archive stays, and every source is kept because the archive was not confirmed.
fn assert_kept_unconfirmed(setup: &Setup, reporter: &Reporter, report: &CompressReport) {
	all_kept_for(report, |reason| matches!(reason, KeptReason::Unconfirmed));
	assert!(setup.backend.log().deleted_files.is_empty());
	assert!(setup.backend.log().trashed_dirs.is_empty());
	setup.backend.assert_released(reporter);
}

/// A scripted reader for the setup's archive: what it sends, how it ends, and its progress.
struct Reader {
	events: mpsc::Sender<WorkerEvent>,
	result: oneshot::Sender<ReadBackResult>,
	shared: Arc<worker::WorkerShared>,
}

impl Reader {
	/// Reads the setup's archive back in place of the extracting codec.
	fn reading(setup: &Setup) -> Self {
		let (events, result, link) = worker::test_support::scripted::<ReadBackResult>();
		let shared = Arc::clone(&link.shared);
		*setup.reader.lock().unwrap() = Some(Box::new(move |_, _| Ok(link)));
		Self {
			events,
			result,
			shared,
		}
	}

	/// Sends `event`, noting the progress a real reader notes with it.
	async fn send(&self, event: WorkerEvent) {
		self.events.send(event).await.unwrap();
		self.shared.note_scripted_progress();
	}
}

/// What reading the setup's archive back finds, in order: its directory, then each file's
/// entry, data and end.
fn found_in_the_archive(setup: &Setup) -> Vec<WorkerEvent> {
	let entry = |ordinal: usize, path: &str, kind| {
		WorkerEvent::Entry(EntryHead {
			ordinal: ordinal as u64,
			path: entry_path(path).unwrap(),
			modified: None,
			kind,
		})
	};
	let mut found = vec![
		WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }),
		entry(0, "docs", EntryKind::Dir),
	];
	for (ordinal, ((path, _), data)) in setup.sources.iter().zip(&setup.contents).enumerate() {
		found.extend([
			entry(
				ordinal + 1,
				path,
				EntryKind::File {
					size: Some(data.len() as u64),
				},
			),
			WorkerEvent::Data(data.clone()),
			WorkerEvent::FileEnd,
		]);
	}
	found
}

#[tokio::test(start_paused = true)]
async fn a_slow_read_back_is_not_given_up_on() {
	let mut setup = setup(|_, _| {});
	setup.hold_archive = true;
	let reader = Reader::reading(&setup);
	let job = dispose_scripted(&setup, b"the archive the reader reads back").await;
	// the reader waits longer for its first chunk than a silent one is given
	let (reply, answer) = oneshot::channel();
	reader
		.send(WorkerEvent::Ask {
			source: 0,
			index: 0,
			reply,
		})
		.await;
	tokio::time::sleep(2 * ARCHIVE_STALL_TIMEOUT).await;
	setup.backend.release_all();
	answer.await.unwrap();
	reader.shared.note_scripted_progress();
	// and takes longer over the entries than a silent one is given, never as long over one
	for found in found_in_the_archive(&setup) {
		tokio::time::sleep(ARCHIVE_STALL_TIMEOUT * 3 / 4).await;
		reader.send(found).await;
	}
	let Reader { events, result, .. } = reader;
	drop(events);
	result
		.send(Ok(ArchiveEnd {
			unaccounted_bytes: 0,
			duplicates: None,
			unchecked_entries: 0,
			password: PasswordCheck::NotNeeded,
		}))
		.unwrap();

	let report = job.running.await.unwrap().unwrap();
	assert_all_disposed(&report, "");
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_silent_read_back_keeps_the_sources() {
	let setup = setup(|_, _| {});
	let reader = Reader::reading(&setup);
	let job = dispose_scripted(&setup, b"an archive nothing reads back").await;
	let waiting = tokio::time::Instant::now();
	let report = tokio::time::timeout(4 * ARCHIVE_STALL_TIMEOUT, job.running)
		.await
		.expect("a silent reader is given up on")
		.unwrap()
		.unwrap();
	assert!(waiting.elapsed() >= ARCHIVE_STALL_TIMEOUT);
	assert_kept_unconfirmed(&setup, &job.reporter, &report);
	// the reader's ends were held open all along
	drop(reader);
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
	let (_, archive) = setup.backend.log().held[0];
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
	setup.backend.release_all();
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
	setup.backend.assert_released(&job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_read_back_holds_nothing_and_reports_its_progress() {
	let mut setup = setup(|_, _| {});
	let (pause, _cancel, control) = controls();
	let (job, archive) = held_at_the_read_back(&mut setup, control).await;
	pause.send_replace(true);
	// the fetch in flight finishes; the pause is seen when the reader asks again
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.run_state),
		(CompressPhase::Verifying, RunState::Paused)
	);
	setup.backend.assert_released(&job.reporter);
	let fetched = archive_fetches(&setup, archive);
	assert_eq!(fetched, 1, "the first chunk is read, the second waits");
	assert_eq!(
		last.counts.bytes_verified, CHUNK_SIZE_U64,
		"the read back reports its progress"
	);
	// the plan's totals, which the engine is handed with its report
	let sources: u64 = setup.contents.iter().map(|c| c.len() as u64).sum();
	let totals = PlanTotals {
		dirs: 1,
		files: 3,
		bytes: sources,
	};
	job.reporter.set_plan(totals, &[], &[]);
	let units = job.reporter.read(|state| state.progress().units);
	assert_eq!(
		units.total - units.settled,
		setup.backend.log().finished[&archive].1.written - CHUNK_SIZE_U64,
		"what is left of the archive to read is the work left"
	);

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_all_disposed(&report, "");
	assert_eq!(report.counts.bytes_verified, report.counts.archive_bytes);
	setup.backend.assert_released(&job.reporter);
}

#[test]
fn an_unreachable_record_counts_every_entry_it_stands_for() {
	let reporter = Reporter::new(Arc::new(Recorder::default()));
	reporter.set_plan(
		PlanTotals::default(),
		&[
			SkippedEntry {
				source_path: "orphans".into(),
				bytes: 0,
				reason: SkipReason::Unreachable { count: 3 },
			},
			SkippedEntry {
				source_path: "docs/locked.bin".into(),
				bytes: 7,
				reason: SkipReason::UndecryptableFile {
					uuid: Uuid::from_u128(0x5D),
				},
			},
		],
		&[],
	);
	assert_eq!(reporter.counts().entries_skipped, 4);
	assert_eq!(reporter.counts().bytes_skipped, 7);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_folder_moved_since_it_was_read_is_kept() {
	let setup = setup(|_, _| {});
	let placed = place(&setup);
	let targets = targets(&setup, &placed);
	// the folder moved, with everything in it, after the job read it
	setup.backend.place_dir(placed.docs, Uuid::from_u128(0x72));
	let job = run_permanent_disposal(&setup, targets, JobControl::default());
	let report = job.running.await.unwrap().unwrap();
	let outcomes = outcomes(&report);
	assert!(
		matches!(
			&outcomes[0],
			DisposalOutcome::Kept {
				reason: KeptReason::Changed,
				bytes_freed: 0
			}
		),
		"{outcomes:?}"
	);
	assert!(matches!(&outcomes[1], DisposalOutcome::Disposed { .. }));
	let log = setup.backend.log();
	assert_eq!(
		log.deleted_files,
		[setup.sources[2].1.uuid()],
		"nothing in the folder is deleted"
	);
	assert!(log.trashed_dirs.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_removing_holds_no_lock_and_removes_nothing() {
	let setup = setup(|backend, _| {
		// registering the archive takes the first lock; the removals wait for theirs
		backend.block_locks_from.send_replace(Some(1));
	});
	let placed = place(&setup);
	let targets = targets(&setup, &placed);
	let (pause, _cancel, control) = controls();
	let job = run_permanent_disposal(&setup, targets, control);

	wait_until("a removal waits for the drive lock", || {
		setup.backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_eq!(
		setup.backend.live_locks.load(Ordering::SeqCst),
		0,
		"a paused job holds no lock"
	);
	// nor waits for one: with no ask for the lock left pending, a lock coming free lets
	// nothing through
	assert_eq!(
		setup.backend.block_locks_from.receiver_count(),
		0,
		"a paused job waits for no lock"
	);
	{
		let log = setup.backend.log();
		assert!(
			log.deleted_files.is_empty() && log.trashed_dirs.is_empty(),
			"nothing is removed while paused"
		);
	}

	setup.backend.block_locks_from.send_replace(None);
	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	for outcome in outcomes(&report) {
		assert!(
			matches!(outcome, DisposalOutcome::Disposed { .. }),
			"{outcome:?}"
		);
	}
	assert_eq!(
		setup.backend.log().lock_waits,
		1,
		"the paused job asked for no lock while they were held back"
	);
	setup.backend.assert_released(&job.reporter);
}

/// [`enclosing`] as a scan of every pair of targets, the definition it has to agree with.
fn enclosing_by_scanning(sources: &[DisposalSource]) -> Vec<Option<usize>> {
	let targets: Vec<&DisposalTarget> = sources.iter().map(|source| &source.target).collect();
	targets
		.iter()
		.enumerate()
		.map(|(index, target)| {
			targets.iter().enumerate().position(|(other, outer)| {
				other != index
					&& (other < index && outer.uuid() == target.uuid()
						|| match (target, outer) {
							(DisposalTarget::File(file), DisposalTarget::Dir(outer)) => {
								outer.read.files.contains_key(&file.uuid)
							}
							(
								DisposalTarget::Dir(ExpectedDir { uuid, .. })
								| DisposalTarget::Unavailable { uuid },
								DisposalTarget::Dir(outer),
							) => {
								outer.read.dirs.contains(uuid)
									|| outer.read.files.contains_key(uuid)
							}
							_ => false,
						})
			})
		})
		.collect()
}

#[test]
fn sources_go_with_the_first_one_holding_them() {
	let mut rng = StdRng::seed_from_u64(0x656e_636c_6f73_696e);
	// few uuids, so sources repeat and hold each other, in loops too
	let pool: Vec<Uuid> = (0..6).map(|i| Uuid::from_u128(0xE0 + i)).collect();
	for _ in 0..2000 {
		let targets: Vec<DisposalTarget> = (0..rng.random_range(0..7))
			.map(|_| {
				let uuid = pool[rng.random_range(0..pool.len())];
				match rng.random_range(0..3) {
					0 => DisposalTarget::File(ExpectedFile {
						uuid,
						size: 1,
						chunks: 1,
						parent: PLACED_PARENT,
					}),
					1 => DisposalTarget::Unavailable { uuid },
					_ => {
						let mut read = Tree::default();
						for &item in &pool {
							match rng.random_range(0..4) {
								0 => {
									read.files.insert(item, 1);
								}
								1 => {
									read.dirs.insert(item);
								}
								_ => {}
							}
						}
						DisposalTarget::Dir(ExpectedDir {
							uuid,
							parent: PLACED_PARENT,
							read,
						})
					}
				}
			})
			.collect();
		let hashed = vec![true; targets.len()];
		let sources = sources(targets, &hashed);
		assert_eq!(
			enclosing(&sources),
			enclosing_by_scanning(&sources),
			"{sources:?}"
		);
	}
}
