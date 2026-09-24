use std::{
	collections::{HashMap, HashSet},
	sync::{
		Mutex,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use tokio::sync::{Notify, watch};

use super::*;
use crate::{
	consts::{CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA_USIZE},
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		copy::{
			plan::{CopyPlanner, Listed, PlanRequest, PlanSource, SourceDir},
			report::{CopyCallback, CopyUpdate, RunState},
		},
		dir::meta::DecryptedDirectoryMeta,
		file::meta::{DecryptedFileMeta, FileMeta},
	},
};
use filen_types::fs::StableUuid;

fn budget(chunks: usize) -> usize {
	chunks * (CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE)
}

/// The plaintext of chunk `index` of the source file `uuid`.
fn chunk_data(uuid: Uuid, index: u64, size: u64) -> Vec<u8> {
	let fill = (uuid.as_u128() as u8) ^ (index as u8);
	vec![fill; chunk_plaintext_len(size, index) as usize]
}

fn file_hash(uuid: Uuid, size: u64) -> Blake3Hash {
	let mut hasher = blake3::Hasher::new();
	for index in 0..size.div_ceil(CHUNK_SIZE_U64) {
		hasher.update(&chunk_data(uuid, index, size));
	}
	Blake3Hash::from(hasher.finalize())
}

fn source_file(name: &str, size: u64) -> RemoteFileType<'static> {
	source_file_with_chunks(name, size, size.div_ceil(CHUNK_SIZE_U64))
}

/// A source file whose stored chunk count is `chunks`.
fn source_file_with_chunks(name: &str, size: u64, chunks: u64) -> RemoteFileType<'static> {
	let uuid = Uuid::new_v4();
	let meta = FileMeta::Decoded(DecryptedFileMeta {
		name: Cow::Owned(name.to_owned()),
		size,
		mime: Cow::Borrowed("text/plain"),
		key: FileKey::V3(EncryptionKey::generate()),
		last_modified: Utc::now(),
		created: None,
		hash: Some(file_hash(uuid, size)),
	});
	let file: crate::fs::file::AnonymousRemoteFile = RemoteFile::from_meta(
		uuid,
		(),
		Uuid::new_v4().into(),
		size,
		chunks,
		"de-1",
		"bucket",
		Utc::now(),
		false,
		meta,
	);
	RemoteFileType::File(Cow::Owned(file))
}

fn source_dir(name: &str) -> SourceDir {
	SourceDir {
		uuid: Uuid::new_v4(),
		name: Some(name.to_owned()),
		created: Some(Utc::now()),
		color: DirColor::Blue,
		handle: (),
	}
}

struct FakeLock(Arc<AtomicUsize>);
impl Drop for FakeLock {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

#[derive(Default)]
struct FakeLog {
	fetched: Vec<(Uuid, u64)>,
	uploaded: Vec<(Uuid, u64)>,
	finished: HashMap<Uuid, (String, UploadCompletion)>,
	created_dirs: Vec<(Uuid, String)>,
	out_of_order_dirs: Vec<String>,
	colored: Vec<Uuid>,
	propagated: Vec<Uuid>,
	propagated_trees: Vec<Uuid>,
	target_fetches: usize,
	probes: Vec<String>,
	/// Drive-lock acquisitions that had to wait for another holder.
	lock_waits: usize,
	/// Slow registrations that have started.
	finishing: Vec<String>,
}

struct FakeUpload {
	spec: UploadSpec,
}

struct FakeBackend {
	memory: Arc<Semaphore>,
	budget: usize,
	live_locks: Arc<AtomicUsize>,
	log: Mutex<FakeLog>,
	known_dirs: Mutex<HashSet<Uuid>>,
	delay: Duration,
	slow: HashMap<String, Duration>,
	fail_fetch: HashMap<String, ErrorKind>,
	fail_upload: HashMap<String, ErrorKind>,
	blocked_uploads: HashSet<String>,
	fail_create: HashMap<String, ErrorKind>,
	/// Colors that cannot be set.
	fail_color: bool,
	/// Every propagation to a share or link fails.
	fail_propagate: bool,
	fail_finish: HashMap<String, ErrorKind>,
	/// Registrations that take this long, logged in `finishing` when they start.
	slow_finish: HashMap<String, Duration>,
	/// Files whose last chunk comes back one byte short.
	short_reads: HashSet<String>,
	/// Fetching the destination's shares and links fails.
	fail_targets: Option<ErrorKind>,
	targets_delay: Duration,
	/// Drive-lock acquisitions from this call index on fail with this kind.
	fail_locks_from: Option<(usize, ErrorKind)>,
	merge_once: Mutex<HashSet<String>>,
	targets: ConnectedTargets,
	later_targets: Option<ConnectedTargets>,
	/// Later chunks of a file download faster than earlier ones.
	reverse_chunks: bool,
	/// Lowercased names the destination holds without the listing having shown them.
	existing: Mutex<HashSet<String>>,
	/// Names the server registers as a new version of the given existing file (a client
	/// writing without the drive lock took them at the last moment).
	version_of: HashMap<String, Uuid>,
	never: Notify,
	lock_calls: AtomicUsize,
	/// Drive-lock acquisitions from this call index on wait (another client holds the lock)
	/// until it is raised again; `usize::MAX` when the lock is free.
	block_locks_from: AtomicUsize,
}

impl FakeBackend {
	fn new(memory_chunks: usize, destinations: &[Uuid]) -> Self {
		Self {
			memory: Arc::new(Semaphore::new(budget(memory_chunks))),
			budget: budget(memory_chunks),
			live_locks: Arc::new(AtomicUsize::new(0)),
			log: Mutex::new(FakeLog::default()),
			known_dirs: Mutex::new(destinations.iter().copied().collect()),
			delay: Duration::from_millis(10),
			slow: HashMap::new(),
			fail_fetch: HashMap::new(),
			fail_upload: HashMap::new(),
			blocked_uploads: HashSet::new(),
			fail_create: HashMap::new(),
			fail_color: false,
			fail_propagate: false,
			fail_finish: HashMap::new(),
			slow_finish: HashMap::new(),
			short_reads: HashSet::new(),
			fail_targets: None,
			targets_delay: Duration::ZERO,
			fail_locks_from: None,
			merge_once: Mutex::new(HashSet::new()),
			targets: ConnectedTargets::default(),
			later_targets: None,
			reverse_chunks: false,
			existing: Mutex::new(HashSet::new()),
			version_of: HashMap::new(),
			never: Notify::new(),
			lock_calls: AtomicUsize::new(0),
			block_locks_from: AtomicUsize::new(usize::MAX),
		}
	}

	fn log(&self) -> std::sync::MutexGuard<'_, FakeLog> {
		self.log.lock().unwrap()
	}

	async fn wait(&self, name: &str) {
		tokio::time::sleep(*self.slow.get(name).unwrap_or(&self.delay)).await;
	}
}

impl CopyBackend for FakeBackend {
	type DriveLock = FakeLock;
	type Upload = FakeUpload;

	fn memory(&self) -> Arc<Semaphore> {
		Arc::clone(&self.memory)
	}

	async fn acquire_drive_lock(&self) -> Result<FakeLock, Error> {
		let call = self.lock_calls.fetch_add(1, Ordering::SeqCst);
		if let Some((from, kind)) = self.fail_locks_from
			&& call >= from
		{
			return Err(Error::custom(kind, "lock failed"));
		}
		if call >= self.block_locks_from.load(Ordering::SeqCst) {
			self.log().lock_waits += 1;
			while call >= self.block_locks_from.load(Ordering::SeqCst) {
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
		}
		self.live_locks.fetch_add(1, Ordering::SeqCst);
		Ok(FakeLock(Arc::clone(&self.live_locks)))
	}

	async fn connected_targets(&self, _dir: Uuid) -> Result<ConnectedTargets, Error> {
		let fetches = {
			let mut log = self.log();
			log.target_fetches += 1;
			log.target_fetches
		};
		tokio::time::sleep(self.targets_delay).await;
		if let Some(kind) = self.fail_targets {
			return Err(Error::custom(kind, "targets failed"));
		}
		Ok(match (&self.later_targets, fetches) {
			(Some(later), 2..) => later.clone(),
			_ => self.targets.clone(),
		})
	}

	async fn create_copy_dir(
		&self,
		parent: Uuid,
		uuid: Uuid,
		name: &ValidatedName,
		created: DateTime<Utc>,
	) -> Result<CreatedDir, Error> {
		assert!(
			self.live_locks.load(Ordering::SeqCst) > 0,
			"creates hold the drive lock"
		);
		self.wait(name.as_ref()).await;
		if let Some(kind) = self.fail_create.get(name.as_ref()) {
			return Err(Error::custom(*kind, "create failed"));
		}
		if self.merge_once.lock().unwrap().remove(name.as_ref())
			|| self
				.existing
				.lock()
				.unwrap()
				.contains(&name.as_ref().to_lowercase())
		{
			return Ok(CreatedDir::Merged);
		}
		if !self.known_dirs.lock().unwrap().contains(&parent) {
			self.log().out_of_order_dirs.push(name.as_ref().to_owned());
		}
		self.known_dirs.lock().unwrap().insert(uuid);
		self.log()
			.created_dirs
			.push((uuid, name.as_ref().to_owned()));
		Ok(CreatedDir::Created(RemoteDirectory::new_from_parts(
			uuid,
			DecryptedDirectoryMeta {
				name: Cow::Owned(name.as_ref().to_owned()),
				created: Some(created),
			},
			parent.into(),
			Utc::now(),
		)))
	}

	async fn set_dir_color(
		&self,
		dir: &mut RemoteDirectory,
		_color: DirColor<'static>,
	) -> Result<(), Error> {
		if self.fail_color {
			return Err(Error::custom(ErrorKind::Server, "color failed"));
		}
		self.log().colored.push(dir.uuid());
		Ok(())
	}

	async fn propagate(
		&self,
		_targets: &ConnectedTargets,
		item: NonRootItemType<'_, Normal>,
	) -> Vec<Error> {
		if self.fail_propagate {
			return vec![Error::custom(ErrorKind::Server, "propagation failed")];
		}
		self.log().propagated.push(item.uuid());
		Vec::new()
	}

	async fn propagate_tree(
		&self,
		_targets: &ConnectedTargets,
		item: &NonRootItemType<'static, Normal>,
	) -> Vec<Error> {
		self.log().propagated_trees.push(item.uuid());
		Vec::new()
	}

	fn begin_upload(&self, spec: UploadSpec) -> FakeUpload {
		FakeUpload { spec }
	}

	async fn fetch_chunk(
		&self,
		file: &RemoteFileType<'static>,
		index: u64,
	) -> Result<Vec<u8>, Error> {
		let name = file.name().unwrap_or_default().to_owned();
		if self.reverse_chunks {
			tokio::time::sleep(Duration::from_millis(10 * (file.chunks() - index))).await;
		} else {
			self.wait(&name).await;
		}
		if let Some(kind) = self.fail_fetch.get(&name) {
			return Err(Error::custom(*kind, "fetch failed"));
		}
		self.log().fetched.push((file.uuid(), index));
		let mut data = chunk_data(file.uuid(), index, file.size());
		if self.short_reads.contains(&name) && index + 1 == file.size().div_ceil(CHUNK_SIZE_U64) {
			data.pop();
		}
		Ok(data)
	}

	async fn upload_chunk(
		&self,
		upload: &FakeUpload,
		index: u64,
		data: Vec<u8>,
	) -> Result<RemoteFileInfo, Error> {
		let name = upload.spec.name.as_ref();
		if self.blocked_uploads.contains(name) {
			self.never.notified().await;
		}
		self.wait(name).await;
		if let Some(kind) = self.fail_upload.get(name) {
			return Err(Error::custom(*kind, "upload failed"));
		}
		assert!(data.len() <= CHUNK_SIZE);
		self.log().uploaded.push((upload.spec.uuid, index));
		Ok(RemoteFileInfo::default())
	}

	async fn name_exists(&self, _parent: Uuid, name: &ValidatedName) -> Result<bool, Error> {
		self.log().probes.push(name.as_ref().to_owned());
		Ok(self
			.existing
			.lock()
			.unwrap()
			.contains(&name.as_ref().to_lowercase()))
	}

	async fn finish_upload(
		&self,
		upload: &FakeUpload,
		name: &ValidatedName,
		completion: UploadCompletion,
		_info: RemoteFileInfo,
	) -> Result<RemoteFile, Error> {
		assert!(
			self.live_locks.load(Ordering::SeqCst) > 0,
			"finalizing holds the drive lock"
		);
		if let Some(delay) = self.slow_finish.get(name.as_ref()) {
			self.log().finishing.push(name.as_ref().to_owned());
			tokio::time::sleep(*delay).await;
		}
		if let Some(kind) = self.fail_finish.get(name.as_ref()) {
			return Err(Error::custom(*kind, "registration failed"));
		}
		assert!(
			!self
				.existing
				.lock()
				.unwrap()
				.contains(&name.as_ref().to_lowercase()),
			"a copy must never be registered under a name the destination holds"
		);
		self.log()
			.finished
			.insert(upload.spec.uuid, (name.as_ref().to_owned(), completion));
		let stable_uuid = self
			.version_of
			.get(name.as_ref())
			.copied()
			.unwrap_or(upload.spec.uuid);
		Ok(RemoteFile::from_meta(
			upload.spec.uuid,
			StableUuid::new_for_test(stable_uuid),
			upload.spec.parent.into(),
			completion.written,
			completion.num_chunks,
			"de-1",
			"bucket",
			Utc::now(),
			false,
			FileMeta::Decoded(DecryptedFileMeta {
				name: Cow::Owned(name.as_ref().to_owned()),
				size: completion.written,
				mime: Cow::Borrowed("text/plain"),
				key: FileKey::V3(EncryptionKey::generate()),
				last_modified: completion.final_times.1,
				created: Some(completion.final_times.0),
				hash: Some(completion.hash),
			}),
		))
	}
}

#[derive(Default)]
struct Recorder {
	planned: Mutex<Vec<PlannedTopLevelItem>>,
	created: Mutex<Vec<CopiedTopLevel>>,
	updates: Mutex<Vec<CopyUpdate>>,
}

impl CopyCallback for Recorder {
	fn top_level_planned(&self, items: Vec<PlannedTopLevelItem>) {
		self.planned.lock().unwrap().extend(items);
	}

	fn top_level_created(&self, item: CopiedTopLevel) {
		self.created.lock().unwrap().push(item);
	}

	fn update(&self, update: CopyUpdate) {
		self.updates.lock().unwrap().push(update);
	}
}

impl Recorder {
	fn events(&self) -> Vec<CopyEvent> {
		self.updates
			.lock()
			.unwrap()
			.iter()
			.flat_map(|u| u.events.clone())
			.collect()
	}

	fn last(&self) -> CopyUpdate {
		self.updates.lock().unwrap().last().unwrap().clone()
	}
}

fn plan(destination: Uuid, sources: Vec<PlanSource>) -> CopyPlan {
	plan_with(destination, sources, false)
}

/// `unverified`: the destination listing had entries whose names it could not show.
fn plan_with(destination: Uuid, sources: Vec<PlanSource>, unverified: bool) -> CopyPlan {
	let mut planner = CopyPlanner::default();
	planner.add_destination(destination, std::iter::empty());
	if unverified {
		planner.mark_unverified(destination);
	}
	planner
		.plan(
			sources
				.into_iter()
				.map(|source| PlanRequest {
					source,
					destination,
					name: None,
				})
				.collect(),
		)
		.unwrap()
}

fn tree(
	root: &SourceDir,
	dirs: Vec<Listed<SourceDir>>,
	files: Vec<Listed<RemoteFileType<'static>>>,
) -> PlanSource {
	PlanSource::Dir {
		root: root.clone(),
		dirs,
		files,
	}
}

fn listed<T>(parent: &SourceDir, item: T) -> Listed<T> {
	Listed {
		parent: parent.uuid,
		item,
	}
}

fn controls() -> (watch::Sender<bool>, watch::Sender<bool>, JobControl) {
	let (pause, pause_rx) = watch::channel(false);
	let (cancel, cancel_rx) = watch::channel(false);
	(
		pause,
		cancel,
		JobControl::from_receivers(Some(pause_rx), Some(cancel_rx)),
	)
}

type Running = tokio::task::JoinHandle<CopyOutcome<()>>;

fn start(
	backend: &Arc<FakeBackend>,
	plan: CopyPlan,
	control: JobControl,
) -> (Running, Arc<Recorder>, MaybeArc<Reporter>) {
	let recorder = Arc::new(Recorder::default());
	let reporter = Reporter::new(Arc::clone(&recorder));
	let destination_dirs = plan
		.dirs
		.iter()
		.map(|dir| dir.parent)
		.chain(plan.files.iter().map(|file| file.parent))
		.filter_map(|parent| match parent {
			DestParent::Existing(uuid) => Some((
				uuid,
				DirType::Root(Cow::Owned(crate::fs::dir::RootDirectory::new(uuid))),
			)),
			DestParent::Planned(_) => None,
		})
		.collect();
	let running = tokio::spawn(run_copy(
		Arc::clone(backend),
		plan,
		destination_dirs,
		control,
		MaybeArc::clone(&reporter),
	));
	(running, recorder, reporter)
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
	for _ in 0..100_000 {
		if condition() {
			return;
		}
		tokio::time::sleep(Duration::from_millis(1)).await;
	}
	panic!("timed out waiting until {what}");
}

/// Everything a finished job must have given back.
fn assert_released(backend: &FakeBackend, reporter: &Reporter) {
	assert_eq!(
		backend.memory.available_permits(),
		backend.budget,
		"every memory reservation is released"
	);
	assert_eq!(
		backend.live_locks.load(Ordering::SeqCst),
		0,
		"no drive lock is held"
	);
	assert_eq!(reporter.ops_in_flight(), 0, "nothing is in flight");
}

fn assert_each_chunk_once(backend: &FakeBackend) {
	let log = backend.log();
	let fetched: HashSet<_> = log.fetched.iter().copied().collect();
	assert_eq!(
		fetched.len(),
		log.fetched.len(),
		"no chunk is fetched twice"
	);
	let uploaded: HashSet<_> = log.uploaded.iter().copied().collect();
	assert_eq!(
		uploaded.len(),
		log.uploaded.len(),
		"no chunk is uploaded twice"
	);
}

#[tokio::test(start_paused = true)]
async fn copies_a_tree_parent_first_with_every_chunk_once() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let sub = source_dir("Sub");
	let files = [
		("a.bin", 2 * CHUNK_SIZE_U64 + 12_345),
		("empty", 0),
		("exact", CHUNK_SIZE_U64),
		("t.txt", 10),
	];
	let sources: Vec<_> = files
		.iter()
		.map(|(name, size)| source_file(name, *size))
		.collect();
	let plan = plan(
		destination,
		vec![
			tree(
				&top,
				vec![listed(&top, sub.clone())],
				vec![
					listed(&top, sources[0].clone()),
					listed(&sub, sources[1].clone()),
					listed(&sub, sources[2].clone()),
				],
			),
			PlanSource::File(sources[3].clone()),
		],
	);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, recorder, reporter) = start(&backend, plan, JobControl::default());
	let outcome = running.await.unwrap();

	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	assert_each_chunk_once(&backend);
	{
		let log = backend.log();
		assert!(
			log.out_of_order_dirs.is_empty(),
			"directories are created parent first"
		);
		assert_eq!(log.created_dirs.len(), 2);
		assert_eq!(log.colored.len(), 2, "colors are kept");
		assert!(
			log.propagated.is_empty(),
			"an unconnected destination gets no propagation"
		);
		assert_eq!(log.finished.len(), 4);
		for (source, (_, size)) in sources.iter().zip(files) {
			let (_, completion) = log
				.finished
				.values()
				.find(|(name, _)| name == source.name().unwrap())
				.unwrap();
			assert_eq!(completion.written, size);
			assert_eq!(completion.num_chunks, size.div_ceil(CHUNK_SIZE_U64));
			assert_eq!(completion.hash, file_hash(source.uuid(), size));
		}
		assert!(
			!log.fetched
				.iter()
				.any(|(uuid, _)| *uuid == sources[1].uuid()),
			"a zero-byte file transfers no chunk"
		);
	}

	let counts = outcome.report.counts;
	assert_eq!(counts.dirs_created, 2);
	assert_eq!(counts.files_done, 4);
	assert_eq!(counts.bytes_done, outcome.report.totals.bytes);
	assert_eq!(counts.files_failed + counts.dirs_failed, 0);
	assert_eq!(outcome.report.top_level.len(), 2);

	let planned = recorder.planned.lock().unwrap().clone();
	let created = recorder.created.lock().unwrap().clone();
	assert_eq!(planned.len(), 2);
	assert_eq!(created.len(), 2);
	for item in &created {
		assert!(
			planned.iter().any(|p| p.dest_uuid == item.item.uuid()),
			"created top-level items were announced up front"
		);
	}

	let events = recorder.events();
	for source in &sources {
		let started = events
			.iter()
			.position(|e| matches!(e, CopyEvent::FileStarted(f) if f.source_uuid == source.uuid()));
		let done = events.iter().position(
			|e| matches!(e, CopyEvent::FileDone { source_uuid, .. } if *source_uuid == source.uuid()),
		);
		assert!(
			started.unwrap() < done.unwrap(),
			"a file starts before it is done"
		);
	}
	let last = recorder.last();
	assert_eq!(last.phase, CopyPhase::Done);
	assert!(last.active.is_empty());
	assert_eq!(last.counts, counts);
}

#[tokio::test(start_paused = true)]
async fn stored_chunks_without_data_are_not_copied() {
	let destination = Uuid::new_v4();
	// chunk counts some clients store: one for an empty file, a trailing empty chunk
	let empty = source_file_with_chunks("empty", 0, 1);
	let full = source_file_with_chunks("full", CHUNK_SIZE_U64, 2);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let plan = plan(
		destination,
		vec![
			PlanSource::File(empty.clone()),
			PlanSource::File(full.clone()),
		],
	);
	let (running, _recorder, reporter) = start(&backend, plan, JobControl::default());
	running.await.unwrap().result.unwrap();

	assert_released(&backend, &reporter);
	let log = backend.log();
	assert_eq!(
		log.fetched,
		vec![(full.uuid(), 0)],
		"only chunks holding data are read"
	);
	assert_eq!(
		log.uploaded.len(),
		1,
		"only chunks holding data are written"
	);
	for (source, chunks) in [(&empty, 0), (&full, 1)] {
		let (_, completion) = log
			.finished
			.values()
			.find(|(name, _)| name == source.name().unwrap())
			.unwrap();
		assert_eq!(completion.num_chunks, chunks);
		assert_eq!(completion.written, source.size());
	}
}

async fn copy_many(memory_chunks: usize, files: usize, chunks_per_file: u64) {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..files)
		.map(|i| {
			source_file(
				&format!("f{i}"),
				chunks_per_file * CHUNK_SIZE_U64 - i as u64,
			)
		})
		.collect();
	let plan = plan(
		destination,
		sources.iter().cloned().map(PlanSource::File).collect(),
	);
	let mut backend = FakeBackend::new(memory_chunks, &[destination]);
	for (i, source) in sources.iter().enumerate() {
		backend.slow.insert(
			source.name().unwrap().to_owned(),
			Duration::from_millis(1 + (i as u64 * 7) % 13),
		);
	}
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(&backend, plan, JobControl::default());
	let outcome = tokio::time::timeout(Duration::from_secs(30), running)
		.await
		.expect("the copy must not deadlock")
		.unwrap();
	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	assert_each_chunk_once(&backend);
	let log = backend.log();
	assert_eq!(log.finished.len(), files);
	for source in &sources {
		let (_, completion) = log
			.finished
			.values()
			.find(|(name, _)| name == source.name().unwrap())
			.unwrap();
		assert_eq!(completion.hash, file_hash(source.uuid(), source.size()));
	}
}

#[tokio::test(start_paused = true)]
async fn never_deadlocks_on_the_smallest_memory_budget() {
	copy_many(1, 8, 5).await;
}

#[tokio::test(start_paused = true)]
async fn never_deadlocks_with_many_files_on_a_small_budget() {
	copy_many(3, 20, 3).await;
}

/// Tasks run in parallel here, so reservations are taken and released concurrently.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_deadlocks_on_a_multi_threaded_runtime() {
	copy_many(2, 12, 4).await;
}

#[tokio::test(start_paused = true)]
async fn hashes_chunks_in_order_when_they_complete_out_of_order() {
	let destination = Uuid::new_v4();
	let source = source_file("f", 6 * CHUNK_SIZE_U64);
	let mut backend = FakeBackend::new(8, &[destination]);
	backend.reverse_chunks = true;
	let backend = Arc::new(backend);
	let plan = plan(destination, vec![PlanSource::File(source.clone())]);
	let (running, _recorder, _reporter) = start(&backend, plan, JobControl::default());
	running.await.unwrap().result.unwrap();
	let log = backend.log();
	let order: Vec<u64> = log.fetched.iter().map(|(_, index)| *index).collect();
	assert_ne!(
		order,
		(0..6).collect::<Vec<_>>(),
		"chunks completed out of order"
	);
	let (_, completion) = log.finished.values().next().unwrap();
	assert_eq!(completion.hash, file_hash(source.uuid(), source.size()));
}

#[tokio::test(start_paused = true)]
async fn pause_during_file_copies_releases_everything_and_resumes() {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..3)
		.map(|i| source_file(&format!("f{i}"), 6 * CHUNK_SIZE_U64))
		.collect();
	let plan = plan(
		destination,
		sources.iter().cloned().map(PlanSource::File).collect(),
	);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, _cancel, control) = controls();
	let (running, recorder, reporter) = start(&backend, plan, control);

	wait_until("some chunks are uploaded", || {
		backend.log().uploaded.len() >= 3
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_released(&backend, &reporter);
	let uploaded = backend.log().uploaded.len();
	tokio::time::sleep(Duration::from_secs(60)).await;
	assert_eq!(
		backend.log().uploaded.len(),
		uploaded,
		"nothing moves while paused"
	);
	assert!(
		recorder
			.updates
			.lock()
			.unwrap()
			.iter()
			.any(|u| u.run_state == RunState::Paused)
	);
	assert!(
		recorder
			.updates
			.lock()
			.unwrap()
			.iter()
			.any(|u| u.run_state == RunState::Pausing)
	);

	pause.send_replace(false);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	assert_each_chunk_once(&backend);
	assert_eq!(backend.log().uploaded.len(), 18);
	assert_eq!(outcome.report.counts.bytes_done, 18 * CHUNK_SIZE_U64);
	assert_eq!(recorder.last().run_state, RunState::Running);
}

// The memory budget is shared with other transfers. A chunk waiting for memory is handed
// what is free while it waits for the rest, so a paused job must stop waiting.
#[tokio::test(start_paused = true)]
async fn a_paused_job_holds_no_memory_while_a_chunk_waited_for_it() {
	let destination = Uuid::new_v4();
	let backend = Arc::new(FakeBackend::new(1, &[destination]));
	let free = 1024;
	let elsewhere = Arc::clone(&backend.memory)
		.try_acquire_many_owned(u32::try_from(backend.budget - free).unwrap())
		.unwrap();
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(source_file("f", CHUNK_SIZE_U64))],
		),
		control,
	);

	tokio::time::sleep(Duration::from_secs(1)).await;
	assert!(
		backend.log().fetched.is_empty(),
		"the chunk waits for memory"
	);
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_eq!(
		backend.memory.available_permits(),
		free,
		"a paused job holds no memory"
	);

	drop(elsewhere);
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_released(&backend, &reporter);
	assert_eq!(backend.log().finished.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn starting_an_operation_ends_a_reported_pause() {
	let reporter = Reporter::new(Arc::new(Recorder::default()));
	reporter.set_pause_requested(true);
	assert!(reporter.is_paused());
	let op = reporter.op();
	assert!(
		!reporter.is_paused(),
		"a job with an operation in flight is not paused"
	);
	drop(op);
	assert!(reporter.is_paused());
}

// Every time the job reports itself paused, it must hold nothing, on any thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_paused_job_holds_nothing_on_a_multi_threaded_runtime() {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..24)
		.map(|i| source_file(&format!("f{i}"), 3 * CHUNK_SIZE_U64 / 2))
		.collect();
	let backend = Arc::new(FakeBackend::new(3, &[destination]));
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(
		&backend,
		plan(
			destination,
			sources.iter().cloned().map(PlanSource::File).collect(),
		),
		control,
	);

	for _ in 0..10 {
		tokio::time::sleep(Duration::from_millis(15)).await;
		pause.send_replace(true);
		wait_until("the job is paused", || {
			reporter.is_paused() || running.is_finished()
		})
		.await;
		if reporter.is_paused() {
			assert_released(&backend, &reporter);
		}
		pause.send_replace(false);
	}
	running.await.unwrap().result.unwrap();
	assert_released(&backend, &reporter);
	assert_eq!(backend.log().finished.len(), 24);
	assert_each_chunk_once(&backend);
}

fn wide_tree(dirs: usize) -> (SourceDir, PlanSource) {
	let root = source_dir("Root");
	let children: Vec<_> = (0..dirs).map(|i| source_dir(&format!("d{i}"))).collect();
	let files = children
		.iter()
		.enumerate()
		.map(|(i, dir)| listed(dir, source_file(&format!("file{i}"), 100)))
		.collect();
	let source = tree(
		&root,
		children
			.iter()
			.map(|child| listed(&root, child.clone()))
			.collect(),
		files,
	);
	(root, source)
}

#[tokio::test(start_paused = true)]
async fn pause_during_directory_creation_holds_no_lock_and_resumes() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(200);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("the root is created", || {
		!backend.log().created_dirs.is_empty()
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_released(&backend, &reporter);
	let created = backend.log().created_dirs.len();
	assert!(created < 201, "paused before every directory existed");
	tokio::time::sleep(Duration::from_secs(60)).await;
	assert_eq!(
		backend.log().created_dirs.len(),
		created,
		"no create starts while paused"
	);

	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().created_dirs.len(), 201);
	assert_eq!(backend.log().finished.len(), 200);
	assert!(backend.log().out_of_order_dirs.is_empty());
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn a_job_paused_before_it_starts_waits() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	let (running, _recorder, reporter) = start(&backend, plan(destination, vec![source]), control);
	tokio::time::sleep(Duration::from_secs(60)).await;
	assert_eq!(backend.log().target_fetches, 0);
	assert!(backend.log().created_dirs.is_empty());
	assert!(reporter.is_paused());
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().finished.len(), 3);
}

// Real time on several threads, so a driver spinning on the stale pause fails the timeout
// instead of starving the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pause_controller_dropped_while_paused_lets_the_job_finish() {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..3)
		.map(|i| source_file(&format!("f{i}"), 6 * CHUNK_SIZE_U64))
		.collect();
	let plan = plan(
		destination,
		sources.iter().cloned().map(PlanSource::File).collect(),
	);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, _cancel, control) = controls();
	let (running, recorder, reporter) = start(&backend, plan, control);

	wait_until("some chunks are uploaded", || {
		backend.log().uploaded.len() >= 3
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	drop(pause);

	let outcome = tokio::time::timeout(Duration::from_secs(30), running)
		.await
		.expect("a lost pause controller does not keep the job paused")
		.unwrap();
	outcome.result.unwrap();
	assert_eq!(backend.log().uploaded.len(), 18);
	assert_released(&backend, &reporter);
	assert_eq!(recorder.last().run_state, RunState::Running);
}

#[tokio::test(start_paused = true)]
async fn a_cancel_while_paused_ends_the_pause() {
	let destination = Uuid::new_v4();
	let big = source_file("big", 6 * CHUNK_SIZE_U64);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(big)]),
		control,
	);

	wait_until("a chunk is uploaded", || !backend.log().uploaded.is_empty()).await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	let updates_before_cancel = recorder.updates.lock().unwrap().len();
	cancel.send_replace(true);
	let outcome = running.await.unwrap();

	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_released(&backend, &reporter);
	let updates = recorder.updates.lock().unwrap();
	let states: Vec<RunState> = updates[updates_before_cancel..]
		.iter()
		.map(|u| u.run_state)
		.collect();
	let cancelling = states
		.iter()
		.position(|s| *s == RunState::Cancelling)
		.expect("the cancel is reported");
	assert!(
		states[cancelling..]
			.iter()
			.all(|s| *s == RunState::Cancelling),
		"a cancelling job is shown as pausing or paused: {states:?}"
	);
	let last = updates.last().unwrap();
	assert_eq!(last.phase, CopyPhase::Cancelled);
	assert!(
		!matches!(last.run_state, RunState::Paused | RunState::Pausing),
		"the final update is not paused"
	);
}

#[tokio::test(start_paused = true)]
async fn cancel_during_directory_creation_reports_what_was_created() {
	let destination = Uuid::new_v4();
	let (first_root, first) = wide_tree(200);
	let (_, second) = wide_tree(1);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) =
		start(&backend, plan(destination, vec![first, second]), control);

	wait_until("a top-level directory exists", || {
		!recorder.created.lock().unwrap().is_empty()
	})
	.await;
	cancel.send_replace(true);
	let outcome = running.await.unwrap();

	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_released(&backend, &reporter);
	assert!(
		backend.log().finished.is_empty(),
		"no file is copied after a cancel"
	);
	let created_uuids: HashSet<_> = backend
		.log()
		.created_dirs
		.iter()
		.map(|(uuid, _)| *uuid)
		.collect();
	for item in &outcome.report.top_level {
		assert!(created_uuids.contains(&item.item.uuid()));
	}
	assert!(
		outcome
			.report
			.top_level
			.iter()
			.any(|item| item.source_uuid == first_root.uuid)
	);
	assert_eq!(
		outcome.report.top_level.len(),
		recorder.created.lock().unwrap().len(),
		"every created top-level item was reported as it was created"
	);
	assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn cancel_during_file_copies_drops_transfers_and_keeps_finished_files() {
	let destination = Uuid::new_v4();
	let small = source_file("small", 10);
	let big = source_file("big", 5 * CHUNK_SIZE_U64);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.blocked_uploads.insert("big".to_owned());
	let backend = Arc::new(backend);
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(small.clone()), PlanSource::File(big)],
		),
		control,
	);

	wait_until("the small file is copied", || {
		backend.log().finished.len() == 1
	})
	.await;
	cancel.send_replace(true);
	let outcome = running.await.unwrap();

	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_released(&backend, &reporter);
	assert_eq!(
		backend.log().finished.len(),
		1,
		"the interrupted file never becomes visible"
	);
	assert_eq!(outcome.report.top_level.len(), 1);
	assert_eq!(outcome.report.top_level[0].source_uuid, small.uuid());
	assert_eq!(outcome.report.counts.files_done, 1);
	assert_eq!(
		outcome.report.counts.files_failed, 0,
		"an interrupted file is not a failure"
	);
	assert_eq!(outcome.report.counts.bytes_done, 10);
	assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
}

/// What a finished job reports: everything planned is done, failed or not attempted.
fn assert_counts_add_up(outcome: &CopyOutcome<()>, last: &CopyUpdate) {
	let counts = outcome.report.counts;
	let totals = outcome.report.totals;
	assert_eq!(
		counts.dirs_created + counts.dirs_failed + counts.dirs_not_attempted,
		totals.dirs
	);
	assert_eq!(
		counts.files_done + counts.files_failed + counts.files_not_attempted,
		totals.files
	);
	assert_eq!(
		counts.bytes_done + counts.bytes_failed + counts.bytes_not_attempted,
		totals.bytes
	);
	assert_eq!(
		last.counts, counts,
		"the last update carries the final counts"
	);
	assert_eq!(
		last.eta,
		Some(Duration::ZERO),
		"nothing is left to copy once the job is over"
	);
}

#[tokio::test(start_paused = true)]
async fn a_job_cancelled_during_file_copies_counts_what_it_never_copied() {
	let destination = Uuid::new_v4();
	let mut sources = vec![
		source_file("small", 10),
		source_file("big", 5 * CHUNK_SIZE_U64),
	];
	sources.extend((0..40).map(|i| source_file(&format!("later{i}"), 100)));
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.blocked_uploads.insert("big".to_owned());
	backend.delay = Duration::from_secs(1);
	let backend = Arc::new(backend);
	let (_pause, cancel, control) = controls();
	let (running, recorder, _reporter) = start(
		&backend,
		plan(
			destination,
			sources.iter().cloned().map(PlanSource::File).collect(),
		),
		control,
	);

	wait_until("the small file is copied", || {
		!backend.log().finished.is_empty()
	})
	.await;
	cancel.send_replace(true);
	let outcome = running.await.unwrap();
	let updates = recorder.updates.lock().unwrap().clone();
	let winding_down: Vec<_> = updates
		.iter()
		.filter(|u| u.run_state == RunState::Cancelling && u.phase == CopyPhase::CopyingFiles)
		.collect();
	assert!(!winding_down.is_empty());
	assert!(
		winding_down.iter().all(|u| u.eta.is_none()),
		"a cancelling job has no time left to estimate"
	);

	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Cancelled
	);
	let counts = outcome.report.counts;
	assert!(counts.files_done >= 1);
	assert!(
		counts.files_not_attempted >= 1,
		"the interrupted file is not attempted"
	);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_job_cancelled_during_directory_creation_counts_what_it_never_copied() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(200);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (_pause, cancel, control) = controls();
	let (running, recorder, _reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("some directories exist", || {
		backend.log().created_dirs.len() >= 3
	})
	.await;
	cancel.send_replace(true);
	let outcome = running.await.unwrap();

	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Cancelled
	);
	let counts = outcome.report.counts;
	assert!(counts.dirs_not_attempted > 0);
	assert_eq!(counts.files_not_attempted, 200);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_completed_job_has_nothing_left_unattempted() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let counts = outcome.report.counts;
	assert_eq!(
		(
			counts.dirs_not_attempted,
			counts.files_not_attempted,
			counts.bytes_not_attempted
		),
		(0, 0, 0)
	);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn running_out_of_storage_ends_the_job() {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..10)
		.map(|i| source_file(&format!("f{i}"), 100))
		.collect();
	let mut backend = FakeBackend::new(16, &[destination]);
	backend.delay = Duration::from_secs(10);
	backend
		.slow
		.insert("f2".to_owned(), Duration::from_millis(1));
	backend
		.fail_upload
		.insert("f2".to_owned(), ErrorKind::MaxStorageReached);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			sources.into_iter().map(PlanSource::File).collect(),
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();

	assert_eq!(
		outcome.result.unwrap_err().kind(),
		ErrorKind::MaxStorageReached
	);
	assert_released(&backend, &reporter);
	assert!(
		backend.log().finished.is_empty(),
		"the other files are stopped, not finished"
	);
	assert_eq!(outcome.report.failures.len(), 1);
	assert_eq!(
		outcome.report.failures[0].info.error.kind(),
		ErrorKind::MaxStorageReached
	);
	assert_eq!(recorder.last().phase, CopyPhase::Failed);
}

#[test]
fn a_job_error_keeps_the_server_error_readable() {
	let error = Arc::new(Error::from(filen_types::error::ResponseError::ApiError {
		message: Some("Max storage reached".into()),
		code: Some("max_storage_reached".into()),
	}));
	let returned = job_error(&error);
	assert_eq!(returned.kind(), error.kind());
	assert_eq!(
		returned.server_code().as_deref(),
		Some("max_storage_reached")
	);
	assert_eq!(
		returned.server_message().as_deref(),
		Some("Max storage reached")
	);
}

#[tokio::test(start_paused = true)]
async fn a_failed_file_does_not_stop_the_others() {
	let destination = Uuid::new_v4();
	let good = source_file("good", 2 * CHUNK_SIZE_U64);
	let bad = source_file("bad", 3 * CHUNK_SIZE_U64);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_fetch
		.insert("bad".to_owned(), ErrorKind::FileChunkNotFound);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(good), PlanSource::File(bad.clone())],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();

	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	assert_eq!(backend.log().finished.len(), 1);
	let [failure] = outcome.report.failures.as_slice() else {
		panic!("exactly one failure");
	};
	assert!(matches!(&failure.source, FailedSource::File(file) if file.uuid() == bad.uuid()));
	assert_eq!(failure.info.stage, CopyStage::Download);
	assert_eq!(failure.info.dest_parent, destination);
	assert_eq!(failure.info.dest_name, "bad");
	assert_eq!(failure.info.error.kind(), ErrorKind::FileChunkNotFound);
	let counts = outcome.report.counts;
	assert_eq!((counts.files_done, counts.files_failed), (1, 1));
	assert_eq!(counts.bytes_failed, 3 * CHUNK_SIZE_U64);
	assert_eq!(
		counts.bytes_done + counts.bytes_failed,
		outcome.report.totals.bytes
	);
	assert!(
		recorder
			.events()
			.iter()
			.any(|e| matches!(e, CopyEvent::FileFailed(info) if info.dest_name == "bad"))
	);
}

#[tokio::test(start_paused = true)]
async fn a_failed_directory_fails_its_subtree_without_attempting_it() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let sub = source_dir("Sub");
	let deep = source_dir("Deep");
	let source = tree(
		&top,
		vec![listed(&top, sub.clone()), listed(&sub, deep.clone())],
		vec![
			listed(&top, source_file("kept", 5)),
			listed(&sub, source_file("lost1", 7)),
			listed(&deep, source_file("lost2", 11)),
		],
	);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_create
		.insert("Sub".to_owned(), ErrorKind::Server);
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();

	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	let log = backend.log();
	assert_eq!(
		log.created_dirs.len(),
		1,
		"nothing below the failed directory is attempted"
	);
	assert_eq!(log.finished.len(), 1);
	let [failure] = outcome.report.failures.as_slice() else {
		panic!("one failure for the whole subtree");
	};
	assert!(matches!(failure.source, FailedSource::Dir(())));
	assert_eq!(failure.info.stage, CopyStage::CreateDirectory);
	assert_eq!(failure.info.source_uuid, sub.uuid);
	assert_eq!(
		(failure.info.affected_files, failure.info.affected_bytes),
		(2, 18)
	);
	let counts = outcome.report.counts;
	assert_eq!((counts.dirs_created, counts.dirs_failed), (1, 2));
	assert_eq!((counts.files_done, counts.files_failed), (1, 2));
	assert_eq!(
		counts.bytes_done + counts.bytes_failed,
		outcome.report.totals.bytes
	);
}

#[tokio::test(start_paused = true)]
async fn a_failure_carries_the_directory_it_was_to_be_created_in() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let sub = source_dir("Sub");
	let source = tree(
		&top,
		vec![listed(&top, sub.clone())],
		vec![listed(&top, source_file("nested", 5))],
	);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_create
		.insert("Sub".to_owned(), ErrorKind::Server);
	backend
		.fail_upload
		.insert("nested".to_owned(), ErrorKind::Server);
	backend
		.fail_upload
		.insert("loose".to_owned(), ErrorKind::Server);
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(
			destination,
			vec![source, PlanSource::File(source_file("loose", 5))],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();

	let created_top = backend.log().created_dirs[0].0;
	let parent_of = |name: &str| {
		let failure = outcome
			.report
			.failures
			.iter()
			.find(|f| f.info.dest_name == name)
			.unwrap_or_else(|| panic!("{name} failed"));
		assert_eq!(
			failure.info.dest_parent_dir.uuid(),
			failure.info.dest_parent
		);
		failure.info.dest_parent_dir.clone()
	};
	let sub_parent = parent_of("Sub");
	assert!(
		matches!(&sub_parent, DirType::Dir(dir) if dir.uuid() == created_top),
		"a nested directory's parent is the directory the copy created"
	);
	assert_eq!(parent_of("nested").uuid(), created_top);
	assert_eq!(
		parent_of("loose").uuid(),
		destination,
		"a top-level item's parent is the destination"
	);
	let failed_events = recorder
		.events()
		.into_iter()
		.filter_map(|e| match e {
			CopyEvent::DirFailed(info) | CopyEvent::FileFailed(info) => Some(info),
			_ => None,
		})
		.count();
	assert_eq!(failed_events, 3);
	assert!(recorder.events().iter().all(|e| match e {
		CopyEvent::DirFailed(info) | CopyEvent::FileFailed(info) =>
			info.dest_parent_dir.uuid() == info.dest_parent,
		_ => true,
	}));
}

#[tokio::test(start_paused = true)]
async fn a_top_level_name_taken_after_listing_gets_the_next_name() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let backend = FakeBackend::new(4, &[destination]);
	backend.merge_once.lock().unwrap().insert("Top".to_owned());
	let backend = Arc::new(backend);
	let (running, _recorder, _reporter) = start(
		&backend,
		plan(destination, vec![tree(&top, Vec::new(), Vec::new())]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	assert_eq!(backend.log().created_dirs[0].1, "Top (1)");
	let NonRootItemType::Dir(dir) = &outcome.report.top_level[0].item else {
		panic!("a directory");
	};
	assert_eq!(dir.name(), Some("Top (1)"));
}

#[tokio::test(start_paused = true)]
async fn a_top_level_item_renamed_during_the_copy_is_reported_as_renamed() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let taken = source_file("a.txt", 10);
	let untouched = source_file("b.txt", 10);
	let backend = FakeBackend::new(4, &[destination]);
	// taken after the destination was listed: the directory by a create that merges, the
	// file by the check right before it is registered
	backend.merge_once.lock().unwrap().insert("Top".to_owned());
	backend.existing.lock().unwrap().insert("a.txt".to_owned());
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(
			destination,
			vec![
				tree(&top, Vec::new(), Vec::new()),
				PlanSource::File(taken.clone()),
				PlanSource::File(untouched),
			],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();

	let renamed: Vec<_> = recorder
		.events()
		.into_iter()
		.filter_map(|e| match e {
			CopyEvent::Renamed {
				source_uuid,
				name,
				reason,
				..
			} => Some((source_uuid, name, reason)),
			_ => None,
		})
		.collect();
	assert_eq!(
		renamed,
		[
			(top.uuid, "Top (1)".to_owned(), RenameReason::DuplicateName),
			(
				taken.uuid(),
				"a (1).txt".to_owned(),
				RenameReason::DuplicateName
			),
		]
	);
	let report: Vec<_> = outcome
		.report
		.renamed
		.iter()
		.map(|r| (r.source_uuid, r.name.as_ref().to_owned()))
		.collect();
	assert_eq!(
		report,
		[
			(top.uuid, "Top (1)".to_owned()),
			(taken.uuid(), "a (1).txt".to_owned())
		]
	);
}

#[tokio::test(start_paused = true)]
async fn propagates_every_created_item_into_a_connected_destination() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.targets = ConnectedTargets::with_test_users(2);
	let backend = Arc::new(backend);
	let (running, _recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	running.await.unwrap().result.unwrap();
	let log = backend.log();
	let created: HashSet<Uuid> = log
		.created_dirs
		.iter()
		.map(|(uuid, _)| *uuid)
		.chain(log.finished.keys().copied())
		.collect();
	let propagated: HashSet<Uuid> = log.propagated.iter().copied().collect();
	assert_eq!(created.len(), 7);
	assert_eq!(propagated, created);
	assert!(
		log.propagated_trees.is_empty(),
		"nothing changed during the copy"
	);
}

#[tokio::test(start_paused = true)]
async fn a_destination_shared_during_the_copy_gets_the_copied_trees() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let file = source_file("f", 1);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.later_targets = Some(ConnectedTargets::with_test_users(1));
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source, PlanSource::File(file)]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	let log = backend.log();
	assert!(log.propagated.is_empty());
	let top_level: HashSet<Uuid> = outcome
		.report
		.top_level
		.iter()
		.map(|t| t.item.uuid())
		.collect();
	let trees: HashSet<Uuid> = log.propagated_trees.iter().copied().collect();
	assert_eq!(trees, top_level);
	assert!(
		recorder
			.updates
			.lock()
			.unwrap()
			.iter()
			.any(|u| u.phase == CopyPhase::Finishing)
	);
}

fn unblock_locks(backend: &FakeBackend) {
	backend.block_locks_from.store(usize::MAX, Ordering::SeqCst);
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_a_drive_lock_wait_before_a_file_is_registered() {
	let destination = Uuid::new_v4();
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	backend.block_locks_from.store(0, Ordering::SeqCst);
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source_file("f", 10))]),
		control,
	);

	wait_until("the file waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	cancel.send_replace(true);
	let outcome = tokio::time::timeout(Duration::from_secs(600), running)
		.await
		.expect("a cancel ends a wait for the drive lock")
		.unwrap();

	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_released(&backend, &reporter);
	assert!(backend.log().finished.is_empty());
	assert_eq!(outcome.report.counts.files_done, 0);
	assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
}

#[tokio::test(start_paused = true)]
async fn pause_ends_a_drive_lock_wait_before_a_file_is_registered() {
	let destination = Uuid::new_v4();
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	backend.block_locks_from.store(0, Ordering::SeqCst);
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source_file("f", 10))]),
		control,
	);

	wait_until("the file waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_released(&backend, &reporter);

	unblock_locks(&backend);
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().finished.len(), 1);
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn pause_ends_a_drive_lock_wait_during_directory_creation() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	backend.block_locks_from.store(0, Ordering::SeqCst);
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("directory creation waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_released(&backend, &reporter);
	assert!(backend.log().created_dirs.is_empty());

	unblock_locks(&backend);
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().created_dirs.len(), 4);
	assert_eq!(backend.log().finished.len(), 3);
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_a_drive_lock_wait_while_propagating_to_new_shares() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.later_targets = Some(ConnectedTargets::with_test_users(1));
	// the file's registration takes the lock first; the propagation afterwards waits
	backend.block_locks_from.store(1, Ordering::SeqCst);
	let backend = Arc::new(backend);
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source_file("f", 10))]),
		control,
	);

	wait_until("the propagation waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	cancel.send_replace(true);
	let outcome = tokio::time::timeout(Duration::from_secs(600), running)
		.await
		.expect("a cancel ends a wait for the drive lock")
		.unwrap();

	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Cancelled
	);
	assert_released(&backend, &reporter);
	assert_eq!(outcome.report.top_level.len(), 1, "the copied file is kept");
	assert!(backend.log().propagated_trees.is_empty());
	assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
	assert_counts_add_up(&outcome, &recorder.last());
}

// The first lock call is the shared one directory creation holds; blocking the next ones
// makes a create's own acquisition wait, as it does once the shared lock lost its lease.
#[tokio::test(start_paused = true)]
async fn pause_ends_a_directory_create_waiting_for_a_fresh_lock_and_retries_it() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	backend.block_locks_from.store(1, Ordering::SeqCst);
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("a create waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	assert_released(&backend, &reporter);
	assert!(backend.log().created_dirs.is_empty());

	unblock_locks(&backend);
	pause.send_replace(false);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	let log = backend.log();
	assert_eq!(
		log.created_dirs.len(),
		4,
		"the interrupted create ran once resumed"
	);
	assert!(log.out_of_order_dirs.is_empty());
	assert_eq!(log.finished.len(), 3);
	assert_eq!(outcome.report.counts.dirs_created, 4);
	assert_eq!(outcome.report.counts.dirs_failed, 0);
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_a_directory_create_waiting_for_a_fresh_lock() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	backend.block_locks_from.store(1, Ordering::SeqCst);
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("a create waits for the drive lock", || {
		backend.log().lock_waits == 1
	})
	.await;
	cancel.send_replace(true);
	let outcome = tokio::time::timeout(Duration::from_secs(600), running)
		.await
		.expect("a cancel ends a create's wait for the drive lock")
		.unwrap();

	assert_eq!(outcome.result.unwrap_err().kind(), ErrorKind::Cancelled);
	assert_released(&backend, &reporter);
	assert!(backend.log().created_dirs.is_empty());
	assert_eq!(
		outcome.report.counts.dirs_failed, 0,
		"a create that never started is not a failure"
	);
	assert!(outcome.report.failures.is_empty());
	assert_eq!(recorder.last().phase, CopyPhase::Cancelled);
}

fn top_dir_and_file() -> (SourceDir, RemoteFileType<'static>, Vec<PlanSource>) {
	let top = source_dir("Top");
	let file = source_file("a.txt", 2 * CHUNK_SIZE_U64);
	let sources = vec![
		tree(&top, Vec::new(), Vec::new()),
		PlanSource::File(file.clone()),
	];
	(top, file, sources)
}

#[tokio::test(start_paused = true)]
async fn names_are_checked_before_use_only_when_the_listing_hid_some() {
	for unverified in [false, true] {
		let destination = Uuid::new_v4();
		let (_, _, sources) = top_dir_and_file();
		let backend = Arc::new(FakeBackend::new(4, &[destination]));
		let (running, _recorder, _reporter) = start(
			&backend,
			plan_with(destination, sources, unverified),
			JobControl::default(),
		);
		running.await.unwrap().result.unwrap();
		let probes = backend.log().probes.clone();
		if unverified {
			assert_eq!(
				probes,
				["Top", "a.txt", "a.txt"],
				"the directory before it is created, the file before its upload and again \
				 before it is registered"
			);
		} else {
			assert_eq!(
				probes,
				["a.txt"],
				"only the check right before a top-level file is registered"
			);
		}
	}
}

#[tokio::test(start_paused = true)]
async fn a_taken_top_level_name_moves_to_the_next_keep_both_name() {
	let destination = Uuid::new_v4();
	let (_, _, sources) = top_dir_and_file();
	let backend = FakeBackend::new(4, &[destination]);
	backend
		.existing
		.lock()
		.unwrap()
		.extend(["top".to_owned(), "a.txt".to_owned()]);
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan_with(destination, sources, true),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	let log = backend.log();
	assert_eq!(log.created_dirs[0].1, "Top (1)");
	assert_eq!(log.finished.values().next().unwrap().0, "a (1).txt");
	let names: Vec<_> = outcome
		.report
		.top_level
		.iter()
		.map(|t| t.item.name().unwrap().to_owned())
		.collect();
	assert!(names.contains(&"Top (1)".to_owned()) && names.contains(&"a (1).txt".to_owned()));
	assert!(
		recorder
			.events()
			.iter()
			.any(|e| matches!(e, CopyEvent::FileDone { name, .. } if name == "a (1).txt"))
	);
}

#[tokio::test(start_paused = true)]
async fn a_name_taken_during_the_copy_is_caught_before_the_file_is_registered() {
	let destination = Uuid::new_v4();
	let file = source_file("big.bin", 4 * CHUNK_SIZE_U64);
	let backend = Arc::new(FakeBackend::new(2, &[destination]));
	let (running, _recorder, _reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(file)]),
		JobControl::default(),
	);
	wait_until("a chunk is uploaded", || !backend.log().uploaded.is_empty()).await;
	// another client creates the same name while the copy runs
	backend
		.existing
		.lock()
		.unwrap()
		.insert("big.bin".to_owned());
	running.await.unwrap().result.unwrap();
	let log = backend.log();
	assert_eq!(log.finished.values().next().unwrap().0, "big (1).bin");
}

#[tokio::test(start_paused = true)]
async fn a_file_gives_up_after_the_bounded_number_of_taken_names() {
	let destination = Uuid::new_v4();
	let file = source_file("a.txt", 10);
	let backend = FakeBackend::new(4, &[destination]);
	backend.existing.lock().unwrap().extend(
		std::iter::once("a.txt".to_owned())
			.chain((1..=TOP_LEVEL_NAME_ATTEMPTS).map(|n| format!("a ({n}).txt"))),
	);
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(
		&backend,
		plan_with(destination, vec![PlanSource::File(file)], true),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	assert!(backend.log().finished.is_empty());
	assert!(
		backend.log().uploaded.is_empty(),
		"nothing is uploaded without a free name"
	);
	let [failure] = outcome.report.failures.as_slice() else {
		panic!("one failure");
	};
	assert_eq!(failure.info.error.kind(), ErrorKind::InvalidState);
}

#[tokio::test(start_paused = true)]
async fn a_file_registered_as_a_version_is_reported_and_not_offered_as_a_copy() {
	let destination = Uuid::new_v4();
	let clashing = source_file("a.txt", CHUNK_SIZE_U64 + 5);
	let other = source_file("b.txt", 10);
	let existing = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.version_of.insert("a.txt".to_owned(), existing);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![
				PlanSource::File(clashing.clone()),
				PlanSource::File(other.clone()),
			],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();

	outcome.result.unwrap();
	assert_released(&backend, &reporter);
	let [failure] = outcome.report.failures.as_slice() else {
		panic!("one failure");
	};
	assert_eq!(
		failure.info.stage,
		CopyStage::RegisteredAsVersion {
			existing_file: existing
		}
	);
	assert_eq!(failure.info.dest_parent, destination);
	assert_eq!(failure.info.dest_name, "a.txt");
	assert!(matches!(&failure.source, FailedSource::File(f) if f.uuid() == clashing.uuid()));
	assert_eq!(
		outcome.report.top_level.len(),
		1,
		"only the real copy can be kept or trashed"
	);
	assert_eq!(outcome.report.top_level[0].source_uuid, other.uuid());

	let counts = outcome.report.counts;
	assert_eq!((counts.files_done, counts.files_failed), (1, 1));
	assert_eq!(counts.bytes_failed, clashing.size());
	assert_eq!(
		counts.bytes_done + counts.bytes_failed,
		outcome.report.totals.bytes
	);
	assert!(recorder.events().iter().any(|e| matches!(
		e,
		CopyEvent::FileFailed(info)
			if info.stage == CopyStage::RegisteredAsVersion { existing_file: existing }
	)));
}

/// A planned file whose metadata claims another file's hash.
fn source_file_with_wrong_hash(name: &str, size: u64) -> RemoteFileType<'static> {
	let RemoteFileType::File(file) = source_file(name, size) else {
		unreachable!("source_file builds a file of the user's drive");
	};
	let mut file = file.into_owned();
	if let FileMeta::Decoded(meta) = &mut file.meta {
		meta.hash = Some(Blake3Hash::from(blake3::hash(b"another file")));
	}
	RemoteFileType::File(Cow::Owned(file))
}

fn undecryptable_source_file(size: u64) -> RemoteFileType<'static> {
	let file: crate::fs::file::AnonymousRemoteFile = RemoteFile::from_meta(
		Uuid::new_v4(),
		(),
		Uuid::new_v4().into(),
		size,
		size.div_ceil(CHUNK_SIZE_U64),
		"de-1",
		"bucket",
		Utc::now(),
		false,
		FileMeta::Encrypted(filen_types::crypto::EncryptedString(Cow::Borrowed(
			"garbage",
		))),
	);
	RemoteFileType::File(Cow::Owned(file))
}

fn only_failure(outcome: &CopyOutcome<()>) -> &FailureInfo {
	let [failure] = outcome.report.failures.as_slice() else {
		panic!("exactly one failure, got {:?}", outcome.report.failures);
	};
	&failure.info
}

#[tokio::test(start_paused = true)]
async fn a_deep_chain_is_created_parent_first() {
	let destination = Uuid::new_v4();
	let root = source_dir("root");
	let mut chain = vec![root.clone()];
	for depth in 0..500 {
		chain.push(source_dir(&format!("d{depth}")));
	}
	let dirs = chain
		.windows(2)
		.map(|pair| listed(&pair[0], pair[1].clone()))
		.collect();
	let deepest = chain.last().unwrap();
	let source = tree(&root, dirs, vec![listed(deepest, source_file("bottom", 3))]);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();

	outcome.result.as_ref().unwrap();
	assert_released(&backend, &reporter);
	let log = backend.log();
	assert_eq!(log.created_dirs.len(), 501);
	assert!(log.out_of_order_dirs.is_empty());
	assert_eq!(log.finished.len(), 1);
	drop(log);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn an_empty_directory_is_created_alone() {
	let destination = Uuid::new_v4();
	let empty = source_dir("Empty");
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![tree(&empty, Vec::new(), Vec::new())]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	assert_eq!(backend.log().created_dirs.len(), 1);
	assert_eq!(outcome.report.top_level.len(), 1);
	assert_eq!(outcome.report.counts.dirs_created, 1);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_hash_mismatch_is_logged_and_the_copy_kept() {
	let destination = Uuid::new_v4();
	let source = source_file_with_wrong_hash("a.txt", CHUNK_SIZE_U64 + 3);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, _recorder, _reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source.clone())]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	assert!(outcome.report.failures.is_empty());
	let (_, completion) = backend.log().finished.values().next().unwrap().clone();
	assert_eq!(
		completion.hash,
		file_hash(source.uuid(), source.size()),
		"the copy is registered with the hash of what was read"
	);
}

#[tokio::test(start_paused = true)]
async fn an_inconsistent_chunk_count_fails_the_file() {
	let destination = Uuid::new_v4();
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, _recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(source_file_with_chunks("bad", 10, 3))],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.stage, CopyStage::Download);
	assert_eq!(info.error.kind(), ErrorKind::Response);
	assert!(backend.log().fetched.is_empty(), "nothing is read");
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn a_short_file_fails_as_a_download() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.short_reads.insert("short".to_owned());
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(source_file(
				"short",
				2 * CHUNK_SIZE_U64 + 5,
			))],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.stage, CopyStage::Download);
	assert_eq!(info.error.kind(), ErrorKind::Response);
	assert!(backend.log().finished.is_empty(), "nothing is registered");
	assert_released(&backend, &reporter);
	assert_eq!(outcome.report.counts.bytes_done, 0);
	assert_eq!(outcome.report.counts.bytes_failed, 2 * CHUNK_SIZE_U64 + 5);
}

#[tokio::test(start_paused = true)]
async fn a_failed_upload_is_an_upload_failure() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_upload
		.insert("a.txt".to_owned(), ErrorKind::Server);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![
				PlanSource::File(source_file("a.txt", CHUNK_SIZE_U64 + 1)),
				PlanSource::File(source_file("b.txt", 1)),
			],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.stage, CopyStage::Upload);
	assert_eq!(info.error.kind(), ErrorKind::Server);
	assert_eq!(backend.log().finished.len(), 1);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_failed_registration_is_a_finalize_failure() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_finish
		.insert("a.txt".to_owned(), ErrorKind::Server);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(
			destination,
			vec![PlanSource::File(source_file("a.txt", 2 * CHUNK_SIZE_U64))],
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.stage, CopyStage::Finalize);
	assert_eq!(backend.log().uploaded.len(), 2, "the chunks were uploaded");
	assert!(backend.log().finished.is_empty());
	assert_eq!(outcome.report.counts.bytes_done, 0);
	assert_eq!(outcome.report.counts.bytes_failed, 2 * CHUNK_SIZE_U64);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_failed_drive_lock_fails_the_file_at_finalize() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.fail_locks_from = Some((0, ErrorKind::Server));
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source_file("a.txt", 5))]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	assert_eq!(only_failure(&outcome).stage, CopyStage::Finalize);
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn a_failed_drive_lock_ends_directory_creation() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(5);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.fail_locks_from = Some((0, ErrorKind::Server));
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Server
	);
	assert!(backend.log().created_dirs.is_empty());
	assert_eq!(recorder.last().phase, CopyPhase::Failed);
	assert_eq!(outcome.report.counts.files_not_attempted, 5);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_fatal_error_during_directory_creation_ends_the_job() {
	let destination = Uuid::new_v4();
	// more directories than run at once, so some are never started
	let (_, source) = wide_tree(3 * MAX_SMALL_PARALLEL_REQUESTS);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.fail_create
		.insert("d3".to_owned(), ErrorKind::Unauthenticated);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Unauthenticated
	);
	assert!(
		backend.log().finished.is_empty(),
		"no file is copied after it"
	);
	assert_eq!(recorder.last().phase, CopyPhase::Failed);
	let counts = outcome.report.counts;
	assert!(counts.dirs_not_attempted > 0);
	assert_eq!(
		counts.files_not_attempted + counts.files_failed,
		3 * MAX_SMALL_PARALLEL_REQUESTS as u64
	);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_failed_target_fetch_ends_the_job_before_anything_is_created() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.fail_targets = Some(ErrorKind::Server);
	let backend = Arc::new(backend);
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Server
	);
	assert!(backend.log().created_dirs.is_empty());
	assert!(outcome.report.top_level.is_empty());
	assert_eq!(recorder.last().phase, CopyPhase::Failed);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_failed_color_is_reported_and_the_directory_kept() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.fail_color = true;
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![tree(&top, Vec::new(), Vec::new())]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	assert!(outcome.report.failures.is_empty());
	assert_eq!(outcome.report.counts.dirs_created, 1);
	let created = outcome.report.top_level[0].item.uuid();
	assert!(
		recorder.events().iter().any(
			|e| matches!(e, CopyEvent::ColorFailed { dest_uuid, .. } if *dest_uuid == created)
		)
	);
}

#[tokio::test(start_paused = true)]
async fn a_failed_propagation_is_reported_and_the_copy_continues() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.targets = ConnectedTargets::with_test_users(1);
	backend.fail_propagate = true;
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	assert!(outcome.report.failures.is_empty());
	let failed: HashSet<Uuid> = recorder
		.events()
		.iter()
		.filter_map(|e| match e {
			CopyEvent::PropagationFailed { dest_uuid, .. } => Some(*dest_uuid),
			_ => None,
		})
		.collect();
	let log = backend.log();
	let created: HashSet<Uuid> = log
		.created_dirs
		.iter()
		.map(|(uuid, _)| *uuid)
		.chain(log.finished.keys().copied())
		.collect();
	assert_eq!(failed, created, "every created item reports its failure");
}

#[tokio::test(start_paused = true)]
async fn a_nested_directory_that_merges_is_a_failure() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let sub = source_dir("Sub");
	let source = tree(
		&top,
		vec![listed(&top, sub.clone())],
		vec![listed(&sub, source_file("inside", 4))],
	);
	let backend = FakeBackend::new(4, &[destination]);
	backend.merge_once.lock().unwrap().insert("Sub".to_owned());
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.source_uuid, sub.uuid);
	assert_eq!(info.stage, CopyStage::CreateDirectory);
	assert_eq!(info.error.kind(), ErrorKind::InvalidState);
	assert!(backend.log().finished.is_empty());
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn a_directory_gives_up_after_the_bounded_number_of_taken_names() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let backend = FakeBackend::new(4, &[destination]);
	backend.existing.lock().unwrap().extend(
		std::iter::once("top".to_owned())
			.chain((1..=TOP_LEVEL_NAME_ATTEMPTS).map(|n| format!("top ({n})"))),
	);
	let backend = Arc::new(backend);
	let (running, _recorder, reporter) = start(
		&backend,
		plan_with(
			destination,
			vec![tree(
				&top,
				Vec::new(),
				vec![listed(&top, source_file("inside", 4))],
			)],
			true,
		),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let info = only_failure(&outcome);
	assert_eq!(info.stage, CopyStage::CreateDirectory);
	assert_eq!(info.error.kind(), ErrorKind::InvalidState);
	assert_eq!(info.affected_files, 1);
	assert!(backend.log().created_dirs.is_empty());
	assert!(backend.log().finished.is_empty());
	assert_released(&backend, &reporter);
}

#[tokio::test(start_paused = true)]
async fn renames_found_by_the_name_checks_are_reported() {
	let destination = Uuid::new_v4();
	let (top, file, sources) = top_dir_and_file();
	let backend = FakeBackend::new(4, &[destination]);
	backend
		.existing
		.lock()
		.unwrap()
		.extend(["top".to_owned(), "a.txt".to_owned()]);
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) = start(
		&backend,
		plan_with(destination, sources, true),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.unwrap();
	let renamed: HashMap<Uuid, String> = recorder
		.events()
		.into_iter()
		.filter_map(|e| match e {
			CopyEvent::Renamed {
				source_uuid, name, ..
			} => Some((source_uuid, name)),
			_ => None,
		})
		.collect();
	assert_eq!(renamed.get(&top.uuid).map(String::as_str), Some("Top (1)"));
	assert_eq!(
		renamed.get(&file.uuid()).map(String::as_str),
		Some("a (1).txt")
	);
	assert_eq!(outcome.report.renamed.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn pause_during_the_target_fetch_waits_and_resumes() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.targets_delay = Duration::from_secs(5);
	let backend = Arc::new(backend);
	let (pause, _cancel, control) = controls();
	let (running, _recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("the targets are being fetched", || {
		backend.log().target_fetches == 1
	})
	.await;
	pause.send_replace(true);
	tokio::time::sleep(Duration::from_secs(60)).await;
	assert!(reporter.is_paused());
	assert!(
		backend.log().created_dirs.is_empty(),
		"nothing starts while paused"
	);
	assert_released(&backend, &reporter);
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().created_dirs.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn pause_while_finishing_waits_and_resumes() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let mut backend = FakeBackend::new(4, &[destination]);
	backend.later_targets = Some(ConnectedTargets::with_test_users(1));
	backend.targets_delay = Duration::from_secs(5);
	let backend = Arc::new(backend);
	let (pause, _cancel, control) = controls();
	let (running, recorder, reporter) = start(&backend, plan(destination, vec![source]), control);

	wait_until("the copy is finishing", || {
		recorder
			.updates
			.lock()
			.unwrap()
			.last()
			.is_some_and(|u| u.phase == CopyPhase::Finishing)
	})
	.await;
	pause.send_replace(true);
	wait_until("the job is paused", || reporter.is_paused()).await;
	tokio::time::sleep(Duration::from_secs(60)).await;
	assert!(
		backend.log().propagated_trees.is_empty(),
		"nothing is propagated while paused"
	);
	assert_released(&backend, &reporter);
	pause.send_replace(false);
	running.await.unwrap().result.unwrap();
	assert_eq!(backend.log().propagated_trees.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_pause_controller_dropped_before_the_job_starts_lets_it_run() {
	let destination = Uuid::new_v4();
	let (_, source) = wide_tree(2);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (pause, _cancel, control) = controls();
	pause.send_replace(true);
	drop(pause);
	let (running, _recorder, _reporter) = start(&backend, plan(destination, vec![source]), control);
	let outcome = tokio::time::timeout(Duration::from_secs(600), running)
		.await
		.expect("a lost pause controller is no pause")
		.unwrap();
	outcome.result.unwrap();
	assert_eq!(backend.log().finished.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_registration_in_flight_finishes_on_cancel_and_is_reported() {
	let destination = Uuid::new_v4();
	let mut backend = FakeBackend::new(4, &[destination]);
	backend
		.slow_finish
		.insert("a.txt".to_owned(), Duration::from_secs(3));
	let backend = Arc::new(backend);
	let (_pause, cancel, control) = controls();
	let (running, recorder, reporter) = start(
		&backend,
		plan(destination, vec![PlanSource::File(source_file("a.txt", 5))]),
		control,
	);
	wait_until("the file is being registered", || {
		!backend.log().finishing.is_empty()
	})
	.await;
	cancel.send_replace(true);
	let outcome = running.await.unwrap();

	assert_eq!(
		outcome.result.as_ref().unwrap_err().kind(),
		ErrorKind::Cancelled
	);
	assert_eq!(backend.log().finished.len(), 1);
	assert_eq!(
		outcome.report.top_level.len(),
		1,
		"a file that exists is reported"
	);
	assert_eq!(outcome.report.counts.files_done, 1);
	assert_released(&backend, &reporter);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn skips_and_renames_are_reported_before_anything_is_created() {
	let destination = Uuid::new_v4();
	let top = source_dir("Top");
	let source = tree(
		&top,
		Vec::new(),
		vec![
			listed(&top, source_file("a.txt", 1)),
			listed(&top, source_file("A.txt", 1)),
			listed(&top, undecryptable_source_file(7)),
		],
	);
	let backend = Arc::new(FakeBackend::new(4, &[destination]));
	let (running, recorder, _reporter) = start(
		&backend,
		plan(destination, vec![source]),
		JobControl::default(),
	);
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let events = recorder.events();
	let first = |matches: fn(&CopyEvent) -> bool| events.iter().position(matches).unwrap();
	let created = first(|e| matches!(e, CopyEvent::DirCreated { .. }));
	assert!(first(|e| matches!(e, CopyEvent::Skipped { .. })) < created);
	assert!(first(|e| matches!(e, CopyEvent::Renamed { .. })) < created);
	assert_eq!(outcome.report.counts.entries_skipped, 1);
	assert_eq!(outcome.report.counts.bytes_skipped, 7);
	assert_eq!(outcome.report.skipped.len(), 1);
	assert_eq!(outcome.report.renamed.len(), 1);
	assert_counts_add_up(&outcome, &recorder.last());
}

#[tokio::test(start_paused = true)]
async fn the_estimate_counts_down_while_copying() {
	let destination = Uuid::new_v4();
	let sources: Vec<_> = (0..40)
		.map(|i| PlanSource::File(source_file(&format!("f{i}"), CHUNK_SIZE_U64)))
		.collect();
	let mut backend = FakeBackend::new(2, &[destination]);
	backend.delay = Duration::from_millis(100);
	let backend = Arc::new(backend);
	let (running, recorder, _reporter) =
		start(&backend, plan(destination, sources), JobControl::default());
	let outcome = running.await.unwrap();
	outcome.result.as_ref().unwrap();
	let etas: Vec<Duration> = recorder
		.updates
		.lock()
		.unwrap()
		.iter()
		.filter(|u| u.phase == CopyPhase::CopyingFiles)
		.filter_map(|u| u.eta)
		.collect();
	assert!(etas.len() > 5, "the estimate is reported while copying");
	let (first, last) = (etas[0], *etas.last().unwrap());
	assert!(
		last < first,
		"the estimate falls as the copy advances: {first:?} then {last:?}"
	);
	assert_counts_add_up(&outcome, &recorder.last());
}
