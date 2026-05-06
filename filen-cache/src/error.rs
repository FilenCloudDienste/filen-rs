use filen_sdk_rs::{
	error::FilenSdkError,
	fs::{dir::meta::DirectoryMeta, file::meta::FileMeta},
	io::{RemoteDirectory, RemoteFile},
};
use uuid::Uuid;

#[derive(Debug)]
pub struct FileCacheableConversionFailed {
	pub file: RemoteFile,
	pub error: FilenSdkError,
}

#[derive(Debug)]
pub struct FileMetaNotDecryptable {
	pub meta: FileMeta<'static>,
	pub id: Uuid,
}

#[derive(Debug)]
pub struct DirCacheableConversionFailed {
	pub dir: RemoteDirectory,
	pub error: FilenSdkError,
}

#[derive(Debug)]
pub struct DirMetaNotDecryptable {
	pub meta: DirectoryMeta<'static>,
	pub id: Uuid,
}

#[derive(Debug)]
pub struct DB {
	pub error: rusqlite::Error,
	pub context: String,
}

#[derive(Debug)]
pub enum CacheError {
	FileCacheableConversion(FileCacheableConversionFailed),
	FileMetaNotDecryptable(FileMetaNotDecryptable),
	DirCacheableConversion(DirCacheableConversionFailed),
	DirMetaNotDecryptable(DirMetaNotDecryptable),
	DB(DB),
}

impl CacheError {
	pub(crate) fn file_cachable_conversion(file: RemoteFile, error: FilenSdkError) -> Self {
		Self::FileCacheableConversion(FileCacheableConversionFailed { file, error })
	}

	pub(crate) fn file_meta_not_decryptable(meta: FileMeta<'static>, id: Uuid) -> Self {
		Self::FileMetaNotDecryptable(FileMetaNotDecryptable { meta, id })
	}

	pub(crate) fn dir_cachable_conversion(dir: RemoteDirectory, error: FilenSdkError) -> Self {
		Self::DirCacheableConversion(DirCacheableConversionFailed { dir, error })
	}

	pub(crate) fn dir_meta_not_decryptable(meta: DirectoryMeta<'static>, id: Uuid) -> Self {
		Self::DirMetaNotDecryptable(DirMetaNotDecryptable { meta, id })
	}

	pub(crate) fn db(error: rusqlite::Error, context: String) -> Self {
		Self::DB(DB { error, context })
	}
}
