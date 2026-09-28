//! The driver against the fake drive. Most tests run the real codec on its thread, so they run in
//! real time on a multi-threaded runtime; the stall test scripts a silent codec on paused time.

mod bounds;
mod dispose;
mod failures;
mod passwords;
mod pause;

use std::{
	borrow::Cow,
	collections::{BTreeMap, HashMap, HashSet},
	io::Write,
	sync::{Mutex, atomic::Ordering},
	time::Duration,
};

use filen_types::crypto::Blake3Hash;
use tokio::{sync::watch, task::JoinHandle};

use super::*;
use crate::{
	consts::CHUNK_SIZE,
	fs::{
		archive::{
			config::CODEC_MEM_BUDGET,
			entry_path::entry_path,
			extract::{
				ArchiveEntryKind, ArchiveTotals, ExpansionLimit, ExtractCallback, ExtractEvent,
				ExtractSkipReason, ExtractStage, ExtractUpdate, PasswordCheck, RunState,
				codec::{Selection, extract_stream},
				test_support::{
					ARCHIVE_PARENT, MAX_MEMBERS, Options, Setup, archive_file_with, list, setup,
					setup_in, stream_job, test_config,
				},
			},
			worker,
		},
		archive::{
			dispose::{DisposalOutcome, KeptReason, SourceDisposal},
			extract::ExtractFailure,
			limits::display_path,
			password::ArchivePassword,
			sevenz::{
				crypto::MAX_CYCLES_POWER,
				write::{SevenZEncryption, SevenZMethod},
			},
			test_support::{
				TarMember, gzip, hash, incompressible, pattern, sevenz_of, tar_of, tar_with, zip_of,
			},
			worker::{ARCHIVE_STALL_TIMEOUT, LinkHead},
		},
		dir::RootDirectory,
		drive_job::{
			backend::ListedNames,
			counts::ItemCounts,
			test_support::{FakeBackend, Request, wait_until},
		},
	},
	job::test_support::{controls, run_states},
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
		let updates = self.updates.lock().unwrap();
		run_states(updates.iter().map(|update| update.run_state))
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
	setup.backend.assert_released(reporter);
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
	let tar = tar_with(&[
		TarMember::Data("docs/", b""),
		TarMember::Data("docs/a.txt", b"alpha"),
		TarMember::Data("docs/sub/big.bin", &big),
		TarMember::Data("empty.txt", b""),
		// the same name in another folder
		TarMember::Data("other/a.txt", b"another alpha"),
		// a symlink the extraction skips
		TarMember::Symlink {
			path: "link",
			target: "docs/a.txt",
		},
	]);
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
	let propagated: HashSet<Uuid> = log.propagated_trees.iter().copied().collect();
	let kept: Vec<Uuid> = report.top_level.iter().map(|top| top.item.uuid()).collect();
	assert!(
		kept.iter()
			.chain(&beyond)
			.all(|uuid| propagated.contains(uuid)),
		"every top-level item reaches the new share"
	);
}

/// Settings with one job slot, for jobs that compete for it.
fn one_slot() -> ArchiveConfig {
	let mut config = ArchiveConfig::new(CODEC_MEM_BUDGET, 1);
	config.max_members = MAX_MEMBERS;
	config
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

/// A directory entry at `path`, as the codec sends it.
fn dir_entry(ordinal: u64, path: &str) -> WorkerEvent {
	WorkerEvent::Entry(EntryHead {
		ordinal,
		path: entry_path(path).unwrap(),
		modified: None,
		kind: EntryKind::Dir,
	})
}

/// Appends a tar hard link at `path` to the member at `target`.
/// A hard link at `path` to the file at `target`.
fn hard_link<'a>(path: &'a str, target: &'a str) -> TarMember<'a> {
	TarMember::HardLink { path, target }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hard_link_is_extracted_as_a_copy_of_the_file_it_names() {
	let big = incompressible(2 * CHUNK_SIZE + 77, 0x51);
	let tar = tar_with(&[
		TarMember::Data("docs/", b""),
		TarMember::Data("docs/a.bin", &big),
		TarMember::Data("empty", b""),
		hard_link("docs/hard", "docs/a.bin"),
		hard_link("top.bin", "docs/a.bin"),
		hard_link("empty-link", "empty"),
		// nothing to copy: a path no file came at, and a directory's
		hard_link("gone", "missing.txt"),
		hard_link("dir-link", "docs"),
	]);
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
	let (shown, truncated) = display_path(path);
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

/// A bare tar of the file `a.bin` holding `data`, then `links` hard links to it.
fn tar_with_links(data: &[u8], links: usize) -> Vec<u8> {
	let paths: Vec<String> = (0..links).map(|link| format!("l{link:03}")).collect();
	let members: Vec<TarMember> = [TarMember::Data("a.bin", data)]
		.into_iter()
		.chain(paths.iter().map(|path| hard_link(path, "a.bin")))
		.collect();
	tar_with(&members)
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
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		hard_link("c\u{202E}txt.exe", "a.txt"),
		hard_link("d:e", "a.txt"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
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
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		hard_link("b", "a.txt"),
		// a link to a link copies what that one copies
		hard_link("c", "b"),
		// a link to itself names nothing before it
		hard_link("self", "self"),
		// a link to a link to nothing
		hard_link("d", "self"),
	]);
	let setup = setup("bundle.tar", tar, |backend| {
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

	// a listing tells the same of each link
	let listing = list(&setup, JobControl::default(), test_config());
	let listed = listing.running.await.unwrap().unwrap();
	assert_eq!(
		listed
			.entries
			.iter()
			.map(|entry| (entry.stored_path.as_str(), entry.size, entry.skip.is_some()))
			.collect::<Vec<_>>(),
		[
			("a.txt", Some(5), false),
			("b", Some(5), false),
			("c", Some(5), false),
			("self", Some(0), true),
			("d", Some(0), true),
		]
	);
	assert_eq!(listed.totals.files, report.counts.files_done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chosen_hard_link_comes_out_with_the_target_its_listing_names() {
	let tar = tar_with(&[
		TarMember::Data("a.txt", b"alpha"),
		TarMember::Data("other.txt", b"other"),
		hard_link("b", "a.txt"),
		hard_link("c", "b"),
	]);
	let listing = list(
		&setup("bundle.tar", tar.clone(), |_| {}),
		JobControl::default(),
		test_config(),
	);
	let listed = listing.running.await.unwrap().unwrap();
	let target_of = |index: usize| match &listed.entries[index].kind {
		ArchiveEntryKind::Hardlink { target_id, .. } => target_id.map(|id| id.index),
		other => panic!("{other:?} is no hard link"),
	};
	// each names the entry it copies, a link naming a link
	assert_eq!((target_of(2), target_of(3)), (Some(0), Some(2)));

	// alone, a link has nothing to copy; with the entries its listing names, it is extracted
	for (chosen_ids, extracted, skipped) in [
		(&[3][..], &[][..], &["c"][..]),
		(&[3, 2, 0], &["a.txt", "b", "c"], &[]),
	] {
		let setup = setup("bundle.tar", tar.clone(), |_| {});
		let job = start(&setup, chosen(chosen_ids, &[]));
		let report = job.running.await.unwrap().unwrap();
		assert_eq!(finished_paths(&setup), extracted, "{chosen_ids:?}");
		assert_eq!(
			report
				.skipped
				.iter()
				.map(|skipped| skipped.path.as_str())
				.collect::<Vec<_>>(),
			skipped,
			"{chosen_ids:?}"
		);
	}
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
