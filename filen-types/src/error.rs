use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConversionError {
	#[error("base64 decoding failed: `{0}`")]
	Base64DecodeError(#[from] base64::DecodeError),
	#[error("Failed to convert EncodedPublicKey to RsaPublicKey: `{0}`")]
	RsaPublicKeyError(#[from] rsa::pkcs8::spki::Error),
	#[error("Failed to convert ParentUuid to Uuid: `{0}`")]
	ParentUuidError(String),
	#[error("Invalid enum value: `{0}` for enum {1}, allowed range `{2}`-`{3}`")]
	InvalidEnumValue(u8, &'static str, u8, u8),
	#[error("Invalid length: `{0}`, expected `{1}`")]
	InvalidLength(usize, usize),
}

#[derive(Debug, Error)]
pub enum ResponseError {
	#[error("API Error, message: `{message:?}`, code: `{code:?}`")]
	ApiError {
		message: Option<String>,
		code: Option<String>,
	},
}

#[derive(Debug)]
pub(crate) struct TransparentError<T>(T);

impl<T> TransparentError<T> {
	pub(crate) fn new(inner: T) -> Self {
		Self(inner)
	}
}

impl<T: core::fmt::Display> core::fmt::Display for TransparentError<T> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{}", self.0)
	}
}

impl<T: core::error::Error + core::fmt::Debug + 'static> core::error::Error
	for TransparentError<T>
{
	fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
		Some(&self.0)
	}
}
