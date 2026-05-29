//! Shared types for converting remote items into their cacheable representations.

use filen_types::fs::ParentUuid;
use thiserror::Error;

use crate::{Error, ErrorKind};

/// Reason a [`RemoteFile`](crate::io::RemoteFile) or
/// [`RemoteDirectory`](crate::io::RemoteDirectory) could not be converted into its cacheable
/// representation.
///
/// Unlike [`Error`], this is a small, self-contained, serializable enum, so it can be embedded
/// in cache events that are persisted to disk. It can be converted into an [`Error`] via [`From`].
#[derive(Debug, Clone, PartialEq, Eq, Error, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum CacheableConversionError {
	#[error("cannot convert remote item to a cacheable item with a non-uuid parent: {0}")]
	ParentNotUuid(ParentUuid),
	#[error(
		"cannot convert remote item to a cacheable item with metadata that was not successfully decrypted: {0}"
	)]
	MetadataNotDecrypted(String),
}

impl From<CacheableConversionError> for Error {
	fn from(value: CacheableConversionError) -> Self {
		let kind = match &value {
			CacheableConversionError::ParentNotUuid(_) => ErrorKind::InvalidState,
			CacheableConversionError::MetadataNotDecrypted(_) => ErrorKind::MetadataWasNotDecrypted,
		};
		Error::custom_with_source(kind, value, None::<&str>)
	}
}
