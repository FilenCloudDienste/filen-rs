use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConversionError {
	#[error("Failed to convert EncodedString to Vec<u8>: `{0}`")]
	Base64DecodeError(#[from] base64::DecodeError),
	#[error("Failed to convert EncodedPublicKey to RsaPublicKey: `{0}`")]
	RsaPublicKeyError(#[from] rsa::pkcs8::spki::Error),
}

#[derive(Debug, Error)]
pub enum ResponseError {
	#[error("API Error, message: `{message:?}`, code: `{code:?}`")]
	ApiError {
		message: Option<String>,
		code: Option<String>,
	},
}
