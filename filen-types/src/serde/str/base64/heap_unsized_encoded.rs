use std::borrow::Cow;

use base64::{Engine, display::Base64Display, prelude::BASE64_STANDARD};
use serde::{Deserialize, Serialize, Serializer};
use sha2::Digest;

use crate::{crypto::Sha512Hash, traits::CowHelpers};

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers)]
pub struct Base64EncodedBytes<'a>(Cow<'a, [u8]>);

impl Serialize for Base64EncodedBytes<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: Serializer,
	{
		serializer.collect_str(&Base64Display::new(self.0.as_ref(), &BASE64_STANDARD))
	}
}

impl<'de> Deserialize<'de> for Base64EncodedBytes<'_> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		let bytes = BASE64_STANDARD.decode(s.as_bytes()).map_err(|err| {
			serde::de::Error::custom(format!("Failed to decode base64 string: {err}"))
		})?;
		Ok(Self(Cow::Owned(bytes)))
	}
}

impl std::fmt::Display for Base64EncodedBytes<'_> {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		Base64Display::new(self.0.as_ref(), &BASE64_STANDARD).fmt(f)
	}
}

impl Base64EncodedBytes<'_> {
	pub fn sha512_hash(&self) -> Result<Sha512Hash, std::io::Error> {
		let mut writer = base64::write::EncoderWriter::new(sha2::Sha512::new(), &BASE64_STANDARD);
		std::io::copy(&mut self.0.as_ref(), &mut writer)?;
		Ok(writer.finish()?.finalize().into())
	}
}

impl<'a> From<&'a [u8]> for Base64EncodedBytes<'a> {
	fn from(bytes: &'a [u8]) -> Self {
		Self(Cow::Borrowed(bytes))
	}
}

impl From<Vec<u8>> for Base64EncodedBytes<'_> {
	fn from(bytes: Vec<u8>) -> Self {
		Self(Cow::Owned(bytes))
	}
}
