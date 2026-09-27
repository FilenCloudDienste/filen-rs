//! The driver against the fake drive. Most tests run the real codec on its thread, so they run in
//! real time on a multi-threaded runtime; the stall test scripts a silent codec on paused time.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
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
			entry_path::entry_path,
			extract::{
				ArchiveTotals, ExpansionLimit, ExtractCallback, ExtractSkipReason, ExtractUpdate,
				RunState,
				codec::{CodecLimits, StreamJob, extract_stream},
			},
			worker,
		},
		archive::{
			dispose::{DisposalOutcome, KeptReason, SourceDisposal},
			password::ArchivePassword,
			sevenz::write::{SevenZEncryption, SevenZMethod, SevenZWriter},
			zip::{
				crypto::AesStrength,
				write::{Encryption, ZipMethod, ZipWriter},
			},
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			counts::ItemCounts,
			test_support::{FakeBackend, wait_until},
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
	top_level: Mutex<Vec<ExtractedTopLevel>>,
	/// The size of each `on_top_level_created` batch.
	batches: Mutex<Vec<usize>>,
	updates: Mutex<Vec<ExtractUpdate>>,
}

impl ExtractCallback for Recorder {
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>) {
		self.batches.lock().unwrap().push(items.len());
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

	/// Every run state the updates went through, each once per stretch.
	fn run_states(&self) -> Vec<RunState> {
		let mut states: Vec<RunState> = self
			.updates
			.lock()
			.unwrap()
			.iter()
			.map(|update| update.run_state)
			.collect();
		states.dedup();
		states
	}
}

fn archive_file(name: &str, bytes: &[u8]) -> RemoteFileType<'static> {
	archive_file_with(name, bytes, None)
}

fn archive_file_with(
	name: &str,
	bytes: &[u8],
	hash: Option<Blake3Hash>,
) -> RemoteFileType<'static> {
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

/// Members a test archive may have, for the codec and the driver alike.
const MAX_MEMBERS: u64 = 2000;

/// The archive settings of a test job, with [`MAX_MEMBERS`].
fn test_config() -> ArchiveConfig {
	let mut config = ArchiveConfig::new(CODEC_MEM_BUDGET, JOB_CONCURRENCY);
	config.max_members = MAX_MEMBERS;
	config
}

struct Options {
	root: ExtractRoot,
	control: JobControl,
	max_bytes: Option<u64>,
	max_items: Option<u64>,
	dispose: Option<(SourceDisposal, Uuid)>,
	password: Option<ArchivePassword>,
	/// Shared between jobs that compete for its slots.
	config: ArchiveConfig,
}

impl Default for Options {
	fn default() -> Self {
		Self {
			root: ExtractRoot::NewFolder { name: None },
			control: JobControl::default(),
			max_bytes: None,
			max_items: None,
			dispose: None,
			password: None,
			config: test_config(),
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
		config: options.config,
		start,
		dispose: options.dispose,
		disposal_requested: options.dispose.is_some(),
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
			max_members: options.config.max_members,
			expansion: Some(ExpansionLimit::DEFAULT),
			max_index_bytes: 32 << 20,
			max_bytes: options.max_bytes,
		},
		password: options.password.clone(),
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

/// The registered files by their path below the destination: size, chunks and hash.
fn finished(setup: &Setup) -> BTreeMap<String, (u64, u64, Blake3Hash)> {
	let log = setup.backend.log();
	let dir_names: HashMap<Uuid, &str> = log
		.created_dirs
		.iter()
		.map(|(uuid, name)| (*uuid, name.as_str()))
		.collect();
	log.finished
		.iter()
		.map(|(uuid, (name, completion))| {
			let mut segments = vec![name.as_str()];
			let mut dir = log.registered_in[uuid];
			// up to the destination, which the job did not create
			while let Some(parent) = log.dir_parents.get(&dir) {
				segments.push(dir_names[&dir]);
				dir = *parent;
			}
			segments.reverse();
			(
				segments.join("/"),
				(completion.written, completion.num_chunks, completion.hash),
			)
		})
		.collect()
}

/// The registered files' paths below the destination.
fn finished_paths(setup: &Setup) -> Vec<String> {
	finished(setup).into_keys().collect()
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
		// the same name in another folder
		("other/a.txt", b"another alpha"),
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

	assert_eq!(created_dirs(&setup), ["sample", "docs", "sub", "other"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("sample/docs/a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			(
				"sample/docs/sub/big.bin".to_owned(),
				(big.len() as u64, 3, hash(&big))
			),
			("sample/empty.txt".to_owned(), (0, 0, hash(b""))),
			(
				"sample/other/a.txt".to_owned(),
				(13, 1, hash(b"another alpha"))
			),
		])
	);
	assert_eq!(
		report.counts,
		ItemCounts {
			dirs_created: 4,
			files_done: 4,
			bytes_done: 5 + big.len() as u64 + 13,
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

/// The report's renames as path, name and reason, sorted: they are recorded as entries finish.
fn renames(report: &ExtractReport) -> Vec<(&str, &str, ExtractRenameReason)> {
	let mut renames: Vec<_> = report
		.renamed
		.iter()
		.map(|r| (r.path.as_str(), r.name.as_str(), r.reason))
		.collect();
	renames.sort_by_key(|(path, ..)| *path);
	renames
}

/// The report's failures as path, name it was to get, stage and error kind, sorted.
fn failures(report: &ExtractReport) -> Vec<(&str, &str, ExtractStage, ErrorKind)> {
	let mut failures: Vec<_> = report
		.failures
		.iter()
		.map(|f| {
			(
				f.path.as_str(),
				f.dest_name.as_str(),
				f.stage,
				f.error.kind(),
			)
		})
		.collect();
	failures.sort_by_key(|(path, ..)| *path);
	failures
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
	assert_eq!(
		finished_paths(&setup),
		["docs (1)/x.txt", "other.txt", "readme (1).txt"]
	);
	assert_eq!(
		renames(&report),
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
async fn a_directory_renamed_twice_is_reported_once_by_its_archive_path() {
	let tar = tar_of(&[("docs/x.txt", b"x"), ("docs/sub/y.txt", b"y")]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend.listed = ListedNames {
			names: vec!["docs".into()],
			unverified: false,
		};
		// the keep-both name picked from the listing was taken since
		backend.merge_once.lock().unwrap().insert("docs (1)".into());
		backend.fail_create.insert("sub".into(), ErrorKind::Server);
	});
	let options = Options {
		root: ExtractRoot::Destination,
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(finished_paths(&setup), ["docs (2)/x.txt"]);
	assert_eq!(
		renames(&report),
		[("docs", "docs (2)", ExtractRenameReason::DuplicateName)]
	);
	// by where they are in the archive, not the names they were to be created under
	assert_eq!(
		failures(&report),
		[
			(
				"docs/sub",
				"sub",
				ExtractStage::CreateDirectory,
				ErrorKind::Server
			),
			(
				"docs/sub/y.txt",
				"y.txt",
				ExtractStage::CreateDirectory,
				ErrorKind::Server
			),
		]
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
	assert_eq!(finished_paths(&setup), ["broken/first.txt"]);
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
		created_dirs(&setup).len() <= MAX_MEMBERS as usize + 1,
		"no more directories than the cap, besides the new folder"
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(start_paused = true)]
async fn directories_are_planned_only_as_fast_as_they_are_created() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend
			.slow
			.insert("blocked".to_owned(), Duration::from_secs(3600));
	});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	let below = MAX_UNCREATED_DIRS + 10;
	let entry = |ordinal: usize| match ordinal {
		0 => dir_entry(0, "blocked"),
		_ => dir_entry(ordinal as u64, &format!("blocked/d{ordinal:04}")),
	};
	events
		.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registrations_run_bounded() {
	let names: Vec<String> = (0..3 * MAX_SMALL_PARALLEL_REQUESTS)
		.map(|i| format!("e{i:03}"))
		.collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let setup = setup("empty.tar", tar_of(&members), |backend| {
		for name in &names {
			backend
				.slow_finish
				.insert(name.clone(), Duration::from_millis(20));
		}
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, names.len() as u64);
	assert_eq!(
		setup.backend.log().peak_finishes,
		MAX_SMALL_PARALLEL_REQUESTS,
		"as many at once as other small requests, and no more"
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(start_paused = true)]
async fn files_are_read_only_as_fast_as_they_are_registered() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		// another client holds the drive lock from the first registration on (the folder's
		// create is the first acquisition)
		backend.block_locks_from.send_replace(Some(1));
	});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	events
		.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
		.await
		.unwrap();
	let count = MAX_OPEN_FILES + 10;
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
		Some(MAX_OPEN_FILES),
		"the codec waits for the files to be registered"
	);
	drop(events);
	let _ = result.send(read_in_full());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, count as u64);
	assert_released(&setup, &job.reporter);
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
	wait_until("done.txt is registered and stuck.bin uploading", || {
		finished(&setup).contains_key("c/done.txt")
			&& job
				.reporter
				.read(|state| state.active_names().contains(&"stuck.bin".to_owned()))
	})
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 1,
			files_done: 1,
			bytes_done: 4,
			// the dropped file, at the size the archive states
			files_not_attempted: 1,
			bytes_not_attempted: CHUNK_SIZE as u64,
			..ItemCounts::default()
		}
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

/// An archive with a hash in its metadata, placed in the fake drive in `parent`.
fn disposable(
	bytes: Vec<u8>,
	hash: Option<Blake3Hash>,
	configure: impl FnOnce(&mut FakeBackend),
) -> (Setup, Uuid) {
	let parent = Uuid::new_v4();
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

fn disposition(report: &ExtractReport) -> DisposalOutcome {
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
		dispose: Some((how, parent)),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_released(&setup, &job.reporter);
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

	// a file that failed to upload
	let (setup, report) = {
		let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |backend| {
			backend
				.fail_upload
				.insert("a.txt".to_owned(), ErrorKind::Server);
		});
		let options = Options {
			dispose: Some((SourceDisposal::Trash, parent)),
			..Options::default()
		};
		let job = start(&setup, options);
		let report = job.running.await.unwrap().unwrap();
		(setup, report)
	};
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_archive_that_changed_or_whose_output_is_gone_is_kept() {
	let tar = good_tar();
	// moved elsewhere while it was extracted
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	setup
		.backend
		.place_file(setup.archive.uuid(), Uuid::new_v4(), tar.len() as u64);
	let job = start(
		&setup,
		Options {
			dispose: Some((SourceDisposal::Trash, parent)),
			..Options::default()
		},
	);
	let report = job.running.await.unwrap().unwrap();
	assert!(matches!(kept(disposition(&report)), KeptReason::Changed));
	assert!(setup.backend.log().trashed_files.is_empty());

	// the extracted folder lost a file before the check: the fake drive forgets every file
	// registered from now on, so the re-listing finds fewer than were created
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |backend| {
		backend.forget_registered = true;
	});
	let job = start(
		&setup,
		Options {
			dispose: Some((SourceDisposal::DeletePermanently, parent)),
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

fn zip_of(entries: &[(&str, Option<&[u8]>)], password: Option<&[u8]>) -> Vec<u8> {
	let mut writer = ZipWriter::new(Vec::new());
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None).unwrap(),
			Some(data) => {
				let encryption = password.map(|password| Encryption {
					password,
					strength: AesStrength::Aes128,
					salt: vec![1; 8],
				});
				writer
					.add_file(
						path,
						None,
						data.len() as u64,
						ZipMethod::Deflate { level: 6 },
						encryption,
						&mut &data[..],
					)
					.unwrap();
			}
		}
	}
	writer.finish().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_an_encrypted_zip_and_removes_it() {
	let big = pattern(2 * CHUNK_SIZE + 5, 6);
	let zip = zip_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
		],
		Some(b"pw"),
	);
	// a zip's entries are checked one by one, so no hash of the whole archive is needed
	let (setup, parent) = disposable(zip, None, |_| {});
	let options = Options {
		dispose: Some((SourceDisposal::DeletePermanently, parent)),
		password: Some(ArchivePassword::new("pw".into()).unwrap()),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(created_dirs(&setup), ["bundle", "docs"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("bundle/docs/a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			(
				"bundle/docs/big.bin".to_owned(),
				(big.len() as u64, 3, hash(&big))
			),
		])
	);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().deleted_files, [setup.archive.uuid()]);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_without_its_password_creates_nothing() {
	let zip = zip_of(&[("a.txt", Some(b"a"))], Some(b"pw"));
	let setup = setup("s.zip", zip, |_| {});
	let failed = start(&setup, Options::default())
		.running
		.await
		.unwrap()
		.unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchivePasswordRequired);
	assert!(created_dirs(&setup).is_empty());
	let options = Options {
		password: Some(ArchivePassword::new("nope".into()).unwrap()),
		..Options::default()
	};
	let failed = start(&setup, options).running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(created_dirs(&setup).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_with_duplicate_names_is_kept() {
	let zip = zip_of(&[("same", Some(b"1")), ("same", Some(b"2"))], None);
	let (setup, parent) = disposable(zip, None, |_| {});
	let options = Options {
		dispose: Some((SourceDisposal::Trash, parent)),
		..Options::default()
	};
	let report = start(&setup, options).running.await.unwrap().unwrap();
	assert_eq!(report.duplicates.as_ref().map(|d| d.count), Some(1));
	assert!(matches!(kept(disposition(&report)), KeptReason::Incomplete));
	assert!(setup.backend.log().trashed_files.is_empty());
}

fn sevenz_of(entries: &[(&str, Option<&[u8]>)], password: Option<&str>) -> Vec<u8> {
	let password: Option<Vec<u8>> =
		password.map(|password| password.encode_utf16().flat_map(u16::to_le_bytes).collect());
	let mut writer = SevenZWriter::with_cycles_power(
		Vec::new(),
		SevenZMethod::Lzma2 { level: 1 },
		true,
		password
			.as_deref()
			.map(|password| (SevenZEncryption::EntriesAndHeaders, password)),
		4,
	)
	.unwrap();
	for (path, data) in entries {
		match data {
			None => writer.add_dir(path, None),
			Some(data) => {
				writer
					.add_file(path, None, data.len() as u64, &mut &data[..])
					.unwrap();
			}
		}
	}
	let (mut archive, start) = writer.finish().unwrap();
	archive[..32].copy_from_slice(&start);
	archive
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_an_encrypted_7z_and_removes_it() {
	let big = pattern(2 * CHUNK_SIZE + 5, 6);
	let archive = sevenz_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
		],
		Some("pw"),
	);
	// a 7z's entries are checked one by one, so no hash of the whole archive is needed
	let (setup, parent) = disposable(archive, None, |_| {});
	let options = Options {
		dispose: Some((SourceDisposal::DeletePermanently, parent)),
		password: Some(ArchivePassword::new("pw".into()).unwrap()),
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	// the files come before their directory's own entry, which then merges with it
	assert_eq!(created_dirs(&setup), ["bundle", "docs"]);
	assert_eq!(
		finished(&setup),
		BTreeMap::from([
			("bundle/docs/a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			(
				"bundle/docs/big.bin".to_owned(),
				(big.len() as u64, 3, hash(&big))
			),
		])
	);
	assert!(matches!(
		disposition(&report),
		DisposalOutcome::Disposed { .. }
	));
	assert_eq!(setup.backend.log().deleted_files, [setup.archive.uuid()]);
	assert_released(&setup, &job.reporter);
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
		("s.7z", sevenz_of(&entries, None)),
	] {
		// what the index states, 9 bytes, reaches the limit
		let refused = setup(name, archive.clone(), |_| {});
		let options = Options {
			max_bytes: Some(9),
			..Options::default()
		};
		let failed = start(&refused, options).running.await.unwrap().unwrap_err();
		assert_eq!(failed.error.kind(), ErrorKind::MaxStorageReached, "{name}");
		assert!(created_dirs(&refused).is_empty(), "{name}");
		assert!(finished(&refused).is_empty(), "{name}");

		let fits = setup(name, archive, |_| {});
		let options = Options {
			max_bytes: Some(10),
			..Options::default()
		};
		let report = start(&fits, options).running.await.unwrap().unwrap();
		assert_eq!(report.counts.bytes_done, 9, "{name}");
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_7z_with_a_wrong_password_creates_nothing() {
	let archive = sevenz_of(&[("a.txt", Some(b"a"))], Some("pw"));
	let setup = setup("s.7z", archive, |_| {});
	let options = Options {
		password: Some(ArchivePassword::new("nope".into()).unwrap()),
		..Options::default()
	};
	let failed = start(&setup, options).running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(created_dirs(&setup).is_empty());
	assert!(finished(&setup).is_empty());
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
async fn items_past_the_reports_records_reach_new_shares_too() {
	let names: Vec<String> = (0..1005).map(|i| format!("f{i:04}.txt")).collect();
	let members: Vec<(&str, &[u8])> = names
		.iter()
		.map(|name| (name.as_str(), &b"x"[..]))
		.collect();
	let setup = setup("bundle.tar", tar_of(&members), |backend| {
		// the destination is shared while the job runs
		backend.later_targets = Some(crate::connect::ConnectedTargets::with_test_users(1));
	});
	let options = Options {
		root: ExtractRoot::Destination,
		..Options::default()
	};
	let job = start(&setup, options);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.top_level.len(), 1000);
	assert_eq!(report.omitted.top_level, 5);
	let log = setup.backend.log();
	let beyond: Vec<Uuid> = log.fetched_items.clone();
	assert_eq!(
		beyond.len(),
		5,
		"only the items the report keeps no record of are fetched"
	);
	let propagated: std::collections::HashSet<Uuid> =
		log.propagated_trees.iter().copied().collect();
	let kept: Vec<Uuid> = report.top_level.iter().map(|top| top.item.uuid()).collect();
	assert!(
		kept.iter()
			.chain(&beyond)
			.all(|uuid| propagated.contains(uuid)),
		"every top-level item reaches the new share"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wrong_password_found_late_trashes_the_directories_it_left() {
	// too large (incompressible, so compressed too) to check the password on up front: it
	// shows once the entry is opened
	let mut state = 0x9E37_79B9_7F4A_7C15u64;
	let big: Vec<u8> = (0..17 << 20)
		.map(|_| {
			state ^= state << 13;
			state ^= state >> 7;
			state ^= state << 17;
			state as u8
		})
		.collect();
	let zip = zip_of(
		&[("docs", None), ("docs/big.bin", Some(&big))],
		Some(b"right"),
	);
	// the entry's first chunk comes slowest, long after the folder is created
	let setup = setup("bundle.zip", zip, |backend| backend.reverse_chunks = true);
	let options = Options {
		password: Some(ArchivePassword::new("wrong".into()).unwrap()),
		..Options::default()
	};
	let failed = start(&setup, options).running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(finished(&setup).is_empty());
	let log = setup.backend.log();
	let root = log
		.created_dirs
		.iter()
		.find(|(_, name)| name == "bundle")
		.map(|(uuid, _)| *uuid)
		.expect("the new folder was created before the password showed wrong");
	assert_eq!(
		log.trashed_dirs,
		[root],
		"the folder, with everything in it, is trashed"
	);
}

/// Settings with one job slot, for jobs that compete for it.
fn one_slot() -> ArchiveConfig {
	let mut config = ArchiveConfig::new(CODEC_MEM_BUDGET, 1);
	config.max_members = MAX_MEMBERS;
	config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_before_it_starts_takes_no_slot() {
	let config = one_slot();
	let setup_paused = setup("paused.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	let paused = start(
		&setup_paused,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("the job reports itself paused", || {
		paused.reporter.is_paused()
	})
	.await;

	// the only slot is free for a job started later
	let setup_other = setup("other.tar", tar_of(&[("b.txt", b"b")]), |_| {});
	let other = start(
		&setup_other,
		Options {
			config: config.clone(),
			..Options::default()
		},
	);
	tokio::time::timeout(Duration::from_secs(20), other.running)
		.await
		.expect("the unpaused job runs")
		.unwrap()
		.unwrap();
	assert!(setup_paused.backend.log().fetched.is_empty());
	assert!(created_dirs(&setup_paused).is_empty());
	assert!(paused.reporter.is_paused());
	assert_eq!(config.free_slots(), 1);

	pause.send_replace(false);
	let report = paused.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 1);
	assert_eq!(
		paused.recorder.run_states(),
		[RunState::Paused, RunState::Running]
	);
	assert_released(&setup_paused, &paused.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_paused_while_queued_leaves_the_slot_to_the_next() {
	let config = one_slot();
	let setup_first = setup("first.tar", tar_of(&[("stuck.bin", b"stuck")]), |backend| {
		backend.blocked_uploads.insert("stuck.bin".to_owned());
	});
	let (_pause, cancel_first, control) = controls();
	let first = start(
		&setup_first,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("the first job holds the slot", || {
		first
			.reporter
			.read(|state| state.active_names() == ["stuck.bin"])
	})
	.await;

	let setup_queued = setup("queued.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (pause, _cancel, control) = controls();
	let queued = start(
		&setup_queued,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	pause.send_replace(true);
	wait_until("the queued job reports itself paused", || {
		queued.reporter.is_paused()
	})
	.await;
	cancel_first.send_replace(true);
	first.running.await.unwrap().unwrap_err();

	let setup_next = setup("next.tar", tar_of(&[("b.txt", b"b")]), |_| {});
	let next = start(
		&setup_next,
		Options {
			config: config.clone(),
			..Options::default()
		},
	);
	tokio::time::timeout(Duration::from_secs(20), next.running)
		.await
		.expect("the slot the paused job waited for is free")
		.unwrap()
		.unwrap();
	assert!(setup_queued.backend.log().fetched.is_empty());

	pause.send_replace(false);
	let report = queued.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 1);
	assert_released(&setup_queued, &queued.reporter);
	assert_eq!(config.free_slots(), 1);
	assert!(config.floor_is_free());
}

/// Asserts a paused job holds nothing it gives back: memory, its floor, a drive lock, an
/// operation in flight.
fn assert_paused_holding_nothing(setup: &Setup, job: &Job, config: &ArchiveConfig) {
	assert!(job.reporter.is_paused());
	assert_eq!(
		setup.backend.memory.available_permits(),
		setup.backend.budget,
		"a paused job holds no memory"
	);
	assert!(config.floor_is_free(), "a paused job holds no floor");
	assert_eq!(
		setup.backend.live_locks.load(Ordering::SeqCst),
		0,
		"a paused job holds no drive lock"
	);
	assert_eq!(job.reporter.ops_in_flight(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_while_a_registration_waits_for_the_lock_holds_nothing() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		// another client holds the drive lock from the registration on (the folder's create
		// is the first acquisition)
		backend.block_locks_from.send_replace(Some(1));
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("a.txt's registration waits for the lock", || {
		setup.backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job, &config);
	assert!(finished(&setup).is_empty());

	setup.backend.block_locks_from.send_replace(None);
	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(report.counts.files_done, 1);
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Running
		]
	);
	assert_released(&setup, &job.reporter);
}

/// A file entry at `path`, as the codec sends it.
fn file_entry(ordinal: u64, path: &str, size: u64) -> WorkerEvent {
	WorkerEvent::Entry(EntryHead {
		ordinal,
		path: entry_path(path).unwrap(),
		modified: None,
		kind: EntryKind::File { size: Some(size) },
	})
}

/// How a scripted codec ends an archive it read in full.
fn read_in_full() -> CodecResult {
	Ok(ArchiveEnd {
		unaccounted_bytes: 0,
		duplicates: None,
		unchecked_entries: 0,
	})
}

#[tokio::test(start_paused = true)]
async fn a_pause_while_the_archive_opens_holds_nothing() {
	// larger than the floor, so chunks are prefetched on the client's budget
	let setup = setup("bundle.tar", pattern(3 * CHUNK_SIZE, 2), |backend| {
		// the destination's shares take long to fetch, so the pause comes while opening
		backend.targets_delay = Duration::from_secs(30);
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
		.await
		.unwrap();
	// chunks are prefetched (their memory taken) before the event is, and wait meanwhile
	wait_until("the job opens the archive", || {
		setup.backend.log().target_fetches == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job, &config);
	assert!(created_dirs(&setup).is_empty(), "the folder waits too");

	pause.send_replace(false);
	for event in [
		file_entry(0, "a.txt", 1),
		WorkerEvent::Data(b"a".to_vec()),
		WorkerEvent::FileEnd,
	] {
		events.send(event).await.unwrap();
	}
	drop(events);
	let _ = result.send(read_in_full());
	job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt"]);
	assert_eq!(
		job.recorder.run_states(),
		[RunState::Running, RunState::Paused, RunState::Running],
		"the pause is taken up once the archive is opened, with nothing left in flight"
	);
	assert_released(&setup, &job.reporter);
}

/// Operations in flight while directories are created as many at once as they can be: those
/// creates, and the archive's one chunk, which a scripted codec never asks for.
const CREATING_WITH_THE_ARCHIVE: u64 = MAX_SMALL_PARALLEL_REQUESTS as u64 + 1;

/// A directory entry at `path`, as the codec sends it.
fn dir_entry(ordinal: u64, path: &str) -> WorkerEvent {
	WorkerEvent::Entry(EntryHead {
		ordinal,
		path: entry_path(path).unwrap(),
		modified: None,
		kind: EntryKind::Dir,
	})
}

#[tokio::test(start_paused = true)]
async fn a_pause_leaves_no_directory_uncreated() {
	// more directories than are created at once, the rest waiting their turn
	let count = MAX_SMALL_PARALLEL_REQUESTS + 10;
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.delay = Duration::from_secs(10);
	});
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
		.await
		.unwrap();
	for ordinal in 0..count {
		events
			.send(dir_entry(ordinal as u64, &format!("d{ordinal:03}")))
			.await
			.unwrap();
	}
	drop(events);
	let _ = result.send(read_in_full());
	wait_until("the first directories are being created", || {
		job.reporter.ops_in_flight() == CREATING_WITH_THE_ARCHIVE
	})
	.await;
	// a moment of the creates' ten seconds, for the driver to take the archive's end
	tokio::time::sleep(Duration::from_secs(1)).await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert!(!job.running.is_finished(), "the rest are created on resume");

	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.dirs_created, count as u64 + 1);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_leaves_the_directories_not_created_yet_not_attempted() {
	let count = MAX_SMALL_PARALLEL_REQUESTS + 10;
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.delay = Duration::from_secs(10);
	});
	let (_pause, cancel, control) = controls();
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			control,
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	events
		.send(WorkerEvent::Opened(StreamLayout::Tar { codec: None }))
		.await
		.unwrap();
	for ordinal in 0..count {
		events
			.send(dir_entry(ordinal as u64, &format!("d{ordinal:03}")))
			.await
			.unwrap();
	}
	wait_until(
		"every directory is planned, the first being created",
		|| {
			events.capacity() == events.max_capacity()
				&& job.reporter.ops_in_flight() == CREATING_WITH_THE_ARCHIVE
		},
	)
	.await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	drop((events, result));
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			// the new folder, and the creates in flight, which finish
			dirs_created: MAX_SMALL_PARALLEL_REQUESTS as u64 + 1,
			dirs_not_attempted: 10,
			..ItemCounts::default()
		}
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_mid_extraction_holds_nothing_and_changes_nothing() {
	let files: Vec<(String, Vec<u8>)> = (0..3)
		.map(|i| (format!("f{i}.bin"), pattern(2 * CHUNK_SIZE + i, i as u8)))
		.collect();
	let members: Vec<(&str, &[u8])> = files
		.iter()
		.map(|(name, data)| (name.as_str(), &data[..]))
		.collect();
	let setup = setup("bundle.tar", tar_of(&members), |backend| {
		backend
			.slow
			.insert("f1.bin".to_owned(), Duration::from_millis(300));
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			..Options::default()
		},
	);
	wait_until("f1.bin's first chunk uploads", || {
		setup.backend.log().upload_starts.len() > 2
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job, &config);

	pause.send_replace(false);
	job.running.await.unwrap().unwrap();
	let expected: BTreeMap<String, (u64, u64, Blake3Hash)> = files
		.iter()
		.map(|(name, data)| {
			(
				format!("bundle/{name}"),
				(
					data.len() as u64,
					data.len().div_ceil(CHUNK_SIZE) as u64,
					hash(data),
				),
			)
		})
		.collect();
	assert_eq!(finished(&setup), expected);
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Running
		]
	);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_paused_winds_down() {
	let setup = setup(
		"bundle.tar",
		tar_of(&[("a.bin", &pattern(3 * CHUNK_SIZE, 4))]),
		|backend| {
			backend
				.slow
				.insert("a.bin".to_owned(), Duration::from_millis(300));
		},
	);
	let (pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("a chunk uploads", || {
		!setup.backend.log().upload_starts.is_empty()
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	assert!(finished(&setup).is_empty());
	assert_eq!(
		job.recorder.run_states(),
		[
			RunState::Running,
			RunState::Pausing,
			RunState::Paused,
			RunState::Cancelling
		]
	);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Cancelled);
	assert_released(&setup, &job.reporter);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_lifted_before_the_job_went_idle_loses_no_file() {
	// b.txt's registration runs long; a.bin's upload ends while the pause is requested, and the
	// pause is lifted while b.txt still registers, so the job never goes idle
	let big = pattern(CHUNK_SIZE + 3, 5);
	let tar = tar_of(&[("b.txt", b"b"), ("a.bin", &big[..])]);
	let setup = setup("bundle.tar", tar, |backend| {
		backend
			.slow_finish
			.insert("b.txt".into(), Duration::from_secs(2));
		backend
			.slow
			.insert("a.bin".into(), Duration::from_millis(600));
	});
	let (pause, _cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("b.txt registers and a.bin's chunks upload", || {
		let log = setup.backend.log();
		log.finishing.contains(&"b.txt".to_owned()) && log.upload_starts.len() == 3
	})
	.await;
	pause.send_replace(true);
	wait_until("a.bin's chunks are uploaded", || {
		setup.backend.log().uploaded.len() == 3
	})
	.await;
	assert!(
		!finished(&setup).contains_key("bundle/b.txt"),
		"b.txt still registers: the job has not gone idle"
	);
	pause.send_replace(false);
	let report = tokio::time::timeout(Duration::from_secs(20), job.running)
		.await
		.expect("the job finishes")
		.unwrap()
		.unwrap();
	assert_eq!(report.counts.files_done, 2);
	assert_eq!(finished(&setup).len(), 2);
	assert_eq!(
		job.recorder.run_states(),
		[RunState::Running, RunState::Pausing, RunState::Running],
		"never paused: a registration was in flight all along"
	);
}
