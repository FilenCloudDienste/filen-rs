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

struct Options {
	root: ExtractRoot,
	control: JobControl,
	max_bytes: Option<u64>,
	max_items: Option<u64>,
	dispose: Option<(SourceDisposal, Uuid)>,
	password: Option<ArchivePassword>,
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
		dispose: options.dispose,
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
			max_members: 2000,
			expansion: Some(ExpansionLimit::DEFAULT),
			max_index_bytes: 32 << 20,
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
		DisposalOutcome::Kept { reason } => reason,
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
			("a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			("big.bin".to_owned(), (big.len() as u64, 3, hash(&big))),
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
			("a.txt".to_owned(), (5, 1, hash(b"alpha"))),
			("big.bin".to_owned(), (big.len() as u64, 3, hash(&big))),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_extract_finishes_once_resumed() {
	let big = pattern(3 * CHUNK_SIZE, 1);
	let tar = tar_of(&[("a.bin", &big[..]), ("b.txt", b"b"), ("c.txt", b"c")]);
	let setup = setup("bundle.tar", tar, |_| {});
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	let job = start(
		&setup,
		Options {
			control,
			..Options::default()
		},
	);
	tokio::time::sleep(Duration::from_millis(300)).await;
	assert!(
		finished(&setup).is_empty(),
		"nothing is registered while paused"
	);
	pause.send_replace(false);
	let report = job.running.await.unwrap().unwrap();
	assert_eq!(report.counts.files_done, 3);
	assert_eq!(finished(&setup).len(), 3);
	assert_released(&setup, &job.reporter);
}
