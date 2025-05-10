use chrono::{DateTime, Utc};
use filen_types::crypto::Sha512Hash;

use crate::{
	consts::CHUNK_SIZE_U64,
	crypto::file::FileKey,
	fs::{HasMeta, HasName, HasRemoteInfo, HasUUID},
};

use super::meta::FileMeta;

pub trait HasRemoteFileInfo: HasRemoteInfo + HasFileInfo {
	fn region(&self) -> &str;
	fn bucket(&self) -> &str;
	fn hash(&self) -> Option<Sha512Hash>;
}

pub trait HasFileInfo {
	fn mime(&self) -> &str;
	fn created(&self) -> DateTime<Utc>;
	fn last_modified(&self) -> DateTime<Utc>;
	fn size(&self) -> u64;
	fn chunks(&self) -> u64 {
		self.size() / CHUNK_SIZE_U64
	}
	fn key(&self) -> &FileKey;
}

pub trait HasFileMeta {
	fn borrow_meta(&self) -> FileMeta<'_>;
	fn get_meta(&self) -> FileMeta<'static>;
}

pub(crate) trait SetFileMeta {
	fn set_meta(&mut self, meta: FileMeta<'_>);
}

pub trait File:
	HasRemoteFileInfo + HasMeta + HasFileInfo + HasRemoteInfo + HasName + HasUUID + Sync
{
}
