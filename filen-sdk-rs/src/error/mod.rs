use std::{borrow::Cow, sync::Arc};

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
				Some("email_or_password_wrong") => ErrorKind::EmailOrPasswordWrong,
				Some("enter_2fa") => ErrorKind::Enter2fa,
				Some("wrong_2fa") => ErrorKind::Wrong2fa,
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
// `MissingStableUuid` is the one foreseeable, actionable conversion failure —
// a link- or shared-listed file fed to a drive operation — so it gets its own
// kind callers can switch on instead of the blanket `Conversion` bucket.
impl From<crate::crypto::error::ConversionError> for Error {
	fn from(e: crate::crypto::error::ConversionError) -> Self {
		let kind = match &e {
			crate::crypto::error::ConversionError::MissingStableUuid => {
				ErrorKind::MissingStableUuid
			}
			_ => ErrorKind::Conversion,
		};
		Error {
			kind,
			inner: Some(Box::new(e)),
			context: None,
		}
	}
}
impl From<filen_types::error::ConversionError> for Error {
	fn from(e: filen_types::error::ConversionError) -> Self {
		crate::crypto::error::ConversionError::from(e).into()
	}
}
// `AsyncRead`/`AsyncWrite` impls (`FileReader`, `FileWriter`) can only surface `io::Error`, so
// an SDK error crossing one travels inside it. Unwrap it here so it keeps its kind
// (`MaxStorageReached`, `Reqwest`, ...) instead of collapsing to `IO`.
impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		match e.downcast::<FilenSdkError>() {
			Ok(sdk_error) => sdk_error,
			Err(e) => match e.downcast::<std::io::Error>() {
				Ok(io_error) => Self::from(io_error),
				Err(e) => FilenSdkError {
					kind: ErrorKind::IO,
					inner: Some(Box::new(e)),
					context: None,
				},
			},
		}
	}
}
impl_from!(serde_json::Error, ErrorKind::Response);
impl_from!(rmp_serde::decode::Error, ErrorKind::Response);
impl_from!(ImageError, ErrorKind::ImageError);
#[cfg(feature = "heif-decoder")]
impl_from!(heif_decoder::HeifError, ErrorKind::HeifError);

/// Enum for all the error kinds that can occur in the SDK.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	derive(serde::Serialize, serde::Deserialize, tsify::Tsify),
	tsify(into_wasm_abi)
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[filen_macros::napi_mirror(export)]
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
	// Only a build with `heif-decoder` produces it, but every build declares it, so that every
	// binding has the same kinds with the same numbers (the napi build has no heif-decoder).
	/// passthrough heif-decoder crate error
	HeifError,
	/// there was an issue with the provided recovery key
	BadRecoveryKey,
	/// Internal logic error
	Internal,
	/// Not enough memory to complete the operation
	/// might be returned by WASM targets when parsing a large response (eg dir/download), and by
	/// a compression whose format's encoder needs more than the client's archive codec budget
	InsufficientMemory,
	/// Error occurred when walking through a directory structure:
	Walk,
	/// Target file changed during upload or download
	FileChangedDuringSync,
	/// Specified folder was not found
	FolderNotFound,
	/// Incorrect password provided for a public link
	WrongPassword,
	/// Max storage limit reached for the account
	MaxStorageReached,
	/// File Chunk was not found when trying to download a file
	FileChunkNotFound,
	/// File not found by the backend
	FileNotFound,
	/// Email or Password was wrong when trying to authenticate
	EmailOrPasswordWrong,
	/// Two-factor authentication code is required
	Enter2fa,
	/// Two-factor authentication code provided was invalid
	Wrong2fa,
	/// The client's view of the item was stale for this operation — refresh
	/// the item from the server and retry
	StaleState,
	/// The file has no whole-life stable id, so it cannot be the target of a
	/// drive operation. Files listed from a public link or a shared-in folder
	/// never carry one — only read operations accept them.
	MissingStableUuid,
	/// An archive's data is damaged: a checksum does not match, a header is malformed, or the
	/// archive ends early.
	ArchiveCorrupt,
	/// An archive uses a format or feature the SDK cannot read.
	ArchiveUnsupported,
	/// An archive exceeds what the SDK allows: more entries, more decoded data per byte read, or
	/// more decoder memory than the limits permit.
	ArchiveTooLarge,
	/// The worker running an archive's codec stopped responding or died. It is not retried: an
	/// archive that crashed its codec once would crash it again.
	ArchiveWorkerDied,
	/// An archive's entries are encrypted, and no password was given.
	ArchivePasswordRequired,
	/// The password given does not open the archive's entries. Usually found before anything is
	/// created. An extraction that finds it only later, before any file was extracted, tries to
	/// move the folders it created to the trash, so a retry with the right password starts
	/// clean. It keeps any folder that now holds a file, or that it could not list or trash, all
	/// of them when it cannot get the drive lock in time, and those not trashed yet once the
	/// lock's lease is lost or cannot be confirmed; its report's top-level items list the folders
	/// it kept.
	ArchiveWrongPassword,
	/// A 7z entry is stored in a solid block after more bytes of other files than the download
	/// allowed to decode and throw away before it. Its source, a `SolidSkipExceeded` reached with
	/// [`Error::downcast_ref`], carries the numbers.
	ArchiveSolidSkipExceeded,
	/// The operation stopped before a write because the resource lock it held lost its
	/// server-side lease: another client may hold the resource now. Nothing was written after
	/// the loss, and running the operation again, which takes the lock afresh, repeats no write
	/// it made before. A job reports it for an item whose step it did not send.
	LockLost,
}

/// Custom error type for the SDK
#[derive(Debug)]
#[cfg_attr(
	all(target_family = "wasm", target_os = "unknown"),
	wasm_bindgen::prelude::wasm_bindgen
)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
// UniFFI has no getter concept, so the react-native surface can't mirror the wasm `message`
// getter. Exporting the traits gets the same outcome by another route: ubrn wraps thrown
// objects in `UniffiThrownObject extends Error`, whose message is built from the object's
// `toDebugString()` (from `Debug`; its own-property check skips the prototype `toString`, so
// `Display` alone would not feed it) — without these exports that message is just the bare
// type name. `Display` additionally gives Kotlin/Swift/TS a real `toString()`.
#[cfg_attr(feature = "uniffi", uniffi::export(Debug, Display))]
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

	// On wasm this is a GETTER (like `kind`), not a method: tooling that renders a thrown
	// value — vitest's browser-mode serializer, DevTools, generic `error.message` readers —
	// reads `.message` as a property. As a method, that read yields a function and vitest
	// printed every uncaught SDK error as "Unknown Error: Function<message>". UniFFI keeps
	// the plain `message()` method — the attribute is wasm-only.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen(getter)
	)]
	pub fn message(&self) -> String {
		format!("{}", self)
	}

	/// The server-provided, human-readable message for server/API errors, if present.
	///
	/// Returns `Some` only when the underlying error is a `ResponseError::ApiError` carrying a
	/// message (i.e. the error kinds that originate from a server response). Prefer this over
	/// `message()` when surfacing a server error to a user — `message()` wraps everything in a
	/// developer-oriented `Error of kind ...` string.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	pub fn server_message(&self) -> Option<String> {
		let filen_types::error::ResponseError::ApiError { message, .. } =
			self.downcast_ref::<filen_types::error::ResponseError>()?;

		message.clone()
	}

	/// The server-provided error code for server/API errors (e.g. `max_storage_reached`), if present.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	pub fn server_code(&self) -> Option<String> {
		let filen_types::error::ResponseError::ApiError { code, .. } =
			self.downcast_ref::<filen_types::error::ResponseError>()?;

		code.clone()
	}

	/// The wrapped (inner) error's message, WITHOUT the `Error of kind ...` wrapper that
	/// `message()` adds. Returns `None` when there is no inner error. Prefer this over `message()`
	/// when surfacing a non-server error to a user.
	#[cfg_attr(
		all(target_family = "wasm", target_os = "unknown"),
		wasm_bindgen::prelude::wasm_bindgen
	)]
	pub fn inner_message(&self) -> Option<String> {
		if let Some(shared) = self.stand_in_for() {
			return shared.inner_message();
		}
		self.inner.as_ref().map(|inner| inner.to_string())
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

	/// Error-shaped `name` property, so generic renderers (`${error.name}: ${error.message}`)
	/// identify the class instead of falling back to "Unknown Error".
	#[cfg(all(target_family = "wasm", target_os = "unknown"))]
	#[wasm_bindgen::prelude::wasm_bindgen(getter)]
	pub fn name(&self) -> String {
		"FilenSdkError".to_string()
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

	/// The error `shared` holds: the original once nothing else holds it, or else an error of
	/// the same kind wrapping the shared one, which reads as it (its message, inner message and
	/// server code) and keeps its downcasting.
	pub(crate) fn unshared(shared: Arc<Self>) -> Self {
		Arc::try_unwrap(shared)
			.unwrap_or_else(|shared| Self::custom_with_source(shared.kind(), shared, None::<&str>))
	}

	/// The shared error this one stands in for, made by [`Self::unshared`]: one of the same kind,
	/// with no context of its own, wrapping nothing but it.
	fn stand_in_for(&self) -> Option<&Self> {
		if self.context.is_some() {
			return None;
		}
		let shared = self.inner.as_deref()?.downcast_ref::<Arc<Self>>()?;
		(shared.kind == self.kind).then_some(shared)
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
		let Some(inner) = self.inner.take() else {
			return Err(self);
		};
		let inner = match inner.downcast::<T>() {
			Ok(inner) => return Ok((*inner, self.context.into_iter().collect())),
			Err(inner) => inner,
		};
		let (nested, was_shared) = match inner.downcast::<FilenSdkError>() {
			Ok(nested) => (*nested, false),
			// A shared error can only be taken apart once nothing else holds it.
			Err(inner) => match inner.downcast::<Arc<FilenSdkError>>() {
				Ok(shared) => match Arc::try_unwrap(*shared) {
					Ok(nested) => (nested, true),
					Err(shared) => {
						self.inner = Some(Box::new(shared));
						return Err(self);
					}
				},
				Err(inner) => {
					self.inner = Some(inner);
					return Err(self);
				}
			},
		};
		match nested.downcast_inner::<T>() {
			Ok((inner, mut context)) => {
				if let Some(ctx) = self.context {
					context.push(ctx);
				}
				Ok((inner, context))
			}
			Err(nested) => {
				// Hand the source back in the shape it came in.
				self.inner = Some(if was_shared {
					Box::new(Arc::new(nested))
				} else {
					Box::new(nested)
				});
				Err(self)
			}
		}
	}

	pub fn downcast_ref<T: std::error::Error + Send + Sync + 'static>(&self) -> Option<&T> {
		let inner = self.inner.as_deref()?;
		if let Some(inner) = inner.downcast_ref::<T>() {
			return Some(inner);
		}
		let nested: &FilenSdkError = match inner.downcast_ref::<FilenSdkError>() {
			Some(nested) => nested,
			// An error shared between several owners, like a failed copy's error turned into an
			// `Error` while a copy event still holds it.
			None => inner.downcast_ref::<Arc<FilenSdkError>>()?,
		};
		nested.downcast_ref::<T>()
	}
}

impl std::fmt::Display for Error {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if let Some(shared) = self.stand_in_for() {
			return shared.fmt(f);
		}
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

impl std::error::Error for Error {
	fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
		// Expose the wrapped error to standard tooling (tracing's `error`/`source` chain
		// walking, anyhow, `{:?}` formatters). Without this the inner error is only reachable
		// via the custom `downcast`, so the cause chain is invisible to everything else.
		self.inner
			.as_deref()
			.map(|e| e as &(dyn std::error::Error + 'static))
	}
}

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

/// A held [`ResourceLock`](crate::sync::lock::ResourceLock) lost its server-side lease, so the
/// operation holding it stopped before its next write.
#[derive(Debug, Error)]
#[error("Lost the lock on resource '{resource}', another client may hold it now")]
pub(crate) struct LockLostError {
	pub(crate) resource: String,
}
impl_from!(LockLostError, ErrorKind::LockLost);

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

/// Why a web worker the SDK spawned never started running its task.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[cfg(any(
	test,
	all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
))]
pub(crate) enum WorkerStartupError {
	#[error("worker could not be created: {0}")]
	Spawn(String),
	#[error("worker failed to load: {0}")]
	Load(String),
	/// A worker whose script fails to load fires a plain `Event`, which carries no message.
	#[error("worker failed to load (no error details)")]
	LoadUnreported,
	#[error("worker did not start within {0:?}")]
	TimedOut(std::time::Duration),
}
#[cfg(any(
	test,
	all(feature = "wasm-full", target_family = "wasm", target_os = "unknown")
))]
impl_from!(WorkerStartupError, ErrorKind::Internal);

#[cfg(test)]
mod tests;
