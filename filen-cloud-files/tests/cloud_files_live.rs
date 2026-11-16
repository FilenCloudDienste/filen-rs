//! The core against the real cache and the test account, through the fake OS: a session sealed
//! into auth.json authenticates the cache the provider opens, and a population and a hydration
//! answer from it. Each test uploads its own files into a fresh test directory.

use std::{
	ops::Range,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex, PoisonError,
		atomic::{AtomicUsize, Ordering},
	},
	task::Poll,
	time::Duration,
};

use filen_cloud_files::{
	AuthPayload, CloudFilesCallback, DataDir, DispatchConfig, Dispatcher, Drive, Failure, FileSize,
	LocalConfig, LocalEngine, LocalReport, PlaceholderIdentity, PlaceholderName, PlaceholderSpec,
	RootPath, SessionStore, SkippedItem, SpecKind, TransferKey,
	test_support::{FakeEvent, FakeLocal, FakeOs, LocalOp, MemoryKeyStore, relocate_for_test},
};
use filen_macros::shared_test_runtime;
use filen_mobile_native_cache::{
	CacheError,
	auth::FilenMobileCacheState,
	ffi::{FfiChanges, FfiFile, FfiId, FfiNonRootObject, FfiObject, UploadFileInfo},
	io::CACHE_DIR,
	remote::{CheckedUpload, PendingCopy},
	traits::WorkingSetUpdateListener,
};
use filen_sdk_rs::{
	auth::Client,
	fs::{
		HasName, HasUUID,
		dir::RemoteDirectory,
		file::{RemoteFile, meta::FileMetaChanges},
	},
	io::client_impl::IoSharedClientExt,
	sync::lock::ResourceLock,
};
use filen_types::fs::Uuid;
use serde_json::json;
use tempfile::TempDir;
use test_utils::TestResources;
use tokio::{runtime::Handle, time::Instant};

/// The small file's size, in bytes: under one SDK chunk.
const SMALL_LEN: usize = 300_000;
/// The big file's size, in bytes: 2.5 MiB, across three SDK chunks.
const BIG_LEN: usize = 2_621_440;

/// `len` bytes of a fixed pattern.
fn pattern(len: usize) -> Vec<u8> {
	(0..len)
		.map(|i| u8::try_from(i * 31 % 251).expect("below 251"))
		.collect()
}

/// The skipped items each population reported.
#[derive(Default)]
struct RecordSkips(Mutex<Vec<(PlaceholderIdentity, Vec<SkippedItem>)>>);

impl RecordSkips {
	fn skipped_under(&self, dir: PlaceholderIdentity) -> Vec<SkippedItem> {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.iter()
			.filter(|(directory, _)| *directory == dir)
			.flat_map(|(_, items)| items.iter().cloned())
			.collect()
	}
}

impl CloudFilesCallback for RecordSkips {
	fn on_skipped_items(&self, directory: PlaceholderIdentity, items: &[SkippedItem]) {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.push((directory, items.to_vec()));
	}

	fn on_local_report(&self, _report: &LocalReport) {}
}

/// The `AuthFile` JSON the desktop app pushes for `client`'s session.
fn session_json(client: &Client, tmp: &Path) -> Vec<u8> {
	let session = client.to_stringified();
	serde_json::to_vec(&json!({
		"providerEnabled": true,
		"sdkConfig": {
			"email": session.email,
			"password": "",
			"twoFactorCode": "",
			"masterKeys": session.auth_info.split('|').collect::<Vec<_>>(),
			"apiKey": session.api_key,
			"publicKey": "",
			"privateKey": session.private_key,
			"authVersion": session.auth_version,
			"baseFolderUUID": session.root_uuid,
			"userId": session.user_id,
			"metadataCache": false,
			"tmpPath": tmp.to_str().unwrap(),
			"connectToSocket": false,
		}
	}))
	.unwrap()
}

/// Opens the cache on `store` from the blocking pool, as the provider must.
async fn open_cache(store: &Arc<SessionStore>) -> Arc<FilenMobileCacheState> {
	let store = Arc::clone(store);
	Arc::new(
		tokio::task::spawn_blocking(move || store.open_cache())
			.await
			.unwrap(),
	)
}

/// A test directory holding `small.bin`, `big.bin` and the directory `sub`, a session for its
/// account sealed into a fresh data directory, and the cache opened on it.
struct Fixture {
	res: TestResources,
	/// Held while the fixture is set up and while a test only reads; a test that writes through the
	/// cache lets go of it first, as the cache's own client takes it for each write.
	lock: Option<Arc<ResourceLock>>,
	temp: TempDir,
	store: Arc<SessionStore>,
	cache: Arc<FilenMobileCacheState>,
	small: Uuid,
	big: Uuid,
	sub: RemoteDirectory,
}

impl Fixture {
	async fn new() -> Self {
		let (res, lock) = test_utils::RESOURCES.get_resources_with_lock().await;
		let small = upload(&res, "small.bin", &pattern(SMALL_LEN)).await;
		let big = upload(&res, "big.bin", &pattern(BIG_LEN)).await;
		let sub = res
			.client
			.create_dir(&(&res.dir).into(), "sub")
			.await
			.unwrap();
		assert!(res.wait_visible().await, "the test directory never showed");
		let temp = tempfile::tempdir().unwrap();
		let store = SessionStore::open(
			DataDir::new(temp.path().join("data")).unwrap(),
			&MemoryKeyStore::new(),
		)
		.unwrap();
		let tmp = temp.path().join("tmp");
		std::fs::create_dir_all(&tmp).unwrap();
		store
			.seal(&AuthPayload::parse(&session_json(&res.client, &tmp)).unwrap())
			.unwrap();
		let store = Arc::new(store);
		let cache = open_cache(&store).await;
		Self {
			res,
			lock: Some(lock),
			temp,
			store,
			cache,
			small,
			big,
			sub,
		}
	}

	/// Lets go of the drive lock, so the cache's client can take it for its writes.
	fn let_go_of_the_lock(&mut self) {
		self.lock = None;
	}

	fn root(&self) -> PlaceholderIdentity {
		PlaceholderIdentity::Root {
			root: self.res.client.root().uuid(),
		}
	}

	fn test_dir(&self) -> PlaceholderIdentity {
		PlaceholderIdentity::Dir {
			uuid: self.res.dir.uuid(),
		}
	}
}

/// Uploads `data` as `name` into the test directory, and returns the version uploaded.
async fn upload(res: &TestResources, name: &str, data: &[u8]) -> Uuid {
	let builder = res.client.make_file_builder(name, res.dir.uuid()).unwrap();
	res.client.upload_file(builder, data).await.unwrap().uuid()
}

/// A dispatcher over `cache` on this runtime, and the callback it reports skips to.
fn dispatcher(
	cache: &Arc<FilenMobileCacheState>,
) -> (Dispatcher<FilenMobileCacheState>, Arc<RecordSkips>) {
	let skips = Arc::new(RecordSkips::default());
	let dispatcher = Dispatcher::new(
		Arc::clone(cache),
		Handle::current(),
		DispatchConfig::default(),
		Arc::clone(&skips) as Arc<dyn CloudFilesCallback>,
	);
	(dispatcher, skips)
}

/// Asks `os` to populate `dir` through `dispatcher` and waits for the answer.
async fn populate(
	dispatcher: &Dispatcher<FilenMobileCacheState>,
	os: &FakeOs,
	dir: PlaceholderIdentity,
) -> FakeEvent {
	let (request, responder) = os.fetch_placeholders(&dir);
	dispatcher.on_fetch_placeholders(request, responder);
	os.wait_for(|event| match event {
		FakeEvent::Populated { dir: populated, .. }
		| FakeEvent::PopulateAbandoned { dir: populated } => *populated == dir,
		FakeEvent::PopulateFailed(_) => true,
		_ => false,
	})
	.await
}

/// Populates the root and then the test directory, and returns the test directory's children.
async fn populate_test_dir(
	fixture: &Fixture,
	dispatcher: &Dispatcher<FilenMobileCacheState>,
	os: &FakeOs,
) -> Vec<PlaceholderSpec> {
	let root = populate(dispatcher, os, fixture.root()).await;
	assert!(
		matches!(root, FakeEvent::Populated { .. }),
		"populating the root: {root:?}"
	);
	let dir = populate(dispatcher, os, fixture.test_dir()).await;
	let FakeEvent::Populated { entries, .. } = dir else {
		panic!("populating the test directory: {dir:?}");
	};
	entries
}

/// The identity of the placeholder named `name` among `entries`.
fn file_named(entries: &[PlaceholderSpec], name: &str) -> PlaceholderIdentity {
	entries
		.iter()
		.find(|spec| spec.name().as_str() == name)
		.unwrap_or_else(|| panic!("no {name} among {entries:?}"))
		.identity()
}

/// Asks `os` for `required` of `file` through `dispatcher` and waits for the answer.
async fn fetch(
	dispatcher: &Dispatcher<FilenMobileCacheState>,
	os: &FakeOs,
	file: PlaceholderIdentity,
	required: (i64, i64),
) -> (TransferKey, FakeEvent) {
	let (request, responder) = os.fetch_data(&file, required, (0, 0));
	let key = request.transfer();
	dispatcher.on_fetch_data(request, responder);
	let answer = os
		.wait_for(|event| match event {
			FakeEvent::Completed(k)
			| FakeEvent::Incomplete(k)
			| FakeEvent::Failed(k, _)
			| FakeEvent::Cancelled(k)
			| FakeEvent::Abandoned(k) => *k == key,
			_ => false,
		})
		.await;
	(key, answer)
}

#[shared_test_runtime]
async fn a_sealed_session_authenticates_the_cache() {
	let fixture = Fixture::new().await;
	assert_eq!(
		fixture.cache.async_root_uuid().await.unwrap(),
		fixture.res.client.root().uuid().to_string()
	);
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn populating_the_test_directory_places_its_children() {
	let fixture = Fixture::new().await;
	let (dispatcher, skips) = dispatcher(&fixture.cache);
	let os = FakeOs::with_root(fixture.root());
	let mut entries = populate_test_dir(&fixture, &dispatcher, &os).await;
	dispatcher.shutdown().await;
	entries.sort_by(|a, b| a.name().as_str().cmp(b.name().as_str()));
	let placed: Vec<(&str, SpecKind)> = entries
		.iter()
		.map(|spec| (spec.name().as_str(), spec.kind()))
		.collect();
	assert_eq!(
		placed,
		[
			(
				"big.bin",
				SpecKind::File {
					size: FileSize::from_i64(2_621_440).unwrap()
				}
			),
			(
				"small.bin",
				SpecKind::File {
					size: FileSize::from_i64(300_000).unwrap()
				}
			),
			("sub", SpecKind::Dir),
		]
	);
	assert!(matches!(
		file_named(&entries, "big.bin"),
		PlaceholderIdentity::File { version, .. } if version == fixture.big
	));
	assert!(matches!(
		file_named(&entries, "small.bin"),
		PlaceholderIdentity::File { version, .. } if version == fixture.small
	));
	assert_eq!(
		file_named(&entries, "sub"),
		PlaceholderIdentity::Dir {
			uuid: fixture.sub.uuid()
		}
	);
	assert_eq!(skips.skipped_under(fixture.test_dir()), []);
	os.assert_clean();
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn hydrating_a_whole_file_completes_byte_equal() {
	let fixture = Fixture::new().await;
	let (dispatcher, _skips) = dispatcher(&fixture.cache);
	let os = FakeOs::with_root(fixture.root());
	let entries = populate_test_dir(&fixture, &dispatcher, &os).await;
	let small = file_named(&entries, "small.bin");
	let (key, answer) = fetch(&dispatcher, &os, small, (0, 300_000)).await;
	dispatcher.shutdown().await;
	assert_eq!(answer, FakeEvent::Completed(key));
	let (data, hydrated) = os.contents(&small);
	assert_eq!(
		hydrated,
		[Range {
			start: 0,
			end: 300_000
		}]
	);
	assert!(data == pattern(SMALL_LEN), "the hydrated bytes differ");
	os.assert_clean();
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn hydrating_across_a_chunk_boundary_is_byte_equal() {
	let fixture = Fixture::new().await;
	let (dispatcher, _skips) = dispatcher(&fixture.cache);
	let os = FakeOs::with_root(fixture.root());
	let entries = populate_test_dir(&fixture, &dispatcher, &os).await;
	let big = file_named(&entries, "big.bin");
	let (key, answer) = fetch(&dispatcher, &os, big, (1_044_480, 8192)).await;
	dispatcher.shutdown().await;
	assert_eq!(answer, FakeEvent::Completed(key));
	let transfers: Vec<(u64, u64)> = os
		.events()
		.iter()
		.filter_map(|event| match event {
			FakeEvent::Transferred {
				key: k,
				offset,
				len,
			} if *k == key => Some((*offset, *len)),
			_ => None,
		})
		.collect();
	assert_eq!(transfers, [(1_044_480, 4096), (1_048_576, 4096)]);
	let (data, hydrated) = os.contents(&big);
	assert_eq!(
		hydrated,
		[Range {
			start: 1_044_480,
			end: 1_052_672
		}]
	);
	assert!(
		data.get(1_044_480..1_052_672) == pattern(BIG_LEN).get(1_044_480..1_052_672),
		"the hydrated bytes differ"
	);
	os.assert_clean();
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn a_cleared_session_reopens_unauthenticated() {
	let fixture = Fixture::new().await;
	fixture.cache.shutdown().await;
	fixture.store.clear().unwrap();
	// A fresh cache: the open one re-reads auth.json only 10 s after its last read.
	let cache = open_cache(&fixture.store).await;
	let (dispatcher, _skips) = dispatcher(&cache);
	let os = FakeOs::with_root(fixture.root());
	let answer = populate(&dispatcher, &os, fixture.root()).await;
	dispatcher.shutdown().await;
	assert_eq!(answer, FakeEvent::PopulateFailed(Failure::NotAuthenticated));
	assert_eq!(os.children(&fixture.root()), []);
	cache.shutdown().await;
}

// ---- the drive seam's writes, against the real cache and server ----

/// The cache id of the directory `dir`.
fn dir_id(dir: &RemoteDirectory) -> FfiId {
	FfiId(format!("stable/{}", dir.uuid()))
}

/// The cache id of the file `file`, by its stable id.
fn file_id(file: &FfiFile) -> FfiId {
	FfiId(format!("stable/{}", file.stable_uuid))
}

/// What a new file named `name` is uploaded as: no dates, the mime from its name.
fn named(name: &str) -> UploadFileInfo {
	UploadFileInfo {
		name: name.to_owned(),
		creation: None,
		modification: None,
		mime: None,
	}
}

/// The BLAKE3 hash of `bytes`.
fn hash(bytes: &[u8]) -> [u8; 32] {
	*blake3::hash(bytes).as_bytes()
}

/// The bytes of a file the test owns outside the cache, written as `bytes`, and its path.
fn local_file(fixture: &Fixture, name: &str, bytes: &[u8]) -> String {
	let path = fixture.temp.path().join("local").join(name);
	std::fs::create_dir_all(path.parent().unwrap()).unwrap();
	std::fs::write(&path, bytes).unwrap();
	path.to_str().unwrap().to_owned()
}

/// Where the cache keeps the bytes of `file`: its slot, `<uuid>/<name>` in the cache directory of
/// the files directory [`SessionStore::open_cache`] gives it.
fn cache_slot(fixture: &Fixture, file: &FfiFile) -> PathBuf {
	let name = &file.meta.as_ref().expect("a decoded file").name;
	fixture
		.temp
		.path()
		.join("data")
		.join("files")
		.join(CACHE_DIR)
		.join(&file.uuid)
		.join(name)
}

/// Whether the file at `path` holds `bytes`: `false` while it is missing or holds anything else.
fn holds(path: &Path, bytes: &[u8]) -> bool {
	std::fs::read(path).is_ok_and(|held| held == bytes)
}

/// The item `id` as the server has it now, through the cache's refresh; `None` when it is gone.
async fn server_item(fixture: &Fixture, id: &FfiId) -> Option<FfiObject> {
	Drive::refresh_item(&*fixture.cache, id.clone())
		.await
		.unwrap()
}

/// The file `id` as the server has it now.
async fn server_file(fixture: &Fixture, id: &FfiId) -> FfiFile {
	match server_item(fixture, id).await {
		Some(FfiObject::File(file)) => file,
		other => panic!("{id:?} is not a file on the server: {other:?}"),
	}
}

/// The bytes of the version `version` of a file, downloaded with the test's own client.
async fn version_bytes(fixture: &Fixture, version: &str) -> Vec<u8> {
	let client = &fixture.res.client;
	let file = client
		.get_file(Uuid::parse_str(version).unwrap())
		.await
		.unwrap();
	client.download_file(&file).await.unwrap()
}

/// How many versions the file whose current version is `version` has.
async fn version_count(fixture: &Fixture, version: &str) -> usize {
	let client = &fixture.res.client;
	let file = client
		.get_file(Uuid::parse_str(version).unwrap())
		.await
		.unwrap();
	client.list_file_versions(&file).await.unwrap().len()
}

/// The child of the test directory named `name` on the server, listed by the test's own client.
async fn listed_file(fixture: &Fixture, name: &str) -> Option<RemoteFile> {
	let (_, files) = fixture
		.res
		.client
		.list_dir(&(&fixture.res.dir).into(), None::<&fn(u64, Option<u64>)>)
		.await
		.unwrap();
	files.into_iter().find(|file| file.name() == Some(name))
}

/// The child of the cache's fresh listing of `dir` named `name`, a file.
async fn child_file(fixture: &Fixture, dir: &FfiId, name: &str) -> FfiFile {
	let listing = Drive::list_children(&*fixture.cache, dir.clone())
		.await
		.unwrap()
		.expect("the cache lists the directory");
	listing
		.objects
		.into_iter()
		.find_map(|object| match object {
			FfiNonRootObject::File(file)
				if file.meta.as_ref().is_some_and(|meta| meta.name == name) =>
			{
				Some(file)
			}
			_ => None,
		})
		.unwrap_or_else(|| panic!("no file {name} in {dir:?}"))
}

/// Waits, a minute at most, for `done` to hold, checking every 100 ms.
async fn eventually<F: Future<Output = bool>>(what: &str, mut done: impl FnMut() -> F) {
	let deadline = Instant::now() + Duration::from_secs(60);
	while !done().await {
		assert!(Instant::now() < deadline, "never happened: {what}");
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
}

#[shared_test_runtime]
async fn every_drive_write_reaches_the_server() {
	let mut fixture = Fixture::new().await;
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let test_dir = dir_id(&fixture.res.dir);
	Drive::list_children(drive, test_dir.clone())
		.await
		.unwrap()
		.expect("the cache lists the test directory");
	let made = Drive::create_dir(drive, test_dir.clone(), "made".to_owned(), None)
		.await
		.unwrap();
	let made_id = FfiId(format!("stable/{}", made.uuid));
	assert!(matches!(
		server_item(&fixture, &made_id).await,
		Some(FfiObject::Dir(dir)) if dir.parent == fixture.res.dir.uuid().to_string()
	));

	let one = local_file(&fixture, "one", b"one");
	let created = Drive::upload_file(drive, one, made_id.clone(), named("a.txt"))
		.await
		.unwrap();
	assert_eq!(version_bytes(&fixture, &created.uuid).await, b"one");
	let file = file_id(&created);

	let two = local_file(&fixture, "two", b"two");
	let CheckedUpload::Uploaded(edited) =
		Drive::upload_version(drive, file.clone(), two.clone(), hash(b"two"))
			.await
			.unwrap()
	else {
		panic!("the version was refused");
	};
	assert_eq!(edited.stable_uuid, created.stable_uuid);
	assert_ne!(edited.uuid, created.uuid);
	assert_eq!(server_file(&fixture, &file).await.uuid, edited.uuid);
	assert_eq!(version_bytes(&fixture, &edited.uuid).await, b"two");
	assert_eq!(Drive::pending_upload_count(drive).await.unwrap(), 0);

	// A copy that does not hash as the caller hashed the file is never uploaded, and leaves
	// nothing marked.
	std::fs::write(&two, b"torn").unwrap();
	assert!(matches!(
		Drive::upload_version(drive, file.clone(), two, hash(b"three"))
			.await
			.unwrap(),
		CheckedUpload::Changed
	));
	assert_eq!(server_file(&fixture, &file).await.uuid, edited.uuid);
	assert_eq!(version_count(&fixture, &edited.uuid).await, 2);
	assert_eq!(Drive::pending_upload_count(drive).await.unwrap(), 0);
	assert!(Drive::pending_uploads(drive).await.unwrap().is_empty());

	let renamed = Drive::rename(drive, file.clone(), "b.txt".to_owned())
		.await
		.unwrap();
	assert!(renamed.is_some());
	let on_server = server_file(&fixture, &file).await;
	assert_eq!(
		on_server.meta.map(|meta| meta.name).as_deref(),
		Some("b.txt")
	);
	assert_eq!(
		Drive::rename(drive, file.clone(), "b.txt".to_owned())
			.await
			.unwrap()
			.map(|_| ()),
		None,
		"a rename to the name it has is nothing to do"
	);

	Drive::move_to(drive, file.clone(), test_dir.clone())
		.await
		.unwrap();
	assert_eq!(
		server_file(&fixture, &file).await.parent,
		fixture.res.dir.uuid().to_string()
	);

	let trashed = Drive::trash(drive, file.clone()).await.unwrap();
	assert!(matches!(trashed, FfiObject::File(ref f) if f.original_parent.is_some()));
	assert!(matches!(
		server_item(&fixture, &file).await,
		Some(FfiObject::File(f)) if f.original_parent.as_deref() == Some(fixture.res.dir.uuid().to_string().as_str())
	));
	// Nothing marked to drop: no error.
	Drive::drop_pending_upload(drive, file).await.unwrap();
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn a_rename_onto_a_taken_name_is_refused_and_a_move_onto_one_supersedes_it() {
	let mut fixture = Fixture::new().await;
	let builder = fixture
		.res
		.client
		.make_file_builder("small.bin", fixture.sub.uuid())
		.unwrap();
	fixture
		.res
		.client
		.upload_file(builder, b"in sub")
		.await
		.unwrap();
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let test_dir = dir_id(&fixture.res.dir);
	let small = child_file(&fixture, &test_dir, "small.bin").await;

	// A new file uploaded under a name of its own, as the provider uploads one, then renamed onto a
	// name another client took meanwhile: refused in the server's case folding, so the other
	// file is never versioned over.
	let mine = local_file(&fixture, "mine", b"mine");
	let uploaded = Drive::upload_file(
		drive,
		mine,
		test_dir.clone(),
		named(".filen-upload-00000000000000000000000000000000"),
	)
	.await
	.unwrap();
	let refused = Drive::rename(drive, file_id(&uploaded), "SMALL.bin".to_owned()).await;
	assert!(
		matches!(refused, Err(CacheError::SDK(ref error)) if error.to_string().contains("file_with_name_already_exists_at_destination")),
		"{refused:?}"
	);
	assert_eq!(
		server_file(&fixture, &file_id(&small)).await.uuid,
		small.uuid,
		"the file holding the name is untouched"
	);
	assert_eq!(version_count(&fixture, &small.uuid).await, 1);
	assert!(
		Drive::rename(drive, file_id(&uploaded), "mine.txt".to_owned())
			.await
			.unwrap()
			.is_some()
	);

	// A move is not refused: the file moved onto the name takes the place of the file there,
	// which lives on only as an older version of it.
	let in_sub = child_file(&fixture, &dir_id(&fixture.sub), "small.bin").await;
	Drive::move_to(drive, file_id(&in_sub), test_dir.clone())
		.await
		.unwrap();
	assert!(
		server_item(&fixture, &file_id(&small)).await.is_none(),
		"the file moved over is no longer current"
	);
	let now = listed_file(&fixture, "small.bin").await.unwrap();
	assert_eq!(now.uuid().to_string(), in_sub.uuid);
	assert_eq!(version_count(&fixture, &in_sub.uuid).await, 2);
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn an_upload_dropped_on_its_way_leaves_its_edit_marked_for_the_take_over() {
	let mut fixture = Fixture::new().await;
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let test_dir = dir_id(&fixture.res.dir);
	let small = child_file(&fixture, &test_dir, "small.bin").await;
	let id = file_id(&small);
	let edit: Vec<u8> = pattern(BIG_LEN).into_iter().rev().collect();
	let path = local_file(&fixture, "edit", &edit);
	let slot = cache_slot(&fixture, &small);
	{
		let upload = Drive::upload_version(drive, id.clone(), path, hash(&edit));
		tokio::pin!(upload);
		// Dropped once the edit's bytes are in the cache's slot, as the provider's stop drops an
		// upload. Not as soon as the edit is marked: the mark goes on before the bytes are copied
		// in, and an upload dropped between the two leaves nothing to copy, its file in the sync
		// root still holding the edit. Polled one step at a time and looked at after each: the
		// step that renames the bytes in cannot also finish the upload, which waits on the server
		// after it.
		loop {
			if let Poll::Ready(done) = futures::poll!(&mut upload) {
				panic!("the upload ended before it could be dropped: {done:?}");
			}
			if holds(&slot, &edit) {
				break;
			}
			tokio::task::yield_now().await;
		}
	}
	assert_eq!(
		Drive::pending_uploads(drive).await.unwrap(),
		std::slice::from_ref(&id)
	);
	assert_eq!(
		server_file(&fixture, &id).await.uuid,
		small.uuid,
		"the dropped upload never reached the server"
	);
	let kept = fixture.temp.path().join("kept.bin");
	assert_eq!(
		Drive::copy_pending_upload(drive, id.clone(), kept.clone())
			.await
			.unwrap(),
		PendingCopy::Copied
	);
	assert!(
		std::fs::read(&kept).unwrap() == edit,
		"the copy is the edit"
	);
	assert!(
		Drive::copy_pending_upload(drive, id.clone(), kept.clone())
			.await
			.is_err(),
		"never over a file that is there"
	);
	assert_eq!(Drive::pending_upload_count(drive).await.unwrap(), 1);
	Drive::drop_pending_upload(drive, id.clone()).await.unwrap();
	assert_eq!(Drive::pending_upload_count(drive).await.unwrap(), 0);
	assert_eq!(
		Drive::copy_pending_upload(drive, id, fixture.temp.path().join("again.bin"))
			.await
			.unwrap(),
		PendingCopy::Nothing
	);
	fixture.cache.shutdown().await;
}

/// Counts the cache's calls to its listener.
#[derive(Default)]
struct Told(AtomicUsize);

impl WorkingSetUpdateListener for Told {
	fn working_set_changed(&self) {
		self.0.fetch_add(1, Ordering::SeqCst);
	}
}

/// Waits, a minute at most, until the listener has not been told anything for two seconds, and
/// says how often it was told: what the server sent before is all in by then.
async fn until_quiet(told: &Told) -> usize {
	let deadline = Instant::now() + Duration::from_secs(60);
	let mut seen = told.0.load(Ordering::SeqCst);
	let mut quiet_since = Instant::now();
	loop {
		tokio::time::sleep(Duration::from_millis(100)).await;
		let now = told.0.load(Ordering::SeqCst);
		if now != seen {
			(seen, quiet_since) = (now, Instant::now());
		} else if quiet_since.elapsed() >= Duration::from_secs(2) {
			return seen;
		}
		assert!(Instant::now() < deadline, "the listener never went quiet");
	}
}

/// The ids of the items `changes` updated.
fn updated_ids(changes: &FfiChanges) -> Vec<(String, Option<String>, bool)> {
	changes
		.updated
		.iter()
		.filter_map(|object| match object {
			FfiObject::File(file) => Some((
				file.stable_uuid.clone(),
				file.meta.as_ref().map(|meta| meta.name.clone()),
				file.original_parent.is_some(),
			)),
			FfiObject::Dir(dir) => Some((
				dir.uuid.clone(),
				dir.meta.as_ref().map(|meta| meta.name.clone()),
				dir.original_parent.is_some(),
			)),
			FfiObject::Root(_) => None,
		})
		.collect()
}

/// Every change after `anchor`, all pages.
async fn changes_after(fixture: &Fixture, anchor: &[u8]) -> FfiChanges {
	let mut all = FfiChanges {
		updated: Vec::new(),
		deleted_ids: Vec::new(),
		anchor: anchor.to_vec(),
		more: true,
	};
	while all.more {
		let page = Drive::changes(&*fixture.cache, Some(all.anchor.clone()), 100)
			.await
			.unwrap();
		all.updated.extend(page.updated);
		all.deleted_ids.extend(page.deleted_ids);
		all.anchor = page.anchor;
		all.more = page.more;
	}
	all
}

#[shared_test_runtime]
async fn the_change_feed_holds_another_clients_changes_and_the_providers_own() {
	let mut fixture = Fixture::new().await;
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let test_dir = dir_id(&fixture.res.dir);
	Drive::set_populated_dirs(drive, vec![test_dir.0.clone()])
		.await
		.unwrap();
	let small = child_file(&fixture, &test_dir, "small.bin").await;
	let big = child_file(&fixture, &test_dir, "big.bin").await;
	let anchor = Drive::change_anchor(drive).await.unwrap();
	let told = Arc::new(Told::default());
	Drive::watch_changes(
		drive,
		Some(Arc::clone(&told) as Arc<dyn WorkingSetUpdateListener>),
	)
	.await;

	// Another client creates, edits, renames and trashes.
	let client = &fixture.res.client;
	upload(&fixture.res, "theirs.txt", b"theirs").await;
	let edited = upload(&fixture.res, "small.bin", b"edited elsewhere").await;
	let mut big_file = client
		.get_file(Uuid::parse_str(&big.uuid).unwrap())
		.await
		.unwrap();
	client
		.update_file_metadata(
			&mut big_file,
			FileMetaChanges::default().name("renamed.bin").unwrap(),
		)
		.await
		.unwrap();
	let mut sub = fixture.sub.clone();
	client.trash_dir(&mut sub).await.unwrap();
	let sub_uuid = fixture.sub.uuid().to_string();
	eventually("the feed holds the other client's four changes", || async {
		let ids = updated_ids(&changes_after(&fixture, &anchor).await);
		let has = |uuid: &str, name: &str, trashed: bool| {
			ids.iter().any(|(id, at, in_trash)| {
				id == uuid && at.as_deref() == Some(name) && *in_trash == trashed
			})
		};
		ids.iter()
			.any(|(_, name, _)| name.as_deref() == Some("theirs.txt"))
			&& has(&big.stable_uuid, "renamed.bin", false)
			&& has(&sub_uuid, "sub", true)
			&& changes_after(&fixture, &anchor)
				.await
				.updated
				.iter()
				.any(|object| matches!(object, FfiObject::File(file) if file.stable_uuid == small.stable_uuid && file.uuid == edited.to_string()))
	})
	.await;
	assert!(
		told.0.load(Ordering::SeqCst) > 0,
		"the listener is told of the other client's changes"
	);

	// The provider's own upload is in the feed too, at its state as uploaded.
	let before = until_quiet(&told).await;
	let mark = Drive::change_anchor(drive).await.unwrap();
	let own = local_file(&fixture, "own", b"own");
	let uploaded = Drive::upload_file(drive, own, test_dir.clone(), named("own.txt"))
		.await
		.unwrap();
	let ids = updated_ids(&changes_after(&fixture, &mark).await);
	assert!(
		ids.iter()
			.any(|(id, name, trashed)| *id == uploaded.stable_uuid
				&& name.as_deref() == Some("own.txt")
				&& !trashed),
		"{ids:?}"
	);
	// And the listener is told of it, as the server's event for it comes back.
	eventually(
		"the listener is told of the provider's own upload",
		|| async { told.0.load(Ordering::SeqCst) > before },
	)
	.await;
	Drive::watch_changes(drive, None).await;
	fixture.cache.shutdown().await;
}

// ---- the local engine over the real cache ----

/// Records every local report.
#[derive(Default)]
struct Reports(Mutex<Vec<LocalReport>>);

impl Reports {
	fn all(&self) -> Vec<LocalReport> {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.clone()
	}
}

impl CloudFilesCallback for Reports {
	fn on_skipped_items(&self, _directory: PlaceholderIdentity, _items: &[SkippedItem]) {}

	fn on_local_report(&self, report: &LocalReport) {
		self.0
			.lock()
			.unwrap_or_else(PoisonError::into_inner)
			.push(report.clone());
	}
}

/// A local engine over the fixture's cache and `local`, started, with a task handing it the
/// watcher's events every 100 ms.
fn start_engine(
	fixture: &Fixture,
	local: &FakeLocal,
	reports: &Arc<Reports>,
) -> (
	Arc<LocalEngine<FilenMobileCacheState, FakeLocal>>,
	tokio::task::JoinHandle<()>,
) {
	let engine = Arc::new(LocalEngine::new(
		Arc::clone(&fixture.cache),
		Arc::new(local.clone()),
		Handle::current(),
		LocalConfig::default(),
		Arc::clone(reports) as Arc<dyn CloudFilesCallback>,
	));
	engine.start();
	let (pumped, watched) = (Arc::clone(&engine), local.clone());
	let pump = tokio::spawn(async move {
		loop {
			for event in watched.take_watch_events() {
				pumped.on_local_event(event);
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
	});
	(engine, pump)
}

#[shared_test_runtime]
async fn an_edit_stopped_on_its_way_never_replaces_a_newer_version_end_to_end() {
	let mut fixture = Fixture::new().await;
	let first = upload(&fixture.res, "f.bin", b"one").await;
	fixture.let_go_of_the_lock();
	let test_dir = dir_id(&fixture.res.dir);
	let f = child_file(&fixture, &test_dir, "f.bin").await;
	assert_eq!(f.uuid, first.to_string());
	let id = file_id(&f);
	let stable = Uuid::parse_str(&f.stable_uuid).unwrap();

	// The account root stays unpopulated: only the test directory is the provider's.
	let local = FakeLocal::new(fixture.root());
	local.mirror_uploads(fixture.temp.path().join("mirror"));
	let dir = RootPath::parse([fixture.res.dir.name().unwrap()]).unwrap();
	local.place_dir(&dir, fixture.test_dir());
	let at = |name: &str| dir.join(PlaceholderName::parse(name).unwrap());
	local.place_file(
		&at("f.bin"),
		PlaceholderIdentity::File {
			stable,
			version: first,
		},
		b"one",
	);
	local.take_watch_events();
	let reports = Arc::new(Reports::default());
	let (engine, pump) = start_engine(&fixture, &local, &reports);

	// A new file is uploaded once, under a name of its own and then its name, and its own changes
	// coming back through the feed make nothing more of it.
	local.user_create(&at("new.txt"), b"new");
	eventually("the new file is in sync", || async {
		local
			.entry(&at("new.txt"))
			.is_some_and(|entry| entry.placeholder && entry.in_sync)
	})
	.await;
	let new = listed_file(&fixture, "new.txt").await.expect("uploaded");
	assert_eq!(version_count(&fixture, &new.uuid().to_string()).await, 1);

	// An edit whose upload is stopped on its way: the provider stops with it marked.
	let edit: Vec<u8> = pattern(BIG_LEN).into_iter().rev().collect();
	local.user_write(&at("f.bin"), &edit).unwrap();
	eventually("the edit's upload is on its way", || async {
		Drive::pending_upload_count(&*fixture.cache).await.unwrap() > 0
	})
	.await;
	engine.shutdown().await;
	pump.abort();
	assert_eq!(
		Drive::pending_uploads(&*fixture.cache).await.unwrap(),
		std::slice::from_ref(&id)
	);
	assert_eq!(server_file(&fixture, &id).await.uuid, f.uuid);

	// Another client uploads a newer version, and the file is open in an editor when the
	// provider starts again.
	let newer = upload(&fixture.res, "f.bin", b"newer elsewhere").await;
	assert_eq!(server_file(&fixture, &id).await.uuid, newer.to_string());
	let writer = local.user_open_for_write(&at("f.bin"));
	let (engine, pump) = start_engine(&fixture, &local, &reports);
	eventually("the take-over gives up the cache's copy", || async {
		Drive::pending_uploads(&*fixture.cache)
			.await
			.unwrap()
			.is_empty()
	})
	.await;
	// Nothing to wait for here: the version count checked once the edit is kept below holds any
	// upload made meanwhile.
	assert_eq!(server_file(&fixture, &id).await.uuid, newer.to_string());
	assert!(
		local.entry(&at("f.bin")).unwrap().bytes == edit,
		"the edit stays the file's"
	);

	// Once the editor lets go, the edit is kept beside the newer version as a new file.
	drop(writer);
	eventually("the edit is kept as a new file", || async {
		listed_file(&fixture, "f.old.bin").await.is_some()
	})
	.await;
	let kept = listed_file(&fixture, "f.old.bin").await.unwrap();
	assert!(version_bytes(&fixture, &kept.uuid().to_string()).await == edit);
	assert_eq!(server_file(&fixture, &id).await.uuid, newer.to_string());
	assert_eq!(
		version_bytes(&fixture, &newer.to_string()).await,
		b"newer elsewhere"
	);
	assert_eq!(
		version_count(&fixture, &newer.to_string()).await,
		2,
		"the file has its own two versions and no other"
	);
	eventually("the kept edit is in sync", || async {
		local
			.entry(&at("f.old.bin"))
			.is_some_and(|entry| entry.in_sync)
	})
	.await;
	assert!(local.entry(&at("f.old.bin")).unwrap().bytes == edit);
	assert!(
		!reports.all().iter().any(
			|report| matches!(report, LocalReport::Conflict { path, .. } if *path == at("new.txt"))
		),
		"the provider's own upload is never taken for a conflict"
	);
	engine.shutdown().await;
	pump.abort();
	fixture.cache.shutdown().await;
}

// ---- moves into another directory, against the server ----

/// The names of the directory `dir`'s children, files and directories, from the cache's fresh
/// listing, sorted.
async fn names_in(fixture: &Fixture, dir: &FfiId) -> Vec<String> {
	let listing = Drive::list_children(&*fixture.cache, dir.clone())
		.await
		.unwrap()
		.expect("the cache lists the directory");
	let mut names: Vec<String> = listing
		.objects
		.into_iter()
		.filter_map(|object| match object {
			FfiNonRootObject::File(file) => file.meta.map(|meta| meta.name),
			FfiNonRootObject::Dir(dir) => dir.meta.map(|meta| meta.name),
		})
		.collect();
	names.sort();
	names
}

/// The directory `id` as the server has it now.
async fn server_dir(fixture: &Fixture, id: &FfiId) -> filen_mobile_native_cache::ffi::FfiDir {
	match server_item(fixture, id).await {
		Some(FfiObject::Dir(dir)) => dir,
		other => panic!("{id:?} is not a directory on the server: {other:?}"),
	}
}

#[shared_test_runtime]
async fn a_file_moves_to_another_directory_under_its_transit_name_and_back_when_its_name_is_taken()
{
	let mut fixture = Fixture::new().await;
	upload(&fixture.res, "mv.txt", b"move me").await;
	upload(&fixture.res, "taken.txt", b"another file").await;
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let (test_dir, sub) = (dir_id(&fixture.res.dir), dir_id(&fixture.sub));
	let mv = child_file(&fixture, &test_dir, "mv.txt").await;
	let taken = child_file(&fixture, &test_dir, "taken.txt").await;
	let stable = Uuid::parse_str(&mv.stable_uuid).unwrap();
	let id = file_id(&mv);

	assert!(
		relocate_for_test(drive, stable, &test_dir, "mv.txt", &sub, "moved.txt")
			.await
			.unwrap()
	);
	let moved = server_file(&fixture, &id).await;
	assert_eq!(moved.parent, fixture.sub.uuid().to_string());
	assert_eq!(
		moved.meta.map(|meta| meta.name).as_deref(),
		Some("moved.txt")
	);
	assert_eq!(moved.uuid, mv.uuid, "a move makes no version");
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["big.bin", "small.bin", "sub", "taken.txt"]
	);
	assert_eq!(names_in(&fixture, &sub).await, ["moved.txt"]);

	// Onto a name another file holds in the server's case folding: refused under the transit
	// name, and moved back under its own.
	assert!(
		!relocate_for_test(drive, stable, &sub, "moved.txt", &test_dir, "TAKEN.txt")
			.await
			.unwrap()
	);
	let back = server_file(&fixture, &id).await;
	assert_eq!(back.parent, fixture.sub.uuid().to_string());
	assert_eq!(
		back.meta.map(|meta| meta.name).as_deref(),
		Some("moved.txt")
	);
	assert_eq!(
		server_file(&fixture, &file_id(&taken)).await.uuid,
		taken.uuid,
		"the file holding the name is untouched"
	);
	assert_eq!(version_count(&fixture, &taken.uuid).await, 1);
	assert_eq!(version_count(&fixture, &mv.uuid).await, 1);
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["big.bin", "small.bin", "sub", "taken.txt"]
	);
	assert_eq!(names_in(&fixture, &sub).await, ["moved.txt"]);
	fixture.cache.shutdown().await;
}

#[shared_test_runtime]
async fn moves_into_another_directory_reach_the_server_and_onto_a_taken_name_are_vetoed() {
	let mut fixture = Fixture::new().await;
	let first = upload(&fixture.res, "f.txt", b"f").await;
	upload(&fixture.res, "taken.txt", b"another file").await;
	let in_test_dir = |name: &'static str| {
		let client = &fixture.res.client;
		let dir = &fixture.res.dir;
		async move { client.create_dir(&dir.into(), name).await.unwrap() }
	};
	let d = in_test_dir("d").await;
	let taken_dir = in_test_dir("takendir").await;
	fixture.let_go_of_the_lock();
	let (test_dir, sub) = (dir_id(&fixture.res.dir), dir_id(&fixture.sub));
	let f = child_file(&fixture, &test_dir, "f.txt").await;
	let stable = Uuid::parse_str(&f.stable_uuid).unwrap();

	// The account root stays unpopulated: only the test directory is the provider's.
	let local = FakeLocal::new(fixture.root());
	let dir = RootPath::parse([fixture.res.dir.name().unwrap()]).unwrap();
	local.place_dir(&dir, fixture.test_dir());
	let at = |name: &str| dir.join(PlaceholderName::parse(name).unwrap());
	let in_sub = |name: &str| at("sub").join(PlaceholderName::parse(name).unwrap());
	local.place_dir(
		&at("sub"),
		PlaceholderIdentity::Dir {
			uuid: fixture.sub.uuid(),
		},
	);
	local.place_dir(&at("d"), PlaceholderIdentity::Dir { uuid: d.uuid() });
	local.place_file(
		&at("f.txt"),
		PlaceholderIdentity::File {
			stable,
			version: first,
		},
		b"f",
	);
	local.take_watch_events();
	let reports = Arc::new(Reports::default());
	let (engine, pump) = start_engine(&fixture, &local, &reports);
	let answers = || {
		local
			.ops()
			.into_iter()
			.filter_map(|op| match op {
				LocalOp::RenameAnswered { answer, .. } => Some(answer),
				_ => None,
			})
			.collect::<Vec<_>>()
	};
	let rename = |from: RootPath, to: RootPath| {
		let (request, responder) = local.user_rename(&from, &to).unwrap().asked();
		engine.on_notify_rename(request, responder);
	};
	let answered = |count: usize| {
		let answers = &answers;
		async move { answers().len() >= count }
	};

	// A file and a directory moved into another directory, each renamed on the way.
	rename(at("f.txt"), in_sub("moved.txt"));
	eventually("the file's move is answered", || answered(1)).await;
	rename(at("d"), in_sub("d2"));
	eventually("the directory's move is answered", || answered(2)).await;
	assert_eq!(answers(), [Ok(()), Ok(())]);
	let moved = server_file(&fixture, &file_id(&f)).await;
	assert_eq!(moved.parent, fixture.sub.uuid().to_string());
	assert_eq!(
		moved.meta.map(|meta| meta.name).as_deref(),
		Some("moved.txt")
	);
	assert_eq!(moved.uuid, f.uuid);
	let d_id = FfiId(format!("stable/{}", d.uuid()));
	let moved_dir = server_dir(&fixture, &d_id).await;
	assert_eq!(moved_dir.parent, fixture.sub.uuid().to_string());
	assert_eq!(moved_dir.meta.map(|meta| meta.name).as_deref(), Some("d2"));
	assert_eq!(names_in(&fixture, &sub).await, ["d2", "moved.txt"]);

	// Back out, onto names another item holds in the server's case folding: vetoed, nothing moved.
	rename(in_sub("moved.txt"), at("TAKEN.txt"));
	eventually("the file's move onto a taken name is answered", || {
		answered(3)
	})
	.await;
	rename(in_sub("d2"), at("TAKENDIR"));
	eventually("the directory's move onto a taken name is answered", || {
		answered(4)
	})
	.await;
	assert_eq!(
		answers(),
		[
			Ok(()),
			Ok(()),
			Err(Failure::Transient),
			Err(Failure::Transient)
		]
	);
	assert_eq!(names_in(&fixture, &sub).await, ["d2", "moved.txt"]);
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["big.bin", "small.bin", "sub", "taken.txt", "takendir"]
	);
	assert_eq!(
		server_dir(&fixture, &FfiId(format!("stable/{}", taken_dir.uuid())))
			.await
			.parent,
		fixture.res.dir.uuid().to_string()
	);

	// The moved file is marked in sync again with nothing uploaded.
	eventually("the moved file is in sync", || async {
		local
			.entry(&in_sub("moved.txt"))
			.is_some_and(|entry| entry.in_sync)
	})
	.await;
	assert_eq!(version_count(&fixture, &f.uuid).await, 1);
	engine.shutdown().await;
	pump.abort();
	fixture.cache.shutdown().await;
}

/// Whether `result` is the server refusing with `code`.
fn refused_with<T: std::fmt::Debug>(result: &Result<T, CacheError>, code: &str) -> bool {
	matches!(result, Err(CacheError::SDK(error)) if error.to_string().contains(code))
}

/// A directory never lands on a name another item in its directory holds, renamed or moved, which
/// is why the engine moves a directory under its own name with no transit name (measured on the
/// server, where a file moved onto a taken name takes the other file's place).
#[shared_test_runtime]
async fn a_directory_is_never_renamed_or_moved_onto_a_taken_name() {
	let mut fixture = Fixture::new().await;
	let client = &fixture.res.client;
	let a = client
		.create_dir(&(&fixture.res.dir).into(), "a")
		.await
		.unwrap();
	client
		.create_dir(&(&fixture.sub).into(), "a")
		.await
		.unwrap();
	client
		.create_dir(&(&fixture.res.dir).into(), "b")
		.await
		.unwrap();
	upload(&fixture.res, "c.txt", b"c").await;
	let d = client
		.create_dir(&(&fixture.res.dir).into(), "d.txt")
		.await
		.unwrap();
	let builder = client
		.make_file_builder("D.txt", fixture.sub.uuid())
		.unwrap();
	client.upload_file(builder, b"d").await.unwrap();
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let (test_dir, sub) = (dir_id(&fixture.res.dir), dir_id(&fixture.sub));
	let a_id = FfiId(format!("stable/{}", a.uuid()));
	let d_id = FfiId(format!("stable/{}", d.uuid()));
	let renamed = Drive::rename(drive, a_id.clone(), "B".to_owned()).await;
	assert!(
		refused_with(&renamed, "folder_with_name_already_exists_at_destination"),
		"{renamed:?}"
	);
	let renamed = Drive::rename(drive, a_id.clone(), "C.txt".to_owned()).await;
	assert!(
		refused_with(&renamed, "file_with_same_name_exists_in_parent"),
		"{renamed:?}"
	);
	let moved = Drive::move_to(drive, a_id.clone(), sub.clone()).await;
	assert!(
		refused_with(&moved, "folder_with_name_already_exists_at_destination"),
		"{moved:?}"
	);
	let moved = Drive::move_to(drive, d_id, sub.clone()).await;
	eprintln!("a directory moved onto a file's name: {moved:?}");
	assert!(
		refused_with(&moved, "file_with_same_name_exists_in_parent"),
		"{moved:?}"
	);
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["a", "b", "big.bin", "c.txt", "d.txt", "small.bin", "sub"]
	);
	assert_eq!(names_in(&fixture, &sub).await, ["D.txt", "a"]);
	assert_eq!(
		server_dir(&fixture, &a_id).await.parent,
		fixture.res.dir.uuid().to_string()
	);
	fixture.cache.shutdown().await;
}

// ---- a save's temporary file uploaded before its rename, against the server ----

/// What the engine asks of the drive when a save's temporary file, uploaded as a file of its own
/// (under an upload name, then named), is renamed over the file it saves: nothing at the rename,
/// then the temporary file trashed, then the saved file given the temporary file's bytes as a new
/// version from the local file. Measured here: the saved file keeps its id with one more version,
/// the temporary file is in the trash with its own bytes, the trash leaves the saved file alone,
/// a second trash of it is no error, and the directory holds the saved file's name only.
#[shared_test_runtime]
async fn a_saves_uploaded_temporary_file_becomes_a_version_of_the_saved_file_and_goes_to_the_trash()
{
	let mut fixture = Fixture::new().await;
	upload(&fixture.res, "save.txt", b"one").await;
	fixture.let_go_of_the_lock();
	let drive = &*fixture.cache;
	let test_dir = dir_id(&fixture.res.dir);
	let saved = child_file(&fixture, &test_dir, "save.txt").await;
	let saved_id = file_id(&saved);

	// The temporary file's own upload, as the engine makes a new file's.
	let temp_path = local_file(&fixture, "save.txt.tmp", b"two");
	let uploaded = Drive::upload_file(
		drive,
		temp_path.clone(),
		test_dir.clone(),
		named(".filen-upload-00000000000000000000000000000001"),
	)
	.await
	.unwrap();
	let temp_id = file_id(&uploaded);
	assert!(
		Drive::rename(drive, temp_id.clone(), "save.txt.tmp".to_owned())
			.await
			.unwrap()
			.is_some()
	);
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["big.bin", "save.txt", "save.txt.tmp", "small.bin", "sub"]
	);

	// Once the rename happened: the temporary upload to the trash first, while the local file
	// holds its bytes.
	let trashed = Drive::trash(drive, temp_id.clone()).await.unwrap();
	assert!(matches!(trashed, FfiObject::File(ref file) if file.original_parent.is_some()));
	assert!(matches!(
		server_item(&fixture, &temp_id).await,
		Some(FfiObject::File(file)) if file.original_parent.as_deref() == Some(fixture.res.dir.uuid().to_string().as_str())
	));
	assert_eq!(version_bytes(&fixture, &uploaded.uuid).await, b"two");
	assert_eq!(
		server_file(&fixture, &saved_id).await.uuid,
		saved.uuid,
		"the saved file is untouched by the trash"
	);
	assert_eq!(version_count(&fixture, &saved.uuid).await, 1);
	// A trash tried again after a lost answer.
	Drive::trash(drive, temp_id.clone()).await.unwrap();

	// Then the saved file's job: its new version from the local file.
	let CheckedUpload::Uploaded(version) =
		Drive::upload_version(drive, saved_id.clone(), temp_path, hash(b"two"))
			.await
			.unwrap()
	else {
		panic!("the version was refused");
	};
	assert_eq!(
		version.stable_uuid, saved.stable_uuid,
		"the saved file keeps its id"
	);
	assert_ne!(version.uuid, saved.uuid);
	let now = server_file(&fixture, &saved_id).await;
	assert_eq!(now.uuid, version.uuid);
	assert!(now.original_parent.is_none());
	assert_eq!(
		now.meta.as_ref().map(|meta| meta.name.as_str()),
		Some("save.txt")
	);
	assert_eq!(version_bytes(&fixture, &version.uuid).await, b"two");
	assert_eq!(version_count(&fixture, &version.uuid).await, 2);
	assert_eq!(
		names_in(&fixture, &test_dir).await,
		["big.bin", "save.txt", "small.bin", "sub"]
	);
	assert_eq!(Drive::pending_upload_count(drive).await.unwrap(), 0);
	fixture.cache.shutdown().await;
}
