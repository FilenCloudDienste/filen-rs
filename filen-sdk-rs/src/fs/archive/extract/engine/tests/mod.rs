//! The driver against the fake drive. Most tests run the real codec on its thread, so they run in
//! real time on a multi-threaded runtime; the stall test scripts a silent codec on paused time.

mod bounds;
mod dispose;
mod failures;
mod links;
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
				ExtractSkipReason, ExtractStage, ExtractUpdate, PasswordCheck,
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
			test_support::{FakeBackend, Quirk, Request, wait_until},
		},
	},
	job::{
		report::RunState,
		test_support::{controls, run_states},
	},
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

/// The uuid of the directory the job created as `name`.
pub(super) fn log_dir(setup: &Setup, name: &str) -> Uuid {
	setup
		.backend
		.log()
		.created_dirs
		.iter()
		.find(|(_, created)| created == name)
		.map(|(uuid, _)| *uuid)
		.expect("the directory was created")
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
	// both go again into the directory as it was created, under the name it got
	use crate::fs::HasName;
	let docs = log_dir(&setup, "docs (2)");
	for failure in &report.failures {
		let retry = failure.retry.as_ref().expect("a failed entry goes again");
		let DirType::Dir(dir) = &retry.destination else {
			panic!("{:?}", retry.destination);
		};
		assert_eq!(
			(dir.uuid(), dir.name(), dir.parent),
			(docs, Some("docs (2)"), setup.destination.into())
		);
		assert_eq!(
			retry.base.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
			["docs"]
		);
	}
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

#[test]
#[cfg(target_pointer_width = "64")]
fn a_directory_slot_holds_no_directory() {
	// an archive may plan as many directories as it holds members: each slot keeps its created
	// directory's uuid, not the directory, which is built again only for a failure's retry
	assert!(
		size_of::<DirSlot>() <= 152,
		"{} bytes",
		size_of::<DirSlot>()
	);
}
