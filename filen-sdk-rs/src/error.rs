use std::borrow::Cow;

use filen_types::fs::ObjectType;
use image::ImageError;
use thiserror::Error;

use crate::fs::name::EntryNameError;

macro_rules! impl_from {
	($error_type:ty, $kind:expr) => {
		impl From<$error_type> for FilenSdkError {
			fn from(e: $error_type) -> Self {
				FilenSdkError {
					kind: $kind,
					inner: Some(Box::new(e)),
					context: None,
				}
			}
		}
	};
}

impl From<filen_types::error::ResponseError> for Error {
	fn from(e: filen_types::error::ResponseError) -> Self {
		let kind = {
			let filen_types::error::ResponseError::ApiError { code, .. } = &e;
			match code.as_deref() {
				// this is the code returned by the server when the API key is invalid
				Some("api_key_not_found") => ErrorKind::Unauthenticated,
				Some("invalid_folder") | Some("folder_not_found") => ErrorKind::FolderNotFound,
				Some("wrong_password") => ErrorKind::WrongPassword,
				Some("max_storage_reached") => ErrorKind::MaxStorageReached,
				Some("file_not_found") => ErrorKind::FileNotFound,
				_ => ErrorKind::Server,
			}
		};

		Error {
			kind,
			inner: Some(Box::new(e)),
			context: None,
		}
	}
}

impl_from!(reqwest::Error, ErrorKind::Reqwest);
impl_from!(crate::crypto::error::ConversionError, ErrorKind::Conversion);
impl From<filen_types::error::ConversionError> for Error {
	fn from(e: filen_types::error::ConversionError) -> Self {
		crate::crypto::error::ConversionError::from(e).into()
	}
}
impl_from!(std::io::Error, ErrorKind::IO);
impl_from!(serde_json::Error, ErrorKind::Response);
impl_from!(rmp_serde::decode::Error, ErrorKind::Response);
impl_from!(ImageError, ErrorKind::ImageError);
#[cfg(feature = "heif-decoder")]
impl_from!(heif_decoder::HeifError, ErrorKind::HeifError);

/// Enum for all the error kinds that can occur in the SDK.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum ErrorKind {
	/// Returned by the server
	Server,
	/// The server replied with an error indicating the client is not authenticated
	Unauthenticated,
	/// Passthrough reqwest crate error
	Reqwest,
	/// The response was unexpected, either failed serde_json conversion
	/// or failed some other validation
	Response,
	/// A request being retried and failing after a certain number of attempts
	RetryFailed,
	/// Error during conversion, e.g. decryption, encryption or parsing
	Conversion,
	/// Error during IO operations
	IO,
	/// The downloaded chunk was too large
	ChunkTooLarge,
	/// A struct was in an invalid state
	InvalidState,
	/// Invalid type
	InvalidType,
	/// Invalid Name, usually due to an empty name
	InvalidName,
	/// Passthrough image crate error
	ImageError,
	/// Tried to use metadata for an item that failed to decrypt metadata
	MetadataWasNotDecrypted,
	/// Operation was cancelled
	Cancelled,
	#[cfg(feature = "heif-decoder")]
	/// passthrough heif-decoder crate error
	HeifError,
	/// there was an issue with the provided recovery key
	BadRecoveryKey,
	/// Internal logic error
	Internal,
	/// Not enough memory to complete the operation
	/// might be returned by WASM targets when parsing a large response (eg dir/download)
	InsufficientMemory,
	/// Error occurred when walking through a directory structure:
	Walk,
	/// Target file changed during upload or download
	FileChangedDuringSync,
	/// Specified folder was not found
	FolderNotFound,
	/// Incorrect password provided
	WrongPassword,
	/// Max storage limit reached for the account
	MaxStorageReached,
	/// File Chunk was not found when trying to download a file
	FileChunkNotFound,
	/// File not found by the backend
	FileNotFound,
}

/// Custom error type for the SDK
#[derive(Debug)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct FilenSdkError {
	kind: ErrorKind,
	inner: Option<Box<dyn std::error::Error + Send + Sync>>,
	context: Option<Cow<'static, str>>,
}

// The error type is called FilenSDKError internally, but we expose it as Error
// this is because TS doesn't allow Error as a type name,
// and this was causing issues with uniffi
// https://github.com/jhugman/uniffi-bindgen-react-native/issues/321
pub type Error = FilenSdkError;

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl FilenSdkError {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(getter)
	)]
	pub fn kind(&self) -> ErrorKind {
		self.kind
	}

	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	pub fn message(&self) -> String {
		format!("{}", self)
	}
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
impl FilenSdkError {
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "toString")]
	pub fn js_to_string(&self) -> String {
		format!("{}", self)
	}
}

impl FilenSdkError {
	/// Adds context to the error, which can be used to provide more information about the error
	pub fn with_context(mut self, context: impl Into<Cow<'static, str>>) -> Self {
		match self.context {
			Some(ref mut ctx) => {
				*ctx = format!("{}: {}", ctx, context.into()).into();
			}
			None => self.context = Some(context.into()),
		}
		self
	}

	/// Creates a new error with the given kind and message
	pub fn custom(kind: ErrorKind, message: impl Into<Cow<'static, str>>) -> Self {
		Self {
			kind,
			inner: None,
			context: Some(message.into()),
		}
	}

	pub fn custom_with_source(
		kind: ErrorKind,
		source: impl std::error::Error + Send + Sync + 'static,
		context: Option<impl Into<Cow<'static, str>>>,
	) -> Self {
		Self {
			kind,
			inner: Some(Box::new(source)),
			context: context.map(Into::into),
		}
	}

	/// Tries to downcast the error to a specific type
	///
	/// This allows you to retrieve the inner error if it is of the specified type.
	/// If the inner error is not of the specified type, it returns an error with the original error.
	pub fn downcast<T: std::error::Error + Send + Sync + 'static>(
		self,
	) -> Result<(T, Vec<Cow<'static, str>>), Self> {
		match self.downcast_inner::<T>() {
			Ok((inner, mut context)) => {
				context.reverse();
				Ok((inner, context))
			}
			Err(e) => Err(e),
		}
	}

	fn downcast_inner<T: std::error::Error + Send + Sync + 'static>(
		mut self,
	) -> Result<(T, Vec<Cow<'static, str>>), Self> {
		match self.inner {
			Some(inner) => match inner.downcast::<T>() {
				Ok(inner) => Ok((*inner, self.context.into_iter().collect())),
				Err(inner) => match inner.downcast::<FilenSdkError>() {
					Ok(inner) => match inner.downcast_inner::<T>() {
						Ok((inner, mut context)) => {
							if let Some(ctx) = self.context {
								context.push(ctx);
							}
							Ok((inner, context))
						}
						Err(inner) => {
							self.inner = Some(Box::new(inner));
							Err(self)
						}
					},
					Err(inner) => {
						self.inner = Some(inner);
						Err(self)
					}
				},
			},
			None => Err(self),
		}
	}

	pub fn downcast_ref<T: std::error::Error + Send + Sync + 'static>(&self) -> Option<&T> {
		match &self.inner {
			Some(inner) => match inner.downcast_ref::<T>() {
				Some(inner) => Some(inner),
				None => match inner.downcast_ref::<FilenSdkError>() {
					Some(inner) => inner.downcast_ref::<T>(),
					None => None,
				},
			},
			None => None,
		}
	}
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Error of kind {:?}", self.kind)?;
		if let Some(context) = &self.context {
			write!(f, ": context: {}", context)?;
		}
		if let Some(inner) = &self.inner {
			if self.context.is_some() {
				write!(f, ", ")?;
			} else {
				write!(f, ": ")?;
			}
			write!(f, "error: {}", inner)?;
		}
		Ok(())
	}
}

impl std::error::Error for Error {}

pub trait ResultExt<T, E> {
	/// Adds context to the error, which can be used to provide more information about the error
	fn context(self, context: impl Into<Cow<'static, str>>) -> Result<T, E>;
	/// Converts the error into an optional value, returning None if the error is of kind FileNotFound or FolderNotFound
	/// and returning the error otherwise
	fn optional(self) -> Result<Option<T>, E>;
}

pub trait ErrorExt<E1> {
	/// Adds context to the error, which can be used to provide more information about the error
	fn with_context(self, context: impl Into<Cow<'static, str>>) -> E1;
}

impl<T, E> ResultExt<T, Error> for Result<T, E>
where
	Error: From<E>,
{
	fn context(self, context: impl Into<Cow<'static, str>>) -> Result<T, Error> {
		self.map_err(|e| Error::from(e).with_context(context))
	}

	fn optional(self) -> Result<Option<T>, Error> {
		match self {
			Ok(value) => Ok(Some(value)),
			Err(e) => {
				let e = Error::from(e);
				match e.kind() {
					ErrorKind::FileNotFound | ErrorKind::FolderNotFound => Ok(None),
					_ => Err(e),
				}
			}
		}
	}
}

impl<E> ErrorExt<Error> for E
where
	Error: From<E>,
{
	fn with_context(self, context: impl Into<Cow<'static, str>>) -> Error {
		Error::from(self).with_context(context)
	}
}

// Internal error types for unique errors
#[derive(Debug, Error)]
#[error("The returned chunk was too large expected `{expected}`, got `{actual}`")]
pub(crate) struct ChunkTooLargeError {
	pub(crate) expected: usize,
	pub(crate) actual: usize,
}
impl_from!(ChunkTooLargeError, ErrorKind::ChunkTooLarge);

#[derive(Debug, Error)]
#[error("The struct was in an invalid state: `{actual}`, expected: `{expected}`")]
pub(crate) struct InvalidStateError {
	pub(crate) actual: String,
	pub(crate) expected: String,
}
impl_from!(InvalidStateError, ErrorKind::InvalidState);

#[derive(Debug, Error)]
#[error("Invalid type: `{actual:?}`, expected: `{expected:?}`")]
pub struct InvalidTypeError {
	pub actual: ObjectType,
	pub expected: ObjectType,
}
impl_from!(InvalidTypeError, ErrorKind::InvalidType);

#[derive(Debug, Error)]
#[error("Invalid Name '{0}'")]
pub(crate) struct InvalidNameError(pub(crate) String);
impl_from!(InvalidNameError, ErrorKind::InvalidName);

#[derive(Debug, Error)]
#[error("Retry failed after {0} attempts")]
pub(crate) struct RetryFailedError(pub(crate) usize);
impl_from!(RetryFailedError, ErrorKind::RetryFailed);

#[derive(Debug, Error)]
#[error("Tried to use metadata for an item that failed to decrypt metadata")]
pub(crate) struct MetadataWasNotDecryptedError;
impl_from!(
	MetadataWasNotDecryptedError,
	ErrorKind::MetadataWasNotDecrypted
);

#[derive(Debug, Error)]
#[error("Operation was cancelled")]
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
pub(crate) struct AbortedError;
#[cfg(any(all(target_family = "wasm", target_os = "unknown"), feature = "uniffi"))]
impl_from!(AbortedError, ErrorKind::Cancelled);

impl_from!(EntryNameError, ErrorKind::InvalidName);

#[cfg(test)]
mod tests {
	use std::io;

	use super::*;

	fn make_io_error() -> io::Error {
		io::Error::other("leaf io error")
	}

	#[test]
	fn optional_returns_some_for_ok() {
		let r: Result<i32, Error> = Ok(42);
		assert_eq!(r.optional().unwrap(), Some(42));
	}

	#[test]
	fn optional_returns_none_for_file_not_found() {
		let err = Error::custom(ErrorKind::FileNotFound, "missing file");
		let r: Result<i32, Error> = Err(err);
		assert_eq!(r.optional().unwrap(), None);
	}

	#[test]
	fn optional_returns_none_for_folder_not_found() {
		let err = Error::custom(ErrorKind::FolderNotFound, "missing folder");
		let r: Result<i32, Error> = Err(err);
		assert_eq!(r.optional().unwrap(), None);
	}

	#[test]
	fn optional_propagates_other_error_kinds() {
		let err = Error::custom(ErrorKind::Server, "boom");
		let r: Result<i32, Error> = Err(err);
		let propagated = r.optional().unwrap_err();
		assert_eq!(propagated.kind(), ErrorKind::Server);
	}

	#[test]
	fn optional_converts_foreign_error_via_from() {
		// io::Error has a From impl for Error mapping to ErrorKind::IO,
		// which is not a "not found" kind even when the io kind is NotFound.
		let r: Result<i32, io::Error> =
			Err(io::Error::new(io::ErrorKind::NotFound, "not the SDK kind"));
		let propagated = r.optional().unwrap_err();
		assert_eq!(propagated.kind(), ErrorKind::IO);
	}

	#[test]
	fn downcast_finds_direct_inner_and_returns_context() {
		let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("ctx_outer"));
		let (inner, ctxs) = err.downcast::<io::Error>().unwrap();
		assert_eq!(inner.to_string(), "leaf io error");
		assert_eq!(ctxs, vec![Cow::Borrowed("ctx_outer")]);
	}

	#[test]
	fn downcast_finds_direct_inner_with_no_context() {
		let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
		let (inner, ctxs) = err.downcast::<io::Error>().unwrap();
		assert_eq!(inner.to_string(), "leaf io error");
		assert!(ctxs.is_empty());
	}

	#[test]
	fn downcast_returns_self_when_type_does_not_match() {
		let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("ctx"));
		let returned = err.downcast::<std::fmt::Error>().unwrap_err();
		assert_eq!(returned.kind(), ErrorKind::IO);
		// The original inner error must still be accessible.
		assert!(returned.downcast_ref::<io::Error>().is_some());
	}

	#[test]
	fn downcast_returns_self_when_no_inner() {
		let err = Error::custom(ErrorKind::Server, "no inner");
		let returned = err.downcast::<io::Error>().unwrap_err();
		assert_eq!(returned.kind(), ErrorKind::Server);
	}

	#[test]
	fn downcast_unwraps_nested_filen_sdk_errors() {
		let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("leaf_ctx"));
		let middle = Error::custom_with_source(ErrorKind::IO, leaf, Some("middle_ctx"));
		let outer = Error::custom_with_source(ErrorKind::IO, middle, Some("outer_ctx"));

		let (inner, ctxs) = outer.downcast::<io::Error>().unwrap();
		assert_eq!(inner.to_string(), "leaf io error");
		// Context order: outermost first, innermost last.
		assert_eq!(
			ctxs,
			vec![
				Cow::Borrowed("outer_ctx"),
				Cow::Borrowed("middle_ctx"),
				Cow::Borrowed("leaf_ctx"),
			]
		);
	}

	#[test]
	fn downcast_skips_missing_intermediate_contexts() {
		let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("leaf_ctx"));
		let middle = Error::custom_with_source(ErrorKind::IO, leaf, None::<&'static str>);
		let outer = Error::custom_with_source(ErrorKind::IO, middle, Some("outer_ctx"));

		let (_, ctxs) = outer.downcast::<io::Error>().unwrap();
		// `None` context is collected as an empty Vec entry — verify it does not leak in.
		assert_eq!(
			ctxs,
			vec![Cow::Borrowed("outer_ctx"), Cow::Borrowed("leaf_ctx")]
		);
	}

	#[test]
	fn downcast_returns_self_when_nested_does_not_contain_type() {
		let inner = Error::custom_with_source(ErrorKind::IO, make_io_error(), Some("inner_ctx"));
		let outer = Error::custom_with_source(ErrorKind::IO, inner, Some("outer_ctx"));

		// Type isn't io::Error in any layer? It is — let's pick a different type.
		let returned = outer.downcast::<std::fmt::Error>().unwrap_err();
		assert_eq!(returned.kind(), ErrorKind::IO);
		// The nested chain must remain intact and reachable via downcast_ref.
		assert!(returned.downcast_ref::<io::Error>().is_some());
	}

	#[test]
	fn downcast_ref_finds_direct_inner() {
		let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
		let inner = err.downcast_ref::<io::Error>().unwrap();
		assert_eq!(inner.to_string(), "leaf io error");
	}

	#[test]
	fn downcast_ref_finds_nested_inner() {
		let leaf = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
		let middle = Error::custom_with_source(ErrorKind::IO, leaf, None::<&'static str>);
		let outer = Error::custom_with_source(ErrorKind::Server, middle, None::<&'static str>);

		let inner = outer.downcast_ref::<io::Error>().unwrap();
		assert_eq!(inner.to_string(), "leaf io error");
	}

	#[test]
	fn downcast_ref_returns_none_when_type_missing() {
		let err = Error::custom_with_source(ErrorKind::IO, make_io_error(), None::<&'static str>);
		assert!(err.downcast_ref::<std::fmt::Error>().is_none());
	}

	#[test]
	fn downcast_ref_returns_none_when_no_inner() {
		let err = Error::custom(ErrorKind::Server, "nothing");
		assert!(err.downcast_ref::<io::Error>().is_none());
	}

	#[test]
	fn response_error_file_not_found_maps_to_file_not_found_kind() {
		let response_err = filen_types::error::ResponseError::ApiError {
			message: Some("File not found".into()),
			code: Some("file_not_found".into()),
		};
		let err: Error = response_err.into();
		assert_eq!(err.kind(), ErrorKind::FileNotFound);
	}

	#[test]
	fn response_error_folder_not_found_still_maps_to_folder_not_found_kind() {
		// Sanity check that adding the new arm did not break the existing folder mapping.
		let response_err = filen_types::error::ResponseError::ApiError {
			message: None,
			code: Some("folder_not_found".into()),
		};
		let err: Error = response_err.into();
		assert_eq!(err.kind(), ErrorKind::FolderNotFound);
	}

	#[test]
	fn response_error_unknown_code_falls_through_to_server() {
		let response_err = filen_types::error::ResponseError::ApiError {
			message: None,
			code: Some("some_brand_new_code".into()),
		};
		let err: Error = response_err.into();
		assert_eq!(err.kind(), ErrorKind::Server);
	}
}
