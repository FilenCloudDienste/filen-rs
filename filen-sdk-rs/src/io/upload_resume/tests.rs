//! The resuming upload against a fake server and an in-memory journal store.

use std::{
	collections::HashMap,
	io::Write as _,
	path::{Path, PathBuf},
	sync::{
		Arc, Mutex,
		atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
	},
	time::{Duration, SystemTime},
};

use chrono::{DateTime, TimeZone as _, Utc};
use filen_types::{
	api::v3::upload::empty::Response,
	crypto::Blake3Hash,
	error::ResponseError,
	fs::{StableUuid, Uuid},
};
use tempfile::TempDir;

use super::{
	backend::ResumeBackend,
	engine::{ResumeCallbacks, ResumeSource, upload_resuming},
	record::{open, seal},
	store::{SealedUploadJournal, UploadJournalId, UploadJournalStore},
};
use crate::{
	Error, ErrorKind,
	auth::{Client, http::SharedClientState, test_support::offline_client},
	consts::CHUNK_SIZE_U64,
	crypto::{
		file::FileKey,
		shared::MetaCrypter,
		v2::{self, MasterKey, MasterKeys},
	},
	fs::{
		file::{
			BaseFile, FileBuilder, FileBuilderOptionalName, RemoteFile, RootFile,
			write::{RemoteFileInfo, UploadCompletion, remote_file_from_upload},
		},
		name::ValidatedName,
	},
	io::FilenMetaExt,
	util::{MaybeSendBoxFuture, MaybeSendCallback},
};

const PARENT: Uuid = Uuid::from_u128(0x100);
const NAME: &str = "source.bin";
/// Eleven chunks, the last one short.
const SIZE: u64 = CHUNK_SIZE_U64 * 10 + 123;
const CHUNKS: u64 = 11;

fn server_error(code: &str) -> Error {
	Error::from(ResponseError::ApiError {
		message: Some(format!("the server says {code}")),
		code: Some(code.to_owned()),
	})
}

/// How the fake server answers chunks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChunkAnswers {
	Accept,
	/// Every chunk is refused as one of a finished upload, which the server then has.
	BlacklistFinished,
	/// Every chunk is refused as one of a finished upload, but the server has no such file.
	BlacklistMissing,
}

/// How the source changes while it is uploaded.
#[derive(Clone, Copy)]
enum SourceChange {
	/// A byte is appended.
	Grow,
	/// Its modification time moves, its contents stay.
	Touch,
}

/// A file registered with the fake server.
struct Registered {
	file: RemoteFile,
	upload_key: String,
	completion: UploadCompletion,
}

/// A server that accepts every chunk and registration unless told otherwise.
struct FakeResumeBackend {
	/// Lends its memory budget and its metadata crypter, which is the same for every offline
	/// client.
	client: Client,
	now: Mutex<DateTime<Utc>>,
	/// `(file uuid, chunk index)` of every chunk sent, in order.
	sent: Mutex<Vec<(Uuid, u64)>>,
	registered: Mutex<Vec<Registered>>,
	/// Files the server has, by uuid.
	existing: Mutex<HashMap<Uuid, RemoteFile>>,
	propagated: Mutex<Vec<Uuid>>,
	fail_chunk_at: Option<u64>,
	chunk_answers: ChunkAnswers,
	done_rejects_once: Mutex<Option<&'static str>>,
	done_fails_transiently: bool,
	/// Changes the file at this path once chunk `.1` is sent.
	change_at: Option<(PathBuf, u64, SourceChange)>,
}

impl FakeResumeBackend {
	fn new() -> Self {
		Self {
			client: offline_client(),
			now: Mutex::new(Utc.with_ymd_and_hms(2026, 11, 9, 12, 0, 0).unwrap()),
			sent: Mutex::default(),
			registered: Mutex::default(),
			existing: Mutex::default(),
			propagated: Mutex::default(),
			fail_chunk_at: None,
			chunk_answers: ChunkAnswers::Accept,
			done_rejects_once: Mutex::new(None),
			done_fails_transiently: false,
			change_at: None,
		}
	}

	fn fail_chunk_at(mut self, idx: u64) -> Self {
		self.fail_chunk_at = Some(idx);
		self
	}

	fn answer_chunks(mut self, answers: ChunkAnswers) -> Self {
		self.chunk_answers = answers;
		self
	}

	fn done_rejects_once(self, code: &'static str) -> Self {
		*self.done_rejects_once.lock().unwrap() = Some(code);
		self
	}

	fn done_fails_transiently(mut self) -> Self {
		self.done_fails_transiently = true;
		self
	}

	fn change_at(mut self, path: &Path, idx: u64, change: SourceChange) -> Self {
		self.change_at = Some((path.to_owned(), idx, change));
		self
	}

	fn file_exists(self, uuid: Uuid) -> Self {
		self.existing
			.lock()
			.unwrap()
			.insert(uuid, existing_file(uuid));
		self
	}

	fn sent(&self) -> Vec<(Uuid, u64)> {
		self.sent.lock().unwrap().clone()
	}

	fn sent_indices(&self, uuid: Uuid) -> Vec<u64> {
		self.sent()
			.into_iter()
			.filter(|(sent, _)| *sent == uuid)
			.map(|(_, idx)| idx)
			.collect()
	}
}

/// A file of [`SIZE`] bytes the server has, as `find_file` answers it.
fn existing_file(uuid: Uuid) -> RemoteFile {
	let base = BaseFile {
		root: RootFile {
			uuid,
			name: ValidatedName::try_from(NAME).unwrap(),
			mime: "application/octet-stream".to_owned(),
			key: FileKey::V2(
				"0123456789abcdef0123456789abcdef"
					.parse::<v2::FileKey>()
					.unwrap(),
			),
			created: Utc.timestamp_millis_opt(0).unwrap(),
			modified: Utc.timestamp_millis_opt(0).unwrap(),
		},
		parent: PARENT,
	};
	remote_file_from_upload(
		base,
		Response {
			uuid,
			stable_uuid: StableUuid::new_for_test(uuid),
			chunks: CHUNKS,
			size: SIZE,
			timestamp: Utc.timestamp_millis_opt(0).unwrap(),
		},
		RemoteFileInfo::default(),
		UploadCompletion {
			written: SIZE,
			num_chunks: CHUNKS,
			hash: Blake3Hash::from(blake3::hash(b"")),
			final_times: (
				Utc.timestamp_millis_opt(0).unwrap(),
				Utc.timestamp_millis_opt(0).unwrap(),
			),
		},
	)
}

impl ResumeBackend for FakeResumeBackend {
	fn state(&self) -> &SharedClientState {
		self.client.client().state()
	}

	fn crypter(&self) -> Arc<impl MetaCrypter + 'static> {
		self.client.crypter()
	}

	fn make_file_key(&self) -> FileKey {
		self.client.make_file_key()
	}

	fn now(&self) -> DateTime<Utc> {
		*self.now.lock().unwrap()
	}

	async fn upload_chunk(
		&self,
		file: &BaseFile,
		_upload_key: &str,
		idx: u64,
		mut data: Vec<u8>,
	) -> Result<(Vec<u8>, RemoteFileInfo), Error> {
		match self.chunk_answers {
			ChunkAnswers::Accept => {}
			ChunkAnswers::BlacklistFinished => {
				self.existing
					.lock()
					.unwrap()
					.insert(file.uuid(), existing_file(file.uuid()));
				return Err(server_error("upload_blacklisted"));
			}
			ChunkAnswers::BlacklistMissing => return Err(server_error("upload_blacklisted")),
		}
		if self.fail_chunk_at == Some(idx) {
			return Err(Error::custom(ErrorKind::IO, "connection reset"));
		}
		self.sent.lock().unwrap().push((file.uuid(), idx));
		if let Some((path, at, change)) = &self.change_at
			&& *at == idx
		{
			let mut source = std::fs::OpenOptions::new().append(true).open(path).unwrap();
			match change {
				SourceChange::Grow => source.write_all(b"x").unwrap(),
				SourceChange::Touch => source
					.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
					.unwrap(),
			}
		}
		data.clear();
		Ok((
			data,
			RemoteFileInfo {
				region: "de-1".to_owned(),
				bucket: "fake-bucket".to_owned(),
			},
		))
	}

	async fn find_file(&self, uuid: Uuid) -> Result<Option<RemoteFile>, Error> {
		Ok(self.existing.lock().unwrap().get(&uuid).cloned())
	}

	async fn register(
		&self,
		file: &BaseFile,
		upload_key: &str,
		completion: UploadCompletion,
		info: RemoteFileInfo,
	) -> Result<RemoteFile, Error> {
		if let Some(code) = self.done_rejects_once.lock().unwrap().take() {
			return Err(server_error(code));
		}
		if self.done_fails_transiently {
			return Err(Error::custom(ErrorKind::IO, "connection reset"));
		}
		let uploaded = remote_file_from_upload(
			file.clone(),
			Response {
				uuid: file.uuid(),
				stable_uuid: StableUuid::new_for_test(file.uuid()),
				chunks: completion.num_chunks,
				size: completion.written,
				timestamp: self.now(),
			},
			info,
			completion,
		);
		self.registered.lock().unwrap().push(Registered {
			file: uploaded.clone(),
			upload_key: upload_key.to_owned(),
			completion,
		});
		Ok(uploaded)
	}

	async fn propagate_existing(&self, file: &RemoteFile) -> Result<(), Error> {
		self.propagated.lock().unwrap().push(file.uuid);
		Ok(())
	}
}

/// An in-memory journal store that counts what it is asked.
#[derive(Debug, Default)]
struct RecordingStore {
	journals: Mutex<HashMap<UploadJournalId, SealedUploadJournal>>,
	saves: AtomicUsize,
	removes: AtomicUsize,
	fail_save: AtomicBool,
}

impl RecordingStore {
	fn get(&self, id: &UploadJournalId) -> Option<SealedUploadJournal> {
		self.journals.lock().unwrap().get(id).cloned()
	}

	fn is_empty(&self) -> bool {
		self.journals.lock().unwrap().is_empty()
	}

	fn saves(&self) -> usize {
		self.saves.load(Ordering::Relaxed)
	}
}

impl UploadJournalStore for RecordingStore {
	fn load<'a>(
		&'a self,
		id: &'a UploadJournalId,
	) -> MaybeSendBoxFuture<'a, Result<Option<SealedUploadJournal>, Error>> {
		Box::pin(async move { Ok(self.get(id)) })
	}

	fn save<'a>(
		&'a self,
		id: &'a UploadJournalId,
		journal: &'a SealedUploadJournal,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		Box::pin(async move {
			self.saves.fetch_add(1, Ordering::Relaxed);
			if self.fail_save.load(Ordering::Relaxed) {
				return Err(Error::custom(ErrorKind::IO, "disk full"));
			}
			self.journals.lock().unwrap().insert(*id, journal.clone());
			Ok(())
		})
	}

	fn remove<'a>(&'a self, id: &'a UploadJournalId) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		Box::pin(async move {
			self.removes.fetch_add(1, Ordering::Relaxed);
			self.journals.lock().unwrap().remove(id);
			Ok(())
		})
	}
}

/// `len` bytes that differ from chunk to chunk.
fn contents(len: u64) -> Vec<u8> {
	(0..len)
		.map(|i| u8::try_from((i * 31 + i / 7919) % 251).unwrap())
		.collect()
}

/// A source file of `len` bytes in a temp dir.
fn source(len: u64) -> (TempDir, PathBuf, Vec<u8>) {
	let dir = TempDir::new().unwrap();
	let path = dir.path().join(NAME);
	let contents = contents(len);
	std::fs::write(&path, &contents).unwrap();
	(dir, path, contents)
}

fn journal_id(path: &Path) -> UploadJournalId {
	UploadJournalId::new(path, PARENT, NAME)
}

/// What one upload reported and ended with.
struct Run {
	result: Result<RemoteFile, Error>,
	sent_bytes: u64,
	resumed: Vec<u64>,
}

/// Uploads the file at `path` as a new upload of `uuid`, the way the client resolves a path
/// upload's builder.
async fn run(backend: &FakeResumeBackend, store: &RecordingStore, path: &Path, uuid: u128) -> Run {
	let file = std::fs::File::open(path).unwrap();
	let meta = file.metadata().unwrap();
	let builder = FileBuilder::new_valid_name(
		ValidatedName::try_from(NAME).unwrap(),
		Uuid::from_u128(uuid),
		PARENT,
		&backend.client,
	)
	.created(FilenMetaExt::created(&meta))
	.modified(FilenMetaExt::modified(&meta));
	let sent = Arc::new(AtomicU64::new(0));
	let resumed = Arc::new(Mutex::new(Vec::new()));
	let result = upload_resuming(
		backend,
		ResumeSource {
			builder,
			file,
			meta,
			path: path.to_owned(),
		},
		store,
		ResumeCallbacks {
			callback: Some(Arc::new({
				let sent = Arc::clone(&sent);
				move |bytes| {
					sent.fetch_add(bytes, Ordering::Relaxed);
				}
			}) as MaybeSendCallback<u64>),
			on_resumed: Some(Arc::new({
				let resumed = Arc::clone(&resumed);
				move |bytes| resumed.lock().unwrap().push(bytes)
			}) as MaybeSendCallback<u64>),
		},
	)
	.await
	.map(|(file, _)| file);
	let resumed = resumed.lock().unwrap().clone();
	Run {
		result,
		sent_bytes: sent.load(Ordering::Relaxed),
		resumed,
	}
}

/// Run 1 of an interrupted upload: chunk 7 fails, after chunks 0 to 6 were acknowledged.
async fn interrupted(store: &RecordingStore, path: &Path) {
	let backend = FakeResumeBackend::new().fail_chunk_at(7);
	let run = run(&backend, store, path, 1).await;
	assert_eq!(run.result.unwrap_err().kind(), ErrorKind::IO);
	assert!(store.saves() > 0, "the interrupted upload saved no journal");
}

async fn opened(store: &RecordingStore, path: &Path) -> super::record::UploadJournalRecord {
	let id = journal_id(path);
	let crypter = offline_client().crypter();
	open(&store.get(&id).unwrap(), &id, &*crypter)
		.await
		.unwrap()
}

fn sorted(mut indices: Vec<u64>) -> Vec<u64> {
	indices.sort_unstable();
	indices
}

#[tokio::test(start_paused = true)]
async fn a_file_under_the_threshold_uploads_without_a_journal_and_drops_a_stale_one() {
	let (_dir, path, contents) = source(CHUNK_SIZE_U64 * 7);
	let store = RecordingStore::default();
	store.journals.lock().unwrap().insert(
		journal_id(&path),
		SealedUploadJournal::new(Utc::now(), "stale".to_owned()),
	);
	let backend = FakeResumeBackend::new();

	let run = run(&backend, &store, &path, 1).await;

	let uploaded = run.result.unwrap();
	assert_eq!(uploaded.uuid, Uuid::from_u128(1));
	assert_eq!(
		sorted(backend.sent_indices(uploaded.uuid)),
		(0..7).collect::<Vec<_>>()
	);
	assert_eq!(run.sent_bytes, CHUNK_SIZE_U64 * 7);
	assert_eq!(run.resumed, Vec::<u64>::new());
	assert_eq!(store.saves(), 0);
	assert!(store.is_empty(), "the stale journal was kept");
	let registered = backend.registered.lock().unwrap();
	assert_eq!(
		registered[0].completion.hash,
		Blake3Hash::from(blake3::hash(&contents))
	);
}

#[tokio::test(start_paused = true)]
async fn an_interrupted_upload_resumes_sending_only_the_unacknowledged_chunks() {
	let (_dir, path, contents) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;
	let journaled = opened(&store, &path).await;
	let acked = (0..CHUNKS)
		.filter(|idx| journaled.acked().contains(*idx))
		.collect::<Vec<_>>();
	assert_eq!(acked, (0..7).collect::<Vec<_>>());
	assert_eq!(journaled.uuid(), Uuid::from_u128(1));

	let backend = FakeResumeBackend::new();
	let run = run(&backend, &store, &path, 2).await;

	let uploaded = run.result.unwrap();
	assert_eq!(
		uploaded.uuid,
		Uuid::from_u128(1),
		"the upload was not resumed"
	);
	assert_eq!(
		backend.sent(),
		[7, 8, 9, 10].map(|idx| (Uuid::from_u128(1), idx))
	);
	assert_eq!(run.resumed, [7 * CHUNK_SIZE_U64]);
	assert_eq!(run.sent_bytes, SIZE - 7 * CHUNK_SIZE_U64);
	let registered = backend.registered.lock().unwrap();
	assert_eq!(registered.len(), 1);
	assert_eq!(
		registered[0].completion.hash,
		Blake3Hash::from(blake3::hash(&contents))
	);
	assert_eq!(registered[0].completion.num_chunks, CHUNKS);
	assert_eq!(registered[0].completion.written, SIZE);
	assert_eq!(registered[0].upload_key, journaled.upload_key());
	assert!(store.is_empty(), "the finished upload's journal was kept");
}

#[tokio::test(start_paused = true)]
async fn a_resume_whose_file_already_exists_finishes_without_sending_a_chunk() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;

	let backend = FakeResumeBackend::new().file_exists(Uuid::from_u128(1));
	let run = run(&backend, &store, &path, 2).await;

	assert_eq!(run.result.unwrap().uuid, Uuid::from_u128(1));
	assert_eq!(backend.sent(), []);
	assert_eq!(run.sent_bytes, 0);
	assert_eq!(run.resumed, [SIZE]);
	assert_eq!(*backend.propagated.lock().unwrap(), [Uuid::from_u128(1)]);
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_chunk_answered_as_blacklisted_finishes_with_the_existing_file() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;

	let backend = FakeResumeBackend::new().answer_chunks(ChunkAnswers::BlacklistFinished);
	let run = run(&backend, &store, &path, 2).await;

	assert_eq!(run.result.unwrap().uuid, Uuid::from_u128(1));
	assert_eq!(*backend.propagated.lock().unwrap(), [Uuid::from_u128(1)]);
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert_eq!(run.sent_bytes, 0);
	// The 7 chunks acknowledged before, then the 4 the finished upload holds.
	assert_eq!(run.resumed, [7 * CHUNK_SIZE_U64, SIZE - 7 * CHUNK_SIZE_U64]);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_chunk_refused_as_blacklisted_without_a_finished_file_fails_and_drops_the_journal() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;

	let backend = FakeResumeBackend::new().answer_chunks(ChunkAnswers::BlacklistMissing);
	let run = run(&backend, &store, &path, 2).await;

	let e = run.result.unwrap_err();
	assert_eq!(e.server_code().as_deref(), Some("upload_blacklisted"));
	assert_eq!(*backend.propagated.lock().unwrap(), Vec::<Uuid>::new());
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert_eq!(run.sent_bytes, 0);
	assert_eq!(run.resumed, [7 * CHUNK_SIZE_U64]);
	assert!(store.is_empty(), "the journal was kept");
}

#[tokio::test(start_paused = true)]
async fn a_done_rejected_for_missing_chunks_restarts_once_under_a_new_uuid() {
	let (_dir, path, contents) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;

	let backend = FakeResumeBackend::new().done_rejects_once("one_or_more_chunks_missing");
	let run = run(&backend, &store, &path, 2).await;

	let uploaded = run.result.unwrap();
	assert_ne!(uploaded.uuid, Uuid::from_u128(1));
	assert_ne!(uploaded.uuid, Uuid::from_u128(2));
	assert_eq!(backend.sent_indices(Uuid::from_u128(1)), [7, 8, 9, 10]);
	assert_eq!(
		backend.sent_indices(uploaded.uuid),
		(0..CHUNKS).collect::<Vec<_>>()
	);
	// The resumed pass's bytes and every byte again.
	assert_eq!(run.sent_bytes, SIZE - 7 * CHUNK_SIZE_U64 + SIZE);
	let registered = backend.registered.lock().unwrap();
	assert_eq!(registered.len(), 1);
	assert_eq!(registered[0].file.uuid, uploaded.uuid);
	assert_eq!(
		registered[0].completion.hash,
		Blake3Hash::from(blake3::hash(&contents))
	);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_fresh_upload_whose_done_reports_missing_chunks_fails_without_restarting() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	let backend = FakeResumeBackend::new().done_rejects_once("one_or_more_chunks_missing");

	let run = run(&backend, &store, &path, 1).await;

	let e = run.result.unwrap_err();
	assert_eq!(
		e.server_code().as_deref(),
		Some("one_or_more_chunks_missing")
	);
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert_eq!(backend.sent().len(), 11);
	assert_eq!(
		sorted(backend.sent_indices(Uuid::from_u128(1))),
		(0..CHUNKS).collect::<Vec<_>>()
	);
	assert_eq!(run.sent_bytes, SIZE);
	assert!(store.is_empty(), "the journal was kept");
}

#[tokio::test(start_paused = true)]
async fn a_done_rejected_for_a_wrong_chunk_count_fails_and_drops_the_journal() {
	for code in ["chunk_mismatch", "upload_chunks_not_matching"] {
		let (_dir, path, _) = source(SIZE);
		let store = RecordingStore::default();
		let backend = FakeResumeBackend::new().done_rejects_once(code);

		let run = run(&backend, &store, &path, 1).await;

		let e = run.result.unwrap_err();
		assert_eq!(e.server_code().as_deref(), Some(code));
		assert!(store.saves() > 0, "{code}: no journal was saved");
		assert!(store.is_empty(), "{code}: the journal was kept");
	}
}

#[tokio::test(start_paused = true)]
async fn a_transient_done_failure_keeps_the_journal() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	let backend = FakeResumeBackend::new().done_fails_transiently();

	let run = run(&backend, &store, &path, 1).await;

	assert_eq!(run.result.unwrap_err().kind(), ErrorKind::IO);
	let journaled = opened(&store, &path).await;
	assert_eq!(journaled.uuid(), Uuid::from_u128(1));
	assert_eq!(journaled.acked().count(), CHUNKS);
}

#[tokio::test(start_paused = true)]
async fn a_journal_that_does_not_decrypt_is_removed_and_the_upload_starts_fresh() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;
	// The same record, sealed under another account's master key.
	let record = opened(&store, &path).await;
	let other = MasterKeys::new_from_key(MasterKey::try_from("1".repeat(64)).unwrap());
	let sealed = seal(&record, &other).await.unwrap();
	store
		.journals
		.lock()
		.unwrap()
		.insert(journal_id(&path), sealed);

	let backend = FakeResumeBackend::new();
	let run = run(&backend, &store, &path, 2).await;

	let uploaded = run.result.unwrap();
	assert_eq!(uploaded.uuid, Uuid::from_u128(2));
	assert_eq!(
		backend.sent_indices(uploaded.uuid),
		(0..CHUNKS).collect::<Vec<_>>()
	);
	assert_eq!(run.resumed, Vec::<u64>::new());
	assert_eq!(run.sent_bytes, SIZE);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_failing_store_does_not_fail_the_upload() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	store.fail_save.store(true, Ordering::Relaxed);
	let backend = FakeResumeBackend::new();

	let run = run(&backend, &store, &path, 1).await;

	assert_eq!(run.result.unwrap().uuid, Uuid::from_u128(1));
	assert!(store.saves() > 0, "no save was attempted");
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_successful_upload_removes_its_journal() {
	let (_dir, path, _) = source(CHUNK_SIZE_U64 * 40);
	let store = RecordingStore::default();
	let backend = FakeResumeBackend::new();

	let run = run(&backend, &store, &path, 1).await;

	assert_eq!(run.result.unwrap().uuid, Uuid::from_u128(1));
	// After 16 and 32 chunks, and the whole bitmap before upload/done.
	assert_eq!(store.saves(), 3);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_source_changed_during_the_upload_fails_and_drops_the_journal() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;
	assert!(!store.is_empty());

	let backend = FakeResumeBackend::new().change_at(&path, 8, SourceChange::Grow);
	let run = run(&backend, &store, &path, 2).await;

	let e = run.result.unwrap_err();
	assert_eq!(e.kind(), ErrorKind::FileChangedDuringSync);
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert!(store.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_source_touched_during_the_upload_fails_and_drops_the_journal() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	interrupted(&store, &path).await;
	assert!(!store.is_empty());

	let backend = FakeResumeBackend::new().change_at(&path, 8, SourceChange::Touch);
	let run = run(&backend, &store, &path, 2).await;

	let e = run.result.unwrap_err();
	assert_eq!(e.kind(), ErrorKind::FileChangedDuringSync);
	assert_eq!(
		sorted(backend.sent_indices(Uuid::from_u128(1))),
		[7, 8, 9, 10]
	);
	assert_eq!(backend.registered.lock().unwrap().len(), 0);
	assert!(store.is_empty(), "the journal was kept");
}

#[tokio::test(start_paused = true)]
async fn an_error_from_done_keeps_its_server_code() {
	let (_dir, path, _) = source(SIZE);
	let store = RecordingStore::default();
	let backend = FakeResumeBackend::new().done_rejects_once("max_storage_reached");

	let run = run(&backend, &store, &path, 1).await;

	let e = run.result.unwrap_err();
	assert_eq!(e.kind(), ErrorKind::MaxStorageReached);
	assert_eq!(e.server_code().as_deref(), Some("max_storage_reached"));
	assert_eq!(
		e.server_message().as_deref(),
		Some("the server says max_storage_reached")
	);
	assert!(
		!store.is_empty(),
		"a resumable upload's journal was dropped"
	);
}

#[test]
fn the_resuming_upload_can_be_sent_across_threads() {
	fn assert_send<T: Send>(_: &T) {}
	let client = offline_client();
	let store = RecordingStore::default();
	let upload = client.upload_file_from_path_resuming(
		FileBuilderOptionalName::new(PARENT),
		PathBuf::from(NAME),
		&store,
		None,
		None,
	);
	assert_send(&upload);
}
