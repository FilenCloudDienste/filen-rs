//! The driver against the fake drive. Most tests run the real codec on its thread, so they run in
//! real time on a multi-threaded runtime; the stall test scripts a silent codec on paused time.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	io::Write,
	sync::{Mutex, atomic::Ordering},
	time::Duration,
};

use tokio::{sync::watch, task::JoinHandle};

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	fs::{
		archive::{
			config::{CODEC_MEM_BUDGET, JOB_CONCURRENCY},
			entry_path::entry_path,
			extract::{
				ArchiveEntry, ArchiveListing, ArchiveTotals, ExpansionLimit, ExtractCallback,
				ExtractEvent, ExtractSkipReason, ExtractUpdate, ListCallback, ListFailed,
				ListPhase, ListTotals, ListUpdate, MAX_LISTED_BYTES, MAX_LISTED_ENTRIES,
				PasswordCheck, RunState,
				codec::{CodecLimits, Selection, StreamJob, Task, extract_stream},
				list::{ListReporter, ListTask, run_list},
				report::CALLBACK_BATCH,
			},
			worker,
		},
		archive::{
			dispose::{DisposalOutcome, KeptReason, SourceDisposal},
			extract::ExtractFailure,
			format::StreamCodec,
			password::ArchivePassword,
			sevenz::write::{SevenZEncryption, SevenZMethod},
			test_support::{gzip, incompressible, pattern, remote_file, sevenz_of, tar_of, zip_of},
			worker::LinkHead,
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			counts::ItemCounts,
			test_support::{FakeBackend, Request, wait_until},
		},
	},
	job::test_support::controls,
};

/// What a job held when an update reported it paused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HeldWhilePaused {
	/// Whether the client's memory budget was all free.
	memory_free: bool,
	/// Whether every job's floor was free (the job's own, when no other runs).
	floor_free: bool,
	drive_locks: usize,
}

/// Tells what a job holds right now.
type Probe = Box<dyn Fn() -> HeldWhilePaused + Send + Sync>;

#[derive(Default)]
struct Recorder {
	top_level: Mutex<Vec<ExtractedTopLevel>>,
	/// The size of each `on_top_level_created` batch.
	batches: Mutex<Vec<usize>>,
	updates: Mutex<Vec<ExtractUpdate>>,
	probe: Option<Probe>,
	/// What the job held at each update reporting it paused, told by `probe`.
	held_while_paused: Mutex<Vec<HeldWhilePaused>>,
}

impl ExtractCallback for Recorder {
	fn on_top_level_created(&self, items: Vec<ExtractedTopLevel>) {
		self.batches.lock().unwrap().push(items.len());
		self.top_level.lock().unwrap().extend(items);
	}

	fn on_update(&self, update: ExtractUpdate) {
		if update.run_state == RunState::Paused
			&& let Some(probe) = &self.probe
		{
			self.held_while_paused.lock().unwrap().push(probe());
		}
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

/// The archive every test extracts, the directory it is in, and the one it is extracted into:
/// fixed, so a failing test runs the same way again.
const ARCHIVE: Uuid = Uuid::from_u128(0xA);
const ARCHIVE_PARENT: Uuid = Uuid::from_u128(0xA0);
const DESTINATION: Uuid = Uuid::from_u128(0xD);

fn archive_file(name: &str, bytes: &[u8]) -> RemoteFileType<'static> {
	archive_file_with(name, bytes, None)
}

fn archive_file_with(
	name: &str,
	bytes: &[u8],
	hash: Option<Blake3Hash>,
) -> RemoteFileType<'static> {
	remote_file(ARCHIVE, ARCHIVE_PARENT, name, bytes, hash)
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
	setup_in(DESTINATION, name, bytes, configure)
}

/// [`setup`] for a job that extracts into `destination`.
fn setup_in(
	destination: Uuid,
	name: &str,
	bytes: Vec<u8>,
	configure: impl FnOnce(&mut FakeBackend),
) -> Setup {
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

/// The method the tests' 7z archives are compressed with.
const LZMA2: SevenZMethod = SevenZMethod::Lzma2 { level: 1 };

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
	/// Every entry, or those a partial extraction chose.
	selection: Option<Selection>,
	expansion: Option<ExpansionLimit>,
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
			selection: None,
			expansion: Some(ExpansionLimit::DEFAULT),
		}
	}
}

fn start_with(
	setup: &Setup,
	options: Options,
	start: Box<dyn FnOnce() -> Result<WorkerLink<CodecResult>, Error> + Send>,
) -> Job {
	let probe: Probe = {
		let memory = Arc::clone(&setup.backend.memory);
		let budget = setup.backend.budget;
		let config = options.config.clone();
		let live_locks = Arc::clone(&setup.backend.live_locks);
		Box::new(move || HeldWhilePaused {
			memory_free: memory.available_permits() == budget,
			floor_free: config.floor_is_free(),
			drive_locks: live_locks.load(Ordering::SeqCst),
		})
	};
	let recorder = Arc::new(Recorder {
		probe: Some(probe),
		..Recorder::default()
	});
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
		expansion: options.expansion,
		base: options
			.selection
			.as_ref()
			.map(|selection| selection.base().to_vec())
			.unwrap_or_default(),
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

/// What the real codec is given for `setup`'s archive.
fn stream_job(setup: &Setup, options: &Options) -> StreamJob {
	StreamJob {
		name: setup.archive.name().unwrap().to_owned(),
		len: setup.archive.size(),
		limits: CodecLimits {
			decoder_memory: CODEC_MEM_BUDGET,
			max_members: options.config.max_members,
			expansion: options.expansion,
			max_index_bytes: 32 << 20,
			max_bytes: options.max_bytes,
		},
		password: options.password.clone(),
		skip_mac_metadata: true,
		task: Task::Extract(options.selection.clone()),
	}
}

/// Runs the real codec on its own thread.
fn start(setup: &Setup, options: Options) -> Job {
	let job = stream_job(setup, &options);
	start_with(
		setup,
		options,
		Box::new(move || worker::start(move |port| extract_stream(&port, job))),
	)
}

/// Everything a finished job must have given back.
fn assert_released(setup: &Setup, reporter: &Reporter, recorder: &Recorder) {
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
	assert!(
		recorder
			.held_while_paused
			.lock()
			.unwrap()
			.iter()
			.all(|held| held.memory_free && held.drive_locks == 0),
		"no update reports the job paused while it holds memory or a lock"
	);
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn names_that_read_as_something_else_are_kept_and_reported() {
	let tar = tar_of(&[
		("plain.txt", b"plain"),
		("invoice\u{202E}fdp.exe", b"not a pdf"),
		("hidden\u{200B}/x.txt", b"x"),
	]);
	let setup = setup("bundle.tar", tar, |_| {});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();

	assert_eq!(
		finished_paths(&setup),
		[
			"bundle/hidden\u{200B}/x.txt",
			"bundle/invoice\u{202E}fdp.exe",
			"bundle/plain.txt"
		]
	);
	let entry = |index| ArchiveEntryId {
		archive: setup.archive.uuid(),
		index,
	};
	let expected = [
		ExtractMisleadingName {
			entry: entry(1),
			path: "invoice\u{202E}fdp.exe".to_owned(),
		},
		ExtractMisleadingName {
			entry: entry(2),
			path: "hidden\u{200B}/x.txt".to_owned(),
		},
	];
	assert_eq!(report.misleading_names, expected);
	let events: Vec<ExtractMisleadingName> = job
		.recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.flat_map(|update| &update.events)
		.filter_map(|event| match event {
			ExtractEvent::MisleadingName(name) => Some(name.clone()),
			_ => None,
		})
		.collect();
	assert_eq!(events, expected);
	// kept as they are: nothing was renamed
	assert!(report.renamed.is_empty());
	assert_released(&setup, &job.reporter, &job.recorder);
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

/// The uuid of the directory the job created as `name`.
fn log_dir(setup: &Setup, name: &str) -> Uuid {
	setup
		.backend
		.log()
		.created_dirs
		.iter()
		.find(|(_, created)| created == name)
		.map(|(uuid, _)| *uuid)
		.expect("the directory was created")
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
async fn a_new_folder_whose_name_was_taken_since_gets_the_next() {
	let setup = setup("bundle.tar", tar_of(&[("a.txt", b"a")]), |backend| {
		backend.merge_once.lock().unwrap().insert("bundle".into());
	});
	let job = start(&setup, Options::default());
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(finished_paths(&setup), ["bundle (1)/a.txt"]);
	let top: Vec<ExtractTopLevelKey> = report.top_level.iter().map(|top| top.key).collect();
	assert_eq!(top, [ExtractTopLevelKey::Root]);
	assert_released(&setup, &job.reporter, &job.recorder);
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
	let last = job.recorder.last();
	assert_eq!(last.phase, ExtractPhase::Failed);
	// what the job did not read it never will: no time is left
	assert_eq!(last.eta, Some(Duration::ZERO));
	assert_released(&setup, &job.reporter, &job.recorder);
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
		created_dirs(&setup).len() <= MAX_MEMBERS as usize + 1,
		"no more directories than the cap, besides the new folder"
	);
	assert_released(&setup, &job.reporter, &job.recorder);
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
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
	assert_eq!(
		setup.backend.log().finishing,
		names,
		"registered in archive order"
	);
	assert_released(&setup, &job.reporter, &job.recorder);
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
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
	let last = job.recorder.last();
	assert_eq!(last.phase, ExtractPhase::Cancelled);
	assert_eq!(last.eta, Some(Duration::ZERO));
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_silent_codec_is_given_up_on() {
	let setup = setup("silent.tar", tar_of(&[("a.txt", b"a")]), |_| {});
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(&setup, Options::default(), Box::new(move || Ok(link)));
	let failed = job.running.await.unwrap().unwrap_err();

	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWorkerDied);
	assert_released(&setup, &job.reporter, &job.recorder);
	// the codec's ends were held open all along
	drop((events, result));
}

/// An archive with a hash in its metadata, placed in the fake drive in `parent`.
fn disposable(
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
		[
			("__MACOSX/", &ExtractSkipReason::MacMetadata),
			("__MACOSX/._a.txt", &ExtractSkipReason::MacMetadata),
			("._a.txt", &ExtractSkipReason::MacMetadata),
		]
	);
	assert_eq!(report.counts.entries_skipped, 3);
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
			dispose: Some((SourceDisposal::DeletePermanently, parent)),
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
	let other = Reporter::new(
		Recorder::default(),
		ArchiveTotals::Streaming { archive_bytes: 0 },
	);
	let _running = config.admit(&JobControl::default(), &other.ops()).await;
	let (setup, parent) = disposable(tar.clone(), Some(hash(&tar)), |_| {});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			config: config.clone(),
			dispose: Some((SourceDisposal::Trash, parent)),
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
			dispose: Some((SourceDisposal::Trash, parent)),
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
			dispose: Some((SourceDisposal::Trash, parent)),
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
	assert_paused_holding_nothing(&setup, &job, &config);
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
	assert_released(&setup, &job.reporter, &job.recorder);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracts_an_encrypted_7z_and_removes_it() {
	let big = pattern(2 * CHUNK_SIZE + 5, 6);
	let archive = sevenz_of(
		&[
			("docs", None),
			("docs/a.txt", Some(b"alpha")),
			("docs/big.bin", Some(&big)),
		],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
async fn a_7z_with_a_wrong_password_creates_nothing() {
	let archive = sevenz_of(
		&[("a.txt", Some(b"a"))],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, "pw")),
	);
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
async fn a_cancel_stops_a_codec_deriving_a_7z_key() {
	// the longest password there is, in UTF-16 surrogate pairs: 4 KiB hashed 2^22 times, most of
	// a minute on wasm and seconds here
	let password = "\u{1F600}".repeat(1024);
	let mut archive = sevenz_of(
		&[("a.txt", Some(b"a"))],
		LZMA2,
		true,
		Some((SevenZEncryption::EntriesAndHeaders, &password)),
	);
	// written at 2^4 rounds to be quick to make; the header key is read at 2^22. Its AES coder's
	// properties sit in the plain part of the header: the rounds, then salt and IV
	let aes = [0x06, 0xF1, 0x07, 0x01, 34, 0xC0 | 4, 0xFF];
	let at = archive
		.windows(aes.len())
		.rposition(|window| window == aes)
		.expect("the header's AES coder");
	archive[at + 5] = 0xC0 | crate::fs::archive::sevenz::crypto::MAX_CYCLES_POWER;
	// the start header's CRC-32 of the header, then its own
	let next = 32 + u64::from_le_bytes(archive[12..20].try_into().unwrap()) as usize;
	let len = u64::from_le_bytes(archive[20..28].try_into().unwrap()) as usize;
	let crc = crc32fast::hash(&archive[next..next + len]);
	archive[28..32].copy_from_slice(&crc.to_le_bytes());
	let crc = crc32fast::hash(&archive[12..32]);
	archive[8..12].copy_from_slice(&crc.to_le_bytes());

	let setup = setup("slow.7z", archive, |_| {});
	let (_pause, cancel, control) = controls();
	let codec = Arc::new(Mutex::new(None));
	let job = {
		let codec = Arc::clone(&codec);
		let options = Options {
			control,
			password: Some(ArchivePassword::new(password).unwrap()),
			..Options::default()
		};
		let job = stream_job(&setup, &options);
		start_with(
			&setup,
			options,
			Box::new(move || {
				let link = worker::start(move |port| extract_stream(&port, job))?;
				*codec.lock().unwrap() = Some(Arc::clone(&link.shared));
				Ok(link)
			}),
		)
	};
	let shared = loop {
		if let Some(shared) = codec.lock().unwrap().take() {
			break shared;
		}
		tokio::time::sleep(Duration::from_millis(5)).await;
	};
	// the codec shows it is alive while it derives, which exchanges nothing with the driver
	while shared.progress() < 10 {
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	cancel.send_replace(true);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	// and stops deriving once the job ended, whatever the hashing speed: its thread lets go of
	// what it shared (a derivation run to its end would too, eventually), having shown progress
	// at most once more, for the 2^16 rounds under way when the job ended
	let at_end = shared.progress();
	let ended = tokio::time::Instant::now();
	while Arc::strong_count(&shared) > 1 {
		assert!(
			ended.elapsed() < Duration::from_secs(120),
			"the codec never stopped"
		);
		tokio::time::sleep(Duration::from_millis(5)).await;
	}
	assert!(
		shared.progress() - at_end <= 1,
		"the codec derived on for {} more checks",
		shared.progress() - at_end
	);
	assert!(created_dirs(&setup).is_empty());
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
	let big = incompressible(17 << 20, 0x9E37_79B9_7F4A_7C15);
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
	let job = start(&setup, options);
	let failed = job.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::ArchiveWrongPassword);
	assert!(finished(&setup).is_empty());
	// the entry failed to open before it was announced, so the job never started it; the
	// folders count as created, trashed since
	assert_eq!(
		failed.report.counts,
		ItemCounts {
			dirs_created: 2,
			..ItemCounts::default()
		}
	);
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
	assert_eq!(
		job.recorder.top_level.lock().unwrap().len(),
		1,
		"the callback got the folder"
	);
	assert!(
		failed.report.top_level.is_empty(),
		"the report no longer lists it as created"
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
	assert_released(&setup_paused, &paused.reporter, &paused.recorder);
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
	assert_released(&setup_queued, &queued.reporter, &queued.recorder);
	assert_eq!(config.free_slots(), 1);
	assert!(config.floor_is_free());
}

/// Asserts a paused job holds nothing it gives back: memory, its floor, a drive lock, an
/// operation in flight.
fn assert_paused_holding_nothing(setup: &Setup, job: &Job, config: &ArchiveConfig) {
	assert!(job.reporter.is_paused());
	let held = job.recorder.held_while_paused.lock().unwrap().clone();
	let nothing = HeldWhilePaused {
		memory_free: true,
		floor_free: true,
		drive_locks: 0,
	};
	assert!(
		!held.is_empty() && held.iter().all(|held| *held == nothing),
		"every update reporting the job paused found it holding nothing: {held:?}"
	);
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
		password: PasswordCheck::NotNeeded,
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
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter, &job.recorder);
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
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
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
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_archive_read_to_its_end_holds_no_memory() {
	let big = pattern(3 * CHUNK_SIZE, 8);
	let setup = setup("bundle.tar", tar_of(&[("big.bin", &big)]), |backend| {
		backend
			.slow_finish
			.insert("big.bin".to_owned(), Duration::from_secs(3));
	});
	let job = start(&setup, Options::default());
	wait_until("big.bin registers, the archive read", || {
		setup
			.backend
			.log()
			.finishing
			.contains(&"big.bin".to_owned())
	})
	.await;
	// while it still registers
	let released = wait_until("the codec's last chunk is given back", || {
		setup.backend.memory.available_permits() == setup.backend.budget
	});
	tokio::time::timeout(Duration::from_secs(2), released)
		.await
		.expect("the last chunk is given back once the codec ended");
	job.running.await.unwrap().unwrap();
	assert_released(&setup, &job.reporter, &job.recorder);
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
	assert_released(&setup, &job.reporter, &job.recorder);
}

#[tokio::test(start_paused = true)]
async fn a_pause_while_finishing_gives_back_the_input_and_the_lock() {
	// larger than the floor, and never asked for in full by the codec: chunks prefetched past
	// its last read, as a zip's index chunks fetched again for its entries
	let setup = setup("bundle.tar", pattern(3 * CHUNK_SIZE, 3), |backend| {
		// the destination is shared while the job runs: every item is propagated again
		backend.later_targets = Some(crate::connect::ConnectedTargets::with_test_users(1));
	});
	let config = test_config();
	let (pause, _cancel, control) = controls();
	let (events, result, link) = worker::scripted::<CodecResult>();
	let job = start_with(
		&setup,
		Options {
			root: ExtractRoot::Destination,
			control,
			config: config.clone(),
			..Options::default()
		},
		Box::new(move || Ok(link)),
	);
	// more top-level items than are propagated at once
	let count = MAX_SMALL_PARALLEL_REQUESTS + 6;
	events
		.send(WorkerEvent::Opened(ArchiveFormat::Tar { codec: None }))
		.await
		.unwrap();
	for ordinal in 0..count {
		for event in [
			file_entry(ordinal as u64, &format!("f{ordinal:03}"), 1),
			WorkerEvent::Data(vec![ordinal as u8]),
			WorkerEvent::FileEnd,
		] {
			events.send(event).await.unwrap();
		}
	}
	wait_until("every file is registered", || {
		setup.backend.log().finished.len() == count
	})
	.await;
	// the first batch propagated waits
	let registered: Vec<Uuid> = setup.backend.log().finished.keys().copied().collect();
	setup.backend.hold_requests(Request::Propagate, registered);
	drop(events);
	let _ = result.send(read_in_full());
	wait_until("the first batch propagates", || {
		setup.backend.log().held.len() == MAX_SMALL_PARALLEL_REQUESTS
	})
	.await;
	pause.send_replace(true);
	setup.backend.release_all();
	wait_until("the job is paused", || job.reporter.is_paused()).await;
	assert_paused_holding_nothing(&setup, &job, &config);
	assert_eq!(job.recorder.last().phase, ExtractPhase::Finishing);
	assert_eq!(
		setup.backend.log().propagated_trees.len(),
		MAX_SMALL_PARALLEL_REQUESTS,
		"the batch in flight finished, then the lock was given back"
	);

	pause.send_replace(false);
	job.running.await.unwrap().unwrap();
	assert_eq!(setup.backend.log().propagated_trees.len(), count);
	assert_released(&setup, &job.reporter, &job.recorder);
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

/// Appends a tar hard link at `path` to the member at `target`.
fn append_hard_link(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
	let mut header = tar::Header::new_gnu();
	header.set_entry_type(tar::EntryType::Link);
	header.set_size(0);
	builder.append_link(&mut header, path, target).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_is_extracted_as_a_copy_of_the_file_it_names() {
	let big = incompressible(2 * CHUNK_SIZE + 77, 0x51);
	let mut tar = tar_of(&[("docs/", b""), ("docs/a.bin", &big), ("empty", b"")]);
	// drop the end-of-archive blocks so the links follow on
	tar.truncate(tar.len() - 1024);
	let mut builder = tar::Builder::new(tar);
	append_hard_link(&mut builder, "docs/hard", "docs/a.bin");
	append_hard_link(&mut builder, "top.bin", "docs/a.bin");
	append_hard_link(&mut builder, "empty-link", "empty");
	// nothing to copy: a path no file came at, and a directory's
	append_hard_link(&mut builder, "gone", "missing.txt");
	append_hard_link(&mut builder, "dir-link", "docs");
	let tar = builder.into_inner().unwrap();
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
	let (shown, truncated) = crate::fs::archive::limits::display_path(path);
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
/// `before_link` holds, a hard link `b.txt` to it; the report.
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
	let setup = setup_slow_finish();
	let report = link_after(&setup, |setup| setup.backend.log().finishing == ["a.txt"])
		.await
		.unwrap();
	assert_eq!(finished_paths(&setup), ["bundle/a.txt", "bundle/b.txt"]);
	assert_eq!(report.counts.files_done, 2);
}

/// A drive where `a.txt` takes long to register.
fn setup_slow_finish() -> Setup {
	setup("bundle.tar", Vec::new(), |backend| {
		backend.keep_uploads = true;
		backend
			.slow_finish
			.insert("a.txt".to_owned(), Duration::from_millis(200));
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
		backend
			.slow_finish
			.insert("a.txt".to_owned(), Duration::from_millis(200));
		backend
			.fail_finish
			.insert("a.txt".to_owned(), ErrorKind::Server);
	});
	let report = link_after(&unregistered, |setup| {
		setup.backend.log().finishing == ["a.txt"]
	})
	.await
	.unwrap();
	assert!(skipped_link(&report), "{:?}", report.skipped);
	assert_eq!(report.counts.files_failed, 1);
}

/// Options that extract only the entries at `indices`, relative to `base`, into the destination.
fn chosen(indices: &[u32], base: &[&str]) -> Options {
	Options {
		root: ExtractRoot::Destination,
		selection: Some(Selection::new(
			indices.iter().map(|&index| u64::from(index)),
			base.iter()
				.map(|segment| ValidatedName::try_from(*segment).unwrap())
				.collect(),
		)),
		..Options::default()
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chosen_entries_are_extracted_with_the_directories_that_hold_them() {
	let tar = tar_of(&[
		("docs/", b""),
		("docs/sub/a.txt", b"alpha"),
		("docs/sub/b.txt", b"beta"),
		("other.txt", b"other"),
	]);
	for (indices, base, expected) in [
		// a file, and the directories it is in
		(&[1][..], &[][..], &["docs/sub/a.txt"][..]),
		// what is in a directory, as the destination's own
		(&[0], &["docs"], &["sub/a.txt", "sub/b.txt"]),
		(&[1, 3], &[], &["docs/sub/a.txt", "other.txt"]),
	] {
		let setup = setup("bundle.tar", tar.clone(), |_| {});
		let job = start(&setup, chosen(indices, base));
		let report = job.running.await.unwrap().unwrap();
		assert_eq!(finished_paths(&setup), expected, "{indices:?} {base:?}");
		// what was left out is neither skipped nor counted
		assert_eq!(report.counts.entries_skipped, 0);
		assert_eq!(report.counts.files_done, expected.len() as u64);
		assert_released(&setup, &job.reporter, &job.recorder);
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

#[derive(Default)]
struct ListRecorder {
	/// The size of each `on_entries` batch, and every entry.
	batches: Mutex<Vec<usize>>,
	entries: Mutex<Vec<ArchiveEntry>>,
	updates: Mutex<Vec<ListUpdate>>,
}

impl ListCallback for ListRecorder {
	fn on_entries(&self, entries: Vec<ArchiveEntry>) {
		self.batches.lock().unwrap().push(entries.len());
		self.entries.lock().unwrap().extend(entries);
	}

	fn on_update(&self, update: ListUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

struct Listing {
	running: JoinHandle<Result<ArchiveListing, ListFailed>>,
	recorder: Arc<ListRecorder>,
	reporter: MaybeArc<ListReporter>,
}

/// Lists `setup`'s archive with the real codec.
fn list(setup: &Setup, control: JobControl, config: ArchiveConfig) -> Listing {
	let recorder = Arc::new(ListRecorder::default());
	let reporter = ListReporter::new(Arc::clone(&recorder), setup.archive.size());
	let job = StreamJob {
		task: Task::List {
			archive: setup.archive.uuid(),
		},
		..stream_job(
			setup,
			&Options {
				config: config.clone(),
				..Options::default()
			},
		)
	};
	let running = tokio::spawn(run_list(ListTask {
		backend: Arc::clone(&setup.backend),
		control,
		reporter: MaybeArc::clone(&reporter),
		archive: setup.archive.clone(),
		config,
		start: Box::new(move || worker::start(move |port| extract_stream(&port, job))),
	}));
	Listing {
		running,
		recorder,
		reporter,
	}
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_zip_is_listed_from_its_index_alone() {
	let big = incompressible(4 * CHUNK_SIZE, 0x77);
	let zip = zip_of(
		&[
			("big.bin", Some(&big)),
			("docs", None),
			("docs/a.txt", Some(b"a")),
		],
		None,
	);
	let setup = setup("bundle.zip", zip.clone(), |_| {});
	let listing = list(&setup, JobControl::default(), test_config());
	let listed = listing.running.await.unwrap().unwrap();

	assert_eq!(listed.format, Some(ArchiveFormat::Zip));
	assert_eq!(listed.password, PasswordCheck::NotNeeded);
	assert_eq!(
		listed
			.entries
			.iter()
			.map(|entry| (entry.id.index, entry.path.as_deref(), entry.size))
			.collect::<Vec<_>>(),
		[
			(0, Some("big.bin"), Some(big.len() as u64)),
			(1, Some("docs"), None),
			(2, Some("docs/a.txt"), Some(1)),
		]
	);
	assert_eq!(
		listed.totals,
		ListTotals {
			entries: 3,
			dirs: 1,
			files: 2,
			bytes: big.len() as u64 + 1,
			skipped: 0,
			bytes_skipped: 0,
		}
	);
	assert_eq!(*listing.recorder.entries.lock().unwrap(), listed.entries);
	// the head to tell the format, and the index in the last chunks: none of the big entry's
	// middle
	let fetched: HashSet<u64> = setup
		.backend
		.log()
		.fetched
		.iter()
		.map(|(_, index)| *index)
		.collect();
	assert!(
		!fetched.contains(&1) && !fetched.contains(&2),
		"{fetched:?}"
	);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!((last.phase, last.entries), (ListPhase::Done, 3));
	assert!(last.bytes_read < zip.len() as u64);
	assert_eq!(listing.reporter.ops_in_flight(), 0);
	assert_eq!(
		setup.backend.memory.available_permits(),
		setup.backend.budget
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tar_listing_reads_it_all_and_can_be_paused_and_cancelled() {
	let data = incompressible(3 * CHUNK_SIZE, 0x99);
	let tar = gzip(&tar_of(&[("a.bin", &data), ("b.txt", b"b")]));
	let config = test_config();

	// paused while it reads: it gives back its memory, and goes on once resumed
	let paused = setup("bundle.tar.gz", tar.clone(), |_| {});
	paused
		.backend
		.hold_requests(Request::Fetch, [paused.archive.uuid()]);
	let (pause, _cancel, control) = controls();
	let listing = list(&paused, control, config.clone());
	wait_until("a fetch is held", || !paused.backend.log().held.is_empty()).await;
	pause.send_replace(true);
	paused.backend.release_all();
	wait_until("the listing is paused", || listing.reporter.is_paused()).await;
	assert!(config.floor_is_free());
	assert_eq!(
		paused.backend.memory.available_permits(),
		paused.backend.budget
	);
	pause.send_replace(false);
	let listed = listing.running.await.unwrap().unwrap();
	assert_eq!(
		listed.format,
		Some(ArchiveFormat::Tar {
			codec: Some(StreamCodec::Gzip)
		})
	);
	assert_eq!(listed.totals.files, 2);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!(last.bytes_read, tar.len() as u64);

	// cancelled while it reads: what was listed so far comes back with the cancel
	let cancelled = setup("bundle.tar.gz", tar, |_| {});
	cancelled
		.backend
		.hold_requests(Request::Fetch, [cancelled.archive.uuid()]);
	let (_pause, cancel, control) = controls();
	let listing = list(&cancelled, control, config.clone());
	wait_until("a fetch is held", || {
		!cancelled.backend.log().held.is_empty()
	})
	.await;
	cancel.send_replace(true);
	let failed = listing.running.await.unwrap().unwrap_err();
	assert_eq!(failed.error.kind(), ErrorKind::Cancelled);
	let last = listing
		.recorder
		.updates
		.lock()
		.unwrap()
		.last()
		.unwrap()
		.clone();
	assert_eq!(
		(last.phase, last.eta),
		(ListPhase::Cancelled, Some(Duration::ZERO))
	);
	assert_eq!(listing.reporter.ops_in_flight(), 0);
	assert!(config.floor_is_free());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_keeps_the_first_entries_and_hands_over_them_all() {
	let names: Vec<String> = (0..MAX_LISTED_ENTRIES + 3)
		.map(|i| format!("f{i:05}"))
		.collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let many = setup("many.tar", tar_of(&members), |_| {});
	let mut config = test_config();
	config.max_members = 2 * MAX_LISTED_ENTRIES as u64;
	let listing = list(&many, JobControl::default(), config);
	let listed = listing.running.await.unwrap().unwrap();

	assert_eq!(listed.entries.len(), MAX_LISTED_ENTRIES);
	assert_eq!(listed.omitted_entries, 3);
	assert_eq!(listed.totals.files, names.len() as u64);
	assert_eq!(listing.recorder.entries.lock().unwrap().len(), names.len());
	let batches = listing.recorder.batches.lock().unwrap().clone();
	assert!(
		batches.iter().all(|&batch| batch <= CALLBACK_BATCH),
		"{batches:?}"
	);

	// long paths fill the listing's bytes before its count
	let dir = vec!["d".repeat(250); 16].join("/");
	let names: Vec<String> = (0..2200).map(|i| format!("{dir}/f{i:04}")).collect();
	let members: Vec<(&str, &[u8])> = names.iter().map(|name| (name.as_str(), &b""[..])).collect();
	let long = setup("long.tar", tar_of(&members), |_| {});
	// a GNU long-name record before each
	let mut config = test_config();
	config.max_members = 3 * names.len() as u64;
	let listing = list(&long, JobControl::default(), config);
	let listed = listing.running.await.unwrap().unwrap();
	// each keeps its stored path and its drive path, some 8 KB in all
	let kept = listed.entries.len();
	assert!(
		kept < names.len() && kept > MAX_LISTED_BYTES / (3 * names[0].len()),
		"{kept}"
	);
	assert_eq!(kept as u64 + listed.omitted_entries, names.len() as u64);
	assert_eq!(listing.recorder.entries.lock().unwrap().len(), names.len());
}

/// A bare tar of the file `a.bin` holding `data`, then `links` hard links to it.
fn tar_with_links(data: &[u8], links: usize) -> Vec<u8> {
	let mut tar = tar_of(&[("a.bin", data)]);
	tar.truncate(tar.len() - 1024);
	let mut builder = tar::Builder::new(tar);
	for link in 0..links {
		append_hard_link(&mut builder, &format!("l{link:03}"), "a.bin");
	}
	builder.into_inner().unwrap()
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
		// every link waits for the file first
		backend
			.slow_finish
			.insert("a.bin".to_owned(), Duration::from_millis(100));
	});
	let job = start(&setup, Options::default());
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
	let mut tar = tar_of(&[("a.txt", b"alpha")]);
	tar.truncate(tar.len() - 1024);
	let mut builder = tar::Builder::new(tar);
	append_hard_link(&mut builder, "c\u{202E}txt.exe", "a.txt");
	append_hard_link(&mut builder, "d:e", "a.txt");
	let setup = setup("bundle.tar", builder.into_inner().unwrap(), |backend| {
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
	let mut tar = tar_of(&[("a.txt", b"alpha")]);
	tar.truncate(tar.len() - 1024);
	let mut builder = tar::Builder::new(tar);
	append_hard_link(&mut builder, "b", "a.txt");
	// a link to a link copies what that one copies
	append_hard_link(&mut builder, "c", "b");
	// a link to itself names nothing before it
	append_hard_link(&mut builder, "self", "self");
	// a link to a link to nothing
	append_hard_link(&mut builder, "d", "self");
	let setup = setup("bundle.tar", builder.into_inner().unwrap(), |backend| {
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_that_waited_for_a_file_that_failed_is_no_item() {
	let setup = setup("bundle.tar", Vec::new(), |backend| {
		backend
			.slow_finish
			.insert("a.txt".to_owned(), Duration::from_millis(100));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_while_the_archive_is_read_ends_with_no_time_left() {
	// fetched slowly enough for the rate to be known before the cancel
	let tar = tar_of(&[("big.bin", &incompressible(6 * CHUNK_SIZE, 0x40))]);
	let setup = setup("slow.tar", tar, |backend| {
		backend
			.slow
			.insert("slow.tar".to_owned(), Duration::from_millis(100));
	});
	let (_pause, cancel, control) = controls();
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	wait_until("the rate is known", || {
		job.recorder
			.updates
			.lock()
			.unwrap()
			.last()
			.is_some_and(|update| update.eta.is_some_and(|eta| !eta.is_zero()))
	})
	.await;
	cancel.send_replace(true);
	job.running.await.unwrap().unwrap_err();
	let last = job.recorder.last();
	assert_eq!(
		(last.phase, last.eta),
		(ExtractPhase::Cancelled, Some(Duration::ZERO))
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
