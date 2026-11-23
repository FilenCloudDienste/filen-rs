//! An in-memory drive standing in for the cache: an account root, directory listings and file
//! contents, with switches that fail each call, and gates that hold reads and uploads until a test
//! opens them. Its writes model the server's rules the core relies on: an upload over a name a
//! file holds (in the server's case folding) becomes a new version of that file, a directory
//! created over an existing directory's name returns that directory (the SDK's live
//! `dir_tests::create_dir_dedup_adopts_existing_meta`), one created over a file's name is refused
//! (unverified server behaviour), and nothing is written into a directory it does not hold, or one in the trash.
//! Every change, the core's and another device's (the `remote_*` helpers), stamps the
//! item in a change feed paged as the cache's is. Public with `test-seams`, so a harness outside
//! the crate can serve a tree from it.

use std::{
	collections::{HashMap, HashSet},
	future,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, MutexGuard, PoisonError,
		atomic::{AtomicBool, AtomicUsize, Ordering},
	},
	thread,
	time::Duration,
};

use filen_mobile_native_cache::{
	CacheError,
	abort::FfiAbortSignal,
	ffi::{
		FfiChanges, FfiDir, FfiDirMeta, FfiFile, FfiFileMeta, FfiId, FfiNonRootObject, FfiObject,
		FfiRoot, QueryChildrenResponse, UploadFileInfo,
	},
	range::{ByteRange, RangeIntegrity, RangeRead, RangeReadError, RangeSink},
	remote::{CheckedUpload, PendingCopy},
	thumbnail::{FittedOutcome, FittedThumbnail, encode_fitted},
	traits::WorkingSetUpdateListener,
};
use filen_sdk_rs::{
	consts::CHUNK_SIZE_U64,
	fs::name::collision_key,
	thumbnail::{ByteSource, MemSource, ThumbSpec, might_be_thumbnailable},
};
use filen_types::fs::Uuid;
use serde_json::{Value, json};
use tokio::{
	select,
	sync::{Semaphore, oneshot, watch},
};

use crate::{
	drive::{Drive, held_item},
	sealed,
	test_support::FakeLocal,
};
// What `place_files` builds its placeholders from.
#[cfg(test)]
use crate::{
	identity::PlaceholderIdentity,
	names::PlaceholderName,
	os::{EntryOutcome, PopulateResponder},
	placeholder::{FileSize, FileTime, PlaceholderSpec},
	test_support::FakeOs,
};

/// The creation date the helpers give an item, in ms since the Unix epoch.
const CREATED_MILLIS: i64 = 1_600_000_000_000;
/// The modification date the helpers give a file, in ms since the Unix epoch.
const MODIFIED_MILLIS: i64 = 1_700_000_000_000;
/// The server timestamp the helpers give an item, in ms since the Unix epoch.
pub const TIMESTAMP_MILLIS: i64 = 1_650_000_000_000;

/// A listed file: `name`, `size` bytes, the version `version` of the file `stable`, with the
/// helpers' dates and no declared hash.
pub fn ffi_file(stable: Uuid, version: Uuid, name: &str, size: i64) -> FfiFile {
	FfiFile {
		uuid: version.to_string(),
		stable_uuid: stable.to_string(),
		parent: String::new(),
		original_parent: None,
		meta: Some(FfiFileMeta {
			name: name.to_owned(),
			mime: "application/octet-stream".to_owned(),
			created: Some(CREATED_MILLIS),
			modified: MODIFIED_MILLIS,
			hash: None,
		}),
		size,
		favorite_rank: 0,
		timestamp: TIMESTAMP_MILLIS,
		local_data: None,
		pending_upload_at: None,
		change_seq: 0,
	}
}

/// A listed directory `name` with the uuid `uuid` and the helpers' dates.
pub fn ffi_dir(uuid: Uuid, name: &str) -> FfiDir {
	FfiDir {
		uuid: uuid.to_string(),
		parent: String::new(),
		original_parent: None,
		meta: Some(FfiDirMeta {
			name: name.to_owned(),
			created: Some(CREATED_MILLIS),
		}),
		color: None,
		favorite_rank: 0,
		timestamp: TIMESTAMP_MILLIS,
		last_listed: 0,
		local_data: None,
		change_seq: 0,
	}
}

/// `len` bytes of a deterministic pattern.
pub fn pattern(len: usize) -> Vec<u8> {
	(0..len)
		.map(|i| u8::try_from(i * 31 % 251).expect("below 251"))
		.collect()
}

/// Populates the not yet populated `dir` of `os` with one file placeholder per
/// `(stable, version, size)`, named `f0.bin`, `f1.bin` and so on.
#[cfg(test)]
pub(crate) fn place_files(os: &FakeOs, dir: &PlaceholderIdentity, files: &[(Uuid, Uuid, i64)]) {
	let time = FileTime::from_unix_millis(MODIFIED_MILLIS).expect("a 2023 date");
	let specs: Vec<_> = files
		.iter()
		.enumerate()
		.map(|(n, &(stable, version, size))| {
			PlaceholderSpec::file(
				PlaceholderName::parse(&format!("f{n}.bin")).expect("a valid name"),
				stable,
				version,
				FileSize::from_i64(size).expect("a size"),
				time,
				time,
			)
		})
		.collect();
	let (_, responder) = os.fetch_placeholders(dir);
	assert_eq!(
		responder.populate(&specs),
		Ok(vec![EntryOutcome::Created; files.len()])
	);
}

/// The fake drive.
pub struct FakeDrive {
	state: Mutex<DriveState>,
	/// How many reads have started (passed `on_start`).
	started: watch::Sender<usize>,
	/// Reads started and not yet returned.
	active: AtomicUsize,
	/// The most reads ever active at once.
	max_active: AtomicUsize,
	/// The remote thumbnails in the making.
	thumbnails: InFlight,
}

/// How many calls of a kind are in progress, and the most ever at once.
#[derive(Debug, Default)]
struct InFlight {
	now: AtomicUsize,
	most: AtomicUsize,
}

/// Counts a call as in progress until dropped, its future dropped included.
struct Entered<'a>(&'a InFlight);

impl<'a> Entered<'a> {
	fn new(calls: &'a InFlight) -> Self {
		let now = calls.now.fetch_add(1, Ordering::SeqCst) + 1;
		calls.most.fetch_max(now, Ordering::SeqCst);
		Self(calls)
	}
}

impl Drop for Entered<'_> {
	fn drop(&mut self) {
		self.0.now.fetch_sub(1, Ordering::SeqCst);
	}
}

/// Whether the echoes of the provider's own writes are held back, as the real cache tells its
/// listener only once the server's event for the write came back.
#[derive(Debug, Clone, Copy)]
enum Echoes {
	/// Told as each write returns.
	Told,
	/// Held back; `pending` once a write's echo is waiting to be told.
	Held { pending: bool },
}

struct DriveState {
	root: Uuid,
	/// Listings by the id the cache resolves them under.
	dirs: HashMap<String, Vec<FfiNonRootObject>>,
	files: HashMap<Uuid, FakeContent>,
	/// What a version upload does.
	versions: Versions,
	fail_root: Option<CacheError>,
	/// Every root lookup fails with the error this builds.
	fail_root_always: Option<fn() -> CacheError>,
	/// [`Drive::stop`] was called.
	stopped: bool,
	fail_list: Option<CacheError>,
	fail_read: Option<CacheError>,
	fail_query: Option<CacheError>,
	gate: Option<Arc<Semaphore>>,
	/// Files whose reads never deliver: each waits after its `on_start` until it is aborted.
	held: HashSet<Uuid>,
	/// Files whose reads wait this long after their `on_start` before delivering.
	delayed: HashMap<Uuid, Duration>,
	/// The ranges reads were asked for, in order.
	reads: Vec<(u64, u64)>,
	/// Trashed items, with the listing they came from.
	trash: Vec<(String, FfiNonRootObject)>,
	/// Every call fails as the server unreachable.
	offline: bool,
	/// The next call of each kind fails with the error given.
	fail_next: HashMap<DriveCall, CacheError>,
	upload_gate: Option<Arc<Semaphore>>,
	/// Calls held until they take a permit of their semaphore.
	call_gates: HashMap<DriveCall, Arc<Semaphore>>,
	/// Calls that fail every time, with the error each builds.
	fail_always: HashMap<DriveCall, fn() -> CacheError>,
	/// The edits the cache holds marked, by stable id, as it keeps them in its slot: marked by
	/// [`Drive::upload_version`] before the upload and cleared before it returns, so only an upload
	/// dropped on its way leaves one.
	pending: HashMap<Uuid, Arc<[u8]>>,
	/// Every version each file was given after it was listed, oldest first: what reached the drive.
	history: HashMap<Uuid, Vec<Arc<[u8]>>>,
	/// Where uploads read their bytes from.
	local: Option<FakeLocal>,
	ops: Vec<DriveOp>,
	feed: Feed,
	/// Who to tell of another device's changes.
	listener: Option<Arc<dyn WorkingSetUpdateListener>>,
	/// Another device's changes tell no listener.
	quiet: bool,
	/// Whether the echoes of the provider's own writes are held back.
	echoes: Echoes,
	/// What the cache's reads serve while it is stale: the listings and trash as they were.
	stale: Option<Snapshot>,
	/// The directories last reported populated, and how many reports there were.
	reported: (Vec<String>, usize),
	/// How many pages of the change feed were read.
	pages_read: usize,
	/// Directories whose children the cache has not listed yet: its own reads give them none
	/// until a listing refreshed from the server.
	unlisted: HashSet<String>,
	/// The fitted thumbnails kept, by the file version and side made at, as the cache keeps them.
	fitted: HashMap<(Uuid, u32), FittedThumbnail>,
	/// How many times the kept thumbnails were looked in.
	cache_lookups: usize,
	/// Whether thumbnails made are kept.
	fitted_kept: Fitted,
}

/// The change feed: every item's latest change, and the items removed for good.
struct Feed {
	/// The incarnation of the cache that issues the anchors.
	instance: u128,
	/// The latest change's number.
	seq: i64,
	/// Each item's latest change, by uuid text (a file's stable id, a directory's uuid).
	stamps: HashMap<String, i64>,
	/// Items removed for good, with the change that removed them.
	tombstones: Vec<(i64, String)>,
}

/// The listings and trash a stale cache serves.
#[derive(Clone)]
struct Snapshot {
	dirs: HashMap<String, Vec<FfiNonRootObject>>,
	trash: Vec<(String, FfiNonRootObject)>,
}

/// A drive call a switch can fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DriveCall {
	/// [`Drive::list_children`].
	List,
	/// [`Drive::refresh_item`].
	Refresh,
	/// [`Drive::upload_file`].
	Upload,
	/// [`Drive::upload_version`].
	UploadVersion,
	/// [`Drive::create_dir`].
	CreateDir,
	/// [`Drive::trash`].
	Trash,
	/// [`Drive::rename`].
	Rename,
	/// [`Drive::move_to`].
	Move,
	/// [`Drive::changes`].
	Changes,
	/// [`Drive::change_anchor`].
	ChangeAnchor,
	/// [`Drive::cached_children`].
	CachedChildren,
	/// [`Drive::query_item`].
	Query,
	/// [`Drive::copy_pending_upload`], gated once its copy is written, before it returns.
	CopyPending,
	/// [`Drive::fitted_thumbnail`] and [`Drive::fitted_thumbnail_of_local`]; only the remote one
	/// is gated.
	Thumbnail,
	/// [`Drive::cached_fitted_thumbnail`]; never gated.
	CachedThumbnail,
}

/// A call the drive answered, in order, by the ids and names it was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriveOp {
	/// A listing of the directory `dir`.
	List(String),
	/// A refresh of the item `id`.
	Refresh(String),
	/// An upload of a new file `name` into `parent`.
	Upload { parent: String, name: String },
	/// An upload of a new version of the file `file`.
	UploadVersion(String),
	/// A directory `name` created in `parent`.
	CreateDir { parent: String, name: String },
	/// The item `id` trashed.
	Trash(String),
	/// The item `id` renamed to `name`.
	Rename { id: String, name: String },
	/// The item `id` moved into `parent`.
	Move { id: String, parent: String },
	/// The cache's copy of a marked edit of the file `id` asked for.
	CopyPending(String),
	/// A thumbnail of the remote file `id` asked for.
	Thumbnail(String),
	/// A thumbnail of local bytes of a file named `name` asked for.
	LocalThumbnail(String),
}

impl DriveOp {
	/// Whether the call changed the drive.
	pub fn writes(&self) -> bool {
		!matches!(
			self,
			Self::List(_)
				| Self::Refresh(_)
				| Self::CopyPending(_)
				| Self::Thumbnail(_)
				| Self::LocalThumbnail(_)
		)
	}

	/// The call as JSON: `op` names it, the other fields are its arguments.
	fn to_json(&self) -> Value {
		match self {
			Self::List(dir) => json!({ "op": "List", "id": dir }),
			Self::Refresh(id) => json!({ "op": "Refresh", "id": id }),
			Self::Upload { parent, name } => {
				json!({ "op": "Upload", "parent": parent, "name": name })
			}
			Self::UploadVersion(id) => json!({ "op": "UploadVersion", "id": id }),
			Self::CreateDir { parent, name } => {
				json!({ "op": "CreateDir", "parent": parent, "name": name })
			}
			Self::Trash(id) => json!({ "op": "Trash", "id": id }),
			Self::Rename { id, name } => json!({ "op": "Rename", "id": id, "name": name }),
			Self::Move { id, parent } => json!({ "op": "Move", "id": id, "parent": parent }),
			Self::CopyPending(id) => json!({ "op": "CopyPending", "id": id }),
			Self::Thumbnail(id) => json!({ "op": "Thumbnail", "id": id }),
			Self::LocalThumbnail(name) => json!({ "op": "LocalThumbnail", "name": name }),
		}
	}
}

/// The uuid a listed child is resolved by: a file's stable id, a directory's uuid.
fn child_uuid(child: &FfiNonRootObject) -> &str {
	match child {
		FfiNonRootObject::File(file) => &file.stable_uuid,
		FfiNonRootObject::Dir(dir) => &dir.uuid,
	}
}

/// A listed child's name, when its metadata decrypts.
fn child_name(child: &FfiNonRootObject) -> Option<&str> {
	match child {
		FfiNonRootObject::File(file) => file.meta.as_ref().map(|meta| meta.name.as_str()),
		FfiNonRootObject::Dir(dir) => dir.meta.as_ref().map(|meta| meta.name.as_str()),
	}
}

fn set_name(child: &mut FfiNonRootObject, name: &str) {
	match child {
		FfiNonRootObject::File(file) => {
			if let Some(meta) = &mut file.meta {
				name.clone_into(&mut meta.name);
			}
		}
		FfiNonRootObject::Dir(dir) => {
			if let Some(meta) = &mut dir.meta {
				name.clone_into(&mut meta.name);
			}
		}
	}
}

fn set_parent(child: &mut FfiNonRootObject, parent: String) {
	match child {
		FfiNonRootObject::File(file) => file.parent = parent,
		FfiNonRootObject::Dir(dir) => dir.parent = parent,
	}
}

fn set_original_parent(child: &mut FfiNonRootObject, original: Option<String>) {
	match child {
		FfiNonRootObject::File(file) => file.original_parent = original,
		FfiNonRootObject::Dir(dir) => dir.original_parent = original,
	}
}

/// The anchor of the change `seq` of the cache incarnation `instance`: the incarnation's 16 bytes,
/// then the number, as the cache lays it out.
fn encode_anchor(instance: u128, seq: i64) -> Vec<u8> {
	let mut anchor = instance.to_le_bytes().to_vec();
	anchor.extend_from_slice(&seq.to_le_bytes());
	anchor
}

/// The incarnation and change number of `anchor`, when it is one.
fn decode_anchor(anchor: &[u8]) -> Option<(u128, i64)> {
	let (instance, seq) = anchor.split_first_chunk::<16>()?;
	let seq: &[u8; 8] = seq.try_into().ok()?;
	Some((u128::from_le_bytes(*instance), i64::from_le_bytes(*seq)))
}

fn object(child: FfiNonRootObject) -> FfiObject {
	match child {
		FfiNonRootObject::File(file) => FfiObject::File(file),
		FfiNonRootObject::Dir(dir) => FfiObject::Dir(dir),
	}
}

fn offline_error() -> CacheError {
	CacheError::remote("the fake drive is offline")
}

/// What the drive does with a version upload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Versions {
	/// Takes it, as the server does.
	Taken,
	/// Answers with the file as it is, taking nothing.
	Ignored,
}

/// What the drive does with a remote thumbnail it made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fitted {
	/// Keeps it, as the cache does.
	Kept,
	/// Keeps nothing, as a cache that cannot write.
	Unkept,
}

/// One file's content.
struct FakeContent {
	version: Uuid,
	bytes: Arc<[u8]>,
	/// The bytes' BLAKE3 hash, worked out once: the dump the VM harness writes every second lists
	/// it for every file, and worked out there, it hashed every byte of the drive (73 MB) each
	/// second with the drive's lock held.
	hash: blake3::Hash,
	/// The hash the file declares, of any length, as the cache would list it.
	declared: Option<Vec<u8>>,
	/// A size other than the bytes' length.
	size: Option<u64>,
	/// Where the server's bytes end, short of the size.
	truncated_at: Option<u64>,
}

/// Counts a read as active until dropped.
struct ActiveRead<'a>(&'a AtomicUsize);

impl Drop for ActiveRead<'_> {
	fn drop(&mut self) {
		self.0.fetch_sub(1, Ordering::SeqCst);
	}
}

impl FakeDrive {
	/// A drive of the account whose root directory is `root`, holding nothing.
	pub fn new(root: Uuid) -> Self {
		Self {
			state: Mutex::new(DriveState {
				root,
				dirs: HashMap::new(),
				files: HashMap::new(),
				fail_root: None,
				fail_root_always: None,
				stopped: false,
				fail_list: None,
				fail_read: None,
				fail_query: None,
				gate: None,
				held: HashSet::new(),
				delayed: HashMap::new(),
				reads: Vec::new(),
				trash: Vec::new(),
				offline: false,
				versions: Versions::Taken,
				fail_next: HashMap::new(),
				upload_gate: None,
				call_gates: HashMap::new(),
				fail_always: HashMap::new(),
				pending: HashMap::new(),
				history: HashMap::new(),
				local: None,
				ops: Vec::new(),
				feed: Feed {
					instance: 1,
					seq: 0,
					stamps: HashMap::new(),
					tombstones: Vec::new(),
				},
				listener: None,
				quiet: false,
				echoes: Echoes::Told,
				stale: None,
				reported: (Vec::new(), 0),
				pages_read: 0,
				unlisted: HashSet::new(),
				fitted: HashMap::new(),
				cache_lookups: 0,
				fitted_kept: Fitted::Kept,
			}),
			thumbnails: InFlight::default(),
			started: watch::Sender::new(0),
			active: AtomicUsize::new(0),
			max_active: AtomicUsize::new(0),
		}
	}

	fn state(&self) -> MutexGuard<'_, DriveState> {
		self.state.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Lists `children` under the cache id `dir`.
	pub fn list(&self, dir: &str, children: Vec<FfiNonRootObject>) {
		self.state().dirs.insert(dir.to_owned(), children);
	}

	/// Holds `bytes` as the version `version` of the file `stable`, declaring their BLAKE3 hash.
	pub fn add_file(&self, stable: Uuid, version: Uuid, bytes: &[u8]) {
		let hash = blake3::hash(bytes);
		self.state().files.insert(
			stable,
			FakeContent {
				version,
				bytes: Arc::from(bytes),
				hash,
				declared: Some(hash.as_bytes().to_vec()),
				size: None,
				truncated_at: None,
			},
		);
	}

	fn with_file(&self, stable: Uuid, change: impl FnOnce(&mut FakeContent)) {
		change(
			self.state()
				.files
				.get_mut(&stable)
				.expect("the file was added"),
		);
	}

	/// Makes the file `stable` declare `declared` as its hash.
	pub fn declare(&self, stable: Uuid, declared: Option<[u8; 32]>) {
		self.with_file(stable, |file| file.declared = declared.map(Vec::from));
	}

	/// Makes the file `stable` declare `declared`, of any length, as its hash.
	pub fn declare_bytes(&self, stable: Uuid, declared: &[u8]) {
		self.with_file(stable, |file| file.declared = Some(declared.to_vec()));
	}

	/// Makes the cache say the file `stable` is `size` bytes long, whatever its bytes.
	pub fn set_size(&self, stable: Uuid, size: u64) {
		self.with_file(stable, |file| file.size = Some(size));
	}

	/// Makes the file `stable` a new version, `version`, of the same bytes.
	pub fn bump_version(&self, stable: Uuid, version: Uuid) {
		self.with_file(stable, |file| file.version = version);
		Self::stamp(&mut self.state(), &stable.to_string());
	}

	/// Makes the server's bytes of the file `stable` end at `offset`, short of its size.
	pub fn truncate_at(&self, stable: Uuid, offset: u64) {
		self.with_file(stable, |file| file.truncated_at = Some(offset));
	}

	/// Fails the next root lookup with `error`.
	pub fn fail_root(&self, error: CacheError) {
		self.state().fail_root = Some(error);
	}

	/// Fails every root lookup with the error `error` builds, as the cache does while it holds no
	/// session, or answers them again with `None`.
	pub fn fail_root_always(&self, error: Option<fn() -> CacheError>) {
		self.state().fail_root_always = error;
	}

	/// Whether [`Drive::stop`] was called.
	pub fn stopped(&self) -> bool {
		self.state().stopped
	}

	/// Fails the next listing with `error`.
	pub fn fail_list(&self, error: CacheError) {
		self.state().fail_list = Some(error);
	}

	/// Fails the next read with `error`, before it resolves the file.
	pub fn fail_read(&self, error: CacheError) {
		self.state().fail_read = Some(error);
	}

	/// Fails the next item lookup with `error`.
	pub fn fail_query(&self, error: CacheError) {
		self.state().fail_query = Some(error);
	}

	/// Holds every later read after its `on_start` until it takes a permit of the semaphore
	/// returned, which starts with none: a test adds one permit per read it lets through.
	pub fn gate_reads(&self) -> Arc<Semaphore> {
		let gate = Arc::new(Semaphore::new(0));
		self.state().gate = Some(Arc::clone(&gate));
		gate
	}

	/// Holds every later read of the file `stable` after its `on_start` until it is aborted, so a
	/// reader can cancel it at leisure.
	pub fn hold_file(&self, stable: Uuid) {
		self.state().held.insert(stable);
	}

	/// Makes every later read of the file `stable` wait `delay` of real time after its `on_start`
	/// before delivering, as a slow server would, so readers in other processes can overlap and
	/// cancel reads of it. Timed on a thread of its own, so the runtime needs no timer.
	///
	/// For the VM harness only, whose readers are other processes on the real clock: a unit test
	/// holds reads with [`FakeDrive::gate_reads`] or [`FakeDrive::hold_file`] instead, since a paused
	/// clock does not reach this thread's sleep.
	pub fn delay_file(&self, stable: Uuid, delay: Duration) {
		self.state().delayed.insert(stable, delay);
	}

	/// Waits until `count` reads have started.
	pub async fn reads_started(&self, count: usize) {
		self.started
			.subscribe()
			.wait_for(|started| *started >= count)
			.await
			.expect("the drive holds the sender");
	}

	/// The most reads that were ever in progress at once.
	pub fn max_concurrent_reads(&self) -> usize {
		self.max_active.load(Ordering::SeqCst)
	}

	/// How many remote thumbnails are in the making now, and the most ever at once; a call whose
	/// future was dropped is no longer counted.
	pub fn thumbnails_in_flight(&self) -> (usize, usize) {
		(
			self.thumbnails.now.load(Ordering::SeqCst),
			self.thumbnails.most.load(Ordering::SeqCst),
		)
	}

	/// How many times the kept thumbnails were looked in ([`Drive::cached_fitted_thumbnail`]).
	pub fn thumbnail_cache_lookups(&self) -> usize {
		self.state().cache_lookups
	}

	/// Whether a fitted thumbnail of the file version `version` made at `side` is kept.
	pub fn keeps_thumbnail(&self, version: Uuid, side: u32) -> bool {
		self.state().fitted.contains_key(&(version, side))
	}

	/// Makes every later remote thumbnail made go unkept, as a cache that cannot write leaves it,
	/// or kept again.
	pub fn keep_no_thumbnails(&self, none: bool) {
		self.state().fitted_kept = if none { Fitted::Unkept } else { Fitted::Kept };
	}

	/// The `(start, end)` of every range a read was asked for, in order.
	pub fn reads(&self) -> Vec<(u64, u64)> {
		self.state().reads.clone()
	}

	/// Makes every later call fail as the server unreachable, or answer again.
	pub fn set_offline(&self, offline: bool) {
		self.state().offline = offline;
	}

	/// Makes every later version upload answer with the file as the drive holds it, taking nothing,
	/// as a drive that never takes an edit would; `false` takes them again.
	pub fn take_no_versions(&self, refuse: bool) {
		self.state().versions = if refuse {
			Versions::Ignored
		} else {
			Versions::Taken
		};
	}

	/// Fails the next `call` with `error`.
	pub fn fail_next(&self, call: DriveCall, error: CacheError) {
		self.state().fail_next.insert(call, error);
	}

	/// Holds every later upload, a new file or a version, until it takes a permit of the semaphore
	/// returned, which starts with none. An upload reads its bytes before it waits, so a write while
	/// it waits is not in what it uploads, as a file changed during a real upload.
	pub fn gate_uploads(&self) -> Arc<Semaphore> {
		let gate = Arc::new(Semaphore::new(0));
		self.state().upload_gate = Some(Arc::clone(&gate));
		gate
	}

	/// Holds every later `call` until it takes a permit of the semaphore returned, which starts
	/// with none. The call is recorded before it waits.
	pub fn gate_calls(&self, call: DriveCall) -> Arc<Semaphore> {
		let gate = Arc::new(Semaphore::new(0));
		self.state().call_gates.insert(call, Arc::clone(&gate));
		gate
	}

	/// Holds no later `call`: one already waiting at the gate still waits for its permit.
	pub fn stop_gating(&self, call: DriveCall) {
		self.state().call_gates.remove(&call);
	}

	/// Fails every later `call` with the error `error` builds.
	pub fn fail_always(&self, call: DriveCall, error: fn() -> CacheError) {
		self.state().fail_always.insert(call, error);
	}

	/// Answers every later `call` again, after [`FakeDrive::fail_always`].
	pub fn stop_failing(&self, call: DriveCall) {
		self.state().fail_always.remove(&call);
	}

	/// The stable ids of the edits the cache holds marked.
	pub fn pending_uploads(&self) -> Vec<Uuid> {
		self.state().pending.keys().copied().collect()
	}

	/// Marks an edit of the file `stable` holding `bytes`, as an upload dropped on its way, or a
	/// build that left failed uploads to the cache's drain, leaves it.
	pub fn mark_pending(&self, stable: Uuid, bytes: &[u8]) {
		self.state().pending.insert(stable, Arc::from(bytes));
	}

	/// The versions the file `stable` was given since it was listed, oldest first.
	pub fn history(&self, stable: Uuid) -> Vec<Vec<u8>> {
		self.state()
			.history
			.get(&stable)
			.map(|versions| versions.iter().map(|bytes| bytes.to_vec()).collect())
			.unwrap_or_default()
	}

	/// Makes uploads read their bytes from `local`'s files.
	pub fn link_local(&self, local: &FakeLocal) {
		self.state().local = Some(local.clone());
	}

	/// Lists `name` holding `bytes` in the directory `dir` (a cache id) as a new file, and returns
	/// it.
	pub fn add_remote_file(&self, dir: &str, name: &str, bytes: &[u8]) -> FfiFile {
		let file = Self::new_file(dir, name, bytes);
		Self::store_file(&mut self.state(), dir, &file, bytes);
		self.signal();
		file
	}

	/// Lists the directory `name` in the directory `dir` (a cache id), and returns it.
	pub fn add_remote_dir(&self, dir: &str, name: &str) -> FfiDir {
		let created = Self::new_dir(dir, name);
		Self::store_dir(&mut self.state(), dir, &created);
		self.signal();
		created
	}

	/// Gives the file `stable` a new version holding `bytes`, as an edit elsewhere would, and
	/// returns the new version's uuid.
	pub fn bump_head(&self, stable: Uuid, bytes: &[u8]) -> Uuid {
		let version =
			Self::new_version(&mut self.state(), stable, bytes).expect("the file is listed");
		self.signal();
		version
	}

	/// Renames the item `uuid` to `name`, as another device would.
	pub fn remote_rename(&self, uuid: Uuid, name: &str) {
		{
			let mut state = self.state();
			let uuid = uuid.to_string();
			let (dir, at) = Self::find(&state, &uuid).expect("the item is listed");
			let item = state
				.dirs
				.get_mut(&dir)
				.and_then(|children| children.get_mut(at))
				.expect("found above");
			set_name(item, name);
			Self::stamp(&mut state, &uuid);
		}
		self.signal();
	}

	/// Marks the listed file `stable` as holding an edit not yet on the server, as the cache's row
	/// of a file whose upload has not finished says, or not.
	pub fn set_upload_pending(&self, stable: Uuid, pending: bool) {
		let mut state = self.state();
		let uuid = stable.to_string();
		let (dir, at) = Self::find(&state, &uuid).expect("the item is listed");
		match state
			.dirs
			.get_mut(&dir)
			.and_then(|children| children.get_mut(at))
			.expect("found above")
		{
			FfiNonRootObject::File(file) => {
				file.pending_upload_at = pending.then_some(TIMESTAMP_MILLIS)
			}
			FfiNonRootObject::Dir(_) => panic!("{uuid} is a directory"),
		}
	}

	/// Makes the metadata of the item `uuid` undecryptable, as a key change elsewhere would.
	pub fn remote_garble(&self, uuid: Uuid) {
		{
			let mut state = self.state();
			let uuid = uuid.to_string();
			let (dir, at) = Self::find(&state, &uuid).expect("the item is listed");
			match state
				.dirs
				.get_mut(&dir)
				.and_then(|children| children.get_mut(at))
				.expect("found above")
			{
				FfiNonRootObject::File(file) => file.meta = None,
				FfiNonRootObject::Dir(dir) => dir.meta = None,
			}
			Self::stamp(&mut state, &uuid);
		}
		self.signal();
	}

	/// Moves the item `uuid` into the directory `dir` (a cache id), as another device would.
	pub fn remote_move(&self, uuid: Uuid, dir: &str) {
		{
			let mut state = self.state();
			let uuid = uuid.to_string();
			let (from, at) = Self::find(&state, &uuid).expect("the item is listed");
			let mut item = state.dirs.get_mut(&from).expect("found above").remove(at);
			set_parent(&mut item, Self::parent_uuid(dir));
			state.dirs.entry(dir.to_owned()).or_default().push(item);
			Self::stamp(&mut state, &uuid);
		}
		self.signal();
	}

	/// Moves the item `uuid` to the trash, as another device would.
	pub fn remote_trash(&self, uuid: Uuid) {
		Self::trash_listed(&mut self.state(), &uuid.to_string()).expect("the item is listed");
		self.signal();
	}

	/// Restores the trashed item `uuid` into the directory it was trashed from, as another device
	/// would.
	pub fn remote_restore(&self, uuid: Uuid) {
		{
			let mut state = self.state();
			let uuid = uuid.to_string();
			let at = state
				.trash
				.iter()
				.position(|(_, item)| child_uuid(item) == uuid)
				.expect("the item is in the trash");
			let (dir, mut item) = state.trash.remove(at);
			set_original_parent(&mut item, None);
			state.dirs.entry(dir).or_default().push(item);
			Self::stamp(&mut state, &uuid);
		}
		self.signal();
	}

	/// Deletes the item `uuid` for good, from its listing or the trash, and with it everything a
	/// directory held, as another device would.
	pub fn remote_delete(&self, uuid: Uuid) {
		{
			let mut state = self.state();
			let uuid = uuid.to_string();
			match Self::find(&state, &uuid) {
				Some((dir, at)) => {
					state.dirs.get_mut(&dir).expect("found above").remove(at);
				}
				None => state.trash.retain(|(_, item)| child_uuid(item) != uuid),
			}
			let mut gone = vec![uuid];
			while let Some(uuid) = gone.pop() {
				if let Some(children) = state.dirs.remove(&format!("stable/{uuid}")) {
					gone.extend(children.iter().map(|child| child_uuid(child).to_owned()));
				}
				if let Ok(stable) = Uuid::parse_str(&uuid) {
					state.files.remove(&stable);
				}
				state.feed.stamps.remove(&uuid);
				state.feed.seq += 1;
				let seq = state.feed.seq;
				state.feed.tombstones.push((seq, uuid));
			}
		}
		self.signal();
	}

	/// Makes the cache a new incarnation, as a wipe or a schema rebuild does: every anchor issued
	/// before is refused as expired.
	pub fn wipe_feed(&self) {
		self.state().feed.instance += 1;
		self.signal();
	}

	/// Holds back the listener's call for each of the provider's own writes from now on, or calls it
	/// as each write returns again: the real cache calls it later, once the server's event for the
	/// write is applied (measured), so after the writer's next steps, or after another device's
	/// change. The feed holds the write meanwhile, as the cache's does.
	pub fn hold_echoes(&self, hold: bool) {
		let mut state = self.state();
		state.echoes = match (hold, state.echoes) {
			(true, Echoes::Told) => Echoes::Held { pending: false },
			(true, held @ Echoes::Held { .. }) => held,
			(false, _) => Echoes::Told,
		};
	}

	/// Tells the listener of the writes whose echoes were held back, once for all of them; whether
	/// any was.
	pub fn release_echoes(&self) -> bool {
		let held = {
			let mut state = self.state();
			let held = matches!(state.echoes, Echoes::Held { pending: true });
			if held {
				state.echoes = Echoes::Held { pending: false };
			}
			held
		};
		if held {
			self.signal();
		}
		held
	}

	/// Tells the listener once more, as an echo the server delivers twice does.
	pub fn echo_again(&self) {
		self.signal();
	}

	/// Makes another device's changes tell the listener nothing, or tell it again.
	pub fn quiet_signals(&self, quiet: bool) {
		self.state().quiet = quiet;
	}

	/// Makes the cache's reads ([`Drive::cached_children`], [`Drive::query_item`]) serve the
	/// listings and trash as they are now, while the refreshing calls see every later change; or
	/// fresh again.
	pub fn stale_cache(&self, stale: bool) {
		let mut state = self.state();
		state.stale = stale.then(|| Snapshot {
			dirs: state.dirs.clone(),
			trash: state.trash.clone(),
		});
	}

	/// Makes the cache hold no children of the directory `dir` (a cache id) until a listing of it
	/// refreshed from the server, as for a directory it knows from its parent's listing alone.
	pub fn unlisted(&self, dir: &str) {
		self.state().unlisted.insert(dir.to_owned());
	}

	/// The directories last reported populated, as reported, and how many reports there were.
	pub fn reported_dirs(&self) -> (Vec<String>, usize) {
		self.state().reported.clone()
	}

	/// How many pages of the change feed were read.
	pub fn pages_read(&self) -> usize {
		self.state().pages_read
	}

	/// Whether a listener is registered.
	pub fn has_listener(&self) -> bool {
		self.state().listener.is_some()
	}

	/// The child of the directory `dir` (a cache id) whose name is `name` in the server's case
	/// folding.
	pub fn child(&self, dir: &str, name: &str) -> Option<FfiNonRootObject> {
		let state = self.state();
		let key = collision_key(name);
		state.dirs.get(dir)?.iter().find_map(|child| {
			(child_name(child).map(collision_key).as_ref() == Some(&key)).then(|| child.clone())
		})
	}

	/// The names of the children of the directory `dir` (a cache id), sorted; a child whose name
	/// does not decrypt is left out.
	pub fn names(&self, dir: &str) -> Vec<String> {
		let state = self.state();
		let mut names: Vec<String> = state
			.dirs
			.get(dir)
			.into_iter()
			.flatten()
			.filter_map(|child| child_name(child).map(str::to_owned))
			.collect();
		names.sort_unstable();
		names
	}

	/// The bytes of the file `stable`.
	pub fn content(&self, stable: Uuid) -> Vec<u8> {
		self.state()
			.files
			.get(&stable)
			.map(|content| content.bytes.to_vec())
			.unwrap_or_default()
	}

	/// The trashed items, oldest first.
	pub fn trashed(&self) -> Vec<FfiNonRootObject> {
		self.state()
			.trash
			.iter()
			.map(|(_, item)| item.clone())
			.collect()
	}

	/// Every call answered, in order.
	pub fn ops(&self) -> Vec<DriveOp> {
		self.state().ops.clone()
	}

	/// The calls that changed the drive, in order.
	pub fn writes(&self) -> Vec<DriveOp> {
		self.ops().into_iter().filter(DriveOp::writes).collect()
	}

	/// The drive as JSON, for a harness checking it from another process: the account root, every
	/// listed item with the listing it is in (`dir`, a cache id), the trash, every call answered
	/// and how many reads were asked for. A file carries its version and the BLAKE3 hash of the
	/// bytes the drive holds for it, hex.
	pub fn dump(&self) -> Value {
		let state = self.state();
		let item = |dir: &str, child: &FfiNonRootObject| match child {
			FfiNonRootObject::File(file) => {
				let hash = Uuid::parse_str(&file.stable_uuid)
					.ok()
					.and_then(|stable| state.files.get(&stable))
					.map(|content| content.hash.to_hex().to_string());
				json!({
					"dir": dir,
					"kind": "file",
					"uuid": file.stable_uuid,
					"version": file.uuid,
					"name": child_name(child),
					"size": file.size,
					"hash": hash,
				})
			}
			FfiNonRootObject::Dir(created) => json!({
				"dir": dir,
				"kind": "dir",
				"uuid": created.uuid,
				"name": child_name(child),
			}),
		};
		let items: Vec<Value> = state
			.dirs
			.iter()
			.flat_map(|(dir, children)| children.iter().map(move |child| (dir, child)))
			.map(|(dir, child)| item(dir, child))
			.collect();
		let trash: Vec<Value> = state
			.trash
			.iter()
			.map(|(dir, child)| item(dir, child))
			.collect();
		json!({
			"root": state.root.to_string(),
			"items": items,
			"trash": trash,
			"ops": state.ops.iter().map(DriveOp::to_json).collect::<Vec<_>>(),
			"reads": state.reads.len(),
		})
	}

	/// The uuid text the directory `dir` (a cache id) is listed by in its children's parent field.
	fn parent_uuid(dir: &str) -> String {
		dir.strip_prefix("stable/").unwrap_or(dir).to_owned()
	}

	fn new_file(dir: &str, name: &str, bytes: &[u8]) -> FfiFile {
		let mut file = ffi_file(
			Uuid::new_v4(),
			Uuid::new_v4(),
			name,
			i64::try_from(bytes.len()).expect("a fake file fits an i64"),
		);
		file.parent = Self::parent_uuid(dir);
		if let Some(meta) = &mut file.meta {
			meta.hash = Some(blake3::hash(bytes).as_bytes().to_vec());
		}
		file
	}

	fn store_file(state: &mut DriveState, dir: &str, file: &FfiFile, bytes: &[u8]) {
		let stable = Uuid::parse_str(&file.stable_uuid).expect("a uuid");
		let version = Uuid::parse_str(&file.uuid).expect("a uuid");
		state
			.dirs
			.entry(dir.to_owned())
			.or_default()
			.push(FfiNonRootObject::File(file.clone()));
		let hash = blake3::hash(bytes);
		state.files.insert(
			stable,
			FakeContent {
				version,
				bytes: Arc::from(bytes),
				hash,
				declared: Some(hash.as_bytes().to_vec()),
				size: None,
				truncated_at: None,
			},
		);
		Self::stamp(state, &file.stable_uuid);
	}

	fn new_dir(dir: &str, name: &str) -> FfiDir {
		let mut created = ffi_dir(Uuid::new_v4(), name);
		created.parent = Self::parent_uuid(dir);
		created
	}

	fn store_dir(state: &mut DriveState, dir: &str, created: &FfiDir) {
		state
			.dirs
			.entry(dir.to_owned())
			.or_default()
			.push(FfiNonRootObject::Dir(created.clone()));
		state
			.dirs
			.entry(format!("stable/{}", created.uuid))
			.or_default();
		Self::stamp(state, &created.uuid);
	}

	/// Records a change of the item `uuid`.
	fn stamp(state: &mut DriveState, uuid: &str) {
		state.feed.seq += 1;
		let seq = state.feed.seq;
		state.feed.stamps.insert(uuid.to_owned(), seq);
	}

	/// Tells the listener, unless quiet, that the cache changed: called with no lock held.
	fn signal(&self) {
		let listener = {
			let state = self.state();
			if state.quiet {
				None
			} else {
				state.listener.clone()
			}
		};
		if let Some(listener) = listener {
			listener.working_set_changed();
		}
	}

	/// The listing and index of the live item `uuid` (a file's stable id, a directory's uuid).
	fn find(state: &DriveState, uuid: &str) -> Option<(String, usize)> {
		state.dirs.iter().find_map(|(dir, children)| {
			children
				.iter()
				.position(|child| child_uuid(child) == uuid)
				.map(|at| (dir.clone(), at))
		})
	}

	/// Gives the listed file `stable` a new version holding `bytes`.
	fn new_version(state: &mut DriveState, stable: Uuid, bytes: &[u8]) -> Option<Uuid> {
		let (dir, at) = Self::find(state, &stable.to_string())?;
		let version = Uuid::new_v4();
		let hash = blake3::hash(bytes);
		if let Some(FfiNonRootObject::File(file)) = state
			.dirs
			.get_mut(&dir)
			.and_then(|children| children.get_mut(at))
		{
			file.uuid = version.to_string();
			file.size = i64::try_from(bytes.len()).expect("a fake file fits an i64");
			if let Some(meta) = &mut file.meta {
				meta.hash = Some(hash.as_bytes().to_vec());
			}
		}
		let bytes: Arc<[u8]> = Arc::from(bytes);
		state
			.history
			.entry(stable)
			.or_default()
			.push(Arc::clone(&bytes));
		state.files.insert(
			stable,
			FakeContent {
				version,
				bytes,
				hash,
				declared: Some(hash.as_bytes().to_vec()),
				size: None,
				truncated_at: None,
			},
		);
		Self::stamp(state, &stable.to_string());
		Some(version)
	}

	/// The live item `uuid` as listed.
	fn listed_item(state: &DriveState, uuid: &str) -> Option<FfiNonRootObject> {
		let (dir, at) = Self::find(state, uuid)?;
		state.dirs.get(&dir)?.get(at).cloned()
	}

	/// Moves the live item `uuid` to the trash, with its original parent set.
	fn trash_listed(state: &mut DriveState, uuid: &str) -> Option<FfiNonRootObject> {
		let (dir, at) = Self::find(state, uuid)?;
		let mut item = state.dirs.get_mut(&dir)?.remove(at);
		set_original_parent(&mut item, Some(Self::parent_uuid(&dir)));
		state.trash.push((dir, item.clone()));
		Self::stamp(state, uuid);
		Some(item)
	}

	/// The directories from the listing `dir` (a cache id) up to the root, as uuid text: `dir`
	/// first, the root not included. Stops at a directory listed in no other, and at one in the
	/// trash, which is the last one given.
	fn ancestry(state: &DriveState, dir: &str) -> Vec<String> {
		let root = state.root.to_string();
		let mut chain = Vec::new();
		let mut at = dir.to_owned();
		// A listing is in at most one other, so the walk ends within as many steps as listings.
		while at != root && chain.len() <= state.dirs.len() {
			let uuid = Self::parent_uuid(&at);
			let trashed = state.trash.iter().any(|(_, item)| child_uuid(item) == uuid);
			chain.push(uuid);
			if trashed {
				break;
			}
			let Some((parent, _)) = Self::find(state, chain.last().expect("pushed above")) else {
				break;
			};
			at = parent;
		}
		chain
	}

	/// Refuses a write into the listing `dir` (a cache id) unless it is the root, or a directory
	/// the drive holds that is neither in the trash nor inside one that is. The server's answer to
	/// such a write is not verified: the fake answers as the cache does for an id it cannot resolve.
	fn writable_dir(state: &DriveState, dir: &str) -> Result<(), CacheError> {
		let root = state.root.to_string();
		let held = dir == root || state.dirs.contains_key(dir);
		let trashed = Self::ancestry(state, dir)
			.last()
			.is_some_and(|top| state.trash.iter().any(|(_, item)| child_uuid(item) == top));
		if !held || trashed {
			return Err(CacheError::DoesNotExist("no such directory".into()));
		}
		Ok(())
	}

	/// The item `uuid` as the cache holds it in `dirs` and `trash`: the live one first, as the
	/// cache resolves a stable id to its live row before a trashed one. A file carries the version,
	/// size and hash its content holds.
	fn cached_item(
		state: &DriveState,
		dirs: &HashMap<String, Vec<FfiNonRootObject>>,
		trash: &[(String, FfiNonRootObject)],
		uuid: &str,
	) -> Option<FfiObject> {
		let listed = dirs
			.values()
			.flatten()
			.chain(trash.iter().map(|(_, item)| item))
			.find(|child| child_uuid(child) == uuid)
			.cloned();
		let stable = Uuid::parse_str(uuid).ok();
		let content = stable.and_then(|stable| state.files.get(&stable).map(|c| (stable, c)));
		match (listed, content) {
			(Some(FfiNonRootObject::File(mut file)), Some((stable, content))) => {
				let overlay = Self::listed(stable, content);
				file.uuid = overlay.uuid;
				file.size = overlay.size;
				if let (Some(meta), Some(overlay)) = (&mut file.meta, overlay.meta) {
					meta.hash = overlay.hash;
				}
				Some(FfiObject::File(file))
			}
			(Some(item), _) => Some(object(item)),
			(None, Some((stable, content))) => Some(FfiObject::File(Self::listed(stable, content))),
			(None, None) => None,
		}
	}

	/// Fails the call when a switch fails it; the cache's local reads record nothing and work
	/// offline.
	fn enter_cached(&self, call: DriveCall) -> Result<(), CacheError> {
		let mut state = self.state();
		if let Some(error) = state.fail_next.remove(&call) {
			return Err(error);
		}
		match state.fail_always.get(&call) {
			Some(error) => Err(error()),
			None => Ok(()),
		}
	}

	/// Fails the call when the drive is offline or a switch fails it, recording it otherwise.
	fn enter(&self, call: Option<DriveCall>, op: DriveOp) -> Result<(), CacheError> {
		let mut state = self.state();
		if state.offline {
			return Err(offline_error());
		}
		if let Some(error) = call.and_then(|call| state.fail_next.remove(&call)) {
			return Err(error);
		}
		if let Some(error) = call.and_then(|call| state.fail_always.get(&call)) {
			return Err(error());
		}
		state.ops.push(op);
		Ok(())
	}

	/// As [`FakeDrive::enter`], then waits at the call's gate, if it has one.
	async fn enter_gated(&self, call: DriveCall, op: DriveOp) -> Result<(), CacheError> {
		self.enter(Some(call), op)?;
		self.wait_at_gate(call).await;
		Ok(())
	}

	/// Waits at the gate of `call`, if it has one.
	async fn wait_at_gate(&self, call: DriveCall) {
		let gate = self.state().call_gates.get(&call).cloned();
		if let Some(gate) = gate {
			gate.acquire()
				.await
				.expect("the gate is never closed")
				.forget();
		}
	}

	/// Reads the bytes at `os_path`.
	fn read_bytes(&self, os_path: &str) -> Result<Vec<u8>, CacheError> {
		let local = self.state().local.clone();
		// Not linked to a fake root, as in the VM harness: the file is read from disk, as the
		// cache reads it.
		match local {
			Some(local) => local.read_upload(Path::new(os_path)),
			None => std::fs::read(os_path).ok(),
		}
		.ok_or_else(|| CacheError::io("the file to upload is gone"))
	}

	/// Waits at the upload gate.
	async fn upload_gate(&self) {
		let gate = self.state().upload_gate.clone();
		if let Some(gate) = gate {
			gate.acquire()
				.await
				.expect("the gate is never closed")
				.forget();
		}
	}

	/// Reads the bytes at `os_path`, then waits at the upload gate.
	async fn upload_bytes(&self, os_path: &str) -> Result<Vec<u8>, CacheError> {
		let bytes = self.read_bytes(os_path)?;
		self.upload_gate().await;
		Ok(bytes)
	}

	/// Tells the listener of a write that went through, as the cache does when the server's event
	/// for the provider's own write comes back (measured against the real cache), and passes the
	/// result on.
	fn signalled<T>(&self, written: Result<T, CacheError>) -> Result<T, CacheError> {
		if written.is_ok() {
			let held = {
				let mut state = self.state();
				match state.echoes {
					Echoes::Told => false,
					Echoes::Held { .. } => {
						state.echoes = Echoes::Held { pending: true };
						true
					}
				}
			};
			if !held {
				self.signal();
			}
		}
		written
	}

	/// The uuid of the file among `children` other than `uuid` whose name is `name` in the server's
	/// case folding.
	fn same_named_file(children: &[FfiNonRootObject], name: &str, uuid: &str) -> Option<String> {
		let key = collision_key(name);
		children.iter().find_map(|child| match child {
			FfiNonRootObject::File(file)
				if file.stable_uuid != uuid
					&& child_name(child).map(collision_key).as_ref() == Some(&key) =>
			{
				Some(file.stable_uuid.clone())
			}
			_ => None,
		})
	}

	/// The item other than `uuid` among `children` whose name is `name` in the server's case
	/// folding, if any.
	fn same_named(
		children: &[FfiNonRootObject],
		name: &str,
		uuid: &str,
	) -> Option<FfiNonRootObject> {
		let key = collision_key(name);
		children
			.iter()
			.find(|child| {
				child_uuid(child) != uuid
					&& child_name(child).map(collision_key).as_ref() == Some(&key)
			})
			.cloned()
	}

	/// What the server answers a directory landing on a name `taken` holds (measured): refused,
	/// whether it is renamed or moved there.
	fn dir_name_taken(taken: &FfiNonRootObject) -> CacheError {
		CacheError::SDK(
			match taken {
				FfiNonRootObject::Dir(_) => {
					"A folder with this name already exists at destination. \
					 (folder_with_name_already_exists_at_destination)"
				}
				FfiNonRootObject::File(_) => {
					"A file with the same name already exists in this directory. \
					 (file_with_same_name_exists_in_parent)"
				}
			}
			.into(),
		)
	}

	/// Takes the file `uuid` out of the listing `dir` and the drive, as a file another one replaced
	/// at its name: it lives on only as an older version of that one.
	fn replace_listed(state: &mut DriveState, dir: &str, uuid: &str) {
		if let Some(children) = state.dirs.get_mut(dir) {
			children.retain(|child| child_uuid(child) != uuid);
		}
		if let Ok(stable) = Uuid::parse_str(uuid) {
			state.files.remove(&stable);
		}
		state.feed.stamps.remove(uuid);
		state.feed.seq += 1;
		let seq = state.feed.seq;
		state.feed.tombstones.push((seq, uuid.to_owned()));
	}

	/// The body of [`Drive::upload_version`], the edit marked.
	async fn upload_marked(
		&self,
		file: FfiId,
		stable: Uuid,
		bytes: &[u8],
		expect: [u8; 32],
	) -> Result<CheckedUpload, CacheError> {
		if *blake3::hash(bytes).as_bytes() != expect {
			return Ok(CheckedUpload::Changed);
		}
		self.enter(
			Some(DriveCall::UploadVersion),
			DriveOp::UploadVersion(file.0.clone()),
		)?;
		self.upload_gate().await;
		let mut state = self.state();
		// As the cache's: the server's content already only when the file declares the hash of
		// what was hashed; a file declaring none gets a version, however equal its bytes.
		let unchanged = state.versions == Versions::Ignored
			|| matches!(
				Self::listed_item(&state, &stable.to_string()),
				Some(FfiNonRootObject::File(head))
					if head.meta.as_ref().and_then(|meta| meta.hash.as_deref())
						== Some(expect.as_slice())
			);
		if !unchanged {
			Self::new_version(&mut state, stable, bytes)
				.ok_or_else(|| CacheError::DoesNotExist("no such file".into()))?;
		}
		match Self::listed_item(&state, &stable.to_string()) {
			Some(FfiNonRootObject::File(file)) => Ok(CheckedUpload::Uploaded(Box::new(file))),
			_ => Err(CacheError::DoesNotExist("no such file".into())),
		}
	}

	/// The listed file the content of `stable` is.
	fn listed(stable: Uuid, content: &FakeContent) -> FfiFile {
		let size = content
			.size
			.unwrap_or(u64::try_from(content.bytes.len()).expect("a fake file fits a u64"));
		let mut file = ffi_file(
			stable,
			content.version,
			"file",
			i64::try_from(size).expect("a fake file fits an i64"),
		);
		if let Some(meta) = &mut file.meta {
			meta.hash.clone_from(&content.declared);
		}
		file
	}
}

/// The uuid a `stable/<uuid>` id names.
/// The spec the fake decodes thumbnails under: a `max_side` square and a 16 MiB budget.
fn thumbnail_spec(max_side: u32) -> ThumbSpec {
	ThumbSpec::new(max_side, max_side, 16 * 1024 * 1024)
}

fn stable_uuid(id: &FfiId) -> Option<Uuid> {
	Uuid::parse_str(id.0.strip_prefix("stable/")?).ok()
}

fn is_aborted(abort: Option<&Arc<FfiAbortSignal>>) -> bool {
	abort.is_some_and(|signal| signal.is_aborted())
}

/// Runs `fut`, dropping it as soon as `abort` fires, biased toward `fut` as the cache's
/// `with_abort` is: `None` when it was dropped.
async fn unless_aborted<T>(
	abort: Option<&Arc<FfiAbortSignal>>,
	fut: impl Future<Output = T>,
) -> Option<T> {
	let Some(signal) = abort else {
		return Some(fut.await);
	};
	select! {
		biased;
		out = fut => Some(out),
		() = signal.aborted() => None,
	}
}

impl sealed::Sealed for FakeDrive {}

impl Drive for FakeDrive {
	async fn root_uuid(&self) -> Result<String, CacheError> {
		let mut state = self.state();
		if let Some(error) = state.fail_root_always {
			return Err(error());
		}
		match state.fail_root.take() {
			Some(error) => Err(error),
			None => Ok(state.root.to_string()),
		}
	}

	async fn list_children(&self, dir: FfiId) -> Result<Option<QueryChildrenResponse>, CacheError> {
		self.enter_gated(DriveCall::List, DriveOp::List(dir.0.clone()))
			.await?;
		let mut state = self.state();
		if let Some(error) = state.fail_list.take() {
			return Err(error);
		}
		state.unlisted.remove(&dir.0);
		let parent = ffi_dir(
			stable_uuid(&dir)
				.or_else(|| Uuid::parse_str(&dir.0).ok())
				.unwrap_or_default(),
			"parent",
		);
		Ok(state
			.dirs
			.get(&dir.0)
			.map(|children| QueryChildrenResponse {
				objects: children.clone(),
				parent,
			}))
	}

	/// The live item, a trashed one, or a file only [`FakeDrive::add_file`] holds, as the cache
	/// holds them (as they were, while [`FakeDrive::stale_cache`] is on); a file with the version,
	/// size and declared hash of its content.
	async fn query_item(&self, id: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		self.enter_cached(DriveCall::Query)?;
		let mut state = self.state();
		if let Some(error) = state.fail_query.take() {
			return Err(error);
		}
		let Some(uuid) = stable_uuid(id).map(|uuid| uuid.to_string()) else {
			return Ok(None);
		};
		let (dirs, trash) = match &state.stale {
			Some(snapshot) => (snapshot.dirs.clone(), snapshot.trash.clone()),
			None => (state.dirs.clone(), state.trash.clone()),
		};
		// As the cache answers a `stable/` id it holds no row for, taken as the drive takes it.
		held_item(
			Self::cached_item(&state, &dirs, &trash, &uuid)
				.map(Some)
				.ok_or_else(|| {
					CacheError::DoesNotExist(format!("No item for stable id: {uuid}").into())
				}),
		)
	}

	async fn cached_children(
		&self,
		dir: &FfiId,
	) -> Result<Option<QueryChildrenResponse>, CacheError> {
		self.enter_cached(DriveCall::CachedChildren)?;
		self.wait_at_gate(DriveCall::CachedChildren).await;
		let state = self.state();
		let dirs = match &state.stale {
			Some(snapshot) => &snapshot.dirs,
			None => &state.dirs,
		};
		let parent = ffi_dir(
			stable_uuid(dir)
				.or_else(|| Uuid::parse_str(&dir.0).ok())
				.unwrap_or_default(),
			"parent",
		);
		let unlisted = state.unlisted.contains(&dir.0);
		Ok(dirs.get(&dir.0).map(|children| QueryChildrenResponse {
			objects: if unlisted {
				Vec::new()
			} else {
				children.clone()
			},
			parent,
		}))
	}

	/// As the cache's paged feed: every item changed after the anchor at its latest state, and,
	/// with an anchor, every item removed after it, pooled in change order; the anchor reached is
	/// the last change handed over, or the latest change once nothing more follows.
	async fn changes(&self, anchor: Option<Vec<u8>>, limit: u32) -> Result<FfiChanges, CacheError> {
		self.enter_cached(DriveCall::Changes)?;
		let mut state = self.state();
		state.pages_read += 1;
		let after = match anchor.as_deref() {
			None => None,
			Some(anchor) => match decode_anchor(anchor) {
				Some((instance, seq)) if instance == state.feed.instance => Some(seq),
				_ => {
					return Err(CacheError::SyncAnchorExpired(
						"the anchor is from another incarnation".into(),
					));
				}
			},
		};
		let from = after.unwrap_or(0);
		let mut pooled: Vec<(i64, Option<&str>)> = state
			.feed
			.stamps
			.iter()
			.filter(|(_, seq)| **seq > from)
			.map(|(uuid, seq)| (*seq, Some(uuid.as_str())))
			.collect();
		if after.is_some() {
			pooled.extend(
				state
					.feed
					.tombstones
					.iter()
					.filter(|(seq, _)| *seq > from)
					.map(|(seq, _)| (*seq, None)),
			);
		}
		pooled.sort_unstable_by_key(|(seq, _)| *seq);
		let limit = usize::try_from(limit).unwrap_or(usize::MAX);
		let taken = if limit == 0 {
			pooled.len()
		} else {
			limit.min(pooled.len())
		};
		let more = taken < pooled.len();
		let mut changes = FfiChanges {
			updated: Vec::new(),
			deleted_ids: Vec::new(),
			anchor: Vec::new(),
			more,
		};
		let mut reached = state.feed.seq;
		for (seq, uuid) in pooled.into_iter().take(taken) {
			reached = seq;
			match uuid {
				Some(uuid) => {
					if let Some(item) = Self::cached_item(&state, &state.dirs, &state.trash, uuid) {
						changes.updated.push(item);
					}
				}
				None => {
					if let Some((_, uuid)) = state.feed.tombstones.iter().find(|(at, _)| *at == seq)
					{
						changes.deleted_ids.push(format!("stable/{uuid}"));
					}
				}
			}
		}
		if !more {
			reached = state.feed.seq;
		}
		changes.anchor = encode_anchor(state.feed.instance, reached);
		Ok(changes)
	}

	async fn change_anchor(&self) -> Result<Vec<u8>, CacheError> {
		self.enter_cached(DriveCall::ChangeAnchor)?;
		let state = self.state();
		Ok(encode_anchor(state.feed.instance, state.feed.seq))
	}

	async fn watch_changes(&self, listener: Option<Arc<dyn WorkingSetUpdateListener>>) {
		self.state().listener = listener;
	}

	async fn set_populated_dirs(&self, ids: Vec<String>) -> Result<(), CacheError> {
		let mut state = self.state();
		state.reported.0 = ids;
		state.reported.1 += 1;
		Ok(())
	}

	/// As the cache's read: refuses an aborted call, resolves `stable/<uuid>`, clamps the range,
	/// calls `on_start`, waits at the gate (forever for a held file), then hands over pieces
	/// ending on chunk boundaries or at the range's end. An abort after `on_start` drops the wait
	/// at the gate and the delivery at once, a pending `on_bytes` included, as the cache's read
	/// drops its download. A whole-file read of a file declaring a hash is hashed as it goes, and
	/// a mismatch fails the read in place of the piece reaching the end, as the SDK's reader does.
	async fn read_range<S: RangeSink>(
		&self,
		id: &FfiId,
		range: ByteRange,
		sink: &mut S,
		abort: Option<&Arc<FfiAbortSignal>>,
	) -> Result<RangeRead, RangeReadError<S::Error>> {
		let aborted = || RangeReadError::Cache(CacheError::Aborted("aborted".into()));
		if is_aborted(abort) {
			return Err(aborted());
		}
		let (uuid, bytes, declared, size, available, gate, held, delay) = {
			let mut state = self.state();
			state.reads.push((range.offset(), range.end()));
			// The cache keeps no file's bytes: a read downloads them, so it fails offline too.
			if state.offline {
				return Err(offline_error().into());
			}
			if let Some(error) = state.fail_read.take() {
				return Err(error.into());
			}
			let stable = stable_uuid(id);
			let held = stable.is_some_and(|stable| state.held.contains(&stable));
			let delay = stable.and_then(|stable| state.delayed.get(&stable).copied());
			let content = stable
				.and_then(|stable| state.files.get(&stable))
				.ok_or_else(|| CacheError::DoesNotExist("no such file".into()))?;
			let length = u64::try_from(content.bytes.len()).expect("a fake file fits a u64");
			let size = content.size.unwrap_or(length);
			(
				content.version,
				Arc::clone(&content.bytes),
				// Cloned out of the state, whose lock is not held across the read.
				content.declared.clone(),
				size,
				content.truncated_at.unwrap_or(size).min(length),
				state.gate.clone(),
				held,
				delay,
			)
		};
		let range = range.clamp_to(size).ok_or(RangeReadError::PastEnd {
			uuid,
			offset: range.offset(),
			size,
		})?;
		let integrity = if declared.is_some() && range.offset() == 0 && range.end() == size {
			RangeIntegrity::HashChecked
		} else {
			RangeIntegrity::Unchecked
		};
		let read = RangeRead {
			uuid,
			file_size: size,
			range,
			integrity,
		};
		sink.on_start(&read).map_err(RangeReadError::Sink)?;
		let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
		let _active = ActiveRead(&self.active);
		self.max_active.fetch_max(active, Ordering::SeqCst);
		self.started.send_modify(|started| *started += 1);
		let delivery = async {
			if let Some(gate) = gate {
				gate.acquire()
					.await
					.expect("the gate is never closed")
					.forget();
			}
			if held {
				future::pending::<()>().await;
			}
			if let Some(delay) = delay {
				let (done, waited) = oneshot::channel();
				thread::spawn(move || {
					thread::sleep(delay);
					// The read may have been dropped meanwhile; nothing then waits.
					let _ = done.send(());
				});
				// The sender is dropped only once sent.
				let _ = waited.await;
			}
			let mut hasher = blake3::Hasher::new();
			let mut offset = range.offset();
			while offset < range.end() {
				if offset >= available {
					return Err(RangeReadError::Truncated {
						uuid,
						reached: offset,
						end: range.end(),
					});
				}
				let end = (offset / CHUNK_SIZE_U64 + 1)
					.saturating_mul(CHUNK_SIZE_U64)
					.min(range.end())
					.min(available);
				let piece = bytes
					.get(
						usize::try_from(offset).expect("fits memory")
							..usize::try_from(end).expect("fits memory"),
					)
					.expect("inside the bytes");
				if integrity == RangeIntegrity::HashChecked {
					hasher.update(piece);
					if end == size
						&& Some(hasher.finalize().as_bytes().as_slice()) != declared.as_deref()
					{
						return Err(RangeReadError::Cache(CacheError::ContentHashMismatch(
							"the file does not hash to its declared hash".into(),
						)));
					}
				}
				sink.on_bytes(offset, piece)
					.await
					.map_err(RangeReadError::Sink)?;
				offset = end;
			}
			Ok(())
		};
		unless_aborted(abort, delivery)
			.await
			.unwrap_or_else(|| Err(aborted()))?;
		Ok(read)
	}

	/// The live item, a trashed one with its original parent, a file only [`FakeDrive::add_file`]
	/// holds, or the root.
	/// Decodes the file's bytes as the cache would, under a fixed 16 MiB budget; the name gate is
	/// left to the local variant, as the bytes' format decides here.
	/// A thumbnail made is kept by the file's version, as the cache keeps it; one the gate held
	/// fails as unreachable when the drive went offline meanwhile, as a download cut off does. A
	/// file held at another version than `expected` is refused as the cache refuses it.
	async fn fitted_thumbnail(
		&self,
		id: &FfiId,
		expected: Uuid,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> Result<FittedOutcome, CacheError> {
		let _entered = Entered::new(&self.thumbnails);
		self.enter_gated(DriveCall::Thumbnail, DriveOp::Thumbnail(id.0.clone()))
			.await?;
		if cancel.load(Ordering::Relaxed) {
			return Err(CacheError::Aborted("the thumbnail was cancelled".into()));
		}
		if self.state().offline {
			return Err(offline_error());
		}
		let (version, bytes) = stable_uuid(id)
			.and_then(|stable| {
				self.state()
					.files
					.get(&stable)
					.map(|content| (content.version, content.bytes.to_vec()))
			})
			.ok_or_else(|| CacheError::DoesNotExist("the drive holds no such file".into()))?;
		if version != expected {
			return Err(CacheError::DoesNotExist(
				"the drive holds another version of the file".into(),
			));
		}
		let outcome = encode_fitted(Box::new(MemSource(bytes)), &thumbnail_spec(max_side))
			.map_err(CacheError::from)?;
		if let FittedOutcome::Thumbnail(thumbnail) = &outcome {
			let mut state = self.state();
			if state.fitted_kept == Fitted::Kept {
				state.fitted.insert((version, max_side), thumbnail.clone());
			}
		}
		Ok(outcome)
	}

	/// What [`FakeDrive::fitted_thumbnail`] kept, offline too: only a switch fails it.
	async fn cached_fitted_thumbnail(
		&self,
		version: Uuid,
		max_side: u32,
	) -> Result<Option<FittedThumbnail>, CacheError> {
		self.enter_cached(DriveCall::CachedThumbnail)?;
		let mut state = self.state();
		state.cache_lookups += 1;
		Ok(state.fitted.get(&(version, max_side)).cloned())
	}

	/// As the cache does, offline too: only a switch fails it.
	async fn fitted_thumbnail_of_local(
		&self,
		source: Box<dyn ByteSource>,
		name: &str,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> Result<FittedOutcome, CacheError> {
		self.enter_cached(DriveCall::Thumbnail)?;
		self.state()
			.ops
			.push(DriveOp::LocalThumbnail(name.to_owned()));
		if cancel.load(Ordering::Relaxed) {
			return Err(CacheError::Aborted("the thumbnail was cancelled".into()));
		}
		if !might_be_thumbnailable(Some(name), None) {
			return Ok(FittedOutcome::Unsupported);
		}
		encode_fitted(source, &thumbnail_spec(max_side)).map_err(CacheError::from)
	}

	async fn refresh_item(&self, id: FfiId) -> Result<Option<FfiObject>, CacheError> {
		self.enter_gated(DriveCall::Refresh, DriveOp::Refresh(id.0.clone()))
			.await?;
		let state = self.state();
		if id.0 == state.root.to_string() {
			return Ok(Some(FfiObject::Root(FfiRoot {
				uuid: id.0,
				storage_used: 0,
				max_storage: 0,
				last_updated: 0,
				last_listed: 0,
			})));
		}
		let Some(uuid) = id.0.strip_prefix("stable/") else {
			return Ok(None);
		};
		if let Some(item) = Self::listed_item(&state, uuid) {
			return Ok(Some(object(item)));
		}
		if let Some((_, item)) = state
			.trash
			.iter()
			.find(|(_, item)| child_uuid(item) == uuid)
		{
			return Ok(Some(object(item.clone())));
		}
		Ok(Uuid::parse_str(uuid).ok().and_then(|stable| {
			state
				.files
				.get(&stable)
				.map(|content| FfiObject::File(Self::listed(stable, content)))
		}))
	}

	async fn upload_file(
		&self,
		os_path: String,
		parent: FfiId,
		info: UploadFileInfo,
	) -> Result<FfiFile, CacheError> {
		let written = async {
			self.enter(
				Some(DriveCall::Upload),
				DriveOp::Upload {
					parent: parent.0.clone(),
					name: info.name.clone(),
				},
			)?;
			let bytes = self.upload_bytes(&os_path).await?;
			let mut state = self.state();
			Self::writable_dir(&state, &parent.0)?;
			let key = collision_key(&info.name);
			let taken = state.dirs.get(&parent.0).and_then(|children| {
				children
					.iter()
					.find(|child| child_name(child).map(collision_key).as_ref() == Some(&key))
					.cloned()
			});
			match taken {
				// The server keeps the file and makes the upload its new version.
				Some(FfiNonRootObject::File(file)) => {
					let stable = Uuid::parse_str(&file.stable_uuid).expect("a uuid");
					Self::new_version(&mut state, stable, &bytes);
					match Self::listed_item(&state, &file.stable_uuid) {
						Some(FfiNonRootObject::File(file)) => Ok(file),
						_ => Err(CacheError::remote("the file vanished")),
					}
				}
				Some(FfiNonRootObject::Dir(_)) => {
					Err(CacheError::remote("a directory has the name"))
				}
				None => {
					let mut file = Self::new_file(&parent.0, &info.name, &bytes);
					if let Some(meta) = &mut file.meta {
						meta.created = info.creation;
						if let Some(modified) = info.modification {
							meta.modified = modified;
						}
					}
					Self::store_file(&mut state, &parent.0, &file, &bytes);
					Ok(file)
				}
			}
		}
		.await;
		self.signalled(written)
	}

	/// As the cache's: the bytes are copied into the file's slot and the edit marked before the
	/// upload; a copy that does not hash to `expect` is refused, and one the head declares the hash
	/// of is the head; the marker is cleared before the call returns, and stays only when the call
	/// is dropped at the upload gate.
	async fn upload_version(
		&self,
		file: FfiId,
		os_path: String,
		expect: [u8; 32],
	) -> Result<CheckedUpload, CacheError> {
		let written = async {
			let stable = stable_uuid(&file)
				.ok_or_else(|| CacheError::DoesNotExist("not a file id".into()))?;
			let bytes = self.read_bytes(&os_path)?;
			self.state()
				.pending
				.insert(stable, Arc::from(bytes.as_slice()));
			let uploaded = self.upload_marked(file, stable, &bytes, expect).await;
			self.state().pending.remove(&stable);
			uploaded
		}
		.await;
		self.signalled(written)
	}

	async fn create_dir(
		&self,
		parent: FfiId,
		name: String,
		created: Option<i64>,
	) -> Result<FfiDir, CacheError> {
		let written = async {
			self.enter_gated(
				DriveCall::CreateDir,
				DriveOp::CreateDir {
					parent: parent.0.clone(),
					name: name.clone(),
				},
			)
			.await?;
			let mut state = self.state();
			Self::writable_dir(&state, &parent.0)?;
			let key = collision_key(&name);
			let taken = state.dirs.get(&parent.0).and_then(|children| {
				children
					.iter()
					.find(|child| child_name(child).map(collision_key).as_ref() == Some(&key))
					.cloned()
			});
			match taken {
				// The server dedups a directory on its name hash and returns the one it holds.
				Some(FfiNonRootObject::Dir(dir)) => Ok(dir),
				// What the server answers here is not verified; the fake refuses it.
				Some(FfiNonRootObject::File(_)) => Err(CacheError::remote("a file has the name")),
				None => {
					let mut dir = Self::new_dir(&parent.0, &name);
					if let Some(meta) = &mut dir.meta {
						meta.created = created;
					}
					Self::store_dir(&mut state, &parent.0, &dir);
					Ok(dir)
				}
			}
		}
		.await;
		self.signalled(written)
	}

	async fn trash(&self, id: FfiId) -> Result<FfiObject, CacheError> {
		let written = async {
			self.enter(Some(DriveCall::Trash), DriveOp::Trash(id.0.clone()))?;
			let mut state = self.state();
			let uuid = id.0.strip_prefix("stable/").unwrap_or(&id.0).to_owned();
			if let Some((_, item)) = state
				.trash
				.iter()
				.find(|(_, item)| child_uuid(item) == uuid)
			{
				return Ok(object(item.clone()));
			}
			Self::trash_listed(&mut state, &uuid)
				.map(object)
				.ok_or_else(|| CacheError::DoesNotExist("gone".into()))
		}
		.await;
		self.signalled(written)
	}

	async fn rename(&self, id: FfiId, name: String) -> Result<Option<FfiObject>, CacheError> {
		let written = async {
			self.enter_gated(
				DriveCall::Rename,
				DriveOp::Rename {
					id: id.0.clone(),
					name: name.clone(),
				},
			)
			.await?;
			let mut state = self.state();
			let uuid = id.0.strip_prefix("stable/").unwrap_or(&id.0).to_owned();
			let (dir, at) =
				Self::find(&state, &uuid).ok_or_else(|| CacheError::DoesNotExist("gone".into()))?;
			let children = state.dirs.get(&dir).expect("found above");
			if matches!(children.get(at), Some(FfiNonRootObject::File(_)))
				&& Self::same_named_file(children, &name, &uuid).is_some()
			{
				// Measured: the server refuses a file's rename onto a name another file holds,
				// in its case folding.
				return Err(CacheError::SDK(
					"File with the same name already exists at destination. \
					 (file_with_name_already_exists_at_destination)"
						.into(),
				));
			}
			if matches!(children.get(at), Some(FfiNonRootObject::Dir(_)))
				&& let Some(taken) = Self::same_named(children, &name, &uuid)
			{
				return Err(Self::dir_name_taken(&taken));
			}
			let item = state
				.dirs
				.get_mut(&dir)
				.and_then(|children| children.get_mut(at))
				.expect("found above");
			if child_name(item) == Some(name.as_str()) {
				return Ok(None);
			}
			set_name(item, &name);
			let renamed = object(item.clone());
			Self::stamp(&mut state, &uuid);
			Ok(Some(renamed))
		}
		.await;
		self.signalled(written)
	}

	async fn move_to(&self, id: FfiId, parent: FfiId) -> Result<FfiObject, CacheError> {
		let written = async {
			self.enter_gated(
				DriveCall::Move,
				DriveOp::Move {
					id: id.0.clone(),
					parent: parent.0.clone(),
				},
			)
			.await?;
			let mut state = self.state();
			let uuid = id.0.strip_prefix("stable/").unwrap_or(&id.0).to_owned();
			let (dir, at) =
				Self::find(&state, &uuid).ok_or_else(|| CacheError::DoesNotExist("gone".into()))?;
			Self::writable_dir(&state, &parent.0)?;
			if Self::ancestry(&state, &parent.0).contains(&uuid) {
				// It would cut the directory off from the root. The server's answer is not verified.
				return Err(CacheError::remote("a directory into its own subtree"));
			}
			if let Some(item @ FfiNonRootObject::Dir(_)) =
				state.dirs.get(&dir).and_then(|children| children.get(at))
				&& let Some(name) = child_name(item)
				&& let Some(taken) = state
					.dirs
					.get(&parent.0)
					.and_then(|children| Self::same_named(children, name, &uuid))
			{
				return Err(Self::dir_name_taken(&taken));
			}
			let mut item = state.dirs.get_mut(&dir).expect("found above").remove(at);
			if let (FfiNonRootObject::File(_), Some(name)) = (&item, child_name(&item))
				&& let Some(taken) = state
					.dirs
					.get(&parent.0)
					.and_then(|children| Self::same_named_file(children, name, &uuid))
			{
				// Measured: the server moves a file onto a name another file holds, which
				// becomes an older version of the moved file; its own lineage is no longer
				// current.
				Self::replace_listed(&mut state, &parent.0, &taken);
			}
			set_parent(&mut item, Self::parent_uuid(&parent.0));
			state.dirs.entry(parent.0).or_default().push(item.clone());
			Self::stamp(&mut state, &uuid);
			Ok(object(item))
		}
		.await;
		self.signalled(written)
	}

	/// The cache's own, with no server round trip: answers offline too.
	async fn pending_uploads(&self) -> Result<Vec<FfiId>, CacheError> {
		let state = self.state();
		Ok(state
			.pending
			.keys()
			.map(|stable| FfiId(format!("stable/{stable}")))
			.collect())
	}

	/// As the cache's: nothing to copy for a file with no marker, or whose marked bytes are the
	/// head's (the marker goes); otherwise the bytes are written as a new file at `dest`, a path
	/// the linked root handed out, and the marker stays. The cache's own, so it works offline.
	async fn copy_pending_upload(
		&self,
		file: FfiId,
		dest: PathBuf,
	) -> Result<PendingCopy, CacheError> {
		self.state().ops.push(DriveOp::CopyPending(file.0.clone()));
		let stable =
			stable_uuid(&file).ok_or_else(|| CacheError::DoesNotExist("not a file id".into()))?;
		let (bytes, local) = {
			let mut state = self.state();
			let Some(bytes) = state.pending.get(&stable).cloned() else {
				return Ok(PendingCopy::Nothing);
			};
			if state
				.files
				.get(&stable)
				.is_some_and(|content| content.bytes == bytes)
			{
				state.pending.remove(&stable);
				return Ok(PendingCopy::Nothing);
			}
			(bytes, state.local.clone())
		};
		let written = match local {
			Some(local) => local.write_new_upload(&dest, &bytes),
			None => std::fs::OpenOptions::new()
				.write(true)
				.create_new(true)
				.open(&dest)
				.and_then(|mut file| std::io::Write::write_all(&mut file, &bytes))
				.is_ok(),
		};
		if written {
			self.wait_at_gate(DriveCall::CopyPending).await;
			Ok(PendingCopy::Copied)
		} else {
			Err(CacheError::io("the copy's destination exists"))
		}
	}

	/// The cache's own, with no server round trip: works offline too.
	async fn drop_pending_upload(&self, file: FfiId) -> Result<(), CacheError> {
		let mut state = self.state();
		if let Some(stable) = stable_uuid(&file) {
			state.pending.remove(&stable);
		}
		Ok(())
	}

	async fn pending_upload_count(&self) -> Result<u32, CacheError> {
		let state = self.state();
		if state.offline {
			return Err(offline_error());
		}
		Ok(u32::try_from(state.pending.len()).unwrap_or(u32::MAX))
	}

	async fn stop(&self) {
		let mut state = self.state();
		state.stopped = true;
		state.listener = None;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ROOT: Uuid = Uuid::from_u128(0x100);

	fn ids(changes: &FfiChanges) -> Vec<String> {
		changes
			.updated
			.iter()
			.map(|item| match item {
				FfiObject::File(file) => file.stable_uuid.clone(),
				FfiObject::Dir(dir) => dir.uuid.clone(),
				FfiObject::Root(root) => root.uuid.clone(),
			})
			.collect()
	}

	#[tokio::test]
	async fn the_feed_pages_changes_in_order_and_resumes_from_each_anchor() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		let start = drive.change_anchor().await.unwrap();
		let files: Vec<FfiFile> = (0..5)
			.map(|n| drive.add_remote_file(&root, &format!("{n}.txt"), b"x"))
			.collect();
		let first = drive.changes(Some(start), 2).await.unwrap();
		assert!(first.more);
		assert_eq!(
			ids(&first),
			vec![files[0].stable_uuid.clone(), files[1].stable_uuid.clone()]
		);
		let second = drive.changes(Some(first.anchor), 0).await.unwrap();
		assert!(!second.more);
		assert_eq!(ids(&second).len(), 3);
		let empty = drive.changes(Some(second.anchor), 0).await.unwrap();
		assert!(empty.updated.is_empty() && empty.deleted_ids.is_empty());
	}

	#[tokio::test]
	async fn a_removal_is_a_tombstone_only_after_an_anchor() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		let dir = drive.add_remote_dir(&root, "d");
		let file = drive.add_remote_file(&format!("stable/{}", dir.uuid), "a.txt", b"a");
		let anchor = drive.change_anchor().await.unwrap();
		drive.remote_delete(Uuid::parse_str(&dir.uuid).unwrap());
		let changes = drive.changes(Some(anchor), 0).await.unwrap();
		assert_eq!(
			changes.deleted_ids,
			vec![
				format!("stable/{}", dir.uuid),
				format!("stable/{}", file.stable_uuid)
			]
		);
		assert!(drive.changes(None, 0).await.unwrap().deleted_ids.is_empty());
	}

	#[tokio::test]
	async fn an_anchor_of_another_incarnation_has_expired() {
		let drive = FakeDrive::new(ROOT);
		let anchor = drive.change_anchor().await.unwrap();
		drive.wipe_feed();
		assert!(matches!(
			drive.changes(Some(anchor), 0).await,
			Err(CacheError::SyncAnchorExpired(_))
		));
	}

	#[tokio::test]
	async fn a_trashed_item_comes_back_with_its_original_parent_and_a_stale_cache_hides_it() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		let file = drive.add_remote_file(&root, "a.txt", b"a");
		let stable = Uuid::parse_str(&file.stable_uuid).unwrap();
		let id = FfiId(format!("stable/{stable}"));
		drive.stale_cache(true);
		drive.remote_trash(stable);
		assert!(matches!(
			drive.query_item(&id).await.unwrap(),
			Some(FfiObject::File(file)) if file.original_parent.is_none()
		));
		drive.stale_cache(false);
		assert!(matches!(
			drive.query_item(&id).await.unwrap(),
			Some(FfiObject::File(file)) if file.original_parent.as_deref() == Some(root.as_str())
		));
		drive.remote_restore(stable);
		assert!(matches!(
			drive.query_item(&id).await.unwrap(),
			Some(FfiObject::File(file)) if file.original_parent.is_none() && file.parent == root
		));
	}

	#[tokio::test]
	async fn the_pending_count_is_the_edits_marked() {
		let drive = FakeDrive::new(ROOT);
		let file = drive.add_remote_file(&ROOT.to_string(), "a.txt", b"a");
		let stable = Uuid::parse_str(&file.stable_uuid).unwrap();
		assert_eq!(drive.pending_upload_count().await.unwrap(), 0);
		drive.state().pending.insert(stable, Arc::from(&b"b"[..]));
		assert_eq!(drive.pending_upload_count().await.unwrap(), 1);
	}

	#[tokio::test]
	async fn a_rename_onto_a_name_another_file_holds_is_refused_as_the_server_does() {
		let drive = FakeDrive::new(ROOT);
		let dir = ROOT.to_string();
		drive.list(&dir, Vec::new());
		drive.add_remote_file(&dir, "a.txt", b"a");
		let b = drive.add_remote_file(&dir, "b.txt", b"b");
		let id = FfiId(format!("stable/{}", b.stable_uuid));
		assert!(matches!(
			drive.rename(id.clone(), "A.TXT".to_owned()).await,
			Err(CacheError::SDK(_))
		));
		assert!(drive.child(&dir, "b.txt").is_some(), "nothing renamed");
		assert!(
			drive
				.rename(id, "c.txt".to_owned())
				.await
				.unwrap()
				.is_some()
		);
	}

	#[tokio::test]
	async fn a_move_onto_a_name_another_file_holds_takes_its_place_as_the_server_does() {
		let drive = FakeDrive::new(ROOT);
		let (dir, sub) = (
			ROOT.to_string(),
			"stable/00000000-0000-0000-0000-000000000200",
		);
		drive.list(&dir, Vec::new());
		drive.list(sub, Vec::new());
		let there = drive.add_remote_file(&dir, "a.txt", b"there");
		let moved = drive.add_remote_file(sub, "a.txt", b"moved");
		drive
			.move_to(
				FfiId(format!("stable/{}", moved.stable_uuid)),
				FfiId(dir.clone()),
			)
			.await
			.unwrap();
		let listed = drive.child(&dir, "a.txt").unwrap();
		assert_eq!(child_uuid(&listed), moved.stable_uuid);
		let stable = Uuid::parse_str(&there.stable_uuid).unwrap();
		assert!(
			drive
				.refresh_item(FfiId(format!("stable/{stable}")))
				.await
				.unwrap()
				.is_none(),
			"the file moved over is no longer current"
		);
	}

	#[tokio::test]
	async fn a_directory_created_over_a_taken_name_is_the_directory_there_or_refused() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		drive.list(&root, Vec::new());
		let dir = drive.add_remote_dir(&root, "d");
		drive.add_remote_file(&root, "a.txt", b"a");
		let created = drive
			.create_dir(FfiId(root.clone()), "D".to_owned(), None)
			.await
			.unwrap();
		assert_eq!(created.uuid, dir.uuid);
		assert!(matches!(
			drive
				.create_dir(FfiId(root.clone()), "A.TXT".to_owned(), None)
				.await,
			Err(CacheError::Remote(_))
		));
		assert_eq!(drive.state().dirs[&root].len(), 2, "nothing created");
	}

	#[tokio::test]
	async fn nothing_is_written_into_a_trashed_directory_or_one_inside_it() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		drive.list(&root, Vec::new());
		let dir = drive.add_remote_dir(&root, "d");
		let dir_id = format!("stable/{}", dir.uuid);
		let inner = drive.add_remote_dir(&dir_id, "e");
		let inner_id = format!("stable/{}", inner.uuid);
		let file = drive.add_remote_file(&root, "a.txt", b"a");
		drive.remote_trash(Uuid::parse_str(&dir.uuid).unwrap());
		let upload = tempfile::NamedTempFile::new().unwrap();
		std::fs::write(upload.path(), b"b").unwrap();
		for target in [&dir_id, &inner_id] {
			assert!(matches!(
				drive
					.create_dir(FfiId(target.clone()), "f".to_owned(), None)
					.await,
				Err(CacheError::DoesNotExist(_))
			));
			assert!(matches!(
				drive
					.upload_file(
						upload.path().to_str().unwrap().to_owned(),
						FfiId(target.clone()),
						UploadFileInfo {
							name: "b.txt".to_owned(),
							creation: None,
							modification: None,
							mime: None,
						},
					)
					.await,
				Err(CacheError::DoesNotExist(_))
			));
			assert!(matches!(
				drive
					.move_to(
						FfiId(format!("stable/{}", file.stable_uuid)),
						FfiId(target.clone())
					)
					.await,
				Err(CacheError::DoesNotExist(_))
			));
		}
		assert!(drive.child(&root, "a.txt").is_some(), "nothing moved");
	}

	#[tokio::test]
	async fn nothing_is_written_into_a_directory_the_drive_does_not_hold() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		drive.list(&root, Vec::new());
		let file = drive.add_remote_file(&root, "a.txt", b"a");
		let missing = FfiId("stable/00000000-0000-0000-0000-000000000300".to_owned());
		assert!(matches!(
			drive
				.create_dir(missing.clone(), "f".to_owned(), None)
				.await,
			Err(CacheError::DoesNotExist(_))
		));
		assert!(matches!(
			drive
				.move_to(FfiId(format!("stable/{}", file.stable_uuid)), missing)
				.await,
			Err(CacheError::DoesNotExist(_))
		));
		assert!(drive.child(&root, "a.txt").is_some(), "nothing moved");
	}

	#[tokio::test]
	async fn a_directory_is_not_moved_into_itself_or_its_own_subtree() {
		let drive = FakeDrive::new(ROOT);
		let root = ROOT.to_string();
		drive.list(&root, Vec::new());
		let dir = drive.add_remote_dir(&root, "d");
		let dir_id = format!("stable/{}", dir.uuid);
		let inner = drive.add_remote_dir(&dir_id, "e");
		let inner_id = format!("stable/{}", inner.uuid);
		for target in [&dir_id, &inner_id] {
			assert!(
				drive
					.move_to(FfiId(dir_id.clone()), FfiId(target.clone()))
					.await
					.is_err()
			);
		}
		assert!(drive.child(&root, "d").is_some(), "nothing moved");
		assert!(drive.child(&dir_id, "e").is_some(), "nothing moved");
	}

	#[tokio::test]
	async fn a_stopped_drive_says_so_and_tells_no_listener() {
		let drive = FakeDrive::new(ROOT);
		assert!(!drive.stopped());
		drive.stop().await;
		assert!(drive.stopped());
		assert!(!drive.has_listener());
	}

	#[tokio::test]
	async fn a_root_lookup_set_to_fail_fails_until_cleared() {
		let drive = FakeDrive::new(ROOT);
		drive.fail_root_always(Some(offline_error));
		assert!(drive.root_uuid().await.is_err());
		assert!(drive.root_uuid().await.is_err());
		drive.fail_root_always(None);
		assert_eq!(drive.root_uuid().await.unwrap(), ROOT.to_string());
	}
}
