//! Removing a job's sources once its result is verified: the archive after extracting it, the
//! items after compressing them.
//!
//! A source is only touched after a job that completed with nothing failed, skipped or left
//! unaccounted, whose output was confirmed with the server, and only if the source is still
//! exactly what the job read (checked again right before it is removed). Directories are only
//! ever trashed, never purged: a permanent removal deletes the files the job read one by one and
//! trashes what is left, so nothing the job did not read is ever deleted for good. Directory
//! sizes are never used, since the server caches them.

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
		categories::{DirType, Normal, fs::CategoryFS},
		drive_job::backend::{ClientBackend, DriveBackend},
		file::traits::HasFileInfo,
	},
	util::MaybeSend,
};

/// What to do with a job's sources once its result is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceDisposal {
	/// Move them to the trash, where they can be restored. Frees no storage until the trash is
	/// emptied.
	Trash,
	/// Delete the files for good (older versions of them are kept, as the drive keeps them);
	/// directories are trashed once emptied.
	DeletePermanently,
}

/// Why a source was kept.
#[derive(Debug, Clone)]
pub enum KeptReason {
	/// Something was not carried over: an entry failed or was skipped.
	Incomplete,
	/// The archive holds data after its last entry that belongs to none.
	UnaccountedData { bytes: u64 },
	/// What was read does not match the hash in the source's metadata.
	HashMismatch,
	/// The source's metadata holds no hash to check what was read against, which a permanent
	/// deletion requires.
	HashUnavailable,
	/// The source changed since the job read it: it moved, was trashed, got a new version, or
	/// holds other items now.
	Changed,
	/// The job's output could not be confirmed with the server.
	Unconfirmed,
	/// Removing it failed.
	Failed { error: Arc<Error> },
}

#[derive(Debug, Clone)]
pub enum DisposalOutcome {
	Disposed {
		how: SourceDisposal,
		/// Bytes of files deleted for good (0 when trashed).
		bytes_freed: u64,
	},
	Kept {
		reason: KeptReason,
	},
}

/// What became of one source.
#[derive(Debug, Clone)]
pub struct SourceDisposition {
	pub uuid: Uuid,
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

fn kept(reason: KeptReason) -> DisposalOutcome {
	DisposalOutcome::Kept { reason }
}

fn failed(error: Error) -> DisposalOutcome {
	kept(KeptReason::Failed {
		error: Arc::new(error),
	})
}

/// Removes one file source if it is still what the job read.
pub(crate) async fn dispose_file<B: DisposalBackend>(
	backend: &B,
	file: ExpectedFile,
	how: SourceDisposal,
) -> DisposalOutcome {
	let state = match backend.file_state(file.uuid).await {
		Ok(state) => state,
		Err(error) => return failed(error),
	};
	if !file.matches(&state) {
		return kept(KeptReason::Changed);
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

/// Removes one directory source if everything below it is still what the job read.
pub(crate) async fn dispose_dir<B: DisposalBackend>(
	backend: &B,
	dir: Uuid,
	read: &Tree,
	how: SourceDisposal,
) -> DisposalOutcome {
	match backend.list_tree(dir).await {
		Ok(listed) if listed == *read => {}
		Ok(_) => return kept(KeptReason::Changed),
		Err(error) => return failed(error),
	}
	let mut bytes_freed = 0;
	if how == SourceDisposal::DeletePermanently {
		for (&uuid, &size) in &read.files {
			if let Err(error) = backend.delete_file_permanently(uuid).await {
				return failed(error);
			}
			bytes_freed += size;
		}
		// only a directory the job emptied is trashed: anything that arrived since stays
		match backend.list_tree(dir).await {
			Ok(left) if left.files.is_empty() => {}
			Ok(_) => return kept(KeptReason::Changed),
			Err(error) => return failed(error),
		}
	}
	match backend.trash_dir(dir).await {
		Ok(()) => DisposalOutcome::Disposed { how, bytes_freed },
		Err(error) => failed(error),
	}
}
