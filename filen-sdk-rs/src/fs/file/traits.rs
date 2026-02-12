use chrono::{DateTime, Utc};
pub use filen_macros::{File, HasFileInfo, HasFileMeta, HasRemoteFileInfo};
use filen_types::crypto::Blake3Hash;

use crate::{
	consts::CHUNK_SIZE_U64,
	crypto::file::FileKey,
	error::Error,
	fs::{
		HasMeta, HasName, HasRemoteInfo, HasUUID,
		file::meta::{FileMeta, FileMetaChanges},
	},
};

pub trait HasRemoteFileInfo: HasRemoteInfo + HasFileInfo {
	fn region(&self) -> &str;
	fn bucket(&self) -> &str;
	fn hash(&self) -> Option<Blake3Hash>;
}

pub trait HasFileInfo {
	fn mime(&self) -> Option<&str>;
	fn created(&self) -> Option<DateTime<Utc>>;
	fn last_modified(&self) -> Option<DateTime<Utc>>;
	fn size(&self) -> u64;
	fn chunks(&self) -> u64 {
		self.size() / CHUNK_SIZE_U64
	}
	fn key(&self) -> Option<&FileKey>;
}

pub trait HasFileMeta {
	fn get_meta(&self) -> &FileMeta<'_>;
}

pub(crate) trait UpdateFileMeta {
	fn update_meta(&mut self, changes: FileMetaChanges) -> Result<(), Error>;
}

pub trait File:
	HasRemoteFileInfo + HasMeta + HasFileInfo + HasRemoteInfo + HasName + HasUUID + Sync
{
}
