use chrono::{DateTime, Utc};
use filen_types::crypto::Blake3Hash;
use uuid::Uuid;

use crate::{
	fs::{
		HasName, HasUUID,
		dir::{RemoteDirectory, meta::DirectoryMeta, traits::HasDirMeta},
		enums::NonRootFSObject,
		file::{
			RemoteFile,
			meta::FileMeta,
			traits::{HasFileInfo, HasFileMeta},
		},
	},
	io::WalkError,
};

pub(crate) struct RemoteFSObjectEntry<'a> {
	obj: NonRootFSObject<'a>,
	depth: usize,
}

impl super::DFSWalkerEntry for RemoteFSObjectEntry<'_> {
	type WalkerFileEntry<'a>
		= &'a RemoteFile
	where
		Self: 'a;
	type WalkerDirEntry<'a>
		= &'a RemoteDirectory
	where
		Self: 'a;

	fn depth(&self) -> usize {
		self.depth
	}

	fn name(&self) -> Result<&str, WalkError> {
		HasName::name(&self.obj).ok_or_else(|| WalkError::EncryptedMeta(self.obj.uuid().into()))
	}

	fn entry_type(&self) -> super::EntryType<&RemoteDirectory, &RemoteFile> {
		match &self.obj {
			NonRootFSObject::Dir(dir) => super::EntryType::Dir(dir),
			NonRootFSObject::File(file) => super::EntryType::File(file),
		}
	}
}

#[derive(Copy, Clone)]
pub(crate) struct ExtraRemoteFileData {
	uuid: Uuid,
	size: u64,
	modified: DateTime<Utc>,
	created: Option<DateTime<Utc>>,
	hash: Option<Blake3Hash>,
}

impl super::DFSWalkerFileEntry for &RemoteFile {
	type Extra = ExtraRemoteFileData;

	fn into_extra_data(self) -> Result<Self::Extra, WalkError> {
		match self.get_meta() {
			FileMeta::Decoded(decrypted_file_meta) => Ok(ExtraRemoteFileData {
				uuid: self.uuid().into(),
				size: self.size(),
				modified: decrypted_file_meta.last_modified(),
				created: decrypted_file_meta.created(),
				hash: decrypted_file_meta.hash(),
			}),
			_ => Err(WalkError::EncryptedMeta(self.uuid().into())),
		}
	}

	fn size(&self) -> Result<u64, WalkError> {
		Ok(HasFileInfo::size(*self))
	}
}

#[derive(Copy, Clone)]
pub(crate) struct ExtraRemoteDirData {
	uuid: Uuid,
	created: Option<DateTime<Utc>>,
}

impl super::DFSWalkerDirEntry for &RemoteDirectory {
	type Extra = ExtraRemoteDirData;

	fn into_extra_data(self) -> Result<Self::Extra, WalkError> {
		match self.get_meta() {
			DirectoryMeta::Decoded(decrypted_dir_meta) => Ok(ExtraRemoteDirData {
				uuid: self.uuid().into(),
				created: decrypted_dir_meta.created(),
			}),
			_ => Err(WalkError::EncryptedMeta(self.uuid().into())),
		}
	}
}
