use std::borrow::Cow;

use generic_array::ArrayLength;
use serde::{Deserialize, Serialize};

use super::sized_str::SizedHexString;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct HexString<'a>(Cow<'a, [u8]>);

impl HexString<'_> {
	pub fn as_slice(&self) -> &[u8] {
		&self.0
	}
}

impl core::fmt::Display for HexString<'_> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "{}", hex::display(&self.0))
	}
}

impl core::fmt::Debug for HexString<'_> {
	fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
		write!(f, "HexString({})", hex::display(&self.0))
	}
}

impl HexString<'_> {
	pub fn new_from_hex_str(hex_str: &str) -> Result<Self, hex::FromHexError> {
		Ok(Self(hex::decode(hex_str)?.into()))
	}
}

impl AsRef<[u8]> for HexString<'_> {
	fn as_ref(&self) -> &[u8] {
		&self.0
	}
}

impl From<Vec<u8>> for HexString<'_> {
	fn from(bytes: Vec<u8>) -> Self {
		Self(bytes.into())
	}
}

impl Serialize for HexString<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		hex::serde::no_prefix::serialize(&self.0, serializer)
	}
}

// use owned deserialization for now
impl<'de> Deserialize<'de> for HexString<'_> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Ok(Self(hex::serde::no_prefix::deserialize(deserializer)?))
	}
}

impl<N: ArrayLength> From<SizedHexString<N>> for HexString<'static> {
	fn from(sized_hex: SizedHexString<N>) -> Self {
		Self(sized_hex.0.to_vec().into())
	}
}

impl<'a, N: ArrayLength> From<&'a SizedHexString<N>> for HexString<'a> {
	fn from(sized_hex: &'a SizedHexString<N>) -> Self {
		Self(Cow::Borrowed(sized_hex.as_slice()))
	}
}

#[cfg(test)]
mod tests;
