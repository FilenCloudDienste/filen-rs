//! Removing a job's sources once its result is verified: the archive after extracting it, the
//! items after compressing them.
//!
//! A source is only touched after a job that completed with nothing failed, skipped or left
//! unaccounted, whose output was confirmed with the server, and only if the source is still
//! exactly what the job read (checked again right before it is removed). Directories are only
//! ever trashed, never purged: a permanent removal deletes the files the job read one by one and
//! trashes what is left, so nothing the job did not read is ever deleted for good. Directory
//! sizes are never used, since the server caches them.
//!
//! A listing holds only finished uploads: a file another device is still uploading into a source
//! directory is not seen, and ends up in the trash with the directory, where it can be
//! restored.

use filen_macros::js_type;
use std::{
	collections::{BTreeMap, BTreeSet},
	future::Future,
	sync::Arc,
};

use filen_types::fs::{ParentUuid, Uuid};

use crate::{
	Error,
	fs::{
		HasUUID,
		categories::{DirType, NonRootItemType, Normal, fs::CategoryFS},
		drive_job::backend::{ClientBackend, DriveBackend},
		file::traits::HasFileInfo,
	},
	job::{JobControl, Stopped},
	util::MaybeSend,
};

/// What to do with a job's sources once its result is verified.
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
	/// deletion requires.
	HashUnavailable,
	/// The source changed since the job read it: it moved, was trashed, got a new version, or
	/// holds other items now.
	Changed,
	/// The job's output could not be confirmed: the server did not hold what was created, the
	/// archive was not read in full, or something the job extracted was checked by nothing (a
	/// 7z entry without a CRC-32, or the files of a brotli or LZMA-alone stream, or of an lz4,
	/// xz or zstd stream written without its optional checksum).
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
	fn list_tree(&self, dir: Uuid) -> impl Future<Output = Result<Tree, Error>> + MaybeSend;
	fn trash_file(&self, uuid: Uuid) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	fn delete_file_permanently(
		&self,
		uuid: Uuid,
	) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	fn trash_dir(&self, uuid: Uuid) -> impl Future<Output = Result<(), Error>> + MaybeSend;
	/// Whether the file has older versions.
	fn has_older_versions(
		&self,
		uuid: Uuid,
	) -> impl Future<Output = Result<bool, Error>> + MaybeSend;
	/// An item of the user's drive, for a job that kept only its uuid.
	fn normal_item(
		&self,
		uuid: Uuid,
		is_dir: bool,
	) -> impl Future<Output = Result<NonRootItemType<'static, Normal>, Error>> + MaybeSend;
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
		let client = self.client();
		let mut file = client.get_file(uuid).await?;
		client.trash_file(&mut file).await
	}

	async fn delete_file_permanently(&self, uuid: Uuid) -> Result<(), Error> {
		let client = self.client();
		let file = client.get_file(uuid).await?;
		client.delete_file_permanently(file).await
	}

	async fn trash_dir(&self, uuid: Uuid) -> Result<(), Error> {
		let client = self.client();
		let mut dir = client.get_dir(uuid).await?;
		client.trash_dir(&mut dir).await
	}

	async fn has_older_versions(&self, uuid: Uuid) -> Result<bool, Error> {
		let client = self.client();
		let file = client.get_file(uuid).await?;
		let versions = client.list_file_versions(&file).await?;
		Ok(versions.iter().any(|version| version.uuid() != uuid))
	}

	async fn normal_item(
		&self,
		uuid: Uuid,
		is_dir: bool,
	) -> Result<NonRootItemType<'static, Normal>, Error> {
		let client = self.client();
		Ok(if is_dir {
			NonRootItemType::Dir(std::borrow::Cow::Owned(client.get_dir(uuid).await?))
		} else {
			NonRootItemType::File(std::borrow::Cow::Owned(client.get_file(uuid).await?))
		})
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
			outcome: kept(reason.clone()),
		})
		.collect()
}

fn kept(reason: KeptReason) -> DisposalOutcome {
	DisposalOutcome::Kept {
		reason,
		bytes_freed: 0,
	}
}

fn failed(error: Error) -> DisposalOutcome {
	kept(KeptReason::Failed {
		error: Arc::new(error),
	})
}

/// Removes one file source if it is still what the job read. A cancel through `control` drops
/// the check in flight, or ends the removal before its next request; a removal already sent is
/// waited for, so its outcome is known.
pub(crate) async fn dispose_file<B: DisposalBackend>(
	backend: &B,
	file: ExpectedFile,
	how: SourceDisposal,
	control: &JobControl,
) -> DisposalOutcome {
	let state = match control.until_stopping(backend.file_state(file.uuid)).await {
		Ok(Ok(state)) => state,
		Ok(Err(error)) => return failed(error),
		Err(Stopped) => return kept(KeptReason::Interrupted),
	};
	if !file.matches(&state) {
		return kept(KeptReason::Changed);
	}
	if how == SourceDisposal::DeletePermanently {
		match control
			.until_stopping(backend.has_older_versions(file.uuid))
			.await
		{
			Ok(Ok(false)) => {}
			Ok(Ok(true)) => return kept(KeptReason::HasVersions),
			Ok(Err(error)) => return failed(error),
			Err(Stopped) => return kept(KeptReason::Interrupted),
		}
	}
	if control.is_stopping() {
		return kept(KeptReason::Interrupted);
	}
	let removed = match how {
		SourceDisposal::Trash => backend.trash_file(file.uuid).await,
		SourceDisposal::DeletePermanently => backend.delete_file_permanently(file.uuid).await,
	};
	match removed {
		Ok(()) => DisposalOutcome::Disposed {
			how,
			bytes_freed: match how {
				SourceDisposal::Trash => 0,
				SourceDisposal::DeletePermanently => file.size,
			},
		},
		Err(error) => failed(error),
	}
}

/// Removes one directory source if everything below it is still what the job read. A cancel
/// through `control` drops the check or listing in flight, or ends the removal before its next
/// request; a removal already sent is waited for, so what it freed is known.
pub(crate) async fn dispose_dir<B: DisposalBackend>(
	backend: &B,
	dir: Uuid,
	read: &Tree,
	how: SourceDisposal,
	control: &JobControl,
	deleted: &mut BTreeSet<Uuid>,
) -> DisposalOutcome {
	match control.until_stopping(backend.list_tree(dir)).await {
		Ok(Ok(listed)) if listed == *read => {}
		Ok(Ok(_)) => return kept(KeptReason::Changed),
		Ok(Err(error)) => return failed(error),
		Err(Stopped) => return kept(KeptReason::Interrupted),
	}
	let mut bytes_freed = 0;
	// once files are deleted, whatever stops the removal leaves them deleted: every outcome says
	// how many
	let partly = |reason, bytes_freed| DisposalOutcome::Kept {
		reason,
		bytes_freed,
	};
	if how == SourceDisposal::DeletePermanently {
		// every file is checked before any is deleted: a directory is removed whole or not at
		// all for this reason
		for &uuid in read.files.keys() {
			match control
				.until_stopping(backend.has_older_versions(uuid))
				.await
			{
				Ok(Ok(false)) => {}
				Ok(Ok(true)) => return kept(KeptReason::HasVersions),
				Ok(Err(error)) => return failed(error),
				Err(Stopped) => return kept(KeptReason::Interrupted),
			}
		}
		for (&uuid, &size) in &read.files {
			if control.is_stopping() {
				return partly(KeptReason::Interrupted, bytes_freed);
			}
			if let Err(error) = backend.delete_file_permanently(uuid).await {
				return partly(
					KeptReason::Failed {
						error: Arc::new(error),
					},
					bytes_freed,
				);
			}
			bytes_freed += size;
			deleted.insert(uuid);
		}
		// only a directory the job emptied is trashed: anything that arrived since stays
		match control.until_stopping(backend.list_tree(dir)).await {
			Ok(Ok(left)) if left.files.is_empty() => {}
			Ok(Ok(_)) => return partly(KeptReason::Changed, bytes_freed),
			Ok(Err(error)) => {
				return partly(
					KeptReason::Failed {
						error: Arc::new(error),
					},
					bytes_freed,
				);
			}
			Err(Stopped) => return partly(KeptReason::Interrupted, bytes_freed),
		}
	}
	if control.is_stopping() {
		return partly(KeptReason::Interrupted, bytes_freed);
	}
	match backend.trash_dir(dir).await {
		Ok(()) => DisposalOutcome::Disposed { how, bytes_freed },
		Err(error) => partly(
			KeptReason::Failed {
				error: Arc::new(error),
			},
			bytes_freed,
		),
	}
}
