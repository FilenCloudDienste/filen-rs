//! What the helper's INFO, WARN and ERROR lines show of the user's items. The helper logs at INFO
//! by default and the desktop app keeps what it logs in files, so those lines never carry a file
//! or folder name: they name an item by its ids. The sync root's own location and the helper's
//! state and data directories are not the user's items: lines about them name them as they are.

use std::fmt;

use filen_mobile_native_cache::CacheError;
use tracing::debug;

/// An error a line at INFO or WARN shows. The cache's text may quote a name it or the server put
/// in it, so an error the cache made shows its kind alone ([`ErrorText::Cache`]), after its text
/// was logged at DEBUG; every other error shows its text, which names no item.
pub(crate) trait LoggedError: fmt::Display {
	/// The cache's error this one is, or carries.
	fn cache_error(&self) -> Option<&CacheError>;
}

impl LoggedError for CacheError {
	fn cache_error(&self) -> Option<&CacheError> {
		Some(self)
	}
}

impl<E: LoggedError + ?Sized> LoggedError for &E {
	fn cache_error(&self) -> Option<&CacheError> {
		(**self).cache_error()
	}
}

/// What a line at INFO or WARN shows of an error ([`error`]).
pub(crate) enum ErrorText<'a> {
	/// An error that names no item, as it is.
	Full(&'a dyn fmt::Display),
	/// The kind of an error the cache made.
	Cache(&'static str),
}

impl fmt::Display for ErrorText<'_> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Full(error) => error.fmt(f),
			Self::Cache(kind) => write!(f, "the cache failed ({kind})"),
		}
	}
}

/// What a line at INFO or WARN shows of `error` ([`LoggedError`]).
pub(crate) fn error(error: &impl LoggedError) -> ErrorText<'_> {
	match error.cache_error() {
		Some(cache) => {
			debug!(%error, "a cache error in full");
			ErrorText::Cache(cache_error_kind(cache))
		}
		None => ErrorText::Full(error),
	}
}

/// `error`'s kind. Exhaustive, so a new variant does not compile until it is named here.
fn cache_error_kind(error: &CacheError) -> &'static str {
	match error {
		CacheError::SQL(_) => "sql",
		CacheError::SDK(_) => "sdk",
		CacheError::Conversion(_) => "conversion",
		CacheError::IO(_) => "io",
		CacheError::Remote(_) => "remote",
		CacheError::Image(_) => "image",
		CacheError::Unauthenticated(_) => "unauthenticated",
		CacheError::Disabled(_) => "disabled",
		CacheError::DoesNotExist(_) => "does not exist",
		CacheError::Unsupported(_) => "unsupported",
		CacheError::NotADirectory(_) => "not a directory",
		CacheError::FailedToDecrypt(_) => "failed to decrypt",
		CacheError::InvalidName(_) => "invalid name",
		CacheError::SyncAnchorExpired(_) => "sync anchor expired",
		CacheError::Aborted(_) => "aborted",
		CacheError::ContentHashMismatch(_) => "content hash mismatch",
	}
}

#[cfg(test)]
mod tests;
