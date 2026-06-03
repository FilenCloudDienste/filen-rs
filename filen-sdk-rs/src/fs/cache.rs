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

/// Helpers for the canonical change-detection fingerprint (`content_fingerprint`) on
/// [`CacheableFile`](crate::fs::file::cache::CacheableFile) and
/// [`CacheableDir`](crate::fs::dir::cache::CacheableDir).
///
/// The fingerprint must be byte-identical across the two paths that build a cacheable item — a live
/// socket event versus a recursive listing — so timestamps are canonicalized to whole milliseconds
/// (the precision baked into the encrypted metadata at write time).
#[cfg(feature = "cache")]
pub(crate) mod fingerprint {
	use chrono::{DateTime, Utc};

	/// Mix a timestamp into the hasher, canonicalized to whole milliseconds — robust to any
	/// sub-millisecond artifact from an rkyv at-rest round-trip versus a fresh decode.
	pub(crate) fn write_dt_ms(hasher: &mut blake3::Hasher, dt: DateTime<Utc>) {
		hasher.update(&dt.timestamp_millis().to_le_bytes());
	}

	/// Mix an optional timestamp, distinguishing `None` from `Some(_)` with a tag byte.
	pub(crate) fn write_opt_dt_ms(hasher: &mut blake3::Hasher, dt: Option<DateTime<Utc>>) {
		match dt {
			Some(dt) => {
				hasher.update(&[1]);
				write_dt_ms(hasher, dt);
			}
			None => {
				hasher.update(&[0]);
			}
		}
	}
}
