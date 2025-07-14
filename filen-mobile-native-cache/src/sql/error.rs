use crate::ffi::ItemType;

#[derive(thiserror::Error, Debug)]
pub enum SQLError {
	#[error("SQLite error: {0}")]
	SQLiteError(#[from] rusqlite::Error),
	#[error("SQLLite FromSqlError: {0}")]
	FromSqlError(#[from] rusqlite::types::FromSqlError),
	#[error("Unexpected type: expected: {0:?}, got: {1:?}")]
	UnexpectedType(ItemType, ItemType),
	#[error("Unexpected None value for item: {0:?}, field: {1}")]
	UnexpectedNone(ItemType, &'static str),
}

pub trait OptionalExtensionSQL<T> {
	fn optional(self) -> Result<Option<T>, SQLError>;
}

impl<T> OptionalExtensionSQL<T> for Result<T, SQLError> {
	fn optional(self) -> Result<Option<T>, SQLError> {
		match self {
			Ok(value) => Ok(Some(value)),
			Err(SQLError::SQLiteError(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
			Err(e) => Err(e),
		}
	}
}
