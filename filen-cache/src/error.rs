use filen_sdk_rs::{
	error::FilenSdkError,
	fs::cache::CacheableConversionError,
	io::{RemoteDirectory, RemoteFile},
};
use uuid::Uuid;

#[derive(Debug)]
pub struct FileCacheableConversionFailed {
	pub file: RemoteFile,
	pub error: FilenSdkError,
}

#[derive(Debug)]
pub struct DirCacheableConversionFailed {
	pub dir: RemoteDirectory,
	pub error: FilenSdkError,
}

#[derive(Debug)]
pub struct DB {
	pub error: rusqlite::Error,
	pub context: String,
}

/// A socket event could not be converted into a cacheable form (e.g. its metadata was not
/// decrypted, or it referenced a non-uuid parent).
#[derive(Debug)]
pub struct EventConversionFailed {
	pub error: CacheableConversionError,
	pub uuid: Uuid,
}

#[derive(Debug)]
pub enum CacheError {
	FileCacheableConversion(FileCacheableConversionFailed),
	DirCacheableConversion(DirCacheableConversionFailed),
	EventConversion(EventConversionFailed),
	DB(DB),
}

impl CacheError {
	pub(crate) fn file_cachable_conversion(file: RemoteFile, error: FilenSdkError) -> Self {
		Self::FileCacheableConversion(FileCacheableConversionFailed { file, error })
	}

	pub(crate) fn dir_cachable_conversion(dir: RemoteDirectory, error: FilenSdkError) -> Self {
		Self::DirCacheableConversion(DirCacheableConversionFailed { dir, error })
	}

	pub(crate) fn db(error: rusqlite::Error, context: String) -> Self {
		Self::DB(DB { error, context })
	}

	pub(crate) fn event_conversion(error: CacheableConversionError, uuid: Uuid) -> Self {
		Self::EventConversion(EventConversionFailed { error, uuid })
	}
}
