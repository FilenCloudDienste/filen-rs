use std::borrow::Cow;

use filen_types::fs::ObjectType;
use image::ImageError;
use thiserror::Error;

macro_rules! impl_from {
	($error_type:ty, $kind:expr) => {
		impl From<$error_type> for Error {
			fn from(e: $error_type) -> Self {
				Error {
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
			// this is the code returned by the server when the API key is invalid
			if code.as_deref() == Some("api_key_not_found") {
				ErrorKind::Unauthenticated
			} else {
				ErrorKind::Server
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
impl_from!(ImageError, ErrorKind::ImageError);
#[cfg(feature = "heif-decoder")]
impl_from!(heif_decoder::HeifError, ErrorKind::HeifError);

/// Enum for all the error kinds that can occur in the SDK.
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi, from_wasm_abi)
)]
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
	#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
	/// Operation was cancelled
	Cancelled,
	#[cfg(feature = "heif-decoder")]
	/// passthrough heif-decoder crate error
	HeifError,
	/// there was an issue with the provided recovery key
	BadRecoveryKey,
}

/// Custom error type for the SDK
#[derive(Debug)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_name = "FilenSDKError")
)]
pub struct Error {
	kind: ErrorKind,
	inner: Option<Box<dyn std::error::Error + Send + Sync>>,
	context: Option<Cow<'static, str>>,
}

#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen(js_class = "FilenSDKError")
)]
impl Error {
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(getter)
	)]
	pub fn kind(&self) -> ErrorKind {
		self.kind
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(js_name = "toString")]
	pub fn js_to_string(&self) -> String {
		format!("{}", self)
	}

	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(getter, js_name = "message")]
	pub fn js_message(&self) -> String {
		format!("{}", self)
	}
}

impl Error {
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
	pub fn downcast<T: std::error::Error + Send + Sync + 'static>(self) -> Result<T, Self> {
		match self.inner {
			Some(inner) => match inner.downcast::<T>() {
				Ok(inner) => Ok(*inner),
				Err(inner) => Err(Self {
					kind: self.kind,
					inner: Some(inner),
					context: self.context,
				}),
			},
			None => Err(self),
		}
	}
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "Error of kind {:?}: ", self.kind)?;
		if let Some(context) = &self.context {
			write!(f, "context: {}", context)?;
		}
		if let Some(inner) = &self.inner {
			write!(f, "error: {}", inner)?;
		}
		Ok(())
	}
}

impl std::error::Error for Error {}

pub trait ResultExt<T, E> {
	/// Adds context to the error, which can be used to provide more information about the error
	fn context(self, context: impl Into<Cow<'static, str>>) -> Result<T, E>;
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
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) struct AbortedError;
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
impl_from!(AbortedError, ErrorKind::Cancelled);
