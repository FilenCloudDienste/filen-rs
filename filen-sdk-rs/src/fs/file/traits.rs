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
		self.size().div_ceil(CHUNK_SIZE_U64)
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

#[cfg(test)]
mod tests {
	use super::*;

	/// A minimal type that relies on the default `HasFileInfo::chunks()` implementation (it does
	/// not override it), used to exercise that default in isolation.
	struct DefaultChunks(u64);

	impl HasFileInfo for DefaultChunks {
		fn mime(&self) -> Option<&str> {
			None
		}

		fn created(&self) -> Option<DateTime<Utc>> {
			None
		}

		fn last_modified(&self) -> Option<DateTime<Utc>> {
			None
		}

		fn size(&self) -> u64 {
			self.0
		}

		fn key(&self) -> Option<&FileKey> {
			None
		}
	}

	/// The default `chunks()` must round up: a sub-chunk file still occupies one chunk. Floor
	/// division returned 0 for any file under 1 MiB, which makes `FileReader` yield an empty
	/// download with no error for an external implementor relying on the default.
	#[test]
	fn default_chunks_rounds_up() {
		assert_eq!(DefaultChunks(0).chunks(), 0);
		assert_eq!(DefaultChunks(1).chunks(), 1);
		assert_eq!(DefaultChunks(CHUNK_SIZE_U64 - 1).chunks(), 1);
		assert_eq!(DefaultChunks(CHUNK_SIZE_U64).chunks(), 1);
		assert_eq!(DefaultChunks(CHUNK_SIZE_U64 + 1).chunks(), 2);
		assert_eq!(DefaultChunks(3 * CHUNK_SIZE_U64).chunks(), 3);
	}
}
