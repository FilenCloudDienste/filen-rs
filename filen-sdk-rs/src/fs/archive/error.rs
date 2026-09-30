//! How a read of an archive fails, as its job reports it.

use std::io;

use crate::{Error, ErrorKind};

use super::{
	decode::CodecError,
	sevenz::SevenZError,
	tar_iter::TarError,
	worker::JobEnded,
	zip::{crypto::CryptoError, read::ZipError},
};

/// The error a read of an archive ended with: the typed error of the reader that raised it (each
/// converts into its own kind), the job ending, or an SDK error a reader carried through.
/// Anything else came from a decoder, so the data is damaged.
pub(crate) fn read_failure(error: io::Error) -> Error {
	typed::<JobEnded>(error)
		.or_else(typed::<CodecError>)
		.or_else(typed::<TarError>)
		.or_else(typed::<SevenZError>)
		.or_else(typed::<ZipError>)
		.or_else(typed::<CryptoError>)
		.or_else(typed::<Error>)
		.unwrap_or_else(|error| {
			if error.kind() == io::ErrorKind::UnexpectedEof && error.get_ref().is_none() {
				Error::custom(ErrorKind::ArchiveCorrupt, "the archive ends early")
			} else {
				Error::custom_with_source(ErrorKind::ArchiveCorrupt, error, None::<&str>)
			}
		})
}

/// The `T` inside `error`, as the error its job reports.
fn typed<T>(error: io::Error) -> Result<Error, io::Error>
where
	T: std::error::Error + Send + Sync + 'static,
	Error: From<T>,
{
	error.downcast::<T>().map(Error::from)
}
