use thiserror::Error;

// todo improve error management
#[derive(Debug, Error)]
pub enum Error {
	#[error("Request Error: `{0}`")]
	RequestError(#[from] filen_types::error::ResponseError),
	#[error("Reqwest Error: `{0}`")]
	ReqwestError(#[from] reqwest::Error),
	#[error("`{0}` context: `{1}`")]
	ErrorWithContext(Box<Error>, &'static str),
	#[error("Conversion Error: `{0}`")]
	ConversionError(#[from] crate::crypto::error::ConversionError),
	#[error("IO Error: `{0}`")]
	IOErorr(#[from] std::io::Error),
	#[error("serde_json Error: `{0}`")]
	SerdeJsonError(#[from] serde_json::Error),
	#[error("`{0}`")]
	Custom(String),
	#[error("The returned chunk was too large expected `{expected}`, got `{actual}`")]
	ChunkTooLarge { expected: usize, actual: usize },
	#[error("The struct was in an invalid state: `{0}`, expected: `{1}`")]
	InvalidState(String, String),
}

pub trait ErrorExt<T, E> {
	fn context(self, context: &'static str) -> Result<T, Error>;
}

impl<T, E> ErrorExt<T, E> for Result<T, E>
where
	Error: From<E>,
{
	fn context(self, context: &'static str) -> Result<T, Error> {
		self.map_err(|e| Error::ErrorWithContext(Box::new(Error::from(e)), context))
	}
}
