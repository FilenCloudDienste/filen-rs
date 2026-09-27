//! The driver against the fake drive. Most tests run the real codec on its thread, so they run in
//! real time on a multi-threaded runtime; the stall test scripts a silent codec on paused time.

use std::{
	collections::{BTreeMap, HashSet},
	io::Write,
	sync::{Mutex, atomic::Ordering},
	time::Duration,
};

use tokio::task::JoinHandle;

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		archive::{
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			extract::{
				ArchiveTotals, ExpansionLimit, ExtractCallback, ExtractSkipReason, ExtractUpdate,
				codec::{CodecLimits, StreamJob, extract_stream},
			},
			worker,
		},
		dir::RootDirectory,
		drive_job::{backend::ListedNames, counts::ItemCounts, test_support::FakeBackend},
		file::{
			AnonymousRemoteFile, RemoteFile,
			meta::{DecryptedFileMeta, FileMeta},
		},
	},
	job::test_support::controls,
};

#[derive(Default)]
struct Recorder {
	top_level: Mutex<Vec<ExtractedTopLevel>>,
	updates: Mutex<Vec<ExtractUpdate>>,
}

impl ExtractCallback for Recorder {
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>) {
		self.top_level.lock().unwrap().extend(items);
	}

	fn on_update(&self, update: ExtractUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

impl Recorder {
	fn last(&self) -> ExtractUpdate {
		self.updates.lock().unwrap().last().unwrap().clone()
	}
}

fn archive_file(name: &str, bytes: &[u8]) -> RemoteFileType<'static> {
	let size = bytes.len() as u64;
	let meta = FileMeta::Decoded(DecryptedFileMeta {
		name: Cow::Owned(name.to_owned()),
		size,
		mime: Cow::Borrowed("application/octet-stream"),
		key: FileKey::V3(EncryptionKey::generate()),
		last_modified: Utc::now(),
		created: None,
		hash: None,
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

fn tar_of(members: &[(&str, &[u8])]) -> Vec<u8> {
	let mut builder = tar::Builder::new(Vec::new());
	for (path, data) in members {
		let mut header = tar::Header::new_gnu();
		header.set_mtime(1_700_000_000);
		header.set_mode(0o644);
		if path.ends_with('/') {
			header.set_entry_type(tar::EntryType::Directory);
			header.set_size(0);
			builder.append_data(&mut header, path, &b""[..]).unwrap();
		} else {
			header.set_entry_type(tar::EntryType::Regular);
			header.set_size(data.len() as u64);
			builder.append_data(&mut header, path, *data).unwrap();
		}
	}
	builder.into_inner().unwrap()
}

fn gzip(data: &[u8]) -> Vec<u8> {
	let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
	encoder.write_all(data).unwrap();
	encoder.finish().unwrap()
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
	(0..len).map(|i| (i % 251) as u8 ^ seed).collect()
}

fn hash(data: &[u8]) -> Blake3Hash {
	Blake3Hash::from(blake3::hash(data))
}

struct Setup {
	backend: Arc<FakeBackend>,
	destination: Uuid,
	archive: RemoteFileType<'static>,
}

fn setup(name: &str, bytes: Vec<u8>, configure: impl FnOnce(&mut FakeBackend)) -> Setup {
	let destination = Uuid::new_v4();
	let archive = archive_file(name, &bytes);
	let mut backend = FakeBackend::new(destination);
	backend.contents.insert(archive.uuid(), bytes);
	configure(&mut backend);
	Setup {
		backend: Arc::new(backend),
		destination,
		archive,
	}
}

type Running = JoinHandle<Result<ExtractReport, ExtractFailed>>;

struct Job {
	running: Running,
	recorder: Arc<Recorder>,
	reporter: MaybeArc<Reporter>,
}

struct Options {
	root: ExtractRoot,
	control: JobControl,
	max_bytes: Option<u64>,
	max_items: Option<u64>,
}

impl Default for Options {
	fn default() -> Self {
		Self {
			root: ExtractRoot::NewFolder { name: None },
			control: JobControl::default(),
			max_bytes: None,
			max_items: None,
		}
	}
}

fn start_with(
	setup: &Setup,
	options: Options,
	start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
) -> Job {
	let recorder = Arc::new(Recorder::default());
	let reporter = Reporter::new(
		Arc::clone(&recorder),
		ArchiveTotals::Streaming {
			archive_bytes: setup.archive.size(),
		},
	);
	let running = tokio::spawn(run_extract(ExtractTask {
		backend: Arc::clone(&setup.backend),
		control: options.control,
		reporter: MaybeArc::clone(&reporter),
		archive: setup.archive.clone(),
		destination: DirType::Root(Cow::Owned(RootDirectory::new(setup.destination))),
		root: options.root,
		max_bytes: options.max_bytes,
		max_items: options.max_items,
		config: ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY),
		start,
	}));
	Job {
		running,
		recorder,
		reporter,
	}
}

/// Runs the real codec on its own thread.
fn start(setup: &Setup, options: Options) -> Job {
	let job = StreamJob {
		name: setup.archive.name().unwrap().to_owned(),
		len: setup.archive.size(),
		limits: CodecLimits {
			decoder_memory: CODEC_MEM_BUDGET,
			max_members: 1000,
			expansion: Some(ExpansionLimit::DEFAULT),
		},
	};
	start_with(
		setup,
		options,
		Box::new(move || worker::start(move |port| extract_stream(&port, job))),
	)
}

/// Everything a finished job must have given back.
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
	let log = setup.backend.log();
	let uploaded: HashSet<_> = log.uploaded.iter().copied().collect();
	assert_eq!(
		uploaded.len(),
		log.uploaded.len(),
		"no chunk is uploaded twice"
	);
	assert!(
		log.out_of_order_dirs.is_empty(),
		"parents are created first"
	);
}

/// The registered files by name: size, chunks and hash.
fn finished(setup: &Setup) -> BTreeMap<String, (u64, u64, Blake3Hash)> {
	setup
		.backend
		.log()
		.finished
		.values()
		.map(|(name, completion)| {
			(
				name.clone(),
				(completion.written, completion.num_chunks, completion.hash),
			)
		})
		.collect()
}

fn created_dirs(setup: &Setup) -> Vec<String> {
	setup
		.backend
		.log()
		.created_dirs
		.iter()
		.map(|(_, name)| name.clone())
		.collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_a_compressed_tar_into_a_new_folder() {
	let big = pattern(2 * CHUNK_SIZE + 100, 7);
	let mut tar = tar_of(&[
		("docs/", b""),
		("docs/a.txt", b"alpha"),
		("docs/sub/big.bin", &big),
		("empty.txt", b""),
	]);
	// a symlink the extraction skips, written into a second tar and joined on
	let mut builder = tar::Builder::new(Vec::new());
	let mut link = tar::Header::new_gnu();
	link.set_entry_type(tar::EntryType::Symlink);
	link.set_size(0);
	builder
		.append_link(&mut link, "link", "docs/a.txt")
		.unwrap();
	let linked = builder.into_inner().unwrap();
	// drop the first tar's end-of-archive blocks so the link member follows on
	tar.truncate(tar.len() - 1024);
	tar.extend(linked);
	let setup = setup("sample.tar.gz", gzip(&tar), |_| {});

	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(created_dirs(&setup), ["sample", "docs", "sub"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			("big.bin".to_owned(), (big.len() as u64, 3, hash(&big))),
			("empty.txt".to_owned(), (0, 0, hash(b""))),
		])
	);
	assert_eq!(
		report.counts,
		ItemCounts {
			dirs_created: 3,
			files_done: 3,
			bytes_done: 5 + big.len() as u64,
			entries_skipped: 1,
			..ItemCounts::default()
		}
	);
	assert_eq!(report.skipped.len(), 1);
	assert_eq!(
		report.skipped[0].reason,
		ExtractSkipReason::Symlink {
			target: "docs/a.txt".into()
		}
	);
	assert_eq!(report.unaccounted_bytes, 0);
	let top: Vec<_> = report.top_level.iter().map(|top| top.key).collect();
	assert_eq!(top, [ExtractTopLevelKey::Root]);
	assert_eq!(
		job.recorder.top_level.lock().unwrap().len(),
		1,
		"the callback got the root too"
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Done);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracting_into_the_destination_keeps_both() {
	let tar = tar_of(&[
		("docs/x.txt", b"x"),
		("readme.txt", b"new readme"),
		("other.txt", b"other"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.listed = ListedNames {
			names: vec!["docs".into(), "README.txt".into()],
			unverified: false,
		};
	});
	let options = Options {
		root: ExtractRoot::Destination,
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(created_dirs(&setup), ["docs (1)"]);
	let names: Vec<String> = finished(&setup).into_keys().collect();
	assert_eq!(names, ["other.txt", "readme (1).txt", "x.txt"]);
	let renamed: Vec<(&str, &str, ExtractRenameReason)> = report
		.renamed
		.iter()
		.map(|r| (r.path.as_str(), r.name.as_str(), r.reason))
		.collect();
	assert_eq!(
		renamed,
		[
			("docs", "docs (1)", ExtractRenameReason::DuplicateName),
			(
				"readme.txt",
				"readme (1).txt",
				ExtractRenameReason::DuplicateName
			),
		]
	);
	let top: Vec<ExtractTopLevelKey> = report.top_level.iter().map(|top| top.key).collect();
	let id = |index| ExtractTopLevelKey::Entry {
		id: ArchiveEntryId {
			archive: setup.archive.uuid(),
			index,
		},
	};
	// in creation order: the directory, then the files at the top as they are registered
	assert_eq!(top.len(), 3);
	assert_eq!(top[0], id(0));
	assert_eq!(
		top[1..].iter().copied().collect::<HashSet<_>>(),
		HashSet::from([id(1), id(2)])
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_single_compressed_file_lands_in_the_destination() {
	let data = pattern(CHUNK_SIZE + 3, 1);
	let setup = setup("notes.txt.gz", gzip(&data), |_| {});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	assert!(created_dirs(&setup).is_empty(), "no folder for one file");
	assert_eq!(
		finished(&setup),
		BTreeMap::from([("notes.txt".to_owned(), (data.len() as u64, 2, hash(&data)))])
	);
	let top: Vec<ExtractTopLevelKey> = report.top_level.iter().map(|top| top.key).collect();
	assert_eq!(
		top,
		[ExtractTopLevelKey::Entry {
			id: ArchiveEntryId {
				archive: setup.archive.uuid(),
				index: 0
			}
		}]
	);
	assert_released(&setup, &job.reporter);
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
	assert_eq!(failed.report.counts.files_done, 1);
	assert_eq!(
		finished(&setup).into_keys().collect::<Vec<_>>(),
		["first.txt"]
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Failed);
	assert_released(&setup, &job.reporter);
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
	assert!(finished(&setup_bytes).is_empty());
	assert_released(&setup_bytes, &job.reporter);

	let setup_items = setup("a.tar", tar, |_| {});
	let options = Options {
		max_items: Some(1),
		..Options::default()
	};
	let job = start(&setup_items, options);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveTooLarge);
	assert_released(&setup_items, &job.reporter);
}

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
	for _ in 0..1000 {
		if finished(&setup).contains_key("done.txt")
			&& job
				.reporter
				.read(|state| state.active_names().contains(&"stuck.bin".to_owned()))
		{
			break;
		}
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(failed.report.counts.files_done, 1);
	assert_eq!(
		failed.report.counts.bytes_done, 4,
		"the dropped file counts nothing"
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Cancelled);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_silent_codec_is_given_up_on() {
	let setup = setup("silent.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWorkerDied);
	assert_released(&setup, &job.reporter);
	// the codec's ends were held open all along
	drop((events, result));
}
