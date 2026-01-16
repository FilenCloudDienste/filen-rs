use std::borrow::Cow;

use chrono::{DateTime, Utc};
use filen_types::fs::{ObjectType, UuidStr};

use crate::{
	crypto::file::FileKey,
	fs::{HasMeta, HasName, HasRemoteInfo, HasType, HasUUID, file::traits::File},
};

use super::{
	RemoteFile, RemoteRootFile,
	traits::{HasFileInfo, HasRemoteFileInfo},
};

pub enum RemoteFileType<'a> {
	File(Cow<'a, RemoteFile>),
	SharedFile(Cow<'a, RemoteRootFile>),
}

impl From<RemoteFile> for RemoteFileType<'static> {
	fn from(file: RemoteFile) -> Self {
		RemoteFileType::File(Cow::Owned(file))
	}
}

impl From<RemoteRootFile> for RemoteFileType<'static> {
	fn from(file: RemoteRootFile) -> Self {
		RemoteFileType::SharedFile(Cow::Owned(file))
	}
}

impl HasUUID for RemoteFileType<'_> {
	fn uuid(&self) -> &UuidStr {
		match self {
			RemoteFileType::File(file) => file.uuid(),
			RemoteFileType::SharedFile(file) => file.uuid(),
		}
	}
}

impl HasName for RemoteFileType<'_> {
	fn name(&self) -> Option<&str> {
		match self {
			RemoteFileType::File(file) => file.name(),
			RemoteFileType::SharedFile(file) => file.name(),
		}
	}
}

impl HasMeta for RemoteFileType<'_> {
	fn get_meta_string(&self) -> Option<Cow<'_, str>> {
		match self {
			RemoteFileType::File(file) => file.get_meta_string(),
			RemoteFileType::SharedFile(file) => file.get_meta_string(),
		}
	}
}

impl HasType for RemoteFileType<'_> {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteFileType<'_> {
	fn mime(&self) -> Option<&str> {
		match self {
			RemoteFileType::File(file) => file.mime(),
			RemoteFileType::SharedFile(file) => file.mime(),
		}
	}

	fn created(&self) -> Option<DateTime<Utc>> {
		match self {
			RemoteFileType::File(file) => file.created(),
			RemoteFileType::SharedFile(file) => file.created(),
		}
	}

	fn last_modified(&self) -> Option<DateTime<Utc>> {
		match self {
			RemoteFileType::File(file) => file.last_modified(),
			RemoteFileType::SharedFile(file) => file.last_modified(),
		}
	}

	fn size(&self) -> u64 {
		match self {
			RemoteFileType::File(file) => file.size(),
			RemoteFileType::SharedFile(file) => file.size(),
		}
	}

	fn chunks(&self) -> u64 {
		match self {
			RemoteFileType::File(file) => file.chunks(),
			RemoteFileType::SharedFile(file) => file.chunks(),
		}
	}

	fn key(&self) -> Option<&FileKey> {
		match self {
			RemoteFileType::File(file) => file.key(),
			RemoteFileType::SharedFile(file) => file.key(),
		}
	}
}

impl HasRemoteInfo for RemoteFileType<'_> {
	fn favorited(&self) -> bool {
		match self {
			RemoteFileType::File(file) => file.favorited(),
			RemoteFileType::SharedFile(file) => file.favorited(),
		}
	}

	fn timestamp(&self) -> DateTime<Utc> {
		match self {
			RemoteFileType::File(file) => file.timestamp(),
			RemoteFileType::SharedFile(file) => file.timestamp(),
		}
	}
}

impl HasRemoteFileInfo for RemoteFileType<'_> {
	fn region(&self) -> &str {
		match self {
			RemoteFileType::File(file) => file.region(),
			RemoteFileType::SharedFile(file) => file.region(),
		}
	}

	fn bucket(&self) -> &str {
		match self {
			RemoteFileType::File(file) => file.bucket(),
			RemoteFileType::SharedFile(file) => file.bucket(),
		}
	}

	fn hash(&self) -> Option<filen_types::crypto::Blake3Hash> {
		match self {
			RemoteFileType::File(file) => file.hash(),
			RemoteFileType::SharedFile(file) => file.hash(),
		}
	}
}

impl File for RemoteFileType<'_> {}
