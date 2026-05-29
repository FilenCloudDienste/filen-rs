use std::borrow::Cow;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use typenum::{U32, U256};

use crate::{
	crypto::EncryptedString,
	fs::UuidStr,
	serde::str::{SizedHexString, SizedStrBase64Chars},
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
	pub salt: Option<LinkPasswordSalt>,
	#[serde(with = "crate::serde::time::seconds_or_millis")]
	pub timestamp: DateTime<Utc>,
	pub download_btn: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum LinkPasswordSalt {
	#[default]
	None,
	V2(Box<SizedStrBase64Chars<U32>>),
	V3(Box<SizedHexString<U256>>),
}

#[cfg(feature = "uniffi")]
uniffi::custom_type!(
	LinkPasswordSalt,
	String, {
	lower: |v: &LinkPasswordSalt| v.to_string(),
	try_lift: |v: String| {
		LinkPasswordSalt::from_cow(Cow::Owned(v)).map_err(|e| uniffi::deps::anyhow::anyhow!(e))
	}}
);

impl LinkPasswordSalt {
	fn from_cow(s: Cow<'_, str>) -> Result<Self, String> {
		Ok(match s {
			salt if salt.len() <= 1 => Self::None,
			salt if salt.len() == 32 => Self::V2(
				<Box<SizedStrBase64Chars<U32>>>::try_from(salt.into_owned())
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
}

// we do this for now because we're still not using yoke everywhere
impl<'de> Deserialize<'de> for LinkPasswordSalt {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>,
	{
		Self::from_cow(Cow::deserialize(deserializer)?).map_err(serde::de::Error::custom)
	}
}

impl Serialize for LinkPasswordSalt {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		match self {
			Self::None => serializer.serialize_str(" "),
			Self::V2(s) => s.serialize(serializer),
			Self::V3(s) => s.serialize(serializer),
		}
	}
}
