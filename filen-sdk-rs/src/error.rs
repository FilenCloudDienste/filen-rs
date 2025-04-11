use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
	#[error("Request Error: `{0}`")]
	RequestError(#[from] filen_types::error::ResponseError),
	#[error("Conversion Error: `{0}`")]
	ConversionError(#[from] crate::crypto::error::ConversionError),
	#[error("IO Error: `{0}`")]
	IOErorr(#[from] std::io::Error),
	#[error("serde_json Error: `{0}`")]
	SerdeJsonError(#[from] serde_json::Error),
	#[error("`{0}`")]
	Custom(String),
}
