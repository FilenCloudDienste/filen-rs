//! Removing a job's sources once its result is verified: the archive after extracting it, the
//! items after compressing them.
//!
//! A source is only touched after a job that completed with nothing failed, skipped or left
//! unaccounted, whose output was confirmed with the server, and only if the source is still
//! exactly what the job read: checked again right before it is removed, while holding the drive
//! lock, so a client that writes under the lock cannot change it in between. A file the job
//! wrote (a compression's archive) is checked again under that same lock too, so no source is
//! removed once the archive that holds it is gone. A pause is waited
//! out between sources, never while holding the lock. Directories are only ever trashed, never
//! purged: a permanent removal deletes the files the job read one by one and trashes what is
//! left, so nothing the job did not read is ever deleted for good. Directory sizes are never
//! used, since the server caches them.
//!
//! A listing holds only finished uploads: a file another device is still uploading into a source
//! directory is not seen, and ends up in the trash with the directory, where it can be
//! restored.

use std::{
	collections::{BTreeMap, BTreeSet},
	future::Future,
	sync::Arc,
};

use filen_macros::js_type;
use filen_types::fs::{ParentUuid, Uuid};

use crate::{
	Error, api,
	fs::{
		HasUUID,
		categories::{DirType, Normal, fs::CategoryFS},
		drive_job::{
			backend::{ClientBackend, DriveBackend},
			lock::{HeldLock, LockWait, wait_for_lock},
		},
		file::traits::HasFileInfo,
	},
	job::{JobControl, Stopped, report::Ops},
	util::MaybeSend,
};

/// What to do with a job's sources once its result is verified.
///
/// An extraction that left macOS metadata out on purpose (`skip_mac_metadata`, on by default)
/// still counts as complete: that metadata keeps nothing from removing the archive. With
/// [`DeletePermanently`](Self::DeletePermanently), what was left out (resource forks, extended
/// attributes, and any `._name` file that starts as an AppleDouble file does) is then gone for
/// good.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[js_type(import, export, no_default)]
pub enum SourceDisposal {
	/// Move them to the trash, where they can be restored. Frees no storage until the trash is
	/// emptied.
	Trash,
	/// Delete the files for good; directories are trashed once emptied. A file with older
	/// versions is kept instead (kept for `HasVersions`): deleting it for good would leave
	/// its versions where no client can reach them.
	DeletePermanently,
}

/// Why a source was kept.
#[derive(Debug, Clone)]
pub enum KeptReason {
	/// Something was not carried over: an entry failed or was skipped.
	Incomplete,
	/// The archive holds data after its last entry that belongs to none.
	UnaccountedData {
		/// Bytes of that data.
		bytes: u64,
	},
	/// What was read does not match the hash in the source's metadata.
	HashMismatch,
	/// The source's metadata holds no hash to check what was read against, which a permanent
	/// deletion requires. Never given for a zip or 7z archive: every entry's own checksum, and
	/// every entry having been extracted, confirm those instead of a whole-archive hash.
	HashUnavailable,
	/// The source changed since the job read it: it moved, was trashed, got a new version, or
	/// holds other items now.
	Changed,
	/// The job's output could not be confirmed: the server did not hold what was created, the
	/// archive was not read in full, or something the job extracted was checked by nothing (a
	/// 7z entry without a CRC-32, or the files of a brotli or LZMA-alone stream, or of an lz4,
	/// xz or zstd stream written without its optional checksum). Before a permanent deletion,
	/// also an archive a compression wrote that does not read back as its sources, or could not
	/// be read back (a request or its reader failed).
	Unconfirmed,
	/// Deleting it for good would lose the older versions of a file in it.
	HasVersions,
	/// The job was cancelled before it removed it: before the removal began (the job ended
	/// early), or while checking or removing it.
	Interrupted,
	/// Removing it failed.
	Failed {
		/// Why.
		error: Arc<Error>,
	},
}

/// What became of a source. A source inside another one the job was given (or given twice)
/// shares that one's outcome with a `bytes_freed` of 0: what the outer removal freed, this
/// source's files included, is counted once, on the outer source. So a folder inside one whose
/// permanent removal stopped part way is kept, and may still have lost files to it.
#[derive(Debug, Clone)]
pub enum DisposalOutcome {
	/// The source was removed.
	Disposed {
		/// How it was removed.
		how: SourceDisposal,
		/// Bytes of files deleted for good (0 when trashed).
		bytes_freed: u64,
	},
	/// The source was left where it is.
	Kept {
		/// Why it was kept.
		reason: KeptReason,
		/// Bytes of its files already deleted for good when a permanent removal stopped part
		/// way (0 otherwise): those files are gone, and only the job's output still holds them.
		/// Empty files may be gone too while this is 0.
		bytes_freed: u64,
	},
}

/// What became of one source.
#[derive(Debug, Clone)]
pub struct SourceDisposition {
	/// The source's uuid, as the caller passed it.
	pub uuid: Uuid,
	/// What became of it.
	pub outcome: DisposalOutcome,
}

/// A file's state on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileState {
	pub(crate) size: u64,
	pub(crate) chunks: u64,
	pub(crate) parent: ParentUuid,
	/// Superseded by a newer version.
	pub(crate) versioned: bool,
	pub(crate) trash: bool,
}

/// A directory's place on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirState {
	pub(crate) parent: ParentUuid,
	pub(crate) trash: bool,
}

/// Everything below a directory: files with their sizes, and directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Tree {
	pub(crate) files: BTreeMap<Uuid, u64>,
	pub(crate) dirs: BTreeSet<Uuid>,
}

impl Tree {
	/// The digest of its items, as [`file_digest`] and [`dir_digest`] add them up.
	pub(crate) fn digest(&self) -> u128 {
		let files = self.files.iter().fold(0u128, |sum, (&uuid, &size)| {
			sum.wrapping_add(file_digest(uuid, size))
		});
		self.dirs
			.iter()
			.fold(files, |sum, &uuid| sum.wrapping_add(dir_digest(uuid)))
	}
}

/// A file's share of an order-free digest of a set of items: the sum of every item's. Two sets
/// with the same sum hold the same items, short of a deliberate 128-bit collision, which the
/// items' uuids (the server's to pick) leave no room for.
pub(crate) fn file_digest(uuid: Uuid, size: u64) -> u128 {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"file");
	hasher.update(uuid.as_bytes());
	hasher.update(&size.to_le_bytes());
	u128::from_le_bytes(
		hasher.finalize().as_bytes()[..16]
			.try_into()
			.expect("16 bytes"),
	)
}

/// A directory's share of an order-free digest, as [`file_digest`].
pub(crate) fn dir_digest(uuid: Uuid) -> u128 {
	let mut hasher = blake3::Hasher::new();
	hasher.update(b"dir");
	hasher.update(uuid.as_bytes());
	u128::from_le_bytes(
		hasher.finalize().as_bytes()[..16]
			.try_into()
			.expect("16 bytes"),
	)
}

/// What removing sources needs besides a [`DriveBackend`].
pub(crate) trait DisposalBackend: DriveBackend {
	fn file_state(&self, uuid: Uuid) -> impl Future<Output = Result<FileState, Error>> + MaybeSend;
	fn dir_state(&self, uuid: Uuid) -> impl Future<Output = Result<DirState, Error>> + MaybeSend;
	fn list_tree(&self, dir: Uuid) -> impl Future<Output = Result<Tree, Error>> + MaybeSend;
	/// The caller holds the drive lock.
	fn trash_file(&self, uuid: Uuid) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// The caller holds the drive lock.
	fn delete_file_permanently(
		&self,
		uuid: Uuid,
	) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// The caller holds the drive lock.
	fn trash_dir(&self, uuid: Uuid) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// Whether the file has older versions.
	fn has_older_versions(
		&self,
		uuid: Uuid,
	) -> impl Future<Output = Result<bool, Error>> + MaybeSend;
}

impl DisposalBackend for ClientBackend {
	async fn file_state(&self, uuid: Uuid) -> Result<FileState, Error> {
		let info = self.client().get_file_with_info(uuid).await?;
		Ok(FileState {
			size: info.file.size(),
			chunks: info.file.chunks(),
			parent: info.file.parent,
			versioned: info.versioned,
			trash: info.trash,
		})
	}

	async fn dir_state(&self, uuid: Uuid) -> Result<DirState, Error> {
		let response =
			api::v3::dir::post(self.client().client(), &api::v3::dir::Request { uuid }).await?;
		Ok(DirState {
			parent: response.parent,
			trash: response.trash,
		})
	}

	async fn list_tree(&self, dir: Uuid) -> Result<Tree, Error> {
		let client = self.client();
		let dir = client.get_dir(dir).await?;
		let (dirs, files) = Normal::list_dir_recursive(
			client,
			&DirType::Dir(std::borrow::Cow::Owned(dir)),
			None::<&fn(u64, Option<u64>)>,
			(),
		)
		.await?;
		Ok(Tree {
			files: files
				.iter()
				.map(|file| (file.uuid(), file.size()))
				.collect(),
			dirs: dirs.iter().map(HasUUID::uuid).collect(),
		})
	}

	async fn trash_file(&self, uuid: Uuid) -> Result<(), Error> {
		api::v3::file::trash::post(
			self.client().client(),
			&api::v3::file::trash::Request { uuid },
		)
		.await
	}

	async fn delete_file_permanently(&self, uuid: Uuid) -> Result<(), Error> {
		api::v3::file::delete::permanent::post(
			self.client().client(),
			&api::v3::file::delete::permanent::Request { uuid },
		)
		.await
	}

	async fn trash_dir(&self, uuid: Uuid) -> Result<(), Error> {
		api::v3::dir::trash::post(
			self.client().client(),
			&api::v3::dir::trash::Request { uuid },
		)
		.await
	}

	async fn has_older_versions(&self, uuid: Uuid) -> Result<bool, Error> {
		let response = api::v3::file::versions::post(
			self.client().client(),
			&api::v3::file::versions::Request { uuid },
		)
		.await?;
		Ok(response.versions.iter().any(|version| version.uuid != uuid))
	}
}

/// A file source as the job read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExpectedFile {
	pub(crate) uuid: Uuid,
	pub(crate) size: u64,
	pub(crate) chunks: u64,
	pub(crate) parent: Uuid,
}

impl ExpectedFile {
	pub(crate) fn of(file: &impl HasFileInfo, uuid: Uuid, parent: Uuid) -> Self {
		Self {
			uuid,
			size: file.size(),
			chunks: file.chunks(),
			parent,
		}
	}

	fn matches(&self, state: &FileState) -> bool {
		!state.trash
			&& !state.versioned
			&& state.size == self.size
			&& state.chunks == self.chunks
			&& state.parent == ParentUuid::Uuid(self.parent)
	}
}

/// A file a job wrote, as it wrote it: its sources are only removed while it still stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrittenFile {
	pub(crate) uuid: Uuid,
	/// Bytes the job wrote.
	pub(crate) size: u64,
	pub(crate) chunks: u64,
}

impl WrittenFile {
	/// Whether the server still holds it as written: out of the trash, not superseded by a
	/// newer version, at its size and chunk count.
	pub(crate) fn stands(&self, state: &FileState) -> bool {
		!state.trash && !state.versioned && state.size == self.size && state.chunks == self.chunks
	}
}

/// What each removal of a job's sources is paced and checked by.
#[derive(Clone, Copy)]
pub(crate) struct Removing<'a> {
	pub(crate) control: &'a JobControl,
	pub(crate) ops: &'a Ops,
	/// The file the job wrote, checked again under the lock each removal holds, so no source is
	/// removed once a client writing under the lock removed or replaced it. `None` for an
	/// extraction, whose output (the items it created) is confirmed once, before its archive
	/// is removed.
	pub(crate) output: Option<WrittenFile>,
}

/// A directory source as the job read it: where it was, and everything below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExpectedDir {
	pub(crate) uuid: Uuid,
	pub(crate) parent: Uuid,
	pub(crate) read: Tree,
}

impl ExpectedDir {
	fn matches(&self, state: &DirState) -> bool {
		!state.trash && state.parent == ParentUuid::Uuid(self.parent)
	}
}

/// Every one of `sources` kept by a job that ended before removing them: as interrupted when it
/// was cancelled, as incomplete otherwise.
pub(crate) fn kept_on_early_end(sources: &[Uuid], cancelled: bool) -> Vec<SourceDisposition> {
	let reason = if cancelled {
		KeptReason::Interrupted
	} else {
		KeptReason::Incomplete
	};
	sources
		.iter()
		.map(|&uuid| SourceDisposition {
			uuid,
			outcome: DisposalOutcome::kept(reason.clone()),
		})
		.collect()
}

impl DisposalOutcome {
	/// Kept for `reason`, before anything of it was deleted.
	pub(crate) fn kept(reason: KeptReason) -> Self {
		Self::Kept {
			reason,
			bytes_freed: 0,
		}
	}
}

/// How a job's source nests among the others, as [`nesting`] tells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Nest {
	/// It lies within no other: it is removed on its own.
	Outermost,
	/// It is removed with this source, the outermost of those it lies within.
	Within(usize),
	/// It lies on or behind a chain of sources that loops back on itself.
	Cyclic,
}

impl Nest {
	/// The source that `source`, nesting so, is removed with: itself unless it is within another.
	pub(crate) fn outermost(self, source: usize) -> usize {
		match self {
			Self::Within(outer) => outer,
			Self::Outermost | Self::Cyclic => source,
		}
	}
}

/// How sources nest, from the source each one lies directly `within` (if any): a source inside
/// another goes with that one, whose removal removes it. A move between two folders' listings
/// can leave each read holding the other: a chain like that has no outermost source, so every
/// source on or behind it goes with itself, and is cyclic.
pub(crate) fn nesting(within: &[Option<usize>]) -> Vec<Nest> {
	let cyclic: Vec<bool> = (0..within.len())
		.map(|source| {
			let mut outer = source;
			for _ in 0..within.len() {
				match within[outer] {
					Some(next) => outer = next,
					None => return false,
				}
			}
			true
		})
		.collect();
	// the chains left are acyclic: none reaches a cyclic source, or it would be one
	(0..within.len())
		.map(|source| {
			if cyclic[source] {
				return Nest::Cyclic;
			}
			let mut outer = source;
			while let Some(next) = within[outer] {
				outer = next;
			}
			if outer == source {
				Nest::Outermost
			} else {
				Nest::Within(outer)
			}
		})
		.collect()
}

impl KeptReason {
	fn failed(error: Error) -> Self {
		Self::Failed {
			error: Arc::new(error),
		}
	}
}

/// Waits for one request of a removal's checks: a cancel through `control` drops it and keeps
/// the source as interrupted, and a failed request keeps it as failed.
async fn checked<T>(
	control: &JobControl,
	request: impl Future<Output = Result<T, Error>>,
) -> Result<T, KeptReason> {
	match control.until_stopping(request).await {
		Ok(Ok(value)) => Ok(value),
		Ok(Err(error)) => Err(KeptReason::failed(error)),
		Err(Stopped) => Err(KeptReason::Interrupted),
	}
}

/// Keeps the source as interrupted once the job is stopping, before a removal request that would
/// not be dropped once sent.
fn unless_stopping(control: &JobControl) -> Result<(), KeptReason> {
	if control.is_stopping() {
		return Err(KeptReason::Interrupted);
	}
	Ok(())
}

/// Takes the drive lock for one source's recheck and removal, waiting out a pause first so a
/// paused job holds no lock, and checks the job's output again under it: a source is kept as
/// unconfirmed once the output no longer stands, or its state cannot be fetched. A cancel keeps
/// the source.
async fn lock_for_removal<B: DisposalBackend>(
	backend: &B,
	removing: Removing<'_>,
) -> Result<HeldLock<B::DriveLock>, KeptReason> {
	let Removing {
		control,
		ops,
		output,
	} = removing;
	let held = loop {
		if ops.checkpoint(control).await.is_err() {
			return Err(KeptReason::Interrupted);
		}
		match wait_for_lock(backend, control, ops).await {
			Ok(LockWait::Locked(held)) => break held,
			Ok(LockWait::Paused) => {}
			Ok(LockWait::Failed(error)) => return Err(KeptReason::failed(error)),
			Err(Stopped) => return Err(KeptReason::Interrupted),
		}
	};
	if let Some(output) = output {
		match control
			.until_stopping(backend.file_state(output.uuid))
			.await
		{
			Ok(Ok(state)) if output.stands(&state) => {}
			Ok(Ok(_)) => return Err(KeptReason::Unconfirmed),
			Ok(Err(error)) => {
				tracing::warn!(
					"file {}: failed to confirm it before removing a source: {error}",
					output.uuid
				);
				return Err(KeptReason::Unconfirmed);
			}
			Err(Stopped) => return Err(KeptReason::Interrupted),
		}
	}
	Ok(held)
}

/// Removes one file source if it is still what the job read, holding the drive lock from the
/// check to the removal; a pause is waited out before the lock is taken. A cancel through
/// `removing` drops the check in flight, or ends the removal before its next request; a removal
/// already sent is waited for, so its outcome is known.
pub(crate) async fn dispose_file<B: DisposalBackend>(
	backend: &B,
	file: ExpectedFile,
	how: SourceDisposal,
	removing: Removing<'_>,
) -> DisposalOutcome {
	match remove_file(backend, file, how, removing).await {
		Ok(()) => DisposalOutcome::Disposed {
			how,
			bytes_freed: match how {
				SourceDisposal::Trash => 0,
				SourceDisposal::DeletePermanently => file.size,
			},
		},
		Err(reason) => DisposalOutcome::kept(reason),
	}
}

async fn remove_file<B: DisposalBackend>(
	backend: &B,
	file: ExpectedFile,
	how: SourceDisposal,
	removing: Removing<'_>,
) -> Result<(), KeptReason> {
	let _lock = lock_for_removal(backend, removing).await?;
	let control = removing.control;
	if !file.matches(&checked(control, backend.file_state(file.uuid)).await?) {
		return Err(KeptReason::Changed);
	}
	if how == SourceDisposal::DeletePermanently
		&& checked(control, backend.has_older_versions(file.uuid)).await?
	{
		return Err(KeptReason::HasVersions);
	}
	unless_stopping(control)?;
	match how {
		SourceDisposal::Trash => backend.trash_file(file.uuid).await,
		SourceDisposal::DeletePermanently => backend.delete_file_permanently(file.uuid).await,
	}
	.map_err(KeptReason::failed)
}

/// Removes one directory source if it is still where the job read it, out of the trash, and
/// everything below it is still what the job read, holding the drive lock from the check to the
/// removal; a pause is waited out before the lock is taken. A cancel through `removing` drops the
/// check or listing in flight, or ends the removal before its next request; a removal already
/// sent is waited for, so what it freed is known.
pub(crate) async fn dispose_dir<B: DisposalBackend>(
	backend: &B,
	expected: &ExpectedDir,
	how: SourceDisposal,
	removing: Removing<'_>,
	deleted: &mut BTreeSet<Uuid>,
) -> DisposalOutcome {
	// once files are deleted, whatever stops the removal leaves them deleted: every outcome says
	// how many
	let mut bytes_freed = 0;
	match remove_dir(backend, expected, how, removing, deleted, &mut bytes_freed).await {
		Ok(()) => DisposalOutcome::Disposed { how, bytes_freed },
		Err(reason) => DisposalOutcome::Kept {
			reason,
			bytes_freed,
		},
	}
}

async fn remove_dir<B: DisposalBackend>(
	backend: &B,
	expected: &ExpectedDir,
	how: SourceDisposal,
	removing: Removing<'_>,
	deleted: &mut BTreeSet<Uuid>,
	bytes_freed: &mut u64,
) -> Result<(), KeptReason> {
	let (dir, read) = (expected.uuid, &expected.read);
	let _lock = lock_for_removal(backend, removing).await?;
	let control = removing.control;
	if !expected.matches(&checked(control, backend.dir_state(dir)).await?)
		|| checked(control, backend.list_tree(dir)).await? != *read
	{
		return Err(KeptReason::Changed);
	}
	if how == SourceDisposal::DeletePermanently {
		// every file is checked before any is deleted: a directory is removed whole or not at
		// all for this reason
		for &uuid in read.files.keys() {
			if checked(control, backend.has_older_versions(uuid)).await? {
				return Err(KeptReason::HasVersions);
			}
		}
		for (&uuid, &size) in &read.files {
			unless_stopping(control)?;
			backend
				.delete_file_permanently(uuid)
				.await
				.map_err(KeptReason::failed)?;
			*bytes_freed += size;
			deleted.insert(uuid);
		}
		// only a directory the job emptied is trashed: anything that arrived since stays
		if !checked(control, backend.list_tree(dir))
			.await?
			.files
			.is_empty()
		{
			return Err(KeptReason::Changed);
		}
	}
	unless_stopping(control)?;
	backend.trash_dir(dir).await.map_err(KeptReason::failed)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_source_goes_with_the_outermost_one_it_lies_within() {
		use Nest::{Cyclic, Outermost, Within};
		// 2 in 1 in 0, and 3 on its own
		assert_eq!(
			nesting(&[None, Some(0), Some(1), None]),
			[Outermost, Within(0), Within(0), Outermost]
		);
		// 0 and 1 in each other, 2 in 1, 3 on its own: the loop and what is behind it go alone
		assert_eq!(
			nesting(&[Some(1), Some(0), Some(1), None]),
			[Cyclic, Cyclic, Cyclic, Outermost]
		);
		// a source within itself is a loop too
		assert_eq!(nesting(&[Some(0)]), [Cyclic]);
		assert_eq!(nesting(&[]), []);
	}
}
