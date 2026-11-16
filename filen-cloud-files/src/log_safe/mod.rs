//! What the helper's INFO, WARN and ERROR lines show of the user's items. The helper logs at INFO
//! by default and the desktop app keeps what it logs in files, so those lines never carry a file
//! or folder name: they name an item by its ids, and one known only by its path by a [`Tag`]. The
//! path itself is logged at DEBUG, beside its tag, so a debug log ties each tag to its path. The
//! sync root's own location and the helper's state and data directories are not the user's items:
//! lines about them name them as they are.

use std::{
	fmt,
	hash::{BuildHasher, Hash, RandomState},
	sync::OnceLock,
};

use filen_mobile_native_cache::CacheError;
use tracing::debug;

use crate::root_path::RootPath;

/// Keys every tag, anew each run, so a tag names nothing outside the run.
static TAG_KEY: OnceLock<RandomState> = OnceLock::new();

/// An item's tag in the log: 32 bits of a hash keyed anew each run. Two lines of one run about one
/// item carry one tag; nothing outside the run maps it back to what was hashed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tag(u64);

impl Tag {
	/// The tag of `value`: an item's id, or its path.
	pub(crate) fn of(value: &(impl Hash + ?Sized)) -> Self {
		Self(TAG_KEY.get_or_init(RandomState::new).hash_one(value) & 0xffff_ffff)
	}
}

impl fmt::Display for Tag {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{:08x}", self.0)
	}
}

/// The tag a line at INFO or WARN names the item at `path` by, after logging the path with it at
/// DEBUG. The tag is of the path's case key, so every spelling of one NTFS item shares it.
pub(crate) fn path_tag(path: &RootPath) -> Tag {
	let tag = Tag::of(&path.case_key());
	debug!(item = %tag, %path, "an item's path");
	tag
}

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
