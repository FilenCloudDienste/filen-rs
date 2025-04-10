use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
	#[error("Request Error: `{0}`")]
	RequestError(#[from] filen_types::error::ResponseError),
	#[error("Conversion Error: `{0}`")]
	ConversionError(#[from] crate::crypto::error::ConversionError),
	#[error("`{0}`")]
	Custom(String),
}
