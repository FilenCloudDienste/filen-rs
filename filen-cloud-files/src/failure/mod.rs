//! What the core tells the OS when it cannot answer a request.

use filen_mobile_native_cache::CacheError;

/// Why the core failed a request. The OS layer maps each to a `STATUS_CLOUD_FILE_*` code, matching
/// exhaustively so a new variant must be mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
	/// No session is signed in, or the placeholder belongs to another account.
	NotAuthenticated,
	/// The item is no longer in the drive.
	NotFound,
	/// The server could not be reached.
	Unreachable,
	/// The item is of a kind the provider does not serve.
	Unsupported,
	/// The content does not match what the drive declares for it.
	ContentDamaged,
	/// The item cannot be decrypted.
	Undecryptable,
	/// The placeholder describes a version of the item the drive no longer holds.
	StalePlaceholder,
	/// The OS asked for something the placeholder cannot answer (a malformed identity or range).
	InvalidRequest,
	/// A failure that may pass if the OS asks again.
	Transient,
	/// A broken invariant in the provider.
	Internal,
}

/// How the core ends a request it could not answer with the bytes or children asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
	/// The OS cancelled the request. Not a failure, and nothing more is sent: a request the
	/// provider gives up on itself, such as on shutdown, fails instead.
	Cancelled,
	/// The request failed.
	Fail(Failure),
	/// The content does not match the hash the drive declares for it: the request fails with
	/// [`Failure::ContentDamaged`], the version is never served again, and the local engine's
	/// hydration job dehydrates the file: the default for the open decision on a hash mismatch
	/// ("dehydrate on a mismatch").
	Damaged,
}

/// How a request that failed with `error` from the cache ends. Exhaustive, with no catch-all arm,
/// so a new [`CacheError`] variant does not compile until it is mapped here, as the Apple File
/// Provider maps them. Each arm is a recorded default, to be checked against what Explorer shows
/// once the Windows layer reports them.
pub(crate) fn cache_answer(error: &CacheError) -> Answer {
	match error {
		// No session, or the provider is turned off: the user must sign in.
		CacheError::Disabled(_) | CacheError::Unauthenticated(_) => {
			Answer::Fail(Failure::NotAuthenticated)
		}
		// The item is gone from the drive, or is no longer what the placeholder says.
		CacheError::DoesNotExist(_) | CacheError::NotADirectory(_) => {
			Answer::Fail(Failure::NotFound)
		}
		// The server could not be reached or refused the call.
		CacheError::Remote(_) => Answer::Fail(Failure::Unreachable),
		CacheError::Unsupported(_) => Answer::Fail(Failure::Unsupported),
		CacheError::FailedToDecrypt(_) => Answer::Fail(Failure::Undecryptable),
		// A name the cache refused: nothing these paths send, so the request was malformed.
		CacheError::InvalidName(_) => Answer::Fail(Failure::InvalidRequest),
		// No sync anchor travels on these paths, so the cache answering with one is a bug.
		CacheError::SyncAnchorExpired(_) => Answer::Fail(Failure::Internal),
		CacheError::Aborted(_) => Answer::Cancelled,
		CacheError::ContentHashMismatch(_) => Answer::Damaged,
		// The database, the SDK, a conversion, local IO or an image: worth asking again. The SDK's
		// errors arrive as text, so an offline read through it lands here rather than as
		// unreachable.
		CacheError::SQL(_)
		| CacheError::SDK(_)
		| CacheError::Conversion(_)
		| CacheError::IO(_)
		| CacheError::Image(_) => Answer::Fail(Failure::Transient),
	}
}

#[cfg(test)]
mod tests;
