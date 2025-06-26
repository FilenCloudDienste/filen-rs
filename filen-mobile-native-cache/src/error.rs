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
}

impl CacheError {
	pub fn remote(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::Remote(ErrorContext(err.into()))
	}

	pub fn conversion(err: impl Into<Cow<'static, str>>) -> Self {
		CacheError::Conversion(ErrorContext(err.into()))
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
		CacheError::IO(err.into_error_context())
	}
}

impl From<image::ImageError> for CacheError {
	fn from(err: image::ImageError) -> Self {
		CacheError::Image(err.into_error_context())
	}
}
