//! Machinery shared by jobs that turn drive items into new drive items: copies, compressed
//! archives and extracted entries.

pub(crate) mod backend;
pub(crate) mod counts;
pub(crate) mod dir;
pub(crate) mod finalize;
pub(crate) mod lock;
pub(crate) mod name_retry;
#[cfg(test)]
pub(crate) mod test_support;

use crate::{Error, ErrorKind};

/// Errors after which nothing else can succeed either, so they end the whole job.
pub(crate) fn ends_job(error: &Error) -> bool {
	matches!(
		error.kind(),
		ErrorKind::MaxStorageReached | ErrorKind::Unauthenticated
	)
}
