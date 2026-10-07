//! A [`DriveBackend`] for the engine tests of every drive job: an in-memory drive with the real
//! memory semaphore, that logs what the job did and can be told to fail, stall or race.

use std::{
	borrow::Cow,
	collections::{HashMap, HashSet},
	num::NonZeroU32,
	sync::{
		Arc, Mutex, MutexGuard,
		atomic::{AtomicUsize, Ordering},
	},
	time::Duration,
};

use chrono::{DateTime, Utc};
use filen_types::{
	api::v3::dir::color::DirColor,
	crypto::Blake3Hash,
	fs::{ParentUuid, StableUuid, Uuid},
};
use tokio::sync::{Semaphore, watch};

use crate::{
	Error, ErrorKind,
	connect::ConnectedTargets,
	consts::{
		CHUNK_SIZE, CHUNK_SIZE_U64, FILE_CHUNK_SIZE, FILE_CHUNK_SIZE_EXTRA, FULL_CHUNK_BYTES,
	},
	crypto::{file::FileKey, shared::CreateRandom, v3::EncryptionKey},
	fs::{
		HasName, HasUUID,
		categories::{DirType, NonRootItemType, Normal},
		dir::{
			RemoteDirectory,
			meta::{DecryptedDirectoryMeta, DirectoryMeta},
		},
		file::{
			AnonymousRemoteFile, RemoteFile,
			enums::RemoteFileType,
			meta::{DecryptedFileMeta, FileMeta},
			read::chunk_plaintext_len,
			traits::HasFileInfo,
			write::{RemoteFileInfo, UploadCompletion},
		},
		name::ValidatedName,
	},
	job::report::{JobState, Reporter},
};

use super::{
	backend::{CreatedDir, DriveBackend, ListedNames, UploadSpec},
	plan::SourceDir,
};

/// Bytes of memory semaphore that hold `chunks` chunks.
fn budget(chunks: usize) -> usize {
	chunks * FULL_CHUNK_BYTES
}

/// A chunk's worth of the memory semaphore, as a download reserves it.
pub(crate) fn full_chunk() -> NonZeroU32 {
	FILE_CHUNK_SIZE.saturating_add(FILE_CHUNK_SIZE_EXTRA.get())
}

/// The plaintext of chunk `index` of the source file `uuid`.
pub(crate) fn chunk_data(uuid: Uuid, index: u64, size: u64) -> Vec<u8> {
	// The low byte of each, on purpose: a fill pattern distinct per file and per chunk.
	let fill = uuid.as_u128().to_le_bytes()[0] ^ index.to_le_bytes()[0];
	vec![fill; usize::try_from(chunk_plaintext_len(size, index)).unwrap()]
}

/// The hash of the source file `uuid` whose chunks hold [`chunk_data`].
pub(crate) fn source_hash(uuid: Uuid, size: u64) -> Blake3Hash {
	let mut hasher = blake3::Hasher::new();
	for index in 0..size.div_ceil(CHUNK_SIZE_U64) {
		hasher.update(&chunk_data(uuid, index, size));
	}
	Blake3Hash::from(hasher.finalize())
}

/// A file `name` in `parent` holding `bytes`, with `hash` in its metadata.
pub(crate) fn remote_file(
	uuid: Uuid,
	parent: Uuid,
	name: &str,
	bytes: &[u8],
	hash: Option<Blake3Hash>,
) -> RemoteFileType<'static> {
	let size = bytes.len() as u64;
	stored_file(
		uuid,
		parent,
		name,
		size,
		size.div_ceil(CHUNK_SIZE_U64),
		hash,
	)
}

/// A file `name` in `parent` of `size` bytes stored as `chunks` chunks, with `hash` in its
/// metadata.
pub(crate) fn stored_file(
	uuid: Uuid,
	parent: Uuid,
	name: &str,
	size: u64,
	chunks: u64,
	hash: Option<Blake3Hash>,
) -> RemoteFileType<'static> {
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
		uuid,
		(),
		parent.into(),
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

/// The directory the drive items below are in.
pub(crate) const PARENT: Uuid = Uuid::from_u128(0x9a);

/// A directory `name` of the user's drive.
pub(crate) fn drive_dir(uuid: Uuid, name: &str) -> RemoteDirectory {
	RemoteDirectory::from_meta(
		uuid,
		ParentUuid::Uuid(PARENT),
		DirColor::Blue,
		false,
		DateTime::<Utc>::UNIX_EPOCH,
		DirectoryMeta::Decoded(DecryptedDirectoryMeta {
			name: Cow::Owned(name.to_owned()),
			created: None,
		}),
	)
}

/// A file `name` of the user's drive, `size` bytes long.
pub(crate) fn drive_file(uuid: Uuid, name: &str, size: u64) -> RemoteFile {
	RemoteFile::from_meta(
		uuid,
		StableUuid::new_for_test(uuid),
		PARENT.into(),
		size,
		1,
		"de-1",
		"bucket",
		DateTime::<Utc>::UNIX_EPOCH,
		false,
		FileMeta::Decoded(DecryptedFileMeta {
			name: Cow::Owned(name.to_owned()),
			size,
			mime: Cow::Borrowed("application/octet-stream"),
			key: FileKey::V3(EncryptionKey::generate()),
			last_modified: DateTime::<Utc>::UNIX_EPOCH,
			created: None,
			hash: None,
		}),
	)
}

/// A directory `name` a job reads as one of its sources, colored so a copy has a color to keep.
pub(crate) fn source_dir(name: &str) -> SourceDir<()> {
	SourceDir {
		uuid: Uuid::new_v4(),
		name: Some(name.to_owned()),
		created: Some(Utc::now()),
		color: DirColor::Blue,
		handle: (),
	}
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

/// Counts one of several calls running, or locks held, at once, for as long as it lives.
pub(crate) struct Running(Arc<AtomicUsize>);

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

/// A drive lock the fake hands out, counted in [`FakeBackend::live_locks`] while held.
pub(crate) type FakeLock = Running;

#[derive(Default)]
pub(crate) struct FakeLog {
	pub(crate) fetched: Vec<(Uuid, u64)>,
	pub(crate) uploaded: Vec<(Uuid, u64)>,
	/// Where every created directory and registered file is: parent, and a file's size and
	/// chunks. Trashing or deleting an item removes it.
	pub(crate) dir_parents: HashMap<Uuid, Uuid>,
	pub(crate) file_parents: HashMap<Uuid, (Uuid, u64, u64)>,
	pub(crate) trashed_files: Vec<Uuid>,
	pub(crate) deleted_files: Vec<Uuid>,
	pub(crate) trashed_dirs: Vec<Uuid>,
	/// The bytes of each uploaded chunk, when [`Quirk::KeepUploads`] is set.
	pub(crate) uploaded_data: HashMap<(Uuid, u64), Vec<u8>>,
	pub(crate) finished: HashMap<Uuid, (String, UploadCompletion)>,
	/// The directory each file was registered in, kept when the file is removed later.
	pub(crate) registered_in: HashMap<Uuid, Uuid>,
	/// Chunk uploads that started, in order.
	pub(crate) upload_starts: Vec<(Uuid, u64)>,
	/// Most registrations that ran at once.
	pub(crate) peak_finishes: usize,
	pub(crate) created_dirs: Vec<(Uuid, String)>,
	/// Items fetched by uuid ([`DriveBackend::normal_item`]).
	pub(crate) fetched_items: Vec<Uuid>,
	/// Most items fetched by uuid at once.
	pub(crate) peak_item_fetches: usize,
	pub(crate) out_of_order_dirs: Vec<String>,
	pub(crate) colored: Vec<Uuid>,
	pub(crate) propagated: Vec<Uuid>,
	pub(crate) propagated_trees: Vec<Uuid>,
	pub(crate) target_fetches: usize,
	pub(crate) probes: Vec<String>,
	/// Drive-lock acquisitions that had to wait for another holder.
	pub(crate) lock_waits: usize,
	/// Requests that waited while held ([`FakeBackend::hold_requests`]), in the order they came.
	pub(crate) held: Vec<(Request, Uuid)>,
	/// Requests that waited while held by name ([`FakeBackend::hold_named`]), in the order they
	/// came.
	pub(crate) held_named: Vec<(Request, String)>,
}

/// A request to a [`FakeBackend`] that a test can hold, to stop a job at that step: one about an
/// item that exists by its uuid ([`FakeBackend::hold_requests`]), one about an item a job creates
/// by its name ([`FakeBackend::hold_named`]), which the test cannot know the uuid of up front.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Request {
	/// Fetching one of a file's chunks.
	Fetch,
	/// Asking for a file's state.
	State,
	/// Listing a directory's tree.
	List,
	/// Deleting a file permanently.
	Delete,
	/// Propagating an item's tree to the destination's shares and links.
	Propagate,
	/// Creating a directory, by its name.
	Create,
	/// Uploading one of a file's chunks, by the file's name.
	Upload,
	/// Registering an uploaded file, by its name.
	Finish,
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
	/// Listing any directory's tree fails.
	FailTrees,
	/// Later chunks of a file download faster than earlier ones.
	ReverseChunks,
	/// Keep every uploaded chunk's bytes in [`FakeLog::uploaded_data`].
	KeepUploads,
	/// Registered files are not remembered as being in the drive, as if removed at once.
	ForgetRegistered,
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
	/// Fetching any item by its uuid fails with this kind.
	pub(crate) fail_items: Option<ErrorKind>,
	pub(crate) fail_upload: HashMap<String, ErrorKind>,
	pub(crate) fail_create: HashMap<String, ErrorKind>,
	pub(crate) fail_finish: HashMap<String, ErrorKind>,
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
	/// Files with older versions.
	pub(crate) versioned_files: HashSet<Uuid>,
	/// Files whose permanent deletion fails.
	pub(crate) fail_deletes_of: HashSet<Uuid>,
	/// Files and directories the server says are in the trash, wherever the fake drive has them.
	pub(crate) in_trash: HashSet<Uuid>,
	/// Files the server says a newer version superseded.
	pub(crate) superseded: HashSet<Uuid>,
	/// Files and directories whose state cannot be fetched.
	pub(crate) fail_state_of: HashSet<Uuid>,
	/// Requests about an item that wait while they are in the set, each wait logged in
	/// [`FakeLog::held`]. Held by kind as well as item, as one job sends several kinds about
	/// the same item and a test means to stop it at one of them.
	held: watch::Sender<HashSet<(Request, Uuid)>>,
	/// Requests about a directory or file of a name that wait while they are in the set, each
	/// wait logged in [`FakeLog::held_named`].
	held_named: watch::Sender<HashSet<(Request, String)>>,
	/// Lowercased names the destination holds without the listing having shown them.
	pub(crate) existing: Mutex<HashSet<String>>,
	/// Names the server registers as a new version of the given existing file (a client
	/// writing without the drive lock took them at the last moment).
	pub(crate) version_of: HashMap<String, Uuid>,
	pub(crate) lock_calls: AtomicUsize,
	/// Registrations running now.
	finishes: Arc<AtomicUsize>,
	/// Items fetched by uuid now.
	item_fetches: Arc<AtomicUsize>,
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
const DEFAULT_MEMORY_CHUNKS: usize = 4;

impl FakeBackend {
	/// A backend for a job writing into `destination`, the one directory that exists before it.
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
			fail_items: None,
			fail_upload: HashMap::new(),
			fail_create: HashMap::new(),
			fail_finish: HashMap::new(),
			short_reads: HashSet::new(),
			fail_targets: None,
			targets_delay: Duration::ZERO,
			fail_locks_from: None,
			merge_once: Mutex::new(HashSet::new()),
			targets: ConnectedTargets::default(),
			later_targets: None,
			versioned_files: HashSet::new(),
			fail_deletes_of: HashSet::new(),
			in_trash: HashSet::new(),
			superseded: HashSet::new(),
			fail_state_of: HashSet::new(),
			held: watch::Sender::new(HashSet::new()),
			held_named: watch::Sender::new(HashSet::new()),
			existing: Mutex::new(HashSet::new()),
			version_of: HashMap::new(),
			lock_calls: AtomicUsize::new(0),
			finishes: Arc::default(),
			item_fetches: Arc::default(),
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

	/// Asserts a job that ended holds nothing of the backend (every memory reservation and drive
	/// lock given back) and has nothing in flight on its `reporter`.
	pub(crate) fn assert_released<S: JobState>(&self, reporter: &Reporter<S>) {
		assert_eq!(
			self.memory.available_permits(),
			self.budget,
			"every memory reservation is released"
		);
		assert_eq!(
			self.live_locks.load(Ordering::SeqCst),
			0,
			"no drive lock is held"
		);
		assert_eq!(reporter.ops_in_flight(), 0, "nothing is in flight");
	}

	async fn wait(&self, name: &str) {
		tokio::time::sleep(*self.slow.get(name).unwrap_or(&self.delay)).await;
	}

	/// Holds each `request` about any of `items` until [`Self::release_all`].
	pub(crate) fn hold_requests(&self, request: Request, items: impl IntoIterator<Item = Uuid>) {
		self.held
			.send_modify(|held| held.extend(items.into_iter().map(|uuid| (request, uuid))));
	}

	/// Holds each `request` about a directory or file of any of `names` until it is released.
	pub(crate) fn hold_named(
		&self,
		request: Request,
		names: impl IntoIterator<Item = impl Into<String>>,
	) {
		self.held_named.send_modify(|held| {
			held.extend(names.into_iter().map(|name| (request, name.into())));
		});
	}

	/// Lets each `request` about any of `names` go on, and holds none of them from now on.
	pub(crate) fn release_named(
		&self,
		request: Request,
		names: impl IntoIterator<Item = impl Into<String>>,
	) {
		self.held_named.send_modify(|held| {
			for name in names {
				held.remove(&(request, name.into()));
			}
		});
	}

	/// Lets every held request go on, and holds none from now on.
	pub(crate) fn release_all(&self) {
		self.held.send_replace(HashSet::new());
		self.held_named.send_replace(HashSet::new());
	}

	/// Waits while `request` about a directory or file named `name` is held.
	async fn hold_name(&self, request: Request, name: &str) {
		let key = (request, name.to_owned());
		if self.held_named.borrow().contains(&key) {
			self.log().held_named.push(key.clone());
			// the sender lives in `self`, which outlives this wait
			let _ = self
				.held_named
				.subscribe()
				.wait_for(|held| !held.contains(&key))
				.await;
		}
	}

	/// Waits while `request` about `uuid` is held.
	pub(crate) async fn hold(&self, request: Request, uuid: Uuid) {
		let key = (request, uuid);
		if self.held.borrow().contains(&key) {
			self.log().held.push(key);
			// the sender lives in `self`, which outlives this wait
			let _ = self
				.held
				.subscribe()
				.wait_for(|held| !held.contains(&key))
				.await;
		}
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
		Ok(Running::start(&self.live_locks, |_| {}))
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
		self.hold_name(Request::Create, name.as_ref()).await;
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
		self.hold(Request::Propagate, item.uuid()).await;
		self.log().propagated_trees.push(item.uuid());
		Vec::new()
	}

	#[cfg(feature = "archive")]
	async fn normal_item(
		&self,
		uuid: Uuid,
		is_dir: bool,
	) -> Result<NonRootItemType<'static, Normal>, Error> {
		let _running = Running::start(&self.item_fetches, |running| {
			let mut log = self.log();
			log.peak_item_fetches = log.peak_item_fetches.max(running);
		});
		tokio::time::sleep(self.delay).await;
		if let Some(kind) = self.fail_items {
			return Err(Error::custom(kind, "item fetch failed"));
		}
		// a file in the fake drive is fetched at its size; any other is one byte
		let (size, chunks) = {
			let mut log = self.log();
			log.fetched_items.push(uuid);
			log.file_parents
				.get(&uuid)
				.map_or((1, 1), |&(_, size, chunks)| (size, chunks))
		};
		Ok(if is_dir {
			NonRootItemType::Dir(Cow::Owned(RemoteDirectory::new_from_parts(
				uuid,
				DecryptedDirectoryMeta {
					name: Cow::Borrowed("fetched"),
					created: None,
				},
				Uuid::new_v4().into(),
				Utc::now(),
			)))
		} else {
			NonRootItemType::File(Cow::Owned(RemoteFile::from_meta(
				uuid,
				StableUuid::new_for_test(uuid),
				Uuid::new_v4().into(),
				size,
				chunks,
				"de-1",
				"bucket",
				Utc::now(),
				false,
				FileMeta::Decoded(DecryptedFileMeta {
					name: Cow::Borrowed("fetched"),
					size,
					mime: Cow::Borrowed("text/plain"),
					key: FileKey::V3(EncryptionKey::generate()),
					last_modified: Utc::now(),
					created: None,
					hash: None,
				}),
			)))
		})
	}

	fn begin_upload(&self, spec: UploadSpec) -> FakeUpload {
		FakeUpload { spec }
	}

	async fn fetch_chunk(
		&self,
		file: &RemoteFileType<'static>,
		index: u64,
	) -> Result<Vec<u8>, Error> {
		self.hold(Request::Fetch, file.uuid()).await;
		let name = file.name().unwrap_or_default().to_owned();
		if self.quirks.contains(&Quirk::ReverseChunks) {
			tokio::time::sleep(Duration::from_millis(10 * (file.chunks() - index))).await;
		} else {
			self.wait(&name).await;
		}
		if let Some(kind) = self.fail_fetch.get(&name) {
			return Err(Error::custom(*kind, "fetch failed"));
		}
		let mut log = self.log();
		log.fetched.push((file.uuid(), index));
		// a file the job uploaded reads back as it was uploaded
		if let Some(data) = log.uploaded_data.get(&(file.uuid(), index)) {
			return Ok(data.clone());
		}
		drop(log);
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
		self.hold_name(Request::Upload, name).await;
		self.wait(name).await;
		if let Some(kind) = self.fail_upload.get(name) {
			return Err(Error::custom(*kind, "upload failed"));
		}
		assert!(data.len() <= CHUNK_SIZE);
		let mut log = self.log();
		log.uploaded.push((upload.spec.uuid, index));
		if self.quirks.contains(&Quirk::KeepUploads) {
			log.uploaded_data.insert((upload.spec.uuid, index), data);
		}
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
		self.hold_name(Request::Finish, name.as_ref()).await;
		if let Some(kind) = self.fail_finish.get(name.as_ref()) {
			return Err(Error::custom(*kind, "registration failed"));
		}
		assert!(
			!self
				.existing
				.lock()
				.unwrap()
				.contains(&name.as_ref().to_lowercase()),
			"a file must never be registered under a name the destination holds"
		);
		let mut log = self.log();
		// the server's rules for finishing an upload: every chunk index once, and nothing past
		// the last
		let mut indices: Vec<u64> = log
			.uploaded
			.iter()
			.filter(|(file, _)| *file == upload.spec.uuid)
			.map(|(_, index)| *index)
			.collect();
		indices.sort_unstable();
		assert_eq!(
			indices,
			(0..completion.num_chunks).collect::<Vec<_>>(),
			"an upload is finished with each of its chunks uploaded once"
		);
		if self.quirks.contains(&Quirk::KeepUploads) {
			let mut hasher = blake3::Hasher::new();
			for index in 0..completion.num_chunks {
				let chunk = &log.uploaded_data[&(upload.spec.uuid, index)];
				hasher.update(chunk);
			}
			assert_eq!(
				completion.hash,
				filen_types::crypto::Blake3Hash::from(hasher.finalize()),
				"the hash an upload is finished with is of its chunks in order"
			);
		}
		log.finished
			.insert(upload.spec.uuid, (name.as_ref().to_owned(), completion));
		log.registered_in
			.insert(upload.spec.uuid, upload.spec.parent);
		if !self.quirks.contains(&Quirk::ForgetRegistered) {
			log.file_parents.insert(
				upload.spec.uuid,
				(
					upload.spec.parent,
					completion.written,
					completion.num_chunks,
				),
			);
		}
		drop(log);
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
