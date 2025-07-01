use chrono::{DateTime, Utc};
use filen_types::fs::{ObjectType, UuidStr};

use crate::{
	crypto::file::FileKey,
	fs::{HasMeta, HasName, HasRemoteInfo, HasType, HasUUID},
};

use super::{
	RemoteFile, RemoteRootFile,
	traits::{HasFileInfo, HasRemoteFileInfo},
};

pub enum RemoteFileType {
	File(RemoteFile),
	SharedFile(RemoteRootFile),
}

impl From<RemoteFile> for RemoteFileType {
	fn from(file: RemoteFile) -> Self {
		RemoteFileType::File(file)
	}
}

impl From<RemoteRootFile> for RemoteFileType {
	fn from(file: RemoteRootFile) -> Self {
		RemoteFileType::SharedFile(file)
	}
}

impl HasUUID for RemoteFileType {
	fn uuid(&self) -> UuidStr {
		match self {
			RemoteFileType::File(file) => file.uuid(),
			RemoteFileType::SharedFile(file) => file.uuid(),
		}
	}
}

impl HasName for RemoteFileType {
	fn name(&self) -> &str {
		match self {
			RemoteFileType::File(file) => file.name(),
			RemoteFileType::SharedFile(file) => file.name(),
		}
	}
}

impl HasMeta for RemoteFileType {
	fn get_meta_string(&self) -> String {
		match self {
			RemoteFileType::File(file) => file.get_meta_string(),
			RemoteFileType::SharedFile(file) => file.get_meta_string(),
		}
	}
}

impl HasType for RemoteFileType {
	fn object_type(&self) -> ObjectType {
		ObjectType::File
	}
}

impl HasFileInfo for RemoteFileType {
	fn mime(&self) -> &str {
		match self {
			RemoteFileType::File(file) => file.mime(),
			RemoteFileType::SharedFile(file) => file.mime(),
		}
	}

	fn created(&self) -> DateTime<Utc> {
		match self {
			RemoteFileType::File(file) => file.created(),
			RemoteFileType::SharedFile(file) => file.created(),
		}
	}

	fn last_modified(&self) -> DateTime<Utc> {
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

	fn key(&self) -> &FileKey {
		match self {
			RemoteFileType::File(file) => file.key(),
			RemoteFileType::SharedFile(file) => file.key(),
		}
	}
}

impl HasRemoteInfo for RemoteFileType {
	fn favorited(&self) -> bool {
		match self {
			RemoteFileType::File(file) => file.favorited(),
			RemoteFileType::SharedFile(file) => file.favorited(),
		}
	}
}

impl HasRemoteFileInfo for RemoteFileType {
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

	fn hash(&self) -> Option<filen_types::crypto::Sha512Hash> {
		match self {
			RemoteFileType::File(file) => file.hash(),
			RemoteFileType::SharedFile(file) => file.hash(),
		}
	}
}
