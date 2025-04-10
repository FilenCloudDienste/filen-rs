use thiserror::Error;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ConversionError {
	#[error("Failed to convert hex encoded string to Vec<u8>: `{0}`")]
	HexDecodeError(#[from] faster_hex::Error),
	#[error("Invalid string length for conversion: `{0}` expected `{1}`")]
	InvalidStringLength(usize, usize),
	#[error("Failed to Encrypt data: `{0}`")]
	AesGcmEncryptError(#[from] aes_gcm::aead::Error),
	#[error("Invalid key length: `{0}`")]
	AesGcmCipherInvalidLength(#[from] aes_gcm::aes::cipher::InvalidLength),
	#[error("Invalid version tag `{0}`, supported versions are `{1:?}`")]
	InvalidVersion(String, Vec<String>),
	#[error("Failed base64 decode: `{0}`")]
	Base64DecodeError(#[from] base64::DecodeError),
	#[error("Failed to convert to utf8: `{0}`")]
	ToStringError(#[from] std::string::FromUtf8Error),
	#[error("Failed to convert from utf8: `{0}`")]
	ToStrError(#[from] std::str::Utf8Error),
	#[error("Failed to convert from slice: `{0}`")]
	TryFromSliceError(#[from] std::array::TryFromSliceError),
	#[error("Multiple errors occurred: `{0:?}`")]
	MultipleErrors(Vec<ConversionError>),
	#[error("Failed to run argon2 hash: `{0}`")]
	Argon2Error(#[from] argon2::Error),
	#[error("Failed to parse RSA key: `{0}`")]
	PKCS8Error(#[from] rsa::pkcs8::Error),
	#[error("`{0}`")]
	FilenTypesError(#[from] filen_types::error::ConversionError),
	#[error("Public and private RSA keys do not match")]
	InvalidKeyPair,
}
