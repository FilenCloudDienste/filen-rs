use std::{
	fs::FileTimes,
	io::{self},
	path::{Path, PathBuf},
	str::FromStr,
	sync::Arc,
	time::{Duration, SystemTime},
};

use crate::{
	auth::{
		AUTH_CLEANUP_INTERVAL, AuthCacheState, AuthStatus, CacheState, DB_FILE_NAME,
		FilenMobileCacheState, UnauthCacheState, UnauthReason,
		update_saved_db_state_cache_cleanup_time,
	},
	sql::{self, error::OptionalExtensionSQL, file::DBFile, item::RawDBItem},
	traits::ProgressCallback,
};
use chrono::{DateTime, Utc};
use filen_sdk_rs::{
	error::FilenSdkError,
	fs::{
		HasName, HasUUID,
		file::{FileBuilderOptionalName, RemoteFile, traits::HasFileInfo},
	},
	io::{FilenMetaExt, client_impl::IoSharedClientExt, is_safe_descendant},
};
use filen_types::{
	crypto::Blake3Hash,
	fs::{Uuid, UuidStr},
};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{
	fs::DirEntry,
	sync::{OwnedRwLockReadGuard, mpsc::UnboundedReceiver},
};
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::{debug, error, info, trace};

#[cfg(windows)]
fn get_file_times(created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	use std::os::windows::fs::FileTimesExt;
	let mut times = FileTimes::new();
	if let Some(created) = created {
		times = times.set_created(created);
	}
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	}
	times
}

#[cfg(unix)]
fn get_file_times(_created: Option<SystemTime>, modified: Option<SystemTime>) -> FileTimes {
	let mut times = FileTimes::new();
	if let Some(modified) = modified {
		times = times.set_modified(modified);
	}
	times
}

pub const CACHE_DIR: &str = "cache";
const TMP_DIR: &str = "tmp";
const THUMBNAIL_DIR: &str = "thumbnails";

const CALLBACK_INTERVAL: Duration = Duration::from_millis(200);

pub(crate) fn get_paths(files_path: &Path) -> (PathBuf, PathBuf, PathBuf) {
	let cache_dir = files_path.join(CACHE_DIR);
	let tmp_dir = files_path.join(TMP_DIR);
	let thumbnail_dir = files_path.join(THUMBNAIL_DIR);
	(cache_dir, tmp_dir, thumbnail_dir)
}

pub(crate) fn init(files_path: &Path) -> Result<(PathBuf, PathBuf, PathBuf), io::Error> {
	let (cache_dir, tmp_dir, thumbnail_dir) = get_paths(files_path);
	std::fs::create_dir_all(&cache_dir)?;
	std::fs::create_dir_all(&tmp_dir)?;
	std::fs::create_dir_all(&thumbnail_dir)?;
	Ok((cache_dir, tmp_dir, thumbnail_dir))
}

/// Removes everything on disk belonging to the signed-in account — the ONE definition of that
/// set, shared by every DB re-init (`auth::init_db`: hash/version/owner mismatch, corruption)
/// and by the confirmed-disable wipe (via `spawn_blocking`):
///
/// - the cache/tmp/thumbnail dirs under `files_dir` — decrypted bytes, staged uploads,
///   thumbnails;
/// - the native cache DB and the SDK search cache (which lives next to it at `db_dir`, ==
///   `files_dir` unless the platform relocated it, see `CacheState::db_dir`), each with its
///   `-wal`/`-shm` sidecars: a stale `-wal` surviving next to a recreated DB is replayed into
///   it on open (sqlite.org/howtocorrupt.html §4.4), and both WALs carry the most recent
///   decrypted names.
///
/// Callers must hold no open connection to either DB. Keeps going after a failure (NotFound is
/// not one) so a wedged path removes as much as it can, and returns the first error so the
/// re-init path can refuse to build on a half-wiped state.
pub(crate) fn wipe_account_data(files_dir: &Path, db_dir: &Path) -> io::Result<()> {
	let (cache_dir, tmp_dir, thumbnail_dir) = get_paths(files_dir);
	let mut first_err = None;
	let mut note = |res: io::Result<()>, path: &Path, what: &str| match res {
		Ok(()) => info!("Removed {what}: {}", path.display()),
		Err(e) if e.kind() == io::ErrorKind::NotFound => {}
		Err(e) => {
			error!("Failed to remove {what} {}: {e}", path.display());
			first_err.get_or_insert(e);
		}
	};
	for dir in [&cache_dir, &tmp_dir, &thumbnail_dir] {
		note(std::fs::remove_dir_all(dir), dir, "account directory");
	}
	for db in [DB_FILE_NAME, crate::search::SDK_CACHE_DB_NAME] {
		for suffix in ["", "-wal", "-shm"] {
			let mut os = db_dir.join(db).into_os_string();
			os.push(suffix);
			let path = PathBuf::from(os);
			note(std::fs::remove_file(&path), &path, "database file");
		}
	}
	match first_err {
		None => Ok(()),
		Some(e) => Err(e),
	}
}

pub(crate) async fn update_task(
	mut receiver: UnboundedReceiver<u64>,
	file_size: u64,
	callback: Arc<dyn ProgressCallback + Send + Sync>,
) {
	callback.set_total(file_size);
	let mut last_update = SystemTime::now();
	let mut written_since_update = 0;
	loop {
		tokio::select! {
			bytes_written = receiver.recv() => {
				match bytes_written {
					Some(bytes) => {
						written_since_update += bytes;
						let now = SystemTime::now();
						if now.duration_since(last_update).expect("Impossible time comparison") > CALLBACK_INTERVAL {
							callback.on_progress(written_since_update);
							written_since_update = 0;
							last_update = now;
						}
					},
					None => {
						if written_since_update > 0 {
							callback.on_progress(written_since_update);
						}
						break;
					},
				}
			},
			_ = tokio::time::sleep(CALLBACK_INTERVAL) => {
				if written_since_update > 0 {
					callback.on_progress(written_since_update);
					last_update = SystemTime::now();
					written_since_update = 0;
				}
			}
		}
	}
}

/// `cache_dir/<uuid>/<name>`. The name is decrypted, so whoever wrote the item (a sharer
/// included) controls it: one that is not a single path component (`..`, `/abs`, `a/b`, empty)
/// would escape `cache_dir` once joined, so it falls back to the uuid like an undecryptable
/// (`None`) name does.
fn cached_file_path(cache_dir: &Path, uuid: &str, name: Option<&str>) -> PathBuf {
	let name = name.filter(|n| is_safe_descendant(n)).unwrap_or(uuid);
	cache_dir.join(uuid).join(name)
}

impl AuthCacheState {
	// we use the uuid as the folder and the actual name of the file because otherwise we run into a bug in IOS
	// where files get shared as a full UUID

	pub(crate) fn get_cached_file_path_from_name(&self, uuid: &str, name: Option<&str>) -> PathBuf {
		cached_file_path(&self.cache_dir, uuid, name)
	}

	pub(crate) fn get_cached_file_path(&self, file: &RemoteFile) -> PathBuf {
		self.get_cached_file_path_from_name(&file.uuid().to_string(), file.name())
	}

	pub async fn download_file_io(
		&self,
		file: &RemoteFile,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<PathBuf, io::Error> {
		let src = self.tmp_dir.join(file.uuid().to_string());
		let mut os_file = tokio::fs::File::create(&src).await?.compat_write();
		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
		let file_size = file.size();
		let callback: Option<Arc<dyn Fn(u64) + Send + Sync + 'static>> =
			if let Some(callback) = callback {
				tokio::task::spawn(async move {
					update_task(receiver, file_size, callback).await;
				});
				Some(Arc::new(move |bytes_written: u64| {
					let _ = sender.send(bytes_written);
				}) as Arc<dyn Fn(u64) + Send + Sync>)
			} else {
				None
			};
		self.client
			.download_file_to_writer(file, &mut os_file, callback)
			.await
			.map_err(|e| io::Error::other(format!("Failed to download file: {e}")))?;
		let os_file = os_file.into_inner().into_std().await;
		let created = file.created().map(Into::into);
		let modified = file.last_modified().map(Into::into);
		tokio::task::spawn_blocking(move || {
			os_file
				.set_times(get_file_times(created, modified))
				.map_err(io::Error::other)
		})
		.await??;

		let dst = self.get_cached_file_path(file);
		let parent = dst
			.parent()
			.expect("cached file path parent should always exist");
		// Known, accepted window: these are `spawn_blocking` underneath and a started blocking
		// task cannot be aborted, so a cancelled download (UniFFI drops the whole future) releases
		// the caller's per-item guard while the rename is still in flight. A clear that takes the
		// freed lock can then be undone by the rename landing after it, leaving a cache entry for
		// a file that was just cleared. It self-heals: the next open hash-checks the cached copy
		// before serving it. Closing it properly means moving this tail into a spawned task that
		// owns the guard and awaiting its handle, which is why it has not been done inline here.
		tokio::fs::create_dir_all(parent).await?;
		tokio::fs::rename(&src, &dst).await?;
		// The bytes are in the cache directory now. Recorded here rather than at the callers so
		// that every download covers itself — the thumbnail path materialises a file too.
		self.record_materialised(file.uuid());
		Ok(dst)
	}

	/// Puts bytes from outside the cache into a file's slot, replacing whatever is there.
	///
	/// The item's lock must already be held: this is the same slot a download writes and a clear
	/// removes. Staged through the tmp directory and renamed in, exactly like a download — in the
	/// slot, a copy interrupted halfway IS the item's content, and the caller marks the edit as
	/// outstanding before calling this, so a truncated file is what the drain would then send to
	/// the server on top of a version that was fine.
	pub(crate) async fn io_import_cached_file(
		&self,
		uuid: Uuid,
		name: Option<&str>,
		src: &Path,
	) -> Result<(), io::Error> {
		let staged = self.tmp_dir.join(uuid.to_string());
		tokio::fs::copy(src, &staged).await?;
		let dst = self.get_cached_file_path_from_name(&uuid.to_string(), name);
		let parent = dst
			.parent()
			.expect("cached file path parent should always exist");
		tokio::fs::create_dir_all(parent).await?;
		if let Err(e) = tokio::fs::rename(&staged, &dst).await {
			// Nothing owns the staged copy once the rename has refused it: the tmp sweep only
			// reaps uuids the database does not know, and this one it does.
			if let Err(e) = tokio::fs::remove_file(&staged).await {
				tracing::warn!("Failed to remove staged copy {}: {}", staged.display(), e);
			}
			return Err(e);
		}
		self.record_materialised(uuid);
		Ok(())
	}

	/// Records that this file's bytes are in the cache directory, as of now.
	///
	/// Deliberately infallible: the caller has already written the bytes, and a database error
	/// must not turn a download that succeeded into a failure. The marker is device-local state,
	/// and [`AuthCacheState::reconcile_materialised`] rebuilds it from the directory itself on the
	/// next cleanup.
	pub(crate) fn record_materialised(&self, uuid: Uuid) {
		if let Err(e) =
			sql::mark_materialised(&self.conn(), uuid, chrono::Utc::now().timestamp_millis())
		{
			error!("Failed to record the cached copy of {uuid}: {e}");
		}
	}

	/// Drops the record once the bytes are gone. Infallible for the same reason: the bytes are
	/// already deleted, and the reconciliation pass is the backstop.
	pub(crate) fn drop_materialised(&self, uuid: Uuid) {
		if let Err(e) = sql::clear_materialised(&self.conn(), uuid) {
			error!("Failed to drop the cached-copy record of {uuid}: {e}");
		}
	}

	pub async fn hash_local_file(
		&self,
		file_uuid: Uuid,
		file_name: Option<&str>,
	) -> Result<Option<Blake3Hash>, io::Error> {
		let path = self.get_cached_file_path_from_name(&file_uuid.to_string(), file_name);
		tokio::task::spawn_blocking(move || {
			let mut hasher = blake3::Hasher::new();
			match hasher.update_mmap_rayon(&path) {
				Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
				Err(e) => Err(e),
				Ok(_) => Ok(Some(hasher.finalize().into())),
			}
		})
		.await
		.unwrap()
	}

	pub(crate) async fn io_upload_file(
		&self,
		path: PathBuf,
		builder: FileBuilderOptionalName,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<(RemoteFile, std::fs::File), FilenSdkError> {
		let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<u64>();
		// redundant metadata call, we do it again in upload_file_from_path, but we need the size here
		// annoying to work around
		let file_size = FilenMetaExt::size(&tokio::fs::metadata(&path).await?);

		let reader_callback = if let Some(callback) = callback {
			tokio::task::spawn(async move {
				update_task(receiver, file_size, callback).await;
			});
			Some(Arc::new(move |bytes_written: u64| {
				let _ = sender.send(bytes_written);
			}) as Arc<dyn Fn(u64) + Send + Sync>)
		} else {
			None
		};

		self.client
			.upload_file_from_path_with_builder(builder, path, reader_callback)
			.await
	}

	/// Uploads the edited local copy and moves it under the uuid the server minted for it.
	///
	/// The returned guard covers the *new* uuid and must be held until that uuid reaches the
	/// database: the rename below puts a directory on disk that nothing knows about yet, and the
	/// cache sweep deletes exactly those. Dropping it before the row is written reopens that
	/// window.
	///
	/// It is `None` when the server handed back the uuid we already had. The caller holds that
	/// uuid's lock for the duration of the edit, and these locks are not reentrant, so taking it
	/// again here would hang the task forever.
	pub(crate) async fn io_upload_updated_file(
		&self,
		old_uuid: Uuid,
		name: String,
		parent_uuid: Uuid,
		mime: String,
		callback: Option<Arc<dyn ProgressCallback>>,
	) -> Result<(RemoteFile, Option<tokio::sync::OwnedMutexGuard<()>>), FilenSdkError> {
		let old_path = self.get_cached_file_path_from_name(&old_uuid.to_string(), Some(&name));

		let mut file_builder = FileBuilderOptionalName::new(parent_uuid);
		file_builder.name(&name)?;
		file_builder.mime(mime);
		let (file, _) = self
			.io_upload_file(old_path.clone(), file_builder, callback)
			.await?;
		let new_uuid_guard = if file.uuid() == old_uuid {
			None
		} else {
			Some(self.lock_local_file(file.uuid()).await)
		};
		let new_path = self.get_cached_file_path(&file);
		let parent = new_path
			.parent()
			.expect("cached file path parent should always exist");
		tokio::fs::create_dir_all(parent).await?;
		tokio::fs::rename(&old_path, &new_path).await?;
		if let Some(parent) = old_path.parent()
			&& let Err(e) = tokio::fs::remove_dir_all(parent).await
		{
			tracing::warn!(
				"Failed to remove old parent directory {}: {}",
				parent.display(),
				e
			)
		};
		Ok((file, new_uuid_guard))
	}

	/// Uploads a brand-new file from its cache slot.
	///
	/// The returned guard covers the slot this creates on disk and must be held until that uuid
	/// reaches the database. It is taken BEFORE the slot exists rather than after the upload: the
	/// directory is on disk for the whole upload with no row to justify it, which is exactly what
	/// the cache sweep deletes.
	pub(crate) async fn io_upload_new_file(
		&self,
		builder: FileBuilderOptionalName,
	) -> Result<(RemoteFile, PathBuf, tokio::sync::OwnedMutexGuard<()>), FilenSdkError> {
		// Keyed on the builder's uuid, which is the one the slot below is named after. The upload
		// response names the file's current version; should the two ever diverge, the slot here is
		// genuinely orphaned and the sweep is right to take it.
		let uuid_guard = self.lock_local_file(builder.get_uuid()).await;
		let target_path = self
			.get_cached_file_path_from_name(&builder.get_uuid().to_string(), builder.get_name());
		let parent_path = target_path
			.parent()
			.expect("cached file path should always have a parent");
		tokio::fs::create_dir_all(parent_path).await?;
		let os_file = tokio::fs::OpenOptions::new()
			.read(true)
			.append(true) // only for create
			.create(true)
			.open(&target_path)
			.await?;
		drop(os_file);
		let (file, _) = self
			.io_upload_file(target_path.clone(), builder, None)
			.await?;
		Ok((file, target_path, uuid_guard))
	}

	/// Reunites cache slots with rows whose uuid a remote edit re-minted (see [`SlotRescue`]).
	/// Called before anything that acts on where a file's bytes are: the drain, the cleanup
	/// sweeps, and the download freshness check.
	pub(crate) async fn rescue_reminted_slots(&self) {
		self.slot_rescue().rescue_all().await;
	}

	pub(crate) async fn io_delete_local(&self, uuid: Uuid) -> Result<(), io::Error> {
		// Serialised against a concurrent download of the same item: otherwise a clear issued just
		// before a re-request can land after the fresh download and evict it. Taken here rather
		// than at each of the callers so every deletion path is covered at one choke point.
		let _local_file_guard = self.lock_local_file(uuid).await;
		let path = self.cache_dir.join(uuid.to_string());
		if let Err(e) = match tokio::fs::metadata(&path).await {
			Ok(meta) => {
				if meta.is_dir() {
					tokio::fs::remove_dir_all(&path).await
				} else if meta.is_file() || meta.is_symlink() {
					tokio::fs::remove_file(&path).await
				} else {
					tracing::warn!(
						"Path {} is neither file nor directory, cannot delete",
						path.display()
					);
					Ok(())
				}
			}
			Err(e) => Err(e),
		} && e.kind() != io::ErrorKind::NotFound
		{
			return Err(e);
		}
		// The slot is gone, so the row must stop claiming a local copy: the working set is built on
		// that claim, and a stale marker keeps an item this device no longer holds inside it.
		self.drop_materialised(uuid);
		Ok(())
	}
}

/// Drains the slot-remint log by renaming cache slots into place.
///
/// The stable tier of `upsert_item` re-files a row under the uuid the server minted for a remote
/// content edit, but the bytes on disk stay in the slot named by the OLD uuid — so a row holding
/// an unsent local edit loses track of the only copy of that edit: the identity sweep sees an
/// orphan slot, and the drain sees a marked row with "no local copy". The log (written by the
/// per-connection trigger, durable across restarts) records each (old, new) pair; this moves the
/// slots to follow their rows.
///
/// Its own struct, rather than methods on [`AuthCacheState`], so the whole decision table is
/// testable against a temp dir and an in-memory database — the state cannot be built without a
/// live client.
pub(crate) struct SlotRescue<'a> {
	pub(crate) conn: &'a std::sync::Mutex<rusqlite::Connection>,
	pub(crate) cache_dir: &'a Path,
	pub(crate) locks: &'a crate::file_locks::FileLocks,
}

impl SlotRescue<'_> {
	fn conn(&self) -> std::sync::MutexGuard<'_, rusqlite::Connection> {
		self.conn
			.lock()
			.unwrap_or_else(|poisoned| poisoned.into_inner())
	}

	/// Best-effort per entry: one failed rescue is retried on the next call, and must not stop
	/// the rest. Entries are processed oldest-first, which is what resolves a CHAIN of re-mints
	/// (a → b, then b → c) as two sequential renames.
	pub(crate) async fn rescue_all(&self) {
		let entries = match sql::select_slot_remints(&self.conn()) {
			Ok(entries) => entries,
			Err(e) => {
				error!("Failed to read the slot-remint log: {e}");
				return;
			}
		};
		for (id, old_uuid, new_uuid) in entries {
			if let Err(e) = self.rescue_one(id, old_uuid, new_uuid).await {
				tracing::warn!("Failed to rescue reminted slot {old_uuid} -> {new_uuid}: {e}");
			}
		}
	}

	async fn rescue_one(
		&self,
		entry_id: i64,
		old_uuid: Uuid,
		new_uuid: Uuid,
	) -> Result<(), io::Error> {
		// Both slots are mutated; take both locks in OLD-THEN-NEW order — the same order the
		// edit-upload path holds them in (`upload_edited_file` holds the old uuid's lock while
		// `io_upload_updated_file` takes the freshly minted one), so a rescue racing an upload
		// whose own echo logged the remint queues behind it instead of deadlocking against it.
		// Rescuers cannot deadlock each other: a chain's entries only ever advance (old → new),
		// never cycle back to a retired uuid.
		let _old_guard = self.locks.lock(old_uuid).await;
		let _new_guard = if old_uuid == new_uuid {
			None
		} else {
			Some(self.locks.lock(new_uuid).await)
		};

		let drop_entry = |why: &str| {
			trace!("Dropping slot-remint entry {old_uuid} -> {new_uuid}: {why}");
			if let Err(e) = sql::delete_slot_remint(&self.conn(), entry_id) {
				error!("Failed to drop slot-remint entry {entry_id}: {e}");
			}
		};

		let old_dir = self.cache_dir.join(old_uuid.to_string());
		// `?`, never unwrap_or(false): a stat FAILURE (as opposed to a definite absence) must
		// keep the entry — dropping it disarms the sweep guard over what may be the only copy
		// of an unsent edit, which is exactly the loss the log exists to prevent.
		if !tokio::fs::try_exists(&old_dir).await? {
			// Nothing left to move — the bytes were delivered (the upload renames the slot
			// itself), cleared, or already rescued by the other process.
			drop_entry("old slot is gone");
			return Ok(());
		}

		// Where the bytes belong now, per the row the remint landed on. Read under the locks so
		// a concurrent edit cannot move the marker mid-decision. A read ERROR keeps the entry
		// for the next pass, for the same reason as the stat above.
		let row = {
			let conn = self.conn();
			DBFile::select(&conn, new_uuid)
				.optional()
				.map_err(|e| io::Error::other(format!("reading the reminted row: {e}")))?
		};
		let new_dir = self.cache_dir.join(new_uuid.to_string());
		let Some(file) = row else {
			// No row under the new uuid. Either the uuid re-minted AGAIN — a later log entry
			// carries (new → newer), and a plain rename here hands the bytes to that entry — or
			// the row is gone for good and the slot is the sweep's to take.
			// Re-read, not the snapshot `rescue_all` is iterating: that one predates every
			// rename this pass has done. `?`, never unwrap_or(false), for the same reason as
			// the stat and the row read above — a read FAILURE would drop the entry and disarm
			// the sweep guard over what may be the only copy of an unsent edit.
			let chained = sql::select_slot_remints(&self.conn())
				.map(|entries| entries.iter().any(|(_, old, _)| *old == new_uuid))
				.map_err(|e| io::Error::other(format!("reading the slot-remint log: {e}")))?;
			if !chained {
				drop_entry("no row under the new uuid and no chained remint");
				return Ok(());
			}
			if tokio::fs::try_exists(&new_dir).await.unwrap_or(false) {
				// The chain target already has bytes of its own; ours are the older state.
				tokio::fs::remove_dir_all(&old_dir).await?;
			} else {
				tokio::fs::rename(&old_dir, &new_dir).await?;
			}
			drop_entry("handed to the chained remint");
			return Ok(());
		};

		if tokio::fs::try_exists(&new_dir).await.unwrap_or(false) {
			if file.pending_upload_at.is_some() {
				// A download of the new head landed next to the unsent edit. The marker says the
				// server is missing the edit, and the drain uploads whatever is in the slot — so
				// the edit's bytes outrank the server copy.
				tokio::fs::remove_dir_all(&new_dir).await?;
			} else {
				// No outstanding edit: the freshly-downloaded head is the right content, and the
				// old slot is a stale copy of a superseded version.
				tokio::fs::remove_dir_all(&old_dir).await?;
				drop_entry("edit resolved; kept the fresh slot");
				return Ok(());
			}
		}
		tokio::fs::rename(&old_dir, &new_dir).await?;

		// The slot layout is cache/<uuid>/<name>, and every read computes <name> from the row's
		// current metadata — so the inner file has to follow the row's name too, or the rescued
		// bytes are invisible to `hash_local_file`. Best-effort: a failure leaves the bytes safe
		// under the right uuid, degraded to the same miss a remote rename leaves behind.
		if let crate::sql::DBFileMeta::Decoded(meta) = &file.meta {
			let target = new_dir.join(&meta.name);
			if let Err(e) = rename_single_slot_file(&new_dir, &target).await {
				tracing::warn!(
					"Failed to rename the rescued file in {} to its current name: {e}",
					new_dir.display()
				);
			}
		}
		// The bytes are in the cache directory under their row's uuid again.
		if let Err(e) = sql::mark_materialised(
			&self.conn(),
			new_uuid,
			chrono::Utc::now().timestamp_millis(),
		) {
			error!("Failed to record the rescued copy of {new_uuid}: {e}");
		}
		drop_entry("rescued");
		Ok(())
	}
}

/// Renames the single regular file inside a rescued slot to `target` (see the caller). A slot
/// holds exactly one file by construction; anything else is left for `process_subdir` to judge.
async fn rename_single_slot_file(slot_dir: &Path, target: &Path) -> Result<(), io::Error> {
	let mut dir = tokio::fs::read_dir(slot_dir).await?;
	while let Some(entry) = dir.next_entry().await? {
		if entry.file_type().await?.is_file() {
			if entry.path() != target {
				tokio::fs::rename(entry.path(), target).await?;
			}
			return Ok(());
		}
	}
	Ok(())
}

async fn cleanup_uuid_dir(auth_state: &AuthCacheState, dir_path: &Path) {
	let Ok(mut dir) = tokio::fs::read_dir(dir_path).await else {
		tracing::warn!(
			"Tried to clean up directory {}, but it does not exist.",
			dir_path.display()
		);
		return;
	};

	let mut uuids: Vec<(UuidStr, DirEntry)> = Vec::new();

	loop {
		match dir.next_entry().await {
			Ok(Some(entry)) => {
				if let Ok(uuid) = UuidStr::from_str(&entry.file_name().to_string_lossy()) {
					uuids.push((uuid, entry));
				}
			}
			Ok(None) => break,
			Err(e) => {
				error!("Failed to read directory {}: {}", dir_path.display(), e);
				return;
			}
		}
	}

	let Ok(removed_uuid_positions) =
		sql::select_positions_not_in_uuids(&auth_state.conn(), uuids.iter().map(|(uuid, _)| *uuid))
	else {
		error!(
			"Failed to select positions not in uuids for directory {}",
			dir_path.display()
		);
		return;
	};

	let mut futures = FuturesUnordered::new();

	for i in removed_uuid_positions {
		let (uuid, entry) = &uuids[i];
		futures.push(async move {
			let path = entry.path();
			// The listing above and the query that decided this uuid is unknown both happened
			// before now, so an item materialised in between would be deleted while live. Re-check
			// under the item's own lock, which is what every other deletion path holds.
			let _local_file_guard = auth_state.lock_local_file(Uuid::from(*uuid)).await;
			match RawDBItem::select(&auth_state.conn(), Uuid::from(*uuid)) {
				Ok(Some(_)) => {
					trace!("{} became known while sweeping, keeping it", path.display());
					return;
				}
				Ok(None) => {}
				Err(e) => {
					error!(
						"Failed to re-check {} before removing it: {e}",
						path.display()
					);
					return;
				}
			}
			match entry.metadata().await {
				Ok(meta) if meta.is_file() => match tokio::fs::remove_file(&path).await {
					Ok(_) => {
						trace!("Removed file: {}", path.display());
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {}
					Err(e) => {
						error!("Failed to remove file {}: {}", path.display(), e);
					}
				},
				Ok(_) => match tokio::fs::remove_dir_all(&path).await {
					Ok(_) => {
						trace!("Removed directory: {}", path.display());
					}
					Err(e) if e.kind() == io::ErrorKind::NotFound => {}
					Err(e) => {
						error!("Failed to remove directory {}: {}", path.display(), e);
					}
				},
				Err(e) if e.kind() == io::ErrorKind::NotFound => {}
				Err(e) => {
					error!("Failed to get metadata for {}: {}", path.display(), e);
				}
			}
		});
	}

	while (futures.next().await).is_some() {}
}

/// How long a staging file has to sit untouched before the sweep calls it wreckage.
///
/// Both writers of `tmp_dir/<uuid>` — [`AuthCacheState::download_file_io`] and
/// [`AuthCacheState::io_import_cached_file`] — write into it for as long as they run, so its
/// modification time tracks a live transfer. A day of no writes is far beyond anything the
/// transfer itself can survive (the SDK gives up on a request that goes 300 s without bytes) and
/// far beyond the ten-minute cleanup cadence, which is what keeps an in-flight download out of
/// reach of this sweep with room to spare.
const STAGING_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// How long a trusted "disabled" reading must hold before the destructive cleanup acts on it —
/// long enough to outlast the app's auth.json rewrite window (a delete-then-recreate measured in
/// milliseconds), short enough that a real disable still cleans up promptly.
const WIPE_CONFIRM_DELAY: Duration = Duration::from_secs(2);

/// Removes staging files left behind by downloads and imports that died.
///
/// [`cleanup_uuid_dir`] cannot do this, and runs over the same directory: it deletes only the
/// uuids the database does not know, while a download interrupted halfway staged its bytes under
/// a uuid whose row is perfectly alive — so until now nothing reaped those short of a logout. Age
/// is the only signal available here and it is enough, see [`STAGING_MAX_AGE`]. The two sweeps
/// racing for one entry is fine: whichever loses sees `NotFound`.
///
/// No per-item lock, unlike the deleters: [`AuthCacheState::lock_local_file`] serialises writers
/// of an item's *cache slot*, and a staging path is not one — it belongs to a single transfer for
/// as long as that transfer runs, and the age bound is what excludes it.
async fn remove_stale_staging(tmp_dir: &Path, max_age: Duration) {
	let Ok(mut dir) = tokio::fs::read_dir(tmp_dir).await else {
		tracing::warn!(
			"Tried to sweep stale staging files in {}, but it does not exist.",
			tmp_dir.display()
		);
		return;
	};

	let now = SystemTime::now();
	loop {
		let entry = match dir.next_entry().await {
			Ok(Some(entry)) => entry,
			Ok(None) => break,
			Err(e) => {
				error!("Failed to read directory {}: {}", tmp_dir.display(), e);
				return;
			}
		};
		let path = entry.path();
		let meta = match entry.metadata().await {
			Ok(meta) => meta,
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => {
				error!("Failed to get metadata for {}: {}", path.display(), e);
				continue;
			}
		};
		// A timestamp that cannot be read, or one in the future, counts as young: leaving
		// wreckage behind costs a file until the next pass, taking a live staging copy costs a
		// transfer.
		let too_old = meta
			.modified()
			.ok()
			.and_then(|modified| now.duration_since(modified).ok())
			.is_some_and(|age| age >= max_age);
		if !too_old {
			continue;
		}
		// Only files are ever staged here; anything else is wreckage of a different kind, and
		// removing it as a file would fail on every pass forever.
		let removed = if meta.is_dir() {
			tokio::fs::remove_dir_all(&path).await
		} else {
			tokio::fs::remove_file(&path).await
		};
		match removed {
			Ok(_) => trace!("Removed stale staging entry: {}", path.display()),
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => error!(
				"Failed to remove stale staging entry {}: {}",
				path.display(),
				e
			),
		}
	}
}

async fn next_entry_filter_icloud_files(
	dir: &mut tokio::fs::ReadDir,
) -> io::Result<Option<tokio::fs::DirEntry>> {
	#[cfg(any(target_os = "ios", target_os = "macos"))]
	loop {
		let entry = match dir.next_entry().await? {
			Some(e) => e,
			None => return Ok(None),
		};
		let file_name = entry.file_name();
		let file_name = file_name.to_string_lossy();
		if file_name.starts_with(".") && file_name.ends_with(".icloud") {
			continue;
		}
		return Ok(Some(entry));
	}
	#[cfg(not(any(target_os = "ios", target_os = "macos")))]
	dir.next_entry().await
}

async fn process_subdir(
	subdir_entry: tokio::fs::DirEntry,
) -> std::io::Result<Option<(PathBuf, DateTime<Utc>, u64)>> {
	// we match on file_type because it's generally free
	let file_type = match subdir_entry.file_type().await {
		Ok(ft) => ft,
		Err(e) => return Err(e),
	};
	let path = subdir_entry.path();
	// if the subfolder is a file or symlink, remove it and return an error
	if file_type.is_file() || file_type.is_symlink() {
		tokio::fs::remove_file(&path).await?;
		return Err(io::Error::new(
			io::ErrorKind::NotADirectory,
			format!("Expected directory but found file: {}", path.display()),
		));
	}

	// then we read the contents of the subfolder
	let mut contents = tokio::fs::read_dir(&path).await?;
	let Some(file_entry) = next_entry_filter_icloud_files(&mut contents).await? else {
		tokio::fs::remove_dir_all(path).await?;
		return Ok(None);
	};
	// we match on file_type first because it's generally free
	let file_type = file_entry.file_type().await?;
	if file_type.is_file() {
		// make sure there is only one file
		if next_entry_filter_icloud_files(&mut contents)
			.await?
			.is_some()
		{
			tracing::warn!(
				"Multiple files found in cache subdirectory {}, removing all",
				path.display()
			);
			tokio::fs::remove_dir_all(path).await?;
			Ok(None)
		} else {
			let meta = file_entry.metadata().await?;
			Ok(Some((
				file_entry.path(),
				FilenMetaExt::accessed_or_modified(&meta),
				FilenMetaExt::size(&meta),
			)))
		}
	} else {
		// if it's not a file, we remove the directory
		tokio::fs::remove_dir_all(path).await?;
		Ok(None)
	}
}

async fn count_cache_files(dir: &Path) -> std::io::Result<Vec<(PathBuf, DateTime<Utc>, u64)>> {
	let stream = tokio_stream::wrappers::ReadDirStream::new(tokio::fs::read_dir(dir).await?);

	let results = stream
		.map(|entry| async move { process_subdir(entry?).await })
		.buffer_unordered(128)
		// don't care about Ok(None), means we fixed the issue by removing invalid files
		// we also don't care about NotFound errors, they mean the file was already removed
		.filter_map(|res: Result<Option<_>, std::io::Error>| async {
			if let Err(e) = &res
				&& e.kind() == io::ErrorKind::NotFound
			{
				None
			} else {
				res.transpose()
			}
		})
		.collect::<Vec<_>>()
		.await;

	// we first want to make sure we try to cleanup as many files as possible
	// and only then return an error if there was one
	results.into_iter().collect()
}

/// One `thumbnails/<uuid>` directory: every `<w>x<h>.webp` inside is a candidate for the
/// sweep. Nothing else is ever written at the top level (`get_or_make_thumbnail` creates the
/// per-uuid directory first), so a bare file there is a leftover of nothing and goes.
///
/// Two things inside are left alone: a `.tmp` is an encode in flight (its guard removes it if
/// that encode dies), and an empty directory is a slot an encode is about to fill — eviction
/// removes a directory with its last size, and `cleanup_uuid_dir` prunes those of items gone
/// from the database, so nothing else has to.
async fn thumbnails_in(
	entry: tokio::fs::DirEntry,
) -> std::io::Result<Vec<(PathBuf, DateTime<Utc>, u64)>> {
	let path = entry.path();
	if !entry.file_type().await?.is_dir() {
		tokio::fs::remove_file(&path).await?;
		return Ok(Vec::new());
	}
	let mut contents = tokio::fs::read_dir(&path).await?;
	let mut found = Vec::new();
	while let Some(file) = next_entry_filter_icloud_files(&mut contents).await? {
		let file_path = file.path();
		if file_path.extension().is_some_and(|ext| ext == "tmp") {
			continue;
		}
		let meta = match file.metadata().await {
			Ok(meta) => meta,
			// Renamed into place or removed between the listing and the stat: an encode
			// finishing. The rest of the item still counts.
			Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
			Err(e) => return Err(e),
		};
		if meta.is_file() {
			found.push((
				file_path,
				FilenMetaExt::accessed_or_modified(&meta),
				FilenMetaExt::size(&meta),
			));
		}
	}
	Ok(found)
}

/// The thumbnail dir is laid out `thumbnails/<uuid>/<w>x<h>.webp`, one directory per item
/// holding every size made for it. The sweep used to expect thumbnails as direct children and
/// removed every directory it met as "invalid state" — which was every thumbnail on the device,
/// on each cleanup pass. Each size is its own entry, evicted on its own: an item's sizes have
/// their own ages, and charging one while removing all was how the sweep over-evicted.
async fn count_thumbnail_files(
	thumbnail_dir: &Path,
) -> std::io::Result<Vec<(PathBuf, DateTime<Utc>, u64)>> {
	let stream =
		tokio_stream::wrappers::ReadDirStream::new(tokio::fs::read_dir(thumbnail_dir).await?);

	let results = stream
		.map(|entry| async move { thumbnails_in(entry?).await })
		.buffer_unordered(128)
		// a NotFound is a directory something else removed between the listing and the read —
		// the identity sweep runs over this directory concurrently — and is not worth stopping for
		.filter_map(|res: Result<Vec<_>, std::io::Error>| async {
			match res {
				Err(e) if e.kind() == io::ErrorKind::NotFound => None,
				res => Some(res),
			}
		})
		.collect::<Vec<_>>()
		.await;
	// we first want to make sure we try to cleanup as many files as possible
	// and only then return an error if there was one
	let mut files = Vec::new();
	for res in results {
		files.extend(res?);
	}
	Ok(files)
}

const MIN_CACHED_FILES: usize = 5;

async fn remove_old_files<F>(dir: &Path, size_budget: u64, func: F) -> std::io::Result<u64>
where
	F: AsyncFnOnce(&Path) -> std::io::Result<Vec<(PathBuf, DateTime<Utc>, u64)>>,
{
	let mut current_files = func(dir).await?;

	current_files.sort_unstable_by(|a, b| match a.1.cmp(&b.1) {
		std::cmp::Ordering::Equal => b.2.cmp(&a.2), // larger files first if same modified time
		other => other,
	});

	let mut total_size: u64 = current_files.iter().map(|(_, _, size)| *size).sum();

	let mut file_count = current_files.len();

	for (path, _, size) in current_files {
		if total_size < size_budget || file_count < MIN_CACHED_FILES {
			break;
		}
		// Exactly the file, so what is charged is what was freed. A concurrent sweep may
		// have got there first; that is not a reason to stop.
		match tokio::fs::remove_file(&path).await {
			Ok(()) => {}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => return Err(e),
		}
		// Both directories keep their files one level down (cache/<uuid>/<name>,
		// thumbnails/<uuid>/<w>x<h>.webp), so the uuid directory goes with its last file
		// rather than being left behind empty. Non-recursive on purpose: while a sibling size
		// or an encode in flight is still inside, the directory stays.
		if let Some(parent) = path.parent()
			&& parent != dir
		{
			let _ = tokio::fs::remove_dir(parent).await;
		}
		total_size = total_size.saturating_sub(size);
		file_count -= 1;
	}

	Ok(total_size)
}

async fn remove_old_cache_files(cache_dir: &Path, size_budget: u64) {
	if let Err(e) = remove_old_files(cache_dir, size_budget, count_cache_files).await {
		error!(
			"Failed to remove old cache files in {}: {}",
			cache_dir.display(),
			e
		);
	}
}

async fn remove_old_thumbnails(thumbnail_dir: &Path, size_budget: u64) {
	if let Err(e) = remove_old_files(thumbnail_dir, size_budget, count_thumbnail_files).await {
		error!(
			"Failed to remove old thumbnail files in {}: {}",
			thumbnail_dir.display(),
			e
		);
	}
}

impl AuthCacheState {
	pub(crate) async fn should_cleanup(&self) -> bool {
		self.last_cleanup
			.read()
			.await
			.is_none_or(|t| t + AUTH_CLEANUP_INTERVAL <= chrono::Utc::now())
	}

	/// Drops the materialisation marker of every file whose cache slot the sweeps just took.
	///
	/// The sweeps delete slots by path — [`remove_old_files`] by age, [`cleanup_uuid_dir`] by
	/// identity, [`process_subdir`] by malformed shape — and none of them is in a position to
	/// write to the database. Reconciling once, after they have all run, keeps the column in step
	/// with the directory whatever removed the bytes, at the cost of one listing of a directory
	/// the budget already bounds.
	async fn reconcile_materialised(&self) {
		// Taken BEFORE the listing: a file materialised while it runs is missing from the snapshot
		// only because it did not exist yet, and the statement spares exactly those.
		let listed_at = chrono::Utc::now().timestamp_millis();
		let Ok(mut dir) = tokio::fs::read_dir(&self.cache_dir).await else {
			tracing::warn!(
				"Tried to reconcile the cached-copy records, but {} does not exist.",
				self.cache_dir.display()
			);
			return;
		};

		let mut uuids: Vec<UuidStr> = Vec::new();
		loop {
			match dir.next_entry().await {
				Ok(Some(entry)) => {
					if let Ok(uuid) = UuidStr::from_str(&entry.file_name().to_string_lossy()) {
						uuids.push(uuid);
					}
				}
				Ok(None) => break,
				Err(e) => {
					error!(
						"Failed to read directory {}: {}",
						self.cache_dir.display(),
						e
					);
					return;
				}
			}
		}

		if let Err(e) =
			sql::clear_materialised_not_in_cache(&self.conn(), uuids.into_iter(), listed_at)
		{
			error!("Failed to reconcile the cached-copy records: {e}");
		}
	}

	pub(crate) async fn cleanup_cache(&self) {
		if !self.should_cleanup().await {
			return;
		}

		let res = self.last_cleanup_sem.try_acquire();
		let _perm = match res {
			Ok(perm) => perm,
			Err(_) => {
				// another cleanup is already running
				return;
			}
		};

		// Before the sweeps: a rescued slot is a slot the identity sweep no longer has to be
		// guarded away from, and the reconciliation pass below then records it as materialised
		// under its current uuid instead of clearing the row's marker.
		self.rescue_reminted_slots().await;

		futures::join!(
			cleanup_uuid_dir(self, &self.cache_dir),
			cleanup_uuid_dir(self, &self.tmp_dir),
			remove_stale_staging(&self.tmp_dir, STAGING_MAX_AGE),
			remove_old_cache_files(&self.cache_dir, self.cache_file_budget,),
			remove_old_thumbnails(&self.thumbnail_dir, self.thumbnail_file_budget,),
			cleanup_uuid_dir(self, &self.thumbnail_dir)
		);
		self.reconcile_materialised().await;

		let mut lock = self.last_cleanup.write().await;
		let now = chrono::Utc::now();
		*lock = Some(now);
		if let Err(e) =
			update_saved_db_state_cache_cleanup_time(self.cache_state_file.as_ref(), now).await
		{
			tracing::error!("Failed to update cache cleanup time in saved db state: {e}");
		}
	}
}

impl CacheState {
	/// Takes the guard by value: the Disabled arm must DROP it before its confirmation delay,
	/// or every queued state writer (the 1s unauthenticated refresh) waits behind a pile of
	/// sleeping cleanup tasks.
	pub(crate) async fn cleanup_cache_if_necessary(
		cache: OwnedRwLockReadGuard<CacheState>,
		state: Arc<tokio::sync::RwLock<CacheState>>,
	) {
		debug!(
			"Cleaning up cache (files_dir {}, db_dir {})",
			cache.files_dir.display(),
			cache.db_dir.display()
		);
		match cache.status {
			AuthStatus::Authenticated(ref auth_state) => {
				debug!("Authenticated, cleaning up old files in cache directories");
				auth_state.cleanup_cache().await;
			}
			// The destructive wipe acts ONLY on an affirmative, trusted disable. Every other
			// unauthenticated flavour — an unreadable or undecryptable auth.json, a missing DEK
			// before first unlock, an enabled file whose config failed to build — leaves the
			// cache (DB, anchors, pending-upload markers) intact: those states say "unknown" or
			// "not right now", not "the user turned the provider off", and wiping on them forced
			// a full re-import over a blip.
			AuthStatus::Unauthenticated(UnauthCacheState {
				reason: UnauthReason::Disabled,
			}) => {
				// Single-flight: every unauthenticated FFI call launches a cleanup task, and a
				// Disabled confirmation sleeps below — without the gate they pile up, each
				// redundantly re-decrypting auth.json while holding a state read guard.
				let Ok(_permit) = cache.disabled_wipe_gate.clone().try_acquire_owned() else {
					return;
				};
				let (auth_file, dek) = cache.wipe_confirmation_handles();
				drop(cache);
				// Even a trusted disable gets re-confirmed after a delay: the app's historical
				// auth.json rewrite deleted-then-recreated the file, and a read landing in that
				// gap is indistinguishable from a real disable until the rewrite completes.
				tokio::time::sleep(WIPE_CONFIRM_DELAY).await;
				if !crate::auth::confirm_disabled(&auth_file, dek.as_ref()).await {
					debug!("Disable did not hold on re-read; leaving the cache intact");
					return;
				}
				// RE-ACQUIRE the state guard for the destructive phase and re-check the status
				// under it: the confirmation is point-in-time, and a re-enable landing during a
				// multi-second recursive delete would otherwise recreate the directories and DB
				// underneath the still-running unlink — destroying staged pending-upload bytes
				// and unlinking a database a fresh connection just opened. Holding the read
				// guard across the deletes makes the auth refresh (a writer) wait instead.
				let cache = state.read_owned().await;
				if !matches!(
					cache.status,
					AuthStatus::Unauthenticated(UnauthCacheState {
						reason: UnauthReason::Disabled,
					})
				) {
					debug!("State changed during the wipe confirmation; leaving the cache intact");
					return;
				}
				debug!("Confirmed disabled, removing all cache directories and database file");
				// We can delete the DBs because we are not authenticated, so no connection is
				// open. The wipe itself is the sync function shared with the re-init path,
				// bounced through spawn_blocking while this task keeps holding the read guard.
				let files_dir = cache.files_dir.clone();
				let db_dir = cache.db_dir.clone();
				if let Err(e) = tokio::task::spawn_blocking(move || {
					// Per-path failures are logged inside; a confirmed disable has no caller
					// to propagate them to, and the next confirmation retries what is left.
					let _ = wipe_account_data(&files_dir, &db_dir);
				})
				.await
				{
					error!("The confirmed-disable wipe task failed: {e}");
				}
			}
			AuthStatus::Unauthenticated(_) => {
				debug!("Not authenticated but not confirmed disabled; leaving the cache intact");
			}
		};
	}
}

impl FilenMobileCacheState {
	pub(crate) async fn async_launch_cleanup_task(&self) {
		trace!("Launching cleanup task asynchronously");
		let cache = self.async_get_cache_state_owned().await;
		let state = self.state.clone();
		self.tasks.spawn(async move {
			CacheState::cleanup_cache_if_necessary(cache, state).await;
		});
	}
	pub(crate) fn sync_launch_cleanup_task(&self) {
		trace!("Launching cleanup task synchronously");
		let cache = self.sync_get_cache_state_owned();
		let state = self.state.clone();
		self.tasks.spawn(async move {
			CacheState::cleanup_cache_if_necessary(cache, state).await;
		});
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const UUID: &str = "00000000-0000-0000-0000-000000000001";

	#[test]
	fn a_plain_name_is_kept_under_its_uuid_dir() {
		assert_eq!(
			cached_file_path(Path::new("/cache"), UUID, Some("photo.jpg")),
			Path::new("/cache/00000000-0000-0000-0000-000000000001/photo.jpg"),
		);
	}

	#[test]
	fn a_name_that_would_leave_the_cache_dir_falls_back_to_the_uuid() {
		let fallback = Path::new(
			"/cache/00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000001",
		);
		for name in ["..", "../../evil", "/etc/passwd", "a/b", "", "."] {
			assert_eq!(
				cached_file_path(Path::new("/cache"), UUID, Some(name)),
				fallback,
				"name {name:?}",
			);
		}
	}

	/// A staging file whose last write was `age` ago. Backdating the timestamp is what makes this
	/// deterministic — the alternative is waiting out the threshold on the real clock.
	fn stage(dir: &Path, name: &str, age: Duration) -> PathBuf {
		let path = dir.join(name);
		let file = std::fs::File::create(&path).unwrap();
		file.set_times(FileTimes::new().set_modified(SystemTime::now() - age))
			.unwrap();
		path
	}

	/// The hole this closes: a download that died left `tmp/<uuid>` behind, and the only sweep
	/// over that directory removes uuids the database does NOT know — so a staging file belonging
	/// to a perfectly live row was reaped by nothing at all. Both names below are exactly that
	/// case, which is why the sweep never asks the database: age decides, and age alone is what
	/// keeps a transfer in flight out of it.
	#[tokio::test]
	async fn a_stale_staging_file_goes_and_one_still_being_written_stays() {
		let dir = std::env::temp_dir().join(format!("filen-staging-sweep-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();

		let abandoned = stage(
			&dir,
			"00000000-0000-0000-0000-000000000001",
			STAGING_MAX_AGE + Duration::from_secs(60),
		);
		let in_flight = stage(
			&dir,
			"00000000-0000-0000-0000-000000000002",
			Duration::from_secs(30),
		);

		remove_stale_staging(&dir, STAGING_MAX_AGE).await;

		assert!(
			!abandoned.exists(),
			"a staging file nothing has touched in a day is wreckage, row or no row"
		);
		assert!(
			in_flight.exists(),
			"a download writing right now must survive the sweep"
		);

		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// A thumbnail laid out the way `get_or_make_thumbnail` writes it: `<uuid>/<w>x<h>.webp`,
	/// several sizes per item. Backdated like `stage` — both timestamps, because the sweep
	/// orders by access time where the filesystem keeps one — so eviction order is
	/// deterministic.
	fn thumbnail(dir: &Path, uuid: &str, size: &str, bytes: usize, age: Duration) -> PathBuf {
		let item = dir.join(uuid);
		std::fs::create_dir_all(&item).unwrap();
		let path = item.join(format!("{size}.webp"));
		std::fs::write(&path, vec![0u8; bytes]).unwrap();
		let then = SystemTime::now() - age;
		// Write access only where `set_times` needs it: on Windows it is SetFileTime on this
		// very handle, which wants FILE_WRITE_ATTRIBUTES, and a read-only open carries only
		// GENERIC_READ ("Access is denied" on every Windows CI leg). Unix `futimens` asks who
		// owns the file, not how it was opened.
		std::fs::OpenOptions::new()
			.read(true)
			.write(cfg!(windows))
			.open(&path)
			.unwrap()
			.set_times(FileTimes::new().set_accessed(then).set_modified(then))
			.unwrap();
		path
	}

	/// The sweep once expected thumbnails as direct children of the thumbnail dir and removed
	/// every directory it met as invalid state — which was every thumbnail on the device, on
	/// every cleanup pass. Under budget the per-uuid layout has to come through untouched; only
	/// what the writer never produces (a bare top-level file) goes, and an empty item directory
	/// — a slot an encode is about to fill — is left alone.
	#[tokio::test]
	async fn thumbnails_under_budget_survive_the_sweep() {
		let dir = std::env::temp_dir().join(format!("filen-thumb-sweep-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let a = "00000000-0000-0000-0000-00000000000a";
		let b = "00000000-0000-0000-0000-00000000000b";
		let kept = [
			thumbnail(&dir, a, "64x64", 100, Duration::ZERO),
			thumbnail(&dir, a, "128x128", 100, Duration::ZERO),
			thumbnail(&dir, b, "64x64", 100, Duration::ZERO),
		];
		let stray = dir.join("stray.webp");
		std::fs::write(&stray, b"x").unwrap();
		// An empty per-uuid directory: an encode between creating it and its temp file.
		let emptied = dir.join("00000000-0000-0000-0000-00000000000c");
		std::fs::create_dir_all(&emptied).unwrap();

		let total = remove_old_files(&dir, u64::MAX, count_thumbnail_files)
			.await
			.unwrap();

		assert_eq!(total, 300, "every size of every item is counted");
		for path in &kept {
			assert!(
				path.exists(),
				"{} was swept while under budget",
				path.display()
			);
		}
		assert!(
			!stray.exists(),
			"a bare file at the top level is nothing the writer produces"
		);
		assert!(
			emptied.exists(),
			"an empty item directory is an encode's, not the sweep's"
		);

		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// Each size is evicted on its own: a stale grid size goes while the item's fresh preview
	/// size stays, and the item's directory stays with it. The old sweep removed the whole
	/// directory for the first size it met and charged one file for it.
	#[tokio::test]
	async fn a_stale_size_goes_without_its_newer_siblings() {
		let dir = std::env::temp_dir().join(format!("filen-thumb-sizes-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let hour = Duration::from_secs(3600);
		let a = "00000000-0000-0000-0000-00000000000a";
		let stale = thumbnail(&dir, a, "64x64", 100, 10 * hour);
		let fresh = thumbnail(&dir, a, "512x512", 100, Duration::from_secs(60));
		let others: Vec<_> = (0xbu32..=0xe)
			.map(|n| {
				thumbnail(
					&dir,
					&format!("00000000-0000-0000-0000-00000000000{n:x}"),
					"64x64",
					100,
					(n - 8) * hour,
				)
			})
			.collect();

		// 600 bytes on disk against a 501-byte budget: exactly one eviction brings it under.
		let total = remove_old_files(&dir, 501, count_thumbnail_files)
			.await
			.unwrap();

		assert!(!stale.exists(), "the stale size is the one over budget");
		assert!(fresh.exists(), "the fresh size of the same item stays");
		for path in &others {
			assert!(
				path.exists(),
				"{} was evicted for bytes already freed",
				path.display()
			);
		}
		assert_eq!(total, 500);

		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// An encode in flight is neither counted nor evicted: its `.tmp` belongs to its guard.
	#[tokio::test]
	async fn an_encode_in_flight_is_left_to_its_guard() {
		let dir = std::env::temp_dir().join(format!("filen-thumb-tmp-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let a = "00000000-0000-0000-0000-00000000000a";
		let done = thumbnail(&dir, a, "64x64", 100, Duration::from_secs(60));
		let in_flight = dir.join(a).join("256x256.0000.tmp");
		std::fs::write(&in_flight, vec![0u8; 5000]).unwrap();

		let total = remove_old_files(&dir, 0, count_thumbnail_files)
			.await
			.unwrap();

		assert_eq!(total, 100, "the temp file is not counted");
		assert!(in_flight.exists(), "the temp file is not evicted");
		assert!(done.exists(), "one file is under MIN_CACHED_FILES");

		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// Over budget the oldest sizes go — here both sizes of one item, and with the last one
	/// its directory — and the sweep finishes.
	#[tokio::test]
	async fn the_oldest_item_is_evicted_whole_and_the_sweep_finishes() {
		let dir = std::env::temp_dir().join(format!("filen-thumb-evict-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let hour = Duration::from_secs(3600);
		let old = "00000000-0000-0000-0000-00000000000a";
		let newer = [
			"00000000-0000-0000-0000-00000000000b",
			"00000000-0000-0000-0000-00000000000c",
		];
		// MIN_CACHED_FILES keeps five files whatever the budget, so six are needed for the
		// sweep to evict anything at all.
		thumbnail(&dir, old, "64x64", 100, 3 * hour);
		thumbnail(&dir, old, "128x128", 100, 3 * hour);
		let kept: Vec<_> = newer
			.iter()
			.flat_map(|uuid| {
				[
					thumbnail(&dir, uuid, "64x64", 100, hour),
					thumbnail(&dir, uuid, "128x128", 100, hour),
				]
			})
			.collect();

		let total = remove_old_files(&dir, 0, count_thumbnail_files)
			.await
			.unwrap();

		assert!(
			!dir.join(old).exists(),
			"the oldest item's sizes go, and its directory with the last of them"
		);
		for path in &kept {
			assert!(
				path.exists(),
				"{} was evicted ahead of the oldest item",
				path.display()
			);
		}
		assert_eq!(
			total, 400,
			"both evicted sizes come off the total, the second on its own turn"
		);

		std::fs::remove_dir_all(&dir).unwrap();
	}
}

#[cfg(test)]
mod slot_rescue_tests {
	use std::sync::Mutex;

	use filen_sdk_rs::cache::open_sqlite_in_memory;
	use filen_types::fs::StableUuid;
	use rusqlite::Connection;

	use super::*;
	use crate::{
		auth::test_support::TempDbDir,
		file_locks::FileLocks,
		sql::{self, item::combine_parent, statements::UPSERT_ITEM},
	};

	fn uuid(byte: u8) -> Uuid {
		Uuid::from_bytes([byte; 16])
	}

	fn stable(byte: u8) -> StableUuid {
		StableUuid::new_for_test(uuid(byte))
	}

	fn db() -> Connection {
		let conn = open_sqlite_in_memory().unwrap();
		crate::auth::configure_conn(&conn).unwrap();
		conn.execute_batch(sql::statements::INIT).unwrap();
		sql::install_slot_remint_log(&conn).unwrap();
		conn.execute(
			"INSERT INTO items (uuid, parent, type) VALUES (?1, NULL, 0);",
			[uuid(9)],
		)
		.unwrap();
		conn
	}

	/// A file row with decoded metadata, upserted the way a listing lands it.
	fn upsert_file(conn: &Connection, uuid_: Uuid, stable_: StableUuid, name: &str) {
		let mut stmt = conn.prepare_cached(UPSERT_ITEM).unwrap();
		let (id, _, _) = crate::sql::item::upsert_file_item_with_stmts(
			uuid_,
			stable_,
			combine_parent(Some(uuid(9)), false),
			Some(name),
			None,
			&mut stmt,
		)
		.unwrap();
		drop(stmt);
		conn.execute(
			"INSERT OR IGNORE INTO files (id, size, chunks, region, bucket, timestamp, metadata_state)
			VALUES (?1, 0, 0, '', '', 0, 0);",
			[id],
		)
		.unwrap();
		conn.execute(
			"INSERT INTO files_meta (id, name, mime, file_key, file_key_version, modified)
			VALUES (?1, ?2, '', '', 3, 0)
			ON CONFLICT (id) DO UPDATE SET name = excluded.name;",
			rusqlite::params![id, name],
		)
		.unwrap();
	}

	fn slot_with_bytes(cache_dir: &Path, uuid_: Uuid, name: &str, bytes: &[u8]) -> PathBuf {
		let slot = cache_dir.join(uuid_.to_string());
		std::fs::create_dir_all(&slot).unwrap();
		let path = slot.join(name);
		std::fs::write(&path, bytes).unwrap();
		path
	}

	async fn rescue_all(conn: &Mutex<Connection>, cache_dir: &Path) {
		let locks = FileLocks::default();
		SlotRescue {
			conn,
			cache_dir,
			locks: &locks,
		}
		.rescue_all()
		.await;
	}

	/// The F1 scenario end to end: a marked row's uuid re-mints under a remote edit, and the
	/// rescue moves the slot — bytes, inner name and all — to follow the row, records the copy
	/// as materialised, and retires the log entry.
	#[tokio::test]
	async fn a_reminted_pending_slot_follows_its_row() {
		let dir = TempDbDir::create("slot-rescue-follow");
		let conn = db();
		upsert_file(&conn, uuid(1), stable(2), "edit.txt");
		sql::mark_pending_upload(&conn, stable(2), 1_000).unwrap();
		slot_with_bytes(&dir.0, uuid(1), "edit.txt", b"the only copy");

		// The remote edit arrives: same stable id, fresh uuid, and (as edits often do) a new name.
		upsert_file(&conn, uuid(11), stable(2), "renamed.txt");
		assert_eq!(sql::select_slot_remints(&conn).unwrap().len(), 1);

		let conn = Mutex::new(conn);
		rescue_all(&conn, &dir.0).await;

		assert!(!dir.0.join(uuid(1).to_string()).exists());
		assert_eq!(
			std::fs::read(dir.0.join(uuid(11).to_string()).join("renamed.txt")).unwrap(),
			b"the only copy",
			"the bytes must land under the row's current uuid and name"
		);
		let conn = conn.into_inner().unwrap();
		assert!(sql::select_slot_remints(&conn).unwrap().is_empty());
		let materialised: Option<i64> = conn
			.query_one(
				"SELECT materialised_at FROM items WHERE uuid = ?1;",
				[uuid(11)],
				|row| row.get(0),
			)
			.unwrap();
		assert!(materialised.is_some(), "the rescued copy must be recorded");
	}

	/// A download of the new head can land before the rescue runs. The pending marker says the
	/// server is missing an edit, and the drain uploads whatever is in the slot — so the edit's
	/// bytes must win over the server copy.
	#[tokio::test]
	async fn pending_bytes_outrank_a_fresh_download() {
		let dir = TempDbDir::create("slot-rescue-pending-wins");
		let conn = db();
		upsert_file(&conn, uuid(1), stable(2), "edit.txt");
		sql::mark_pending_upload(&conn, stable(2), 1_000).unwrap();
		slot_with_bytes(&dir.0, uuid(1), "edit.txt", b"unsent edit");
		upsert_file(&conn, uuid(11), stable(2), "edit.txt");
		slot_with_bytes(&dir.0, uuid(11), "edit.txt", b"server head");

		let conn = Mutex::new(conn);
		rescue_all(&conn, &dir.0).await;

		assert_eq!(
			std::fs::read(dir.0.join(uuid(11).to_string()).join("edit.txt")).unwrap(),
			b"unsent edit"
		);
	}

	/// The opposite call when the marker is gone: the fresh download is the right content and
	/// the old slot is a stale copy of a superseded version — drop it, keep the download.
	#[tokio::test]
	async fn a_resolved_edit_keeps_the_fresh_slot() {
		let dir = TempDbDir::create("slot-rescue-resolved");
		let conn = db();
		upsert_file(&conn, uuid(1), stable(2), "edit.txt");
		sql::mark_pending_upload(&conn, stable(2), 1_000).unwrap();
		slot_with_bytes(&dir.0, uuid(1), "edit.txt", b"stale");
		upsert_file(&conn, uuid(11), stable(2), "edit.txt");
		slot_with_bytes(&dir.0, uuid(11), "edit.txt", b"current");
		sql::clear_pending_upload(&conn, stable(2)).unwrap();

		let conn = Mutex::new(conn);
		rescue_all(&conn, &dir.0).await;

		assert!(!dir.0.join(uuid(1).to_string()).exists());
		assert_eq!(
			std::fs::read(dir.0.join(uuid(11).to_string()).join("edit.txt")).unwrap(),
			b"current"
		);
		assert!(
			sql::select_slot_remints(&conn.into_inner().unwrap())
				.unwrap()
				.is_empty()
		);
	}

	/// Two re-mints before any rescue ran: the entries chain (a → b, b → c) and oldest-first
	/// processing walks the bytes to the final uuid.
	#[tokio::test]
	async fn a_chain_of_remints_resolves_to_the_final_uuid() {
		let dir = TempDbDir::create("slot-rescue-chain");
		let conn = db();
		upsert_file(&conn, uuid(1), stable(2), "edit.txt");
		sql::mark_pending_upload(&conn, stable(2), 1_000).unwrap();
		slot_with_bytes(&dir.0, uuid(1), "edit.txt", b"the only copy");
		upsert_file(&conn, uuid(11), stable(2), "edit.txt");
		upsert_file(&conn, uuid(12), stable(2), "edit.txt");
		assert_eq!(sql::select_slot_remints(&conn).unwrap().len(), 2);

		let conn = Mutex::new(conn);
		rescue_all(&conn, &dir.0).await;

		assert_eq!(
			std::fs::read(dir.0.join(uuid(12).to_string()).join("edit.txt")).unwrap(),
			b"the only copy"
		);
		assert!(
			sql::select_slot_remints(&conn.into_inner().unwrap())
				.unwrap()
				.is_empty()
		);
	}
}
