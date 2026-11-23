//! The cache calls the core makes, behind a trait so population, hydration and the local side run
//! offline against a fake drive in the tests.

use std::{
	path::PathBuf,
	sync::{Arc, atomic::AtomicBool},
};

use filen_mobile_native_cache::{
	CacheError,
	abort::FfiAbortSignal,
	auth::FilenMobileCacheState,
	ffi::{FfiChanges, FfiDir, FfiFile, FfiId, FfiObject, QueryChildrenResponse, UploadFileInfo},
	range::{ByteRange, RangeRead, RangeReadError, RangeSink},
	remote::{CheckedUpload, PendingCopy},
	thumbnail::{FittedOutcome, FittedThumbnail},
	traits::WorkingSetUpdateListener,
};
use filen_sdk_rs::thumbnail::ByteSource;
use filen_types::fs::Uuid;

use crate::sealed;

/// What the core reads from the drive (the account's root, a directory's children, an item and a
/// byte range of a file) and how it changes it (uploads, directories, trash, renames and moves).
/// Implemented by the cache, [`FilenMobileCacheState`]; sealed, as the core relies on each call
/// meaning what the cache's does. Ids are the cache's: the root's uuid, or `stable/<uuid>` for a
/// directory or a file.
pub trait Drive: sealed::Sealed + Send + Sync + 'static {
	/// The uuid of the signed-in account's root directory, as text.
	fn root_uuid(&self) -> impl Future<Output = Result<String, CacheError>> + Send;

	/// The children of the directory `dir`, refreshed from the server first; `None` when the cache
	/// does not hold the directory.
	fn list_children(
		&self,
		dir: FfiId,
	) -> impl Future<Output = Result<Option<QueryChildrenResponse>, CacheError>> + Send;

	/// The children of the directory `dir` as the cache holds them, with no server round trip;
	/// `None` when the cache does not hold the directory.
	fn cached_children(
		&self,
		dir: &FfiId,
	) -> impl Future<Output = Result<Option<QueryChildrenResponse>, CacheError>> + Send;

	/// The page of changes to the cache's items after `anchor`, at most `limit` of them, as
	/// [`FilenMobileCacheState::enumerate_changes_page`] gives them: every item changed since, at
	/// its latest state (trashed ones with their original parent set), and, only when an anchor is
	/// given, the ids of the items removed since. An anchor another incarnation of the cache issued
	/// fails with [`CacheError::SyncAnchorExpired`]. The provider's own changes are in it too.
	fn changes(
		&self,
		anchor: Option<Vec<u8>>,
		limit: u32,
	) -> impl Future<Output = Result<FfiChanges, CacheError>> + Send;

	/// The anchor of the cache's latest change: a page read from it holds only what changes after.
	fn change_anchor(&self) -> impl Future<Output = Result<Vec<u8>, CacheError>> + Send;

	/// Registers `listener` to be told when the live updates changed the cache, or clears it with
	/// `None`. Told of the provider's own changes too, once the server's events for them come back
	/// (measured against the cache), so what it wakes must take them for what they are.
	fn watch_changes(
		&self,
		listener: Option<Arc<dyn WorkingSetUpdateListener>>,
	) -> impl Future<Output = ()> + Send;

	/// Replaces the directories the provider holds populated, as `stable/<uuid>` ids (the root by
	/// its uuid): the cache keeps those fresh after a reconnect or a gap in the live updates.
	fn set_populated_dirs(
		&self,
		ids: Vec<String>,
	) -> impl Future<Output = Result<(), CacheError>> + Send;

	/// The item `id` as the cache holds it, with no server round trip; `None` when it holds none.
	fn query_item(
		&self,
		id: &FfiId,
	) -> impl Future<Output = Result<Option<FfiObject>, CacheError>> + Send;

	/// Reads `range` of the file `id` into `sink`, as
	/// [`FilenMobileCacheState::read_file_range`] does.
	fn read_range<S: RangeSink>(
		&self,
		id: &FfiId,
		range: ByteRange,
		sink: &mut S,
		abort: Option<&Arc<FfiAbortSignal>>,
	) -> impl Future<Output = Result<RangeRead, RangeReadError<S::Error>>> + Send;

	/// The thumbnail of the version `version` of the file `id` fitted inside a `max_side` pixel
	/// square, its aspect ratio kept, made from the remote file's chunks, never from the local
	/// placeholder, and kept in the cache's thumbnail directory, as
	/// [`FilenMobileCacheState::fitted_thumbnail`] makes it; [`CacheError::DoesNotExist`] when the
	/// cache holds another version of the file. Setting `cancel` stops the decode at its next chunk.
	fn fitted_thumbnail(
		&self,
		id: &FfiId,
		version: Uuid,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> impl Future<Output = Result<FittedOutcome, CacheError>> + Send;

	/// The thumbnail of the file version `version` fitted inside a `max_side` pixel square, if the
	/// cache's thumbnail directory holds it, as [`Drive::fitted_thumbnail`] kept it: read from disk
	/// with no server round trip and nothing made, as
	/// [`FilenMobileCacheState::cached_fitted_thumbnail`] reads it.
	fn cached_fitted_thumbnail(
		&self,
		version: Uuid,
		max_side: u32,
	) -> impl Future<Output = Result<Option<FittedThumbnail>, CacheError>> + Send;

	/// The thumbnail of local bytes, `source`, of a file named `name`, fitted as
	/// [`Drive::fitted_thumbnail`] fits it and not cached, as
	/// [`FilenMobileCacheState::fitted_thumbnail_of_local`] makes it. Setting `cancel` stops the
	/// decode at its next read.
	fn fitted_thumbnail_of_local(
		&self,
		source: Box<dyn ByteSource>,
		name: &str,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> impl Future<Output = Result<FittedOutcome, CacheError>> + Send;

	/// The item `id`, refreshed from the server first; `None` when the server no longer has it. A
	/// trashed item comes back with its original parent set.
	fn refresh_item(
		&self,
		id: FfiId,
	) -> impl Future<Output = Result<Option<FfiObject>, CacheError>> + Send;

	/// Uploads the file at `os_path` into the directory `parent` as described by `info`. The server
	/// replaces a file of the same name (its old content survives as a version only when versioning
	/// is on), so the caller uploads under a name no file holds and then renames the file
	/// ([`Drive::rename`]), which the server refuses while another file holds the name.
	fn upload_file(
		&self,
		os_path: String,
		parent: FfiId,
		info: UploadFileInfo,
	) -> impl Future<Output = Result<FfiFile, CacheError>> + Send;

	/// Uploads the file at `os_path`, which hashed to `expect` while no writer had it open, as a
	/// new version of the file `file`, through the cache's copy of it: a copy that does not hash to
	/// `expect` is never uploaded ([`CheckedUpload::Changed`]). The local file is the edit's copy,
	/// so the cache keeps no edit outstanding once this returns; only a call dropped before it
	/// returns leaves the edit marked, for [`Drive::pending_uploads`] to find.
	fn upload_version(
		&self,
		file: FfiId,
		os_path: String,
		expect: [u8; 32],
	) -> impl Future<Output = Result<CheckedUpload, CacheError>> + Send;

	/// Creates the directory `name` in `parent`, dated `created` (ms since the Unix epoch).
	fn create_dir(
		&self,
		parent: FfiId,
		name: String,
		created: Option<i64>,
	) -> impl Future<Output = Result<FfiDir, CacheError>> + Send;

	/// Moves the item `id` to the trash; an item already there stays.
	fn trash(&self, id: FfiId) -> impl Future<Output = Result<FfiObject, CacheError>> + Send;

	/// Renames the item `id` to `name`; `None` when it already has that name. A file's rename onto
	/// a name another file in its directory holds, in the server's case folding, is refused
	/// (measured: `file_with_name_already_exists_at_destination`).
	fn rename(
		&self,
		id: FfiId,
		name: String,
	) -> impl Future<Output = Result<Option<FfiObject>, CacheError>> + Send;

	/// Moves the item `id` into the directory `parent`. Not refused when a file there holds a
	/// moved file's name (measured): that file stops being current and lives on only as an older
	/// version of the moved one, so the caller checks the name first.
	fn move_to(
		&self,
		id: FfiId,
		parent: FfiId,
	) -> impl Future<Output = Result<FfiObject, CacheError>> + Send;

	/// The files whose edits the cache holds marked as not on the server, as `stable/<uuid>` ids:
	/// left by an upload dropped before it returned, or by a build that let the cache drain them.
	/// The cache's own drain is never run: it uploads its copy with no check of the version the
	/// edit was made from.
	fn pending_uploads(&self) -> impl Future<Output = Result<Vec<FfiId>, CacheError>> + Send;

	/// Gives up the edit of the file `file` the cache holds marked: the edit is kept elsewhere
	/// (the local file holds it, or it was copied out), and the drive's version must not be
	/// uploaded over.
	fn drop_pending_upload(
		&self,
		file: FfiId,
	) -> impl Future<Output = Result<(), CacheError>> + Send;

	/// Copies the edit of the file `file` the cache holds marked to the new file `dest`, keeping
	/// the marker for [`Drive::drop_pending_upload`] once the copy is safe; with nothing to copy
	/// (no edit, or one the drive has), the marker goes and nothing is written.
	fn copy_pending_upload(
		&self,
		file: FfiId,
		dest: PathBuf,
	) -> impl Future<Output = Result<PendingCopy, CacheError>> + Send;

	/// How many edits the cache holds marked as not on the server: uploads on their way, and
	/// those [`Drive::pending_uploads`] lists.
	fn pending_upload_count(&self) -> impl Future<Output = Result<u32, CacheError>> + Send;

	/// Stops the drive for good: its live updates first, so no change from the server reaches it
	/// once the session is gone, then every task it runs. Nothing may be called on it after.
	fn stop(&self) -> impl Future<Output = ()> + Send;
}

/// The cache's answer to a query of an item as [`Drive::query_item`] gives it: the cache answers a
/// `stable/` id whose row it no longer holds, as for an item deleted while the provider was down,
/// with [`CacheError::DoesNotExist`] rather than `None`, and either way it holds no such item.
pub(crate) fn held_item(
	answer: Result<Option<FfiObject>, CacheError>,
) -> Result<Option<FfiObject>, CacheError> {
	match answer {
		Err(CacheError::DoesNotExist(_)) => Ok(None),
		answer => answer,
	}
}

impl sealed::Sealed for FilenMobileCacheState {}

impl Drive for FilenMobileCacheState {
	fn root_uuid(&self) -> impl Future<Output = Result<String, CacheError>> + Send {
		self.async_root_uuid()
	}

	fn list_children(
		&self,
		dir: FfiId,
	) -> impl Future<Output = Result<Option<QueryChildrenResponse>, CacheError>> + Send {
		// One call for every child: the OS takes a directory's placeholders in one call anyway.
		self.update_and_query_dir_children(dir, None)
	}

	fn cached_children(
		&self,
		dir: &FfiId,
	) -> impl Future<Output = Result<Option<QueryChildrenResponse>, CacheError>> + Send {
		self.async_query_dir_children(dir, None)
	}

	fn changes(
		&self,
		anchor: Option<Vec<u8>>,
		limit: u32,
	) -> impl Future<Output = Result<FfiChanges, CacheError>> + Send {
		self.async_enumerate_changes_page(anchor, limit)
	}

	fn change_anchor(&self) -> impl Future<Output = Result<Vec<u8>, CacheError>> + Send {
		self.async_current_sync_anchor()
	}

	fn watch_changes(
		&self,
		listener: Option<Arc<dyn WorkingSetUpdateListener>>,
	) -> impl Future<Output = ()> + Send {
		self.async_set_working_set_listener(listener)
	}

	fn set_populated_dirs(
		&self,
		ids: Vec<String>,
	) -> impl Future<Output = Result<(), CacheError>> + Send {
		self.set_materialized_containers(ids)
	}

	async fn query_item(&self, id: &FfiId) -> Result<Option<FfiObject>, CacheError> {
		held_item(self.async_query_item(id).await)
	}

	fn read_range<S: RangeSink>(
		&self,
		id: &FfiId,
		range: ByteRange,
		sink: &mut S,
		abort: Option<&Arc<FfiAbortSignal>>,
	) -> impl Future<Output = Result<RangeRead, RangeReadError<S::Error>>> + Send {
		self.read_file_range(id, range, sink, abort)
	}

	fn refresh_item(
		&self,
		id: FfiId,
	) -> impl Future<Output = Result<Option<FfiObject>, CacheError>> + Send {
		self.update_and_query_item(id)
	}

	fn fitted_thumbnail(
		&self,
		id: &FfiId,
		version: Uuid,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> impl Future<Output = Result<FittedOutcome, CacheError>> + Send {
		FilenMobileCacheState::fitted_thumbnail(self, id, version, max_side, cancel)
	}

	fn cached_fitted_thumbnail(
		&self,
		version: Uuid,
		max_side: u32,
	) -> impl Future<Output = Result<Option<FittedThumbnail>, CacheError>> + Send {
		FilenMobileCacheState::cached_fitted_thumbnail(self, version, max_side)
	}

	fn fitted_thumbnail_of_local(
		&self,
		source: Box<dyn ByteSource>,
		name: &str,
		max_side: u32,
		cancel: &Arc<AtomicBool>,
	) -> impl Future<Output = Result<FittedOutcome, CacheError>> + Send {
		FilenMobileCacheState::fitted_thumbnail_of_local(self, source, name, max_side, cancel)
	}

	async fn upload_file(
		&self,
		os_path: String,
		parent: FfiId,
		info: UploadFileInfo,
	) -> Result<FfiFile, CacheError> {
		self.upload_new_file_abortable(os_path, parent, info, None, None)
			.await
			.map(|uploaded| uploaded.file)
	}

	fn upload_version(
		&self,
		file: FfiId,
		os_path: String,
		expect: [u8; 32],
	) -> impl Future<Output = Result<CheckedUpload, CacheError>> + Send {
		self.upload_checked_version(file, os_path, expect)
	}

	async fn create_dir(
		&self,
		parent: FfiId,
		name: String,
		created: Option<i64>,
	) -> Result<FfiDir, CacheError> {
		FilenMobileCacheState::create_dir(self, parent, name, created)
			.await
			.map(|created| created.dir)
	}

	async fn trash(&self, id: FfiId) -> Result<FfiObject, CacheError> {
		self.trash_item(id).await.map(|trashed| trashed.object)
	}

	async fn rename(&self, id: FfiId, name: String) -> Result<Option<FfiObject>, CacheError> {
		self.rename_item(id, name)
			.await
			.map(|renamed| renamed.map(|renamed| renamed.object))
	}

	async fn move_to(&self, id: FfiId, parent: FfiId) -> Result<FfiObject, CacheError> {
		self.move_item(id, parent).await.map(|moved| moved.object)
	}

	fn pending_uploads(&self) -> impl Future<Output = Result<Vec<FfiId>, CacheError>> + Send {
		self.pending_upload_ids()
	}

	fn drop_pending_upload(
		&self,
		file: FfiId,
	) -> impl Future<Output = Result<(), CacheError>> + Send {
		FilenMobileCacheState::drop_pending_upload(self, file)
	}

	fn copy_pending_upload(
		&self,
		file: FfiId,
		dest: PathBuf,
	) -> impl Future<Output = Result<PendingCopy, CacheError>> + Send {
		FilenMobileCacheState::copy_pending_upload(self, file, dest)
	}

	fn pending_upload_count(&self) -> impl Future<Output = Result<u32, CacheError>> + Send {
		self.async_pending_upload_count()
	}

	async fn stop(&self) {
		// The live updates stop only at the cache's next re-read of auth.json otherwise, which only
		// a call into the cache triggers: stopped here, so none outlives the session.
		self.async_stop_live_updates().await;
		self.shutdown().await;
	}
}
