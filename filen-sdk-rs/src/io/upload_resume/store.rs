//! Where the journals of resumable uploads are kept: the store trait, and a store over a
//! directory of files.
//!
//! Two processes resuming the same upload at once both send its chunks under the same upload key.
//! The server keeps the last write of each chunk and `upload/done` is idempotent, so the result
//! is the same file either way: no lock file is taken, and opening a store may remove the temp
//! file of a save running in another process at that moment, which then fails.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::{
	borrow::Cow,
	fmt,
	path::{Path, PathBuf},
};

use chrono::{DateTime, SubsecRound as _, Utc};
use filen_types::fs::Uuid;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use super::JOURNAL_MAX_AGE;
use crate::{
	Error, ErrorKind,
	error::ResultExt as _,
	io::private_file::{remove_file_if_present, write_private_file_atomically},
	util::MaybeSendBoxFuture,
};

/// Extension of a journal file in a [`FileUploadJournalStore`].
const JOURNAL_EXT: &str = "filenup";
/// Extension appended to a journal file's name while it is being written.
const TEMP_EXT: &str = "tmp";
/// The journal file format this store writes and reads.
const JOURNAL_FILE_VERSION: u32 = 1;

/// The key a journal is kept under: a BLAKE3 hash of the source path, the parent directory and
/// the name the file is uploaded as. A store sees only this id, never the path or the name.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct UploadJournalId([u8; 32]);

impl UploadJournalId {
	/// The id of the upload of the file at `source` into the directory `parent`, as `name`.
	pub fn new(source: &Path, parent: Uuid, name: &str) -> Self {
		// The zero bytes keep the fields apart: a path holds no zero byte and a uuid is always
		// 16 bytes long, so no two different inputs hash the same bytes.
		let mut hasher = blake3::Hasher::new();
		hasher.update(source.as_os_str().as_encoded_bytes());
		hasher.update(&[0]);
		hasher.update(parent.as_bytes());
		hasher.update(&[0]);
		hasher.update(name.as_bytes());
		Self(*hasher.finalize().as_bytes())
	}

	/// The id's 32 bytes.
	pub fn as_bytes(&self) -> &[u8; 32] {
		&self.0
	}

	/// The id as 64 lowercase hex digits.
	pub fn to_hex(&self) -> String {
		hex::encode(self.0)
	}
}

impl fmt::Debug for UploadJournalId {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_tuple("UploadJournalId")
			.field(&self.to_hex())
			.finish()
	}
}

/// A journal as a store keeps it.
///
/// `sealed` is the journal's record encrypted with the account's metadata crypter (its master
/// keys), so a store holds neither the upload's keys nor its source path in plaintext.
/// `started_at` is when the upload's first chunk was acknowledged, kept in plaintext so that a
/// store can purge old journals by age without the keys.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedUploadJournal {
	started_at: DateTime<Utc>,
	sealed: String,
}

impl SealedUploadJournal {
	/// A journal whose first chunk was acknowledged at `started_at` (kept to the millisecond),
	/// holding the encrypted record `sealed`.
	pub fn new(started_at: DateTime<Utc>, sealed: String) -> Self {
		Self {
			started_at: started_at.trunc_subsecs(3),
			sealed,
		}
	}

	/// When the upload's first chunk was acknowledged, to the millisecond.
	pub fn started_at(&self) -> DateTime<Utc> {
		self.started_at
	}

	/// The encrypted record.
	pub fn sealed(&self) -> &str {
		&self.sealed
	}
}

impl fmt::Debug for SealedUploadJournal {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("SealedUploadJournal")
			.field("started_at", &self.started_at)
			.field("sealed_len", &self.sealed.len())
			.finish()
	}
}

/// Keeps the journals of resumable uploads, one per [`UploadJournalId`].
///
/// An implementation purges, whenever it is opened, every journal whose `started_at` is more than
/// 24 hours old: the server may have dropped that upload's chunks by then.
pub trait UploadJournalStore: fmt::Debug + Send + Sync {
	/// The journal kept under `id`, if there is one.
	fn load<'a>(
		&'a self,
		id: &'a UploadJournalId,
	) -> MaybeSendBoxFuture<'a, Result<Option<SealedUploadJournal>, Error>>;

	/// Keeps `journal` under `id`, replacing the one kept there atomically: a load sees either the
	/// old journal or the new one, never a mix.
	fn save<'a>(
		&'a self,
		id: &'a UploadJournalId,
		journal: &'a SealedUploadJournal,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>>;

	/// Removes the journal kept under `id`. Succeeds when there is none.
	fn remove<'a>(&'a self, id: &'a UploadJournalId) -> MaybeSendBoxFuture<'a, Result<(), Error>>;
}

/// An [`UploadJournalStore`] over a directory: one file per journal, named after its id. On Unix
/// the directory is mode `0o700` and each file `0o600`; on other platforms the files take the
/// directory's permissions. The journals are encrypted with the account's master keys either way.
#[derive(Debug)]
pub struct FileUploadJournalStore {
	dir: PathBuf,
}

impl FileUploadJournalStore {
	/// Opens the store in `dir`, creating the directory if it is missing, and removes the journals
	/// more than 24 hours old, unreadable journal files and temp files left by an interrupted save.
	/// Entries that are not the store's own, directories and symlinks among them, are left alone.
	pub async fn open(dir: PathBuf) -> Result<Self, Error> {
		Self::open_at(dir, Utc::now()).await
	}

	/// [`Self::open`], with `now` as the current time.
	pub(crate) async fn open_at(dir: PathBuf, now: DateTime<Utc>) -> Result<Self, Error> {
		let dir = tokio::task::spawn_blocking(move || {
			prepare_dir(&dir)?;
			purge(&dir, now)?;
			Ok::<_, Error>(dir)
		})
		.await
		.map_err(std::io::Error::from)??;
		Ok(Self { dir })
	}

	fn journal_path(&self, id: &UploadJournalId) -> PathBuf {
		self.dir.join(format!("{}.{JOURNAL_EXT}", id.to_hex()))
	}

	fn temp_path(&self, id: &UploadJournalId) -> PathBuf {
		self.dir
			.join(format!("{}.{JOURNAL_EXT}.{TEMP_EXT}", id.to_hex()))
	}
}

impl UploadJournalStore for FileUploadJournalStore {
	fn load<'a>(
		&'a self,
		id: &'a UploadJournalId,
	) -> MaybeSendBoxFuture<'a, Result<Option<SealedUploadJournal>, Error>> {
		let path = self.journal_path(id);
		Box::pin(async move {
			let (path, read) = tokio::task::spawn_blocking(move || {
				let read = std::fs::read(&path);
				(path, read)
			})
			.await
			.map_err(std::io::Error::from)?;
			let bytes = match read {
				Ok(bytes) => bytes,
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
				Err(e) => {
					return Err(Error::from(e).with_context(format!(
						"failed to read upload journal {}",
						path.display()
					)));
				}
			};
			parse_journal_file(&bytes).map(Some).map_err(|cause| {
				Error::custom_with_source(
					ErrorKind::Conversion,
					MalformedJournalFile { path, cause },
					None::<&str>,
				)
			})
		})
	}

	fn save<'a>(
		&'a self,
		id: &'a UploadJournalId,
		journal: &'a SealedUploadJournal,
	) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		let path = self.journal_path(id);
		let temp = self.temp_path(id);
		let contents = serde_json::to_vec(&JournalFile {
			v: JOURNAL_FILE_VERSION,
			started_at: journal.started_at.timestamp_millis(),
			sealed: Cow::Borrowed(&journal.sealed),
		})
		.expect("a journal file is a number and a string (should be impossible)");
		Box::pin(async move {
			let (path, written) = tokio::task::spawn_blocking(move || {
				let written = write_private_file_atomically(&temp, &path, &contents);
				(path, written)
			})
			.await
			.map_err(std::io::Error::from)?;
			written.context(format!("failed to write upload journal {}", path.display()))
		})
	}

	fn remove<'a>(&'a self, id: &'a UploadJournalId) -> MaybeSendBoxFuture<'a, Result<(), Error>> {
		let path = self.journal_path(id);
		Box::pin(async move {
			let (path, removed) = tokio::task::spawn_blocking(move || {
				let removed = remove_file_if_present(&path);
				(path, removed)
			})
			.await
			.map_err(std::io::Error::from)?;
			removed.context(format!(
				"failed to remove upload journal {}",
				path.display()
			))
		})
	}
}

/// A journal file: `{"v":1,"startedAt":<unix millis>,"sealed":"..."}`.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct JournalFile<'a> {
	v: u32,
	started_at: i64,
	#[serde(borrow)]
	sealed: Cow<'a, str>,
}

/// A journal file that could not be read as one.
#[derive(Debug, Error)]
#[error("malformed upload journal file {}", .path.display())]
struct MalformedJournalFile {
	path: PathBuf,
	#[source]
	cause: MalformedJournalCause,
}

#[derive(Debug, Error)]
enum MalformedJournalCause {
	#[error(transparent)]
	Json(#[from] serde_json::Error),
	#[error("unknown journal file version {0}")]
	UnknownVersion(u32),
	#[error("start time {0} ms is out of range")]
	StartedAtOutOfRange(i64),
}

/// Which of the store's own files a directory entry is.
enum StoreFile {
	Journal,
	Temp,
}

impl StoreFile {
	/// `<64 lowercase hex digits>.filenup` is a journal, the same with `.tmp` appended a temp file
	/// a save writes. Any other name is not the store's.
	fn of(name: &str) -> Option<Self> {
		let (stem, kind) = match name.strip_suffix(TEMP_EXT) {
			Some(journal) => (journal.strip_suffix('.')?, Self::Temp),
			None => (name, Self::Journal),
		};
		let hex = stem.strip_suffix(JOURNAL_EXT)?.strip_suffix('.')?;
		let is_id = hex.len() == 64
			&& hex
				.bytes()
				.all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
		is_id.then_some(kind)
	}
}

fn parse_journal_file(bytes: &[u8]) -> Result<SealedUploadJournal, MalformedJournalCause> {
	let file = serde_json::from_slice::<JournalFile<'_>>(bytes)?;
	if file.v != JOURNAL_FILE_VERSION {
		return Err(MalformedJournalCause::UnknownVersion(file.v));
	}
	let started_at = DateTime::from_timestamp_millis(file.started_at)
		.ok_or(MalformedJournalCause::StartedAtOutOfRange(file.started_at))?;
	Ok(SealedUploadJournal {
		started_at,
		sealed: file.sealed.into_owned(),
	})
}

/// Creates `dir` if it is missing and makes it private to the current user.
fn prepare_dir(dir: &Path) -> Result<(), Error> {
	std::fs::create_dir_all(dir).context(format!(
		"failed to create upload journal directory {}",
		dir.display()
	))?;
	#[cfg(unix)]
	std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).context(format!(
		"failed to make upload journal directory {} private",
		dir.display()
	))?;
	Ok(())
}

/// Removes from `dir` the journals that started more than [`JOURNAL_MAX_AGE`] before `now`, the
/// journal files that cannot be read as one, and leftover temp files.
fn purge(dir: &Path, now: DateTime<Utc>) -> Result<(), Error> {
	// Before the earliest time chrono holds, nothing can be older.
	let cutoff = now.checked_sub_signed(JOURNAL_MAX_AGE);
	let entries = std::fs::read_dir(dir).context(format!(
		"failed to list upload journal directory {}",
		dir.display()
	))?;
	for entry in entries {
		let entry = entry.context(format!(
			"failed to list upload journal directory {}",
			dir.display()
		))?;
		let Some(kind) = entry.file_name().to_str().and_then(StoreFile::of) else {
			continue;
		};
		let path = entry.path();
		// A directory or a symlink with a journal's name is not one the store wrote. The type
		// comes from the listing and does not follow symlinks.
		let file_type = entry.file_type().context(format!(
			"failed to read the type of upload journal directory entry {}",
			path.display()
		))?;
		if !file_type.is_file() {
			continue;
		}
		let stale = match kind {
			StoreFile::Temp => true,
			StoreFile::Journal => match std::fs::read(&path) {
				Ok(bytes) => match parse_journal_file(&bytes) {
					Ok(journal) => cutoff.is_some_and(|cutoff| journal.started_at < cutoff),
					Err(cause) => {
						warn!(
							"removing the unreadable upload journal file {}: {cause}",
							path.display()
						);
						true
					}
				},
				// Removed since it was listed.
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
				Err(e) => {
					return Err(Error::from(e).with_context(format!(
						"failed to read upload journal {}",
						path.display()
					)));
				}
			},
		};
		if stale {
			remove_file_if_present(&path).context(format!(
				"failed to remove upload journal file {}",
				path.display()
			))?;
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	#[cfg(unix)]
	use std::os::unix::fs::PermissionsExt as _;

	use chrono::TimeDelta;
	use tempfile::TempDir;

	use super::*;

	const ID_HEX: &str = "dc77b88922dd8b47b7f17be101d73af2289b445b63e97d3369e37901f2f927fe";

	fn id() -> UploadJournalId {
		UploadJournalId::new(
			Path::new("/home/user/video.mp4"),
			Uuid::from_u128(1),
			"video.mp4",
		)
	}

	fn time(millis: i64) -> DateTime<Utc> {
		DateTime::from_timestamp_millis(millis).unwrap()
	}

	fn journal(started_at: DateTime<Utc>) -> SealedUploadJournal {
		SealedUploadJournal::new(started_at, "sealed record".to_owned())
	}

	async fn open(dir: &TempDir) -> FileUploadJournalStore {
		FileUploadJournalStore::open(dir.path().to_owned())
			.await
			.unwrap()
	}

	fn file_names(dir: &Path) -> Vec<String> {
		let mut names = std::fs::read_dir(dir)
			.unwrap()
			.map(|entry| entry.unwrap().file_name().into_string().unwrap())
			.collect::<Vec<_>>();
		names.sort();
		names
	}

	fn journal_file_name(id: &UploadJournalId) -> String {
		format!("{}.filenup", id.to_hex())
	}

	#[tokio::test]
	async fn a_saved_journal_loads_back() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		let saved = journal(time(1_700_000_000_123));
		store.save(&id(), &saved).await.unwrap();
		let loaded = store.load(&id()).await.unwrap().unwrap();
		assert_eq!(loaded.started_at(), time(1_700_000_000_123));
		assert_eq!(loaded.sealed(), "sealed record");
		assert_eq!(
			std::fs::read_to_string(dir.path().join(journal_file_name(&id()))).unwrap(),
			r#"{"v":1,"startedAt":1700000000123,"sealed":"sealed record"}"#
		);
	}

	#[tokio::test]
	async fn a_missing_journal_loads_as_none() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		assert_eq!(store.load(&id()).await.unwrap(), None);
	}

	#[tokio::test]
	async fn saving_again_replaces_the_journal_and_leaves_no_temp_file() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		store.save(&id(), &journal(time(1_000))).await.unwrap();
		let replacement = SealedUploadJournal::new(time(2_000), "replacement".to_owned());
		store.save(&id(), &replacement).await.unwrap();
		assert_eq!(store.load(&id()).await.unwrap(), Some(replacement));
		assert_eq!(file_names(dir.path()), [journal_file_name(&id())]);
	}

	#[tokio::test]
	async fn removing_a_journal_removes_its_file() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		store.save(&id(), &journal(time(1_000))).await.unwrap();
		store.remove(&id()).await.unwrap();
		assert_eq!(store.load(&id()).await.unwrap(), None);
		assert_eq!(file_names(dir.path()), Vec::<String>::new());
	}

	#[tokio::test]
	async fn removing_a_missing_journal_succeeds() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		store.remove(&id()).await.unwrap();
	}

	#[cfg(unix)]
	#[tokio::test]
	async fn journal_files_and_their_directory_are_private_to_the_owner() {
		let parent = TempDir::new().unwrap();
		let dir = parent.path().join("uploads");
		let store = FileUploadJournalStore::open(dir.clone()).await.unwrap();
		store.save(&id(), &journal(time(1_000))).await.unwrap();
		let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
		assert_eq!(mode(&dir), 0o700);
		assert_eq!(mode(&dir.join(journal_file_name(&id()))), 0o600);
	}

	#[tokio::test]
	async fn opening_keeps_a_journal_exactly_a_day_old_and_removes_one_a_millisecond_older() {
		let dir = TempDir::new().unwrap();
		let now = time(1_700_000_000_000);
		let day_old = UploadJournalId::new(Path::new("/a"), Uuid::from_u128(1), "a");
		let older = UploadJournalId::new(Path::new("/b"), Uuid::from_u128(1), "b");
		let store = open(&dir).await;
		store
			.save(&day_old, &journal(now - TimeDelta::hours(24)))
			.await
			.unwrap();
		store
			.save(
				&older,
				&journal(now - TimeDelta::hours(24) - TimeDelta::milliseconds(1)),
			)
			.await
			.unwrap();

		let store = FileUploadJournalStore::open_at(dir.path().to_owned(), now)
			.await
			.unwrap();
		assert_eq!(
			store.load(&day_old).await.unwrap(),
			Some(journal(now - TimeDelta::hours(24)))
		);
		assert_eq!(store.load(&older).await.unwrap(), None);
		assert_eq!(file_names(dir.path()), [journal_file_name(&day_old)]);
	}

	#[tokio::test]
	async fn opening_removes_an_unparsable_journal_file_and_a_leftover_temp_file() {
		let dir = TempDir::new().unwrap();
		std::fs::write(dir.path().join(journal_file_name(&id())), "not json").unwrap();
		std::fs::write(
			dir.path().join(format!("{}.filenup.tmp", id().to_hex())),
			"half written",
		)
		.unwrap();
		open(&dir).await;
		assert_eq!(file_names(dir.path()), Vec::<String>::new());
	}

	#[tokio::test]
	async fn opening_leaves_files_that_are_not_journals_alone() {
		let dir = TempDir::new().unwrap();
		let others = [
			"notes.txt",
			"abc.filenup",
			"ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789.filenup",
			&format!("{}.filenup.bak", id().to_hex()),
			&format!("{}.tmp", id().to_hex()),
		];
		for name in others {
			std::fs::write(dir.path().join(name), "not json").unwrap();
		}
		open(&dir).await;
		let mut expected = others.map(str::to_owned).to_vec();
		expected.sort();
		assert_eq!(file_names(dir.path()), expected);
	}

	#[tokio::test]
	async fn opening_leaves_directories_named_like_journals_alone() {
		let dir = TempDir::new().unwrap();
		let names = [
			journal_file_name(&id()),
			format!("{}.filenup.tmp", id().to_hex()),
		];
		for name in &names {
			std::fs::create_dir(dir.path().join(name)).unwrap();
		}
		open(&dir).await;
		let mut expected = names.to_vec();
		expected.sort();
		assert_eq!(file_names(dir.path()), expected);
	}

	#[tokio::test]
	async fn loading_a_malformed_journal_file_fails() {
		let dir = TempDir::new().unwrap();
		let store = open(&dir).await;
		let path = dir.path().join(journal_file_name(&id()));
		std::fs::write(&path, r#"{"v":2,"startedAt":1000,"sealed":"x"}"#).unwrap();
		let e = store.load(&id()).await.unwrap_err();
		assert_eq!(e.kind(), ErrorKind::Conversion);
		let malformed = e.downcast_ref::<MalformedJournalFile>().unwrap();
		assert_eq!(malformed.path, path);
		assert!(matches!(
			malformed.cause,
			MalformedJournalCause::UnknownVersion(2)
		));
	}

	#[test]
	fn the_journal_id_is_stable_for_the_same_inputs() {
		assert_eq!(id().to_hex(), ID_HEX);
		assert_eq!(id(), id());
		assert_eq!(
			format!("{:?}", id()),
			format!("UploadJournalId(\"{ID_HEX}\")")
		);
	}

	#[test]
	fn the_journal_id_differs_by_path_parent_and_name() {
		let path = Path::new("/home/user/video.mp4");
		assert_ne!(
			UploadJournalId::new(
				Path::new("/home/user/other.mp4"),
				Uuid::from_u128(1),
				"video.mp4"
			),
			id()
		);
		assert_ne!(
			UploadJournalId::new(path, Uuid::from_u128(2), "video.mp4"),
			id()
		);
		assert_ne!(
			UploadJournalId::new(path, Uuid::from_u128(1), "other.mp4"),
			id()
		);
	}
}
