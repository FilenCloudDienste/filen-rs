use std::{borrow::Cow, error::Error};

use filen_sdk_rs::ErrorKind;

use crate::sql::SQLError;

pub struct ErrorContext(Cow<'static, str>);

impl std::fmt::Debug for ErrorContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}
impl std::fmt::Display for ErrorContext {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl Default for ErrorContext {
	fn default() -> Self {
		ErrorContext(Cow::Borrowed(""))
	}
}

trait IntoErrorContext {
	fn into_error_context(self) -> ErrorContext;
}

impl<T> IntoErrorContext for T
where
	T: Error,
{
	fn into_error_context(self) -> ErrorContext {
		ErrorContext(self.to_string().into())
	}
}

impl From<String> for ErrorContext {
	fn from(value: String) -> Self {
		ErrorContext(Cow::Owned(value))
	}
}

impl From<&'static str> for ErrorContext {
	fn from(value: &'static str) -> Self {
		ErrorContext(Cow::Borrowed(value))
	}
}

uniffi::custom_type!(ErrorContext, String, {
	lower: |s| s.0.into(),
});

#[derive(uniffi::Error, Debug)]
pub enum CacheError {
	SQL(ErrorContext),
	SDK(ErrorContext),
	Conversion(ErrorContext),
	IO(ErrorContext),
	Remote(ErrorContext),
	Image(ErrorContext),
	Unauthenticated(ErrorContext),
	Disabled(ErrorContext),
	DoesNotExist(ErrorContext),
	Unsupported(ErrorContext),
	NotADirectory(ErrorContext),
	FailedToDecrypt(ErrorContext),
	InvalidName(ErrorContext),
	/// The sync anchor handed in does not name a sequence in THIS database — it is
	/// malformed, or it was issued by an incarnation a wipe has since replaced. Its
	/// own variant because the file providers have to tell it apart from every other
	/// failure: it is the one they answer by re-enumerating from scratch
	/// (`NSFileProviderErrorSyncAnchorExpired`), not by reporting an error.
	SyncAnchorExpired(ErrorContext),
	/// The caller aborted the call through the [`FfiAbortSignal`](crate::abort::FfiAbortSignal)
	/// it handed in. Its own variant because it is the one failure that is not a failure: the
	/// caller asked for it, so a provider reports it as a cancellation rather than surfacing it
	/// to the user as an error.
	Aborted(ErrorContext),
	/// A downloaded file's content does not hash to the BLAKE3 its metadata declares, so its
	/// stored data or its metadata is not what the client that uploaded it wrote. A download
	/// failing with it writes nothing to the file's cache slot. Its own variant because retrying
	/// fetches the same bytes: unlike a failed transfer, it is for the app to report as damaged
	/// content, not as a network error.
	ContentHashMismatch(ErrorContext),
}

impl CacheError {
	pub fn remote(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::Remote(ErrorContext(err.into()))
	}

	pub fn conversion(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::Conversion(ErrorContext(err.into()))
	}

	pub fn io(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::IO(ErrorContext(err.into()))
	}

	pub fn image(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::Image(ErrorContext(err.into()))
	}

	pub fn context(self, context: impl Into<Cow<'static, str>>) -> Self {
		match self {
			CacheError::SQL(err) => CacheError::SQL(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::SDK(err) => CacheError::SDK(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Conversion(err) => CacheError::Conversion(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::IO(err) => CacheError::IO(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Remote(err) => CacheError::Remote(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Image(err) => CacheError::Image(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Unauthenticated(err) => CacheError::Unauthenticated(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Disabled(err) => CacheError::Disabled(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::DoesNotExist(err) => CacheError::DoesNotExist(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Unsupported(err) => CacheError::Unsupported(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::NotADirectory(error_context) => CacheError::NotADirectory(ErrorContext(
				format!("{}: {}", context.into(), error_context.0).into(),
			)),
			CacheError::FailedToDecrypt(err) => CacheError::FailedToDecrypt(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::InvalidName(err) => CacheError::InvalidName(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::SyncAnchorExpired(err) => CacheError::SyncAnchorExpired(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::Aborted(err) => CacheError::Aborted(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
			CacheError::ContentHashMismatch(err) => CacheError::ContentHashMismatch(ErrorContext(
				format!("{}: {}", context.into(), err.0).into(),
			)),
		}
	}
}

#[allow(unused)]
pub(crate) trait CacheErrorContextExt<T> {
	fn context(self, context: impl Into<Cow<'static, str>>) -> Result<T, CacheError>;
}

impl<T> CacheErrorContextExt<T> for Result<T, CacheError> {
	fn context(self, context: impl Into<Cow<'static, str>>) -> Result<T, CacheError> {
		self.map_err(|e| e.context(context))
	}
}

impl std::fmt::Display for CacheError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			CacheError::SQL(err) => err.fmt(f),
			CacheError::SDK(err) => err.fmt(f),
			CacheError::Conversion(err) => err.fmt(f),
			CacheError::IO(err) => err.fmt(f),
			CacheError::Remote(err) => err.fmt(f),
			CacheError::Image(err) => err.fmt(f),
			CacheError::Unauthenticated(err) => err.fmt(f),
			CacheError::Disabled(err) => err.fmt(f),
			CacheError::DoesNotExist(err) => err.fmt(f),
			CacheError::Unsupported(err) => err.fmt(f),
			CacheError::NotADirectory(err) => err.fmt(f),
			CacheError::FailedToDecrypt(err) => err.fmt(f),
			CacheError::InvalidName(err) => err.fmt(f),
			CacheError::SyncAnchorExpired(err) => err.fmt(f),
			CacheError::Aborted(err) => err.fmt(f),
			CacheError::ContentHashMismatch(err) => err.fmt(f),
		}
	}
}

impl From<SQLError> for CacheError {
	fn from(err: SQLError) -> Self {
		CacheError::SQL(err.into_error_context())
	}
}

impl From<rusqlite::Error> for CacheError {
	fn from(err: rusqlite::Error) -> Self {
		CacheError::SQL(err.into_error_context())
	}
}

impl From<filen_sdk_rs::error::Error> for CacheError {
	fn from(err: filen_sdk_rs::error::Error) -> Self {
		if err.kind() == ErrorKind::ContentHashMismatch {
			CacheError::ContentHashMismatch(err.into_error_context())
		} else {
			CacheError::SDK(err.into_error_context())
		}
	}
}

/// The kind of the first SDK error in `err`'s chain: the error an SDK call failed with, carried
/// inside an `io::Error` by a cache function that returns one.
fn sdk_error_kind(err: &std::io::Error) -> Option<ErrorKind> {
	let mut source: Option<&(dyn Error + 'static)> = err.get_ref().map(|e| e as _);
	while let Some(e) = source {
		if let Some(sdk) = e.downcast_ref::<filen_sdk_rs::error::Error>() {
			return Some(sdk.kind());
		}
		source = e.source();
	}
	None
}

impl From<uuid::Error> for CacheError {
	fn from(err: uuid::Error) -> Self {
		CacheError::Conversion(err.into_error_context())
	}
}

impl From<filen_sdk_rs::crypto::error::ConversionError> for CacheError {
	fn from(err: filen_sdk_rs::crypto::error::ConversionError) -> Self {
		CacheError::Conversion(err.into_error_context())
	}
}

impl From<std::io::Error> for CacheError {
	fn from(err: std::io::Error) -> Self {
		if err.kind() == std::io::ErrorKind::NotFound {
			CacheError::DoesNotExist(err.into_error_context())
		} else if sdk_error_kind(&err) == Some(ErrorKind::ContentHashMismatch) {
			CacheError::ContentHashMismatch(err.into_error_context())
		} else {
			CacheError::IO(err.into_error_context())
		}
	}
}

impl From<image::ImageError> for CacheError {
	fn from(err: image::ImageError) -> Self {
		CacheError::Image(err.into_error_context())
	}
}

impl From<filen_sdk_rs::fs::name::EntryNameError> for CacheError {
	fn from(err: filen_sdk_rs::fs::name::EntryNameError) -> Self {
		CacheError::InvalidName(err.into_error_context())
	}
}

#[cfg(test)]
mod tests {
	use std::io;

	use filen_sdk_rs::{
		ErrorKind,
		error::Error as SdkError,
		thumbnail::{
			APP_PROCESS_THUMBNAIL_MEM_BUDGET, ByteSource, ThumbSpec, ThumbnailFit,
			make_thumbnail_from_source,
		},
	};

	use super::CacheError;
	use crate::io::DownloadError;

	const MISMATCH: &str = "The content of file 1 does not match its hash";

	#[test]
	fn an_sdk_hash_mismatch_is_its_own_error() {
		let err = CacheError::from(SdkError::custom(ErrorKind::ContentHashMismatch, MISMATCH));

		let CacheError::ContentHashMismatch(context) = err else {
			panic!("mapped to {err:?}");
		};
		assert_eq!(
			context.to_string(),
			"Error of kind ContentHashMismatch: context: The content of file 1 does not match its \
			 hash"
		);
	}

	/// A byte source whose reads fail as a remote chunk's read does when the file's content does not
	/// match its hash.
	struct MismatchedSource;

	impl ByteSource for MismatchedSource {
		fn len(&self) -> u64 {
			4
		}

		fn read_at(&mut self, _offset: u64, _buf: &mut [u8]) -> io::Result<usize> {
			Err(io::Error::other(SdkError::custom(
				ErrorKind::ContentHashMismatch,
				MISMATCH,
			)))
		}
	}

	#[test]
	fn a_thumbnail_ended_by_a_hash_mismatch_is_its_own_error() {
		let err = make_thumbnail_from_source(
			Box::new(MismatchedSource),
			&ThumbSpec::new(64, 64, APP_PROCESS_THUMBNAIL_MEM_BUDGET),
			ThumbnailFit::Cover,
			None,
			&mut Vec::new(),
		)
		.expect_err("no thumbnail comes of a source that cannot be read");

		let err = CacheError::from(err);
		assert!(
			matches!(err, CacheError::ContentHashMismatch(_)),
			"mapped to {err:?}"
		);
	}

	#[test]
	fn any_other_sdk_error_is_still_an_sdk_error() {
		let err = CacheError::from(SdkError::custom(ErrorKind::Reqwest, "offline"));

		assert!(matches!(err, CacheError::SDK(_)), "mapped to {err:?}");
	}

	#[test]
	fn a_hash_mismatch_carried_in_an_io_error_is_its_own_error() {
		let sdk = SdkError::custom(ErrorKind::ContentHashMismatch, MISMATCH);
		let err = CacheError::from(io::Error::other(DownloadError::from(sdk)));

		let CacheError::ContentHashMismatch(context) = err else {
			panic!("mapped to {err:?}");
		};
		assert_eq!(
			context.to_string(),
			"Failed to download file: Error of kind ContentHashMismatch: context: The content of \
			 file 1 does not match its hash"
		);
	}

	#[test]
	fn a_hash_mismatch_directly_in_an_io_error_is_its_own_error() {
		let sdk = SdkError::custom(ErrorKind::ContentHashMismatch, MISMATCH);
		let err = CacheError::from(io::Error::other(sdk));

		assert!(
			matches!(err, CacheError::ContentHashMismatch(_)),
			"mapped to {err:?}"
		);
	}

	#[test]
	fn any_other_sdk_error_carried_in_an_io_error_is_still_an_io_error() {
		let sdk = SdkError::custom(ErrorKind::Reqwest, "offline");
		let err = CacheError::from(io::Error::other(DownloadError::from(sdk)));

		assert!(matches!(err, CacheError::IO(_)), "mapped to {err:?}");
	}

	#[test]
	fn context_keeps_a_hash_mismatch_its_own_error() {
		let err = CacheError::from(SdkError::custom(ErrorKind::ContentHashMismatch, MISMATCH))
			.context("opening the file");

		let CacheError::ContentHashMismatch(context) = err else {
			panic!("mapped to {err:?}");
		};
		assert_eq!(
			context.to_string(),
			"opening the file: Error of kind ContentHashMismatch: context: The content of file 1 \
			 does not match its hash"
		);
	}
}
