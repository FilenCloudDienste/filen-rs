//! A [`DriveBackend`] for the engine tests of every drive job: an in-memory drive with the real
//! memory semaphore, that logs what the job did and can be told to fail, stall or race.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor,
	fs::{StableUuid, Uuid},
};
use tokio::sync::{Semaphore, watch};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE_EXTRA_USIZE},
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasName, HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		dir::{RemoteDirectory, meta::DecryptedDirectoryMeta},
		file::{
			RemoteFile,
			enums::RemoteFileType,
			meta::{DecryptedFileMeta, FileMeta},
			read::chunk_plaintext_len,
			traits::HasFileInfo,
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::ValidatedName,
	},
};

use super::backend::{CreatedDir, DriveBackend, ListedNames, UploadSpec};

/// Bytes of memory semaphore that hold `chunks` chunks.
pub(crate) fn budget(chunks: usize) -> usize {
	chunks * (CHUNK_SIZE + FILE_CHUNK_SIZE_EXTRA_USIZE)
}

/// The plaintext of chunk `index` of the source file `uuid`.
pub(crate) fn chunk_data(uuid: Uuid, index: u64, size: u64) -> Vec<u8> {
	// The low byte of each, on purpose: a fill pattern distinct per file and per chunk.
	let fill = uuid.as_u128().to_le_bytes()[0] ^ index.to_le_bytes()[0];
	vec![fill; usize::try_from(chunk_plaintext_len(size, index)).unwrap()]
}

/// Waits until `condition` holds, panicking with `what` if it does not within a generous time.
pub(crate) async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
	for _ in 0..100_000 {
		if condition() {
			return;
		}
		tokio::time::sleep(Duration::from_millis(1)).await;
	}
	panic!("timed out waiting until {what}");
}

/// Counts one of several calls running at once, for as long as it lives.
struct Running(Arc<AtomicUsize>);

impl Running {
	/// Counts a call in `running`, handing `peak` how many run now.
	fn start(running: &Arc<AtomicUsize>, peak: impl FnOnce(usize)) -> Self {
		peak(running.fetch_add(1, Ordering::SeqCst) + 1);
		Self(Arc::clone(running))
	}
}

impl Drop for Running {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

pub(crate) struct FakeLock(Arc<AtomicUsize>);
impl Drop for FakeLock {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

#[derive(Default)]
pub(crate) struct FakeLog {
	pub(crate) fetched: Vec<(Uuid, u64)>,
	pub(crate) uploaded: Vec<(Uuid, u64)>,
	/// The parent of every created directory.
	pub(crate) dir_parents: HashMap<Uuid, Uuid>,
	pub(crate) finished: HashMap<Uuid, (String, UploadCompletion)>,
	/// The directory each file was registered in, kept when the file is removed later.
	pub(crate) registered_in: HashMap<Uuid, Uuid>,
	/// Chunk uploads that started, in order.
	pub(crate) upload_starts: Vec<(Uuid, u64)>,
	/// Most registrations that ran at once.
	pub(crate) peak_finishes: usize,
	pub(crate) created_dirs: Vec<(Uuid, String)>,
	pub(crate) out_of_order_dirs: Vec<String>,
	pub(crate) colored: Vec<Uuid>,
	pub(crate) propagated: Vec<Uuid>,
	pub(crate) propagated_trees: Vec<Uuid>,
	pub(crate) target_fetches: usize,
	pub(crate) probes: Vec<String>,
	/// Drive-lock acquisitions that had to wait for another holder.
	pub(crate) lock_waits: usize,
	/// Slow registrations that have started.
	pub(crate) finishing: Vec<String>,
}

pub(crate) struct FakeUpload {
	pub(crate) spec: UploadSpec,
}

/// A way a [`FakeBackend`] departs from a plain drive, set in [`FakeBackend::quirks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Quirk {
	/// Colors that cannot be set.
	FailColor,
	/// Every propagation to a share or link fails.
	FailPropagate,
	/// Later chunks of a file download faster than earlier ones.
	ReverseChunks,
}

pub(crate) struct FakeBackend {
	pub(crate) memory: Arc<Semaphore>,
	pub(crate) budget: usize,
	pub(crate) live_locks: Arc<AtomicUsize>,
	pub(crate) log: Mutex<FakeLog>,
	pub(crate) known_dirs: Mutex<HashSet<Uuid>>,
	pub(crate) delay: Duration,
	pub(crate) slow: HashMap<String, Duration>,
	pub(crate) fail_fetch: HashMap<String, ErrorKind>,
	pub(crate) fail_upload: HashMap<String, ErrorKind>,
	pub(crate) blocked_uploads: HashSet<String>,
	pub(crate) fail_create: HashMap<String, ErrorKind>,
	pub(crate) fail_finish: HashMap<String, ErrorKind>,
	/// Registrations that take this long, logged in `finishing` when they start.
	pub(crate) slow_finish: HashMap<String, Duration>,
	/// Files whose last chunk comes back one byte short.
	pub(crate) short_reads: HashSet<String>,
	/// Fetching the destination's shares and links fails.
	pub(crate) fail_targets: Option<ErrorKind>,
	pub(crate) targets_delay: Duration,
	/// Drive-lock acquisitions from this call index on fail with this kind.
	pub(crate) fail_locks_from: Option<(usize, ErrorKind)>,
	pub(crate) merge_once: Mutex<HashSet<String>>,
	pub(crate) targets: ConnectedTargets,
	pub(crate) later_targets: Option<ConnectedTargets>,
	/// Lowercased names the destination holds without the listing having shown them.
	pub(crate) existing: Mutex<HashSet<String>>,
	/// Names the server registers as a new version of the given existing file (a client
	/// writing without the drive lock took them at the last moment).
	pub(crate) version_of: HashMap<String, Uuid>,
	pub(crate) lock_calls: AtomicUsize,
	/// Registrations running now.
	finishes: Arc<AtomicUsize>,
	/// Drive-lock acquisitions from this call index on wait (another client holds the lock)
	/// until it is cleared; `None` while the lock is free.
	pub(crate) block_locks_from: watch::Sender<Option<usize>>,
	/// What listing any directory returns.
	pub(crate) listed: ListedNames,
	/// How this backend departs from a plain drive.
	pub(crate) quirks: HashSet<Quirk>,
	/// Files whose chunks are these bytes instead of [`chunk_data`].
	pub(crate) contents: HashMap<Uuid, Vec<u8>>,
}

/// Chunks of memory a [`FakeBackend`] has unless a test asks for [`FakeBackend::with_memory`].
pub(crate) const DEFAULT_MEMORY_CHUNKS: usize = 4;

impl FakeBackend {
	/// A backend copying into `destination`, the one directory that exists before the copy.
	pub(crate) fn new(destination: Uuid) -> Self {
		Self {
			memory: Arc::new(Semaphore::new(budget(DEFAULT_MEMORY_CHUNKS))),
			budget: budget(DEFAULT_MEMORY_CHUNKS),
			live_locks: Arc::new(AtomicUsize::new(0)),
			log: Mutex::new(FakeLog::default()),
			known_dirs: Mutex::new(HashSet::from([destination])),
			delay: Duration::from_millis(10),
			slow: HashMap::new(),
			fail_fetch: HashMap::new(),
			fail_upload: HashMap::new(),
			blocked_uploads: HashSet::new(),
			fail_create: HashMap::new(),
			fail_finish: HashMap::new(),
			slow_finish: HashMap::new(),
			short_reads: HashSet::new(),
			fail_targets: None,
			targets_delay: Duration::ZERO,
			fail_locks_from: None,
			merge_once: Mutex::new(HashSet::new()),
			targets: ConnectedTargets::default(),
			later_targets: None,
			existing: Mutex::new(HashSet::new()),
			version_of: HashMap::new(),
			lock_calls: AtomicUsize::new(0),
			finishes: Arc::default(),
			block_locks_from: watch::Sender::new(None),
			listed: ListedNames::default(),
			quirks: HashSet::new(),
			contents: HashMap::new(),
		}
	}

	/// Room for `chunks` chunks in flight instead of [`DEFAULT_MEMORY_CHUNKS`].
	pub(crate) fn with_memory(mut self, chunks: usize) -> Self {
		self.memory = Arc::new(Semaphore::new(budget(chunks)));
		self.budget = budget(chunks);
		self
	}

	pub(crate) fn log(&self) -> MutexGuard<'_, FakeLog> {
		self.log.lock().unwrap()
	}

	async fn wait(&self, name: &str) {
		tokio::time::sleep(*self.slow.get(name).unwrap_or(&self.delay)).await;
	}
}

impl DriveBackend for FakeBackend {
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
		if self
			.block_locks_from
			.borrow()
			.is_some_and(|from| call >= from)
		{
			self.log().lock_waits += 1;
			// the sender lives in `self`, which outlives this wait
			let _ = self
				.block_locks_from
				.subscribe()
				.wait_for(|from| from.is_none_or(|from| call < from))
				.await;
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

	async fn list_dir_names(&self, _dir: &DirType<'static, Normal>) -> Result<ListedNames, Error> {
		tokio::time::sleep(self.delay).await;
		Ok(self.listed.clone())
	}

	async fn create_dir_unpropagated(
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
		let mut log = self.log();
		log.created_dirs.push((uuid, name.as_ref().to_owned()));
		log.dir_parents.insert(uuid, parent);
		drop(log);
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
		if self.quirks.contains(&Quirk::FailColor) {
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
		if self.quirks.contains(&Quirk::FailPropagate) {
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
		if self.quirks.contains(&Quirk::ReverseChunks) {
			tokio::time::sleep(Duration::from_millis(10 * (file.chunks() - index))).await;
		} else {
			self.wait(&name).await;
		}
		if let Some(kind) = self.fail_fetch.get(&name) {
			return Err(Error::custom(*kind, "fetch failed"));
		}
		self.log().fetched.push((file.uuid(), index));
		if let Some(contents) = self.contents.get(&file.uuid()) {
			let start = usize::try_from(index * CHUNK_SIZE_U64).unwrap();
			return Ok(contents[start..(start + CHUNK_SIZE).min(contents.len())].to_vec());
		}
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
		self.log().upload_starts.push((upload.spec.uuid, index));
		if self.blocked_uploads.contains(name) {
			std::future::pending::<()>().await;
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
		let _running = Running::start(&self.finishes, |running| {
			let mut log = self.log();
			log.peak_finishes = log.peak_finishes.max(running);
		});
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
		self.log()
			.registered_in
			.insert(upload.spec.uuid, upload.spec.parent);
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
