use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use typenum::{U32, U256};

use crate::{
	crypto::EncryptedString,
	fs::UuidStr,
	serde::str::{SizedHexString, SizedStringBase64Chars},
	traits::CowHelpers,
};

pub const ENDPOINT: &str = "v3/dir/link/info";

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct Request {
	pub uuid: UuidStr,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Response<'a> {
	pub parent: UuidStr,
	pub metadata: EncryptedString<'a>,
	pub has_password: bool,
	pub salt: Option<LinkPasswordSalt<'a>>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, CowHelpers, Default)]
pub enum LinkPasswordSalt<'a> {
	#[default]
	None,
	V2(SizedStringBase64Chars<'a, U32>),
	V3(Box<SizedHexString<U256>>),
}

pub type LinkPasswordSaltOwned = LinkPasswordSalt<'static>;

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	LinkPasswordSaltOwned,
	String, {
	lower: |v: &LinkPasswordSaltOwned| v.to_string(),
	try_lift: |v: String| {
		LinkPasswordSaltOwned::from_cow(Cow::Owned(v)).map_err(|e| uniffi::deps::anyhow::anyhow!(e))
	}}
);

impl<'de> LinkPasswordSalt<'de> {
	fn from_cow(s: Cow<'de, str>) -> Result<Self, String> {
		Ok(match s {
			Cow::Borrowed("") => Self::None,
			salt if salt.len() == 32 => Self::V2(
				SizedStringBase64Chars::try_from(salt)
					.map_err(|e| format!("invalid V2 salt: {e}"))?,
			),
			salt if salt.len() == 512 => Self::V3(Box::new(
				SizedHexString::new_from_hex_str(&salt)
					.map_err(|e| format!("invalid V3 salt: {e}"))?,
			)),
			salt => {
				return Err(format!("invalid salt length: {}", salt.len()));
			}
		})
	}

	#[allow(clippy::inherent_to_string)] // we don't want this to be public
	#[cfg(feature = "uniffi")]
	fn to_string(&self) -> String {
		match self {
			Self::None => String::new(),
			Self::V2(s) => s.to_string(),
			Self::V3(s) => s.to_string(),
		}
	}

	fn deserialize_ref<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Self::from_cow(Cow::deserialize(deserializer)?).map_err(serde::de::Error::custom)
	}
}

// we do this for now because we're still not using yoke everywhere
impl<'de> Deserialize<'de> for LinkPasswordSalt<'_> {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		LinkPasswordSalt::deserialize_ref(deserializer).map(|v| v.into_owned_cow())
	}
}

impl Serialize for LinkPasswordSalt<'_> {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			Self::None => serializer.serialize_str(""),
			Self::V2(s) => s.serialize(serializer),
			Self::V3(s) => s.serialize(serializer),
		}
	}
}
