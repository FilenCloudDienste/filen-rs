use std::{borrow::Cow, error::Error};

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
		CacheError::SDK(err.into_error_context())
	}
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
