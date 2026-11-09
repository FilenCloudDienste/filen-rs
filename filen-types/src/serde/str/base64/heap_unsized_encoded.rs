use std::borrow::Cow;

use base64::{Engine, display::Base64Display, prelude::BASE64_STANDARD};
use serde::{Deserialize, Serialize, Serializer};
use sha2::Digest;

use crate::{
	crypto::{DigestWriter, Sha512Hash},
	traits::CowHelpers,
};

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
	/// The decoded bytes, copied only if they were borrowed.
	pub fn into_bytes(self) -> Vec<u8> {
		self.0.into_owned()
	}

	pub fn sha512_hash(&self) -> Result<Sha512Hash, std::io::Error> {
		let mut writer = base64::write::EncoderWriter::new(
			DigestWriter::new(sha2::Sha512::new()),
			&BASE64_STANDARD,
		);
		std::io::copy(&mut self.0.as_ref(), &mut writer)?;
		Ok(writer.finish()?.into_inner().finalize().into())
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

#[cfg(test)]
mod tests {
	use base64::{Engine, prelude::BASE64_STANDARD};

	use super::Base64EncodedBytes;
	use crate::crypto::Sha512Hash;

	// SHA-512 of "ZmlsZW4=", the base64 text of b"filen".
	const SHA512_OF_FILEN_BASE64: &str = "4d3fb83fc9edff346f8b4bded595d57bb8c5c85f045c6fd91403a1e318b2043938262fcac26d68704f93ed3f3c4c3353037e0d26efa9466e5e27950d6ca5dcf6";

	#[test]
	fn sha512_hash_digests_the_base64_text_not_the_raw_bytes() {
		let hash = Base64EncodedBytes::from(&b"filen"[..])
			.sha512_hash()
			.unwrap();
		assert_eq!(hex::encode(hash.as_ref()), SHA512_OF_FILEN_BASE64);
		assert_eq!(
			hash,
			Sha512Hash::digest(BASE64_STANDARD.encode(b"filen").as_bytes())
		);
	}
}
