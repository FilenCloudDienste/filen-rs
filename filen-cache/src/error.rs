use filen_sdk_rs::{
	error::FilenSdkError,
	io::{RemoteDirectory, RemoteFile},
};

/// A remote file from the SDK could not be converted into its cacheable form (e.g. metadata decrypt
/// failure). The original `file` is retained so the caller can log or inspect the offending record.
#[derive(Debug)]
pub struct FileCacheableConversionFailed {
	pub file: RemoteFile,
	pub error: FilenSdkError,
}

/// A remote directory from the SDK could not be converted into its cacheable form. The original `dir`
/// is retained so the caller can log or inspect the offending record.
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

#[derive(Debug)]
pub enum CacheError {
	FileCacheableConversion(FileCacheableConversionFailed),
	DirCacheableConversion(DirCacheableConversionFailed),
	DB(DB),
	/// A `CacheEvent` could not be encoded into its durable rkyv blob for the `events` table.
	Serialization(String),
	/// A per-sync-root [`SyncRootCallback`](crate::SyncRootCallback) panicked during dispatch; the
	/// panic was caught so other roots/events still apply. The string is the root uuid + panic payload.
	SyncRootCallbackPanic(String),
	/// An `AddSyncRoot` uuid could not be validated as a reachable directory, so the root was NOT added.
	/// `message` is the underlying SDK error. The app should re-issue `add_sync_root` with a valid uuid
	/// (or retry the same one if the failure was transient).
	InvalidSyncRoot {
		uuid: uuid::Uuid,
		message: String,
	},
}

impl CacheError {
	pub(crate) fn file_cacheable_conversion(file: RemoteFile, error: FilenSdkError) -> Self {
		Self::FileCacheableConversion(FileCacheableConversionFailed { file, error })
	}

	pub(crate) fn dir_cacheable_conversion(dir: RemoteDirectory, error: FilenSdkError) -> Self {
		Self::DirCacheableConversion(DirCacheableConversionFailed { dir, error })
	}

	pub(crate) fn db(error: rusqlite::Error, context: String) -> Self {
		Self::DB(DB { error, context })
	}

	pub(crate) fn serialization(error: impl ToString) -> Self {
		Self::Serialization(error.to_string())
	}

	pub(crate) fn sync_root_callback_panic(message: String) -> Self {
		Self::SyncRootCallbackPanic(message)
	}

	pub(crate) fn invalid_sync_root(uuid: uuid::Uuid, message: String) -> Self {
		Self::InvalidSyncRoot { uuid, message }
	}
}

impl std::fmt::Display for CacheError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::FileCacheableConversion(e) => {
				write!(
					f,
					"failed to convert a remote file into a cacheable form: {}",
					e.error
				)
			}
			Self::DirCacheableConversion(e) => {
				write!(
					f,
					"failed to convert a remote dir into a cacheable form: {}",
					e.error
				)
			}
			Self::DB(db) => write!(f, "cache database error ({}): {}", db.context, db.error),
			Self::Serialization(msg) => write!(f, "event serialization error: {msg}"),
			Self::SyncRootCallbackPanic(msg) => write!(f, "sync-root callback panicked: {msg}"),
			Self::InvalidSyncRoot { uuid, message } => {
				write!(f, "invalid sync root {uuid}: {message}")
			}
		}
	}
}

impl std::error::Error for CacheError {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		match self {
			Self::DB(db) => Some(&db.error),
			Self::FileCacheableConversion(e) => Some(&e.error),
			Self::DirCacheableConversion(e) => Some(&e.error),
			Self::Serialization(_)
			| Self::SyncRootCallbackPanic(_)
			| Self::InvalidSyncRoot { .. } => None,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// `CacheError` is `Display` + `Error`, so consumers get `.to_string()` and an error `source()` chain
	/// instead of being forced to `{:?}` / pattern-match.
	#[test]
	fn display_and_source_are_wired() {
		let db = CacheError::db(
			rusqlite::Error::QueryReturnedNoRows,
			"while testing".to_string(),
		);
		assert!(
			db.to_string().contains("while testing"),
			"Display carries the context: {db}"
		);
		assert!(
			std::error::Error::source(&db).is_some(),
			"DB exposes its rusqlite source"
		);

		let ser = CacheError::serialization("boom");
		assert!(ser.to_string().contains("boom"));
		assert!(
			std::error::Error::source(&ser).is_none(),
			"Serialization has no underlying source"
		);
	}
}
