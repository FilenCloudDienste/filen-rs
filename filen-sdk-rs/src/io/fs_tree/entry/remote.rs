use std::cmp::Ordering;

use uuid::Uuid;

use crate::{
	fs::{
		HasName, HasRemoteInfo, HasUUID,
		dir::{RemoteDirectory, meta::DirectoryMeta, traits::HasDirMeta},
		enums::NonRootFSObject,
		file::{
			RemoteFile,
			meta::FileMeta,
			traits::{HasFileInfo, HasFileMeta},
		},
	},
	io::fs_tree::WalkError,
};

pub(crate) struct RemoteFSObjectEntry<'a> {
	obj: NonRootFSObject<'a>,
	depth: usize,
}

impl<'a> RemoteFSObjectEntry<'a> {
	pub(crate) fn new(obj: NonRootFSObject<'a>, depth: usize) -> Self {
		Self { obj, depth }
	}

	pub(crate) fn depth(&self) -> usize {
		self.depth
	}

	pub(crate) fn obj(&self) -> &NonRootFSObject<'a> {
		&self.obj
	}

	pub(crate) fn into_obj(self) -> NonRootFSObject<'a> {
		self.obj
	}
}

impl super::DFSWalkerEntry for RemoteFSObjectEntry<'_> {
	type WalkerFileEntry = RemoteFile;
	type WalkerDirEntry = RemoteDirectory;
	type CompareStrategy = RemoteCompareStrategy;

	fn depth(&self) -> usize {
		self.depth
	}

	fn name(&self) -> Result<&str, WalkError> {
		HasName::name(&self.obj).ok_or_else(|| WalkError::EncryptedMeta(self.obj.uuid().into()))
	}

	fn into_entry_type(self) -> super::EntryType<RemoteDirectory, RemoteFile> {
		match self.obj {
			NonRootFSObject::Dir(dir) => super::EntryType::Dir(dir.into_owned()),
			NonRootFSObject::File(file) => super::EntryType::File(file.into_owned()),
		}
	}
}

impl super::DFSWalkerFileEntry for RemoteFile {
	type Extra = RemoteFile;

	fn into_extra_data(self) -> Self::Extra {
		self
	}

	fn size(&self) -> Result<u64, WalkError> {
		Ok(HasFileInfo::size(self))
	}
}

impl super::DFSWalkerDirEntry for RemoteDirectory {
	type Extra = RemoteDirectory;

	fn into_extra_data(self) -> Self::Extra {
		self
	}
}

pub(crate) struct RemoteCompareStrategy;
impl super::CompareStrategy<RemoteDirectory, RemoteFile> for RemoteCompareStrategy {
	fn should_replace(
		existing: &super::Entry<RemoteDirectory, RemoteFile>,
		new: &super::Entry<RemoteDirectory, RemoteFile>,
	) -> bool {
		let replace = {
			match (existing, new) {
				// prefer dirs over files
				(super::Entry::Dir(_), super::Entry::File(_)) => false,
				(super::Entry::File(_), super::Entry::Dir(_)) => true,
				(super::Entry::File(existing), super::Entry::File(new)) => {
					let existing = existing.extra_data();
					let new = new.extra_data();
					match (existing.get_meta(), new.get_meta()) {
						(FileMeta::Decoded(existing_meta), FileMeta::Decoded(new_meta)) => {
							match existing_meta.last_modified.cmp(&new_meta.last_modified) {
								Ordering::Less => return true,
								Ordering::Greater => return false,
								Ordering::Equal => {}
							}
							match existing_meta.created.cmp(&new_meta.created) {
								Ordering::Less => return true,
								Ordering::Greater => return false,
								Ordering::Equal => {}
							}
						}
						(_, FileMeta::Decoded(_)) => return true,
						_ => {}
					}
					match existing.timestamp().cmp(&new.timestamp()) {
						Ordering::Less => true,
						Ordering::Greater => false,
						Ordering::Equal => Uuid::from(existing.uuid()) < Uuid::from(new.uuid()),
					}
				}
				(super::Entry::Dir(existing), super::Entry::Dir(new)) => {
					let existing = existing.extra_data();
					let new = new.extra_data();
					match (existing.get_meta(), new.get_meta()) {
						(
							DirectoryMeta::Decoded(existing_meta),
							DirectoryMeta::Decoded(new_meta),
						) => {
							if let (Some(existing_created), Some(new_created)) =
								(existing_meta.created, new_meta.created)
							{
								match existing_created.cmp(&new_created) {
									Ordering::Less => return true,
									Ordering::Greater => return false,
									Ordering::Equal => {}
								}
							}
						}
						(_, DirectoryMeta::Decoded(_)) => return true,
						_ => {}
					};
					match existing.timestamp().cmp(&new.timestamp()) {
						Ordering::Less => true,
						Ordering::Greater => false,
						Ordering::Equal => Uuid::from(existing.uuid()) < Uuid::from(new.uuid()),
					}
				}
			}
		};

		log::info!(
			"Conflict detected between entries. Existing: {:?}, New: {:?}. Resolved by choosing new: {}",
			existing,
			new,
			replace
		);
		replace
	}
}
